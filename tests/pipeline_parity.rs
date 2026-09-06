use kilnr::pipeline;
use serde_json::{json, Value};

fn networks() -> Vec<String> {
    vec!["none".into(), "kilnr-ci".into()]
}

fn base_job() -> Value {
    json!({"image": "alpine:3.22", "run": ["true"]})
}

fn base_pipeline() -> Value {
    json!({
        "schema": 1,
        "trigger": {"type": "branch", "branches": ["main"]},
        "jobs": {"tests": base_job()}
    })
}

fn parse(value: &Value, kind: &str, package_manager: Option<&str>) -> anyhow::Result<Value> {
    pipeline::load(
        &serde_json::to_vec(value)?,
        kind,
        3,
        &networks(),
        package_manager,
    )
}

fn expect_error(value: &Value, message: &str, kind: &str, package_manager: Option<&str>) {
    let error = parse(value, kind, package_manager).unwrap_err().to_string();
    assert!(
        error.contains(message),
        "expected error containing {message:?}, got {error:?}"
    );
}

#[test]
fn test_basic_normalization() {
    let result = parse(&base_pipeline(), "ci", None).unwrap();
    assert_eq!(result["schema"], 1);
    assert_eq!(result["max_parallel"], 3);
    assert_eq!(
        result["trigger"],
        json!({"type":"branch", "branches":["main"]})
    );
    assert_eq!(result["groups"], json!({}));
    let jobs = result["jobs"].as_object().unwrap();
    assert_eq!(jobs.keys().collect::<Vec<_>>(), vec!["tests"]);
    let job = &result["jobs"]["tests"];
    assert_eq!(job["name"], "tests");
    assert!(job["group"].is_null());
    for field in [
        "needs",
        "resolved_needs",
        "inputs",
        "resolved_inputs",
        "secrets",
        "artifacts",
    ] {
        assert_eq!(job[field], json!([]), "field {field}");
    }
    assert_eq!(job["network"], "none");
    assert_eq!(job["env"], json!({}));
    assert_eq!(job["tools"], json!({}));
    assert_eq!(job["run"], json!(["true"]));
}

#[test]
fn test_checked_in_ci_pipeline_parses_with_dogfooding_values() {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/.kilnr/pipelines/ci.json"
    ))
    .unwrap();
    let result = pipeline::load(&bytes, "ci", 3, &networks(), None).unwrap();
    assert_eq!(result["schema"], 1);
    assert_eq!(result["max_parallel"], 1);
    assert_eq!(
        result["trigger"],
        json!({"type":"branch", "branches":["*"]})
    );
    assert_eq!(
        result["jobs"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["tests"]
    );
    assert_eq!(result["jobs"]["tests"]["image"], "rust:1.85-bookworm");
    assert_eq!(result["jobs"]["tests"]["network"], "none");
    assert_eq!(result["jobs"]["tests"]["run"], json!(["./tests/run.sh"]));
}

#[test]
fn test_schema_must_be_one() {
    let mut data = base_pipeline();
    data["schema"] = json!(2);
    expect_error(&data, "pipeline.schema must be 1", "ci", None);
}

#[test]
fn test_jobs_must_be_non_empty_object() {
    for jobs in [json!({}), json!([])] {
        let mut data = base_pipeline();
        data["jobs"] = jobs;
        expect_error(
            &data,
            "pipeline.jobs must be a non-empty object",
            "ci",
            None,
        );
    }
}

#[test]
fn test_max_parallel_validation() {
    let mut data = base_pipeline();
    data["max_parallel"] = json!(2);
    assert_eq!(parse(&data, "ci", None).unwrap()["max_parallel"], 2);
    for value in [json!(0), json!("2")] {
        data["max_parallel"] = value;
        expect_error(&data, "pipeline.max_parallel invalid", "ci", None);
    }
}

#[test]
fn test_exactly_one_execution_mode() {
    let mut data = base_pipeline();
    data["jobs"] = json!({"x":{"image":"alpine:3.22"}});
    expect_error(
        &data,
        "must define exactly one of run, script, command",
        "ci",
        None,
    );
    data["jobs"] = json!({"x":{"image":"alpine:3.22","run":["true"],"command":["true"]}});
    expect_error(
        &data,
        "must define exactly one of run, script, command",
        "ci",
        None,
    );
}

#[test]
fn test_run_validation() {
    for run in [json!([]), json!([""]), json!(["ok\0bad"])] {
        let data = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"x":{"image":"alpine:3.22","run":run}}});
        expect_error(&data, "invalid run", "ci", None);
    }
}

#[test]
fn test_script_validation() {
    let good = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"x":{"image":"alpine:3.22","script":"scripts/ci/test.sh"}}});
    assert_eq!(
        parse(&good, "ci", None).unwrap()["jobs"]["x"]["script"],
        "scripts/ci/test.sh"
    );
    for script in ["/tmp/test.sh", "../test.sh"] {
        let bad = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"x":{"image":"alpine:3.22","script":script}}});
        expect_error(&bad, "invalid script", "ci", None);
    }
}

#[test]
fn test_command_validation() {
    let good = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"x":{"image":"alpine:3.22","command":["echo","ok"]}}});
    assert_eq!(
        parse(&good, "ci", None).unwrap()["jobs"]["x"]["command"],
        json!(["echo", "ok"])
    );
    for command in [json!([]), json!(["ok", 1])] {
        let bad = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"x":{"image":"alpine:3.22","command":command}}});
        expect_error(&bad, "invalid command", "ci", None);
    }
}

#[test]
fn test_network_defaults_and_allowlist() {
    assert_eq!(
        parse(&base_pipeline(), "ci", None).unwrap()["jobs"]["tests"]["network"],
        "none"
    );
    let allowed = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"x":{"image":"alpine:3.22","run":["true"],"network":"kilnr-ci"}}});
    assert_eq!(
        parse(&allowed, "ci", None).unwrap()["jobs"]["x"]["network"],
        "kilnr-ci"
    );
    let denied = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"x":{"image":"alpine:3.22","run":["true"],"network":"host"}}});
    expect_error(&denied, "network \"host\" not allowed", "ci", None);
}

#[test]
fn test_reserved_env_is_rejected() {
    let data = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"x":{"image":"alpine:3.22","run":["true"],"env":{"KILNR_SHA":"fake"}}}});
    expect_error(&data, "reserved environment variable", "ci", None);
}

#[test]
fn test_release_has_no_branch_trigger_requirement() {
    let data = json!({"schema":1,"jobs":{"release":base_job()}});
    let result = parse(&data, "release", None).unwrap();
    assert!(result.get("trigger").is_none());
}

#[test]
fn test_group_expansion_and_inputs() {
    let data = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{
        "lint":{"image":"alpine:3.22","run":["true"],"group":"quality","artifacts":["lint/**"]},
        "tests":{"image":"alpine:3.22","run":["true"],"group":"quality","artifacts":["tests/**"]},
        "build":{"image":"alpine:3.22","run":["true"],"needs":["quality"],"inputs":["quality"]}
    }});
    let result = parse(&data, "ci", None).unwrap();
    assert_eq!(result["groups"], json!({"quality":["lint","tests"]}));
    assert_eq!(
        result["jobs"]["build"]["resolved_needs"],
        json!(["lint", "tests"])
    );
    assert_eq!(
        result["jobs"]["build"]["resolved_inputs"],
        json!(["lint", "tests"])
    );
}

#[test]
fn test_job_group_name_collision_is_rejected() {
    let data = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"quality":base_job(),"lint":{"image":"x","run":["true"],"group":"quality"}}});
    expect_error(&data, "used by both a job and a group", "ci", None);
}

#[test]
fn test_unknown_need_is_rejected() {
    let data = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"build":{"image":"x","run":["true"],"needs":["missing"]}}});
    expect_error(&data, "needs unknown job or group \"missing\"", "ci", None);
}

#[test]
fn test_self_dependency_through_group_is_rejected() {
    let data = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"tests":{"image":"x","run":["true"],"group":"quality","needs":["quality"]},"lint":{"image":"x","run":["true"],"group":"quality"}}});
    expect_error(&data, "depends on itself", "ci", None);
}

#[test]
fn test_direct_cycle_is_rejected() {
    let data = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"a":{"image":"x","run":["true"],"needs":["b"]},"b":{"image":"x","run":["true"],"needs":["a"]}}});
    expect_error(&data, "dependency cycle", "ci", None);
}

#[test]
fn test_group_expanded_cycle_is_rejected() {
    let data = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"a":{"image":"x","run":["true"],"group":"quality","needs":["build"]},"b":{"image":"x","run":["true"],"group":"quality"},"build":{"image":"x","run":["true"],"needs":["quality"]}}});
    expect_error(&data, "dependency cycle", "ci", None);
}

#[test]
fn test_dependency_deduplication_preserves_job_order() {
    let data = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"lint":{"image":"x","run":["true"],"group":"quality"},"tests":{"image":"x","run":["true"],"group":"quality"},"build":{"image":"x","run":["true"],"needs":["tests","quality","lint"]}}});
    assert_eq!(
        parse(&data, "ci", None).unwrap()["jobs"]["build"]["resolved_needs"],
        json!(["tests", "lint"])
    );
}

#[test]
fn test_cross_group_job_dependency_is_allowed() {
    let data = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"lint":{"image":"x","run":["true"],"group":"quality"},"assets":{"image":"x","run":["true"],"group":"build","needs":["lint"]},"package-linux":{"image":"x","run":["true"],"group":"package","needs":["assets"]}}});
    let result = parse(&data, "ci", None).unwrap();
    assert_eq!(result["jobs"]["assets"]["resolved_needs"], json!(["lint"]));
    assert_eq!(
        result["jobs"]["package-linux"]["resolved_needs"],
        json!(["assets"])
    );
}

#[test]
fn test_artifact_patterns_are_safe_and_non_empty() {
    let good = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"x":{"image":"x","run":["true"],"artifacts":["dist/**","release/*.zip"]}}});
    assert_eq!(
        parse(&good, "ci", None).unwrap()["jobs"]["x"]["artifacts"],
        json!(["dist/**", "release/*.zip"])
    );
    for artifacts in [json!(["/tmp/out"]), json!(["../out"]), json!([""])] {
        let bad = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"x":{"image":"x","run":["true"],"artifacts":artifacts}}});
        expect_error(&bad, "invalid artifact", "ci", None);
    }
}

#[test]
fn test_secret_names_are_environment_names_and_unique() {
    let good = json!({"schema":1,"jobs":{"release":{"image":"x","run":["true"],"secrets":["APPLE_ID","CSC_KEY_PASSWORD"]}}});
    assert_eq!(
        parse(&good, "release", None).unwrap()["jobs"]["release"]["secrets"],
        json!(["APPLE_ID", "CSC_KEY_PASSWORD"])
    );
    let invalid =
        json!({"schema":1,"jobs":{"release":{"image":"x","run":["true"],"secrets":["bad-name"]}}});
    expect_error(&invalid, "invalid secret", "release", None);
    let duplicate = json!({"schema":1,"jobs":{"release":{"image":"x","run":["true"],"secrets":["APPLE_ID","APPLE_ID"]}}});
    expect_error(&duplicate, "duplicate secret", "release", None);
}

#[test]
fn test_ci_cannot_request_release_secrets() {
    let data = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"tests":{"image":"x","run":["true"],"secrets":["APPLE_ID"]}}});
    expect_error(&data, "release-only", "ci", None);
}

#[test]
fn test_inputs_must_be_completed_by_needs_dag() {
    let missing_need = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"build":{"image":"x","run":["true"],"artifacts":["dist/**"]},"publish":{"image":"x","run":["true"],"inputs":["build"]}}});
    expect_error(
        &missing_need,
        "input \"build\" is not a dependency",
        "ci",
        None,
    );
    let transitive = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"quality":{"image":"x","run":["true"],"artifacts":["reports/**"]},"build":{"image":"x","run":["true"],"needs":["quality"],"artifacts":["dist/**"]},"publish":{"image":"x","run":["true"],"needs":["build"],"inputs":["quality"]}}});
    assert_eq!(
        parse(&transitive, "ci", None).unwrap()["jobs"]["publish"]["resolved_inputs"],
        json!(["quality"])
    );
    let no_artifacts = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"build":base_job(),"publish":{"image":"x","run":["true"],"needs":["build"],"inputs":["build"]}}});
    expect_error(&no_artifacts, "declares no artifacts", "ci", None);
}

#[test]
fn test_env_and_secret_names_may_not_overlap() {
    let data = json!({"schema":1,"jobs":{"release":{"image":"x","run":["true"],"env":{"TOKEN":"public"},"secrets":["TOKEN"]}}});
    expect_error(&data, "overlap", "release", None);
}

#[test]
fn test_input_environment_alias_collisions_are_rejected() {
    let data = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"foo-bar":{"image":"x","run":["true"],"artifacts":["a/**"]},"foo_bar":{"image":"x","run":["true"],"artifacts":["b/**"]},"publish":{"image":"x","run":["true"],"needs":["foo-bar","foo_bar"],"inputs":["foo-bar","foo_bar"]}}});
    expect_error(&data, "input environment alias collision", "ci", None);
}

#[test]
fn test_tools_explicit_version_normalizes_to_map() {
    let data = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"tests":{"image":"x","run":["true"],"tools":{"pnpm":"11.15.1"}}}});
    assert_eq!(
        parse(&data, "ci", None).unwrap()["jobs"]["tests"]["tools"],
        json!({"pnpm":"11.15.1"})
    );
}

#[test]
fn test_tools_list_resolves_from_package_manager() {
    let data = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"tests":{"image":"x","run":["true"],"tools":["pnpm"]}}});
    assert_eq!(
        parse(&data, "ci", Some("pnpm@11.15.1")).unwrap()["jobs"]["tests"]["tools"],
        json!({"pnpm":"11.15.1"})
    );
}

#[test]
fn test_tools_list_accepts_package_manager_integrity_suffix() {
    let data = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"tests":{"image":"x","run":["true"],"tools":["pnpm"]}}});
    assert_eq!(
        parse(&data, "ci", Some("pnpm@11.15.1+sha512.deadbeef")).unwrap()["jobs"]["tests"]["tools"],
        json!({"pnpm":"11.15.1"})
    );
}

#[test]
fn test_tools_auto_requires_matching_package_manager() {
    let data = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"tests":{"image":"x","run":["true"],"tools":["pnpm"]}}});
    expect_error(&data, "packageManager", "ci", None);
    expect_error(&data, "packageManager", "ci", Some("npm@11.0.0"));
}

#[test]
fn test_tools_reject_unknown_tools_invalid_versions_and_duplicates() {
    let unknown = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"tests":{"image":"x","run":["true"],"tools":{"yarn":"4.0.0"}}}});
    expect_error(&unknown, "unsupported tool", "ci", None);
    let invalid = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"tests":{"image":"x","run":["true"],"tools":{"pnpm":"latest"}}}});
    expect_error(&invalid, "invalid pnpm version", "ci", None);
    let duplicate = json!({"schema":1,"trigger":{"type":"branch","branches":["main"]},"jobs":{"tests":{"image":"x","run":["true"],"tools":["pnpm","pnpm"]}}});
    expect_error(&duplicate, "duplicate tool", "ci", Some("pnpm@11.15.1"));
}
