use anyhow::{Context, Result};
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

fn write_inner(
    path: &Path,
    bytes: &[u8],
    mode: u32,
    inherit_parent_group: bool,
    owner: Option<(u32, u32)>,
) -> Result<()> {
    let parent = path.parent().context("destination has no parent")?;
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!(
            ".{}.",
            path.file_name().unwrap_or_default().to_string_lossy()
        ))
        .tempfile_in(parent)
        .with_context(|| format!("create temporary file in {}", parent.display()))?;
    let current = temporary.as_file().metadata()?;
    let desired = if let Some(owner) = owner {
        Some(owner)
    } else if inherit_parent_group {
        Some((current.uid(), fs::metadata(parent)?.gid()))
    } else {
        None
    };
    if let Some((uid, gid)) = desired {
        if (current.uid(), current.gid()) != (uid, gid) {
            let rc = unsafe { libc::fchown(temporary.as_file().as_raw_fd(), uid, gid) };
            if rc != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("set ownership for {}", path.display()));
            }
        }
    }
    temporary
        .as_file_mut()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    temporary.write_all(bytes)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

pub fn write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    write_inner(path, bytes, mode, false, None)
}

pub fn write_with_parent_group(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    write_inner(path, bytes, mode, true, None)
}

pub fn write_owned(path: &Path, bytes: &[u8], mode: u32, uid: u32, gid: u32) -> Result<()> {
    write_inner(path, bytes, mode, false, Some((uid, gid)))
}

pub fn write_json<T: Serialize>(path: &Path, value: &T, mode: u32) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    write(path, &bytes, mode)
}

pub fn write_json_with_parent_group<T: Serialize>(path: &Path, value: &T, mode: u32) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    write_with_parent_group(path, &bytes, mode)
}

pub fn create_new(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
