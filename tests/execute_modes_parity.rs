use kilnr::{runtime, secrets};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

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

#[test]
fn test_run_script_keeps_commands_in_one_shell() {
    let commands = ["cd frontend", "export NODE_ENV=test", "pnpm test"].map(str::to_owned);
    let script = runtime::render_run_script(&commands);
    assert!(script.starts_with("#!/bin/sh\nset -eu\n"));
    assert!(script.find(commands[0].as_str()) < script.find(commands[1].as_str()));
    assert!(script.find(commands[1].as_str()) < script.find(commands[2].as_str()));
    for command in commands {
        assert!(script.contains(&format!("$ {command}")));
    }
}

#[test]
fn test_execution_argv_for_run_uses_generated_read_only_script() {
    let root = tempfile::tempdir().unwrap();
    let (mounts, argv) =
        runtime::prepare_execution(root.path(), "tests", &json!({"run":["echo ok"]})).unwrap();
    assert_eq!(argv, ["/bin/sh", "/run/kilnr/job.sh"]);
    assert_eq!(mounts.len(), 1);
    assert!(mounts[0].contains("dst=/run/kilnr/job.sh"));
    assert!(mounts[0].contains("readonly"));
    assert!(fs::read_to_string(root.path().join("commands/tests.sh"))
        .unwrap()
        .contains("echo ok"));
}

#[test]
fn test_execution_argv_for_script_stays_inside_workspace() {
    let (mounts, argv) = runtime::prepare_execution(
        std::path::Path::new("/tmp/build"),
        "package",
        &json!({"script":"scripts/ci/package.sh"}),
    )
    .unwrap();
    assert!(mounts.is_empty());
    assert_eq!(argv, ["/workspace/scripts/ci/package.sh"]);
}

#[test]
fn test_execution_argv_for_command_is_direct_argv() {
    let (mounts, argv) = runtime::prepare_execution(
        std::path::Path::new("/tmp/build"),
        "tool",
        &json!({"command":["node","tool.mjs","--foo"]}),
    )
    .unwrap();
    assert!(mounts.is_empty());
    assert_eq!(argv, ["node", "tool.mjs", "--foo"]);
}

#[test]
fn test_status_updates_target_pipeline_job() {
    let root = tempfile::tempdir().unwrap();
    runtime::write_json(
        &root.path().join("status.json"),
        &json!({"pipeline":{"jobs":{"tests":{"state":"pending"}}}}),
    )
    .unwrap();
    runtime::update_job(root.path(), "tests", &json!({"state":"running"})).unwrap();
    let status: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("status.json")).unwrap()).unwrap();
    assert_eq!(status["pipeline"]["jobs"]["tests"]["state"], "running");
}

fn sample_runtime() -> serde_json::Value {
    json!({"build_id":"build-1","project":"demo","sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","ref":"refs/heads/main","job_type":"ci","branch":"main"})
}

#[test]
fn test_public_environment_includes_context_declared_env_and_input_paths() {
    let roots = BTreeMap::from([("package-linux".into(), PathBuf::from("/tmp/linux"))]);
    let env = runtime::build_public_env(
        &sample_runtime(),
        "tests",
        &json!({"env":{"NODE_ENV":"test"},"secrets":["APPLE_ID"]}),
        &roots,
    )
    .unwrap();
    for (key, value) in [
        ("CI", "true"),
        ("HOME", "/run/kilnr/home"),
        ("XDG_RUNTIME_DIR", "/run/kilnr/tmp"),
        ("TMPDIR", "/run/kilnr/tmp"),
        ("TMP", "/run/kilnr/tmp"),
        ("TEMP", "/run/kilnr/tmp"),
        ("KILNR_BUILD_ID", "build-1"),
        ("KILNR_PROJECT", "demo"),
        ("KILNR_SHA", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        ("KILNR_REF", "refs/heads/main"),
        ("KILNR_JOB_TYPE", "ci"),
        ("KILNR_JOB", "tests"),
        ("KILNR_BRANCH", "main"),
        ("NODE_ENV", "test"),
        (
            "KILNR_INPUT_PACKAGE_LINUX",
            "/run/kilnr/inputs/package-linux",
        ),
    ] {
        assert_eq!(env[key], value, "{key}");
    }
    assert!(!env.contains_key("KILNR_TAG"));
    assert!(!env.contains_key("APPLE_ID"));
}

#[test]
fn test_input_mounts_are_read_only_and_separate() {
    let roots = BTreeMap::from([
        ("linux".into(), PathBuf::from("/build/artifacts/linux")),
        ("windows".into(), PathBuf::from("/build/artifacts/windows")),
    ]);
    let mounts = runtime::build_input_mounts(&roots);
    assert_eq!(mounts.len(), 2);
    assert_eq!(
        mounts[0],
        "type=bind,src=/build/artifacts/linux,dst=/run/kilnr/inputs/linux,readonly"
    );
    assert_eq!(
        mounts[1],
        "type=bind,src=/build/artifacts/windows,dst=/run/kilnr/inputs/windows,readonly"
    );
}

#[test]
fn test_collect_job_artifacts_uses_workspace_not_special_mount() {
    let root = tempfile::tempdir().unwrap();
    let work = root.path().join("work/package");
    fs::create_dir_all(work.join("release")).unwrap();
    fs::write(work.join("release/demo.AppImage"), "binary").unwrap();
    let collected = runtime::collect_job_artifacts(
        root.path(),
        &work,
        "package",
        &json!({"artifacts":["release/*.AppImage"]}),
    )
    .unwrap();
    assert_eq!(collected, ["release/demo.AppImage"]);
    assert!(root
        .path()
        .join("artifacts/package/release/demo.AppImage")
        .is_file());
}

#[test]
fn test_secret_wrapper_contains_names_and_paths_but_no_values() {
    let metadata = BTreeMap::from([
        (
            "APPLE_ID".into(),
            secrets::SecretMetadata {
                schema: 1,
                scope: "release".into(),
                kind: "text".into(),
            },
        ),
        (
            "CSC_LINK".into(),
            secrets::SecretMetadata {
                schema: 1,
                scope: "release".into(),
                kind: "file".into(),
            },
        ),
    ]);
    let wrapper = runtime::render_secret_wrapper(&metadata).unwrap();
    assert!(wrapper.contains("export APPLE_ID=\"$(cat /run/kilnr/secrets/APPLE_ID.value)\""));
    assert!(wrapper.contains("export CSC_LINK=\"/run/kilnr/secrets/CSC_LINK.value\""));
    assert!(wrapper.contains("exec \"$@\""));
    assert!(!wrapper.contains("actual-secret"));
}

#[test]
fn test_prepare_secret_stage_is_outside_builds_and_contains_only_requested_files() {
    let root = tempfile::tempdir().unwrap();
    let secrets_root = root.path().join("etc-secrets");
    let staging_root = root.path().join("staging");
    fs::create_dir_all(secrets_root.join("demo")).unwrap();
    secrets::store(
        &secrets_root,
        "demo",
        "APPLE_ID",
        b"dev@example.com",
        "text",
    )
    .unwrap();
    let runtime_value = json!({"project":"demo","job_type":"release"});
    let prepared = runtime::prepare_secret_stage(
        &secrets_root,
        &staging_root,
        "build-1",
        "release",
        &runtime_value,
        &json!({"secrets":["APPLE_ID"]}),
    )
    .unwrap();
    let stage = prepared.path.as_ref().unwrap();
    assert_eq!(stage.as_path(), staging_root.join("build-1/release"));
    assert_eq!(
        fs::read(stage.join("APPLE_ID.value")).unwrap(),
        b"dev@example.com"
    );
    assert_eq!(prepared.metadata["APPLE_ID"].kind, "text");
    assert!(prepared
        .redaction_values
        .iter()
        .any(|value| value == "dev@example.com"));
    assert!(fs::read_dir(stage).unwrap().all(|entry| entry
        .unwrap()
        .path()
        .extension()
        .is_none_or(|ext| ext != "json")));
}

#[test]
fn test_redaction_masks_known_secret_tokens() {
    let tokens = runtime::redaction_tokens(&["dev@example.com".into(), "line1\nline2".into()]);
    let redacted = runtime::redact_text("login dev@example.com\nline1\nline2\n".into(), &tokens);
    for secret in ["dev@example.com", "line1", "line2"] {
        assert!(!redacted.contains(secret));
    }
    assert!(redacted.contains("***"));
}

#[test]
fn test_tools_wrapper_keeps_corepack_home_on_executable_tmpfs() {
    let tools = serde_json::Map::from_iter([("pnpm".into(), json!("11.15.1"))]);
    let wrapper = runtime::render_tools_wrapper(&tools);
    assert!(wrapper.contains("TOOLS_ROOT=\"/run/kilnr/tmp/tools\""));
    assert!(wrapper.contains("COREPACK_HOME=\"$TOOLS_ROOT/corepack\""));
    assert!(wrapper.contains("PATH=\"/run/kilnr/tools/bin:$PATH\""));
    assert!(!wrapper.contains("TOOLS_ROOT=\"/tmp/"));
    assert!(!wrapper.contains("cat >"));
    assert!(!wrapper.contains("chmod"));
}

#[test]
fn test_prepare_tools_wrapper_mounts_executable_tools_read_only() {
    let root = tempfile::tempdir().unwrap();
    let tools = serde_json::Map::from_iter([("pnpm".into(), json!("11.15.1"))]);
    let (mounts, argv) = runtime::prepare_tools_wrapper(
        root.path(),
        "tests",
        &tools,
        &["/bin/sh".into(), "/run/kilnr/job.sh".into()],
    )
    .unwrap();
    assert_eq!(mounts.len(), 2);
    assert!(mounts.iter().any(
        |mount| mount.contains("dst=/run/kilnr/tools-wrapper.sh") && mount.contains("readonly")
    ));
    assert!(mounts
        .iter()
        .any(|mount| mount.contains("dst=/run/kilnr/tools") && mount.contains("readonly")));
    let pnpm = root.path().join("runtime/tests/tools/bin/pnpm");
    assert!(pnpm.is_file());
    assert_ne!(fs::metadata(&pnpm).unwrap().permissions().mode() & 0o100, 0);
    assert!(fs::read_to_string(pnpm)
        .unwrap()
        .contains("corepack 'pnpm@11.15.1'"));
    assert_eq!(
        argv,
        [
            "/bin/sh",
            "/run/kilnr/tools-wrapper.sh",
            "/bin/sh",
            "/run/kilnr/job.sh"
        ]
    );
}

#[test]
fn test_runner_security_flags_remain_present() {
    let source =
        fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ops_runtime.rs")).unwrap();
    for token in [
        "\"--rm\"",
        "\"--init\"",
        "\"--cpus\"",
        "\"--memory\"",
        "\"--pids-limit\"",
        "\"--cap-drop\"",
        "\"ALL\"",
        "\"no-new-privileges=true\"",
    ] {
        assert!(source.contains(token), "{token}");
    }
    assert!(source.contains("/tmp:rw,nosuid,nodev,noexec,size=512m"));
    assert!(!source.contains("/var/run/docker.sock"));
    assert!(!source.contains("--privileged"));
}
