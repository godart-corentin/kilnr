use anyhow::{bail, Context, Result};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

fn string_list(value: &Value, label: &str, max: usize, allow_empty: bool) -> Result<Vec<String>> {
    let array = value
        .as_array()
        .with_context(|| format!("{label} invalid"))?;
    if array.len() > max || (!allow_empty && array.is_empty()) {
        bail!("{label} invalid")
    }
    array
        .iter()
        .map(|item| {
            item.as_str()
                .filter(|s| !s.is_empty() && !s.contains('\0'))
                .map(str::to_owned)
                .with_context(|| format!("{label} invalid"))
        })
        .collect()
}

fn valid_name(name: &str) -> bool {
    Regex::new(r"^[a-z0-9][a-z0-9_-]{0,62}$")
        .unwrap()
        .is_match(name)
}
fn valid_env(name: &str) -> bool {
    Regex::new(r"^[A-Z_][A-Z0-9_]*$").unwrap().is_match(name)
}
fn valid_version(version: &str) -> bool {
    Regex::new(r"^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?$")
        .unwrap()
        .is_match(version)
}

fn trigger(value: Option<&Value>, kind: &str) -> Result<Option<Value>> {
    if kind == "release" {
        if value.is_some_and(|v| !v.is_null()) {
            bail!("release pipeline must not define trigger")
        };
        return Ok(None);
    }
    if kind != "ci" {
        bail!("unsupported pipeline kind: {kind:?}")
    }
    let obj = value
        .and_then(Value::as_object)
        .context("pipeline.trigger must be an object")?;
    if obj.get("type").and_then(Value::as_str) != Some("branch") {
        bail!("pipeline.trigger.type must be 'branch'")
    }
    let branches = string_list(
        obj.get("branches").unwrap_or(&Value::Null),
        "pipeline.trigger.branches",
        64,
        false,
    )?;
    let mut seen = BTreeSet::new();
    for pattern in &branches {
        if pattern.len() > 255 || pattern.starts_with("refs/") {
            bail!("pipeline.trigger.branches invalid pattern {pattern:?}")
        }
        if !seen.insert(pattern) {
            bail!("pipeline.trigger.branches contains duplicates")
        }
    }
    Ok(Some(json!({"type":"branch", "branches":branches})))
}

fn path_value(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('\0')
        && !value.ends_with('/')
        && !Path::new(value).is_absolute()
        && !Path::new(value)
            .components()
            .any(|c| matches!(c, Component::ParentDir))
}

fn normalize_job(
    name: &str,
    spec: &Value,
    allowed_networks: &[String],
    package_manager: Option<&str>,
) -> Result<Value> {
    if !valid_name(name) {
        bail!("invalid job name: {name:?}")
    }
    let obj = spec
        .as_object()
        .with_context(|| format!("job {name:?} must be an object"))?;
    let image = obj
        .get("image")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 255 && !s.chars().any(|c| c < ' '))
        .with_context(|| format!("job {name:?}: invalid image"))?;
    let modes = ["run", "script", "command"]
        .into_iter()
        .filter(|key| obj.contains_key(*key))
        .collect::<Vec<_>>();
    if modes.len() != 1 {
        bail!("job {name:?} must define exactly one of run, script, command")
    }
    let mode = modes[0];
    let execution = if mode == "script" {
        let script = obj[mode]
            .as_str()
            .filter(|s| path_value(s))
            .with_context(|| format!("job {name:?}: invalid script"))?;
        Value::String(script.into())
    } else {
        Value::Array(
            string_list(
                &obj[mode],
                &format!("job {name:?}: invalid {mode}"),
                64,
                false,
            )?
            .into_iter()
            .map(Value::String)
            .collect(),
        )
    };
    let network = obj.get("network").and_then(Value::as_str).unwrap_or("none");
    if !allowed_networks.iter().any(|n| n == network) {
        bail!("job {name:?}: network {network:?} not allowed")
    }
    let group = match obj.get("group") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(v)) if valid_name(v) => Value::String(v.clone()),
        _ => bail!("job {name:?}: invalid group"),
    };
    let needs = string_list(
        obj.get("needs").unwrap_or(&json!([])),
        &format!("job {name:?}: invalid needs"),
        64,
        true,
    )?;
    let inputs = string_list(
        obj.get("inputs").unwrap_or(&json!([])),
        &format!("job {name:?}: invalid inputs"),
        64,
        true,
    )?;
    let artifacts = string_list(
        obj.get("artifacts").unwrap_or(&json!([])),
        &format!("job {name:?}: invalid artifacts"),
        128,
        true,
    )?;
    let secrets = string_list(
        obj.get("secrets").unwrap_or(&json!([])),
        &format!("job {name:?}: invalid secrets"),
        64,
        true,
    )?;
    for value in &artifacts {
        if !path_value(value) {
            bail!("job {name:?}: invalid artifact pattern {value:?}")
        }
    }
    for value in &secrets {
        if !valid_env(value) || value.starts_with("KILNR_") {
            bail!("job {name:?}: invalid secret name {value:?}")
        }
    }
    if artifacts.iter().collect::<BTreeSet<_>>().len() != artifacts.len() {
        bail!("job {name:?}: duplicate artifact pattern")
    }
    if secrets.iter().collect::<BTreeSet<_>>().len() != secrets.len() {
        bail!("job {name:?}: duplicate secret")
    }
    let mut env = Map::new();
    if let Some(value) = obj.get("env") {
        for (key, value) in value
            .as_object()
            .with_context(|| format!("job {name:?}: invalid env"))?
        {
            if key.starts_with("KILNR_") {
                bail!("job {name:?}: reserved environment variable {key:?}")
            }
            if !valid_env(key) {
                bail!("job {name:?}: invalid environment variable name {key:?}")
            }
            let text = value
                .as_str()
                .filter(|v| !v.contains('\0'))
                .with_context(|| format!("job {name:?}: invalid environment variable {key:?}"))?;
            env.insert(key.clone(), Value::String(text.into()));
        }
    }
    if let Some(overlap) = secrets.iter().find(|key| env.contains_key(*key)) {
        bail!("job {name:?}: environment and secret names overlap: {overlap}")
    }
    let mut tools = Map::new();
    if let Some(value) = obj.get("tools") {
        match value {
            Value::Array(_) => {
                for tool in string_list(value, &format!("job {name:?}: invalid tools"), 8, true)? {
                    if tool != "pnpm" {
                        bail!("job {name:?}: unsupported tool {tool:?}")
                    }
                    if tools.contains_key(&tool) {
                        bail!("job {name:?}: duplicate tool {tool:?}")
                    }
                    let prefix = format!("{tool}@");
                    let version = package_manager.and_then(|p| p.strip_prefix(&prefix)).map(|v| v.split('+').next().unwrap()).filter(|v| valid_version(v)).with_context(|| format!("job {name:?}: tools requests {tool:?} but package.json packageManager must declare {tool}@<version>"))?;
                    tools.insert(tool, Value::String(version.into()));
                }
            }
            Value::Object(values) => {
                for (tool, version) in values {
                    let version = version
                        .as_str()
                        .filter(|v| valid_version(v))
                        .with_context(|| format!("job {name:?}: invalid {tool} version"))?;
                    if tool != "pnpm" {
                        bail!("job {name:?}: unsupported tool {tool:?}")
                    }
                    tools.insert(tool.clone(), Value::String(version.into()));
                }
            }
            _ => bail!("job {name:?}: invalid tools"),
        }
    }
    let cache_names = match obj.get("cache") {
        None | Some(Value::Null) => vec![],
        Some(v) => string_list(v, &format!("job {name:?}: invalid cache"), 8, true)?,
    };
    let mut cache = Map::new();
    for item in cache_names {
        if item != "pnpm" {
            bail!("job {name:?}: unsupported cache {item:?}")
        }
        if cache.contains_key(&item) {
            bail!("job {name:?}: duplicate cache {item:?}")
        };
        let v = tools.get(&item).with_context(|| {
            format!("job {name:?}: cache {item:?} requires managed tool {item:?}")
        })?;
        cache.insert(item, v.clone());
    }
    let mut normalized = Map::from_iter([
        ("name".into(), json!(name)),
        ("group".into(), group),
        ("needs".into(), json!(needs)),
        ("resolved_needs".into(), json!([])),
        ("inputs".into(), json!(inputs)),
        ("resolved_inputs".into(), json!([])),
        ("image".into(), json!(image)),
        ("network".into(), json!(network)),
        ("env".into(), Value::Object(env)),
        ("secrets".into(), json!(secrets)),
        ("artifacts".into(), json!(artifacts)),
        ("tools".into(), Value::Object(tools)),
        ("cache".into(), Value::Object(cache)),
    ]);
    normalized.insert(mode.into(), execution);
    Ok(Value::Object(normalized))
}

fn strings(value: &Value, key: &str) -> Vec<String> {
    value[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

pub fn load(
    raw: &[u8],
    kind: &str,
    default_parallel: u64,
    allowed_networks: &[String],
    package_manager: Option<&str>,
) -> Result<Value> {
    let data: Value = serde_json::from_slice(raw).context("invalid pipeline JSON")?;
    let root = data.as_object().context("pipeline must be a JSON object")?;
    if root.get("schema").and_then(Value::as_u64) != Some(1) {
        bail!("pipeline.schema must be 1")
    }
    let trigger = trigger(root.get("trigger"), kind)?;
    let parallel = root
        .get("max_parallel")
        .map(Value::as_u64)
        .unwrap_or(Some(default_parallel))
        .filter(|v| *v >= 1)
        .context("pipeline.max_parallel invalid")?;
    let raw_jobs = root
        .get("jobs")
        .and_then(Value::as_object)
        .filter(|v| !v.is_empty())
        .context("pipeline.jobs must be a non-empty object")?;
    let mut jobs = Map::new();
    for (name, spec) in raw_jobs {
        let job = normalize_job(name, spec, allowed_networks, package_manager)?;
        if kind == "ci" && !strings(&job, "secrets").is_empty() {
            bail!("job {name:?}: secrets are release-only")
        };
        jobs.insert(name.clone(), job);
    }
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, job) in &jobs {
        if let Some(group) = job["group"].as_str() {
            if jobs.contains_key(group) {
                bail!("name {group:?} is used by both a job and a group")
            };
            groups.entry(group.into()).or_default().push(name.clone());
        }
    }
    for name in jobs.keys().cloned().collect::<Vec<_>>() {
        for (field, target) in [("needs", "resolved_needs"), ("inputs", "resolved_inputs")] {
            let mut resolved = vec![];
            for reference in strings(&jobs[&name], field) {
                let candidates = if jobs.contains_key(&reference) {
                    vec![reference.clone()]
                } else {
                    groups.get(&reference).cloned().with_context(|| {
                        format!("job {name:?} {field} unknown job or group {reference:?}")
                    })?
                };
                for candidate in candidates {
                    if candidate == name {
                        bail!("job {name:?} depends on itself via {field}")
                    };
                    if !resolved.contains(&candidate) {
                        resolved.push(candidate)
                    }
                }
            }
            jobs.get_mut(&name).unwrap()[target] = json!(resolved);
        }
    }
    fn visit(
        name: &str,
        jobs: &Map<String, Value>,
        state: &mut BTreeMap<String, u8>,
        stack: &mut Vec<String>,
    ) -> Result<()> {
        match state.get(name) {
            Some(2) => return Ok(()),
            Some(1) => {
                stack.push(name.into());
                bail!("dependency cycle: {}", stack.join(" -> "))
            }
            _ => {}
        }
        state.insert(name.into(), 1);
        stack.push(name.into());
        for dep in strings(&jobs[name], "resolved_needs") {
            visit(&dep, jobs, state, stack)?;
        }
        stack.pop();
        state.insert(name.into(), 2);
        Ok(())
    }
    let mut state = BTreeMap::new();
    for name in jobs.keys() {
        visit(name, &jobs, &mut state, &mut vec![])?;
    }
    for (name, job) in &jobs {
        let mut allowed = BTreeSet::new();
        let mut pending = strings(job, "resolved_needs");
        while let Some(dep) = pending.pop() {
            if allowed.insert(dep.clone()) {
                pending.extend(strings(&jobs[&dep], "resolved_needs"));
            }
        }
        let mut aliases = BTreeMap::new();
        for producer in strings(job, "resolved_inputs") {
            if !allowed.contains(&producer) {
                bail!("job {name:?}: input {producer:?} is not a dependency")
            };
            if strings(&jobs[&producer], "artifacts").is_empty() {
                bail!("job {name:?}: input producer {producer:?} declares no artifacts")
            };
            let alias = producer.to_uppercase().replace('-', "_");
            if let Some(old) = aliases.insert(alias, producer.clone()) {
                if old != producer {
                    bail!("job {name:?}: input environment alias collision")
                }
            }
        }
    }
    let mut result = json!({"schema":1,"max_parallel":parallel,"groups":groups,"jobs":jobs});
    if let Some(value) = trigger {
        result["trigger"] = value;
    }
    Ok(result)
}

pub fn load_trigger(raw: &[u8]) -> Result<Value> {
    let data: Value = serde_json::from_slice(raw).context("invalid pipeline JSON")?;
    if data["schema"] != 1 {
        bail!("pipeline.schema must be 1")
    };
    Ok(trigger(data.get("trigger"), "ci")?.unwrap())
}

pub fn matches_branch(pipeline: &Value, branch: &str) -> bool {
    pipeline["trigger"]["branches"]
        .as_array()
        .is_some_and(|patterns| {
            patterns
                .iter()
                .filter_map(Value::as_str)
                .any(|pattern| glob_match(pattern, branch))
        })
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let escaped = regex::escape(pattern)
        .replace(r"\*", ".*")
        .replace(r"\?", ".");
    Regex::new(&format!("^{escaped}$")).unwrap().is_match(text)
}
