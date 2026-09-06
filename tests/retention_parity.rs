use chrono::{DateTime, Duration, Utc};
use fs2::FileExt;
use kilnr::{ops, project_lock, retention};
use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn now() -> DateTime<Utc> {
    "2026-09-04T00:00:00Z".parse().unwrap()
}

struct Fixture {
    _temp: tempfile::TempDir,
    roots: retention::Roots,
    builds: PathBuf,
    locks: PathBuf,
    sequence: u32,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let state = root.join("state");
        let config = root.join("config");
        let git = root.join("git");
        for path in [
            state.join("builds"),
            state.join("queue/incoming"),
            state.join("queue/running"),
            state.join("locks/projects"),
            config.clone(),
            git.clone(),
        ] {
            fs::create_dir_all(path).unwrap();
        }
        let locks = state.join("locks/projects");
        fs::set_permissions(&locks, fs::Permissions::from_mode(0o750)).unwrap();
        let controller = state.join("locks/controller.lock");
        File::create(&controller).unwrap();
        fs::set_permissions(controller, fs::Permissions::from_mode(0o660)).unwrap();
        let builds = state.join("builds");
        let mut fixture = Self {
            _temp: temp,
            roots: retention::Roots { state, config, git },
            builds,
            locks,
            sequence: 0,
        };
        fixture.project("demo", Some(retention::Policy::DEFAULT));
        fixture
    }

    fn project(&mut self, name: &str, policy: Option<retention::Policy>) {
        let repository = self.roots.git.join(format!("{name}.git"));
        fs::create_dir_all(repository.join("refs/kilnr/jobs")).unwrap();
        fs::create_dir_all(repository.join("refs/heads")).unwrap();
        fs::create_dir_all(repository.join("refs/tags")).unwrap();
        let mut config = json!({
            "schema": 1,
            "project": name,
            "repository": repository.to_string_lossy(),
        });
        if let Some(policy) = policy {
            config["retention"] = json!({
                "max_age_days": policy.max_age_days,
                "max_builds_per_ref": policy.max_builds_per_ref,
                "keep_releases": policy.keep_releases,
            });
        }
        write_json(&self.roots.config.join(format!("{name}.json")), &config);
        project_lock::provision(&self.locks, &[name.to_owned()]).unwrap();
    }

    fn build(
        &mut self,
        age: i64,
        project: &str,
        reference: &str,
        kind: &str,
        state: &str,
    ) -> PathBuf {
        self.sequence += 1;
        let finished = now() - Duration::days(age);
        let received = finished - Duration::hours(1);
        let sha = "a".repeat(40);
        let id = format!(
            "{}-{project}-{}-{:08x}",
            received.format("%Y%m%dT%H%M%S%6fZ"),
            &sha[..7],
            self.sequence
        );
        let path = self.builds.join(&id);
        fs::create_dir(&path).unwrap();
        let job = json!({
            "schema": 1, "id": id, "project": project,
            "received_at": received.to_rfc3339(), "old_sha": sha, "new_sha": sha,
            "sha": sha, "ref": reference, "type": kind,
            "pin_ref": format!("refs/kilnr/jobs/{id}"),
        });
        let mut status = job.clone();
        status["build_id"] = json!(id);
        status["job_id"] = json!(id);
        status["state"] = json!(state);
        status["finished_at"] = json!(finished.to_rfc3339());
        write_json(&path.join("job.json"), &job);
        write_json(&path.join("status.json"), &status);
        fs::create_dir(path.join("work")).unwrap();
        fs::write(path.join("work/data"), "payload").unwrap();
        path
    }

    fn ordinary_build(&mut self, age: i64) -> PathBuf {
        self.build(age, "demo", "refs/heads/main", "ci", "success")
    }

    fn cleanup(&self, project: Option<&str>, dry_run: bool) -> retention::CleanupReport {
        retention::cleanup(
            &self.roots,
            &retention::CleanupOptions {
                project: project.map(str::to_owned),
                dry_run,
                now: now(),
            },
        )
        .unwrap()
    }

    fn mutate(&self, build: &Path, filename: &str, changes: Value) {
        let path = build.join(filename);
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        for (key, replacement) in changes.as_object().unwrap() {
            value_at(&mut value, key).clone_from(replacement);
        }
        write_json(&path, &value);
    }
}

fn value_at<'a>(value: &'a mut Value, key: &str) -> &'a mut Value {
    value
        .as_object_mut()
        .unwrap()
        .entry(key)
        .or_insert(Value::Null)
}

fn write_json(path: &Path, value: &Value) {
    kilnr::atomic::write_json(path, value, 0o644).unwrap();
}

fn disabled_with_count(count: usize) -> retention::Policy {
    retention::Policy {
        max_builds_per_ref: Some(count),
        ..retention::Policy::DISABLED
    }
}

#[test]
fn test_age_and_exact_boundary() {
    let mut fixture = Fixture::new();
    let new = fixture.ordinary_build(1);
    let boundary = fixture.ordinary_build(30);
    let old = fixture.ordinary_build(31);
    let report = fixture.cleanup(None, false);
    assert_eq!(report.code, 0);
    assert!(new.exists() && boundary.exists() && !old.exists());
    assert!(report.lines.join("\n").contains("max age"));
}

#[test]
fn test_count_per_project_and_ref() {
    let mut fixture = Fixture::new();
    fixture.project("demo", Some(disabled_with_count(2)));
    fixture.project("other", Some(disabled_with_count(2)));
    let mut groups = vec![];
    for (project, reference) in [
        ("demo", "refs/heads/main"),
        ("demo", "refs/heads/feature/x"),
        ("other", "refs/heads/main"),
    ] {
        groups.push(
            (1..=4)
                .map(|age| fixture.build(age, project, reference, "ci", "success"))
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(fixture.cleanup(None, false).code, 0);
    for group in groups {
        assert_eq!(
            group.iter().map(|path| path.exists()).collect::<Vec<_>>(),
            [true, true, false, false]
        );
    }
}

#[test]
fn test_completion_order_and_tie_breaker() {
    let mut fixture = Fixture::new();
    fixture.project("demo", Some(disabled_with_count(1)));
    let a = fixture.ordinary_build(2);
    let b = fixture.ordinary_build(1);
    fixture.mutate(
        &a,
        "status.json",
        json!({"finished_at": now().to_rfc3339()}),
    );
    assert_eq!(fixture.cleanup(None, false).code, 0);
    assert!(a.exists() && !b.exists());
    let c = fixture.ordinary_build(0);
    assert_eq!(fixture.cleanup(None, false).code, 0);
    assert!(!a.exists() && c.exists());
}

#[test]
fn test_age_and_count_are_union() {
    let mut fixture = Fixture::new();
    fixture.project(
        "demo",
        Some(retention::Policy {
            max_builds_per_ref: Some(1),
            ..retention::Policy::DEFAULT
        }),
    );
    let old = fixture.ordinary_build(40);
    let older = fixture.ordinary_build(50);
    let report = fixture.cleanup(None, false);
    assert_eq!(report.code, 0);
    assert!(!old.exists() && !older.exists());
    assert!(report
        .lines
        .join("\n")
        .contains("max age, excess builds for ref"));
}

#[test]
fn test_releases_preserved_and_explicit_opt_out() {
    let mut fixture = Fixture::new();
    let release = fixture.build(100, "demo", "refs/tags/v1.0.0", "release", "success");
    assert_eq!(fixture.cleanup(None, false).code, 0);
    assert!(release.exists());
    fixture.project(
        "demo",
        Some(retention::Policy {
            keep_releases: false,
            ..retention::Policy::DEFAULT
        }),
    );
    assert_eq!(fixture.cleanup(None, false).code, 0);
    assert!(!release.exists());
}

#[test]
fn test_nonterminal_and_active_queues() {
    let mut fixture = Fixture::new();
    let mut paths = ["running", "preparing", "queued"]
        .map(|state| fixture.build(50, "demo", "refs/heads/main", "ci", state))
        .to_vec();
    for queue in ["incoming", "running"] {
        let path = fixture.ordinary_build(50);
        fs::copy(
            path.join("job.json"),
            fixture.roots.state.join("queue").join(queue).join(format!(
                "{}.json",
                path.file_name().unwrap().to_string_lossy()
            )),
        )
        .unwrap();
        paths.push(path);
    }
    assert_eq!(fixture.cleanup(None, false).code, 0);
    assert!(paths.iter().all(|path| path.exists()));
}

#[test]
fn test_corrupt_queue_fails_closed() {
    let mut fixture = Fixture::new();
    let path = fixture.ordinary_build(50);
    fs::write(fixture.roots.state.join("queue/incoming/bad.json"), "[]").unwrap();
    assert!(retention::cleanup(
        &fixture.roots,
        &retention::CleanupOptions {
            project: None,
            dry_run: false,
            now: now()
        }
    )
    .is_err());
    assert!(path.exists());
}

#[test]
fn test_terminal_failures_are_eligible() {
    let mut fixture = Fixture::new();
    let paths = ["failed", "aborted"]
        .map(|state| fixture.build(50, "demo", "refs/heads/main", "ci", state));
    assert_eq!(fixture.cleanup(None, false).code, 0);
    assert!(paths.iter().all(|path| !path.exists()));
}

#[test]
fn test_historical_enqueue_timestamp_skew_is_accepted() {
    let mut fixture = Fixture::new();
    let path = fixture.ordinary_build(50);
    let job: Value =
        serde_json::from_slice(&fs::read(path.join("job.json")).unwrap()).unwrap();
    let received = DateTime::parse_from_rfc3339(job["received_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&Utc)
        + Duration::milliseconds(3);
    let received = received.to_rfc3339();

    fixture.mutate(&path, "job.json", json!({"received_at": received}));
    fixture.mutate(&path, "status.json", json!({"received_at": received}));

    let report = fixture.cleanup(None, false);
    assert_eq!(report.code, 0);
    assert!(!path.exists());
}

#[test]
fn test_excessive_enqueue_timestamp_skew_is_refused() {
    let mut fixture = Fixture::new();
    let path = fixture.ordinary_build(50);
    let job: Value =
        serde_json::from_slice(&fs::read(path.join("job.json")).unwrap()).unwrap();
    let received = DateTime::parse_from_rfc3339(job["received_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&Utc)
        + Duration::seconds(2);
    let received = received.to_rfc3339();

    fixture.mutate(&path, "job.json", json!({"received_at": received}));
    fixture.mutate(&path, "status.json", json!({"received_at": received}));

    let report = fixture.cleanup(None, false);
    assert_eq!(report.code, 1);
    assert!(path.exists());
}

#[test]
fn test_metadata_identity_failures() {
    let mut fixture = Fixture::new();
    let variants = [
        ("job.json", json!({"project":"other"})),
        ("status.json", json!({"project":"other"})),
        ("job.json", json!({"pin_ref":"refs/heads/main"})),
        ("job.json", json!({"id":"../victim"})),
        ("job.json", json!({"sha":"b".repeat(40)})),
        ("status.json", json!({"sha":"b".repeat(40)})),
        ("status.json", json!({"finished_at":null})),
        ("status.json", json!({"finished_at":"2020-01-01"})),
        ("status.json", json!({"job_id":"../victim"})),
        ("job.json", json!({"ref":"refs/heads/../victim"})),
    ];
    let mut paths = vec![];
    for (filename, changes) in variants {
        let path = fixture.ordinary_build(50);
        fixture.mutate(&path, filename, changes);
        paths.push(path);
    }
    let victim = fixture._temp.path().join("victim");
    fs::write(&victim, "keep").unwrap();
    assert_eq!(fixture.cleanup(None, false).code, 1);
    assert!(paths.iter().all(|path| path.exists()));
    assert_eq!(fs::read_to_string(victim).unwrap(), "keep");
}

#[test]
fn test_symlink_build_and_metadata_are_refused() {
    let mut fixture = Fixture::new();
    let path = fixture.ordinary_build(50);
    let target = fixture._temp.path().join("outside");
    fs::rename(&path, &target).unwrap();
    symlink(&target, &path).unwrap();
    let other = fixture.ordinary_build(50);
    let outside_status = fixture._temp.path().join("status");
    fs::rename(other.join("status.json"), &outside_status).unwrap();
    symlink(&outside_status, other.join("status.json")).unwrap();
    assert_eq!(fixture.cleanup(None, false).code, 1);
    assert!(target.join("work/data").exists() && other.exists());
}

#[test]
fn test_payload_symlinks_are_unlinked_not_followed() {
    let mut fixture = Fixture::new();
    let path = fixture.ordinary_build(50);
    let target = fixture._temp.path().join("outside");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("precious"), "keep").unwrap();
    symlink(&target, path.join("work/link")).unwrap();
    assert_eq!(fixture.cleanup(None, false).code, 0);
    assert!(!path.exists());
    assert_eq!(fs::read_to_string(target.join("precious")).unwrap(), "keep");
}

#[test]
fn test_symlink_roots_and_project_traversal() {
    let mut fixture = Fixture::new();
    let path = fixture.ordinary_build(50);
    let alias = fixture._temp.path().join("alias");
    symlink(&fixture.roots.state, &alias).unwrap();
    let roots = retention::Roots {
        state: alias,
        ..fixture.roots.clone()
    };
    assert!(retention::cleanup(
        &roots,
        &retention::CleanupOptions {
            project: None,
            dry_run: false,
            now: now()
        }
    )
    .is_err());
    assert!(retention::cleanup(
        &fixture.roots,
        &retention::CleanupOptions {
            project: Some("../demo".into()),
            dry_run: false,
            now: now()
        }
    )
    .is_err());
    assert!(path.exists());
}

#[test]
fn test_hardlinked_metadata_refused() {
    let mut fixture = Fixture::new();
    let path = fixture.ordinary_build(50);
    fs::hard_link(path.join("job.json"), fixture._temp.path().join("job-link")).unwrap();
    assert_eq!(fixture.cleanup(None, false).code, 1);
    assert!(path.exists());
}

#[test]
fn test_metadata_nonowner_writers_refused() {
    let mut fixture = Fixture::new();
    let path = fixture.ordinary_build(50);
    fs::set_permissions(path.join("status.json"), fs::Permissions::from_mode(0o666)).unwrap();
    assert_eq!(fixture.cleanup(None, false).code, 1);
    assert!(path.exists());
}

#[test]
fn test_dry_run_same_candidates_and_idempotence() {
    let mut fixture = Fixture::new();
    let paths = [1, 40, 50].map(|age| fixture.ordinary_build(age));
    let pins = fixture.roots.git.join("demo.git/refs/kilnr/jobs");
    fs::write(
        pins.join(paths[1].file_name().unwrap()),
        format!("{}\n", "a".repeat(40)),
    )
    .unwrap();
    let report = fixture.cleanup(None, true);
    let dry = report
        .lines
        .iter()
        .map(|line| line.replace("Would delete", "Deleting"))
        .collect::<Vec<_>>();
    assert!(paths.iter().all(|path| path.exists()));
    assert!(pins.join(paths[1].file_name().unwrap()).exists());
    assert!(!fs::read_dir(&fixture.builds).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(retention::TRANSACTION_PREFIX)));
    let real = fixture.cleanup(None, false);
    assert_eq!(dry, real.lines);
    assert!(!pins.join(paths[1].file_name().unwrap()).exists());
    assert!(fixture.cleanup(None, false).lines.is_empty());
}

#[test]
fn test_pin_mismatch_and_symlink_and_lock_retained() {
    let mut fixture = Fixture::new();
    let pins = fixture.roots.git.join("demo.git/refs/kilnr/jobs");
    let a = fixture.ordinary_build(50);
    let b = fixture.ordinary_build(50);
    let c = fixture.ordinary_build(50);
    fs::write(
        pins.join(a.file_name().unwrap()),
        format!("{}\n", "b".repeat(40)),
    )
    .unwrap();
    symlink(
        pins.join(a.file_name().unwrap()),
        pins.join(b.file_name().unwrap()),
    )
    .unwrap();
    File::create(pins.join(format!("{}.lock", c.file_name().unwrap().to_string_lossy()))).unwrap();
    assert_eq!(fixture.cleanup(None, false).code, 1);
    assert!([a, b, c].iter().all(|path| path.exists()));
}

#[test]
fn test_packed_pin_fails_closed_and_ordinary_refs_untouched() {
    let mut fixture = Fixture::new();
    let path = fixture.ordinary_build(50);
    let repository = fixture.roots.git.join("demo.git");
    let ordinary = repository.join("refs/heads/main");
    fs::write(&ordinary, format!("{}\n", "a".repeat(40))).unwrap();
    let packed = repository.join("packed-refs");
    fs::write(
        &packed,
        format!(
            "{} refs/kilnr/jobs/{}\n",
            "a".repeat(40),
            path.file_name().unwrap().to_string_lossy()
        ),
    )
    .unwrap();
    let report = fixture.cleanup(None, false);
    assert_eq!(report.code, 1);
    assert!(path.exists() && report.lines.join("\n").contains("administrator repair"));
    fs::remove_file(packed).unwrap();
    assert_eq!(fixture.cleanup(None, false).code, 0);
    assert!(ordinary.exists());
}

#[test]
fn test_existing_config_and_policy_validation() {
    let mut fixture = Fixture::new();
    fixture.project("demo", None);
    let path = fixture.ordinary_build(100);
    assert_eq!(fixture.cleanup(None, false).code, 0);
    assert!(path.exists());
    for value in [
        json!(null),
        json!([]),
        json!({"max_age_days":0}),
        json!({"max_age_days":true}),
        json!({"max_builds_per_ref":-1}),
        json!({"keep_releases":"yes"}),
        json!({"max_age_day":30}),
    ] {
        assert!(retention::policy(&json!({"retention":value})).is_err());
    }
    assert_eq!(
        retention::policy(&json!({"retention":{}})).unwrap(),
        retention::Policy::DISABLED
    );
}

#[test]
fn test_project_scope_and_malformed_config() {
    let mut fixture = Fixture::new();
    fixture.project("other", Some(retention::Policy::DEFAULT));
    let a = fixture.ordinary_build(50);
    let b = fixture.build(50, "other", "refs/heads/main", "ci", "success");
    assert_eq!(fixture.cleanup(Some("demo"), false).code, 0);
    assert!(!a.exists() && b.exists());
    let config = fixture.roots.config.join("other.json");
    let mut value: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
    value["repository"] = json!(fixture.roots.git.join("demo.git").to_string_lossy());
    write_json(&config, &value);
    assert_eq!(fixture.cleanup(None, false).code, 1);
    assert!(b.exists());
}

#[test]
fn test_controller_and_project_locks() {
    let mut fixture = Fixture::new();
    let path = fixture.ordinary_build(50);
    let controller = OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.roots.state.join("locks/controller.lock"))
        .unwrap();
    controller.lock_exclusive().unwrap();
    let report = fixture.cleanup(None, false);
    assert_eq!(report.code, 0);
    assert!(path.exists() && report.lines[0].contains("controller is active"));
    fs2::FileExt::unlock(&controller).unwrap();
    let guard = project_lock::ProjectLocks::acquire(
        &fixture.locks,
        &["demo".into()],
        project_lock::Mode::Shared,
        false,
    )
    .unwrap();
    let report = fixture.cleanup(None, false);
    assert_eq!(report.code, 0);
    assert!(path.exists() && report.lines[0].contains("project demo is busy"));
    drop(guard);
    assert_eq!(fixture.cleanup(None, false).code, 0);
    assert!(!path.exists());
}

#[test]
fn test_status_lock_protects_terminal_build() {
    let mut fixture = Fixture::new();
    let path = fixture.ordinary_build(50);
    let lock_path = path.join("status.lock");
    File::create(&lock_path).unwrap();
    fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o640)).unwrap();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    lock.lock_exclusive().unwrap();
    assert_eq!(fixture.cleanup(None, false).code, 1);
    assert!(path.exists());
    fs2::FileExt::unlock(&lock).unwrap();
    assert_eq!(fixture.cleanup(None, false).code, 0);
    assert!(!path.exists());
}

#[test]
fn test_invalid_directory_names_remain_untouched() {
    let fixture = Fixture::new();
    for name in [
        "demo",
        "..demo",
        "20260901-demo-abcdefg",
        "20260901T000000000000Z-other-aaaaaaa-00000001",
    ] {
        fs::create_dir(fixture.builds.join(name)).unwrap();
    }
    assert_eq!(fixture.cleanup(Some("demo"), false).code, 0);
    assert_eq!(fs::read_dir(fixture.builds).unwrap().count(), 4);
}

#[test]
fn test_pin_repository_symlink_cannot_cross_project() {
    let mut fixture = Fixture::new();
    fixture.project("other", Some(retention::Policy::DEFAULT));
    let path = fixture.ordinary_build(50);
    let original = fixture.roots.git.join("demo.git");
    fs::remove_dir_all(&original).unwrap();
    symlink(fixture.roots.git.join("other.git"), &original).unwrap();
    let other_pin = fixture
        .roots
        .git
        .join("other.git/refs/kilnr/jobs")
        .join(path.file_name().unwrap());
    fs::write(&other_pin, "a".repeat(40)).unwrap();
    assert_eq!(fixture.cleanup(Some("demo"), false).code, 1);
    assert!(path.exists() && other_pin.exists());
}

#[test]
fn test_bind_mounts_on_same_device_are_refused() {
    let mut fixture = Fixture::new();
    let path = fixture.ordinary_build(50);
    let line = format!(
        "42 1 8:1 /outside {}/work rw - ext4 /dev/sda rw\n",
        path.display()
    );
    assert!(retention::reject_nested_mounts(&fixture.builds, &line).is_err());
    retention::reject_nested_mounts(
        &fixture.builds,
        &format!(
            "42 1 8:1 / {} rw - ext4 /dev/sda rw\n",
            fixture.builds.display()
        ),
    )
    .unwrap();
}

fn git(repository: &Path, args: &[&str], input: Option<&str>) -> String {
    let mut command = Command::new("git");
    command
        .arg(format!("--git-dir={}", repository.display()))
        .args(args);
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    command
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.test")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.test");
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(input) = input {
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn test_real_git_pin_removal_preserves_branches_tags_and_other_pins() {
    let mut fixture = Fixture::new();
    let repository = fixture.roots.git.join("demo.git");
    git(&repository, &["init", "--bare"], None);
    git(&repository, &["config", "gc.packRefs", "false"], None);
    let blob = git(&repository, &["hash-object", "-w", "--stdin"], Some("test"));
    let tree = git(
        &repository,
        &["mktree"],
        Some(&format!("100644 blob {blob}\tfile\n")),
    );
    let sha = git(&repository, &["commit-tree", &tree], Some("test\n"));
    let original = fixture.ordinary_build(50);
    let id = original
        .file_name()
        .unwrap()
        .to_string_lossy()
        .replace("aaaaaaa", &sha[..7]);
    let path = original.with_file_name(&id);
    fs::rename(&original, &path).unwrap();
    fixture.mutate(&path, "job.json", json!({"id":id,"old_sha":sha,"new_sha":sha,"sha":sha,"pin_ref":format!("refs/kilnr/jobs/{id}")}));
    fixture.mutate(
        &path,
        "status.json",
        json!({"build_id":id,"job_id":id,"sha":sha}),
    );
    for reference in [
        format!("refs/kilnr/jobs/{id}"),
        "refs/kilnr/jobs/unrelated".into(),
        "refs/heads/main".into(),
        "refs/tags/v1.0.0".into(),
    ] {
        git(&repository, &["update-ref", &reference, &sha], None);
    }
    assert_eq!(fixture.cleanup(None, false).code, 0);
    assert_eq!(
        git(&repository, &["for-each-ref", "--format=%(refname)"], None)
            .lines()
            .collect::<Vec<_>>(),
        [
            "refs/heads/main",
            "refs/kilnr/jobs/unrelated",
            "refs/tags/v1.0.0"
        ]
    );
}

struct Disappear(PathBuf);
impl retention::CleanupHooks for Disappear {
    fn after_candidates(&self, _: &[retention::Candidate]) -> anyhow::Result<()> {
        fs::remove_dir_all(&self.0)?;
        Ok(())
    }
}

#[test]
fn test_concurrent_disappearance() {
    let mut fixture = Fixture::new();
    let path = fixture.ordinary_build(50);
    let report = retention::cleanup_with_hooks(
        &fixture.roots,
        &retention::CleanupOptions {
            project: None,
            dry_run: false,
            now: now(),
        },
        &Disappear(path),
    )
    .unwrap();
    assert_eq!(report.code, 0);
}

struct InterruptRemove {
    partial: bool,
}
impl retention::CleanupHooks for InterruptRemove {
    fn before_transaction_remove(&self, transaction: &Path) -> anyhow::Result<()> {
        if self.partial {
            fs::remove_file(transaction.join("build/job.json"))?;
            fs::remove_file(transaction.join("build/status.json"))?;
        }
        anyhow::bail!("simulated interruption")
    }
}

#[test]
fn test_interrupted_deletion_resumes_even_if_policy_disabled() {
    let mut fixture = Fixture::new();
    let path = fixture.ordinary_build(50);
    let report = retention::cleanup_with_hooks(
        &fixture.roots,
        &retention::CleanupOptions {
            project: None,
            dry_run: false,
            now: now(),
        },
        &InterruptRemove { partial: false },
    )
    .unwrap();
    assert_eq!(report.code, 1);
    assert!(!path.exists());
    let pending = fixture.builds.join(format!(
        "{}{}",
        retention::TRANSACTION_PREFIX,
        path.file_name().unwrap().to_string_lossy()
    ));
    assert!(pending.join("record.json").exists());
    fixture.project("demo", None);
    assert_eq!(fixture.cleanup(None, true).code, 0);
    assert!(pending.exists());
    assert_eq!(fixture.cleanup(None, false).code, 0);
    assert!(!pending.exists());
}

#[test]
fn test_recovery_survives_partial_payload_metadata_removal() {
    let mut fixture = Fixture::new();
    fixture.ordinary_build(50);
    let report = retention::cleanup_with_hooks(
        &fixture.roots,
        &retention::CleanupOptions {
            project: None,
            dry_run: false,
            now: now(),
        },
        &InterruptRemove { partial: true },
    )
    .unwrap();
    assert_eq!(report.code, 1);
    assert_eq!(fixture.cleanup(None, false).code, 0);
    assert_eq!(fs::read_dir(&fixture.builds).unwrap().count(), 0);
}

struct InterruptRename;
impl retention::CleanupHooks for InterruptRename {
    fn before_retire_rename(&self, _: &Path, _: &Path) -> anyhow::Result<()> {
        anyhow::bail!("simulated interruption")
    }
}

#[test]
fn test_interrupted_before_rename_preserves_original() {
    let mut fixture = Fixture::new();
    let path = fixture.ordinary_build(50);
    let report = retention::cleanup_with_hooks(
        &fixture.roots,
        &retention::CleanupOptions {
            project: None,
            dry_run: false,
            now: now(),
        },
        &InterruptRename,
    )
    .unwrap();
    assert_eq!(report.code, 1);
    assert!(path.exists());
    fixture.project("demo", None);
    assert_eq!(fixture.cleanup(None, false).code, 0);
    assert!(path.exists());
    assert_eq!(fs::read_dir(&fixture.builds).unwrap().count(), 1);
}

#[test]
fn test_transaction_cannot_cross_project_or_escape() {
    let mut fixture = Fixture::new();
    let path = fixture.ordinary_build(50);
    let pending = fixture.builds.join(format!(
        "{}{}",
        retention::TRANSACTION_PREFIX,
        path.file_name().unwrap().to_string_lossy()
    ));
    fs::create_dir(&pending).unwrap();
    let mut job: Value = serde_json::from_slice(&fs::read(path.join("job.json")).unwrap()).unwrap();
    let status: Value =
        serde_json::from_slice(&fs::read(path.join("status.json")).unwrap()).unwrap();
    job["project"] = json!("other");
    write_json(
        &pending.join("record.json"),
        &json!({"job":job,"status":status}),
    );
    let outside = fixture._temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    File::create(outside.join("keep")).unwrap();
    symlink(&outside, pending.join("build")).unwrap();
    fixture.project("demo", None);
    assert_eq!(fixture.cleanup(None, false).code, 1);
    assert!(outside.join("keep").exists());
}

struct DifferentDevice;
impl retention::CleanupHooks for DifferentDevice {
    fn tree_device(&self, device: u64) -> u64 {
        device + 1
    }
}

#[test]
fn test_mount_boundary_refused() {
    let mut fixture = Fixture::new();
    let path = fixture.ordinary_build(50);
    let report = retention::cleanup_with_hooks(
        &fixture.roots,
        &retention::CleanupOptions {
            project: None,
            dry_run: false,
            now: now(),
        },
        &DifferentDevice,
    )
    .unwrap();
    assert_eq!(report.code, 1);
    assert!(path.exists());
}

#[test]
fn test_rerun_holds_project_lock_until_enqueue() {
    let source = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/ops.rs")).unwrap();
    let start = source.find("fn rerun(").unwrap();
    let end = source[start..].find("fn git_key_add(").unwrap() + start;
    let body = &source[start..end];
    let lock = body.find("ProjectLocks::acquire").unwrap();
    let metadata = body.find("status.json").unwrap();
    let enqueue = body.find("Command::new").unwrap();
    assert!(lock < metadata && lock < enqueue);
    assert!(body.contains("Mode::Shared"));
    assert!(body.contains("let _lock"));
}

#[test]
fn test_unit_and_defaults_installation_twice() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = fs::read_to_string(root.join("install.sh")).unwrap();
    let defaults_start = script
        .find("if [[ ! -f /etc/kilnr/defaults.json ]]")
        .unwrap();
    let defaults_end = script[defaults_start..].find("\nfi").unwrap() + defaults_start + 3;
    let units_start = script.find("for unit in ").unwrap();
    let units_end = script[units_start..].find("\necho").unwrap() + units_start;
    let temp = tempfile::tempdir().unwrap();
    let etc = temp.path().join("etc");
    fs::create_dir_all(etc.join("kilnr")).unwrap();
    fs::create_dir_all(etc.join("systemd/system")).unwrap();
    let defaults = etc.join("kilnr/defaults.json");
    fs::write(&defaults, "{\"existing\":true}\n").unwrap();
    let body = format!(
        "set -eu\nsystemctl() {{ :; }}\n{}\n{}",
        &script[defaults_start..defaults_end],
        &script[units_start..units_end]
    )
    .replace("/etc/", &format!("{}/", etc.display()))
    .replace("install -o root -g root", "install");
    for _ in 0..2 {
        let status = Command::new("bash")
            .arg("-c")
            .arg(&body)
            .env("ROOT_DIR", root)
            .status()
            .unwrap();
        assert!(status.success());
    }
    assert_eq!(
        fs::read_to_string(defaults).unwrap(),
        "{\"existing\":true}\n"
    );
    for unit in ["kilnr-cleanup.service", "kilnr-cleanup.timer"] {
        assert_eq!(
            fs::read(etc.join("systemd/system").join(unit)).unwrap(),
            fs::read(root.join("systemd").join(unit)).unwrap()
        );
    }
    let service = fs::read_to_string(root.join("systemd/kilnr-cleanup.service")).unwrap();
    assert!(service.contains("User=kilnr"));
    assert!(service.contains("ProtectSystem=strict"));
    assert!(!service.contains("SupplementaryGroups=docker"));
    assert!(fs::read_to_string(root.join("update.sh"))
        .unwrap()
        .contains("\"$ROOT_DIR/install.sh\" --update"));
    assert!(fs::read_to_string(root.join("uninstall.sh"))
        .unwrap()
        .contains("kilnr-cleanup.timer"));
}

#[test]
fn test_project_creation_copies_policy_without_upgrade_inheritance() {
    let repository = Path::new("/srv/git/demo.git");
    let webhook = Path::new("/etc/kilnr/secrets/demo.discord-webhook");
    for include in [false, true] {
        let mut defaults = json!({"runner":{}});
        if include {
            defaults["retention"] =
                json!({"max_age_days":30,"max_builds_per_ref":10,"keep_releases":true});
        }
        let config = ops::new_project_config("demo", repository, webhook, &defaults).unwrap();
        assert_eq!(config.get("retention").is_some(), include);
        assert_eq!(
            retention::policy(&config).unwrap(),
            if include {
                retention::Policy::DEFAULT
            } else {
                retention::Policy::DISABLED
            }
        );
    }
}
