use anyhow::{bail, Context, Result};
use std::fs::{self, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::Command;

const MANAGED: &[&str] = &[
    "job.json",
    "status.json",
    "runtime.json",
    "pipeline.mk",
    "status.lock",
];

fn safe_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("unsafe managed directory: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("unsafe managed directory: {}", path.display());
    }
    Ok(())
}

pub fn set_directory_owner(path: &Path, uid: u32, gid: u32) -> Result<()> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("cannot open managed directory: {}", path.display()))?;
    if !directory.metadata()?.is_dir() {
        bail!("managed path is not a directory: {}", path.display())
    }

    let result = unsafe { libc::fchown(directory.as_raw_fd(), uid, gid) };
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot assign managed directory: {}", path.display()));
    }

    let metadata = directory.metadata()?;
    if (metadata.uid(), metadata.gid()) != (uid, gid) {
        bail!(
            "managed directory owner mismatch after assignment: {}",
            path.display()
        )
    }
    directory.sync_all()?;
    Ok(())
}

pub fn normalize_build_metadata(builds_root: &Path) -> Result<()> {
    safe_directory(builds_root)?;
    for entry in fs::read_dir(builds_root)? {
        let build = entry?.path();
        safe_directory(&build)?;
        for name in MANAGED {
            let path = build.join(name);
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.nlink() != 1 {
                bail!("unsafe managed file: {}", path.display());
            }
            fs::set_permissions(path, fs::Permissions::from_mode(0o640))?;
        }
    }
    Ok(())
}

fn setfacl(path: &Path, acl: &str) -> Result<()> {
    let output = Command::new("setfacl")
        .args(["-m", acl])
        .arg(path)
        .output()?;
    if !output.status.success() {
        bail!(
            "cannot apply managed ACL: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

pub fn normalize_repository_refs(repository: &Path, kilnr_uid: u32) -> Result<()> {
    safe_directory(repository)?;
    let kilnr = repository.join("refs/kilnr");
    let jobs = kilnr.join("jobs");
    safe_directory(&repository.join("refs"))?;
    safe_directory(&kilnr)?;
    safe_directory(&jobs)?;
    let directory_acl = format!("u::rwx,u:{kilnr_uid}:rwx,g::r-x,m::rwx,o::---,d:u::rwx,d:u:{kilnr_uid}:rwx,d:g::r-x,d:m::rwx,d:o::---");
    for path in [&kilnr, &jobs] {
        setfacl(path, &directory_acl)?;
    }
    let file_acl = format!("u::rw-,u:{kilnr_uid}:rwx,g::r-x,m::rw-,o::---");
    for entry in fs::read_dir(&jobs)? {
        let path = entry?.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.nlink() != 1 {
            bail!("unsafe managed file: {}", path.display());
        }
        setfacl(&path, &file_acl)?;
    }
    Ok(())
}

pub fn helper(args: &[String]) -> Result<()> {
    match args {
        [flag, path] if flag == "--provision-lock-namespace" => {
            crate::project_lock::provision_production_namespace(Path::new(path))
        }
        [flag, path] if flag == "--normalize-builds" => normalize_build_metadata(Path::new(path)),
        [flag, path] if flag == "--normalize-repository" => {
            let output = Command::new("id").args(["-u", "kilnr"]).output()?;
            if !output.status.success() {
                bail!("cannot resolve kilnr user");
            }
            let uid = String::from_utf8(output.stdout)?.trim().parse()?;
            normalize_repository_refs(Path::new(path), uid)
        }
        _ => bail!("usage: permissions --provision-lock-namespace <path>|--normalize-builds <path>|--normalize-repository <path>"),
    }
}
