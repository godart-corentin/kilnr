use crate::{artifacts, atomic, pipeline, runtime as runtime_helpers};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const BUILDS: &str = "/var/lib/kilnr/builds";
const CONFIG: &str = "/etc/kilnr/projects";
const INCOMING: &str = "/var/lib/kilnr/queue/incoming";
const RUNNING: &str = "/var/lib/kilnr/queue/running";
const SECRET_ROOT: &str = "/etc/kilnr/secrets";
fn now() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}
fn read(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn elapsed(value: &str) -> f64 {
    DateTime::parse_from_rfc3339(value)
        .map(|v| (Utc::now() - v.with_timezone(&Utc)).num_milliseconds() as f64 / 1000.)
        .unwrap_or(0.)
}
fn command(program: &str, args: &[&str]) -> Result<std::process::Output> {
    let out = Command::new(program).args(args).output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim())
    }
    Ok(out)
}
fn git(repo: &str, args: &[&str]) -> Result<Vec<u8>> {
    let mut all = vec![format!("--git-dir={repo}")];
    all.extend(args.iter().map(|v| v.to_string()));
    Ok(command(
        "/usr/bin/git",
        &all.iter().map(String::as_str).collect::<Vec<_>>(),
    )?
    .stdout)
}
fn cfg(project: &str) -> Result<Value> {
    let value = read(&Path::new(CONFIG).join(format!("{project}.json")))?;
    let expected = format!("/srv/git/{project}.git");
    if value["schema"] != 1 || value["project"] != project || value["repository"] != expected {
        bail!("invalid project configuration")
    };
    Ok(value)
}
fn branch(job: &Value) -> Option<&str> {
    job.get("branch")
        .and_then(Value::as_str)
        .or_else(|| job["ref"].as_str()?.strip_prefix("refs/heads/"))
}

pub fn select_pipeline(job: &Value, cfg: &Value) -> Result<Option<(String, Value)>> {
    let repo = cfg["repository"].as_str().unwrap();
    let sha = job["sha"].as_str().unwrap();
    let package_manager = git(repo, &["show", &format!("{sha}:package.json")])
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["packageManager"].as_str().map(str::to_owned));
    let max = cfg["runner"]["max_parallel"].as_u64().unwrap_or(3);
    let networks = cfg["runner"]["allowed_networks"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if job["type"] == "release" {
        let path = ".kilnr/release.json";
        let raw = git(repo, &["show", &format!("{sha}:{path}")])?;
        return Ok(Some((
            path.into(),
            pipeline::load(&raw, "release", max, &networks, package_manager.as_deref())?,
        )));
    }
    let listing = git(
        repo,
        &[
            "ls-tree",
            "-r",
            "-z",
            "--name-only",
            sha,
            "--",
            ".kilnr/pipelines",
        ],
    )?;
    let source = branch(job).context("CI job does not identify a source branch")?;
    let mut matches = vec![];
    for path in listing
        .split(|b| *b == 0)
        .filter_map(|p| std::str::from_utf8(p).ok())
        .filter(|p| p.ends_with(".json"))
    {
        let raw = git(repo, &["show", &format!("{sha}:{path}")])?;
        let trigger = pipeline::load_trigger(&raw).with_context(|| path.to_owned())?;
        if pipeline::matches_branch(&json!({"trigger":trigger}), source) {
            matches.push((path.to_owned(), raw));
        }
    }
    if matches.len() > 1 {
        bail!("branch {source:?} matches multiple CI pipelines")
    };
    let Some((path, raw)) = matches.pop() else {
        return Ok(None);
    };
    Ok(Some((
        path,
        pipeline::load(&raw, "ci", max, &networks, package_manager.as_deref())?,
    )))
}

fn skeleton(job: &Value, path: Option<&str>) -> Result<(PathBuf, Value)> {
    let id = job["id"].as_str().context("missing job id")?;
    let dir = Path::new(BUILDS).join(id);
    if dir.exists() {
        bail!("build already exists")
    };
    fs::create_dir(&dir)?;
    for name in ["src", "work", "logs", "artifacts", "commands"] {
        fs::create_dir(dir.join(name))?;
    }
    let started = now();
    let mut status = json!({"schema":1,"build_id":id,"job_id":id,"project":job["project"],"sha":job["sha"],"ref":job["ref"],"type":job["type"],"event":job.get("event"),"pipeline_path":path,"pipeline":null,"prepare":{"state":"running","exit_code":null,"started_at":started,"finished_at":null,"duration_seconds":null,"log":"logs/prepare.log"},"state":"preparing","received_at":job["received_at"],"started_at":started,"finished_at":null,"duration_seconds":null});
    if let Some(v) = branch(job) {
        status["branch"] = json!(v)
    }
    if let Some(v) = job.get("tag") {
        status["tag"] = v.clone()
    }
    atomic::write_json(&dir.join("job.json"), job, 0o640)?;
    atomic::write_json(&dir.join("status.json"), &status, 0o640)?;
    Ok((dir, status))
}
fn snapshot(job: &Value, cfg: &Value, dir: &Path) -> Result<()> {
    let repo = cfg["repository"].as_str().context("missing repository")?;
    let sha = job["sha"].as_str().context("missing job sha")?;
    let mut child = Command::new("/usr/bin/git")
        .args([
            format!("--git-dir={repo}"),
            "archive".into(),
            "--format=tar".into(),
            sha.into(),
        ])
        .stdout(Stdio::piped())
        .spawn()
        .context("failed to start git archive")?;
    let stdout = child
        .stdout
        .take()
        .context("git archive stdout unavailable")?;
    let unpack = tar::Archive::new(stdout)
        .unpack(dir.join("src"))
        .context("archive extraction failed");
    let status = child.wait().context("failed to wait for git archive")?;
    unpack?;
    if !status.success() {
        bail!("git archive failed")
    }
    Ok(())
}
pub fn resolve_pipeline(
    job: &Value,
    cfg: &Value,
    path: &str,
    pipeline: &Value,
    secret_root: Option<&Path>,
) -> Result<Value> {
    if job["type"] == "release" {
        let root = secret_root.unwrap_or_else(|| Path::new(SECRET_ROOT));
        let project = job["project"].as_str().context("invalid project")?;
        for spec in pipeline["jobs"]
            .as_object()
            .context("invalid pipeline jobs")?
            .values()
        {
            for name in spec["secrets"]
                .as_array()
                .unwrap_or(&vec![])
                .iter()
                .filter_map(Value::as_str)
            {
                crate::secrets::metadata(root, project, name)?;
            }
        }
    }
    let requested = pipeline["max_parallel"].as_u64().unwrap();
    let limit = cfg["runner"]["max_parallel"].as_u64().unwrap();
    let mut v = json!({"schema":1,"build_id":job["id"],"project":job["project"],"sha":job["sha"],"ref":job["ref"],"job_type":job["type"],"event":job.get("event"),"pipeline":path,"max_parallel":requested.min(limit),"runner":{"cpus":cfg["runner"]["cpus"],"memory":cfg["runner"]["memory"],"pids_limit":cfg["runner"]["pids_limit"],"timeout_seconds":cfg["runner"]["timeout_seconds"]},"groups":pipeline["groups"],"jobs":pipeline["jobs"]});
    if let Some(b) = branch(job) {
        v["branch"] = json!(b)
    }
    if let Some(t) = job.get("tag") {
        v["tag"] = t.clone()
    }
    Ok(v)
}
pub fn write_makefile(dir: &Path, runtime: &Value) -> Result<()> {
    let jobs = runtime["jobs"].as_object().unwrap();
    let ids = jobs.keys().cloned().collect::<Vec<_>>();
    let mut text = format!(
        ".DELETE_ON_ERROR:\n.PHONY: all {}\n\nall: {}\n\n",
        ids.iter()
            .map(|v| format!("job-{v}"))
            .collect::<Vec<_>>()
            .join(" "),
        ids.iter()
            .map(|v| format!("job-{v}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    for (id, spec) in jobs {
        let deps = spec["resolved_needs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| format!("job-{}", v.as_str().unwrap()))
            .collect::<Vec<_>>()
            .join(" ");
        text += &format!(
            "job-{id}: {deps}\n\t@/usr/local/libexec/kilnr/execute {} {id}\n\n",
            runtime["build_id"].as_str().unwrap()
        );
    }
    fs::write(dir.join("pipeline.mk"), text)?;
    fs::set_permissions(dir.join("pipeline.mk"), fs::Permissions::from_mode(0o640))?;
    Ok(())
}
pub fn start_pipeline_status(mut status: Value, runtime: &Value, dir: &Path) -> Result<()> {
    let done = now();
    status["pipeline_path"] = runtime["pipeline"].clone();
    let started = status["prepare"]["started_at"].as_str().unwrap();
    status["prepare"] = json!({"state":"success","exit_code":0,"started_at":started,"finished_at":done,"duration_seconds":elapsed(started),"log":"logs/prepare.log"});
    let mut jobs = Map::new();
    for (id, spec) in runtime["jobs"].as_object().unwrap() {
        jobs.insert(id.clone(),json!({"group":spec["group"],"needs":spec["needs"],"resolved_needs":spec["resolved_needs"],"inputs":spec["inputs"],"resolved_inputs":spec["resolved_inputs"],"state":"pending","exit_code":null,"started_at":null,"finished_at":null,"duration_seconds":null,"log":format!("logs/{id}.log"),"artifacts":[],"tools":spec["tools"]}));
    }
    status["pipeline"] = json!({"groups":runtime["groups"],"jobs":jobs});
    status["state"] = json!("running");
    atomic::write_json(&dir.join("status.json"), &status, 0o640)
}
fn fail_status(dir: &Path, message: &str) -> Result<()> {
    let mut s = read(&dir.join("status.json"))?;
    let finished = now();
    let started = s["started_at"].as_str().unwrap_or(&finished).to_owned();
    s["state"] = json!("failed");
    s["finished_at"] = json!(finished);
    s["duration_seconds"] = json!(elapsed(&started));
    s["error"] = json!(message);
    atomic::write_json(&dir.join("status.json"), &s, 0o640)
}
pub fn finalize(dir: &Path, rc: i32) -> Result<String> {
    let mut s = read(&dir.join("status.json"))?;
    let mut failed = rc != 0;
    if let Some(jobs) = s["pipeline"]["jobs"].as_object_mut() {
        for j in jobs.values_mut() {
            if j["state"] == "pending" {
                j["state"] = json!("skipped");
                j["reason"] = json!("dependency_failed")
            } else if j["state"] != "success" && j["state"] != "skipped" {
                failed = true
            }
        }
    }
    let started = s["started_at"].as_str().unwrap().to_owned();
    s["state"] = json!(if failed { "failed" } else { "success" });
    s["finished_at"] = json!(now());
    s["duration_seconds"] = json!(elapsed(&started));
    let state = s["state"].as_str().unwrap().to_owned();
    atomic::write_json(&dir.join("status.json"), &s, 0o640)?;
    Ok(state)
}

pub fn initial_status(job: &Value, path: Option<&str>) -> Value {
    let started = now();
    let mut status = json!({"schema":1,"build_id":job["id"],"job_id":job["id"],"project":job["project"],"sha":job["sha"],"ref":job["ref"],"type":job["type"],"event":job.get("event"),"pipeline_path":path,"pipeline":null,"prepare":{"state":"running","exit_code":null,"started_at":started,"finished_at":null,"duration_seconds":null,"log":"logs/prepare.log"},"state":"preparing","received_at":job["received_at"],"started_at":started,"finished_at":null,"duration_seconds":null});
    if let Some(value) = branch(job) {
        status["branch"] = json!(value);
    }
    if let Some(value) = job.get("tag") {
        status["tag"] = value.clone();
    }
    status
}
fn process(path: &Path) -> Result<()> {
    let job = read(path)?;
    let config = cfg(job["project"].as_str().context("invalid project")?)?;
    let selection = select_pipeline(&job, &config)?;
    let Some((pipeline_path, pipeline)) = selection else {
        fs::remove_file(path)?;
        return Ok(());
    };
    let (dir, status) = skeleton(&job, Some(&pipeline_path))?;
    let result = (|| {
        snapshot(&job, &config, &dir)?;
        let rt = resolve_pipeline(&job, &config, &pipeline_path, &pipeline, None)?;
        atomic::write_json(&dir.join("runtime.json"), &rt, 0o640)?;
        write_makefile(&dir, &rt)?;
        start_pipeline_status(status, &rt, &dir)?;
        let log = File::create(dir.join("logs/pipeline.log"))?;
        let rc = Command::new("/usr/bin/make")
            .args([
                "-f",
                dir.join("pipeline.mk").to_str().unwrap(),
                &format!("-j{}", rt["max_parallel"]),
                "-k",
                "--output-sync=target",
                "all",
            ])
            .current_dir(&dir)
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .status()?
            .code()
            .unwrap_or(1);
        finalize(&dir, rc).map(|_| ())
    })();
    if let Err(e) = &result {
        let _ = fail_status(&dir, &e.to_string());
    }
    if job["pin_ref"] == format!("refs/kilnr/jobs/{}", job["id"].as_str().unwrap()) {
        let _ = git(
            config["repository"].as_str().unwrap(),
            &["update-ref", "-d", job["pin_ref"].as_str().unwrap()],
        );
    }
    let _ = Command::new("/usr/local/libexec/kilnr/notify-discord")
        .arg(job["id"].as_str().unwrap())
        .status();
    let _ = fs::remove_file(path);
    result
}

pub fn controller() -> Result<()> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open("/var/lib/kilnr/locks/controller.lock")?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(());
    }
    loop {
        let mut jobs = fs::read_dir(INCOMING)?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect::<Vec<_>>();
        jobs.sort();
        let Some(source) = jobs.first() else { break };
        let destination = Path::new(RUNNING).join(source.file_name().unwrap());
        match fs::rename(source, &destination) {
            Ok(()) => {
                if let Err(e) = process(&destination) {
                    eprintln!("kilnr controller: {e:#}")
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        fs::remove_dir_all(dst)?
    }
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let meta = fs::symlink_metadata(&from)?;
        if meta.file_type().is_symlink() {
            let target = fs::read_link(&from)?;
            std::os::unix::fs::symlink(target, to)?
        } else if meta.is_dir() {
            copy_tree(&from, &to)?
        } else if meta.is_file() {
            fs::copy(&from, &to)?;
            fs::set_permissions(&to, meta.permissions())?
        }
    }
    Ok(())
}
pub fn execute(args: &[String]) -> Result<()> {
    if args.len() != 2 {
        bail!("usage: execute <build-id> <job-id>")
    }
    let (build_id, id) = (&args[0], &args[1]);
    if !Regex::new(r"^[A-Za-z0-9_.-]+$").unwrap().is_match(build_id)
        || !Regex::new(r"^[a-z0-9][a-z0-9_-]{0,62}$")
            .unwrap()
            .is_match(id)
    {
        bail!("invalid build/job id")
    }
    let dir = Path::new(BUILDS).join(build_id);
    let rt = read(&dir.join("runtime.json"))?;
    if rt["build_id"] != *build_id {
        bail!("runtime build id mismatch")
    }
    let job = &rt["jobs"][id];
    if job.is_null() {
        bail!("unknown job")
    };
    let started = now();
    runtime_helpers::update_job(
        &dir,
        id,
        &json!({"state":"running","started_at":started,"finished_at":null,"duration_seconds":null,"exit_code":null}),
    )?;
    let work = dir.join("work").join(id);
    copy_tree(&dir.join("src"), &work)?;
    let log_path = dir.join("logs").join(format!("{id}.log"));
    let mut log = File::create(&log_path)?;
    writeln!(
        log,
        "build: {build_id}\njob: {id}\nimage: {}\nnetwork: {}\n",
        job["image"], job["network"]
    )?;
    let mut mounts = vec![format!("type=bind,src={},dst=/workspace", work.display())];
    let (execution_mounts, mut argv) = runtime_helpers::prepare_execution(&dir, id, job)?;
    mounts.extend(execution_mounts);
    let inputs = job["resolved_inputs"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let roots = artifacts::input_roots(&dir, &inputs)?;
    mounts.extend(runtime_helpers::build_input_mounts(&roots));
    if let Some(tools) = job["tools"].as_object() {
        let (tool_mounts, wrapped) =
            runtime_helpers::prepare_tools_wrapper(&dir, id, tools, &argv)?;
        mounts.extend(tool_mounts);
        argv = wrapped;
    }
    mounts.extend(runtime_helpers::prepare_cache_mounts(
        Path::new("/var/lib/kilnr/cache"),
        &rt,
        job,
    )?);
    let secret_stage = runtime_helpers::prepare_secret_stage(
        Path::new(SECRET_ROOT),
        Path::new("/var/lib/kilnr/secret-staging"),
        build_id,
        id,
        &rt,
        job,
    )?;
    if let Some(stage) = secret_stage.path.as_ref() {
        let wrapper = dir.join("commands").join(format!("{id}.secrets.sh"));
        fs::write(
            &wrapper,
            runtime_helpers::render_secret_wrapper(&secret_stage.metadata)?,
        )?;
        mounts.push(format!(
            "type=bind,src={},dst=/run/kilnr/secrets,readonly",
            stage.display()
        ));
        mounts.push(format!(
            "type=bind,src={},dst=/run/kilnr/secret-wrapper.sh,readonly",
            wrapper.display()
        ));
        let mut wrapped = vec!["/bin/sh".into(), "/run/kilnr/secret-wrapper.sh".into()];
        wrapped.extend(argv);
        argv = wrapped;
    }
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    let mut docker = Command::new("/usr/bin/docker");
    docker.args([
        "run",
        "--rm",
        "--name",
        &format!(
            "kilnr-{}-{id}",
            &build_id[build_id.len().saturating_sub(32)..]
        ),
        "--init",
        "--pull=missing",
        "--network",
        job["network"].as_str().unwrap(),
        "--cpus",
        rt["runner"]["cpus"].as_str().unwrap(),
        "--memory",
        rt["runner"]["memory"].as_str().unwrap(),
        "--pids-limit",
        &rt["runner"]["pids_limit"].to_string(),
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges=true",
        "--user",
        &format!("{uid}:{gid}"),
        "--workdir",
        "/workspace",
        "--tmpfs",
        "/tmp:rw,nosuid,nodev,noexec,size=512m",
        "--tmpfs",
        &format!(
            "/run/kilnr/tmp:rw,nosuid,nodev,noexec,size=512m,mode=0700,uid={uid},gid={gid}"
        ),
    ]);
    for (key, value) in runtime_helpers::build_public_env(&rt, id, job, &roots)? {
        docker.args(["--env", &format!("{key}={value}")]);
    }
    for m in mounts {
        docker.args(["--mount", &m]);
    }
    docker
        .arg(job["image"].as_str().unwrap())
        .args(argv)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = docker.spawn()?;
    let timeout = Duration::from_secs(rt["runner"]["timeout_seconds"].as_u64().unwrap());
    let clock = Instant::now();
    let rc = loop {
        if let Some(status) = child.try_wait()? {
            break status.code().unwrap_or(1);
        }
        if clock.elapsed() > timeout {
            let _ = Command::new("/usr/bin/docker")
                .args([
                    "rm",
                    "-f",
                    &format!(
                        "kilnr-{}-{id}",
                        &build_id[build_id.len().saturating_sub(32)..]
                    ),
                ])
                .status();
            break 124;
        }
        thread::sleep(Duration::from_millis(100))
    };
    let output = child.wait_with_output()?;
    let tokens = runtime_helpers::redaction_tokens(&secret_stage.redaction_values);
    let bytes = runtime_helpers::redact_text(
        String::from_utf8_lossy(&[output.stdout, output.stderr].concat()).into_owned(),
        &tokens,
    )
    .into_bytes();
    log.write_all(&bytes)?;
    let collected = if rc == 0 {
        runtime_helpers::collect_job_artifacts(&dir, &work, id, job)?
    } else {
        vec![]
    };
    let finished = now();
    runtime_helpers::update_job(
        &dir,
        id,
        &json!({"state":if rc==0{"success"}else{"failed"},"exit_code":rc,"finished_at":finished,"duration_seconds":elapsed(&started),"artifacts":collected}),
    )?;
    if rc != 0 {
        std::process::exit(rc)
    }
    Ok(())
}