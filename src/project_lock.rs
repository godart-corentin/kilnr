use anyhow::{bail, Context, Result};
use fs2::FileExt;
use regex::Regex;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct NamespacePolicy {
    pub root_uid: u32,
    pub kilnr_uid: u32,
    pub kilnr_gid: u32,
    pub submit_gid: u32,
    pub state_traverse_group_gids: Vec<u32>,
    pub lock_traverse_user_uids: Vec<u32>,
}

pub const PRODUCTION_ROOT: &str = "/var/lib/kilnr/locks/projects";

pub fn validate_name(name: &str) -> Result<&str> {
    let valid = Regex::new(r"^[a-z0-9][a-z0-9_-]{0,62}$").unwrap();
    if !valid.is_match(name) {
        bail!("invalid project name: {name:?}");
    }
    Ok(name)
}

#[derive(Clone, Copy)]
pub enum Mode {
    Shared,
    Exclusive,
}

pub struct ProjectLocks {
    _files: Vec<File>,
    names: Vec<String>,
}

fn reject_symlink_components(path: &Path) -> Result<()> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut current = PathBuf::from("/");
    for component in absolute.components().skip(1) {
        current.push(component);
        let Ok(metadata) = fs::symlink_metadata(&current) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            // macOS exposes these stable system aliases. They are outside the
            // caller-controlled lock namespace and safe to traverse.
            if cfg!(target_os = "macos") && matches!(current.to_str(), Some("/var" | "/tmp")) {
                continue;
            }
            bail!("symlink in project lock path: {}", current.display())
        }
    }
    Ok(())
}

impl ProjectLocks {
    pub fn acquire(root: &Path, names: &[String], mode: Mode, nonblocking: bool) -> Result<Self> {
        reject_symlink_components(root)?;
        let root_metadata = fs::symlink_metadata(root)
            .with_context(|| format!("project lock root missing: {}", root.display()))?;
        if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
            bail!("unsafe project lock root: {}", root.display())
        }
        if root_metadata.permissions().mode() & 0o022 != 0 {
            bail!(
                "project lock namespace is writable by submitters: {}",
                root.display()
            )
        }
        let mut names = names.to_vec();
        names.sort();
        names.dedup();
        let mut files = Vec::with_capacity(names.len());
        for name in &names {
            validate_name(name)?;
            let path = root.join(format!("{name}.lock"));
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("project lock missing: {}", path.display()))?;
            if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.nlink() != 1 {
                bail!("unsafe project lock: {}", path.display());
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)?;
            let opened = file.metadata()?;
            if opened.dev() != metadata.dev()
                || opened.ino() != metadata.ino()
                || opened.nlink() != 1
            {
                bail!("project lock changed while opening: {}", path.display())
            }
            if (opened.uid(), opened.gid()) != (root_metadata.uid(), root_metadata.gid())
                || opened.permissions().mode() & 0o777 != 0o660
            {
                bail!(
                    "project lock entry has unexpected policy: {}",
                    path.display()
                )
            }
            let result = match (mode, nonblocking) {
                (Mode::Shared, false) => FileExt::lock_shared(&file),
                (Mode::Shared, true) => FileExt::try_lock_shared(&file),
                (Mode::Exclusive, false) => FileExt::lock_exclusive(&file),
                (Mode::Exclusive, true) => FileExt::try_lock_exclusive(&file),
            };
            result.with_context(|| format!("project lock busy: {name}"))?;
            files.push(file);
        }
        Ok(Self {
            _files: files,
            names,
        })
    }

    pub fn names(&self) -> &[String] {
        &self.names
    }
}

pub fn provision(root: &Path, names: &[String]) -> Result<()> {
    reject_symlink_components(root)?;
    fs::create_dir_all(root)?;
    let root_metadata = fs::symlink_metadata(root)?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        bail!("unsafe project lock root: {}", root.display())
    }
    if root_metadata.permissions().mode() & 0o022 != 0 {
        bail!(
            "project lock namespace is writable by submitters: {}",
            root.display()
        )
    }
    for name in names {
        validate_name(name)?;
        let path: PathBuf = root.join(format!("{name}.lock"));
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o660)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let metadata = file.metadata()?;
        if (metadata.uid(), metadata.gid()) != (root_metadata.uid(), root_metadata.gid()) {
            let result = unsafe {
                libc::fchown(
                    std::os::unix::io::AsRawFd::as_raw_fd(&file),
                    root_metadata.uid(),
                    root_metadata.gid(),
                )
            };
            if result != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        file.set_permissions(fs::Permissions::from_mode(0o660))?;
        file.sync_all()?;
    }
    File::open(root)?.sync_all()?;
    Ok(())
}

fn directory(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    if !file.metadata()?.is_dir() {
        bail!(
            "lock namespace component is not a directory: {}",
            path.display()
        );
    }
    Ok(file)
}

#[cfg(target_os = "linux")]
fn acl_bytes(mode: u32, users: &[u32], groups: &[u32]) -> Vec<u8> {
    const UNDEFINED: u32 = u32::MAX;
    let mut entries = vec![(0x01_u16, ((mode >> 6) & 7) as u16, UNDEFINED)];
    entries.extend(users.iter().map(|uid| (0x02, 0o1, *uid)));
    entries.push((0x04, ((mode >> 3) & 7) as u16, UNDEFINED));
    entries.extend(groups.iter().map(|gid| (0x08, 0o1, *gid)));
    if !users.is_empty() || !groups.is_empty() {
        entries.push((0x10, ((mode >> 3) & 7) as u16, UNDEFINED));
    }
    entries.push((0x20, (mode & 7) as u16, UNDEFINED));
    let mut bytes = 2_u32.to_le_bytes().to_vec();
    for (tag, permissions, id) in entries {
        bytes.extend(tag.to_le_bytes());
        bytes.extend(permissions.to_le_bytes());
        bytes.extend(id.to_le_bytes());
    }
    bytes
}

#[cfg(target_os = "linux")]
fn apply_acl(file: &File, mode: u32, users: &[u32], groups: &[u32]) -> Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    for name in ["system.posix_acl_default", "system.posix_acl_access"] {
        let name = CString::new(name).unwrap();
        let result = unsafe { libc::fremovexattr(file.as_raw_fd(), name.as_ptr()) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if !matches!(error.raw_os_error(), Some(libc::ENODATA)) {
                return Err(error.into());
            }
        }
    }
    if !users.is_empty() || !groups.is_empty() {
        let name = CString::new("system.posix_acl_access").unwrap();
        let value = acl_bytes(mode, users, groups);
        let result = unsafe {
            libc::fsetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn apply_acl(_file: &File, _mode: u32, _users: &[u32], _groups: &[u32]) -> Result<()> {
    Ok(())
}

fn apply_directory_policy(
    path: &Path,
    uid: u32,
    gid: u32,
    mode: u32,
    users: &[u32],
    groups: &[u32],
) -> Result<()> {
    use std::os::fd::AsRawFd;
    let file = directory(path)?;
    let metadata = file.metadata()?;
    if (metadata.uid(), metadata.gid()) != (uid, gid) {
        let result = unsafe { libc::fchown(file.as_raw_fd(), uid, gid) };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    apply_acl(&file, mode, &[], &[])?;
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    apply_acl(&file, mode, users, groups)?;
    let final_metadata = file.metadata()?;
    if (
        final_metadata.uid(),
        final_metadata.gid(),
        final_metadata.permissions().mode() & 0o7777,
    ) != (uid, gid, mode)
    {
        bail!(
            "cannot apply required lock namespace policy: {}",
            path.display()
        );
    }
    file.sync_all()?;
    Ok(())
}

pub fn provision_namespace(state: &Path, policy: &NamespacePolicy) -> Result<()> {
    reject_symlink_components(state.parent().context("state root has no parent")?)?;
    if fs::symlink_metadata(state).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!("unsafe state root: {}", state.display());
    }
    fs::create_dir_all(state)?;
    let locks = state.join("locks");
    if fs::symlink_metadata(&locks).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!("unsafe lock root: {}", locks.display());
    }
    fs::create_dir(&locks).or_else(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            Ok(())
        } else {
            Err(error)
        }
    })?;
    let projects = locks.join("projects");
    if fs::symlink_metadata(&projects).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!("unsafe project lock root: {}", projects.display());
    }
    fs::create_dir(&projects).or_else(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            Ok(())
        } else {
            Err(error)
        }
    })?;
    apply_directory_policy(
        state,
        policy.root_uid,
        policy.submit_gid,
        0o710,
        &[],
        &policy.state_traverse_group_gids,
    )?;
    apply_directory_policy(
        &locks,
        policy.root_uid,
        policy.kilnr_gid,
        0o750,
        &policy.lock_traverse_user_uids,
        &[],
    )?;
    let projects_mode = if cfg!(target_os = "macos") {
        0o750
    } else {
        0o2750
    };
    apply_directory_policy(
        &projects,
        policy.root_uid,
        policy.submit_gid,
        projects_mode,
        &[],
        &[],
    )?;
    Ok(())
}

fn numeric_identity(program: &str, args: &[&str]) -> Result<u32> {
    let output = std::process::Command::new(program).args(args).output()?;
    if !output.status.success() {
        bail!(
            "required lock namespace identity is missing: {}",
            args.last().unwrap_or(&"")
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().parse()?)
}

fn group_identity(name: &str) -> Result<u32> {
    let output = std::process::Command::new("getent")
        .args(["group", name])
        .output()?;
    if !output.status.success() {
        bail!("required lock namespace group is missing: {name}");
    }
    String::from_utf8(output.stdout)?
        .split(':')
        .nth(2)
        .context("invalid group database entry")?
        .parse()
        .map_err(Into::into)
}

pub fn provision_production_namespace(state: &Path) -> Result<()> {
    let kilnr_uid = numeric_identity("id", &["-u", "kilnr"])?;
    let kilnr_primary_gid = numeric_identity("id", &["-g", "kilnr"])?;
    let kilnr_gid = group_identity("kilnr")?;
    if kilnr_primary_gid != kilnr_gid {
        bail!("Linux user 'kilnr' does not use group 'kilnr'")
    }
    provision_namespace(
        state,
        &NamespacePolicy {
            root_uid: 0,
            kilnr_uid,
            kilnr_gid,
            submit_gid: group_identity("kilnr-submit")?,
            state_traverse_group_gids: vec![group_identity("kilnr-readers")?],
            lock_traverse_user_uids: vec![numeric_identity("id", &["-u", "git"])?],
        },
    )
}
