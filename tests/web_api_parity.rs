use kilnr::web;
use serde_json::{json, Value};
use std::fs;
use std::io::Read;
use std::path::Path;

fn write_status(build: &Path) -> Value {
    let status = json!({
        "build_id":build.file_name().unwrap().to_str().unwrap(), "project":"demo",
        "sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "ref":"refs/heads/main",
        "type":"ci", "state":"running",
        "pipeline":{"groups":{"quality":["tests"]},"jobs":{"tests":{
            "group":"quality","needs":[],"resolved_needs":[],"state":"running","log":"logs/tests.log"
        }}}
    });
    fs::write(
        build.join("status.json"),
        serde_json::to_vec(&status).unwrap(),
    )
    .unwrap();
    status
}

#[test]
fn test_build_lookup_and_traversal() {
    let root = tempfile::tempdir().unwrap();
    let build = root.path().join("20260826-demo-abc");
    fs::create_dir(&build).unwrap();
    write_status(&build);
    assert_eq!(
        web::get_build(root.path(), "20260826-demo-abc").unwrap().1["project"],
        "demo"
    );
    assert!(web::get_build(root.path(), "../etc").is_none());
    assert!(web::get_build(root.path(), "bad/name").is_none());
}

#[test]
fn test_list_builds_newest_first() {
    let root = tempfile::tempdir().unwrap();
    for name in ["20260825-old", "20260826-new"] {
        let build = root.path().join(name);
        fs::create_dir(&build).unwrap();
        write_status(&build);
    }
    let builds = web::builds(root.path());
    assert_eq!(builds[0]["build_id"], "20260826-new");
    assert_eq!(builds[1]["build_id"], "20260825-old");
}

#[test]
fn test_log_snapshot_uses_raw_byte_offset_and_sanitizes() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("tests.log");
    let raw = b"hello\n\x1b[31mred\x1b[0m\n";
    fs::write(&path, raw).unwrap();
    let snapshot = web::log_snapshot(&path).unwrap();
    assert_eq!(snapshot["offset"], raw.len());
    assert_eq!(snapshot["content"], "hello\nred\n");
    assert_eq!(snapshot["truncated"], false);
}

#[test]
fn test_read_appended_log_chunk() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("tests.log");
    fs::write(&path, b"old\nnew\n").unwrap();
    assert_eq!(web::read_chunk(&path, 4).unwrap(), (8, "new\n".into()));
}

#[test]
fn test_artifacts_do_not_follow_symlinks() {
    let root = tempfile::tempdir().unwrap();
    let build = root.path().join("build");
    let artifacts = build.join("artifacts/job");
    fs::create_dir_all(&artifacts).unwrap();
    fs::write(artifacts.join("ok.txt"), "ok").unwrap();
    fs::write(root.path().join("outside.txt"), "secret").unwrap();
    std::os::unix::fs::symlink(root.path().join("outside.txt"), artifacts.join("escape")).unwrap();
    assert_eq!(
        web::artifacts(&build),
        json!([{"path":"job/ok.txt","size":2}])
    );
}

#[test]
fn test_log_path_cannot_escape_logs_directory() {
    let root = tempfile::tempdir().unwrap();
    let build = root.path().join("build");
    fs::create_dir_all(build.join("logs")).unwrap();
    fs::write(root.path().join("outside.log"), "secret").unwrap();
    let mut status = write_status(&build);
    status["pipeline"]["jobs"]["tests"]["log"] = json!("../outside.log");
    assert!(web::log_path(&build, &status, "tests").is_none());
}

#[test]
fn test_diff_status_events_returns_only_deltas() {
    let previous = json!({"state":"running","pipeline":{"jobs":{"tests":{"state":"running"},"build":{"state":"pending"}}}});
    let current = json!({"state":"success","duration_seconds":4.2,"pipeline":{"jobs":{"tests":{"state":"success","duration_seconds":2.0},"build":{"state":"success"}}}});
    let events = web::diff_status_events(&previous, &current);
    assert!(events.contains(&(
        "job".into(),
        json!({"name":"tests","state":"success","duration_seconds":2.0})
    )));
    assert!(events.contains(&("job".into(), json!({"name":"build","state":"success"}))));
    assert!(events.contains(&(
        "build".into(),
        json!({"state":"success","duration_seconds":4.2})
    )));
}

#[test]
fn test_job_terminal() {
    let mut status = json!({"state":"running","pipeline":{"jobs":{"tests":{"state":"failed"}}}});
    assert!(web::is_terminal(&status, "tests"));
    assert!(!web::is_terminal(&status, "pipeline"));
    status["state"] = json!("failed");
    assert!(web::is_terminal(&status, "pipeline"));
}

#[test]
fn test_build_disappears_between_listing_and_status_read() {
    let root = tempfile::tempdir().unwrap();
    let build = root.path().join("20260826-demo-abc");
    fs::create_dir(&build).unwrap();
    write_status(&build);
    fs::remove_dir_all(&build).unwrap();
    assert_eq!(web::builds(root.path()), json!([]));
    assert!(web::get_build(root.path(), "20260826-demo-abc").is_none());
    assert_eq!(web::artifacts(&build), json!([]));
}

#[test]
fn test_cleanup_transactions_do_not_consume_listing_limit() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..=web::MAX_BUILDS {
        fs::create_dir(root.path().join(format!(".cleanup-{index}"))).unwrap();
    }
    let build = root.path().join("20260826-demo-abc");
    fs::create_dir(&build).unwrap();
    write_status(&build);
    assert_eq!(web::builds(root.path())[0]["build_id"], "20260826-demo-abc");
    assert!(web::get_build(root.path(), ".cleanup-0").is_none());
}

#[test]
fn test_live_streams_end_when_build_disappears() {
    let root = tempfile::tempdir().unwrap();
    let build = root.path().join("build");
    fs::create_dir_all(build.join("logs")).unwrap();
    let log = build.join("logs/tests.log");
    fs::write(&log, "hello").unwrap();
    let status = write_status(&build);
    let response = web::log_stream_response(build.clone(), status.clone(), "tests".into(), log, 0);
    fs::remove_dir_all(&build).unwrap();
    let mut output = String::new();
    response.into_reader().read_to_string(&mut output).unwrap();
    assert!(output.contains("event: end"));
    assert!(output.contains("\"state\":\"deleted\""));

    fs::create_dir_all(build.join("logs")).unwrap();
    write_status(&build);
    let response = web::event_response(build.clone(), status);
    fs::remove_dir_all(&build).unwrap();
    output.clear();
    response.into_reader().read_to_string(&mut output).unwrap();
    assert!(output.contains("event: end"));
    assert!(output.contains("\"state\":\"deleted\""));
}
