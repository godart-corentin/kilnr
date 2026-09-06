use serde_json::json;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn request(port: u16, path: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .write_all(format!("GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").as_bytes())
        .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn body(response: &str) -> &str {
    response.split_once("\r\n\r\n").unwrap().1
}

#[test]
fn test_web_http_json_api_spa_fallback_and_terminal_sse_streams() {
    let root = tempfile::tempdir().unwrap();
    let builds = root.path().join("builds");
    let static_root = root.path().join("static");
    let build = builds.join("20260826-demo-abc");
    fs::create_dir_all(build.join("logs")).unwrap();
    fs::create_dir(&static_root).unwrap();
    fs::write(
        static_root.join("index.html"),
        "<!doctype html><div id=\"root\">SPA</div>",
    )
    .unwrap();
    fs::write(build.join("logs/tests.log"), "hello\n").unwrap();
    fs::write(build.join("logs/pipeline.log"), "pipeline\n").unwrap();
    let status = json!({
        "build_id":"20260826-demo-abc","project":"demo","sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "ref":"refs/heads/main","type":"ci","state":"success",
        "pipeline":{"groups":{"quality":["tests"]},"jobs":{"tests":{"group":"quality","needs":[],"resolved_needs":[],"state":"success","log":"logs/tests.log"}}}
    });
    fs::write(
        build.join("status.json"),
        serde_json::to_vec(&status).unwrap(),
    )
    .unwrap();
    for path in [
        root.path(),
        &builds,
        &static_root,
        &build,
        &build.join("logs"),
    ] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    for path in [
        static_root.join("index.html"),
        build.join("status.json"),
        build.join("logs/tests.log"),
        build.join("logs/pipeline.log"),
    ] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    }

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let helper = root.path().join("web");
    fs::copy(env!("CARGO_BIN_EXE_kilnr"), &helper).unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
    let mut command = Command::new(helper);
    command
        .env("KILNR_WEB_BUILDS", &builds)
        .env("KILNR_WEB_STATIC", &static_root)
        .env("KILNR_WEB_HOST", "127.0.0.1")
        .env("KILNR_WEB_PORT", port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if unsafe { libc::geteuid() } == 0 {
        command.uid(54_001).gid(54_001);
    }
    let child = command.spawn().unwrap();
    let mut server = Server(child);
    let deadline = Instant::now() + Duration::from_secs(3);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        if let Some(status) = server.0.try_wait().unwrap() {
            let mut error = String::new();
            server
                .0
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut error)
                .unwrap();
            panic!("web server exited with {status}: {error}");
        }
        assert!(Instant::now() < deadline, "web server did not start");
        thread::sleep(Duration::from_millis(20));
    }

    let response = request(port, "/api/builds");
    assert!(response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body(&response)).unwrap()["builds"][0]
            ["build_id"],
        "20260826-demo-abc"
    );

    let response = request(port, "/build/20260826-demo-abc");
    assert!(response.contains("Content-Type: text/html"));
    assert!(body(&response).contains("SPA"));

    let response = request(port, "/api/builds/20260826-demo-abc/logs/tests");
    let log: serde_json::Value = serde_json::from_str(body(&response)).unwrap();
    assert_eq!(log["content"], "hello\n");
    assert_eq!(log["offset"], 6);

    let response = request(
        port,
        "/api/builds/20260826-demo-abc/logs/tests/stream?offset=0",
    );
    assert!(response.contains("event: chunk"));
    assert!(response.contains("hello\\n"));
    assert!(response.contains("event: end"));

    let response = request(port, "/api/builds/20260826-demo-abc/events");
    assert!(response.contains("event: end"));
    assert!(response.contains("\"state\":\"success\""));
}
