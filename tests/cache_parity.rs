use kilnr::pipeline;
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs};

fn parse_job(job: Value, package_manager: Option<&str>) -> anyhow::Result<Value> {
    let data =
        json!({"schema":1,"trigger":{"type":"branch","branches":["*"]},"jobs":{"tests":job}});
    Ok(pipeline::load(
        &serde_json::to_vec(&data)?,
        "ci",
        3,
        &["none".into(), "kilnr-ci".into()],
        package_manager,
    )?["jobs"]["tests"]
        .clone())
}

fn base_job(cache: Option<Value>) -> Value {
    let mut job = json!({"image":"node:24-bookworm","tools":["pnpm"],"run":["pnpm install --frozen-lockfile"]});
    if let Some(value) = cache {
        job["cache"] = value;
    }
    job
}

fn error(job: Value, expected: &str) {
    let message = parse_job(job, Some("pnpm@11.15.1"))
        .unwrap_err()
        .to_string();
    assert!(
        message.contains(expected),
        "expected {expected:?}, got {message:?}"
    );
}

#[test]
fn test_pnpm_cache_normalizes_to_resolved_tool_version() {
    assert_eq!(
        parse_job(base_job(Some(json!(["pnpm"]))), Some("pnpm@11.15.1")).unwrap()["cache"],
        json!({"pnpm":"11.15.1"})
    );
}

#[test]
fn test_cache_defaults_to_empty_map() {
    assert_eq!(
        parse_job(base_job(None), Some("pnpm@11.15.1")).unwrap()["cache"],
        json!({})
    );
}

#[test]
fn test_cache_requires_matching_managed_tool() {
    error(
        json!({"image":"node:24-bookworm","cache":["pnpm"],"run":["true"]}),
        "cache \"pnpm\" requires managed tool \"pnpm\"",
    );
}

#[test]
fn test_cache_rejects_unknown_names_duplicates_and_invalid_shape() {
    error(base_job(Some(json!(["yarn"]))), "unsupported cache");
    error(base_job(Some(json!(["pnpm", "pnpm"]))), "duplicate cache");
    error(base_job(Some(json!("pnpm"))), "invalid cache");
}

#[test]
fn test_cache_root_is_project_job_type_tool_and_version_scoped() {
    let root = tempfile::tempdir().unwrap();
    let job = json!({"cache":{"pnpm":"11.15.1"}});
    let cases = [
        json!({"project":"review_desk","job_type":"ci"}),
        json!({"project":"review_desk","job_type":"release"}),
        json!({"project":"other","job_type":"ci"}),
    ];
    let mounts = cases
        .iter()
        .map(|value| kilnr::runtime::prepare_cache_mounts(root.path(), value, &job).unwrap())
        .collect::<Vec<_>>();
    let other_version = kilnr::runtime::prepare_cache_mounts(
        root.path(),
        &cases[0],
        &json!({"cache":{"pnpm":"11.16.0"}}),
    )
    .unwrap();
    assert_ne!(mounts[0], mounts[1]);
    assert_ne!(mounts[0], mounts[2]);
    assert_ne!(mounts[0], other_version);
    assert!(mounts[0][0].contains("/review_desk/ci/pnpm/11.15.1"));
    assert!(mounts[0][0].contains("dst=/run/kilnr/cache/pnpm"));
    assert!(!mounts[0][0].contains("readonly"));
}

#[test]
fn test_public_env_configures_pnpm_store_only_when_cache_enabled() {
    let runtime = json!({"build_id":"build-1","project":"review_desk","sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","ref":"refs/heads/main","job_type":"ci","branch":"main"});
    let cached = kilnr::runtime::build_public_env(
        &runtime,
        "tests",
        &json!({"env":{},"cache":{"pnpm":"11.15.1"}}),
        &BTreeMap::new(),
    )
    .unwrap();
    let uncached =
        kilnr::runtime::build_public_env(&runtime, "tests", &json!({"env":{}}), &BTreeMap::new())
            .unwrap();
    assert_eq!(cached["PNPM_CONFIG_STORE_DIR"], "/run/kilnr/cache/pnpm");
    assert!(!uncached.contains_key("PNPM_CONFIG_STORE_DIR"));
    assert!(!cached.contains_key("npm_config_store_dir"));
}

#[test]
fn test_install_declares_private_cache_root() {
    let text = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh")).unwrap();
    assert!(text.contains("/var/lib/kilnr/cache"));
    assert!(text.contains("install -d -o kilnr -g kilnr -m 0700"));
}
