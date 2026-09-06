use kilnr::{ops, ops_runtime};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

struct Repo {
    _root: tempfile::TempDir,
    work: PathBuf,
    bare: PathBuf,
    sha: String,
}

fn pipeline(branches: &[&str], name: &str) -> Value {
    json!({"schema":1,"trigger":{"type":"branch","branches":branches},"jobs":{name:{"image":"alpine:3.22","network":"none","run":["true"]}}})
}

fn make_repo(files: &[(&str, Value)]) -> Repo {
    let root = tempfile::tempdir().unwrap();
    let work = root.path().join("work");
    let bare = root.path().join("repo.git");
    fs::create_dir(&work).unwrap();
    run(&work, &["init", "-b", "main"]);
    run(
        &work,
        &["config", "user.email", "kilnr-test@example.invalid"],
    );
    run(&work, &["config", "user.name", "Kilnr Test"]);
    for (relative, value) in files {
        let path = work.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut bytes = serde_json::to_vec_pretty(value).unwrap();
        bytes.push(b'\n');
        fs::write(path, bytes).unwrap();
    }
    run(&work, &["add", "."]);
    run(&work, &["commit", "-m", "test"]);
    let sha = run(&work, &["rev-parse", "HEAD"]);
    let output = Command::new("git")
        .args([
            "clone",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    Repo {
        _root: root,
        work,
        bare,
        sha,
    }
}

fn config(repo: &Path) -> Value {
    json!({"schema":1,"project":"demo","repository":repo,"runner":{"max_parallel":3,"cpus":"0.75","memory":"768m","pids_limit":256,"timeout_seconds":1800,"allowed_networks":["none","kilnr-ci"]},"release":{"tag_pattern":"^v[0-9]+\\.[0-9]+\\.[0-9]+$"}})
}

fn ci_job(sha: &str, branch: &str) -> Value {
    json!({"schema":1,"id":"20260826T000000000000Z-demo-abcdef0-12345678","project":"demo","received_at":"2026-08-26T00:00:00Z","old_sha":sha,"new_sha":sha,"sha":sha,"ref":format!("refs/heads/{branch}"),"type":"ci","event":"push","branch":branch,"pin_ref":"refs/kilnr/jobs/20260826T000000000000Z-demo-abcdef0-12345678"})
}

fn release_job(sha: &str) -> Value {
    let mut value = ci_job(sha, "main");
    value["ref"] = json!("refs/tags/v1.2.3");
    value["type"] = json!("release");
    value["event"] = json!("tag");
    value["tag"] = json!("v1.2.3");
    value.as_object_mut().unwrap().remove("branch");
    value
}

#[test]
fn test_enqueue_classifies_every_branch_as_ci() {
    let cfg = json!({"release":{"tag_pattern":"^v[0-9]+\\.[0-9]+\\.[0-9]+$"}});
    for reference in ["refs/heads/main", "refs/heads/feature/special"] {
        assert_eq!(
            ops::classify_job(&cfg, &"b".repeat(40), &"a".repeat(40), reference)
                .unwrap()
                .as_deref(),
            Some("ci")
        );
    }
}

#[test]
fn test_branch_pipeline_selection() {
    let repo = make_repo(&[
        (
            ".kilnr/pipelines/main.json",
            pipeline(&["main"], "main-check"),
        ),
        (
            ".kilnr/pipelines/features.json",
            pipeline(&["feature/*"], "feature-check"),
        ),
    ]);
    let (path, selected) =
        ops_runtime::select_pipeline(&ci_job(&repo.sha, "main"), &config(&repo.bare))
            .unwrap()
            .unwrap();
    assert_eq!(path, ".kilnr/pipelines/main.json");
    assert!(selected["jobs"].get("main-check").is_some());
    let (path, selected) =
        ops_runtime::select_pipeline(&ci_job(&repo.sha, "feature/login"), &config(&repo.bare))
            .unwrap()
            .unwrap();
    assert_eq!(path, ".kilnr/pipelines/features.json");
    assert!(selected["jobs"].get("feature-check").is_some());
    assert!(
        ops_runtime::select_pipeline(&ci_job(&repo.sha, "docs"), &config(&repo.bare))
            .unwrap()
            .is_none()
    );
}

#[test]
fn test_selection_uses_exact_job_sha() {
    let repo = make_repo(&[(".kilnr/pipelines/main.json", pipeline(&["main"], "old-job"))]);
    fs::write(
        repo.work.join(".kilnr/pipelines/main.json"),
        serde_json::to_vec(&pipeline(&["main"], "new-job")).unwrap(),
    )
    .unwrap();
    run(&repo.work, &["add", "."]);
    run(&repo.work, &["commit", "-m", "new pipeline"]);
    let new_sha = run(&repo.work, &["rev-parse", "HEAD"]);
    run(&repo.work, &["push", repo.bare.to_str().unwrap(), "main"]);
    let old = ops_runtime::select_pipeline(&ci_job(&repo.sha, "main"), &config(&repo.bare))
        .unwrap()
        .unwrap()
        .1;
    let new = ops_runtime::select_pipeline(&ci_job(&new_sha, "main"), &config(&repo.bare))
        .unwrap()
        .unwrap()
        .1;
    assert!(old["jobs"].get("old-job").is_some() && old["jobs"].get("new-job").is_none());
    assert!(new["jobs"].get("new-job").is_some());
}

#[test]
fn test_multiple_branch_pipeline_matches_fail() {
    let repo = make_repo(&[
        (
            ".kilnr/pipelines/all.json",
            pipeline(&["feature/*"], "all-check"),
        ),
        (
            ".kilnr/pipelines/special.json",
            pipeline(&["feature/special"], "special-check"),
        ),
    ]);
    let error =
        ops_runtime::select_pipeline(&ci_job(&repo.sha, "feature/special"), &config(&repo.bare))
            .unwrap_err()
            .to_string();
    assert!(error.contains("matches multiple CI pipelines"));
}

#[test]
fn test_release_uses_only_fixed_release_pipeline() {
    let release = json!({"schema":1,"jobs":{"release-job":{"image":"alpine:3.22","network":"none","run":["true"]}}});
    let repo = make_repo(&[
        (
            ".kilnr/pipelines/main.json",
            pipeline(&["main"], "main-check"),
        ),
        (".kilnr/release.json", release),
    ]);
    let (path, selected) =
        ops_runtime::select_pipeline(&release_job(&repo.sha), &config(&repo.bare))
            .unwrap()
            .unwrap();
    assert_eq!(path, ".kilnr/release.json");
    assert!(
        selected["jobs"].get("release-job").is_some()
            && selected["jobs"].get("main-check").is_none()
    );
}

#[test]
fn test_release_requires_fixed_release_pipeline() {
    let repo = make_repo(&[(
        ".kilnr/pipelines/main.json",
        pipeline(&["main"], "main-check"),
    )]);
    let error = ops_runtime::select_pipeline(&release_job(&repo.sha), &config(&repo.bare))
        .unwrap_err()
        .to_string();
    assert!(error.contains(".kilnr/release.json") || error.contains("does not exist"));
}

#[test]
fn test_legacy_pipeline_is_not_used() {
    let repo = make_repo(&[(".kilnr/pipeline.json", pipeline(&["main"], "legacy"))]);
    assert!(
        ops_runtime::select_pipeline(&ci_job(&repo.sha, "main"), &config(&repo.bare))
            .unwrap()
            .is_none()
    );
}

#[test]
fn test_tools_auto_resolution_uses_package_json_from_exact_sha() {
    let mut pipeline_value = pipeline(&["main"], "tests");
    pipeline_value["jobs"]["tests"]["tools"] = json!(["pnpm"]);
    let repo = make_repo(&[
        (".kilnr/pipelines/main.json", pipeline_value),
        ("package.json", json!({"packageManager":"pnpm@11.15.1"})),
    ]);
    fs::write(
        repo.work.join("package.json"),
        b"{\"packageManager\":\"pnpm@11.16.0\"}\n",
    )
    .unwrap();
    run(&repo.work, &["add", "."]);
    run(&repo.work, &["commit", "-m", "bump pnpm"]);
    let new_sha = run(&repo.work, &["rev-parse", "HEAD"]);
    run(&repo.work, &["push", repo.bare.to_str().unwrap(), "main"]);
    let old = ops_runtime::select_pipeline(&ci_job(&repo.sha, "main"), &config(&repo.bare))
        .unwrap()
        .unwrap()
        .1;
    let new = ops_runtime::select_pipeline(&ci_job(&new_sha, "main"), &config(&repo.bare))
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(old["jobs"]["tests"]["tools"], json!({"pnpm":"11.15.1"}));
    assert_eq!(new["jobs"]["tests"]["tools"], json!({"pnpm":"11.16.0"}));
}
