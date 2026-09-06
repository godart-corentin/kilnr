use kilnr::ops;
use serde_json::{json, Value};
use std::fs;

fn status(state: &str, job_state: &str) -> Value {
    json!({"schema":1,"build_id":"build-1","job_id":"build-1","project":"demo","sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","ref":"refs/heads/main","type":"ci","state":state,"started_at":"2026-08-26T00:00:00Z","duration_seconds":null,"prepare":{"state":"success","log":"logs/prepare.log"},"pipeline_path":".kilnr/pipelines/ci.json","pipeline":{"groups":{"quality":["tests"]},"jobs":{"tests":{"group":"quality","needs":[],"resolved_needs":[],"state":job_state,"duration_seconds":null,"log":"logs/tests.log"}}}})
}

#[test]
fn test_cli_status_lists_jobs_and_groups() {
    let text = ops::format_status(&status("running", "running"));
    assert!(text.contains("tests"));
    assert!(text.contains("quality"));
    assert!(!text.contains("Steps"));
}

#[test]
fn test_cli_terminal_state_reads_pipeline_job() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("status.json"),
        serde_json::to_vec(&status("running", "success")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        ops::terminal(root.path(), "tests").unwrap().as_deref(),
        Some("success")
    );
}

#[test]
fn test_discord_groups_jobs_by_group() {
    let text = ops::render_notification(&status("success", "success"));
    assert!(text.contains("**Quality**"));
    assert!(text.contains("tests"));
}
