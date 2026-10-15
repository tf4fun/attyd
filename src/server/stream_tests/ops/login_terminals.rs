//! Login terminals (protocol §4.3, §4.6, §6.2, §7; acceptance 9): a
//! `terminal_login` operation runs the method's login in a real terminal
//! that `global.terminals` shows by the operation's ID. Input, resize and
//! dismiss are resource actions; the terminal outlives its operation until
//! it has exited and is dismissed.
//!
//! The Agent is the stdio fake Agent; its launch script runs a login method
//! instead when the bridge appends that method's arguments. No login exits
//! `0`: a successful login refreshes the connection, which these tests do
//! not follow.
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

/// The `got:` lines of `output`, in order.
fn echoed(output: &str) -> Vec<String> {
    output
        .match_indices("got:")
        .map(|(at, _)| output[at..].split(['\r', '\n']).next().unwrap().to_string())
        .collect()
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

/// Input is written in the order it arrives and never deduplicated; a resize
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
            output(live, &terminal).contains("login echo")
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
    for data in ["one\n", "two\n", "one\n"] {
        input(&bench, &terminal, data).await;
    }
    watch
        .until_live("global", "three echoes", |live| {
            echoed(&output(live, &terminal)).len() == 3
        })
        .await;
    assert_eq!(
        echoed(&output(&watch.live("global"), &terminal)),
        ["got:one", "got:two", "got:one"]
    );

    // The size: as started, then as resized; the same size again changes
    // nothing.
    input(&bench, &terminal, "size\n").await;
    watch
        .until_live("global", "the started size", |live| {
            output(live, &terminal).contains("24 80")
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
            output(live, &terminal).contains("30 100")
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
    assert!(output(&live, &terminal).contains("got:two"));

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
    assert!(output(&live, next).contains("login exit"));
    watch.quiet(QUIET).await;
    watch.converged(&bench).await;
}

/// A login ended by a signal completes with that signal; dismissing it while
/// another method's login runs removes only it. A cancelled login ends
/// `cancelled`, without a result.
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
            output(live, &second).contains("login echo")
        })
        .await;
    let live = watch.live("global");
    assert_eq!(live["terminals"][&first]["exitStatus"], result);
    let running = live["terminals"][&second].clone();
    assert_eq!(running["terminalId"], b);
    assert_eq!(running["methodId"], "login-echo");
    assert_eq!(running["exitStatus"], Value::Null);

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

    // Cancelling B stops its login; repeating the cancel changes nothing.
    let (status, body) = bench.cancel("global", &b).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["op"], b);
    let (status, body) = bench.cancel("global", &b).await;
    assert!(
        status == StatusCode::ACCEPTED || body["error"]["code"] == "not_allowed",
        "a repeated cancel: {status} {body}"
    );
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
