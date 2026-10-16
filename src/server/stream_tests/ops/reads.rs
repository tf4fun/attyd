//! The one-shot reads of protocol §8: a session's attachments and its
//! workspace context. A read holds, loads and installs nothing; an unheld
//! session's read is `not_found`.
use super::*;

/// Every `AttachmentRef` in `value`, in document order.
fn references(value: &Value) -> Vec<Value> {
    let mut found = Vec::new();
    collect_references(value, &mut found);
    found
}

fn collect_references(value: &Value, found: &mut Vec<Value>) {
    match value {
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some("attachment")
                && object.get("id").is_some_and(Value::is_string)
            {
                found.push(value.clone());
            }
            object
                .values()
                .for_each(|value| collect_references(value, found));
        }
        Value::Array(items) => items
            .iter()
            .for_each(|value| collect_references(value, found)),
        _ => {}
    }
}

fn attachment_path(session_id: &str, reference: &Value) -> String {
    format!(
        "/api/v2/sessions/{session_id}/attachments/{}",
        reference["id"].as_str().unwrap()
    )
}

/// `GET` of `reference` in `session_id` serves the ACP block it names.
async fn assert_served(bench: &Bench, session_id: &str, reference: &Value, block: &Value) {
    let (status, body) = bench.get(&attachment_path(session_id, reference)).await;
    assert_eq!(status, StatusCode::OK, "{session_id}: {reference}: {body}");
    assert_eq!(&body, block, "{session_id}: the block {reference} names");
}

async fn assert_unserved(bench: &Bench, session_id: &str, reference: &Value, what: &str) {
    let (status, body) = bench.get(&attachment_path(session_id, reference)).await;
    not_found(status, &body, what);
}

/// A server whose scripted Agent advertises `capabilities`, launched with
/// `flags` before the Agent's URL.
async fn ws_with(flags: &[&str], capabilities: Value) -> (Bench, Link) {
    let agents = Agents::listen().await;
    let mut args = vec!["attyd", "--transport", "ws"];
    args.extend_from_slice(flags);
    args.extend(["--", agents.url.as_str()]);
    let options = Options::try_parse_from(args).unwrap().normalized().unwrap();
    let mut bench = Bench::serve(options, Some(agents)).await;
    let mut link = bench.agents.as_mut().unwrap().connect().await;
    let initialize = link.request("initialize").await;
    link.reply(
        &initialize,
        json!({"protocolVersion": 1, "agentCapabilities": capabilities, "authMethods": []}),
    )
    .await;
    bench
        .timeline
        .lock()
        .await
        .find(0, "the Agent is ready", |documents| {
            documents
                .get("global")
                .is_some_and(|global| global.live["agent"]["phase"] == "ready")
        })
        .await;
    (bench, link)
}

/// An attachment is served while its own session's log or live references
/// it: a failed prompt's input until the next operation replaces it, a
/// running prompt's process and then the entry it settles into. Another session's reference
/// to the same block does not stand in for this one's, and a running
/// operation leaves the log's references readable.
#[tokio::test]
async fn attachments_are_served_while_their_own_session_references_them() {
    let (bench, mut link) = Bench::ws(json!({"promptCapabilities": {"image": true}})).await;
    bench.create(&mut link, "s", json!({})).await;
    bench.create(&mut link, "t", json!({})).await;
    let mut watch = bench.watch(&scopes(&["session/s", "session/t"])).await;
    watch
        .until("the alignments", |watch| {
            watch.has("session/s") && watch.has("session/t")
        })
        .await;
    let (logged, failed, process) = (image("bG9nZ2Vk"), image("ZmFpbGVk"), image("cHJvY2Vzcw=="));

    // t logs the block the failed prompt of s also sends.
    let open = bench.expect_of("session/t").await;
    let (status, body) = bench
        .op(
            "session/t",
            &open,
            "prompt",
            json!({"prompt": [text("keep"), failed.clone()]}),
        )
        .await;
    let kept = assert_accepted(status, &body, "t's prompt");
    let request = link.request("session/prompt").await;
    link.reply(&request, json!({"stopReason": "end_turn"}))
        .await;
    watch.until_op("session/t", &kept, "completed").await;
    let in_t = references(&json!(watch.log("session/t")));
    assert_eq!(in_t.len(), 1, "{in_t:?}");
    assert_served(&bench, "t", &in_t[0], &failed).await;

    // s logs one block.
    let open = bench.expect_of("session/s").await;
    let (status, body) = bench
        .op(
            "session/s",
            &open,
            "prompt",
            json!({"prompt": [text("log"), logged.clone()]}),
        )
        .await;
    let first = assert_accepted(status, &body, "the logged prompt");
    let request = link.request("session/prompt").await;
    link.reply(&request, json!({"stopReason": "end_turn"}))
        .await;
    watch.until_op("session/s", &first, "completed").await;
    let in_log = references(&json!(watch.log("session/s")));
    assert_eq!(in_log.len(), 1, "{in_log:?}");
    assert_served(&bench, "s", &in_log[0], &logged).await;

    // A failed prompt adds no entry; its input keeps its block readable.
    let (status, body) = bench
        .op(
            "session/s",
            &first,
            "prompt",
            json!({"prompt": [text("fail"), failed.clone()]}),
        )
        .await;
    let second = assert_accepted(status, &body, "the failing prompt");
    let request = link.request("session/prompt").await;
    link.refuse(&request).await;
    watch.until_op("session/s", &second, "failed").await;
    assert_eq!(
        watch.log("session/s").len(),
        1,
        "a failed prompt adds no entry"
    );
    let in_input = references(&watch.live("session/s")["op"]["input"]);
    assert_eq!(in_input.len(), 1, "{in_input:?}");
    assert_eq!(in_input[0], in_t[0], "the reference is content-addressed");
    let before = bench.document("session/s").await;
    assert_served(&bench, "s", &in_input[0], &failed).await;
    assert_served(&bench, "s", &in_log[0], &logged).await;

    // A missing ID is not found; a malformed one is not a request at all.
    let missing = json!({"type": "attachment", "id": "0".repeat(64), "block": "image"});
    assert_unserved(&bench, "s", &missing, "a well-formed missing ID").await;
    let (status, body) = bench
        .get("/api/v2/sessions/s/attachments/not-an-attachment-id")
        .await;
    invalid(status, &body, "a malformed attachment ID");
    link.quiet(QUIET).await;
    watch.quiet(QUIET).await;
    assert_eq!(
        bench.document("session/s").await,
        before,
        "reads change nothing"
    );

    // The next operation replaces the failed one: its input no longer
    // references the block, while t's log still does. The running prompt's
    // process references a block of its own, and its entry keeps it.
    let (status, body) = bench
        .op(
            "session/s",
            &second,
            "prompt",
            json!({"prompt": [text("plain")]}),
        )
        .await;
    let third = assert_accepted(status, &body, "the attachment-free prompt");
    let request = link.request("session/prompt").await;
    link.update(
        "s",
        json!({
            "sessionUpdate": "tool_call", "toolCallId": "look", "title": "Look",
            "status": "in_progress",
            "content": [{"type": "content", "content": process.clone()}],
        }),
    )
    .await;
    watch
        .until_live("session/s", "the process references its block", |live| {
            live["op"]["id"] == third && !references(&live["op"]["process"]).is_empty()
        })
        .await;
    let in_process = references(&watch.live("session/s")["op"]["process"]);
    assert_eq!(in_process.len(), 1, "{in_process:?}");
    assert_served(&bench, "s", &in_process[0], &process).await;
    assert_served(&bench, "s", &in_log[0], &logged).await;
    assert_unserved(
        &bench,
        "s",
        &in_input[0],
        "the replaced input's block, in s",
    )
    .await;
    assert_served(&bench, "t", &in_t[0], &failed).await;

    link.reply(&request, json!({"stopReason": "end_turn"}))
        .await;
    watch.until_op("session/s", &third, "completed").await;
    let log = watch.log("session/s");
    assert_eq!(log.len(), 2, "{log:?}");
    assert_eq!(
        references(&json!(log)),
        [in_log[0].clone(), in_process[0].clone()],
        "the process block settles into the log"
    );
    assert_served(&bench, "s", &in_process[0], &process).await;
    assert_served(&bench, "s", &in_log[0], &logged).await;
    link.quiet(QUIET).await;
    watch.quiet(QUIET).await;
    watch.converged(&bench).await;
}

/// A compaction summary's block is served while the running prompt's process
/// references it, and then from the log its turn settles into.
#[tokio::test]
async fn a_compaction_summary_block_is_served_while_running_and_once_settled() {
    let (bench, mut link) = Bench::ws(json!({"promptCapabilities": {"image": true}})).await;
    bench.create(&mut link, "s", json!({})).await;
    let mut watch = bench.watch(&scopes(&["session/s"])).await;
    watch
        .until("the alignment", |watch| watch.has("session/s"))
        .await;
    let summary = image("c3VtbWFyeQ==");

    let open = bench.expect_of("session/s").await;
    let (status, body) = bench
        .op(
            "session/s",
            &open,
            "prompt",
            json!({"prompt": [text("go")]}),
        )
        .await;
    let prompt = assert_accepted(status, &body, "the prompt");
    let request = link.request("session/prompt").await;
    link.update(
        "s",
        json!({"sessionUpdate": "compaction_update", "compactionId": "c", "status": "in_progress"}),
    )
    .await;
    link.update(
        "s",
        json!({
            "sessionUpdate": "compaction_update", "compactionId": "c", "status": "completed",
            "summary": [summary.clone()],
        }),
    )
    .await;
    watch
        .until_live("session/s", "the process references the summary", |live| {
            live["op"]["id"] == prompt && !references(&live["op"]["process"]).is_empty()
        })
        .await;
    let in_process = references(&watch.live("session/s")["op"]["process"]);
    assert_eq!(in_process.len(), 1, "{in_process:?}");
    assert_served(&bench, "s", &in_process[0], &summary).await;

    link.reply(&request, json!({"stopReason": "end_turn"}))
        .await;
    watch.until_op("session/s", &prompt, "completed").await;
    assert_eq!(
        references(&json!(watch.log("session/s"))),
        in_process,
        "the summary block settles into the log"
    );
    assert_served(&bench, "s", &in_process[0], &summary).await;
    link.quiet(QUIET).await;
    watch.quiet(QUIET).await;
    watch.converged(&bench).await;
}

/// A read holds nothing: an unheld session's attachment is not found and
/// loads nothing, and once a loaded session is actually released, its
/// logged attachment is not found either and it is not held again.
#[tokio::test]
async fn reads_hold_nothing_and_a_released_session_serves_no_attachment() {
    let (bench, mut link) = ws_with(
        &["--session-unobserved-timeout", "0"],
        json!({"loadSession": true, "promptCapabilities": {"image": true}}),
    )
    .await;
    let mut global = bench.watch(&scopes(&["global"])).await;
    global
        .until("the alignment", |watch| watch.has("global"))
        .await;

    let missing = json!({"type": "attachment", "id": "0".repeat(64), "block": "image"});
    assert_unserved(&bench, "unheld", &missing, "an unheld session").await;
    link.quiet(QUIET).await;
    global.quiet(QUIET).await;

    let picture = image("c2F2ZWQ=");
    let mut session = bench.watch(&cold("saved")).await;
    link.serve_load(
        "saved",
        &[
            json!({"sessionUpdate": "user_message_chunk", "content": picture.clone()}),
            json!({"sessionUpdate": "agent_message_chunk", "content": text("seen")}),
        ],
    )
    .await;
    session
        .until_live("session/saved", "the load completes", |live| {
            live["op"]["kind"] == "load" && live["op"]["status"] == "completed"
        })
        .await;
    let logged = references(&json!(session.log("session/saved")));
    assert_eq!(logged.len(), 1, "{logged:?}");
    assert_served(&bench, "saved", &logged[0], &picture).await;
    global
        .until_live("global", "saved is held", |live| {
            live["sessions"].get("saved").is_some()
        })
        .await;

    // The actual release is the barrier: global stops listing the session.
    drop(session);
    global
        .until_live("global", "saved is released", |live| {
            live["sessions"].get("saved").is_none()
        })
        .await;
    assert_unserved(&bench, "saved", &logged[0], "a released session's block").await;
    let (status, body) = bench
        .get("/api/v2/sessions/saved/files?query=anything")
        .await;
    not_found(status, &body, "a released session's context search");
    link.quiet(QUIET).await;
    global.quiet(QUIET).await;
    let (status, body) = bench.read("session/saved").await;
    not_found(status, &body, "the read did not hold the session again");
    global.converged(&bench).await;
}

// ---------------------------------------------------------------------------
// Workspace context

/// A real workspace with one context file named `name`, holding `content`.
fn workspace(
    parent: &std::path::Path,
    directory: &str,
    name: &str,
    content: &str,
) -> std::path::PathBuf {
    let root = parent.join(directory);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join(name), content).unwrap();
    std::fs::canonicalize(root).unwrap()
}

fn files_path(session_id: &str, query: &str) -> String {
    let encoded = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("query", query)
        .finish();
    format!("/api/v2/sessions/{session_id}/files?{encoded}")
}

fn content_path(session_id: &str, path: &std::path::Path) -> String {
    let encoded = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("path", path.to_str().unwrap())
        .finish();
    format!("/api/v2/sessions/{session_id}/files/content?{encoded}")
}

/// The search matches' absolute paths.
fn matched(body: &Value) -> Vec<String> {
    body["matches"]
        .as_array()
        .unwrap_or_else(|| panic!("a search answers matches: {body}"))
        .iter()
        .map(|found| found["path"].as_str().unwrap().to_string())
        .collect()
}

async fn assert_reads(bench: &Bench, session_id: &str, path: &std::path::Path, content: &str) {
    let (status, body) = bench.get(&content_path(session_id, path)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{session_id} reads {path:?}: {body}"
    );
    assert_eq!(
        body["attachment"]["block"]["resource"]["text"], content,
        "{session_id} reads {path:?}: {body}"
    );
    assert_eq!(body["attachment"]["block"]["type"], "resource", "{body}");
}

/// A session's context search and read run in its own `cwd`, plus the
/// launch's additional roots, read-only launches included: two sessions with
/// same-named files read their own, and neither reads the other's workspace.
/// Names with spaces, Chinese and `+` survive the query encoding. An unheld
/// session's context is not found.
#[tokio::test]
async fn context_reads_follow_each_sessions_own_workspace() {
    let directory = tempfile::tempdir().unwrap();
    let name = "笔记 x+y.md";
    let alpha = workspace(directory.path(), "工作 区 a+1", name, "alpha 内容");
    let beta = workspace(directory.path(), "工作 区 b+2", name, "beta 内容");
    let shared = workspace(directory.path(), "共享 根", "共享 c+d.md", "shared 内容");
    let manifest = env!("CARGO_MANIFEST_DIR");
    let fixture = std::path::Path::new(manifest).join("tests/fixtures/fake-agent.ts");
    let options = Options::try_parse_from([
        "attyd",
        "--cwd",
        manifest,
        "--read-only",
        "--add-dir",
        shared.to_str().unwrap(),
        "--",
        "node",
        "--import",
        "tsx",
        fixture.to_str().unwrap(),
        "--race-new",
    ])
    .unwrap()
    .normalized()
    .unwrap();
    let bench = Bench::serve(options, None).await;
    bench
        .timeline
        .lock()
        .await
        .find(0, "the Agent is ready", |documents| {
            documents
                .get("global")
                .is_some_and(|global| global.live["agent"]["phase"] == "ready")
        })
        .await;
    let mut sessions = Vec::new();
    for cwd in [&alpha, &beta] {
        let expect = bench.expect_of("global").await;
        let (status, body) = bench
            .spawn_op(
                "global",
                &expect,
                "create",
                json!({"cwd": cwd.to_str().unwrap()}),
            )
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK, "create in {cwd:?}: {body}");
        sessions.push(body["result"]["sessionId"].as_str().unwrap().to_string());
    }
    let (a, b) = (sessions[0].as_str(), sessions[1].as_str());
    assert_ne!(a, b);
    // A read is not an operation: no document changes, whether it succeeds
    // or is refused.
    let mut before = Vec::new();
    for scope in [
        "global".to_string(),
        format!("session/{a}"),
        format!("session/{b}"),
    ] {
        before.push((bench.document(&scope).await, scope));
    }

    for (session, own, other, content) in [
        (a, &alpha, &beta, "alpha 内容"),
        (b, &beta, &alpha, "beta 内容"),
    ] {
        let (status, body) = bench.get(&files_path(session, name)).await;
        assert_eq!(status, StatusCode::OK, "{session} searches: {body}");
        assert_eq!(
            matched(&body),
            [own.join(name).to_str().unwrap()],
            "{session} finds only its own file: {body}"
        );
        assert_reads(&bench, session, &own.join(name), content).await;

        // The additional root is searched and read in a read-only launch.
        let (status, body) = bench.get(&files_path(session, "共享 c+d")).await;
        assert_eq!(status, StatusCode::OK, "{session} searches: {body}");
        assert_eq!(
            matched(&body),
            [shared.join("共享 c+d.md").to_str().unwrap()],
            "{body}"
        );
        assert_reads(&bench, session, &shared.join("共享 c+d.md"), "shared 内容").await;

        // The other session's workspace is refused with a protocol error.
        let (status, body) = bench.get(&content_path(session, &other.join(name))).await;
        assert!(
            !status.is_success(),
            "{session} must not read {other:?}: {body}"
        );
        let expected = match body["error"]["code"].as_str() {
            Some("invalid_request") => StatusCode::BAD_REQUEST,
            Some("not_found") => StatusCode::NOT_FOUND,
            _ => panic!("{session}: a refused read has a protocol error body: {body}"),
        };
        assert_refused(
            status,
            &body,
            expected,
            body["error"]["code"].as_str().unwrap(),
            "a read outside the session's workspace",
        );
    }

    // Refused reads answer the protocol's error body.
    let (status, body) = bench
        .get(&content_path(a, &alpha.join("missing 文件.md")))
        .await;
    not_found(status, &body, "a missing file");
    let (status, body) = bench
        .get(&format!("/api/v2/sessions/{a}/files/content"))
        .await;
    invalid(status, &body, "a read without a path");
    let (status, body) = bench.get(&files_path(a, "nul\0query")).await;
    invalid(status, &body, "a search query with a NUL byte");
    let (status, body) = bench.get(&files_path("unheld", name)).await;
    not_found(status, &body, "an unheld session's search");
    let (status, body) = bench.get(&content_path("unheld", &alpha.join(name))).await;
    not_found(status, &body, "an unheld session's read");
    tokio::time::sleep(QUIET).await;
    for (document, scope) in before {
        assert_eq!(
            bench.document(&scope).await,
            document,
            "{scope}: reads hold, load and change nothing"
        );
    }
}
