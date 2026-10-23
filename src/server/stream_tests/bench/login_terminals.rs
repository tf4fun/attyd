//! Login terminals (protocol §4.3, §4.6, §6.2, §7; acceptance 16, 17): a
//! `terminal_login` operation runs the method's login in a real terminal
//! that `global.terminals` shows by the operation's ID. Input, resize and
//! dismiss are resource actions; the terminal outlives its operation until
//! it has exited and is dismissed.
//!
//! The Agent is the stdio fake Agent; its launch script runs a login method
//! instead when the bridge appends that method's arguments. Only the last
//! test's login exits `0`: a successful login refreshes the connection, and
//! that test follows a global stream into the next connection.
//!
//! PTY output arrives in arbitrary chunks, so the tests wait for complete
//! lines only.
use tokio::io::AsyncWriteExt;

use super::*;

/// The fake Agent's terminal methods. `login-echo` answers each line with
/// `got:<line>`, `size` with `stty size`, and `done` by exiting `3`.
fn methods() -> Value {
    json!([
        {"id": "login-exit", "name": "Exit", "type": "terminal", "args": ["--login-exit"]},
        {"id": "login-echo", "name": "Echo", "type": "terminal", "args": ["--login-echo"]},
        {"id": "login-kill", "name": "Kill", "type": "terminal", "args": ["--login-kill"]},
    ])
}

/// A stdio bench whose fake Agent offers `methods()`, and a `global` stream
/// once the Agent is ready.
async fn login_bench() -> (Bench, Watch) {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let fixture = std::path::Path::new(manifest).join("tests/fixtures/fake-agent.ts");
    let script = format!(
        r#"case "$1" in
  --login-exit) printf 'login exit\n'; exit 1 ;;
  --login-echo)
    printf 'login echo\n'
    while IFS= read -r line; do
      case "$line" in
        size) stty size ;;
        done) exit 3 ;;
        *) printf 'got:%s\n' "$line" ;;
      esac
    done
    exit 4 ;;
  --login-kill) kill -TERM $$ ;;
  *) export ATTYD_FAKE_AUTH_METHODS='{}'; exec node --import tsx '{}' "$@" ;;
esac"#,
        methods(),
        fixture.display(),
    );
    let bench = Bench::stdio(&script, manifest).await;
    let mut watch = bench.watch(&scopes(&["global"])).await;
    watch
        .until_live("global", "the Agent is ready", |live| {
            live["agent"]["phase"] == "ready"
        })
        .await;
    (bench, watch)
}

fn output(live: &Value, terminal: &str) -> String {
    live["terminals"][terminal]["output"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// The complete lines `terminal` printed, without their CR/LF; a line still
/// missing its newline is not among them.
fn lines(live: &Value, terminal: &str) -> Vec<String> {
    let output = output(live, terminal);
    let mut lines: Vec<&str> = output.split('\n').collect();
    lines.pop();
    lines
        .into_iter()
        .map(|line| line.trim_end_matches('\r').to_string())
        .collect()
}

fn printed(live: &Value, terminal: &str, line: &str) -> bool {
    lines(live, terminal).iter().any(|printed| printed == line)
}

/// The complete `got:` lines, in order.
fn echoed(live: &Value, terminal: &str) -> Vec<String> {
    lines(live, terminal)
        .into_iter()
        .filter(|line| line.starts_with("got:"))
        .collect()
}

/// Takes events until the complete `got:` lines are exactly `expected`.
async fn until_echoed(watch: &mut Watch, terminal: &str, expected: &[&str]) {
    watch
        .until_live("global", &format!("echoes {expected:?}"), |live| {
            echoed(live, terminal) == expected
        })
        .await;
}

async fn start(bench: &Bench, expect: &Value, method: &str) -> Value {
    let (status, body) = bench
        .op(
            "global",
            expect,
            "terminal_login",
            json!({"methodId": method, "cols": 80, "rows": 24}),
        )
        .await;
    assert_accepted(status, &body, method)
}

async fn input(bench: &Bench, terminal: &str, data: &str) {
    let (status, body) = bench
        .act("global", terminal, "input", json!({"data": data}))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "input {data:?}: {body}");
}

/// Sends `data` as input on a connection of its own and never reads the
/// response; the connection stays open until the returned stream drops.
async fn unread_input(bench: &Bench, terminal: &str, data: &str) -> TcpStream {
    let address = bench.address;
    let body = json!({"data": data}).to_string();
    let request = format!(
        "POST /api/v2/scopes/global/resources/{}/input HTTP/1.1\r\n\
         Host: {address}\r\nOrigin: http://{address}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        encoded(terminal),
        body.len(),
    );
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    stream
}

/// Input is written in the order it arrives and never deduplicated, and
/// input whose response the client never read stays written, once; a resize
/// to the same size leaves the same state; a running terminal cannot be
/// dismissed; an exited one is dismissed once.
#[cfg(unix)]
#[tokio::test]
async fn a_login_terminal_takes_input_in_order_and_is_dismissed_after_it_exits() {
    let (bench, mut watch) = login_bench().await;
    let expect = watch.live("global")["op"]["id"].clone();
    let login = start(&bench, &expect, "login-echo").await;
    let terminal = login.as_str().unwrap().to_string();
    watch
        .until_live("global", "the login prints", |live| {
            printed(live, &terminal, "login echo")
        })
        .await;
    let live = watch.live("global");
    assert_eq!(live["op"]["id"], login);
    assert_eq!(live["op"]["kind"], "terminal_login");
    assert_eq!(live["op"]["status"], "running");
    assert_eq!(
        live["op"]["input"],
        json!({"methodId": "login-echo", "cols": 80, "rows": 24})
    );
    let shown = &live["terminals"][&terminal];
    assert_eq!(shown["terminalId"], login, "the terminal ID is the op ID");
    assert_eq!(shown["methodId"], "login-echo");
    assert_eq!(shown["exitStatus"], Value::Null);

    // In order, and the same bytes twice are written twice.
    let mut expected = Vec::new();
    for data in ["one", "two", "one"] {
        input(&bench, &terminal, &format!("{data}\n")).await;
        expected.push(format!("got:{data}"));
        let expected: Vec<&str> = expected.iter().map(String::as_str).collect();
        until_echoed(&mut watch, &terminal, &expected).await;
    }

    // Input whose response is never read: once its echo shows, the client
    // goes away; the bytes stay written, once.
    let unread = unread_input(&bench, &terminal, "raw\n").await;
    until_echoed(
        &mut watch,
        &terminal,
        &["got:one", "got:two", "got:one", "got:raw"],
    )
    .await;
    drop(unread);
    input(&bench, &terminal, "after\n").await;
    let all = ["got:one", "got:two", "got:one", "got:raw", "got:after"];
    until_echoed(&mut watch, &terminal, &all).await;

    // The size: as started, then as resized; the same size again changes
    // nothing.
    input(&bench, &terminal, "size\n").await;
    watch
        .until_live("global", "the started size", |live| {
            printed(live, &terminal, "24 80")
        })
        .await;
    let (status, body) = bench
        .act(
            "global",
            &terminal,
            "resize",
            json!({"cols": 100, "rows": 30}),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    watch.quiet(QUIET).await;
    let resized = bench.document("global").await;
    let (status, body) = bench
        .act(
            "global",
            &terminal,
            "resize",
            json!({"cols": 100, "rows": 30}),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    watch.quiet(QUIET).await;
    assert_eq!(bench.document("global").await, resized);
    input(&bench, &terminal, "size\n").await;
    watch
        .until_live("global", "the resized size", |live| {
            printed(live, &terminal, "30 100")
        })
        .await;

    // A running login cannot be dismissed.
    watch.quiet(QUIET).await;
    let before = bench.document("global").await;
    let (status, body) = bench.act("global", &terminal, "dismiss", json!({})).await;
    not_allowed(status, &body, "a running login's dismissal");
    watch.quiet(QUIET).await;
    assert_eq!(bench.document("global").await, before);

    // A non-zero exit is the login's result: the op completes with it.
    input(&bench, &terminal, "done\n").await;
    watch.until_op("global", &login, "completed").await;
    let live = watch.live("global");
    let exit = json!({"exitCode": 3, "signal": null});
    assert_eq!(live["op"]["result"], exit);
    assert_eq!(live["terminals"][&terminal]["exitStatus"], exit);
    assert_eq!(live["terminals"][&terminal]["methodId"], "login-echo");
    assert_eq!(echoed(&live, &terminal), all);

    // Dismissed once it exited; a repeat is not found and changes nothing.
    let (status, body) = bench.act("global", &terminal, "dismiss", json!({})).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    watch
        .until_live("global", "the dismissed terminal is gone", |live| {
            live["terminals"].get(&terminal).is_none()
        })
        .await;
    let live = watch.live("global");
    assert_eq!(live["op"]["id"], login, "a dismissal is not an operation");
    assert_eq!(live["op"]["result"], exit);
    let before = bench.document("global").await;
    let (status, body) = bench.act("global", &terminal, "dismiss", json!({})).await;
    not_found(status, &body, "a repeated dismissal");
    let (status, body) = bench
        .act("global", &terminal, "input", json!({"data": "late\n"}))
        .await;
    not_found(status, &body, "input to a dismissed terminal");
    watch.quiet(QUIET).await;
    assert_eq!(bench.document("global").await, before);

    // The next login is admitted on the ended one.
    let next = start(&bench, &login, "login-exit").await;
    watch.until_op("global", &next, "completed").await;
    let live = watch.live("global");
    let exit = json!({"exitCode": 1, "signal": null});
    assert_eq!(live["op"]["result"], exit);
    let next = next.as_str().unwrap();
    assert_eq!(live["terminals"][next]["exitStatus"], exit);
    assert!(printed(&live, next, "login exit"));
    watch.quiet(QUIET).await;
    watch.converged(&bench).await;
}

/// A login ended by a signal completes with that signal; a new stream aligns
/// on it and on another method's running login; dismissing it removes only
/// it. A cancelled login ends `cancelled`, without a result.
#[cfg(unix)]
#[tokio::test]
async fn a_signalled_login_completes_and_a_dismissal_removes_only_its_terminal() {
    let (bench, mut watch) = login_bench().await;
    let expect = watch.live("global")["op"]["id"].clone();
    let a = start(&bench, &expect, "login-kill").await;
    let first = a.as_str().unwrap().to_string();
    watch.until_op("global", &a, "completed").await;
    let live = watch.live("global");
    let result = live["op"]["result"].clone();
    assert_eq!(result["exitCode"], Value::Null, "{result}");
    assert!(
        result["signal"]
            .as_str()
            .is_some_and(|signal| !signal.is_empty()),
        "{result}"
    );
    assert_eq!(live["terminals"][&first]["exitStatus"], result);
    assert_eq!(live["terminals"][&first]["methodId"], "login-kill");

    // B, another method, runs while A's terminal stays.
    let b = start(&bench, &a, "login-echo").await;
    let second = b.as_str().unwrap().to_string();
    watch
        .until_live("global", "login B prints", |live| {
            printed(live, &second, "login echo")
        })
        .await;
    let live = watch.live("global");
    assert_eq!(live["terminals"][&first]["exitStatus"], result);
    let running = live["terminals"][&second].clone();
    assert_eq!(running["terminalId"], b);
    assert_eq!(running["methodId"], "login-echo");
    assert_eq!(running["exitStatus"], Value::Null);

    // A new stream's first alignment shows both: A exited, B running, each
    // with its own method.
    let mut fresh = bench.watch(&scopes(&["global"])).await;
    fresh
        .until("the new stream aligns", |watch| watch.has("global"))
        .await;
    let aligned = fresh.live("global");
    assert_eq!(aligned["terminals"][&first], live["terminals"][&first]);
    assert_eq!(aligned["terminals"][&first]["methodId"], "login-kill");
    assert_eq!(aligned["terminals"][&first]["exitStatus"], result);
    assert_eq!(aligned["terminals"][&second], running);
    assert_eq!(aligned["op"]["id"], b);
    assert_eq!(aligned["op"]["status"], "running");
    drop(fresh);

    // Dismissing A removes only A.
    let (status, body) = bench.act("global", &first, "dismiss", json!({})).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    watch
        .until_live("global", "A is gone", |live| {
            live["terminals"].get(&first).is_none()
        })
        .await;
    let live = watch.live("global");
    assert_eq!(live["terminals"][&second], running);
    assert_eq!(live["op"]["id"], b);
    assert_eq!(live["op"]["status"], "running");
    let (status, body) = bench.act("global", &first, "dismiss", json!({})).await;
    not_found(status, &body, "A dismissed again");

    // Cancelling B stops its login. The repeat races the login's exit: it is
    // accepted for B again while B runs, and refused once B ended. (This does
    // not show a repeat that surely meets a running terminal.)
    let (status, body) = bench.cancel("global", &b).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["op"], b);
    let (status, body) = bench.cancel("global", &b).await;
    if status == StatusCode::ACCEPTED {
        assert_eq!(body["op"], b, "a repeated cancel: {body}");
    } else {
        not_allowed(status, &body, "a cancel repeated after B ended");
    }
    watch.until_op("global", &b, "cancelled").await;
    let live = watch.live("global");
    assert_eq!(live["op"]["cancelRequested"], true);
    assert!(live["op"].get("result").is_none(), "{}", live["op"]);
    assert_ne!(
        live["terminals"][&second]["exitStatus"],
        Value::Null,
        "the stopped login has exited"
    );
    let (status, body) = bench.act("global", &second, "dismiss", json!({})).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    watch
        .until_live("global", "B is gone", |live| {
            live["terminals"]
                .as_object()
                .is_some_and(|terminals| terminals.is_empty())
        })
        .await;
    watch.quiet(QUIET).await;
    watch.converged(&bench).await;
}

/// A pure v2 `terminal_login` that exits `0` completes with that exit; the
/// connection is refreshed, and a global-only stream, checked event by event
/// across both generations, sees the next connection ready. The bridge
/// starts exactly one next connection.
#[cfg(unix)]
#[tokio::test]
async fn a_successful_login_completes_and_a_global_only_stream_sees_the_next_connection() {
    let root = tempfile::tempdir().unwrap();
    let auth = root.path().join("authenticated");
    let launches = root.path().join("launches");
    let manifest = env!("CARGO_MANIFEST_DIR");
    let fixture = std::path::Path::new(manifest).join("tests/fixtures/fake-agent.ts");
    // Each launch, the Agent's and the login's (which appends its method's
    // arguments), records its arguments before it becomes the fixture.
    let script = format!(
        "export ATTYD_FAKE_AUTH_FILE='{}'; echo \"$*\" >> '{}'; exec node --import tsx '{}' \"$@\"",
        auth.display(),
        launches.display(),
        fixture.display(),
    );
    let options = Options::try_parse_from([
        "attyd",
        "--cwd",
        manifest,
        "--",
        "sh",
        "-c",
        &script,
        "agent",
        "--terminal-auth-required",
    ])
    .unwrap()
    .normalized()
    .unwrap();
    let launched = || {
        let lines = std::fs::read_to_string(&launches).unwrap_or_default();
        let (logins, agents): (Vec<&str>, Vec<&str>) = lines
            .lines()
            .partition(|line| line.contains("--terminal-login"));
        (agents.len(), logins.len())
    };
    let bench = Bench::serve(options, Agents::listen().await).await;
    let mut watch = bench.watch(&scopes(&["global"])).await;
    let phase = |watch: &Watch| watch.live("global")["agent"]["phase"].clone();
    watch
        .until_live("global", "the Agent is ready", |live| {
            live["agent"]["phase"] == "ready"
        })
        .await;
    assert_eq!(launched(), (1, 0));

    let expect = watch.live("global")["op"]["id"].clone();
    let login = start(&bench, &expect, "terminal-login").await;
    let terminal = login.as_str().unwrap().to_string();
    watch
        .until_live("global", "the login prompts", |live| {
            output(live, &terminal).contains("Enter access code")
        })
        .await;
    let live = watch.live("global");
    assert_eq!(live["terminals"][&terminal]["methodId"], "terminal-login");
    input(&bench, &terminal, "open-sesame\r").await;
    watch.until_op("global", &login, "completed").await;
    assert_eq!(
        watch.live("global")["op"]["result"],
        json!({"exitCode": 0, "signal": null})
    );
    assert!(auth.exists(), "the login signed in");

    // The old connection ends, and the next one becomes ready on the same
    // stream.
    let mut ended = phase(&watch) != "ready";
    while !(ended && phase(&watch) == "ready") {
        watch.event().await;
        ended |= phase(&watch) != "ready";
    }
    assert_eq!(
        launched(),
        (2, 1),
        "one login and exactly one next connection"
    );
    let live = watch.live("global");
    assert!(
        live["terminals"].get(&terminal).is_none(),
        "the login's terminal ended with its connection: {live}"
    );
    watch.quiet(QUIET).await;
    watch.converged(&bench).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(launched(), (2, 1), "the next connection is started once");
    bench.hub.shutdown().await;
}
