//! A terminal's stdin and stdout must make progress together: writing stdin
//! cannot hold the bridge guard that its output observer needs.
use super::*;

/// The login shell writes its own PID and then execs cat, so cleanup kills
/// only this test's login process, even if its HTTP input is blocked.
struct LoginProcess {
    pid_file: std::path::PathBuf,
    signalled: bool,
}

impl LoginProcess {
    fn kill(&mut self) {
        if self.signalled {
            return;
        }
        let Some(pid) = std::fs::read_to_string(&self.pid_file)
            .ok()
            .and_then(|pid| pid.trim().parse::<u32>().ok())
            .filter(|pid| *pid > 1)
        else {
            return;
        };
        self.signalled = true;
        let _ = std::process::Command::new("/bin/kill")
            .args(["-KILL", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

impl Drop for LoginProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

#[test]
fn terminal_input_backpressure_does_not_block_its_output_observer() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(check_terminal_backpressure());
    }));
    // A regressed PTY write may stay blocked even after its child dies on
    // macOS. Never let runtime teardown hide the assertion by waiting on it
    // forever; the test's own login process is still killed by its guard.
    runtime.shutdown_timeout(Duration::from_secs(2));
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn check_terminal_backpressure() {
    let directory = tempfile::tempdir().unwrap();
    let mut login_process = LoginProcess {
        pid_file: directory.path().join("login.pid"),
        signalled: false,
    };
    let manifest = env!("CARGO_MANIFEST_DIR");
    let fixture = std::path::Path::new(manifest).join("tests/fixtures/fake-agent.ts");
    let quote =
        |path: &std::path::Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
    let script = format!(
        r#"case "$1" in
  --login-cat)
    printf '%s\n' "$$" > {}
    stty -echo -icanon min 1 time 0 || exit 70
    printf 'cat ready\n'
    exec cat ;;
  *) export ATTYD_FAKE_AUTH_METHODS='[{{"id":"login-cat","name":"Cat","type":"terminal","args":["--login-cat"]}}]'
    exec node --import tsx {} "$@" ;;
esac"#,
        quote(&login_process.pid_file),
        quote(&fixture),
    );
    let bench = Bench::stdio(&script, manifest).await;
    let mut watch = bench.watch(&scopes(&["global"])).await;
    watch
        .until_live("global", "the Agent is ready", |live| {
            live["agent"]["phase"] == "ready"
        })
        .await;
    let expect = watch.live("global")["op"]["id"].clone();
    let (status, body) = bench
        .op(
            "global",
            &expect,
            "terminal_login",
            json!({"methodId": "login-cat", "cols": 80, "rows": 24}),
        )
        .await;
    let login = assert_accepted(status, &body, "the cat login");
    let terminal = login.as_str().unwrap().to_string();
    watch
        .until_live("global", "cat has configured the terminal", |live| {
            live["terminals"][&terminal]["output"]
                .as_str()
                .is_some_and(|output| {
                    output
                        .split('\n')
                        .any(|line| line.trim_end_matches('\r') == "cat ready")
                })
        })
        .await;

    // Larger than the PTY's input and output queues. With echo disabled,
    // only cat can drain this input and produce the matching output.
    let data = format!("{}\ninput-backpressure-complete\n", "x".repeat(512 * 1024));
    let address = bench.address;
    let path = format!(
        "/api/v2/targets/global/resources/{}/input",
        encoded(&terminal)
    );
    let mut input = tokio::spawn(async move {
        body_of(send(address, Method::POST, &path, Some(json!({"data": data}))).await).await
    });
    let mut read = tokio::spawn(async move {
        body_of(send(address, Method::GET, "/api/v2/nodes/global", None).await).await
    });
    let deadline = tokio::time::Instant::now() + WAIT;
    let (input_result, read_result) = tokio::join!(
        tokio::time::timeout_at(deadline, &mut input),
        tokio::time::timeout_at(deadline, &mut read),
    );

    // Terminate this test's login before asserting a timeout or HTTP error.
    // The outer runtime also has a bounded shutdown if killing the child
    // does not release a regressed PTY write on this platform.
    login_process.kill();
    if input_result.is_err() {
        input.abort();
    }
    if read_result.is_err() {
        read.abort();
    }
    let (status, body) = input_result
        .expect("terminal input deadlocked while its output observer waited for the bridge")
        .expect("the input HTTP task failed");
    assert_eq!(status, StatusCode::NO_CONTENT, "terminal input: {body}");
    let (status, body) = read_result
        .expect("GET global stopped responding during terminal input")
        .expect("the GET HTTP task failed");
    assert_eq!(status, StatusCode::OK, "GET global: {body}");
    watch.until_op("global", &login, "completed").await;
    watch.converged(&bench).await;
}
