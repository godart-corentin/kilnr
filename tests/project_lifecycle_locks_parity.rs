use kilnr::{
    ops,
    project_lock::{Mode, ProjectLocks},
};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::thread;
use std::time::{Duration, Instant};

fn source() -> String {
    fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ops.rs")).unwrap()
}
fn function<'a>(source: &'a str, name: &str, next: &str) -> &'a str {
    let start = source.find(name).unwrap();
    let end = source[start..]
        .find(next)
        .map(|offset| start + offset)
        .unwrap_or(source.len());
    &source[start..end]
}
fn assert_order(text: &str, first: &str, second: &str) {
    assert!(
        text.find(first) < text.find(second),
        "{first} must precede {second}"
    );
}

#[test]
fn test_enqueue_shared_lock_spans_config_load_and_atomic_publication() {
    let source = source();
    let body = function(&source, "fn enqueue(", "fn cleanup(");
    assert_order(body, "ProjectLocks::acquire", "read_json(&config_path)");
    assert_order(body, "ProjectLocks::acquire", "atomic::write_json");
    assert!(body.contains("Mode::Shared"));
}

#[test]
fn test_enqueue_shared_lock_spans_failed_pin_cleanup() {
    let source = source();
    let body = function(&source, "fn enqueue(", "fn cleanup(");
    assert_order(body, "ProjectLocks::acquire", "update-ref\", \"-d");
    assert!(body.find("let _lock") < body.find("if result.is_err()"));
}

#[test]
fn test_delete_exclusive_lock_spans_config_validation_and_deletion() {
    let source = source();
    let body = function(&source, "fn project_delete(", "fn rewrite_json_paths(");
    assert!(body.contains("Mode::Exclusive"));
    assert_order(body, "ProjectLocks::acquire", "read_json(&cfg_path)");
    assert_order(body, "ProjectLocks::acquire", "remove_dir_all");
}

fn executable(root: &std::path::Path, body: &str) -> std::path::PathBuf {
    let path = root.join("program with spaces");
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn test_project_lock_run_executes_exact_argv_under_exclusive_lock() {
    let root = tempfile::tempdir().unwrap();
    let locks = root.path().join("locks");
    fs::create_dir(&locks).unwrap();
    fs::set_permissions(&locks, fs::Permissions::from_mode(0o750)).unwrap();
    let output = root.path().join("argv");
    let program = executable(
        root.path(),
        &format!(
            "printf '%s\\n' \"$#\" \"$1\" > '{}'\nexit 17",
            output.display()
        ),
    );
    let args = vec![
        "--exclusive".into(),
        "demo".into(),
        "--".into(),
        program.to_string_lossy().into_owned(),
        "literal;argument".into(),
    ];
    assert_eq!(ops::run_under_lock(&locks, &args).unwrap(), 17);
    assert_eq!(fs::read_to_string(output).unwrap(), "1\nliteral;argument\n");
}

#[test]
fn test_project_lock_run_normalizes_signal_exit_status() {
    let root = tempfile::tempdir().unwrap();
    let locks = root.path().join("locks");
    fs::create_dir(&locks).unwrap();
    fs::set_permissions(&locks, fs::Permissions::from_mode(0o750)).unwrap();
    let program = executable(root.path(), "kill -TERM $$");
    let args = vec![
        "--exclusive".into(),
        "demo".into(),
        "--".into(),
        program.to_string_lossy().into_owned(),
    ];
    assert_eq!(
        ops::run_under_lock(&locks, &args).unwrap(),
        128 + libc::SIGTERM
    );
}

#[test]
fn test_create_cli_dispatches_through_project_lock_wrapper() {
    let source = source();
    let body = function(&source, "pub fn cli(", "pub fn format_status(");
    assert!(body.contains("project\" && b == \"create"));
    assert!(body.contains("\"project-lock-run\""));
    assert!(body.contains("\"--exclusive\""));
    assert!(body.contains("format!(\"{LIBEXEC}/project-create\")"));
}

fn receive_identity(rollback: bool) {
    let root = tempfile::tempdir().unwrap();
    let git = root.path().join("git");
    fs::create_dir(&git).unwrap();
    let old = git.join("old.git");
    let new = git.join("new.git");
    fs::create_dir(&old).unwrap();
    fs::rename(&old, &new).unwrap();
    if rollback {
        fs::rename(&new, &old).unwrap();
    }
    let repository = if rollback { &old } else { &new };
    assert_eq!(
        ops::receive_project(&git, repository).unwrap(),
        if rollback { "old" } else { "new" }
    );
}

#[test]
fn test_actual_receive_resolves_new_identity_after_successful_rename() {
    receive_identity(false);
}
#[test]
fn test_actual_receive_resolves_restored_identity_after_rename_rollback() {
    receive_identity(true);
}

fn assert_mutator_lock(function_name: &str, next: &str, mutation: &str) {
    let source = source();
    let body = function(&source, function_name, next);
    assert!(body.contains("Mode::Shared"));
    assert_order(body, "ProjectLocks::acquire", mutation);
}

#[test]
fn test_webhook_mutation_holds_shared_lock_through_durable_write() {
    assert_mutator_lock("fn webhook(", "fn active_jobs(", "atomic::write");
}
#[test]
fn test_text_secret_mutation_holds_shared_lock_through_durable_write() {
    assert_mutator_lock("fn secret_set(", "fn secret_list(", "secrets::store");
}
#[test]
fn test_file_secret_mutation_holds_shared_lock_through_durable_write() {
    assert_mutator_lock("fn secret_set(", "fn secret_list(", "secrets::store");
}
#[test]
fn test_secret_delete_holds_shared_lock_through_durable_write() {
    assert_mutator_lock("fn secret_delete(", "fn lock_run(", "secrets::delete");
}

#[test]
fn test_lifecycle_lock_helpers_are_installed() {
    let install = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh")).unwrap();
    assert!(install.contains("project-lock-run"));
    assert!(install.contains("kilnr-agent"));
    assert!(
        std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src/project_lock.rs")).is_file()
    );
}

#[test]
fn test_project_lock_run_holds_lock_until_child_exits() {
    let root = tempfile::tempdir().unwrap();
    let locks = root.path().join("locks");
    fs::create_dir(&locks).unwrap();
    fs::set_permissions(&locks, fs::Permissions::from_mode(0o750)).unwrap();
    let ready = root.path().join("ready");
    let resume = root.path().join("resume");
    let program = executable(
        root.path(),
        &format!(
            "touch '{}'\nwhile [ ! -e '{}' ]; do sleep 0.01; done",
            ready.display(),
            resume.display()
        ),
    );
    let args = vec![
        "--exclusive".into(),
        "demo".into(),
        "--".into(),
        program.to_string_lossy().into_owned(),
    ];
    let lock_root = locks.clone();
    let worker = thread::spawn(move || ops::run_under_lock(&lock_root, &args).unwrap());
    let deadline = Instant::now() + Duration::from_secs(3);
    while !ready.exists() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    assert!(ProjectLocks::acquire(&locks, &["demo".into()], Mode::Shared, true).is_err());
    fs::write(resume, "go").unwrap();
    assert_eq!(worker.join().unwrap(), 0);
}
