use kilnr::{artifacts, pipeline, project_lock, secrets};
use serde_json::json;
use std::fs;

fn networks() -> Vec<String> {
    vec!["none".into(), "kilnr-ci".into()]
}

#[test]
fn pipeline_normalizes_dependencies_groups_and_package_manager_tools() {
    let value = json!({
        "schema": 1,
        "trigger": {"type":"branch", "branches":["main", "feature/*"]},
        "jobs": {
            "compile": {"image":"node:22", "run":["pnpm build"], "group":"build", "artifacts":["dist/**"], "tools":["pnpm"], "cache":["pnpm"]},
            "test": {"image":"node:22", "command":["pnpm","test"], "needs":["build"], "inputs":["compile"]}
        }
    });
    let parsed = pipeline::load(
        &serde_json::to_vec(&value).unwrap(),
        "ci",
        3,
        &networks(),
        Some("pnpm@9.15.0+sha512.deadbeef"),
    )
    .unwrap();
    assert_eq!(parsed["jobs"]["test"]["resolved_needs"], json!(["compile"]));
    assert_eq!(parsed["jobs"]["compile"]["tools"]["pnpm"], "9.15.0");
    assert!(pipeline::matches_branch(&parsed, "feature/rust"));
}

#[test]
fn pipeline_rejects_cycles_reserved_environment_and_ci_secrets() {
    let cycle = json!({"schema":1,"trigger":{"type":"branch","branches":["*"]},"jobs":{"a":{"image":"x","run":["true"],"needs":["b"]},"b":{"image":"x","run":["true"],"needs":["a"]}}});
    assert!(pipeline::load(
        &serde_json::to_vec(&cycle).unwrap(),
        "ci",
        3,
        &networks(),
        None
    )
    .unwrap_err()
    .to_string()
    .contains("cycle"));
    let reserved = json!({"schema":1,"trigger":{"type":"branch","branches":["*"]},"jobs":{"a":{"image":"x","run":["true"],"env":{"KILNR_SHA":"fake"}}}});
    assert!(pipeline::load(
        &serde_json::to_vec(&reserved).unwrap(),
        "ci",
        3,
        &networks(),
        None
    )
    .is_err());
    let secret = json!({"schema":1,"trigger":{"type":"branch","branches":["*"]},"jobs":{"a":{"image":"x","run":["true"],"secrets":["TOKEN"]}}});
    assert!(pipeline::load(
        &serde_json::to_vec(&secret).unwrap(),
        "ci",
        3,
        &networks(),
        None
    )
    .is_err());
}

#[test]
fn secrets_are_atomic_validated_and_never_list_values() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("project")).unwrap();
    secrets::store(temp.path(), "project", "TOKEN", b"sensitive", "text").unwrap();
    assert_eq!(
        secrets::read(temp.path(), "project", "TOKEN").unwrap(),
        b"sensitive"
    );
    let listed = secrets::list(temp.path(), "project").unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0, "TOKEN");
    assert!(secrets::store(temp.path(), "project", "KILNR_SHA", b"bad", "text").is_err());
    assert!(secrets::store(temp.path(), "project", "BINARY", b"a\0b", "text").is_err());
    secrets::delete(temp.path(), "project", "TOKEN").unwrap();
    assert!(secrets::read(temp.path(), "project", "TOKEN").is_err());
}

#[test]
fn artifact_collection_preserves_paths_and_rejects_symlinks() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("work");
    let dest = temp.path().join("out");
    fs::create_dir_all(workspace.join("dist/nested")).unwrap();
    fs::write(workspace.join("dist/a.txt"), "a").unwrap();
    fs::write(workspace.join("dist/nested/b.txt"), "b").unwrap();
    let result = artifacts::collect(&workspace, &["dist/**/*.txt".into()], &dest).unwrap();
    assert_eq!(result, vec!["dist/a.txt", "dist/nested/b.txt"]);
    assert_eq!(
        fs::read_to_string(dest.join("dist/nested/b.txt")).unwrap(),
        "b"
    );
    std::os::unix::fs::symlink("a.txt", workspace.join("dist/link.txt")).unwrap();
    assert!(artifacts::collect(&workspace, &["dist/link.txt".into()], &dest).is_err());
}

#[test]
fn project_names_and_existing_lock_files_are_hardened() {
    assert!(project_lock::validate_name("good-project_1").is_ok());
    assert!(project_lock::validate_name("../escape").is_err());
    let temp = tempfile::tempdir().unwrap();
    project_lock::provision(temp.path(), &["project".into()]).unwrap();
    let _guard = project_lock::ProjectLocks::acquire(
        temp.path(),
        &["project".into()],
        project_lock::Mode::Exclusive,
        false,
    )
    .unwrap();
}
