use crate::{artifacts, atomic, secrets};
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub fn write_json(path: &Path, value: &Value) -> Result<()> {
    atomic::write_json(path, value, 0o640)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn render_run_script(commands: &[String]) -> String {
    let mut text = "#!/bin/sh\nset -eu\n".to_owned();
    for command in commands {
        text.push_str(&format!(
            "printf '%s\\n' {}\n{command}\n",
            shell_quote(&format!("$ {command}"))
        ));
    }
    text
}

pub fn prepare_execution(
    build_dir: &Path,
    job_id: &str,
    job: &Value,
) -> Result<(Vec<String>, Vec<String>)> {
    let modes = ["run", "script", "command"]
        .into_iter()
        .filter(|mode| job.get(*mode).is_some())
        .collect::<Vec<_>>();
    if modes.len() != 1 {
        bail!("job {job_id:?} has invalid execution mode")
    }
    match modes[0] {
        "run" => {
            let commands = job["run"]
                .as_array()
                .context("invalid run commands")?
                .iter()
                .map(|item| {
                    item.as_str()
                        .context("invalid run command")
                        .map(str::to_owned)
                })
                .collect::<Result<Vec<_>>>()?;
            let commands_dir = build_dir.join("commands");
            fs::create_dir_all(&commands_dir)?;
            fs::set_permissions(&commands_dir, fs::Permissions::from_mode(0o750))?;
            let script = commands_dir.join(format!("{job_id}.sh"));
            fs::write(&script, render_run_script(&commands))?;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o600))?;
            Ok((
                vec![format!(
                    "type=bind,src={},dst=/run/kilnr/job.sh,readonly",
                    script.display()
                )],
                vec!["/bin/sh".into(), "/run/kilnr/job.sh".into()],
            ))
        }
        "script" => Ok((
            vec![],
            vec![format!(
                "/workspace/{}",
                job["script"].as_str().context("invalid script")?
            )],
        )),
        _ => Ok((
            vec![],
            job["command"]
                .as_array()
                .context("invalid command")?
                .iter()
                .map(|item| {
                    item.as_str()
                        .context("invalid command argument")
                        .map(str::to_owned)
                })
                .collect::<Result<Vec<_>>>()?,
        )),
    }
}

pub fn update_job(build_dir: &Path, job_id: &str, changes: &Value) -> Result<()> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o640)
        .open(build_dir.join("status.lock"))?;
    lock.set_permissions(fs::Permissions::from_mode(0o640))?;
    lock.lock_exclusive()?;
    let mut status: Value = serde_json::from_slice(&fs::read(build_dir.join("status.json"))?)?;
    let target = status["pipeline"]["jobs"]
        .get_mut(job_id)
        .with_context(|| format!("job {job_id:?} missing from status"))?
        .as_object_mut()
        .context("invalid job status")?;
    target.extend(
        changes
            .as_object()
            .context("job changes must be an object")?
            .clone(),
    );
    write_json(&build_dir.join("status.json"), &status)
}

pub fn build_input_mounts(input_roots: &BTreeMap<String, PathBuf>) -> Vec<String> {
    input_roots
        .iter()
        .map(|(producer, root)| {
            format!(
                "type=bind,src={},dst=/run/kilnr/inputs/{producer},readonly",
                root.display()
            )
        })
        .collect()
}

pub fn build_public_env(
    runtime: &Value,
    job_id: &str,
    job: &Value,
    input_roots: &BTreeMap<String, PathBuf>,
) -> Result<BTreeMap<String, String>> {
    let required = |key: &str| {
        runtime[key]
            .as_str()
            .with_context(|| format!("runtime {key} must be a string"))
    };
    let mut env = BTreeMap::from([
        ("CI".into(), "true".into()),
        ("HOME".into(), "/run/kilnr/home".into()),
        ("KILNR_BUILD_ID".into(), required("build_id")?.into()),
        ("KILNR_PROJECT".into(), required("project")?.into()),
        ("KILNR_SHA".into(), required("sha")?.into()),
        ("KILNR_REF".into(), required("ref")?.into()),
        ("KILNR_JOB_TYPE".into(), required("job_type")?.into()),
        ("KILNR_JOB".into(), job_id.into()),
        ("XDG_RUNTIME_DIR".into(), "/run/kilnr/tmp".into()),
        ("TMPDIR".into(), "/run/kilnr/tmp".into()),
        ("TMP".into(), "/run/kilnr/tmp".into()),
        ("TEMP".into(), "/run/kilnr/tmp".into()),
    ]);
    for key in ["branch", "tag"] {
        if let Some(value) = runtime.get(key).and_then(Value::as_str) {
            env.insert(format!("KILNR_{}", key.to_uppercase()), value.into());
        }
    }
    if let Some(values) = job.get("env").and_then(Value::as_object) {
        for (key, value) in values {
            env.insert(
                key.clone(),
                value.as_str().context("invalid public environment")?.into(),
            );
        }
    }
    if job["cache"].get("pnpm").is_some() {
        env.insert(
            "PNPM_CONFIG_STORE_DIR".into(),
            "/run/kilnr/cache/pnpm".into(),
        );
    }
    let mut aliases = BTreeSet::new();
    for producer in input_roots.keys() {
        let alias = format!("KILNR_INPUT_{}", producer.to_uppercase().replace('-', "_"));
        if !aliases.insert(alias.clone()) {
            bail!("input environment alias collision for {producer:?}")
        }
        env.insert(alias, format!("/run/kilnr/inputs/{producer}"));
    }
    Ok(env)
}

pub fn prepare_cache_mounts(root: &Path, runtime: &Value, job: &Value) -> Result<Vec<String>> {
    let Some(caches) = job.get("cache").and_then(Value::as_object) else {
        return if job.get("cache").is_none_or(Value::is_null) {
            Ok(vec![])
        } else {
            bail!("invalid runtime cache configuration")
        };
    };
    let project = runtime["project"]
        .as_str()
        .filter(|value| {
            Regex::new(r"^[a-z0-9][a-z0-9_-]{0,62}$")
                .unwrap()
                .is_match(value)
        })
        .context("invalid runtime project for cache")?;
    let job_type = runtime["job_type"]
        .as_str()
        .filter(|value| matches!(*value, "ci" | "release"))
        .context("invalid runtime job type for cache")?;
    let version_re = Regex::new(r"^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?$").unwrap();
    let mut mounts = vec![];
    for (name, version) in caches {
        if name != "pnpm" {
            bail!("unsupported runtime cache {name:?}")
        }
        let version = version
            .as_str()
            .filter(|value| version_re.is_match(value))
            .with_context(|| format!("invalid runtime cache version for {name:?}"))?;
        let path = root.join(project).join(job_type).join(name).join(version);
        fs::create_dir_all(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        mounts.push(format!(
            "type=bind,src={},dst=/run/kilnr/cache/{name}",
            path.display()
        ));
    }
    Ok(mounts)
}

pub fn collect_job_artifacts(
    build_dir: &Path,
    work_dir: &Path,
    job_id: &str,
    job: &Value,
) -> Result<Vec<String>> {
    let patterns = job["artifacts"]
        .as_array()
        .map(|values| {
            values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .context("invalid artifact pattern")
                        .map(str::to_owned)
                })
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();
    if patterns.is_empty() {
        Ok(vec![])
    } else {
        artifacts::collect(
            work_dir,
            &patterns,
            &build_dir.join("artifacts").join(job_id),
        )
    }
}

pub fn render_secret_wrapper(
    metadata: &BTreeMap<String, secrets::SecretMetadata>,
) -> Result<String> {
    let mut text = "#!/bin/sh\nset -eu\n".to_owned();
    for (name, item) in metadata {
        match item.kind.as_str() {
            "text" => text.push_str(&format!(
                "export {name}=\"$(cat /run/kilnr/secrets/{name}.value)\"\n"
            )),
            "file" => text.push_str(&format!(
                "export {name}=\"/run/kilnr/secrets/{name}.value\"\n"
            )),
            _ => bail!("secret {name:?} has invalid kind"),
        }
    }
    text.push_str("exec \"$@\"\n");
    Ok(text)
}

#[derive(Debug)]
pub struct SecretStage {
    pub path: Option<PathBuf>,
    pub metadata: BTreeMap<String, secrets::SecretMetadata>,
    pub redaction_values: Vec<String>,
}

pub fn prepare_secret_stage(
    secrets_root: &Path,
    staging_root: &Path,
    build_id: &str,
    job_id: &str,
    runtime: &Value,
    job: &Value,
) -> Result<SecretStage> {
    let names = job["secrets"]
        .as_array()
        .map(|values| values.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    if names.is_empty() {
        return Ok(SecretStage {
            path: None,
            metadata: BTreeMap::new(),
            redaction_values: vec![],
        });
    }
    if runtime["job_type"] != "release" {
        bail!("secrets are release-only")
    }
    let project = runtime["project"].as_str().context("invalid project")?;
    let stage = staging_root.join(build_id).join(job_id);
    if stage.exists() {
        fs::remove_dir_all(&stage)?;
    }
    fs::create_dir_all(&stage)?;
    fs::set_permissions(&stage, fs::Permissions::from_mode(0o700))?;
    let mut metadata = BTreeMap::new();
    let mut redact = vec![];
    for name in names {
        let item = secrets::metadata(secrets_root, project, name)?;
        let data = secrets::read(secrets_root, project, name)?;
        let target = stage.join(format!("{name}.value"));
        fs::write(&target, &data)?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o400))?;
        if let Ok(value) = String::from_utf8(data) {
            if !value.is_empty() {
                redact.push(value);
            }
        }
        metadata.insert(name.into(), item);
    }
    Ok(SecretStage {
        path: Some(stage),
        metadata,
        redaction_values: redact,
    })
}

pub fn redaction_tokens(values: &[String]) -> Vec<String> {
    let mut tokens = BTreeSet::new();
    for value in values.iter().filter(|value| !value.is_empty()) {
        tokens.insert(value.clone());
        tokens.extend(
            value
                .lines()
                .filter(|line| !line.is_empty())
                .map(str::to_owned),
        );
    }
    let mut result = tokens.into_iter().collect::<Vec<_>>();
    result.sort_by_key(|value| std::cmp::Reverse(value.len()));
    result
}

pub fn redact_text(mut text: String, tokens: &[String]) -> String {
    for token in tokens {
        text = text.replace(token, "***");
    }
    text
}

pub fn render_tools_wrapper(tools: &serde_json::Map<String, Value>) -> String {
    if tools.is_empty() {
        return "#!/bin/sh\nset -eu\nexec \"$@\"\n".into();
    }
    "#!/bin/sh\nset -eu\nTOOLS_ROOT=\"/run/kilnr/tmp/tools\"\nmkdir -p \"$TOOLS_ROOT/corepack\"\nexport COREPACK_HOME=\"$TOOLS_ROOT/corepack\"\nexport COREPACK_DEFAULT_TO_LATEST=0\nexport COREPACK_ENABLE_PROJECT_SPEC=0\nexport PATH=\"/run/kilnr/tools/bin:$PATH\"\nexec \"$@\"\n".into()
}

pub fn prepare_tools_wrapper(
    build_dir: &Path,
    job_id: &str,
    tools: &serde_json::Map<String, Value>,
    execution_argv: &[String],
) -> Result<(Vec<String>, Vec<String>)> {
    if tools.is_empty() {
        return Ok((vec![], execution_argv.to_vec()));
    }
    let commands = build_dir.join("commands");
    fs::create_dir_all(&commands)?;
    let wrapper = commands.join(format!("{job_id}.tools.sh"));
    fs::write(&wrapper, render_tools_wrapper(tools))?;
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o600))?;
    let tools_root = build_dir.join("runtime").join(job_id).join("tools");
    let bin = tools_root.join("bin");
    fs::create_dir_all(&bin)?;
    if let Some(version) = tools.get("pnpm").and_then(Value::as_str) {
        let pnpm = bin.join("pnpm");
        fs::write(
            &pnpm,
            format!(
                "#!/bin/sh\nexec corepack {} \"$@\"\n",
                shell_quote(&format!("pnpm@{version}"))
            ),
        )?;
        fs::set_permissions(&pnpm, fs::Permissions::from_mode(0o750))?;
    }
    let mounts = vec![
        format!(
            "type=bind,src={},dst=/run/kilnr/tools-wrapper.sh,readonly",
            wrapper.display()
        ),
        format!(
            "type=bind,src={},dst=/run/kilnr/tools,readonly",
            tools_root.display()
        ),
    ];
    let mut argv = vec!["/bin/sh".into(), "/run/kilnr/tools-wrapper.sh".into()];
    argv.extend_from_slice(execution_argv);
    Ok((mounts, argv))
}
