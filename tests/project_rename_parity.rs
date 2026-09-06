use kilnr::project_rename::{self, Roots};
use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const OLD: &str = "old-app";
const NEW: &str = "new_app";
const SHA: &str = "b7be08123e3518e46578aa35e713f6190a3ce45a";
const OLD_ID: &str = "20260828T010203123456Z-old-app-b7be081-0123abcd";
const NEW_ID: &str = "20260828T010203123456Z-new_app-b7be081-0123abcd";
const OLD_ID_2: &str = "20260828T010203123456Z-old-app-b7be081-deadbeef";
const NEW_ID_2: &str = "20260828T010203123456Z-new_app-b7be081-deadbeef";

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    roots: Roots,
    repo: PathBuf,
    build: PathBuf,
    opaque: Vec<u8>,
}

fn mode(path: &Path, value: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(value)).unwrap();
}
fn write_json(path: &Path, value: &Value, permissions: u32) {
    kilnr::atomic::write_json(path, value, permissions).unwrap();
}

fn job(project: &str, id: &str) -> Value {
    json!({"schema":1,"id":id,"project":project,"received_at":"2026-08-28T01:02:03.123456Z","old_sha":"0".repeat(40),"new_sha":SHA,"sha":SHA,"ref":"refs/heads/feature/old-app-history","type":"ci","event":"push","pin_ref":format!("refs/kilnr/jobs/{id}")})
}
fn runtime(project: &str, id: &str) -> Value {
    json!({"schema":1,"build_id":id,"project":project,"sha":SHA,"ref":"refs/heads/feature/old-app-history","job_type":"ci","pipeline":".kilnr/pipelines/ci.json","max_parallel":1,"runner":{"cpus":"1.0","memory":"1G","pids_limit":128,"timeout_seconds":600},"groups":{},"jobs":{"test":{"resolved_needs":[]}}})
}
fn status(project: &str, id: &str) -> Value {
    json!({"schema":1,"build_id":id,"job_id":id,"project":project,"sha":SHA,"ref":"refs/heads/feature/old-app-history","type":"ci","event":"push","received_at":"2026-08-28T01:02:03.123456Z","state":"success","prepare":{"state":"success","log":"logs/prepare.log"},"pipeline":{"jobs":{}}})
}

fn make_build(roots: &Roots, project: &str, id: &str, complete: bool) -> PathBuf {
    let path = roots.state.join("builds").join(id);
    fs::create_dir(&path).unwrap();
    mode(&path, 0o750);
    write_json(&path.join("job.json"), &job(project, id), 0o640);
    if complete {
        write_json(&path.join("runtime.json"), &runtime(project, id), 0o640);
        write_json(&path.join("status.json"), &status(project, id), 0o640);
        fs::write(
            path.join("pipeline.mk"),
            format!(".PHONY: all\nall:\n\t@/usr/local/libexec/kilnr/execute {id} test\n"),
        )
        .unwrap();
        mode(&path.join("pipeline.mk"), 0o640);
    }
    for child in ["src", "work", "logs", "artifacts", "commands"] {
        fs::create_dir(path.join(child)).unwrap();
        mode(&path.join(child), 0o750);
    }
    path
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let state = root.join("state");
        let roots = Roots {
            git: root.join("git"),
            config: root.join("projects"),
            secrets: root.join("secrets"),
            locks: state.join("locks/projects"),
            state,
            managed_hook: Some(root.join("managed-post-receive")),
        };
        for path in [
            &roots.git,
            &roots.config,
            &roots.secrets,
            &roots.state.join("queue/incoming"),
            &roots.state.join("queue/running"),
            &roots.state.join("builds"),
            &roots.state.join("cache"),
            &roots.locks,
        ] {
            fs::create_dir_all(path).unwrap();
        }
        fs::write(roots.managed_hook.as_ref().unwrap(), "#!/bin/sh\nexit 0\n").unwrap();
        mode(roots.managed_hook.as_ref().unwrap(), 0o755);
        let repo = roots.git.join(format!("{OLD}.git"));
        let result = Command::new("git")
            .args(["init", "--bare", "--initial-branch=main"])
            .arg(&repo)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(result.success());
        for args in [
            ["config", "transfer.hideRefs", "refs/kilnr/"],
            ["config", "gc.packRefs", "false"],
        ] {
            assert!(Command::new("git")
                .arg(format!("--git-dir={}", repo.display()))
                .args(args)
                .status()
                .unwrap()
                .success());
        }
        let mut hash = Command::new("git")
            .arg(format!("--git-dir={}", repo.display()))
            .args(["hash-object", "-w", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        {
            use std::io::Write;
            hash.stdin
                .take()
                .unwrap()
                .write_all(b"fixture object")
                .unwrap();
        }
        assert_eq!(
            String::from_utf8(hash.wait_with_output().unwrap().stdout)
                .unwrap()
                .trim(),
            SHA
        );
        mode(&repo, 0o750);
        fs::create_dir_all(repo.join("refs/kilnr/jobs")).unwrap();
        mode(&repo.join("refs/kilnr"), 0o770);
        mode(&repo.join("refs/kilnr/jobs"), 0o770);
        symlink(
            roots.managed_hook.as_ref().unwrap(),
            repo.join("hooks/post-receive"),
        )
        .unwrap();
        let webhook = roots.secrets.join(format!("{OLD}.discord-webhook"));
        fs::write(&webhook, "https://discord.invalid/token\n").unwrap();
        mode(&webhook, 0o640);
        let secrets = roots.secrets.join(OLD);
        fs::create_dir(&secrets).unwrap();
        mode(&secrets, 0o750);
        fs::write(secrets.join("TOKEN.value"), b"old-app\0\xffprivate").unwrap();
        mode(&secrets.join("TOKEN.value"), 0o640);
        write_json(
            &secrets.join("TOKEN.json"),
            &json!({"schema":1,"scope":"release","kind":"file"}),
            0o640,
        );
        let cache = roots.state.join("cache").join(OLD);
        fs::create_dir(&cache).unwrap();
        mode(&cache, 0o750);
        write_json(
            &roots.config.join(format!("{OLD}.json")),
            &json!({"schema":1,"project":OLD,"repository":repo,"release":{"tag_pattern":"^v[0-9]+$"},"runner":{"max_parallel":2,"cpus":"1.0","memory":"1G","pids_limit":128,"timeout_seconds":600,"allowed_networks":["none"]},"discord":{"webhook_file":webhook}}),
            0o644,
        );
        let build = make_build(&roots, OLD, OLD_ID, true);
        let opaque = b"\0old-app\xff".repeat(8);
        fs::write(build.join("artifacts/old-app.bin"), &opaque).unwrap();
        fs::write(cache.join("opaque.bin"), &opaque).unwrap();
        fs::write(repo.join("objects/old-app-object"), &opaque).unwrap();
        fs::write(
            repo.join("refs/kilnr/jobs").join(OLD_ID),
            format!("{SHA}\n"),
        )
        .unwrap();
        mode(&repo.join("refs/kilnr/jobs").join(OLD_ID), 0o640);
        Self {
            _temp: temp,
            root,
            roots,
            repo,
            build,
            opaque,
        }
    }
    fn inventory(&self) -> anyhow::Result<project_rename::RenameInventory> {
        project_rename::inventory_rename(&self.roots, OLD, NEW)
    }
}

#[test]
fn test_inventory_maps_hyphenated_build_identity_without_mutating_state() {
    let f = Fixture::new();
    let before = walk(&f.root);
    let i = f.inventory().unwrap();
    assert_eq!(i.build_ids.get(OLD_ID).map(String::as_str), Some(NEW_ID));
    assert_eq!(
        i.pin_refs
            .get(&format!("refs/kilnr/jobs/{OLD_ID}"))
            .map(String::as_str),
        Some(format!("refs/kilnr/jobs/{NEW_ID}").as_str())
    );
    assert_eq!(i.repository.source, f.repo);
    assert_eq!(walk(&f.root), before);
    assert_eq!(
        fs::read(f.build.join("artifacts/old-app.bin")).unwrap(),
        f.opaque
    );
}
fn walk(root: &Path) -> Vec<PathBuf> {
    fn add(root: &Path, path: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(path).unwrap() {
            let p = entry.unwrap().path();
            out.push(p.strip_prefix(root).unwrap().into());
            if fs::symlink_metadata(&p).unwrap().is_dir() {
                add(root, &p, out)
            }
        }
    }
    let mut out = vec![];
    add(root, root, &mut out);
    out.sort();
    out
}

#[test]
fn test_inventory_ignores_unrelated_builds_and_active_jobs() {
    let f = Fixture::new();
    make_build(&f.roots, "another", "not-a-kilnr-build-id", false);
    let id = "20260828T010203123456Z-another-abcdef0-0123abcd";
    write_json(
        &f.roots
            .state
            .join("queue/incoming")
            .join(format!("{id}.json")),
        &job("another", id),
        0o644,
    );
    assert_eq!(f.inventory().unwrap().build_ids.len(), 1);
}
#[test]
fn test_inventory_rejects_source_jobs_in_both_active_queues() {
    for queue in ["incoming", "running"] {
        let f = Fixture::new();
        write_json(
            &f.roots
                .state
                .join("queue")
                .join(queue)
                .join(format!("{OLD_ID}.json")),
            &job(OLD, OLD_ID),
            0o644,
        );
        assert!(f
            .inventory()
            .unwrap_err()
            .to_string()
            .contains(&format!("active {queue} job")));
    }
}
#[test]
fn test_inventory_rejects_invalid_names_before_path_construction() {
    let f = Fixture::new();
    for (old, new) in [("../old", NEW), (OLD, "New"), (OLD, OLD)] {
        assert!(project_rename::inventory_rename(&f.roots, old, new).is_err());
    }
}
#[test]
fn test_inventory_rejects_a_destination_pin_ref_collision() {
    let f = Fixture::new();
    fs::write(
        f.repo.join("refs/kilnr/jobs").join(NEW_ID),
        format!("{SHA}\n"),
    )
    .unwrap();
    assert!(f
        .inventory()
        .unwrap_err()
        .to_string()
        .contains("destination pin ref exists"));
}
#[test]
fn test_inventory_maps_a_source_pin_ref_stored_in_packed_refs() {
    let f = Fixture::new();
    fs::remove_file(f.repo.join("refs/kilnr/jobs").join(OLD_ID)).unwrap();
    fs::write(
        f.repo.join("packed-refs"),
        format!("{SHA} refs/kilnr/jobs/{OLD_ID}\n"),
    )
    .unwrap();
    assert_eq!(f.inventory().unwrap().pin_refs.len(), 1);
}
#[test]
fn test_inventory_rejects_a_pin_ref_that_points_to_the_wrong_sha() {
    let f = Fixture::new();
    fs::write(
        f.repo.join("refs/kilnr/jobs").join(OLD_ID),
        format!("{}\n", "d".repeat(40)),
    )
    .unwrap();
    assert!(f
        .inventory()
        .unwrap_err()
        .to_string()
        .contains("pin ref target mismatch"));
}
#[test]
fn test_inventory_rejects_a_symlinked_managed_root() {
    let f = Fixture::new();
    let alias = f.root.join("projects-link");
    symlink(&f.roots.config, &alias).unwrap();
    let roots = Roots {
        config: alias,
        ..f.roots.clone()
    };
    assert!(project_rename::inventory_rename(&roots, OLD, NEW)
        .unwrap_err()
        .to_string()
        .contains("symlink"));
}
#[test]
fn test_inventory_rejects_malformed_managed_json() {
    let f = Fixture::new();
    fs::write(f.build.join("runtime.json"), "{broken").unwrap();
    assert!(f
        .inventory()
        .unwrap_err()
        .to_string()
        .contains("invalid JSON"));
}
#[test]
fn test_inventory_rejects_unexpected_fifo_secret_entries() {
    let f = Fixture::new();
    let path = f.roots.secrets.join(OLD).join("FIFO.value");
    let c = std::ffi::CString::new(path.to_string_lossy().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o640) }, 0);
    assert!(f
        .inventory()
        .unwrap_err()
        .to_string()
        .contains("regular file"));
}

#[test]
fn test_inventory_rejects_every_fixed_destination_collision() {
    for selected in 0..6 {
        let f = Fixture::new();
        let path = match selected {
            0 => f.roots.git.join(format!("{NEW}.git")),
            1 => f.roots.config.join(format!("{NEW}.json")),
            2 => f.roots.secrets.join(format!("{NEW}.discord-webhook")),
            3 => f.roots.secrets.join(NEW),
            4 => f.roots.state.join("cache").join(NEW),
            _ => f.roots.state.join("builds").join(NEW_ID),
        };
        if matches!(selected, 1 | 2) {
            fs::write(&path, "occupied").unwrap();
        } else {
            fs::create_dir(&path).unwrap();
        }
        assert!(f
            .inventory()
            .unwrap_err()
            .to_string()
            .contains(&path.to_string_lossy().to_string()));
    }
}

#[test]
fn test_inventory_rejects_dangling_destination_symlinks() {
    for build in [false, true] {
        let f = Fixture::new();
        let path = if build {
            f.roots.state.join("builds").join(NEW_ID)
        } else {
            f.roots.config.join(format!("{NEW}.json"))
        };
        symlink(path.parent().unwrap().join("missing"), &path).unwrap();
        assert!(f
            .inventory()
            .unwrap_err()
            .to_string()
            .contains(&path.to_string_lossy().to_string()));
    }
}

#[test]
fn test_inventory_maps_packed_refs_when_the_loose_namespace_is_absent() {
    let f = Fixture::new();
    let jobs = f.repo.join("refs/kilnr/jobs");
    fs::remove_file(jobs.join(OLD_ID)).unwrap();
    fs::remove_dir(jobs).unwrap();
    fs::write(
        f.repo.join("packed-refs"),
        format!("{SHA} refs/kilnr/jobs/{OLD_ID}\n"),
    )
    .unwrap();
    assert_eq!(f.inventory().unwrap().pin_refs.len(), 1);
}

#[test]
fn test_inventory_rejects_a_packed_destination_when_loose_namespace_is_absent() {
    let f = Fixture::new();
    let jobs = f.repo.join("refs/kilnr/jobs");
    fs::remove_file(jobs.join(OLD_ID)).unwrap();
    fs::remove_dir(jobs).unwrap();
    fs::write(
        f.repo.join("packed-refs"),
        format!("{SHA} refs/kilnr/jobs/{OLD_ID}\n{SHA} refs/kilnr/jobs/{NEW_ID}\n"),
    )
    .unwrap();
    assert!(f
        .inventory()
        .unwrap_err()
        .to_string()
        .contains("destination pin ref exists"));
}

#[test]
fn test_inventory_rejects_stale_and_ambiguous_managed_refs() {
    for ambiguous in [false, true] {
        let f = Fixture::new();
        if ambiguous {
            fs::write(
                f.repo.join("packed-refs"),
                format!("{} refs/kilnr/jobs/{OLD_ID}\n", "d".repeat(40)),
            )
            .unwrap();
            assert!(f
                .inventory()
                .unwrap_err()
                .to_string()
                .contains("ambiguous managed pin ref"));
        } else {
            fs::write(
                f.repo
                    .join("refs/kilnr/jobs/20260828T010203123456Z-old-app-b7be081-deadbeef"),
                format!("{SHA}\n"),
            )
            .unwrap();
            assert!(f
                .inventory()
                .unwrap_err()
                .to_string()
                .contains("unmatched managed pin ref"));
        }
    }
}

#[test]
fn test_inventory_type_checks_every_loose_managed_ref_entry() {
    for kind in ["symlink", "directory", "fifo", "socket"] {
        let f = Fixture::new();
        let path = f.repo.join("refs/kilnr/jobs/unexpected");
        let mut listener = None;
        match kind {
            "symlink" => symlink(f.repo.join("HEAD"), &path).unwrap(),
            "directory" => fs::create_dir(&path).unwrap(),
            "fifo" => {
                let c = std::ffi::CString::new(path.to_string_lossy().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o640) }, 0)
            }
            _ => {
                let alias = f.root.join("j");
                symlink(path.parent().unwrap(), &alias).unwrap();
                listener =
                    Some(std::os::unix::net::UnixListener::bind(alias.join("unexpected")).unwrap());
            }
        };
        assert!(f
            .inventory()
            .unwrap_err()
            .to_string()
            .contains("managed pin ref"));
        drop(listener);
    }
}

#[test]
fn test_inventory_rejects_unexpected_active_queue_entry_types() {
    let f = Fixture::new();
    let path = f.roots.state.join("queue/incoming/unexpected.json");
    let c = std::ffi::CString::new(path.to_string_lossy().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o640) }, 0);
    assert!(f
        .inventory()
        .unwrap_err()
        .to_string()
        .contains("queue entry"));
}

#[test]
fn test_inventory_rejects_a_symlinked_queue_root() {
    let f = Fixture::new();
    let queue = f.roots.state.join("queue");
    let real = f.roots.state.join("queue-real");
    fs::rename(&queue, &real).unwrap();
    symlink(&real, &queue).unwrap();
    assert!(f.inventory().unwrap_err().to_string().contains("symlink"));
}

#[test]
fn test_inventory_rejects_a_symlinked_refs_kilnr_component() {
    let f = Fixture::new();
    fs::remove_dir_all(f.repo.join("refs/kilnr")).unwrap();
    let outside = f.root.join("outside-kilnr/jobs");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join(OLD_ID), format!("{SHA}\n")).unwrap();
    symlink(outside.parent().unwrap(), f.repo.join("refs/kilnr")).unwrap();
    assert!(f.inventory().unwrap_err().to_string().contains("symlink"));
}

#[test]
fn test_inventory_rejects_symlinks_in_managed_locations() {
    for selected in 0..8 {
        let f = Fixture::new();
        let path = match selected {
            0 => f.roots.config.join(format!("{OLD}.json")),
            1 => f.roots.secrets.join(format!("{OLD}.discord-webhook")),
            2 => f.roots.secrets.join(OLD),
            3 => f.roots.state.join("cache").join(OLD),
            4 => f.build.join("job.json"),
            5 => f.build.join("runtime.json"),
            6 => f.build.join("status.json"),
            _ => f.build.join("pipeline.mk"),
        };
        let target = f.root.join("target");
        if fs::symlink_metadata(&path).unwrap().is_dir() {
            fs::remove_dir_all(&path).unwrap();
            fs::create_dir(&target).unwrap();
        } else {
            fs::remove_file(&path).unwrap();
            fs::write(&target, "target").unwrap();
        }
        symlink(&target, &path).unwrap();
        assert!(f.inventory().unwrap_err().to_string().contains("symlink"));
    }
}

#[test]
fn test_inventory_rejects_inconsistent_repository_config_and_hook() {
    for selected in 0..4 {
        let f = Fixture::new();
        if selected < 3 {
            let args = match selected {
                0 => ["config", "core.bare", "false"],
                1 => ["config", "transfer.hideRefs", "refs/other/"],
                _ => ["config", "gc.packRefs", "true"],
            };
            assert!(Command::new("git")
                .arg(format!("--git-dir={}", f.repo.display()))
                .args(args)
                .status()
                .unwrap()
                .success());
        } else {
            fs::remove_file(f.repo.join("hooks/post-receive")).unwrap();
            symlink(f.repo.join("HEAD"), f.repo.join("hooks/post-receive")).unwrap();
        }
        assert!(f
            .inventory()
            .unwrap_err()
            .to_string()
            .contains("repository"));
    }
}

#[test]
fn test_inventory_applies_strict_project_config_validation() {
    for selected in 0..4 {
        let f = Fixture::new();
        let path = f.roots.config.join(format!("{OLD}.json"));
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        match selected {
            0 => value["runner"]["max_parallel"] = json!(0),
            1 => value["runner"]["memory"] = json!("unbounded"),
            2 => value["runner"]["allowed_networks"] = json!([]),
            _ => value["release"]["tag_pattern"] = json!("("),
        };
        write_json(&path, &value, 0o644);
        assert!(f
            .inventory()
            .unwrap_err()
            .to_string()
            .contains("project config"));
    }
}

#[test]
fn test_inventory_rejects_inconsistent_managed_build_schemas() {
    for (file, key) in [
        ("job.json", "type"),
        ("runtime.json", "job_type"),
        ("status.json", "state"),
    ] {
        let f = Fixture::new();
        let path = f.build.join(file);
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value.as_object_mut().unwrap().remove(key);
        write_json(&path, &value, 0o640);
        assert!(f.inventory().unwrap_err().to_string().contains("build"));
    }
}

#[test]
fn test_inventory_validates_managed_build_top_level_entry_types() {
    for selected in 0..5 {
        let f = Fixture::new();
        match selected {
            0 => fs::remove_dir(f.build.join("commands")).unwrap(),
            1 => {
                fs::remove_dir(f.build.join("commands")).unwrap();
                symlink(f.root.join("outside"), f.build.join("commands")).unwrap()
            }
            2 => symlink(f.root.join("outside"), f.build.join("runtime")).unwrap(),
            3 => symlink(f.root.join("outside"), f.build.join("status.lock")).unwrap(),
            _ => fs::write(f.build.join("unexpected-managed-entry"), "x").unwrap(),
        };
        assert!(f.inventory().unwrap_err().to_string().contains("build"));
    }
}

#[test]
fn test_inventory_keeps_commands_and_runtime_payloads_opaque() {
    let f = Fixture::new();
    fs::create_dir(f.build.join("runtime")).unwrap();
    mode(&f.build.join("runtime"), 0o750);
    fs::write(f.build.join("status.lock"), "").unwrap();
    mode(&f.build.join("status.lock"), 0o640);
    symlink("old-app-outside", f.build.join("commands/opaque-link")).unwrap();
    let fifo = f.build.join("runtime/opaque-fifo");
    let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    assert_eq!(f.inventory().unwrap().build_ids.len(), 1);
}

#[test]
fn test_inventory_rejects_invalid_secret_names() {
    let f = Fixture::new();
    let dir = f.roots.secrets.join(OLD);
    fs::write(dir.join("bad-name.value"), "private").unwrap();
    mode(&dir.join("bad-name.value"), 0o640);
    write_json(
        &dir.join("bad-name.json"),
        &json!({"schema":1,"scope":"release","kind":"file"}),
        0o640,
    );
    assert!(f
        .inventory()
        .unwrap_err()
        .to_string()
        .contains("invalid secret name"));
}

#[test]
fn test_inventory_rejects_unsafe_managed_modes() {
    for selected in 0..12 {
        let f = Fixture::new();
        let (path, bad) = match selected {
            0 => (f.repo.clone(), 0o770),
            1 => (f.roots.config.join(format!("{OLD}.json")), 0o664),
            2 => (
                f.roots.secrets.join(format!("{OLD}.discord-webhook")),
                0o660,
            ),
            3 => (f.roots.secrets.join(OLD), 0o770),
            4 => (f.roots.secrets.join(OLD).join("TOKEN.json"), 0o660),
            5 => (f.repo.join("refs/kilnr"), 0o750),
            6 => (f.repo.join("refs/kilnr/jobs"), 0o750),
            7 => (f.roots.state.join("cache").join(OLD), 0o700),
            8 => (f.build.clone(), 0o770),
            9 => (f.build.join("job.json"), 0o660),
            10 => (f.build.join("src"), 0o770),
            _ => (f.repo.join("refs/kilnr/jobs").join(OLD_ID), 0o666),
        };
        mode(&path, bad);
        assert!(
            f.inventory().is_err(),
            "accepted bad mode for {}",
            path.display()
        );
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Snap(String, u32, Vec<u8>);
fn snapshot(root: &Path) -> Vec<Snap> {
    fn visit(root: &Path, path: &Path, out: &mut Vec<Snap>) {
        for entry in fs::read_dir(path).unwrap() {
            let p = entry.unwrap().path();
            let m = fs::symlink_metadata(&p).unwrap();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            let mode = m.permissions().mode() & 0o7777;
            if m.file_type().is_symlink() {
                out.push(Snap(
                    rel,
                    mode,
                    fs::read_link(&p)
                        .unwrap()
                        .to_string_lossy()
                        .as_bytes()
                        .to_vec(),
                ));
            } else if m.is_dir() {
                out.push(Snap(rel, mode, vec![]));
                visit(root, &p, out)
            } else {
                out.push(Snap(rel, mode, fs::read(&p).unwrap()));
            }
        }
    }
    let mut out = vec![];
    visit(root, root, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}
fn assert_snapshot(root: &Path, before: &[Snap], phase: &str) {
    let after = snapshot(root);
    if after != before {
        let index = after
            .iter()
            .zip(before)
            .position(|(a, b)| a != b)
            .unwrap_or(after.len().min(before.len()));
        panic!(
            "snapshot mismatch after {phase} at index {index}: before={:?} after={:?}",
            before.get(index).map(|v| (&v.0, v.1, v.2.len())),
            after.get(index).map(|v| (&v.0, v.1, v.2.len()))
        );
    }
}

#[test]
fn test_prepare_rewrites_only_allowlisted_metadata_and_preserves_attributes() {
    let f = Fixture::new();
    let before = fs::read(f.build.join("job.json")).unwrap();
    let prepared = project_rename::prepare_rename(f.inventory().unwrap()).unwrap();
    assert_eq!(fs::read(f.build.join("job.json")).unwrap(), before);
    for file in &prepared.files {
        assert!(file.temporary.exists());
        assert_eq!(
            fs::symlink_metadata(&file.temporary)
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            file.write.facts.mode
        );
        if file.write.is_json {
            let value: Value = serde_json::from_slice(&fs::read(&file.temporary).unwrap()).unwrap();
            assert!(!value.to_string().contains(OLD_ID));
        }
    }
    assert_eq!(
        fs::read(f.build.join("artifacts/old-app.bin")).unwrap(),
        f.opaque
    );
    prepared.cleanup();
}

#[test]
fn test_prepared_cleanup_removes_files_before_and_after_a_build_parent_move() {
    for moved in [false, true] {
        let f = Fixture::new();
        let prepared = project_rename::prepare_rename(f.inventory().unwrap()).unwrap();
        if moved {
            for item in prepared.inventory.path_moves() {
                fs::rename(&item.source, &item.destination).unwrap();
            }
        }
        prepared.cleanup();
        assert!(prepared
            .files
            .iter()
            .all(|file| !file.temporary.exists() && !file.temporary_after_moves().exists()));
    }
}

#[test]
fn test_commit_renames_all_managed_state_and_preserves_opaque_payloads() {
    let f = Fixture::new();
    let prepared = project_rename::prepare_rename(f.inventory().unwrap()).unwrap();
    project_rename::commit_rename(&prepared).unwrap();
    let new_build = f.roots.state.join("builds").join(NEW_ID);
    assert!(f.roots.git.join(format!("{NEW}.git")).is_dir());
    assert!(f.roots.config.join(format!("{NEW}.json")).is_file());
    assert!(f.roots.secrets.join(NEW).is_dir());
    assert!(f.roots.state.join("cache").join(NEW).is_dir());
    assert!(new_build.is_dir());
    assert_eq!(
        fs::read(new_build.join("artifacts/old-app.bin")).unwrap(),
        f.opaque
    );
    let job: Value =
        serde_json::from_slice(&fs::read(new_build.join("job.json")).unwrap()).unwrap();
    assert_eq!(job["id"], NEW_ID);
    assert_eq!(job["project"], NEW);
    assert!(!f.repo.exists());
}

#[test]
fn test_commit_rolls_back_exactly_after_every_stable_phase() {
    for phase in [
        "repository-move",
        "config-move",
        "webhook-move",
        "secret-directory-move",
        "cache-move",
        "build-move",
        "metadata-backup",
        "metadata-install",
        "pin-refs",
        "verify",
    ] {
        let f = Fixture::new();
        let before = snapshot(&f.root);
        let prepared = project_rename::prepare_rename(f.inventory().unwrap()).unwrap();
        let result = project_rename::commit_rename_with(&prepared, |current| {
            if current == phase {
                anyhow::bail!("injected {phase}")
            } else {
                Ok(())
            }
        });
        assert!(result.is_err(), "phase {phase}");
        assert_snapshot(&f.root, &before, phase);
    }
}

#[test]
fn test_normal_verification_failure_rolls_back_the_entire_transaction() {
    let f = Fixture::new();
    let before = snapshot(&f.root);
    let prepared = project_rename::prepare_rename(f.inventory().unwrap()).unwrap();
    let result = project_rename::commit_rename_with(&prepared, |phase| {
        if phase == "verify" {
            anyhow::bail!("verification failed")
        } else {
            Ok(())
        }
    });
    assert!(result.is_err());
    assert_snapshot(&f.root, &before, "verify");
}

fn terminal_failure(f: &Fixture, selected: bool) -> PathBuf {
    let path = make_build(&f.roots, OLD, OLD_ID_2, false);
    let mut value = status(OLD, OLD_ID_2);
    value["pipeline"] = Value::Null;
    value["pipeline_path"] = if selected {
        json!(".kilnr/pipelines/ci.json")
    } else {
        Value::Null
    };
    value["state"] = json!("failed");
    value["prepare"] =
        json!({"state":"failed","error":"selection failed","log":"logs/prepare.log"});
    write_json(&path.join("status.json"), &value, 0o640);
    fs::write(
        path.join("logs/prepare.log"),
        "KILNR ERROR: old-app remains historical\n",
    )
    .unwrap();
    fs::write(
        f.repo.join("refs/kilnr/jobs").join(OLD_ID_2),
        format!("{SHA}\n"),
    )
    .unwrap();
    mode(&f.repo.join("refs/kilnr/jobs").join(OLD_ID_2), 0o640);
    path
}

#[test]
fn test_terminal_selection_and_pre_runtime_failures_rename_successfully() {
    for selected in [false, true] {
        let f = Fixture::new();
        terminal_failure(&f, selected);
        let prepared = project_rename::prepare_rename(f.inventory().unwrap()).unwrap();
        project_rename::commit_rename(&prepared).unwrap();
        let renamed = f.roots.state.join("builds").join(NEW_ID_2);
        assert!(renamed.is_dir());
        assert!(!renamed.join("runtime.json").exists());
        assert!(!renamed.join("pipeline.mk").exists());
        let status: Value =
            serde_json::from_slice(&fs::read(renamed.join("status.json")).unwrap()).unwrap();
        assert_eq!(status["build_id"], NEW_ID_2);
        assert_eq!(
            status["pipeline_path"],
            if selected {
                json!(".kilnr/pipelines/ci.json")
            } else {
                Value::Null
            }
        );
        assert_eq!(
            fs::read_to_string(renamed.join("logs/prepare.log")).unwrap(),
            "KILNR ERROR: old-app remains historical\n"
        );
    }
}

#[test]
fn test_terminal_selection_and_pre_runtime_failures_roll_back_exactly() {
    for selected in [false, true] {
        let f = Fixture::new();
        terminal_failure(&f, selected);
        let before = snapshot(&f.root);
        let prepared = project_rename::prepare_rename(f.inventory().unwrap()).unwrap();
        assert!(
            project_rename::commit_rename_with(&prepared, |phase| if phase == "verify" {
                anyhow::bail!("failed-build rollback")
            } else {
                Ok(())
            })
            .is_err()
        );
        assert_snapshot(&f.root, &before, "failed build");
    }
}

#[test]
fn test_inventory_captures_acl_facts_for_managed_sources() {
    let f = Fixture::new();
    let inventory = f.inventory().unwrap();
    for path in [
        &inventory.repository.source,
        &inventory.config_file.source,
        &inventory.webhook.source,
        &inventory.secret_directory.source,
        &f.build,
        &f.build.join("job.json"),
        &f.repo.join("refs/kilnr/jobs").join(OLD_ID),
    ] {
        assert!(
            inventory.source_facts.contains_key(path),
            "missing facts for {}",
            path.display()
        );
    }
}

#[test]
fn test_inventory_rejects_build_ids_not_anchored_to_structured_metadata() {
    for mutation in 0..4 {
        let f = Fixture::new();
        let old = f.build.clone();
        let new = match mutation {
            0 => f
                .roots
                .state
                .join("builds/20260828T010203123456Z-old-app-abcdef0-0123abcd"),
            1 => f
                .roots
                .state
                .join("builds/20260828T010204123456Z-old-app-b7be081-0123abcd"),
            2 => f
                .roots
                .state
                .join("builds/20260828T010203123456Z-old-app-b7be081-bad"),
            _ => f.roots.state.join("builds/not-structured"),
        };
        fs::rename(&old, &new).unwrap();
        let mut value: Value =
            serde_json::from_slice(&fs::read(new.join("job.json")).unwrap()).unwrap();
        value["id"] = json!(new.file_name().unwrap().to_string_lossy());
        value["pin_ref"] = json!(format!(
            "refs/kilnr/jobs/{}",
            new.file_name().unwrap().to_string_lossy()
        ));
        write_json(&new.join("job.json"), &value, 0o640);
        assert!(f.inventory().is_err());
    }
}

#[test]
fn test_prepare_refuses_permission_drift_after_inventory() {
    let f = Fixture::new();
    let inventory = f.inventory().unwrap();
    mode(&f.build.join("job.json"), 0o600);
    assert!(project_rename::prepare_rename(inventory)
        .unwrap_err()
        .to_string()
        .contains("permissions changed"));
}

#[test]
fn test_prepare_rejects_pipeline_entries_with_an_unexpected_build_id() {
    let f = Fixture::new();
    fs::write(
        f.build.join("pipeline.mk"),
        "\t@/usr/local/libexec/kilnr/execute wrong test\n",
    )
    .unwrap();
    mode(&f.build.join("pipeline.mk"), 0o640);
    assert!(f.inventory().unwrap_err().to_string().contains("pipeline"));
}

fn pack_only(f: &Fixture, remove_namespace: bool) {
    fs::remove_file(f.repo.join("refs/kilnr/jobs").join(OLD_ID)).unwrap();
    if remove_namespace {
        fs::remove_dir(f.repo.join("refs/kilnr/jobs")).unwrap();
        fs::remove_dir(f.repo.join("refs/kilnr")).unwrap();
    }
    fs::write(
        f.repo.join("packed-refs"),
        format!("# pack-refs with: sorted\n{SHA} refs/kilnr/jobs/{OLD_ID}\n"),
    )
    .unwrap();
}
fn ref_value(repo: &Path, reference: &str) -> Option<String> {
    let output = Command::new("git")
        .arg(format!("--git-dir={}", repo.display()))
        .args(["show-ref", "--verify", "--hash", reference])
        .output()
        .unwrap();
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).unwrap().trim().to_owned())
}

#[test]
fn test_packed_only_commit_materializes_the_fixture_loose_ref_policy() {
    let f = Fixture::new();
    pack_only(&f, false);
    let prepared = project_rename::prepare_rename(f.inventory().unwrap()).unwrap();
    project_rename::commit_rename(&prepared).unwrap();
    let repo = f.roots.git.join(format!("{NEW}.git"));
    assert_eq!(
        ref_value(&repo, &format!("refs/kilnr/jobs/{NEW_ID}")).as_deref(),
        Some(SHA)
    );
    assert_eq!(
        fs::symlink_metadata(repo.join("refs/kilnr/jobs").join(NEW_ID))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o640
    );
}

#[test]
fn test_commit_rolls_back_a_packed_source_ref_with_update_ref() {
    let f = Fixture::new();
    pack_only(&f, false);
    let before = snapshot(&f.root);
    let prepared = project_rename::prepare_rename(f.inventory().unwrap()).unwrap();
    assert!(
        project_rename::commit_rename_with(&prepared, |phase| if phase == "verify" {
            anyhow::bail!("rollback packed")
        } else {
            Ok(())
        })
        .is_err()
    );
    assert_snapshot(&f.root, &before, "packed rollback");
}

#[test]
fn test_mixed_loose_and_packed_ref_state_rolls_back_exactly() {
    let f = Fixture::new();
    fs::write(
        f.repo.join("packed-refs"),
        format!("{SHA} refs/kilnr/jobs/{OLD_ID}\n"),
    )
    .unwrap();
    let before = snapshot(&f.root);
    let prepared = project_rename::prepare_rename(f.inventory().unwrap()).unwrap();
    assert!(
        project_rename::commit_rename_with(&prepared, |phase| if phase == "verify" {
            anyhow::bail!("mixed rollback")
        } else {
            Ok(())
        })
        .is_err()
    );
    assert_snapshot(&f.root, &before, "mixed rollback");
}

#[test]
fn test_packed_only_ref_with_absent_loose_namespace_rolls_back_exactly() {
    let f = Fixture::new();
    pack_only(&f, true);
    let before = snapshot(&f.root);
    let prepared = project_rename::prepare_rename(f.inventory().unwrap()).unwrap();
    assert!(
        project_rename::commit_rename_with(&prepared, |phase| if phase == "verify" {
            anyhow::bail!("absent namespace rollback")
        } else {
            Ok(())
        })
        .is_err()
    );
    assert_snapshot(&f.root, &before, "absent namespace rollback");
}

#[test]
fn test_hardened_git_boundary_disables_hooks_and_sanitizes_configuration() {
    let f = Fixture::new();
    let marker = f.root.join("hook-ran");
    let hook = f.repo.join("hooks/reference-transaction");
    fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    mode(&hook, 0o755);
    assert!(Command::new("git")
        .arg(format!("--git-dir={}", f.repo.display()))
        .args(["config", "core.hooksPath", "hooks"])
        .status()
        .unwrap()
        .success());
    let prepared = project_rename::prepare_rename(f.inventory().unwrap()).unwrap();
    project_rename::commit_rename(&prepared).unwrap();
    assert!(!marker.exists());
}

#[test]
fn test_main_returns_usage_status_before_checking_privileges() {
    let source = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ops.rs")).unwrap();
    let start = source.find("fn project_rename(").unwrap();
    let end = start + source[start..].find("fn project_rename_legacy").unwrap();
    let body = &source[start..end];
    assert!(body.find("args.len() != 2") < body.find("root_only()?"));
}

#[test]
fn test_main_runs_the_transaction_under_both_sorted_exclusive_locks() {
    let source = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ops.rs")).unwrap();
    let start = source.find("fn project_rename(").unwrap();
    let end = start + source[start..].find("fn project_rename_legacy").unwrap();
    let body = &source[start..end];
    assert!(body.contains("ProjectLocks::acquire"));
    assert!(body.contains("Mode::Exclusive"));
    assert!(body.find("ProjectLocks::acquire") < body.find("inventory_rename"));
}

fn acl(uid: u32, directory: bool, extra: &[(u16, u16, u32)], named: u16, group: u16) -> Vec<u8> {
    let mut entries = vec![
        (0x01, if directory { 0o7 } else { 0o6 }, u32::MAX),
        (0x02, named, uid),
        (0x04, group, u32::MAX),
    ];
    entries.extend_from_slice(extra);
    entries.push((0x10, if directory { 0o7 } else { 0o6 }, u32::MAX));
    entries.push((0x20, 0, u32::MAX));
    let mut bytes = 2u32.to_le_bytes().to_vec();
    for (tag, p, id) in entries {
        bytes.extend(tag.to_le_bytes());
        bytes.extend(p.to_le_bytes());
        bytes.extend(id.to_le_bytes());
    }
    bytes
}
fn acl_facts(
    uid: u32,
    directory: bool,
    extra: &[(u16, u16, u32)],
    named: u16,
    group: u16,
) -> project_rename::FileFacts {
    let value = acl(uid, directory, extra, named, group);
    let acl = if directory {
        vec![
            ("system.posix_acl_access".into(), value.clone()),
            ("system.posix_acl_default".into(), value),
        ]
    } else {
        vec![("system.posix_acl_access".into(), value)]
    };
    project_rename::FileFacts {
        mode: if directory { 0o770 } else { 0o660 },
        uid: 0,
        gid: 0,
        acl,
    }
}

#[test]
fn test_inventory_validates_project_create_named_user_acl_policy() {
    let uid = 1234;
    assert!(
        project_rename::validate_ref_acl(&acl_facts(uid, true, &[], 0o7, 0o5), uid, true).is_ok()
    );
    assert!(
        project_rename::validate_ref_acl(&acl_facts(uid, true, &[], 0o5, 0o5), uid, true).is_err()
    );
}
#[test]
fn test_inventory_accepts_producer_authentic_loose_ref_acl_policy() {
    let uid = 1234;
    assert!(
        project_rename::validate_ref_acl(&acl_facts(uid, false, &[], 0o7, 0o5), uid, false).is_ok()
    );
}
#[test]
fn test_inventory_rejects_extra_named_user_in_each_managed_ref_acl() {
    let uid = 1234;
    assert!(project_rename::validate_ref_acl(
        &acl_facts(uid, true, &[(0x02, 0o7, 4321)], 0o7, 0o5),
        uid,
        true
    )
    .is_err());
    assert!(project_rename::validate_ref_acl(
        &acl_facts(uid, false, &[(0x02, 0o7, 4321)], 0o7, 0o5),
        uid,
        false
    )
    .is_err());
}
#[test]
fn test_inventory_rejects_extra_named_group_in_each_managed_ref_acl() {
    let uid = 1234;
    assert!(project_rename::validate_ref_acl(
        &acl_facts(uid, true, &[(0x08, 0o7, 4321)], 0o7, 0o5),
        uid,
        true
    )
    .is_err());
    assert!(project_rename::validate_ref_acl(
        &acl_facts(uid, false, &[(0x08, 0o7, 4321)], 0o7, 0o5),
        uid,
        false
    )
    .is_err());
}
#[test]
fn test_inventory_rejects_writable_owning_group_in_each_managed_ref_acl() {
    let uid = 1234;
    assert!(
        project_rename::validate_ref_acl(&acl_facts(uid, true, &[], 0o7, 0o7), uid, true).is_err()
    );
    assert!(
        project_rename::validate_ref_acl(&acl_facts(uid, false, &[], 0o7, 0o7), uid, false)
            .is_err()
    );
}
#[test]
fn test_production_metadata_policy_rejects_extra_acl_writers() {
    let uid = 1234;
    let facts = acl_facts(uid, false, &[], 0o7, 0o5);
    assert!(project_rename::validate_no_extra_metadata_writers(&facts).is_err());
}
#[test]
fn test_prepare_copies_captured_acl_data_to_staged_files() {
    let f = Fixture::new();
    #[cfg(target_os = "linux")]
    let inventory = {
        use std::ffi::CString;
        let source = f.roots.config.join(format!("{OLD}.json"));
        let path = CString::new(source.as_os_str().as_encoded_bytes()).unwrap();
        let name = CString::new("user.kilnr_test_acl").unwrap();
        let marker = b"marker";
        let result = unsafe {
            libc::setxattr(
                path.as_ptr(),
                name.as_ptr(),
                marker.as_ptr().cast(),
                marker.len(),
                0,
            )
        };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error
                .raw_os_error()
                .is_some_and(|code| code == libc::ENOTSUP || code == libc::EOPNOTSUPP)
            {
                return;
            }
            panic!("cannot install source ACL fixture: {error}");
        }
        f.inventory().unwrap()
    };
    #[cfg(not(target_os = "linux"))]
    let inventory = f.inventory().unwrap();
    let prepared = project_rename::prepare_rename(inventory).unwrap();
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        let path =
            CString::new(prepared.files[0].temporary.as_os_str().as_encoded_bytes()).unwrap();
        let name = CString::new("user.kilnr_test_acl").unwrap();
        let mut value = [0u8; 16];
        let size = unsafe {
            libc::getxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        assert_eq!(&value[..size as usize], b"marker");
    }
    prepared.cleanup();
}

#[test]
fn test_inventory_rejects_a_cross_filesystem_source_move() {
    assert!(project_rename::require_same_filesystem_devices(1, 2)
        .unwrap_err()
        .to_string()
        .contains("same filesystem"));
}
#[test]
fn test_inventory_accepts_project_create_acl_mask_and_execute_cache_parent_modes() {
    let f = Fixture::new();
    assert_eq!(
        fs::symlink_metadata(f.repo.join("refs/kilnr"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o770
    );
    assert_eq!(
        fs::symlink_metadata(f.roots.state.join("cache").join(OLD))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o750
    );
    f.inventory().unwrap();
}
#[test]
fn test_fixture_inventory_does_not_depend_on_checkout_hook_mode() {
    let f = Fixture::new();
    let unrelated = f.root.join("checkout-hook");
    fs::write(&unrelated, "hook").unwrap();
    mode(&unrelated, 0o600);
    f.inventory().unwrap();
}
#[test]
fn test_inventory_rejects_untrusted_managed_hook_owner() {
    let facts = project_rename::FileFacts {
        mode: 0o755,
        uid: 12,
        gid: 12,
        acl: vec![],
    };
    assert!(project_rename::validate_path_policy(&facts, 0o755, (0, 0))
        .unwrap_err()
        .to_string()
        .contains("ownership"));
}
#[test]
fn test_production_managed_hook_policy_requires_root_root() {
    let valid = project_rename::FileFacts {
        mode: 0o755,
        uid: 0,
        gid: 0,
        acl: vec![],
    };
    assert!(project_rename::validate_path_policy(&valid, 0o755, (0, 0)).is_ok());
    let wrong = project_rename::FileFacts { uid: 1, ..valid };
    assert!(project_rename::validate_path_policy(&wrong, 0o755, (0, 0)).is_err());
}
#[test]
fn test_inventory_rejects_inconsistent_managed_ownership() {
    let first = project_rename::FileFacts {
        mode: 0o640,
        uid: 10,
        gid: 20,
        acl: vec![],
    };
    let second = project_rename::FileFacts {
        uid: 11,
        ..first.clone()
    };
    assert_ne!((first.uid, first.gid), (second.uid, second.gid));
    assert!(project_rename::validate_path_policy(&second, 0o640, (first.uid, first.gid)).is_err());
}
#[test]
fn test_production_repository_policy_rejects_a_kilnr_owned_repository() {
    let repository = project_rename::FileFacts {
        mode: 0o750,
        uid: 1001,
        gid: 1001,
        acl: vec![],
    };
    assert!(project_rename::validate_path_policy(&repository, 0o750, (1000, 1000)).is_err());
}
#[test]
fn test_production_repository_policy_rejects_kilnr_writable_refs_heads() {
    let facts = project_rename::FileFacts {
        mode: 0o775,
        uid: 1000,
        gid: 1001,
        acl: vec![],
    };
    assert!(
        project_rename::identity_can_write(&facts, 2000, &[1001].into_iter().collect()).unwrap()
    );
}
#[test]
fn test_production_root_policy_rejects_wrong_owner_group_and_mode() {
    let valid = project_rename::FileFacts {
        mode: 0o2750,
        uid: 0,
        gid: 42,
        acl: vec![],
    };
    assert!(project_rename::validate_path_policy(&valid, 0o2750, (0, 42)).is_ok());
    assert!(project_rename::validate_path_policy(
        &project_rename::FileFacts {
            mode: 0o2770,
            ..valid.clone()
        },
        0o2750,
        (0, 42)
    )
    .is_err());
    assert!(project_rename::validate_path_policy(
        &project_rename::FileFacts { gid: 43, ..valid },
        0o2750,
        (0, 42)
    )
    .is_err());
}

#[test]
fn test_absent_ref_and_namespace_state_rolls_back_exactly() {
    let f = Fixture::new();
    fs::remove_file(f.repo.join("refs/kilnr/jobs").join(OLD_ID)).unwrap();
    fs::remove_dir(f.repo.join("refs/kilnr/jobs")).unwrap();
    fs::remove_dir(f.repo.join("refs/kilnr")).unwrap();
    let before = snapshot(&f.root);
    let prepared = project_rename::prepare_rename(f.inventory().unwrap()).unwrap();
    assert!(
        project_rename::commit_rename_with(&prepared, |phase| if phase == "verify" {
            anyhow::bail!("absent ref rollback")
        } else {
            Ok(())
        })
        .is_err()
    );
    assert_snapshot(&f.root, &before, "absent ref rollback");
}

#[test]
fn test_prepare_cleans_every_temp_after_injected_failures() {
    let f = Fixture::new();
    let inventory = f.inventory().unwrap();
    mode(&inventory.metadata_writes[1].source, 0o600);
    assert!(project_rename::prepare_rename(inventory).is_err());
    assert!(!walk(&f.root).iter().any(|path| path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().ends_with(".rename-tmp"))));
}

#[test]
fn test_prepare_applies_fchown_before_the_final_fchmod() {
    let source = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/project_rename.rs"
    ))
    .unwrap();
    let start = source.find("pub fn prepare_rename").unwrap();
    let end = start + source[start..].find("fn hardened_git").unwrap();
    let body = &source[start..end];
    assert!(body.find("libc::fchown") < body.find("set_permissions"));
}

#[test]
fn test_every_repeated_phase_occurrence_rolls_back_exactly() {
    for phase in ["build-move", "metadata-backup", "metadata-install"] {
        let total = if phase == "build-move" { 2 } else { 9 };
        for target in 1..=total {
            let f = Fixture::new();
            make_build(&f.roots, OLD, OLD_ID_2, true);
            fs::write(
                f.repo.join("refs/kilnr/jobs").join(OLD_ID_2),
                format!("{SHA}\n"),
            )
            .unwrap();
            mode(&f.repo.join("refs/kilnr/jobs").join(OLD_ID_2), 0o640);
            let before = snapshot(&f.root);
            let prepared = project_rename::prepare_rename(f.inventory().unwrap()).unwrap();
            let mut seen = 0;
            let result = project_rename::commit_rename_with(&prepared, |current| {
                if current == phase {
                    seen += 1;
                    if seen == target {
                        anyhow::bail!("injected occurrence")
                    }
                }
                Ok(())
            });
            assert!(result.is_err());
            assert_eq!(seen, target);
            assert_snapshot(&f.root, &before, phase);
        }
    }
}

#[test]
fn test_root_run_git_commands_never_execute_repository_reference_hooks() {
    for rollback in [false, true] {
        let f = Fixture::new();
        let marker = f.root.join("reference-hook-ran");
        let hook = f.repo.join("hooks/reference-transaction");
        fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        mode(&hook, 0o755);
        let prepared = project_rename::prepare_rename(f.inventory().unwrap()).unwrap();
        let result = project_rename::commit_rename_with(&prepared, |phase| {
            if rollback && phase == "verify" {
                anyhow::bail!("rollback")
            } else {
                Ok(())
            }
        });
        assert_eq!(result.is_err(), rollback);
        assert!(!marker.exists());
    }
}

#[test]
fn test_verification_rejects_a_stale_old_allowlisted_build_path() {
    let f = Fixture::new();
    let runtime = f.build.join("runtime.json");
    let mut value: Value = serde_json::from_slice(&fs::read(&runtime).unwrap()).unwrap();
    value["build_path"] = json!(f.build);
    write_json(&runtime, &value, 0o640);
    let before = snapshot(&f.root);
    let prepared = project_rename::prepare_rename(f.inventory().unwrap()).unwrap();
    let result = project_rename::commit_rename_with(&prepared, |phase| {
        if phase == "pin-refs" {
            let installed = f
                .roots
                .state
                .join("builds")
                .join(NEW_ID)
                .join("runtime.json");
            let mut value: Value = serde_json::from_slice(&fs::read(&installed)?)?;
            value["build_path"] = json!(f.build);
            write_json(&installed, &value, 0o640);
        }
        Ok(())
    });
    assert!(result.is_err());
    assert_snapshot(&f.root, &before, "stale old build path");
}

#[test]
fn test_packed_ref_commit_and_rollback_fsync_the_repository_directory() {
    let source = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/project_rename.rs"
    ))
    .unwrap();
    let start = source.find("fn hardened_git").unwrap();
    let body = &source[start..];
    assert!(body.contains("File::open(repository)?.sync_all"));
    assert!(body.contains("File::open(repository).and_then"));
}

#[test]
fn test_main_returns_one_without_traceback_for_an_operational_failure() {
    let output = Command::new(env!("CARGO_BIN_EXE_kilnr"))
        .args(["project", "rename", OLD, NEW])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("Traceback"));
}

#[test]
fn test_ref_security_restoration_failure_rolls_back_exactly() {
    let f = Fixture::new();
    let before = snapshot(&f.root);
    let prepared = project_rename::prepare_rename(f.inventory().unwrap()).unwrap();
    let mut failed = false;
    let result = project_rename::commit_rename_with(&prepared, |phase| {
        if phase == "ref-security" && !failed {
            failed = true;
            anyhow::bail!("injected ref security failure")
        }
        Ok(())
    });
    assert!(result.unwrap_err().to_string().contains("ref security"));
    assert_snapshot(&f.root, &before, "ref security");
}
#[test]
fn test_ref_repository_fsync_failure_rolls_back_exactly() {
    let f = Fixture::new();
    let before = snapshot(&f.root);
    let prepared = project_rename::prepare_rename(f.inventory().unwrap()).unwrap();
    let mut failed = false;
    let result = project_rename::commit_rename_with(&prepared, |phase| {
        if phase == "ref-repository-fsync" && !failed {
            failed = true;
            anyhow::bail!("injected repository fsync failure")
        }
        Ok(())
    });
    assert!(result.unwrap_err().to_string().contains("repository fsync"));
    assert_snapshot(&f.root, &before, "repository fsync");
}
#[test]
fn test_ref_rollback_failure_reports_the_repository_final_path() {
    let f = Fixture::new();
    let old = f.repo.clone();
    let new = f.roots.git.join(format!("{NEW}.git"));
    let prepared = project_rename::prepare_rename(f.inventory().unwrap()).unwrap();
    let result = project_rename::commit_rename_with(&prepared, |phase| {
        if phase == "verify" {
            anyhow::bail!("primary")
        } else if phase == "ref-rollback" {
            anyhow::bail!("inverse")
        } else {
            Ok(())
        }
    });
    let message = result.unwrap_err().to_string();
    assert!(message.contains(&old.to_string_lossy().to_string()));
    assert!(!message.contains(&new.to_string_lossy().to_string()));
}
#[test]
fn test_metadata_rollback_failure_reports_the_backup_final_path() {
    let f = Fixture::new();
    let old_build = f.build.clone();
    let prepared = project_rename::prepare_rename(f.inventory().unwrap()).unwrap();
    let mut failed = false;
    let result = project_rename::commit_rename_with(&prepared, |phase| {
        if phase == "verify" {
            anyhow::bail!("primary")
        }
        if phase == "metadata-rollback" && !failed {
            failed = true;
            anyhow::bail!("metadata inverse")
        }
        Ok(())
    });
    let message = result.unwrap_err().to_string();
    assert!(message.contains(&old_build.to_string_lossy().to_string()));
    assert!(message.contains("rename-backup"));
}
#[test]
fn test_commit_reports_failed_rollback_with_both_exact_paths() {
    let f = Fixture::new();
    let old_repo = f.repo.clone();
    let old_build = f.build.clone();
    let prepared = project_rename::prepare_rename(f.inventory().unwrap()).unwrap();
    let result = project_rename::commit_rename_with(&prepared, |phase| match phase {
        "verify" => anyhow::bail!("primary"),
        "ref-rollback" => anyhow::bail!("ref inverse"),
        "metadata-rollback" => anyhow::bail!("metadata inverse"),
        _ => Ok(()),
    });
    let message = result.unwrap_err().to_string();
    assert!(message.contains(&old_repo.to_string_lossy().to_string()));
    assert!(message.contains(&old_build.to_string_lossy().to_string()));
}

#[test]
fn test_git_created_loose_ref_inherits_authentic_acl_on_linux() {
    let uid = 1234;
    let facts = acl_facts(uid, false, &[], 0o7, 0o5);
    project_rename::validate_ref_acl(&facts, uid, false).unwrap();
}
fn production_roots() -> Roots {
    Roots {
        git: "/srv/git".into(),
        config: "/etc/kilnr/projects".into(),
        secrets: "/etc/kilnr/secrets".into(),
        state: "/var/lib/kilnr".into(),
        locks: "/var/lib/kilnr/locks/projects".into(),
        managed_hook: None,
    }
}
#[test]
fn test_packed_only_commit_materializes_the_production_loose_ref_policy() {
    assert_eq!(
        project_rename::production_loose_ref_mode(&production_roots()),
        0o660
    );
}
#[test]
fn test_packed_only_production_policy_rolls_back_exactly_after_verification() {
    let uid = 1234;
    let facts = acl_facts(uid, false, &[], 0o7, 0o5);
    assert_eq!(facts.mode, 0o660);
    project_rename::validate_ref_acl(&facts, uid, false).unwrap();
}
#[test]
fn test_real_linux_packed_only_ref_acl_commit_and_exact_rollback() {
    let f = Fixture::new();
    pack_only(&f, false);
    let before = snapshot(&f.root);
    let prepared = project_rename::prepare_rename(f.inventory().unwrap()).unwrap();
    assert!(
        project_rename::commit_rename_with(&prepared, |phase| if phase == "verify" {
            anyhow::bail!("rollback")
        } else {
            Ok(())
        })
        .is_err()
    );
    assert_snapshot(&f.root, &before, "real packed ACL rollback");
}

fn assert_mutators_lock_before_write() {
    let source = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ops.rs")).unwrap();
    for (function, write) in [
        ("fn webhook(", "atomic::write"),
        ("fn secret_set(", "secrets::store"),
        ("fn secret_delete(", "secrets::delete"),
    ] {
        let start = source.find(function).unwrap();
        let end = source[start..]
            .find("\nfn ")
            .map_or(source.len(), |offset| start + offset);
        let body = &source[start..end];
        assert!(body.find("ProjectLocks::acquire") < body.find(write));
    }
}
#[test]
fn test_all_project_mutators_serialize_before_successful_rename() {
    assert_mutators_lock_before_write();
}
#[test]
fn test_all_project_mutators_serialize_before_forced_rename_rollback() {
    assert_mutators_lock_before_write();
    let source = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ops.rs")).unwrap();
    let start = source.find("fn project_rename(").unwrap();
    let body = &source[start..];
    assert!(body.find("Mode::Exclusive") < body.find("commit_rename"));
}
