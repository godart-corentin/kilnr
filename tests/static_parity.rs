use serde_json::Value;
use std::fs;
use std::process::Command;

const ROOT: &str = env!("CARGO_MANIFEST_DIR");

fn read(relative: &str) -> String {
    fs::read_to_string(format!("{ROOT}/{relative}")).unwrap()
}

#[test]
fn test_platform_compatibility() {
    let root = tempfile::tempdir().unwrap();
    for (content, success, message) in [
        ("ID=ubuntu\nVERSION_ID=\"24.04\"\n", true, ""),
        ("ID=ubuntu\nVERSION_ID=\"26.04\"\n", true, ""),
        (
            "ID=ubuntu\nVERSION_ID=\"25.10\"\n",
            true,
            "supported on Ubuntu 24.04 and 26.04 LTS",
        ),
        (
            "ID=debian\nVERSION_ID=\"13\"\n",
            false,
            "this installer targets Ubuntu",
        ),
    ] {
        let path = root.path().join("os-release");
        fs::write(&path, content).unwrap();
        let output = Command::new("bash")
            .arg(format!("{ROOT}/libexec/check-platform"))
            .arg(&path)
            .output()
            .unwrap();
        assert_eq!(output.status.success(), success);
        assert!(String::from_utf8_lossy(&output.stderr).contains(message));
    }
    assert!(read("install.sh").contains("\"$ROOT_DIR/libexec/check-platform\""));
}

#[test]
fn test_frontend_react_routes_dag_and_live_logs_are_wired() {
    let package: Value = serde_json::from_str(&read("web/frontend/package.json")).unwrap();
    for dependency in ["react", "react-dom", "@tanstack/react-router"] {
        assert!(
            package["dependencies"].get(dependency).is_some(),
            "{dependency}"
        );
    }
    for dependency in ["vite", "typescript", "@vitejs/plugin-react"] {
        assert!(
            package["devDependencies"].get(dependency).is_some(),
            "{dependency}"
        );
    }
    let router = read("web/frontend/src/router.tsx");
    for route in [
        "path: '/'",
        "'/build/$buildId'",
        "'/build/$buildId/logs/$job'",
    ] {
        assert!(router.contains(route), "{route}");
    }
    let viewer = read("web/frontend/src/components/LogViewer.tsx");
    assert!(viewer.contains("EventSource") && viewer.contains("offsetRef"));
    assert!(!viewer.contains("window.location.reload") && !viewer.contains("meta http-equiv"));
    let page = read("web/frontend/src/routes/BuildPage.tsx");
    assert!(page.contains("PipelineGraph") && page.contains("EventSource"));
    let graph = read("web/frontend/src/components/PipelineGraph.tsx");
    assert!(graph.contains("resolved_needs"));
    assert!(!graph.contains(".needs"));
    assert!(read("src/web.rs").contains("text/event-stream"));
    let docker = read("web/Dockerfile");
    assert!(docker.contains("FROM node:22-alpine AS frontend"));
    assert!(docker.contains("FROM rust:1.85-alpine AS backend"));
    assert!(docker.contains("COPY --from=frontend /src/dist /opt/kilnr/static"));
    assert!(read("install.sh").contains("/usr/local/share/kilnr/web-src"));
    let update = read("update.sh");
    assert!(update.contains("up -d --build kilnr-web"));
    assert!(!update.contains("docker restart kilnr-web"));
}

#[test]
fn test_install_web_preserves_caddy_networks_and_isolates_runtime() {
    let text = read("install-web.sh");
    for fragment in [
        "default:",
        "kilnr_proxy:",
        "name: ${NETWORK}",
        "# KILNR MANAGED OVERRIDE",
        "MERGED_COMPOSE_JSON=\"${TMP_DIR}/merged-compose.json\"",
        ">\"$MERGED_COMPOSE_JSON\"",
        "config-tool compose-networks",
        "BACKUP_DIR=\"${KILNR_ROOT}/backups/$(date +%Y%m%d-%H%M%S)\"",
        "sed '/^[[:space:]]*$/d'",
        "[[ -n \"$original_network\" ]] || continue",
        "CADDY_ORIGINAL_NETWORKS",
        "Caddy lost its pre-existing Docker network",
        "grep -Fxq \"default\"",
        "grep -Fxq \"kilnr_proxy\"",
        "build:",
        "context: ${WEB_SOURCE}",
        "dockerfile: Dockerfile",
        "image: kilnr-web:local",
        "KILNR_WEB_STATIC: \"/opt/kilnr/static\"",
        "source: /var/lib/kilnr/builds",
        "target: /var/lib/kilnr/builds",
        "read_only: true",
        "up -d --build",
    ] {
        assert!(text.contains(fragment), "{fragment}");
    }
    for forbidden in [
        "/usr/local/libexec/kilnr/web",
        "/var/run/docker.sock",
        "/etc/kilnr/secrets",
        "/srv/git",
        "/var/lib/kilnr/queue",
    ] {
        let compose_start = text.find("cat >\"$TMP_COMPOSE\"").unwrap();
        let compose = &text[compose_start
            ..text[compose_start..]
                .find("\nEOF")
                .map(|n| compose_start + n)
                .unwrap()];
        assert!(!compose.contains(forbidden), "{forbidden}");
    }
}

#[test]
fn test_rust_and_shell_sources_parse_and_runner_has_no_forbidden_options() {
    for file in [
        "install.sh",
        "update.sh",
        "uninstall.sh",
        "install-web.sh",
        "uninstall-web.sh",
        "libexec/check-platform",
        "libexec/doctor",
        "libexec/network-setup",
        "libexec/network-teardown",
        "libexec/git-hooks/post-receive",
        "tests/run.sh",
    ] {
        assert!(
            Command::new("bash")
                .arg("-n")
                .arg(format!("{ROOT}/{file}"))
                .status()
                .unwrap()
                .success(),
            "{file}"
        );
    }
    for entry in fs::read_dir(format!("{ROOT}/examples")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "json") {
            let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert!(value.get("steps").is_none(), "{}", path.display());
        }
    }
    assert!(!std::path::Path::new(&format!("{ROOT}/examples/pipeline.json")).exists());
    let execute = read("src/ops_runtime.rs");
    for forbidden in [
        "/var/run/docker.sock",
        "--privileged",
        "--network=host",
        "src=/etc/kilnr/secrets",
        "dst=/artifacts",
    ] {
        assert!(!execute.contains(forbidden), "{forbidden}");
    }
    let output = Command::new("cargo")
        .args(["check", "--all-targets", "--locked", "--offline"])
        .current_dir(ROOT)
        .status()
        .unwrap();
    assert!(output.success());
}

#[test]
fn test_project_rename_cli_dispatch_and_installation() {
    let ops = read("src/ops.rs");
    assert!(ops.contains("kilnr project rename <old-name> <new-name>"));
    assert!(ops.contains("privileged(\"project-rename\", &[old.clone(), new.clone()], None)"));
    let install = read("install.sh");
    assert!(install.contains("project-rename"));
    assert!(install.contains("/usr/local/libexec/kilnr/$name"));
}

#[test]
fn test_uninstall_purge_is_explicit_guarded_and_comprehensive() {
    let script = read("uninstall.sh");
    for fragment in [
        "Usage: sudo ./uninstall.sh [--purge [--yes]]",
        "[[ \"$confirmation\" != \"PURGE\" ]]",
        "[[ \"$ASSUME_YES\" -eq 1 && \"$PURGE\" -ne 1 ]]",
        "/usr/local/share/kilnr",
        "/opt/kilnr",
        "/var/lib/kilnr",
        "/etc/kilnr",
        "/srv/git",
        "for user in kilnr-web kilnr",
        "for group in kilnr-readers kilnr-submit kilnr-web kilnr",
    ] {
        assert!(script.contains(fragment), "{fragment}");
    }
    assert!(
        script.find("[[ \"$confirmation\" != \"PURGE\" ]]")
            < script.find("rm -rf \\\n        /usr/local/share/kilnr")
    );
}

#[test]
fn test_enqueue_atomic_publish_permissions() {
    let source = read("src/ops.rs");
    assert!(source.contains("atomic::write_json("));
    assert!(source.contains("/var/lib/kilnr/queue/incoming"));
    let atomic = read("src/atomic.rs");
    assert!(atomic.find("tempfile_in(parent)") < atomic.find("temporary.persist(path)"));
    assert!(
        atomic.find("temporary.persist(path)") < atomic.rfind("File::open(parent)?.sync_all()")
    );
    assert!(read("install.sh").contains("setfacl -m u:git:rwx /var/lib/kilnr/queue/incoming"));
    assert!(read("update.sh").contains("\"$ROOT_DIR/install.sh\" --update"));
}

#[test]
fn test_no_python_sources_or_import_shadowing_remain() {
    fn walk(path: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
        for entry in fs::read_dir(path).unwrap().flatten() {
            let path = entry.path();
            if path
                .file_name()
                .is_some_and(|name| name == ".git" || name == "target" || name == "node_modules")
            {
                continue;
            }
            if path.is_dir() {
                walk(&path, found);
            } else if path
                .extension()
                .is_some_and(|ext| ext == "py" || ext == "pyc")
            {
                found.push(path);
            }
        }
    }
    let mut found = vec![];
    walk(std::path::Path::new(ROOT), &mut found);
    assert!(found.is_empty(), "Python files remain: {found:?}");
    assert!(read("install.sh").contains("secrets.py")); // update removes obsolete installed modules
}

#[test]
fn test_libexec_does_not_shadow_stdlib_secrets() {
    assert!(!std::path::Path::new(&format!("{ROOT}/libexec/secrets.py")).exists());
    assert!(!std::path::Path::new(&format!("{ROOT}/libexec/kilnr_secrets.py")).exists());
}
