use crate::atomic;
use anyhow::{bail, Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretMetadata {
    pub schema: u8,
    pub scope: String,
    pub kind: String,
}

pub fn validate_project(value: &str) -> Result<&str> {
    if !Regex::new(r"^[a-z0-9][a-z0-9_-]{0,62}$")
        .unwrap()
        .is_match(value)
    {
        bail!("invalid project name: {value:?}")
    }
    Ok(value)
}

pub fn validate_name(value: &str) -> Result<&str> {
    if value.starts_with("KILNR_") || !Regex::new(r"^[A-Z_][A-Z0-9_]*$").unwrap().is_match(value) {
        bail!("invalid secret name: {value:?}")
    }
    Ok(value)
}

fn project_dir(root: &Path, project: &str) -> Result<PathBuf> {
    validate_project(project)?;
    let path = root.join(project);
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("secret directory missing for project {project:?}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("secret directory missing for project {project:?}")
    }
    Ok(path)
}

fn paths(root: &Path, project: &str, name: &str) -> Result<(PathBuf, PathBuf)> {
    validate_name(name)?;
    let dir = project_dir(root, project)?;
    Ok((
        dir.join(format!("{name}.value")),
        dir.join(format!("{name}.json")),
    ))
}

pub fn store(root: &Path, project: &str, name: &str, data: &[u8], kind: &str) -> Result<()> {
    if !matches!(kind, "text" | "file") {
        bail!("invalid secret kind: {kind:?}")
    }
    if data.is_empty() {
        bail!("secret value must not be empty")
    }
    if kind == "text" {
        if data.contains(&0) {
            bail!("text secret must not contain NUL")
        }
        std::str::from_utf8(data).context("text secret must be valid UTF-8")?;
    }
    let (value, metadata) = paths(root, project, name)?;
    atomic::write(&value, data, 0o640)?;
    atomic::write_json(
        &metadata,
        &SecretMetadata {
            schema: 1,
            scope: "release".into(),
            kind: kind.into(),
        },
        0o640,
    )
}

pub fn metadata(root: &Path, project: &str, name: &str) -> Result<SecretMetadata> {
    let (value, path) = paths(root, project, name)?;
    for candidate in [&value, &path] {
        let meta = fs::symlink_metadata(candidate).with_context(|| {
            format!("secret {name:?} is not configured for project {project:?}")
        })?;
        if meta.file_type().is_symlink() || !meta.is_file() {
            bail!("unsafe secret path: {}", candidate.display())
        }
    }
    let data: SecretMetadata = serde_json::from_slice(&fs::read(path)?)?;
    if data.schema != 1 || data.scope != "release" || !matches!(data.kind.as_str(), "text" | "file")
    {
        bail!("invalid secret metadata for {name:?}")
    }
    Ok(data)
}

pub fn read(root: &Path, project: &str, name: &str) -> Result<Vec<u8>> {
    metadata(root, project, name)?;
    Ok(fs::read(paths(root, project, name)?.0)?)
}

pub fn list(root: &Path, project: &str) -> Result<Vec<(String, SecretMetadata)>> {
    let dir = project_dir(root, project)?;
    let mut names = fs::read_dir(dir)?
        .filter_map(Result::ok)
        .filter_map(|e| {
            e.path()
                .file_stem()
                .and_then(|n| n.to_str())
                .map(str::to_owned)
        })
        .filter(|n| e_valid(n))
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    Ok(names
        .into_iter()
        .filter_map(|n| metadata(root, project, &n).ok().map(|m| (n, m)))
        .collect())
}

fn e_valid(name: &str) -> bool {
    validate_name(name).is_ok()
}

pub fn delete(root: &Path, project: &str, name: &str) -> Result<()> {
    let (value, metadata) = paths(root, project, name)?;
    for path in [value, metadata] {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
