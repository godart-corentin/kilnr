use kilnr::secrets;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

#[test]
fn test_store_list_load_and_delete_secret() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("demo")).unwrap();
    secrets::store(root.path(), "demo", "APPLE_ID", b"dev@example.com", "text").unwrap();
    let listed = secrets::list(root.path(), "demo").unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0, "APPLE_ID");
    assert_eq!(listed[0].1.schema, 1);
    assert_eq!(listed[0].1.scope, "release");
    assert_eq!(listed[0].1.kind, "text");
    assert!(!serde_json::to_string(&listed[0].1)
        .unwrap()
        .contains("dev@example.com"));
    assert_eq!(
        secrets::read(root.path(), "demo", "APPLE_ID").unwrap(),
        b"dev@example.com"
    );
    assert_eq!(
        fs::metadata(root.path().join("demo/APPLE_ID.value"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o640
    );
    secrets::delete(root.path(), "demo", "APPLE_ID").unwrap();
    assert!(secrets::list(root.path(), "demo").unwrap().is_empty());
}

#[test]
fn test_text_secret_rejects_nul_and_names_are_strict() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("demo")).unwrap();
    for name in ["bad-name", "lower", "KILNR_SHA"] {
        assert!(
            secrets::store(root.path(), "demo", name, b"x", "text").is_err(),
            "{name}"
        );
    }
    let error = secrets::store(root.path(), "demo", "TOKEN", b"a\0b", "text")
        .unwrap_err()
        .to_string();
    assert!(error.contains("NUL"));
}

#[test]
fn test_release_scope_policy_is_enforced() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("demo")).unwrap();
    secrets::store(root.path(), "demo", "TOKEN", b"secret", "text").unwrap();
    assert_eq!(
        secrets::metadata(root.path(), "demo", "TOKEN")
            .unwrap()
            .scope,
        "release"
    );
    // CI rejection is enforced while normalizing its pipeline, before secrets can be staged.
    let pipeline = serde_json::json!({"schema":1,"trigger":{"type":"branch","branches":["*"]},"jobs":{"tests":{"image":"x","run":["true"],"secrets":["TOKEN"]}}});
    let error = kilnr::pipeline::load(
        &serde_json::to_vec(&pipeline).unwrap(),
        "ci",
        3,
        &["none".into()],
        None,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("release-only"));
}

#[test]
fn test_cli_secret_set_uses_hidden_input_and_stdin_not_argv() {
    let source = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ops.rs")).unwrap();
    assert!(source.contains("rpassword::prompt_password"));
    assert!(source.contains("Some(value.as_bytes())"));
    assert!(!source.contains("Command::new(\"sudo\").arg(value)"));
}

#[test]
fn test_cli_usage_lists_secret_commands() {
    let output = Command::new(env!("CARGO_BIN_EXE_kilnr"))
        .arg("--help")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stderr);
    for expected in [
        "kilnr secret set|delete <project> <name>",
        "kilnr secret set-file <project> <name> <path>",
        "kilnr secret list <project>",
    ] {
        assert!(text.contains(expected), "{expected}");
    }
}

#[test]
fn test_install_and_project_lifecycle_wire_secret_storage() {
    let root = env!("CARGO_MANIFEST_DIR");
    let install = fs::read_to_string(format!("{root}/install.sh")).unwrap();
    let uninstall = fs::read_to_string(format!("{root}/uninstall.sh")).unwrap();
    let ops = fs::read_to_string(format!("{root}/src/ops.rs")).unwrap();
    assert!(install.contains("/var/lib/kilnr/secret-staging"));
    assert!(install.contains("/etc/kilnr/projects/*.json"));
    for name in [
        "secret-set",
        "secret-set-file",
        "secret-list",
        "secret-delete",
    ] {
        assert!(install.contains(name));
    }
    assert!(ops.contains("fs::create_dir(&secret_dir)"));
    assert!(ops.contains("fs::remove_dir_all(&secret_dir)"));
    assert!(uninstall.contains("rm -rf /var/lib/kilnr/secret-staging"));
}
