use kilnr::{ops_runtime, pipeline, runtime, secrets};
use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::PermissionsExt;

fn job() -> Value {
    json!({"schema":1,"id":"20260826T000000000000Z-demo-abcdef0-12345678","project":"demo","received_at":"2026-08-26T00:00:00Z","old_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","new_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","ref":"refs/heads/main","type":"ci","event":"push","branch":"main","pin_ref":"refs/kilnr/jobs/20260826T000000000000Z-demo-abcdef0-12345678"})
}

fn config(max_parallel: u64) -> Value {
    json!({"runner":{"max_parallel":max_parallel,"cpus":"0.75","memory":"768m","pids_limit":256,"timeout_seconds":1800,"allowed_networks":["none","kilnr-ci"]}})
}

fn normalized(requested: u64) -> Value {
    let raw = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"max_parallel":requested,"jobs":{
        "lint":{"group":"quality","image":"alpine:3.22","run":["echo lint"]},
        "tests":{"group":"quality","image":"alpine:3.22","run":["echo tests"]},
        "build":{"group":"build-group","needs":["quality"],"image":"alpine:3.22","run":["echo build"]},
        "package":{"needs":["build"],"image":"alpine:3.22","command":["true"]}
    }});
    pipeline::load(
        &serde_json::to_vec(&raw).unwrap(),
        "ci",
        3,
        &["none".into(), "kilnr-ci".into()],
        None,
    )
    .unwrap()
}

#[test]
fn test_runtime_uses_jobs_and_caps_parallelism() {
    let value = ops_runtime::resolve_pipeline(
        &job(),
        &config(3),
        ".kilnr/pipelines/ci.json",
        &normalized(8),
        None,
    )
    .unwrap();
    assert!(value.get("jobs").is_some());
    assert!(value.get("steps").is_none());
    assert_eq!(
        value["groups"],
        json!({"build-group":["build"],"quality":["lint","tests"]})
    );
    assert_eq!(
        value["jobs"]["build"]["resolved_needs"],
        json!(["lint", "tests"])
    );
    assert_eq!(value["jobs"]["package"]["resolved_needs"], json!(["build"]));
    assert_eq!(value["max_parallel"], 3);
}

#[test]
fn test_makefile_uses_resolved_job_dependencies_without_group_targets() {
    let value = ops_runtime::resolve_pipeline(
        &job(),
        &config(4),
        ".kilnr/pipelines/ci.json",
        &normalized(4),
        None,
    )
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    ops_runtime::write_makefile(root.path(), &value).unwrap();
    let text = fs::read_to_string(root.path().join("pipeline.mk")).unwrap();
    assert!(text.contains("job-build: job-lint job-tests"));
    assert!(text.contains("job-package: job-build"));
    assert!(!text.contains("job-quality:"));
    assert!(text.contains("/usr/local/libexec/kilnr/execute"));
    assert!(text.contains(value["build_id"].as_str().unwrap()));
}

#[test]
fn test_status_contains_resolved_pipeline_jobs() {
    let value = ops_runtime::resolve_pipeline(
        &job(),
        &config(4),
        ".kilnr/pipelines/ci.json",
        &normalized(4),
        None,
    )
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("logs")).unwrap();
    let status = ops_runtime::initial_status(&job(), Some(".kilnr/pipelines/ci.json"));
    runtime::write_json(&root.path().join("status.json"), &status).unwrap();
    ops_runtime::start_pipeline_status(status, &value, root.path()).unwrap();
    let stored: Value =
        serde_json::from_slice(&fs::read(root.path().join("status.json")).unwrap()).unwrap();
    assert_eq!(stored["pipeline_path"], ".kilnr/pipelines/ci.json");
    assert_eq!(stored["prepare"]["state"], "success");
    assert_eq!(
        stored["pipeline"]["groups"],
        json!({"build-group":["build"],"quality":["lint","tests"]})
    );
    assert_eq!(
        stored["pipeline"]["jobs"]["build"]["needs"],
        json!(["quality"])
    );
    assert_eq!(
        stored["pipeline"]["jobs"]["build"]["resolved_needs"],
        json!(["lint", "tests"])
    );
    assert_eq!(stored["pipeline"]["jobs"]["build"]["state"], "pending");
    assert!(stored["pipeline"]["jobs"]["build"].get("run").is_none());
}

#[test]
fn test_finalize_marks_pending_jobs_skipped() {
    let value = ops_runtime::resolve_pipeline(
        &job(),
        &config(4),
        ".kilnr/pipelines/ci.json",
        &normalized(4),
        None,
    )
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let status = ops_runtime::initial_status(&job(), Some(".kilnr/pipelines/ci.json"));
    runtime::write_json(&root.path().join("status.json"), &status).unwrap();
    ops_runtime::start_pipeline_status(status, &value, root.path()).unwrap();
    let mut stored: Value =
        serde_json::from_slice(&fs::read(root.path().join("status.json")).unwrap()).unwrap();
    stored["pipeline"]["jobs"]["lint"]["state"] = json!("failed");
    stored["pipeline"]["jobs"]["tests"]["state"] = json!("success");
    runtime::write_json(&root.path().join("status.json"), &stored).unwrap();
    assert_eq!(ops_runtime::finalize(root.path(), 2).unwrap(), "failed");
    let final_status: Value =
        serde_json::from_slice(&fs::read(root.path().join("status.json")).unwrap()).unwrap();
    assert_eq!(
        final_status["pipeline"]["jobs"]["build"]["state"],
        "skipped"
    );
    assert_eq!(
        final_status["pipeline"]["jobs"]["package"]["state"],
        "skipped"
    );
}

fn release_job() -> Value {
    let mut value = job();
    value["type"] = json!("release");
    value["event"] = json!("tag");
    value["ref"] = json!("refs/tags/v1.2.3");
    value["tag"] = json!("v1.2.3");
    value.as_object_mut().unwrap().remove("branch");
    value
}

fn release_pipeline() -> Value {
    let raw = json!({"schema":1,"jobs":{"publish":{"image":"alpine:3.22","run":["true"],"secrets":["APPLE_ID"]}}});
    pipeline::load(
        &serde_json::to_vec(&raw).unwrap(),
        "release",
        3,
        &["none".into(), "kilnr-ci".into()],
        None,
    )
    .unwrap()
}

#[test]
fn test_release_pipeline_secrets_are_validated_before_runtime() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("demo")).unwrap();
    secrets::store(root.path(), "demo", "APPLE_ID", b"dev@example.com", "text").unwrap();
    let value = ops_runtime::resolve_pipeline(
        &release_job(),
        &config(3),
        ".kilnr/release.json",
        &release_pipeline(),
        Some(root.path()),
    )
    .unwrap();
    assert_eq!(value["jobs"]["publish"]["secrets"], json!(["APPLE_ID"]));
}

#[test]
fn test_missing_release_secret_fails_before_execution() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("demo")).unwrap();
    let error = ops_runtime::resolve_pipeline(
        &release_job(),
        &config(3),
        ".kilnr/release.json",
        &release_pipeline(),
        Some(root.path()),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("APPLE_ID"));
}

#[test]
fn test_write_json_clamps_metadata_mode() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("status.json");
    runtime::write_json(&path, &json!({"schema":1})).unwrap();
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o640
    );
}
