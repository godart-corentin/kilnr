use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

const MAX_JSON: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roots {
    pub git: PathBuf,
    pub config: PathBuf,
    pub secrets: PathBuf,
    pub state: PathBuf,
    pub locks: PathBuf,
    pub managed_hook: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathMove {
    pub source: PathBuf,
    pub destination: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFacts {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub acl: Vec<(String, Vec<u8>)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductionPolicy {
    pub root: (u32, u32),
    pub git: (u32, u32),
    pub kilnr: (u32, u32),
    pub submit_gid: u32,
    pub kilnr_groups: BTreeSet<u32>,
}

pub fn production_loose_ref_mode(roots: &Roots) -> u32 {
    if roots.git == Path::new("/srv/git")
        && roots.config == Path::new("/etc/kilnr/projects")
        && roots.secrets == Path::new("/etc/kilnr/secrets")
        && roots.state == Path::new("/var/lib/kilnr")
    {
        0o660
    } else {
        0o640
    }
}

#[cfg(target_os = "linux")]
fn read_acl(path: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    use std::ffi::CString;
    let path = CString::new(path.as_os_str().as_encoded_bytes())?;
    let size = unsafe { libc::llistxattr(path.as_ptr(), std::ptr::null_mut(), 0) };
    if size < 0 {
        let error = std::io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::ENOTSUP)) {
            return Ok(vec![]);
        }
        return Err(error.into());
    }
    let mut names = vec![0u8; size as usize];
    if size > 0
        && unsafe { libc::llistxattr(path.as_ptr(), names.as_mut_ptr().cast(), names.len()) } < 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut result = vec![];
    for raw in names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let name = String::from_utf8(raw.to_vec())?;
        if !name.to_ascii_lowercase().contains("acl") {
            continue;
        }
        let cname = CString::new(name.as_bytes())?;
        let length =
            unsafe { libc::lgetxattr(path.as_ptr(), cname.as_ptr(), std::ptr::null_mut(), 0) };
        if length < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut value = vec![0u8; length as usize];
        if length > 0
            && unsafe {
                libc::lgetxattr(
                    path.as_ptr(),
                    cname.as_ptr(),
                    value.as_mut_ptr().cast(),
                    value.len(),
                )
            } < 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        result.push((name, value));
    }
    result.sort();
    Ok(result)
}
#[cfg(not(target_os = "linux"))]
fn read_acl(_path: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    Ok(vec![])
}

pub fn decode_posix_acl(value: &[u8]) -> Result<Vec<(u16, u16, u32)>> {
    if value.len() < 4
        || (value.len() - 4) % 8 != 0
        || u32::from_le_bytes(value[..4].try_into().unwrap()) != 2
    {
        bail!("invalid ACL encoding")
    }
    let mut result = vec![];
    for chunk in value[4..].chunks_exact(8) {
        let tag = u16::from_le_bytes(chunk[..2].try_into().unwrap());
        let permissions = u16::from_le_bytes(chunk[2..4].try_into().unwrap());
        let id = u32::from_le_bytes(chunk[4..].try_into().unwrap());
        if !matches!(tag, 0x01 | 0x02 | 0x04 | 0x08 | 0x10 | 0x20) || permissions & !0o7 != 0 {
            bail!("invalid ACL entry")
        }
        result.push((tag, permissions, id));
    }
    Ok(result)
}

pub fn validate_no_extra_metadata_writers(facts: &FileFacts) -> Result<()> {
    let Some((_, value)) = facts
        .acl
        .iter()
        .find(|(name, _)| name == "system.posix_acl_access")
    else {
        if facts.mode & 0o022 != 0 {
            bail!("unexpected metadata writer permitted by mode")
        }
        return Ok(());
    };
    let entries = decode_posix_acl(value)?;
    let masks = entries
        .iter()
        .filter(|(tag, _, _)| *tag == 0x10)
        .map(|(_, p, _)| *p)
        .collect::<Vec<_>>();
    if masks.len() != 1 {
        bail!("invalid metadata access ACL mask")
    }
    for (tag, mut permissions, _) in entries {
        if matches!(tag, 0x02 | 0x04 | 0x08) {
            permissions &= masks[0]
        }
        if matches!(tag, 0x02 | 0x04 | 0x08 | 0x20) && permissions & 0o2 != 0 {
            bail!("unexpected metadata writer in access ACL")
        }
    }
    Ok(())
}

pub fn validate_ref_acl(facts: &FileFacts, uid: u32, directory: bool) -> Result<()> {
    let expected = if directory {
        vec![
            (0x01, 0o7, u32::MAX),
            (0x02, 0o7, uid),
            (0x04, 0o5, u32::MAX),
            (0x10, 0o7, u32::MAX),
            (0x20, 0, u32::MAX),
        ]
    } else {
        vec![
            (0x01, 0o6, u32::MAX),
            (0x02, 0o7, uid),
            (0x04, 0o5, u32::MAX),
            (0x10, 0o6, u32::MAX),
            (0x20, 0, u32::MAX),
        ]
    };
    let required = if directory {
        ["system.posix_acl_access", "system.posix_acl_default"].as_slice()
    } else {
        ["system.posix_acl_access"].as_slice()
    };
    if facts.acl.len() != required.len() {
        bail!("repository Kilnr ref ACL policy is invalid")
    }
    for name in required {
        let value = facts
            .acl
            .iter()
            .find(|(actual, _)| actual == name)
            .map(|(_, value)| value)
            .context("repository Kilnr ref ACL is missing")?;
        let mut entries = decode_posix_acl(value)?;
        entries.sort();
        let mut expected = expected.clone();
        expected.sort();
        if entries != expected {
            bail!("repository Kilnr ref ACL policy is invalid")
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct MetadataWrite {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub data: Vec<u8>,
    pub is_json: bool,
    pub stage_directory: PathBuf,
    pub facts: FileFacts,
}

#[derive(Debug, Clone)]
pub struct RenameInventory {
    pub roots: Roots,
    pub old: String,
    pub new: String,
    pub repository: PathMove,
    pub config_file: PathMove,
    pub webhook: PathMove,
    pub secret_directory: PathMove,
    pub cache: Option<PathMove>,
    pub builds: Vec<PathMove>,
    pub build_ids: BTreeMap<String, String>,
    pub pin_refs: BTreeMap<String, String>,
    pub metadata_writes: Vec<MetadataWrite>,
    pub source_facts: BTreeMap<PathBuf, FileFacts>,
}

#[derive(Debug, Clone)]
pub struct PreparedFile {
    pub write: MetadataWrite,
    pub temporary: PathBuf,
}

impl PreparedFile {
    pub fn temporary_after_moves(&self) -> PathBuf {
        self.write
            .destination
            .parent()
            .unwrap()
            .join(self.temporary.file_name().unwrap())
    }
}

#[derive(Debug)]
pub struct PreparedRename {
    pub inventory: RenameInventory,
    pub files: Vec<PreparedFile>,
}

impl PreparedRename {
    pub fn cleanup(&self) {
        for file in &self.files {
            for path in [&file.temporary, &file.temporary_after_moves()] {
                if fs::symlink_metadata(path).is_ok() {
                    let _ = fs::remove_file(path);
                }
            }
        }
    }
}

impl RenameInventory {
    pub fn path_moves(&self) -> Vec<&PathMove> {
        let mut moves = vec![
            &self.repository,
            &self.config_file,
            &self.webhook,
            &self.secret_directory,
        ];
        moves.extend(self.cache.iter());
        moves.extend(&self.builds);
        moves
    }
}

fn facts(path: &Path, kind: &str, mode: Option<u32>) -> Result<FileFacts> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("{kind} missing: {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("{kind} is a symlink: {}", path.display());
    }
    if let Some(expected) = mode {
        let actual = metadata.permissions().mode() & 0o7777;
        if actual != expected {
            bail!(
                "unexpected mode for {}: {actual:#06o}, expected {expected:#06o}",
                path.display()
            );
        }
    }
    Ok(FileFacts {
        mode: metadata.permissions().mode() & 0o7777,
        uid: metadata.uid(),
        gid: metadata.gid(),
        acl: read_acl(path)?,
    })
}

fn directory(path: &Path, kind: &str, mode: Option<u32>) -> Result<FileFacts> {
    let value = facts(path, kind, mode)?;
    if !fs::metadata(path)?.is_dir() {
        bail!("{kind} is not a directory: {}", path.display());
    }
    Ok(value)
}

fn regular(path: &Path, kind: &str, mode: Option<u32>) -> Result<FileFacts> {
    let value = facts(path, kind, mode)?;
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        bail!("{kind} is not a regular file: {}", path.display());
    }
    Ok(value)
}

fn read_json(path: &Path, kind: &str) -> Result<Value> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("{kind} missing: {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("{kind} is a symlink: {}", path.display());
    }
    if !metadata.is_file() || metadata.len() > MAX_JSON {
        bail!("{kind} is not a safe regular file: {}", path.display());
    }
    let value: Value = serde_json::from_slice(&fs::read(path)?)
        .with_context(|| format!("invalid JSON in {kind} {}", path.display()))?;
    if !value.is_object() {
        bail!("invalid JSON object in {kind}: {}", path.display());
    }
    Ok(value)
}

fn absent(path: &Path, kind: &str) -> Result<()> {
    if fs::symlink_metadata(path).is_ok() {
        bail!("{kind} exists: {}", path.display());
    }
    Ok(())
}

fn same_filesystem(item: &PathMove) -> Result<()> {
    require_same_filesystem_devices(
        fs::symlink_metadata(&item.source)?.dev(),
        fs::metadata(
            item.destination
                .parent()
                .context("destination has no parent")?,
        )?
        .dev(),
    )
    .with_context(|| {
        format!(
            "{} -> {}",
            item.source.display(),
            item.destination.display()
        )
    })
}

pub fn require_same_filesystem_devices(source: u64, destination_parent: u64) -> Result<()> {
    if source != destination_parent {
        bail!("source and destination must be on the same filesystem");
    }
    Ok(())
}

pub fn validate_path_policy(facts: &FileFacts, mode: u32, owner: (u32, u32)) -> Result<()> {
    if facts.mode != mode {
        bail!("unexpected mode")
    }
    if (facts.uid, facts.gid) != owner {
        bail!("unexpected ownership")
    }
    Ok(())
}

pub fn identity_can_write(facts: &FileFacts, uid: u32, gids: &BTreeSet<u32>) -> Result<bool> {
    if let Some((_, value)) = facts
        .acl
        .iter()
        .find(|(name, _)| name == "system.posix_acl_access")
    {
        let entries = decode_posix_acl(value)?;
        let mask = entries
            .iter()
            .find(|(tag, _, _)| *tag == 0x10)
            .map(|(_, p, _)| *p)
            .context("invalid access ACL mask")?;
        if uid == facts.uid {
            return Ok(entries
                .iter()
                .find(|(tag, _, _)| *tag == 0x01)
                .is_some_and(|(_, p, _)| p & 0o2 != 0));
        }
        if let Some((_, p, _)) = entries
            .iter()
            .find(|(tag, _, id)| *tag == 0x02 && *id == uid)
        {
            return Ok(p & mask & 0o2 != 0);
        }
        let group = entries
            .iter()
            .filter(|(tag, _, id)| {
                (*tag == 0x04 && gids.contains(&facts.gid)) || (*tag == 0x08 && gids.contains(id))
            })
            .fold(0, |value, (_, p, _)| value | p);
        if group != 0 {
            return Ok(group & mask & 0o2 != 0);
        }
        return Ok(entries
            .iter()
            .find(|(tag, _, _)| *tag == 0x20)
            .is_some_and(|(_, p, _)| p & 0o2 != 0));
    }
    Ok(if uid == facts.uid {
        facts.mode & 0o200 != 0
    } else if gids.contains(&facts.gid) {
        facts.mode & 0o020 != 0
    } else {
        facts.mode & 0o002 != 0
    })
}

fn validate_config(value: &Value, old: &str, repository: &Path, webhook: &Path) -> Result<()> {
    let runner = value["runner"]
        .as_object()
        .context("project config runner is invalid")?;
    let max = runner.get("max_parallel").and_then(Value::as_u64);
    let pids = runner.get("pids_limit").and_then(Value::as_u64);
    let timeout = runner.get("timeout_seconds").and_then(Value::as_u64);
    let cpu = Regex::new(r"^[0-9]+(?:\.[0-9]+)?$")?;
    let memory = Regex::new(r"^[0-9]+[kKmMgG]$")?;
    let networks = runner.get("allowed_networks").and_then(Value::as_array);
    if value["schema"] != 1
        || value["project"] != old
        || value["repository"] != repository.to_string_lossy().as_ref()
        || value["discord"]["webhook_file"] != webhook.to_string_lossy().as_ref()
        || !value["release"]["tag_pattern"].is_string()
        || !max.is_some_and(|v| (1..=32).contains(&v))
        || !runner
            .get("cpus")
            .and_then(Value::as_str)
            .is_some_and(|v| cpu.is_match(v))
        || !runner
            .get("memory")
            .and_then(Value::as_str)
            .is_some_and(|v| memory.is_match(v))
        || !pids.is_some_and(|v| (16..=65535).contains(&v))
        || !timeout.is_some_and(|v| (1..=86400).contains(&v))
        || !networks.is_some_and(|v| {
            !v.is_empty()
                && v.iter()
                    .all(|n| matches!(n.as_str(), Some("none" | "kilnr-ci")))
        })
    {
        bail!("project config validation failed");
    }
    Regex::new(value["release"]["tag_pattern"].as_str().unwrap())
        .context("project config release pattern is invalid")?;
    Ok(())
}

fn packed_refs(repository: &Path) -> Result<BTreeMap<String, String>> {
    let path = repository.join("packed-refs");
    if fs::symlink_metadata(&path).is_err() {
        return Ok(BTreeMap::new());
    }
    regular(&path, "repository packed refs", None)?;
    let text = fs::read_to_string(path).context("invalid repository packed refs")?;
    let oid = Regex::new(r"^(?:[0-9a-f]{40}|[0-9a-f]{64})$")?;
    let mut refs = BTreeMap::new();
    for line in text
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with(['#', '^']))
    {
        let (sha, reference) = line
            .split_once(' ')
            .context("invalid repository packed ref")?;
        if !oid.is_match(sha)
            || !reference.starts_with("refs/")
            || refs.insert(reference.into(), sha.into()).is_some()
        {
            bail!("invalid or duplicate repository packed ref");
        }
    }
    Ok(refs)
}

fn loose_refs(
    repository: &Path,
    facts_map: &mut BTreeMap<PathBuf, FileFacts>,
) -> Result<BTreeMap<String, String>> {
    directory(&repository.join("refs"), "repository refs", None)?;
    let namespace = repository.join("refs/kilnr");
    if fs::symlink_metadata(&namespace).is_err() {
        return Ok(BTreeMap::new());
    }
    facts_map.insert(
        namespace.clone(),
        directory(&namespace, "repository Kilnr refs", Some(0o770))?,
    );
    let jobs = namespace.join("jobs");
    if fs::symlink_metadata(&jobs).is_err() {
        return Ok(BTreeMap::new());
    }
    facts_map.insert(
        jobs.clone(),
        directory(&jobs, "repository Kilnr job refs", Some(0o770))?,
    );
    let oid = Regex::new(r"^(?:[0-9a-f]{40}|[0-9a-f]{64})$")?;
    let mut refs = BTreeMap::new();
    for entry in fs::read_dir(&jobs)? {
        let path = entry?.path();
        let entry_facts = regular(&path, "managed pin ref", None)?;
        if entry_facts.mode & 0o022 != 0 && entry_facts.mode != 0o660 {
            bail!(
                "unexpected mode for managed loose pin ref {}",
                path.display()
            );
        }
        let target = fs::read_to_string(&path)
            .context("invalid managed pin ref target")?
            .trim()
            .to_owned();
        if !oid.is_match(&target) {
            bail!("invalid managed pin ref target: {}", path.display());
        }
        facts_map.insert(path.clone(), entry_facts);
        refs.insert(
            format!(
                "refs/kilnr/jobs/{}",
                path.file_name().unwrap().to_string_lossy()
            ),
            target,
        );
    }
    Ok(refs)
}

fn validate_queues(roots: &Roots, old: &str) -> Result<()> {
    for name in ["incoming", "running"] {
        let queue = roots.state.join("queue").join(name);
        directory(&queue, &format!("{name} queue"), None)?;
        for entry in fs::read_dir(queue)? {
            let path = entry?.path();
            regular(&path, "queue entry", None)?;
            if path.extension().and_then(|v| v.to_str()) != Some("json") {
                bail!("unexpected queue entry: {}", path.display());
            }
            let job = read_json(&path, &format!("{name} queue job"))?;
            if job["schema"] != 1
                || !job["id"].is_string()
                || path.file_name().and_then(|name| name.to_str())
                    != Some(format!("{}.json", job["id"].as_str().unwrap()).as_str())
            {
                bail!("invalid {name} queue job");
            }
            crate::project_lock::validate_name(
                job["project"]
                    .as_str()
                    .context("invalid project in queue job")?,
            )?;
            if job["project"] == old {
                bail!("active {name} job for source project: {}", path.display());
            }
        }
    }
    Ok(())
}

fn map_build(job: &Value, path: &Path, old: &str, new: &str) -> Result<String> {
    let id = job["id"].as_str().context("invalid build id")?;
    if path.file_name().and_then(|v| v.to_str()) != Some(id) {
        bail!("invalid build id/dirname mismatch");
    }
    let sha = job["sha"].as_str().context("invalid build sha")?;
    if !Regex::new(r"^(?:[0-9a-f]{40}|[0-9a-f]{64})$")?.is_match(sha) {
        bail!("invalid build sha");
    }
    let received: DateTime<Utc> = DateTime::parse_from_rfc3339(
        job["received_at"]
            .as_str()
            .context("invalid build received_at")?,
    )?
    .with_timezone(&Utc);
    let prefix = format!(
        "{}-{old}-{}-",
        received.format("%Y%m%dT%H%M%S%6fZ"),
        &sha[..7]
    );
    let suffix = id
        .strip_prefix(&prefix)
        .context("invalid build id for structured metadata")?;
    if !Regex::new(r"^[0-9a-f]{8}$")?.is_match(suffix) {
        bail!("invalid build id random suffix");
    }
    Ok(format!(
        "{}-{new}-{}-{suffix}",
        received.format("%Y%m%dT%H%M%S%6fZ"),
        &sha[..7]
    ))
}

fn json_bytes(value: &Value) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn rewrite_paths(value: &mut Value, old: &str, new: &str) {
    match value {
        Value::Array(items) => {
            for item in items {
                rewrite_paths(item, old, new);
            }
        }
        Value::Object(items) => {
            for (key, item) in items {
                if matches!(
                    key.as_str(),
                    "repository"
                        | "repository_path"
                        | "webhook_file"
                        | "secret_path"
                        | "secrets_path"
                        | "cache_path"
                        | "build_path"
                        | "build_dir"
                ) {
                    if let Some(text) = item.as_str() {
                        *item = Value::String(
                            text.replace(&format!("/{old}.git"), &format!("/{new}.git"))
                                .replace(&format!("/{old}.json"), &format!("/{new}.json"))
                                .replace(
                                    &format!("/{old}.discord-webhook"),
                                    &format!("/{new}.discord-webhook"),
                                )
                                .replace(&format!("/{old}/"), &format!("/{new}/")),
                        );
                    }
                } else {
                    rewrite_paths(item, old, new);
                }
            }
        }
        _ => {}
    }
}

pub fn inventory_rename(roots: &Roots, old: &str, new: &str) -> Result<RenameInventory> {
    crate::project_lock::validate_name(old)?;
    crate::project_lock::validate_name(new)?;
    if old == new {
        bail!("old and new project names must be different");
    }
    directory(&roots.git, "Git root", None)?;
    let config_root_facts = directory(&roots.config, "project config root", None)?;
    let secret_root_facts = directory(&roots.secrets, "secret root", None)?;
    directory(&roots.state, "state root", None)?;
    directory(&roots.locks, "project lock root", None)?;
    directory(&roots.state.join("queue"), "queue root", None)?;
    let build_root_facts = directory(&roots.state.join("builds"), "build root", None)?;
    let cache_root_facts = directory(&roots.state.join("cache"), "cache root", None)?;
    validate_queues(roots, old)?;
    let repository = PathMove {
        source: roots.git.join(format!("{old}.git")),
        destination: roots.git.join(format!("{new}.git")),
    };
    let config_file = PathMove {
        source: roots.config.join(format!("{old}.json")),
        destination: roots.config.join(format!("{new}.json")),
    };
    let webhook = PathMove {
        source: roots.secrets.join(format!("{old}.discord-webhook")),
        destination: roots.secrets.join(format!("{new}.discord-webhook")),
    };
    let secret_directory = PathMove {
        source: roots.secrets.join(old),
        destination: roots.secrets.join(new),
    };
    let cache_source = roots.state.join("cache").join(old);
    let cache = fs::symlink_metadata(&cache_source)
        .is_ok()
        .then(|| PathMove {
            source: cache_source,
            destination: roots.state.join("cache").join(new),
        });
    for item in [&repository, &config_file, &webhook, &secret_directory] {
        absent(&item.destination, "destination")?;
        same_filesystem(item)?;
    }
    if let Some(item) = &cache {
        absent(&item.destination, "destination")?;
        same_filesystem(item)?;
    }
    let mut source_facts = BTreeMap::new();
    let repository_facts = directory(&repository.source, "source repository", Some(0o750))?;
    let repository_owner = (repository_facts.uid, repository_facts.gid);
    source_facts.insert(repository.source.clone(), repository_facts);
    for child in ["HEAD", "config"] {
        let path = repository.source.join(child);
        let child_facts = regular(&path, &format!("repository {child}"), None)?;
        if (child_facts.uid, child_facts.gid) != repository_owner || child_facts.mode & 0o022 != 0 {
            bail!("repository {child} has inconsistent ownership or writable permissions");
        }
        source_facts.insert(path, child_facts);
    }
    for child in ["objects", "refs", "hooks"] {
        let path = repository.source.join(child);
        let child_facts = directory(&path, &format!("repository {child}"), None)?;
        if (child_facts.uid, child_facts.gid) != repository_owner || child_facts.mode & 0o022 != 0 {
            bail!("repository {child} has inconsistent ownership or writable permissions");
        }
        source_facts.insert(path, child_facts);
    }
    let git_config = fs::read_to_string(repository.source.join("config"))?;
    if !git_config.contains("bare = true")
        || !git_config
            .to_ascii_lowercase()
            .contains("hiderefs = refs/kilnr/")
        || !git_config.to_ascii_lowercase().contains("packrefs = false")
    {
        bail!("repository configuration is invalid");
    }
    let hook = repository.source.join("hooks/post-receive");
    let expected_hook = roots
        .managed_hook
        .as_ref()
        .context("managed post-receive hook missing")?;
    if !fs::symlink_metadata(&hook)?.file_type().is_symlink()
        || hook.canonicalize()? != expected_hook.canonicalize()?
    {
        bail!("repository post-receive hook is invalid");
    }
    let hook_meta = regular(expected_hook, "managed post-receive hook", None)?;
    if hook_meta.mode & 0o111 == 0 || hook_meta.mode & 0o022 != 0 {
        bail!("repository managed post-receive hook permissions are invalid");
    }
    let config_facts = regular(&config_file.source, "source project config", Some(0o644))?;
    validate_path_policy(
        &config_facts,
        0o644,
        (config_root_facts.uid, config_root_facts.gid),
    )?;
    source_facts.insert(config_file.source.clone(), config_facts);
    let webhook_facts = regular(&webhook.source, "source webhook", Some(0o640))?;
    validate_path_policy(
        &webhook_facts,
        0o640,
        (secret_root_facts.uid, secret_root_facts.gid),
    )?;
    source_facts.insert(webhook.source.clone(), webhook_facts);
    let secret_directory_facts = directory(
        &secret_directory.source,
        "source secret directory",
        Some(0o750),
    )?;
    validate_path_policy(
        &secret_directory_facts,
        0o750,
        (secret_root_facts.uid, secret_root_facts.gid),
    )?;
    source_facts.insert(secret_directory.source.clone(), secret_directory_facts);
    if let Some(item) = &cache {
        let cache_facts = directory(&item.source, "source cache", Some(0o750))?;
        validate_path_policy(
            &cache_facts,
            0o750,
            (cache_root_facts.uid, cache_root_facts.gid),
        )?;
        source_facts.insert(item.source.clone(), cache_facts);
    }
    let config = read_json(&config_file.source, "project config")?;
    validate_config(&config, old, &repository.source, &webhook.source)?;
    let mut config_after = config.clone();
    rewrite_paths(&mut config_after, old, new);
    config_after["project"] = Value::String(new.into());
    let mut metadata_writes = vec![MetadataWrite {
        source: config_file.source.clone(),
        destination: config_file.destination.clone(),
        data: json_bytes(&config_after)?,
        is_json: true,
        stage_directory: roots.config.clone(),
        facts: source_facts[&config_file.source].clone(),
    }];
    let secret_name = Regex::new(r"^[A-Z_][A-Z0-9_]*$")?;
    let mut secret_parts = BTreeSet::new();
    for entry in fs::read_dir(&secret_directory.source)? {
        let path = entry?.path();
        let f = regular(&path, "secret entry", Some(0o640))?;
        validate_path_policy(&f, 0o640, (secret_root_facts.uid, secret_root_facts.gid))?;
        let extension = path
            .extension()
            .and_then(|v| v.to_str())
            .context("unexpected secret entry")?;
        let name = path
            .file_stem()
            .and_then(|v| v.to_str())
            .context("invalid secret name")?;
        if !secret_name.is_match(name)
            || name.starts_with("KILNR_")
            || !matches!(extension, "json" | "value")
        {
            bail!(
                "invalid secret name or unexpected secret entry: {}",
                path.display()
            )
        }
        if extension == "json" {
            let meta = read_json(&path, "secret metadata")?;
            if meta["schema"] != 1
                || meta["scope"] != "release"
                || !matches!(meta["kind"].as_str(), Some("text" | "file"))
            {
                bail!("invalid secret metadata")
            }
        }
        secret_parts.insert((name.to_owned(), extension.to_owned()));
        source_facts.insert(path, f);
    }
    let names = secret_parts
        .iter()
        .map(|v| v.0.clone())
        .collect::<BTreeSet<_>>();
    if names.iter().any(|name| {
        !secret_parts.contains(&(name.clone(), "json".into()))
            || !secret_parts.contains(&(name.clone(), "value".into()))
    }) {
        bail!("secret value/metadata mismatch")
    }
    let loose = loose_refs(&repository.source, &mut source_facts)?;
    let packed = packed_refs(&repository.source)?;
    let builds_root = roots.state.join("builds");
    let mut builds = vec![];
    let mut build_ids = BTreeMap::new();
    let mut expected_refs = BTreeMap::new();
    for entry in fs::read_dir(&builds_root)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .context("invalid build root entry name")?;
        if name.starts_with(".cleanup-") {
            bail!("unfinished build cleanup; run kilnr cleanup before renaming projects");
        }
        let build_facts = directory(&path, "completed build directory", Some(0o750))?;
        validate_path_policy(
            &build_facts,
            0o750,
            (build_root_facts.uid, build_root_facts.gid),
        )?;
        source_facts.insert(path.clone(), build_facts);
        let job_path = path.join("job.json");
        let job = read_json(&job_path, "build job")?;
        let project = job["project"]
            .as_str()
            .context("invalid project in build job metadata")?;
        crate::project_lock::validate_name(project)?;
        if job["project"] != old {
            continue;
        }
        if job["schema"] != 1 || !matches!(job["type"].as_str(), Some("ci" | "release")) {
            bail!("invalid source build job metadata")
        };
        let new_id = map_build(&job, &path, old, new)?;
        let old_id = job["id"].as_str().unwrap().to_owned();
        let destination = builds_root.join(&new_id);
        absent(&destination, "destination")?;
        let item = PathMove {
            source: path.clone(),
            destination,
        };
        same_filesystem(&item)?;
        let actual = fs::read_dir(&path)?
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect::<std::io::Result<BTreeSet<_>>>()?;
        let required = [
            "job.json",
            "status.json",
            "src",
            "work",
            "logs",
            "artifacts",
            "commands",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
        let optional = ["runtime.json", "pipeline.mk", "runtime", "status.lock"]
            .into_iter()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        if !required.is_subset(&actual) {
            bail!("managed build entries missing: {}", path.display());
        }
        if actual
            .difference(&required)
            .any(|name| !optional.contains(name))
        {
            bail!("unexpected managed build entries: {}", path.display());
        }
        for dir in ["src", "work", "logs", "artifacts", "commands"] {
            let child = path.join(dir);
            let child_facts = directory(&child, "build directory", Some(0o750))?;
            validate_path_policy(
                &child_facts,
                0o750,
                (build_root_facts.uid, build_root_facts.gid),
            )?;
            source_facts.insert(child, child_facts);
        }
        if actual.contains("runtime") {
            let child = path.join("runtime");
            let child_facts = directory(&child, "build runtime directory", Some(0o750))?;
            validate_path_policy(
                &child_facts,
                0o750,
                (build_root_facts.uid, build_root_facts.gid),
            )?;
            source_facts.insert(child, child_facts);
        }
        if actual.contains("status.lock") {
            let child = path.join("status.lock");
            let child_facts = regular(&child, "build status lock", Some(0o640))?;
            validate_path_policy(
                &child_facts,
                0o640,
                (build_root_facts.uid, build_root_facts.gid),
            )?;
            source_facts.insert(child, child_facts);
        }
        for filename in ["job.json", "status.json", "runtime.json"] {
            let source = path.join(filename);
            if fs::symlink_metadata(&source).is_err() {
                if filename == "runtime.json" {
                    continue;
                } else {
                    bail!("build metadata missing")
                }
            }
            let f = regular(&source, "build metadata", Some(0o640))?;
            validate_path_policy(&f, 0o640, (build_root_facts.uid, build_root_facts.gid))?;
            let mut value = read_json(&source, "build metadata")?;
            if value["project"] != old {
                bail!("inconsistent build metadata")
            };
            if filename == "status.json"
                && (value["schema"] != 1
                    || value["build_id"] != old_id
                    || value["job_id"] != old_id
                    || value["sha"] != job["sha"]
                    || value["ref"] != job["ref"]
                    || value["type"] != job["type"]
                    || value["received_at"] != job["received_at"]
                    || !matches!(
                        value["state"].as_str(),
                        Some("success" | "failed" | "aborted")
                    )
                    || !value["prepare"].is_object()
                    || !(value["pipeline"].is_null() || value["pipeline"].is_object()))
            {
                bail!("inconsistent build status metadata")
            };
            if filename == "runtime.json"
                && (value["schema"] != 1
                    || value["build_id"] != old_id
                    || value["sha"] != job["sha"]
                    || value["job_type"] != job["type"]
                    || value["ref"] != job["ref"]
                    || !value["pipeline"].is_string()
                    || !value["runner"].is_object()
                    || !value["groups"].is_object()
                    || !value["jobs"].is_object())
            {
                bail!("inconsistent build runtime metadata")
            };
            rewrite_paths(&mut value, old, new);
            value["project"] = Value::String(new.into());
            match filename {
                "job.json" => {
                    value["id"] = Value::String(new_id.clone());
                    value["pin_ref"] = Value::String(format!("refs/kilnr/jobs/{new_id}"));
                }
                "runtime.json" => value["build_id"] = Value::String(new_id.clone()),
                "status.json" => {
                    value["build_id"] = Value::String(new_id.clone());
                    value["job_id"] = Value::String(new_id.clone());
                }
                _ => unreachable!(),
            }
            metadata_writes.push(MetadataWrite {
                source: source.clone(),
                destination: item.destination.join(filename),
                data: json_bytes(&value)?,
                is_json: true,
                stage_directory: path.clone(),
                facts: f.clone(),
            });
            source_facts.insert(source, f);
        }
        let make = path.join("pipeline.mk");
        if fs::symlink_metadata(&make).is_ok() {
            let f = regular(&make, "build pipeline", Some(0o640))?;
            validate_path_policy(&f, 0o640, (build_root_facts.uid, build_root_facts.gid))?;
            let before = fs::read_to_string(&make)?;
            if !before.contains(&format!("/execute {old_id} ")) {
                bail!("pipeline entries have unexpected build id")
            };
            metadata_writes.push(MetadataWrite {
                source: make.clone(),
                destination: item.destination.join("pipeline.mk"),
                data: before
                    .replace(
                        &format!("/execute {old_id} "),
                        &format!("/execute {new_id} "),
                    )
                    .into_bytes(),
                is_json: false,
                stage_directory: path.clone(),
                facts: f.clone(),
            });
            source_facts.insert(make, f);
        }
        let old_ref = format!("refs/kilnr/jobs/{old_id}");
        if job["pin_ref"] != old_ref {
            bail!("invalid source build pin ref: {}", path.display());
        }
        expected_refs.insert(old_ref.clone(), job["sha"].as_str().unwrap().to_owned());
        build_ids.insert(old_id, new_id);
        builds.push(item);
    }
    let mut combined = packed.clone();
    for (reference, target) in &loose {
        if combined
            .insert(reference.clone(), target.clone())
            .is_some_and(|prior| prior != *target)
        {
            bail!("ambiguous managed pin ref: {reference}")
        }
    }
    let mut pin_refs = BTreeMap::new();
    let destination_refs = build_ids
        .values()
        .map(|id| format!("refs/kilnr/jobs/{id}"))
        .collect::<BTreeSet<_>>();
    for reference in combined
        .keys()
        .filter(|reference| reference.starts_with("refs/kilnr/jobs/"))
    {
        if destination_refs.contains(reference) {
            bail!("destination pin ref exists: {reference}")
        }
        let target = expected_refs
            .get(reference)
            .with_context(|| format!("unmatched managed pin ref: {reference}"))?;
        if combined.get(reference) != Some(target) {
            bail!("pin ref target mismatch: {reference}")
        }
        let old_ref = reference;
        let old_id = old_ref.trim_start_matches("refs/kilnr/jobs/");
        let new_ref = format!("refs/kilnr/jobs/{}", build_ids[old_id]);
        pin_refs.insert(old_ref.clone(), new_ref);
    }
    Ok(RenameInventory {
        roots: roots.clone(),
        old: old.into(),
        new: new.into(),
        repository,
        config_file,
        webhook,
        secret_directory,
        cache,
        builds,
        build_ids,
        pin_refs,
        metadata_writes,
        source_facts,
    })
}

fn verify_facts(path: &Path, expected: &FileFacts) -> Result<()> {
    let current = facts(path, "managed source", None)?;
    if &current != expected {
        bail!(
            "managed source permissions changed after inventory: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn copy_acl(file: &fs::File, acl: &[(String, Vec<u8>)]) -> Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    for (name, value) in acl {
        let name = CString::new(name.as_bytes())?;
        if unsafe {
            libc::fsetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}
#[cfg(not(target_os = "linux"))]
fn copy_acl(_file: &fs::File, _acl: &[(String, Vec<u8>)]) -> Result<()> {
    Ok(())
}

pub fn prepare_rename(inventory: RenameInventory) -> Result<PreparedRename> {
    let mut files = Vec::new();
    let result = (|| -> Result<()> {
        for write in &inventory.metadata_writes {
            verify_facts(&write.source, &write.facts)?;
            directory(&write.stage_directory, "metadata staging directory", None)?;
            let mut temporary = tempfile::Builder::new()
                .prefix(".")
                .suffix(".rename-tmp")
                .tempfile_in(&write.stage_directory)?;
            temporary.as_file_mut().write_all(&write.data)?;
            temporary.as_file_mut().flush()?;
            temporary.as_file().sync_all()?;
            let metadata = temporary.as_file().metadata()?;
            if (metadata.uid(), metadata.gid()) != (write.facts.uid, write.facts.gid) {
                use std::os::fd::AsRawFd;
                let result = unsafe {
                    libc::fchown(
                        temporary.as_file().as_raw_fd(),
                        write.facts.uid,
                        write.facts.gid,
                    )
                };
                if result != 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
            temporary
                .as_file()
                .set_permissions(fs::Permissions::from_mode(write.facts.mode))?;
            copy_acl(temporary.as_file(), &write.facts.acl)?;
            let path = temporary.into_temp_path().keep()?;
            files.push(PreparedFile {
                write: write.clone(),
                temporary: path,
            });
        }
        Ok(())
    })();
    if let Err(error) = result {
        PreparedRename {
            inventory: inventory.clone(),
            files,
        }
        .cleanup();
        return Err(error);
    }
    Ok(PreparedRename { inventory, files })
}

fn hardened_git(repository: &Path, input: &str) -> Result<()> {
    use std::process::{Command, Stdio};
    let mut child = Command::new("git")
        .arg(format!("--git-dir={}", repository.display()))
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "include.path=/dev/null",
            "-c",
            &format!("safe.directory={}", repository.display()),
            "update-ref",
            "--stdin",
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(input.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "git reference transaction failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    fs::File::open(repository)?.sync_all()?;
    Ok(())
}

pub fn verify_rename(prepared: &PreparedRename) -> Result<()> {
    let inventory = &prepared.inventory;
    for item in inventory.path_moves() {
        if fs::symlink_metadata(&item.source).is_ok()
            || fs::symlink_metadata(&item.destination).is_err()
        {
            bail!(
                "rename verification failed: {} -> {}",
                item.source.display(),
                item.destination.display()
            )
        }
    }
    for write in &inventory.metadata_writes {
        if fs::read(&write.destination)? != write.data {
            bail!("renamed metadata mismatch: {}", write.destination.display())
        }
    }
    let repository = &inventory.repository.destination;
    for (old, new) in &inventory.pin_refs {
        let old_value = std::process::Command::new("git")
            .arg(format!("--git-dir={}", repository.display()))
            .args(["show-ref", "--verify", "--hash", old])
            .output()?;
        if old_value.status.success() {
            bail!("old managed pin remains: {old}")
        }
        let new_value = std::process::Command::new("git")
            .arg(format!("--git-dir={}", repository.display()))
            .args(["show-ref", "--verify", "--hash", new])
            .output()?;
        if !new_value.status.success() {
            bail!("new managed pin missing: {new}")
        }
    }
    Ok(())
}

pub fn commit_rename(prepared: &PreparedRename) -> Result<()> {
    commit_rename_with(prepared, |_| Ok(()))
}

fn final_rollback_path(path: &Path, inventory: &RenameInventory) -> PathBuf {
    for item in inventory.path_moves() {
        if let Ok(relative) = path.strip_prefix(&item.destination) {
            return item.source.join(relative);
        }
    }
    path.to_owned()
}

pub fn commit_rename_with<F>(prepared: &PreparedRename, mut fault: F) -> Result<()>
where
    F: FnMut(&str) -> Result<()>,
{
    let inventory = &prepared.inventory;
    let mut moved: Vec<&PathMove> = Vec::new();
    let mut backups: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut installed: Vec<PathBuf> = Vec::new();
    let repository = &inventory.repository.destination;
    let original_repository = &inventory.repository.source;
    let packed_path = original_repository.join("packed-refs");
    let packed_snapshot = fs::symlink_metadata(&packed_path).ok().map(|metadata| {
        (
            fs::read(&packed_path).unwrap(),
            metadata.permissions().mode() & 0o7777,
        )
    });
    let namespace_existed = original_repository.join("refs/kilnr").is_dir();
    let jobs_existed = original_repository.join("refs/kilnr/jobs").is_dir();
    let loose_snapshots = inventory
        .pin_refs
        .keys()
        .map(|reference| {
            let path = original_repository.join(reference);
            let snapshot = fs::symlink_metadata(&path).ok().map(|metadata| {
                (
                    fs::read(&path).unwrap(),
                    metadata.permissions().mode() & 0o7777,
                )
            });
            (reference.clone(), snapshot)
        })
        .collect::<Vec<_>>();
    let mut refs_done = false;
    let result = (|| -> Result<()> {
        for item in inventory.path_moves() {
            verify_facts(
                &item.source,
                inventory
                    .source_facts
                    .get(&item.source)
                    .context("move source facts missing")?,
            )?;
            fs::rename(&item.source, &item.destination)?;
            moved.push(item);
            let phase = if item.source == inventory.repository.source {
                "repository-move"
            } else if item.source == inventory.config_file.source {
                "config-move"
            } else if item.source == inventory.webhook.source {
                "webhook-move"
            } else if item.source == inventory.secret_directory.source {
                "secret-directory-move"
            } else if inventory
                .cache
                .as_ref()
                .is_some_and(|cache| item.source == cache.source)
            {
                "cache-move"
            } else {
                "build-move"
            };
            fault(phase)?;
        }
        for file in &prepared.files {
            let destination = &file.write.destination;
            let backup = destination.with_file_name(format!(
                ".{}.rename-backup",
                destination.file_name().unwrap().to_string_lossy()
            ));
            fs::rename(destination, &backup)?;
            backups.push((destination.clone(), backup));
            fault("metadata-backup")?;
            fs::rename(file.temporary_after_moves(), destination)?;
            installed.push(destination.clone());
            fault("metadata-install")?;
        }
        let mut commands = String::from("start\n");
        for (old, new) in &inventory.pin_refs {
            let sha = inventory
                .builds
                .iter()
                .find_map(|item| {
                    let id = item.source.file_name()?.to_str()?;
                    if old.ends_with(id) {
                        inventory
                            .metadata_writes
                            .iter()
                            .find(|w| w.source == item.source.join("job.json"))
                            .and_then(|w| serde_json::from_slice::<Value>(&w.data).ok())
                            .and_then(|v| v["sha"].as_str().map(str::to_owned))
                    } else {
                        None
                    }
                })
                .context("pin SHA missing")?;
            commands += &format!("create {new} {sha}\ndelete {old} {sha}\n");
        }
        commands += "commit\n";
        if !inventory.pin_refs.is_empty() {
            hardened_git(repository, &commands)?;
            refs_done = true;
            fault("ref-repository-fsync")?;
            for (old, new) in &inventory.pin_refs {
                fault("ref-security")?;
                let original = inventory.repository.source.join(old);
                let selected_mode = inventory.source_facts.get(&original).map_or_else(
                    || production_loose_ref_mode(&inventory.roots),
                    |facts| facts.mode,
                );
                fs::set_permissions(
                    repository.join(new),
                    fs::Permissions::from_mode(selected_mode),
                )?;
            }
        }
        fault("pin-refs")?;
        verify_rename(prepared)?;
        fault("verify")?;
        for (_, backup) in &backups {
            fs::remove_file(backup)?;
        }
        prepared.cleanup();
        Ok(())
    })();
    if let Err(primary) = result {
        let mut rollback = Vec::new();
        if refs_done {
            if let Err(error) = fault("ref-rollback") {
                rollback.push(format!(
                    "pin-refs inverse failed at {}: {error}",
                    inventory.repository.source.display()
                ));
            } else {
                for (old, new) in &inventory.pin_refs {
                    for reference in [old, new] {
                        let path = repository.join(reference);
                        if fs::symlink_metadata(&path).is_ok() {
                            if let Err(error) = fs::remove_file(&path) {
                                rollback.push(format!(
                                    "pin ref inverse failed at {}: {error}",
                                    path.display()
                                ))
                            }
                        }
                    }
                }
                for (reference, snapshot) in &loose_snapshots {
                    if let Some((bytes, mode)) = snapshot {
                        if let Some(parent) = repository.join(reference).parent() {
                            let _ = fs::create_dir_all(parent);
                        }
                        if let Err(error) =
                            crate::atomic::write(&repository.join(reference), bytes, *mode)
                        {
                            rollback.push(format!(
                                "pin ref inverse failed at {}: {error}",
                                repository.join(reference).display()
                            ))
                        }
                    }
                }
                let current_packed = repository.join("packed-refs");
                match &packed_snapshot {
                    Some((bytes, mode)) => {
                        if let Err(error) = crate::atomic::write(&current_packed, bytes, *mode) {
                            rollback.push(format!(
                                "packed ref inverse failed at {}: {error}",
                                current_packed.display()
                            ))
                        }
                    }
                    None => {
                        if fs::symlink_metadata(&current_packed).is_ok() {
                            if let Err(error) = fs::remove_file(&current_packed) {
                                rollback.push(format!(
                                    "packed ref inverse failed at {}: {error}",
                                    current_packed.display()
                                ))
                            }
                        }
                    }
                }
                if !jobs_existed {
                    let _ = fs::remove_dir(repository.join("refs/kilnr/jobs"));
                }
                if !namespace_existed {
                    let _ = fs::remove_dir(repository.join("refs/kilnr"));
                }
                if let Err(error) = fs::File::open(repository).and_then(|file| file.sync_all()) {
                    rollback.push(format!(
                        "repository inverse fsync failed at {}: {error}",
                        repository.display()
                    ));
                }
                if let Err(error) = fault("ref-rollback-fsync") {
                    rollback.push(format!(
                        "pin-refs inverse failed at {}: {error}",
                        inventory.repository.source.display()
                    ));
                }
            }
        }
        for destination in installed.iter().rev() {
            if let Err(error) = fs::remove_file(destination) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    rollback.push(error.to_string())
                }
            }
        }
        for (destination, backup) in backups.iter().rev() {
            if fs::symlink_metadata(backup).is_ok() {
                if let Err(error) = fault("metadata-rollback") {
                    rollback.push(format!(
                        "metadata-backup inverse failed at {}: {error}",
                        final_rollback_path(backup, inventory).display()
                    ));
                } else if let Err(error) = fs::rename(backup, destination) {
                    rollback.push(format!(
                        "metadata-backup inverse failed at {}: {error}",
                        final_rollback_path(backup, inventory).display()
                    ))
                }
            }
        }
        for item in moved.iter().rev() {
            if let Err(error) = fs::rename(&item.destination, &item.source) {
                rollback.push(error.to_string())
            }
        }
        prepared.cleanup();
        if rollback.is_empty() {
            return Err(primary);
        }
        bail!(
            "rename failed: {primary}; rollback failures: {}",
            rollback.join("; ")
        )
    }
    Ok(())
}
