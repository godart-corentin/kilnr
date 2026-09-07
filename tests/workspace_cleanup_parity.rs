use anyhow::anyhow;
use kilnr::{artifacts, ops_runtime, retention, runtime};
use serde_json::{json, Value};
use std::fs;

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    for name in [
        "src",
        "work",
        "runtime",
        "logs",
        "artifacts",
        "commands",
        "cache",
    ] {
        fs::create_dir(root.path().join(name)).unwrap();
    }
    for name in ["job.json", "runtime.json"] {
        fs::write(root.path().join(name), "{}\n").unwrap();
    }
    fs::write(root.path().join("logs/pipeline.log"), "pipeline\n").unwrap();
    root
}

fn status(root: &std::path::Path, id: &str) -> Value {
    runtime::write_json(
        &root.join("status.json"),
        &json!({
            "pipeline": {"jobs": {(id): {
                "state": "running",
                "exit_code": null,
                "artifacts": []
            }}}
        }),
    )
    .unwrap();
    fs::write(root.join("logs").join(format!("{id}.log")), "job\n").unwrap();
    json!({"artifacts": []})
}

fn stored_status(root: &std::path::Path) -> Value {
    serde_json::from_slice(&fs::read(root.join("status.json")).unwrap()).unwrap()
}

#[test]
fn ephemeral_job_subtrees_are_removed_after_success_while_outputs_remain() {
    let root = fixture();
    let id = "package";
    let mut job = status(root.path(), id);
    job["artifacts"] = json!(["dist/**"]);
    let work = root.path().join("work/package");
    fs::create_dir_all(work.join("dist")).unwrap();
    fs::write(work.join("dist/app.bin"), "artifact").unwrap();
    let runtime = root.path().join("runtime/package");
    fs::create_dir_all(runtime.join("home/.cache")).unwrap();
    fs::create_dir_all(runtime.join("tools/bin")).unwrap();
    fs::write(runtime.join("home/.cache/package.tgz"), "cache").unwrap();
    fs::write(runtime.join("tools/bin/tool"), "tool").unwrap();
    fs::write(root.path().join("cache/pnpm-store"), "persistent").unwrap();

    assert_eq!(
        ops_runtime::complete_job(root.path(), id, &job, "2026-01-01T00:00:00Z", Ok(0),).unwrap(),
        0
    );

    assert!(!work.exists());
    assert!(!runtime.exists());
    assert_eq!(
        fs::read(root.path().join("artifacts/package/dist/app.bin")).unwrap(),
        b"artifact"
    );
    for path in [
        "src",
        "logs/package.log",
        "logs/pipeline.log",
        "artifacts",
        "commands",
        "job.json",
        "runtime.json",
        "status.json",
        "cache/pnpm-store",
    ] {
        assert!(root.path().join(path).exists(), "{path}");
    }
    let stored = stored_status(root.path());
    assert_eq!(stored["pipeline"]["jobs"][id]["state"], "success");
    assert_eq!(stored["pipeline"]["jobs"][id]["exit_code"], 0);
    assert_eq!(
        stored["pipeline"]["jobs"][id]["artifacts"],
        json!(["dist/app.bin"])
    );
}

#[test]
fn ephemeral_job_subtrees_are_removed_after_failure_and_timeout() {
    for (id, execution, expected_code) in [("failed", Ok(7), 7), ("timeout", Ok(124), 124)] {
        let root = fixture();
        let job = status(root.path(), id);
        let work = root.path().join("work").join(id);
        fs::create_dir(&work).unwrap();
        fs::write(work.join("large.tmp"), "temporary").unwrap();
        let runtime = root.path().join("runtime").join(id);
        fs::create_dir(&runtime).unwrap();
        fs::write(runtime.join("cache.tmp"), "temporary").unwrap();

        assert_eq!(
            ops_runtime::complete_job(root.path(), id, &job, "2026-01-01T00:00:00Z", execution,)
                .unwrap(),
            expected_code
        );
        assert!(!work.exists());
        assert!(!runtime.exists());
        let stored = stored_status(root.path());
        assert_eq!(stored["pipeline"]["jobs"][id]["state"], "failed");
        assert_eq!(stored["pipeline"]["jobs"][id]["exit_code"], expected_code);
        assert!(root.path().join(format!("logs/{id}.log")).is_file());
    }
}

#[test]
fn execution_and_artifact_errors_still_remove_ephemeral_job_subtrees() {
    for (id, job, execution, message) in [
        (
            "runtime-error",
            json!({"artifacts": []}),
            Err(anyhow!("docker spawn failed")),
            "docker spawn failed",
        ),
        (
            "artifact-error",
            json!({"artifacts": ["missing/**"]}),
            Ok(0),
            "artifact pattern matched no files",
        ),
    ] {
        let root = fixture();
        status(root.path(), id);
        let work = root.path().join("work").join(id);
        fs::create_dir(&work).unwrap();
        let runtime = root.path().join("runtime").join(id);
        fs::create_dir(&runtime).unwrap();
        let error =
            ops_runtime::complete_job(root.path(), id, &job, "2026-01-01T00:00:00Z", execution)
                .unwrap_err();
        assert!(format!("{error:#}").contains(message));
        assert!(!work.exists());
        assert!(!runtime.exists());
        assert_eq!(
            stored_status(root.path())["pipeline"]["jobs"][id]["state"],
            "failed"
        );
    }
}

#[test]
fn failed_terminal_status_update_preserves_workspace_for_diagnosis() {
    let root = fixture();
    let id = "status-error";
    let work = root.path().join("work").join(id);
    fs::create_dir(&work).unwrap();
    fs::write(work.join("diagnostic.tmp"), "keep").unwrap();
    let runtime = root.path().join("runtime").join(id);
    fs::create_dir(&runtime).unwrap();
    fs::write(runtime.join("diagnostic.tmp"), "keep").unwrap();
    fs::write(root.path().join("status.json"), "not json").unwrap();

    ops_runtime::complete_job(
        root.path(),
        id,
        &json!({"artifacts": []}),
        "2026-01-01T00:00:00Z",
        Ok(0),
    )
    .unwrap_err();

    assert!(work.join("diagnostic.tmp").is_file());
    assert!(runtime.join("diagnostic.tmp").is_file());
}

#[test]
fn failed_execution_log_write_preserves_workspace_for_diagnosis() {
    let root = fixture();
    let id = "log-error";
    status(root.path(), id);
    let work = root.path().join("work").join(id);
    fs::create_dir(&work).unwrap();
    fs::write(work.join("diagnostic.tmp"), "keep").unwrap();
    let runtime = root.path().join("runtime").join(id);
    fs::create_dir(&runtime).unwrap();
    fs::write(runtime.join("diagnostic.tmp"), "keep").unwrap();
    fs::remove_dir_all(root.path().join("logs")).unwrap();

    let error = ops_runtime::complete_job(
        root.path(),
        id,
        &json!({"artifacts": []}),
        "2026-01-01T00:00:00Z",
        Err(anyhow!("docker failed")),
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("job log write also failed"));
    assert!(work.join("diagnostic.tmp").is_file());
    assert!(runtime.join("diagnostic.tmp").is_file());
}

#[test]
fn dependent_inputs_use_collected_artifacts_after_producer_workspace_cleanup() {
    let root = fixture();
    let id = "producer";
    let mut job = status(root.path(), id);
    job["artifacts"] = json!(["out/**"]);
    let work = root.path().join("work/producer");
    fs::create_dir_all(work.join("out")).unwrap();
    fs::write(work.join("out/result.txt"), "result").unwrap();
    let runtime = root.path().join("runtime/producer");
    fs::create_dir(&runtime).unwrap();
    fs::write(runtime.join("temporary"), "runtime").unwrap();
    ops_runtime::complete_job(root.path(), id, &job, "2026-01-01T00:00:00Z", Ok(0)).unwrap();

    let roots = artifacts::input_roots(root.path(), &[id.into()]).unwrap();
    assert!(!work.exists());
    assert!(!runtime.exists());
    assert_eq!(
        fs::read(roots[id].join("out/result.txt")).unwrap(),
        b"result"
    );
}

#[cfg(unix)]
#[test]
fn workspace_cleanup_unlinks_symlinks_without_touching_external_targets() {
    use std::os::unix::fs::symlink;

    let root = fixture();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("keep.txt"), "keep").unwrap();
    let work = root.path().join("work/symlinks");
    fs::create_dir(&work).unwrap();
    symlink(outside.path(), work.join("outside-dir")).unwrap();
    symlink(outside.path().join("missing"), work.join("dangling")).unwrap();

    retention::remove_job_workspace(&root.path().join("work"), "symlinks").unwrap();

    assert!(!work.exists());
    assert_eq!(fs::read(outside.path().join("keep.txt")).unwrap(), b"keep");
}

#[cfg(unix)]
#[test]
fn runtime_cleanup_unlinks_symlinks_without_touching_external_targets() {
    use std::os::unix::fs::symlink;

    let root = fixture();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("keep.txt"), "keep").unwrap();
    let runtime = root.path().join("runtime/symlinks");
    fs::create_dir(&runtime).unwrap();
    symlink(outside.path(), runtime.join("outside-dir")).unwrap();

    retention::remove_job_subtree(&root.path().join("runtime"), "symlinks").unwrap();

    assert!(!runtime.exists());
    assert_eq!(fs::read(outside.path().join("keep.txt")).unwrap(), b"keep");
}

#[test]
fn job_subtree_cleanup_rejects_invalid_job_ids() {
    let root = fixture();
    let runtime = root.path().join("runtime/valid");
    fs::create_dir(&runtime).unwrap();

    let error =
        retention::remove_job_subtree(&root.path().join("runtime"), "../valid").unwrap_err();

    assert!(format!("{error:#}").contains("invalid job subtree id"));
    assert!(runtime.exists());
}

#[test]
fn runtime_cleanup_failure_does_not_mask_an_execution_failure() {
    let root = fixture();
    let id = "failed-runtime-cleanup";
    status(root.path(), id);
    fs::create_dir(root.path().join("work").join(id)).unwrap();
    fs::remove_dir(root.path().join("runtime")).unwrap();
    fs::write(root.path().join("runtime"), "not a directory").unwrap();

    let error = ops_runtime::complete_job(
        root.path(),
        id,
        &json!({"artifacts": []}),
        "2026-01-01T00:00:00Z",
        Err(anyhow!("docker spawn failed")),
    )
    .unwrap_err();
    let message = format!("{error:#}");

    assert!(message.contains("docker spawn failed"));
    assert!(message.contains("runtime cleanup failed"));
    assert_eq!(
        stored_status(root.path())["pipeline"]["jobs"][id]["exit_code"],
        1
    );
}

#[test]
fn runtime_cleanup_failure_preserves_a_nonzero_execution_exit_code() {
    let root = fixture();
    let id = "failed-command-cleanup";
    status(root.path(), id);
    fs::create_dir(root.path().join("work").join(id)).unwrap();
    fs::remove_dir(root.path().join("runtime")).unwrap();
    fs::write(root.path().join("runtime"), "not a directory").unwrap();

    let rc = ops_runtime::complete_job(
        root.path(),
        id,
        &json!({"artifacts": []}),
        "2026-01-01T00:00:00Z",
        Ok(7),
    )
    .unwrap();

    assert_eq!(rc, 7);
    let stored = stored_status(root.path());
    assert_eq!(stored["pipeline"]["jobs"][id]["state"], "failed");
    assert_eq!(stored["pipeline"]["jobs"][id]["exit_code"], 7);
    assert_eq!(
        stored["pipeline"]["jobs"][id]["runtime_cleanup"]["state"],
        "failed"
    );
}

#[test]
fn runtime_cleanup_failure_fails_an_otherwise_successful_job() {
    let root = fixture();
    let id = "successful-runtime-cleanup";
    status(root.path(), id);
    fs::create_dir(root.path().join("work").join(id)).unwrap();
    fs::remove_dir(root.path().join("runtime")).unwrap();
    fs::write(root.path().join("runtime"), "not a directory").unwrap();

    let error = ops_runtime::complete_job(
        root.path(),
        id,
        &json!({"artifacts": []}),
        "2026-01-01T00:00:00Z",
        Ok(0),
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("unsafe managed directory"));
    let stored = stored_status(root.path());
    assert_eq!(stored["pipeline"]["jobs"][id]["state"], "failed");
    assert_eq!(stored["pipeline"]["jobs"][id]["exit_code"], 1);
    assert_eq!(
        stored["pipeline"]["jobs"][id]["runtime_cleanup"]["state"],
        "failed"
    );
    assert!(
        fs::read_to_string(root.path().join(format!("logs/{id}.log")))
            .unwrap()
            .contains("runtime cleanup failed")
    );
}
