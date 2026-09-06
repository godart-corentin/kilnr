use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use regex::Regex;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const TRANSACTION_PREFIX: &str = ".cleanup-";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub max_age_days: Option<u64>,
    pub max_builds_per_ref: Option<usize>,
    pub keep_releases: bool,
}

impl Policy {
    pub const DEFAULT: Self = Self {
        max_age_days: Some(30),
        max_builds_per_ref: Some(10),
        keep_releases: true,
    };
    pub const DISABLED: Self = Self {
        max_age_days: None,
        max_builds_per_ref: None,
        keep_releases: true,
    };
}

pub fn policy(config: &Value) -> Result<Policy> {
    let Some(value) = config.get("retention") else {
        return Ok(Policy::DISABLED);
    };
    let object = value.as_object().context("invalid retention object")?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "max_age_days" | "max_builds_per_ref" | "keep_releases"
        )
    }) {
        bail!("invalid retention object or unknown retention field")
    }
    let positive = |key: &str| -> Result<Option<u64>> {
        match object.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(number)) => number
                .as_u64()
                .filter(|number| (1..=1_000_000).contains(number))
                .map(Some)
                .with_context(|| format!("retention.{key} must be a positive integer or null")),
            _ => bail!("retention.{key} must be a positive integer or null"),
        }
    };
    Ok(Policy {
        max_age_days: positive("max_age_days")?,
        max_builds_per_ref: positive("max_builds_per_ref")?.map(|value| value as usize),
        keep_releases: match object.get("keep_releases") {
            None => true,
            Some(Value::Bool(value)) => *value,
            _ => bail!("retention.keep_releases must be boolean"),
        },
    })
}

fn timestamp(value: Option<&Value>) -> Result<DateTime<Utc>> {
    let value = value.and_then(Value::as_str).context("missing timestamp")?;
    let parsed = DateTime::parse_from_rfc3339(value).context("invalid timestamp")?;
    Ok(parsed.with_timezone(&Utc))
}

#[derive(Debug, Clone)]
pub struct BuildRecord {
    pub id: String,
    pub job: Value,
    pub status: Value,
    pub finished: DateTime<Utc>,
}

pub fn validate_build(
    id: &str,
    job: &Value,
    status: &Value,
    project: &str,
) -> Result<Option<BuildRecord>> {
    crate::project_lock::validate_name(project)?;
    let pattern =
        Regex::new(r"^(\d{8}T\d{12}Z)-([a-z0-9][a-z0-9_-]{0,62})-([0-9a-f]{7})-([0-9a-f]{8})$")
            .unwrap();
    let captures = pattern
        .captures(id)
        .context("invalid structured build id/project")?;
    if &captures[2] != project
        || job["schema"] != 1
        || status["schema"] != 1
        || job["id"] != id
        || job["project"] != project
    {
        bail!("build metadata identity mismatch")
    }
    let oid = Regex::new(r"^(?:[0-9a-f]{40}|[0-9a-f]{64})$").unwrap();
    for key in ["old_sha", "new_sha", "sha"] {
        if !job[key].as_str().is_some_and(|value| oid.is_match(value)) {
            bail!("invalid {key}");
        }
    }
    let received = timestamp(job.get("received_at"))?;
    let id_received = chrono::NaiveDateTime::parse_from_str(&captures[1], "%Y%m%dT%H%M%S%6fZ")
        .context("invalid build id timestamp")?
        .and_utc();
    let received_skew = received.signed_duration_since(id_received);
    if received_skew < chrono::Duration::zero()
        || received_skew > chrono::Duration::seconds(1)
        || captures[3] != job["sha"].as_str().unwrap()[..7]
    {
        bail!("build id does not match timestamp/SHA")
    }
    if job["pin_ref"] != format!("refs/kilnr/jobs/{id}") {
        bail!("invalid job pin");
    }
    let kind = job["type"].as_str().context("invalid build type")?;
    let reference = job["ref"].as_str().context("invalid build ref")?;
    let prefix = match kind {
        "ci" => "refs/heads/",
        "release" => "refs/tags/",
        _ => bail!("invalid build type/ref"),
    };
    if !reference.starts_with(prefix)
        || reference.len() == prefix.len()
        || reference.contains("..")
        || reference.contains('\\')
        || reference.chars().any(|character| character.is_control())
    {
        bail!("unsafe ref");
    }
    for key in ["project", "sha", "ref", "type", "received_at"] {
        if status[key] != job[key] {
            bail!("status/job {key} mismatch");
        }
    }
    if status["build_id"] != id || status["job_id"] != id {
        bail!("status identity mismatch");
    }
    if !matches!(
        status["state"].as_str(),
        Some("success" | "failed" | "aborted")
    ) {
        return Ok(None);
    }
    let finished = timestamp(status.get("finished_at"))?;
    if finished < received {
        bail!("completion precedes receipt");
    }
    Ok(Some(BuildRecord {
        id: id.into(),
        job: job.clone(),
        status: status.clone(),
        finished,
    }))
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub record: BuildRecord,
    pub reasons: Vec<&'static str>,
    pub age_days: f64,
}

pub fn candidates(records: Vec<BuildRecord>, policy: Policy, now: DateTime<Utc>) -> Vec<Candidate> {
    let mut groups: BTreeMap<(String, String), Vec<BuildRecord>> = BTreeMap::new();
    for record in records {
        if record.job["type"] == "release" && policy.keep_releases {
            continue;
        }
        groups
            .entry((
                record.job["project"].as_str().unwrap().into(),
                record.job["ref"].as_str().unwrap().into(),
            ))
            .or_default()
            .push(record);
    }
    let mut selected = vec![];
    for group in groups.values_mut() {
        group.sort_by(|left, right| {
            right
                .finished
                .cmp(&left.finished)
                .then_with(|| right.id.cmp(&left.id))
        });
        for (index, record) in group.drain(..).enumerate() {
            let age_days = (now - record.finished).num_milliseconds() as f64 / 86_400_000.0;
            let mut reasons = vec![];
            if policy
                .max_age_days
                .is_some_and(|days| age_days > days as f64)
            {
                reasons.push("max age");
            }
            if policy
                .max_builds_per_ref
                .is_some_and(|limit| index >= limit)
            {
                reasons.push("excess builds for ref");
            }
            if !reasons.is_empty() {
                selected.push(Candidate {
                    record,
                    reasons,
                    age_days,
                });
            }
        }
    }
    selected.sort_by(|left, right| {
        left.record
            .finished
            .cmp(&right.record.finished)
            .then_with(|| left.record.id.cmp(&right.record.id))
    });
    selected
}

pub fn reject_nested_mounts(builds: &Path, mountinfo: &str) -> Result<()> {
    for line in mountinfo.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 6 {
            bail!("invalid mount table");
        }
        let mount = fields[4]
            .replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\");
        if let Ok(relative) = Path::new(&mount).strip_prefix(builds) {
            if !relative.as_os_str().is_empty() {
                bail!("refusing nested mount beneath builds: {mount}");
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Roots {
    pub state: PathBuf,
    pub config: PathBuf,
    pub git: PathBuf,
}

#[derive(Debug, Clone)]
pub struct CleanupOptions {
    pub project: Option<String>,
    pub dry_run: bool,
    pub now: DateTime<Utc>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CleanupReport {
    pub code: i32,
    pub lines: Vec<String>,
}

/// Fault and race boundaries used by callers that need deterministic testing.
/// Production cleanup uses the no-op implementation.
pub trait CleanupHooks {
    fn after_candidates(&self, _candidates: &[Candidate]) -> Result<()> {
        Ok(())
    }

    fn before_retire_rename(&self, _build: &Path, _transaction: &Path) -> Result<()> {
        Ok(())
    }

    fn before_transaction_remove(&self, _transaction: &Path) -> Result<()> {
        Ok(())
    }

    fn tree_device(&self, device: u64) -> u64 {
        device
    }
}

struct NoopHooks;
impl CleanupHooks for NoopHooks {}

fn safe_dir(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("unsafe managed directory: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("unsafe managed directory: {}", path.display());
    }
    Ok(metadata)
}

fn safe_json(path: &Path, owner: Option<u32>) -> Result<Value> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("unsafe metadata: {}", path.display()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.len() > 4 * 1024 * 1024
        || metadata.permissions().mode() & 0o022 != 0
        || owner.is_some_and(|uid| metadata.uid() != uid)
    {
        bail!("unsafe metadata ownership/mode: {}", path.display());
    }
    let value: Value = serde_json::from_slice(&fs::read(path)?)?;
    if !value.is_object() {
        bail!("metadata must be an object: {}", path.display());
    }
    Ok(value)
}

fn active_ids(state: &Path) -> Result<std::collections::BTreeSet<String>> {
    let mut active = std::collections::BTreeSet::new();
    for queue in ["incoming", "running"] {
        let root = state.join("queue").join(queue);
        safe_dir(&root)?;
        for entry in fs::read_dir(root)? {
            let path = entry?.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            if let Some(name) = path.file_stem().and_then(|name| name.to_str()) {
                active.insert(name.into());
            }
            let value = safe_json(&path, None)?;
            active.insert(
                value["id"]
                    .as_str()
                    .context("invalid active queue identity")?
                    .into(),
            );
        }
    }
    Ok(active)
}

fn tree_check(path: &Path, device: u64) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.dev() != device {
        bail!("refusing filesystem boundary");
    }
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        tree_check(&entry?.path(), device)?;
    }
    Ok(())
}

fn remove_tree(path: &Path, device: u64) -> Result<()> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.dev() != device {
        bail!("refusing filesystem boundary");
    }
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        for entry in fs::read_dir(path)? {
            remove_tree(&entry?.path(), device)?;
        }
        fs::remove_dir(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// Remove one ephemeral job workspace without following symlinks or crossing a
/// filesystem boundary. The complete tree is checked before removal so an
/// unsafe entry leaves the workspace available for diagnosis.
pub fn remove_job_workspace(work_root: &Path, job_id: &str) -> Result<()> {
    if !Regex::new(r"^[a-z0-9][a-z0-9_-]{0,62}$")
        .unwrap()
        .is_match(job_id)
    {
        bail!("invalid job workspace id");
    }
    let root = safe_dir(work_root)?;
    let workspace = work_root.join(job_id);
    match fs::symlink_metadata(&workspace) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    tree_check(&workspace, root.dev())?;
    remove_tree(&workspace, root.dev())?;
    File::open(work_root)?.sync_all()?;
    Ok(())
}

fn pin_cleanup(roots: &Roots, project: &str, record: &BuildRecord, dry_run: bool) -> Result<()> {
    let repository = roots.git.join(format!("{project}.git"));
    safe_dir(&repository)?;
    let packed = repository.join("packed-refs");
    if packed.exists() {
        let text = fs::read_to_string(&packed)?;
        let reference = format!("refs/kilnr/jobs/{}", record.id);
        if text
            .lines()
            .any(|line| line.split_whitespace().last() == Some(reference.as_str()))
        {
            bail!("packed job pin needs administrator repair: {reference}");
        }
    }
    let jobs = repository.join("refs/kilnr/jobs");
    safe_dir(&repository.join("refs"))?;
    safe_dir(&repository.join("refs/kilnr"))?;
    safe_dir(&jobs)?;
    let lock = jobs.join(format!("{}.lock", record.id));
    if lock.exists() {
        bail!("job pin is locked");
    }
    let pin = jobs.join(&record.id);
    if !pin.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(&pin)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.nlink() != 1 {
        bail!("unsafe job pin");
    }
    if fs::read_to_string(&pin)?.trim() != record.job["sha"].as_str().unwrap() {
        bail!("job pin SHA mismatch");
    }
    if !dry_run {
        crate::atomic::create_new(&lock, b"", 0o640)?;
        let result = fs::remove_file(&pin);
        let _ = fs::remove_file(&lock);
        result?;
        File::open(&jobs)?.sync_all()?;
    }
    Ok(())
}

fn finish_transaction(transaction: &Path, builds_device: u64) -> Result<()> {
    remove_tree(&transaction.join("build"), builds_device)?;
    if transaction.join("record.json").exists() {
        fs::remove_file(transaction.join("record.json"))?;
    }
    fs::remove_dir(transaction)?;
    Ok(())
}

fn retire(builds: &Path, record: &BuildRecord, hooks: &dyn CleanupHooks) -> Result<()> {
    let transaction = builds.join(format!("{TRANSACTION_PREFIX}{}", record.id));
    fs::create_dir(&transaction)?;
    crate::atomic::write_json(
        &transaction.join("record.json"),
        &serde_json::json!({"job":record.job,"status":record.status}),
        0o640,
    )?;
    hooks.before_retire_rename(&builds.join(&record.id), &transaction)?;
    fs::rename(builds.join(&record.id), transaction.join("build"))?;
    File::open(builds)?.sync_all()?;
    hooks.before_transaction_remove(&transaction)?;
    finish_transaction(&transaction, fs::metadata(builds)?.dev())?;
    File::open(builds)?.sync_all()?;
    Ok(())
}

fn read_record(build: &Path, id: &str, project: &str, owner: u32) -> Result<Option<BuildRecord>> {
    let metadata = safe_dir(build)?;
    if metadata.uid() != owner || metadata.permissions().mode() & 0o022 != 0 {
        bail!("unsafe build directory ownership/mode");
    }
    let job = safe_json(&build.join("job.json"), Some(owner))?;
    let status = safe_json(&build.join("status.json"), Some(owner))?;
    validate_build(id, &job, &status, project)
}

pub fn cleanup(roots: &Roots, options: &CleanupOptions) -> Result<CleanupReport> {
    cleanup_with_hooks(roots, options, &NoopHooks)
}

pub fn cleanup_with_hooks(
    roots: &Roots,
    options: &CleanupOptions,
    hooks: &dyn CleanupHooks,
) -> Result<CleanupReport> {
    if let Some(project) = &options.project {
        crate::project_lock::validate_name(project)?;
    }
    safe_dir(&roots.state)?;
    safe_dir(&roots.config)?;
    safe_dir(&roots.git)?;
    let builds = roots.state.join("builds");
    let builds_metadata = safe_dir(&builds)?;
    if builds_metadata.permissions().mode() & 0o022 != 0 {
        bail!("unsafe builds root ownership/mode");
    }
    #[cfg(target_os = "linux")]
    reject_nested_mounts(&builds, &fs::read_to_string("/proc/self/mountinfo")?)?;
    let controller_path = roots.state.join("locks/controller.lock");
    let controller = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(controller_path)?;
    if controller.try_lock_exclusive().is_err() {
        return Ok(CleanupReport {
            code: 0,
            lines: vec!["Deferred: controller is active".into()],
        });
    }
    let active = active_ids(&roots.state)?;
    let projects = if let Some(project) = &options.project {
        vec![project.clone()]
    } else {
        let mut values = fs::read_dir(&roots.config)?
            .filter_map(std::result::Result::ok)
            .filter_map(|entry| {
                entry
                    .path()
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>();
        values.sort();
        values
    };
    let mut report = CleanupReport::default();
    for project in projects {
        let project_result = (|| -> Result<()> {
            crate::project_lock::validate_name(&project)?;
            let _lock = match crate::project_lock::ProjectLocks::acquire(
                &roots.state.join("locks/projects"),
                std::slice::from_ref(&project),
                crate::project_lock::Mode::Exclusive,
                true,
            ) {
                Ok(lock) => lock,
                Err(error)
                    if error.to_string().contains("busy")
                        || error
                            .to_string()
                            .contains("Resource temporarily unavailable") =>
                {
                    report
                        .lines
                        .push(format!("Deferred: project {project} is busy"));
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            let config = safe_json(&roots.config.join(format!("{project}.json")), None)?;
            if config["schema"] != 1
                || config["project"] != project
                || config["repository"]
                    != roots
                        .git
                        .join(format!("{project}.git"))
                        .to_string_lossy()
                        .as_ref()
            {
                bail!("project configuration identity mismatch");
            }
            let policy = policy(&config)?;
            let mut records = vec![];
            let pattern = Regex::new(&format!(
                r"^\d{{8}}T\d{{12}}Z-{}-[0-9a-f]{{7}}-[0-9a-f]{{8}}$",
                regex::escape(&project)
            ))?;
            for entry in fs::read_dir(&builds)? {
                let path = entry?.path();
                let name = entry_name(&path)?;
                if let Some(id) = name.strip_prefix(TRANSACTION_PREFIX) {
                    if !pattern.is_match(id) {
                        continue;
                    }
                    let transaction_metadata = safe_dir(&path)?;
                    let entries = fs::read_dir(&path)?
                        .filter_map(std::result::Result::ok)
                        .map(|entry| entry.file_name())
                        .collect::<Vec<_>>();
                    if entries.is_empty() {
                        if !options.dry_run {
                            fs::remove_dir(&path)?;
                        }
                        continue;
                    }
                    if entries
                        .iter()
                        .any(|name| name != "record.json" && name != "build")
                    {
                        bail!("unexpected cleanup transaction entries");
                    }
                    if !entries.iter().any(|name| name == "build") {
                        safe_json(&path.join("record.json"), Some(transaction_metadata.uid()))?;
                        report.lines.push(format!(
                            "{} empty cleanup transaction: {id}",
                            if options.dry_run {
                                "Would finish"
                            } else {
                                "Finishing"
                            }
                        ));
                    } else {
                        safe_dir(&path.join("build"))?;
                        let record =
                            safe_json(&path.join("record.json"), Some(transaction_metadata.uid()))?;
                        let validated = validate_build(
                            id,
                            record.get("job").context("cleanup record missing job")?,
                            record
                                .get("status")
                                .context("cleanup record missing status")?,
                            &project,
                        )?
                        .context("cleanup transaction is nonterminal")?;
                        tree_check(&path, builds_metadata.dev())?;
                        report.lines.push(format!(
                            "{} interrupted cleanup: {}",
                            if options.dry_run {
                                "Would finish"
                            } else {
                                "Finishing"
                            },
                            validated.id
                        ));
                    }
                    if !options.dry_run {
                        finish_transaction(&path, builds_metadata.dev())?;
                    }
                    continue;
                }
                if !pattern.is_match(&name) || active.contains(&name) {
                    continue;
                }
                match read_record(&path, &name, &project, builds_metadata.uid())? {
                    Some(record) => records.push(record),
                    None => continue,
                }
            }
            let selected = candidates(records, policy, options.now);
            hooks.after_candidates(&selected)?;
            for candidate in selected {
                let build = builds.join(&candidate.record.id);
                if !build.exists() {
                    continue;
                }
                let status_lock_path = build.join("status.lock");
                let status_lock = if status_lock_path.exists() {
                    let file = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .custom_flags(libc::O_NOFOLLOW)
                        .open(&status_lock_path)?;
                    let metadata = file.metadata()?;
                    if !metadata.is_file()
                        || metadata.nlink() != 1
                        || metadata.uid() != builds_metadata.uid()
                        || metadata.permissions().mode() & 0o022 != 0
                    {
                        bail!("unsafe status lock");
                    }
                    if file.try_lock_exclusive().is_err() {
                        bail!("build is busy");
                    }
                    Some(file)
                } else {
                    None
                };
                let current = match read_record(
                    &build,
                    &candidate.record.id,
                    &project,
                    builds_metadata.uid(),
                ) {
                    Ok(Some(current)) => current,
                    Ok(None) => bail!("metadata changed during cleanup"),
                    Err(_error) if !build.exists() => continue,
                    Err(error) => return Err(error),
                };
                if current.job != candidate.record.job || current.status != candidate.record.status
                {
                    bail!("metadata changed during cleanup");
                }
                tree_check(&build, hooks.tree_device(builds_metadata.dev()))?;
                pin_cleanup(roots, &project, &candidate.record, options.dry_run)?;
                report.lines.push(format!(
                    "{} {} project={} ref={} age={:.2}d reason={}",
                    if options.dry_run {
                        "Would delete"
                    } else {
                        "Deleting"
                    },
                    candidate.record.id,
                    project,
                    candidate.record.job["ref"].as_str().unwrap(),
                    candidate.age_days,
                    candidate.reasons.join(", ")
                ));
                if !options.dry_run {
                    retire(&builds, &candidate.record, hooks)?;
                }
                drop(status_lock);
            }
            Ok(())
        })();
        if let Err(error) = project_result {
            report.code = 1;
            report
                .lines
                .push(format!("Refused project {project}: {error}"));
        }
    }
    Ok(report)
}

fn entry_name(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .context("invalid directory entry name")
}
