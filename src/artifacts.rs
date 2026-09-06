use anyhow::{bail, Context, Result};
use glob::glob;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn reject_symlinks(path: &Path, root: &Path) -> Result<()> {
    let relative = path
        .strip_prefix(root)
        .context("artifact escapes workspace")?;
    let mut current = root.to_owned();
    for component in relative.components() {
        current.push(component);
        if fs::symlink_metadata(&current)?.file_type().is_symlink() {
            bail!("artifact path contains symlink: {}", relative.display());
        }
    }
    Ok(())
}

fn visit(path: &Path, root: &Path, selected: &mut BTreeMap<String, PathBuf>) -> Result<usize> {
    reject_symlinks(path, root)?;
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        bail!("artifact path is a symlink")
    }
    if meta.is_file() {
        let key = path
            .strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/");
        selected.insert(key, path.to_owned());
        return Ok(1);
    }
    if !meta.is_dir() {
        return Ok(0);
    }
    let mut count = 0;
    for entry in fs::read_dir(path)? {
        let child = entry?.path();
        count += visit(&child, root, selected)?;
    }
    Ok(count)
}

pub fn collect(workspace: &Path, patterns: &[String], destination: &Path) -> Result<Vec<String>> {
    let workspace = workspace.canonicalize()?;
    if !workspace.is_dir() {
        bail!("workspace does not exist: {}", workspace.display())
    }
    let mut selected = BTreeMap::new();
    for pattern in patterns {
        let mut count = 0;
        // `pathlib.Path.glob("dir/**")`, used by the original implementation,
        // includes the directory itself. The glob crate treats `**` slightly
        // differently, so handle recursive directory suffixes explicitly and
        // add the zero-directory form for `**/` patterns.
        if let Some(prefix) = pattern.strip_suffix("/**") {
            let path = workspace.join(prefix);
            if path.exists() {
                count += visit(&path, &workspace, &mut selected)?;
            }
        } else {
            let mut variants = vec![pattern.clone()];
            if pattern.contains("**/") {
                variants.push(pattern.replacen("**/", "", 1));
            }
            for variant in variants {
                let full = workspace.join(variant).to_string_lossy().into_owned();
                for path in glob(&full).context("invalid artifact pattern")?.flatten() {
                    count += visit(&path, &workspace, &mut selected)?;
                }
            }
        }
        if count == 0 {
            bail!("artifact pattern matched no files: {pattern}")
        }
    }
    if destination.exists() {
        fs::remove_dir_all(destination)?;
    }
    fs::create_dir_all(destination)?;
    fs::set_permissions(destination, fs::Permissions::from_mode(0o750))?;
    for (relative, source) in &selected {
        let target = destination.join(relative);
        fs::create_dir_all(target.parent().unwrap())?;
        fs::copy(source, &target)?;
        fs::set_permissions(
            &target,
            fs::Permissions::from_mode(fs::metadata(source)?.permissions().mode() & 0o777),
        )?;
    }
    Ok(selected.into_keys().collect())
}

pub fn input_roots(build: &Path, producers: &[String]) -> Result<BTreeMap<String, PathBuf>> {
    let mut result = BTreeMap::new();
    for producer in producers {
        let path = build.join("artifacts").join(producer);
        if !path.is_dir() {
            bail!("input artifacts unavailable for producer {producer:?}")
        }
        result.insert(producer.clone(), path);
    }
    Ok(result)
}
