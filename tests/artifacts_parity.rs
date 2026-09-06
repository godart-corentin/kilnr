use kilnr::artifacts;
use std::fs;

#[test]
fn test_collects_globs_and_preserves_layout() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let destination = root.path().join("dest");
    fs::create_dir_all(workspace.join("dist/nested")).unwrap();
    fs::write(workspace.join("dist/app.js"), "app").unwrap();
    fs::write(workspace.join("dist/nested/chunk.js"), "chunk").unwrap();
    fs::create_dir(workspace.join("coverage")).unwrap();
    fs::write(workspace.join("coverage/index.html"), "coverage").unwrap();
    let collected = artifacts::collect(
        &workspace,
        &["dist/**".into(), "coverage/index.html".into()],
        &destination,
    )
    .unwrap();
    assert_eq!(
        collected,
        ["coverage/index.html", "dist/app.js", "dist/nested/chunk.js"]
    );
    assert_eq!(
        fs::read_to_string(destination.join("dist/app.js")).unwrap(),
        "app"
    );
    assert_eq!(
        fs::read_to_string(destination.join("dist/nested/chunk.js")).unwrap(),
        "chunk"
    );
    assert_eq!(
        fs::read_to_string(destination.join("coverage/index.html")).unwrap(),
        "coverage"
    );
}

#[test]
fn test_each_declared_pattern_must_match_a_file() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let error = artifacts::collect(
        &workspace,
        &["missing/*.zip".into()],
        &root.path().join("dest"),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("matched no files"));
}

#[test]
fn test_rejects_symlinks_even_when_target_is_inside_workspace() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    fs::create_dir_all(workspace.join("dist")).unwrap();
    fs::write(workspace.join("real.bin"), "ok").unwrap();
    std::os::unix::fs::symlink("../real.bin", workspace.join("dist/link.bin")).unwrap();
    let error = artifacts::collect(&workspace, &["dist/*".into()], &root.path().join("dest"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("symlink"));
}

#[test]
fn test_resolve_input_roots_keeps_producers_separate() {
    let root = tempfile::tempdir().unwrap();
    for producer in ["linux", "windows"] {
        let path = root.path().join("artifacts").join(producer);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join(format!("{producer}.txt")), producer).unwrap();
    }
    let roots = artifacts::input_roots(root.path(), &["linux".into(), "windows".into()]).unwrap();
    assert_eq!(roots["linux"], root.path().join("artifacts/linux"));
    assert_eq!(roots["windows"], root.path().join("artifacts/windows"));
}

#[test]
fn test_missing_input_artifacts_fail_clearly() {
    let root = tempfile::tempdir().unwrap();
    let error = artifacts::input_roots(root.path(), &["linux".into()])
        .unwrap_err()
        .to_string();
    assert!(error.contains("linux"));
}
