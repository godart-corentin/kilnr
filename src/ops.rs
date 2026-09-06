use crate::{atomic, project_lock, project_rename as rename, retention, secrets, web};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::{json, Value};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const BUILDS: &str = "/var/lib/kilnr/builds";
const CONFIG: &str = "/etc/kilnr/projects";
const GIT: &str = "/srv/git";
const SECRET_ROOT: &str = "/etc/kilnr/secrets";
const LOCKS: &str = "/var/lib/kilnr/locks/projects";
const LIBEXEC: &str = "/usr/local/libexec/kilnr";

fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_slice(
        &fs::read(path).with_context(|| format!("file not found: {}", path.display()))?,
    )
    .with_context(|| format!("invalid JSON: {}", path.display()))
}
fn valid_build(id: &str) -> bool {
    Regex::new(r"^[A-Za-z0-9_.-]+$").unwrap().is_match(id)
}
fn valid_oid(id: &str) -> bool {
    Regex::new(r"^(?:[0-9a-f]{40}|[0-9a-f]{64})$")
        .unwrap()
        .is_match(id)
}

pub fn classify_job(
    config: &Value,
    old: &str,
    new: &str,
    reference: &str,
) -> Result<Option<String>> {
    if reference.starts_with("refs/heads/") {
        return Ok(Some("ci".into()));
    }
    if let Some(tag) = reference.strip_prefix("refs/tags/") {
        let initial = old.chars().all(|character| character == '0');
        let pattern = config["release"]["tag_pattern"]
            .as_str()
            .context("release.tag_pattern is missing")?;
        if initial
            && !new.chars().all(|character| character == '0')
            && Regex::new(pattern)?.is_match(tag)
        {
            return Ok(Some("release".into()));
        }
    }
    Ok(None)
}

pub fn new_project_config(
    project: &str,
    repository: &Path,
    webhook: &Path,
    defaults: &Value,
) -> Result<Value> {
    project_lock::validate_name(project)?;
    let runner = defaults.get("runner").context("defaults runner missing")?;
    let mut config = json!({
        "schema": 1,
        "project": project,
        "repository": repository,
        "release": {"tag_pattern": r"^v[0-9]+\.[0-9]+\.[0-9]+$"},
        "runner": runner,
        "discord": {"webhook_file": webhook},
    });
    if let Some(retention) = defaults.get("retention") {
        config["retention"] = retention.clone();
    }
    Ok(config)
}

pub fn receive_project(git_root: &Path, repository: &Path) -> Result<String> {
    let canonical = repository.canonicalize()?;
    let root = git_root.canonicalize()?;
    if canonical.parent() != Some(root.as_path())
        || canonical.extension() != Some(OsStr::new("git"))
    {
        bail!("receive repository is outside {}", root.display())
    }
    let project = canonical
        .file_stem()
        .context("receive repository has no project name")?
        .to_string_lossy()
        .into_owned();
    project_lock::validate_name(&project)?;
    Ok(project)
}
fn root_only() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("must run as root")
    }
    Ok(())
}
fn privileged(helper: &str, args: &[String], input: Option<&[u8]>) -> Result<()> {
    let path = format!("{LIBEXEC}/{helper}");
    let mut command = if unsafe { libc::geteuid() } == 0 {
        Command::new(path)
    } else {
        let mut c = Command::new("/usr/bin/sudo");
        c.arg(path);
        c
    };
    command.args(args);
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn()?;
    if let Some(bytes) = input {
        child.stdin.take().unwrap().write_all(bytes)?;
    }
    let status = child.wait()?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1))
    }
    Ok(())
}

pub fn cli(args: &[String]) -> Result<()> {
    match args {
        [cmd, rest @ ..] if cmd == "cleanup" => privileged("cleanup", rest, None),
        [a, b, name] if a == "project" && b == "create" => privileged(
            "project-lock-run",
            &[
                "--exclusive".into(),
                name.clone(),
                "--".into(),
                format!("{LIBEXEC}/project-create"),
                name.clone(),
            ],
            None,
        ),
        [a, b, name] if a == "project" && b == "delete" => {
            println!(
                "This will permanently delete the Git repository and Kilnr configuration for '{name}'.\nExisting build history, logs and artifacts will be preserved.\nType '{name}' to confirm:"
            );
            let mut answer = String::new();
            io::stdin().read_line(&mut answer)?;
            if answer.trim() != name {
                bail!("deletion cancelled")
            }
            privileged("project-delete", std::slice::from_ref(name), None)
        }
        [a, b, old, new] if a == "project" && b == "rename" => {
            privileged("project-rename", &[old.clone(), new.clone()], None)
        }
        [a, b, c, project] if a == "project" && b == "webhook" && c == "set" => {
            let value = rpassword::prompt_password("Discord webhook URL: ")?;
            privileged(
                "project-webhook-set",
                std::slice::from_ref(project),
                Some(format!("{value}\n").as_bytes()),
            )
        }
        [a, b, project, name] if a == "secret" && b == "set" => {
            let value = rpassword::prompt_password("Secret value: ")?;
            if value.is_empty() {
                bail!("secret cannot be empty")
            }
            privileged(
                "secret-set",
                &[project.clone(), name.clone()],
                Some(value.as_bytes()),
            )
        }
        [a, b, project, name, path] if a == "secret" && b == "set-file" => privileged(
            "secret-set-file",
            &[project.clone(), name.clone(), path.clone()],
            None,
        ),
        [a, b, project] if a == "secret" && b == "list" => {
            privileged("secret-list", std::slice::from_ref(project), None)
        }
        [a, b, project, name] if a == "secret" && b == "delete" => {
            privileged("secret-delete", &[project.clone(), name.clone()], None)
        }
        [a, b] if a == "git-key" && b == "add" => {
            println!("Paste one Ed25519 SSH public key:");
            let mut value = String::new();
            io::stdin().read_line(&mut value)?;
            privileged("git-key-add", &[], Some(value.as_bytes()))
        }
        [cmd] if cmd == "doctor" => privileged("doctor", &[], None),
        [cmd, query] if cmd == "status" => show_status(&resolve_build(query)?),
        [cmd, query] if cmd == "logs" => show_logs(&resolve_build(query)?, None),
        [cmd, query, job] if cmd == "logs" => show_logs(&resolve_build(query)?, Some(job)),
        [cmd, query, job] if cmd == "watch" => watch(&resolve_build(query)?, job),
        [cmd, query] if cmd == "rerun" => {
            let build = resolve_build(query)?;
            privileged(
                "rerun",
                &[build.file_name().unwrap().to_string_lossy().into()],
                None,
            )
        }
        _ => {
            eprintln!(
                "Usage:\n  kilnr status <latest|build-id|sha-prefix>\n  kilnr logs <latest|build-id|sha-prefix> [job]\n  kilnr watch <latest|build-id|sha-prefix> <job|pipeline>\n  kilnr rerun <latest|build-id|sha-prefix>\n  kilnr cleanup [--dry-run] [--project <project>]\n\n  kilnr project create|delete <project-name>\n  kilnr project rename <old-name> <new-name>\n  kilnr project webhook set <project-name>\n\n  kilnr secret set|delete <project> <name>\n  kilnr secret set-file <project> <name> <path>\n  kilnr secret list <project>\n\n  kilnr git-key add"
            );
            std::process::exit(2)
        }
    }
}

fn build_dirs() -> Vec<PathBuf> {
    let mut values = fs::read_dir(BUILDS)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            !p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .starts_with('.')
                && fs::symlink_metadata(p).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
        })
        .collect::<Vec<_>>();
    values.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    values
}
fn resolve_build(query: &str) -> Result<PathBuf> {
    let values = build_dirs();
    if values.is_empty() {
        bail!("no builds found")
    }
    if query == "latest" {
        return Ok(values[0].clone());
    }
    let exact = Path::new(BUILDS).join(query);
    if exact.is_dir() {
        return Ok(exact);
    }
    for path in values {
        if path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(query)
            || read_json(&path.join("status.json"))
                .ok()
                .and_then(|s| s["sha"].as_str().map(|v| v.starts_with(query)))
                .unwrap_or(false)
        {
            return Ok(path);
        }
    }
    bail!("no build matches {query:?}")
}
fn duration(v: &Value) -> String {
    v.as_f64()
        .map(|n| {
            let s = n.round() as u64;
            if s >= 60 {
                format!("{}m{:02}s", s / 60, s % 60)
            } else {
                format!("{s}s")
            }
        })
        .unwrap_or_else(|| "-".into())
}
fn short_ref(value: &str) -> &str {
    value
        .strip_prefix("refs/heads/")
        .or_else(|| value.strip_prefix("refs/tags/"))
        .unwrap_or(value)
}
pub fn format_status(status: &Value) -> String {
    let s = status;
    let mut text = format!(
        "Build:    {}\nProject:  {}\nRef:      {}\nSHA:      {}\nType:     {}\nState:    {}\nDuration: {}\n",
        s["build_id"],
        s["project"],
        short_ref(s["ref"].as_str().unwrap_or("")),
        s["sha"],
        s["type"],
        s["state"],
        duration(&s["duration_seconds"])
    );
    if let Some(jobs) = s["pipeline"]["jobs"].as_object() {
        for (name, job) in jobs {
            text.push_str(&format!(
                "{:<4}  {:<12} {} ({})\n",
                marker(job["state"].as_str().unwrap_or("?")),
                job["group"].as_str().unwrap_or("-"),
                name,
                duration(&job["duration_seconds"])
            ));
        }
    }
    text
}

fn show_status(build: &Path) -> Result<()> {
    let s = read_json(&build.join("status.json"))?;
    print!("{}", format_status(&s));
    Ok(())
}
fn marker(s: &str) -> &str {
    match s {
        "success" => "OK",
        "failed" => "FAIL",
        "skipped" => "SKIP",
        "running" => "RUN",
        "pending" => "WAIT",
        "aborted" => "ABRT",
        _ => "?",
    }
}
fn show_logs(build: &Path, job: Option<&String>) -> Result<()> {
    let logs = build.join("logs");
    if let Some(j) = job {
        if !Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$")
            .unwrap()
            .is_match(j)
        {
            bail!("invalid log name")
        }
        print!(
            "{}",
            String::from_utf8_lossy(&fs::read(logs.join(format!("{j}.log")))?)
        );
        return Ok(());
    }
    let mut paths = fs::read_dir(logs)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|v| v == "log"))
        .collect::<Vec<_>>();
    paths.sort();
    if paths.is_empty() {
        bail!("no logs found")
    }
    for p in paths {
        println!("===== {} =====", p.file_name().unwrap().to_string_lossy());
        print!("{}", String::from_utf8_lossy(&fs::read(p)?));
    }
    Ok(())
}
pub fn terminal(build: &Path, target: &str) -> Result<Option<String>> {
    let s = read_json(&build.join("status.json"))?;
    let state = if target == "pipeline" {
        s["state"].as_str()
    } else {
        s["pipeline"]["jobs"][target]["state"].as_str()
    };
    Ok(state
        .filter(|v| matches!(*v, "success" | "failed" | "skipped" | "aborted"))
        .map(str::to_owned))
}
fn watch(build: &Path, target: &str) -> Result<()> {
    let path = build.join("logs").join(format!("{target}.log"));
    let mut offset = 0;
    loop {
        if let Ok(bytes) = fs::read(&path) {
            if bytes.len() > offset {
                io::stdout().write_all(&bytes[offset..])?;
                io::stdout().flush()?;
                offset = bytes.len();
            }
        }
        if let Some(state) = terminal(build, target)? {
            eprintln!("kilnr: {target}: {state}");
            if state != "success" {
                bail!("{target} failed")
            }
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250))
    }
}

pub fn helper(name: &str, args: &[String]) -> Result<()> {
    match name {
        "web" => {
            if args == ["--healthcheck"] {
                web::healthcheck()
            } else {
                web::serve()
            }
        }
        "enqueue" => enqueue(args),
        "secret-set" => secret_set(args, false),
        "secret-set-file" => secret_set(args, true),
        "secret-list" => secret_list(args),
        "secret-delete" => secret_delete(args),
        "project-lock-run" => lock_run(args),
        "project-webhook-set" => webhook(args),
        "project-delete" => project_delete(args),
        "project-create" => project_create(args),
        "project-rename" => project_rename(args),
        "rerun" => rerun(args),
        "git-key-add" => git_key_add(),
        "cleanup" => cleanup(args),
        "notify-discord" => notify(args),
        "controller" => crate::ops_runtime::controller(),
        "execute" => crate::ops_runtime::execute(args),
        "permissions" => crate::permissions::helper(args),
        "config-tool" => config_tool(args),
        _ => bail!("unknown helper: {name}"),
    }
}

fn project_create(args: &[String]) -> Result<()> {
    root_only()?;
    if args.len() != 1 {
        bail!("usage: project-create <project-name>")
    }
    let project = project_lock::validate_name(&args[0])?;
    let repo = Path::new(GIT).join(format!("{project}.git"));
    let config_path = Path::new(CONFIG).join(format!("{project}.json"));
    let webhook = Path::new(SECRET_ROOT).join(format!("{project}.discord-webhook"));
    let secret_dir = Path::new(SECRET_ROOT).join(project);
    for path in [&repo, &config_path, &webhook, &secret_dir] {
        if path.exists() {
            bail!("destination already exists: {}", path.display())
        }
    }
    fs::create_dir(&repo)?;
    let result = (|| -> Result<()> {
        let status = Command::new("/usr/sbin/runuser")
            .args([
                "-u",
                "git",
                "--",
                "git",
                "init",
                "--bare",
                "--initial-branch=main",
                repo.to_str().unwrap(),
            ])
            .status()?;
        if !status.success() {
            bail!("git init failed")
        }
        for pair in [
            ["config", "transfer.hideRefs", "refs/kilnr/"],
            ["config", "gc.packRefs", "false"],
        ] {
            let mut command = Command::new("/usr/sbin/runuser");
            command
                .args(["-u", "git", "--", "git"])
                .arg(format!("--git-dir={}", repo.display()))
                .args(pair);
            if !command.status()?.success() {
                bail!("git config failed")
            }
        }
        fs::create_dir_all(repo.join("refs/kilnr/jobs"))?;
        Command::new("setfacl")
            .args(["-R", "-m", "u:kilnr:rX", repo.to_str().unwrap()])
            .status()?;
        std::os::unix::fs::symlink(
            "/usr/local/libexec/kilnr/git-hooks/post-receive",
            repo.join("hooks/post-receive"),
        )?;
        let defaults = read_json(Path::new("/etc/kilnr/defaults.json"))?;
        let config = new_project_config(project, &repo, &webhook, &defaults)?;
        atomic::write_json(&config_path, &config, 0o644)?;
        fs::create_dir(&secret_dir)?;
        fs::set_permissions(&secret_dir, fs::Permissions::from_mode(0o750))?;
        atomic::write(&webhook, b"", 0o640)?;
        project_lock::provision(Path::new(LOCKS), &[project.into()])?;
        println!(
            "Created Kilnr project: {project}\nGit remote: ssh://git@<host>/srv/git/{project}.git"
        );
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&config_path);
        let _ = fs::remove_file(&webhook);
        let _ = fs::remove_dir_all(&secret_dir);
        let _ = fs::remove_dir_all(&repo);
    }
    result
}

fn config_tool(args: &[String]) -> Result<()> {
    match args {
        [cmd, cidr] if cmd == "gateway" => {
            let (address, prefix) = cidr.split_once('/').context("invalid IPv4 CIDR")?;
            let octets = address
                .split('.')
                .map(str::parse::<u8>)
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let prefix: u8 = prefix.parse()?;
            if octets.len() != 4 || prefix > 28 {
                bail!("Kilnr CI subnet must be an IPv4 network of /28 or larger")
            }
            let raw = u32::from_be_bytes([octets[0], octets[1], octets[2], octets[3]]);
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - prefix)
            };
            println!("{}", std::net::Ipv4Addr::from((raw & mask) + 1));
            Ok(())
        }
        [cmd, path] if cmd == "strip-caddy" => {
            let text = fs::read_to_string(path)?;
            let re = Regex::new(r"(?s)\n?# BEGIN KILNR\n.*?# END KILNR\n?")?;
            fs::write(
                path,
                format!("{}\n", re.replace_all(&text, "\n").trim_end()),
            )?;
            Ok(())
        }
        [cmd, path] if cmd == "write-caddy" => {
            let domain = std::env::var("DOMAIN")?;
            let user = std::env::var("AUTH_USER")?;
            let hash = std::env::var("CADDY_HASH")?;
            let directive = std::env::var("AUTH_DIRECTIVE")?;
            let text = fs::read_to_string(path)?;
            let re = Regex::new(r"(?s)\n?# BEGIN KILNR\n.*?# END KILNR\n?")?;
            let clean = re.replace_all(&text, "\n");
            let block = format!(
                "# BEGIN KILNR\n{domain} {{\n    @health path /healthz\n    handle @health {{\n        reverse_proxy kilnr-web:8088\n    }}\n\n    handle {{\n        {directive} {{\n            {user} {hash}\n        }}\n\n        encode zstd gzip\n        reverse_proxy kilnr-web:8088\n    }}\n}}\n# END KILNR\n"
            );
            fs::write(path, format!("{}\n\n{block}", clean.trim_end()))?;
            Ok(())
        }
        [cmd, path, service] if cmd == "compose-networks" => {
            let data = read_json(Path::new(path))?;
            let networks = &data["services"][service]["networks"];
            let mut names = if let Some(o) = networks.as_object() {
                o.keys().cloned().collect::<Vec<_>>()
            } else if let Some(a) = networks.as_array() {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            } else {
                bail!("invalid merged Caddy networks")
            };
            names.sort();
            for n in names {
                println!("{n}")
            }
            Ok(())
        }
        _ => bail!("usage: config-tool gateway|strip-caddy|write-caddy|compose-networks ..."),
    }
}

fn secret_set(args: &[String], file: bool) -> Result<()> {
    root_only()?;
    if args.len() != if file { 3 } else { 2 } {
        bail!("usage: secret-set[-file] <project> <name> [path]")
    }
    let _lock = project_lock::ProjectLocks::acquire(
        Path::new(LOCKS),
        &[args[0].clone()],
        project_lock::Mode::Shared,
        false,
    )?;
    if !Path::new(CONFIG)
        .join(format!("{}.json", args[0]))
        .is_file()
    {
        bail!("project does not exist: {}", args[0])
    }
    let data = if file {
        fs::read(Path::new(&args[2]).canonicalize()?)?
    } else {
        let mut b = vec![];
        io::stdin().take(1024 * 1024 + 1).read_to_end(&mut b)?;
        b
    };
    secrets::store(
        Path::new(SECRET_ROOT),
        &args[0],
        &args[1],
        &data,
        if file { "file" } else { "text" },
    )?;
    println!("Secret {} configured for {}", args[1], args[0]);
    Ok(())
}
fn secret_list(args: &[String]) -> Result<()> {
    root_only()?;
    if args.len() != 1 {
        bail!("usage: secret-list <project>")
    }
    for (n, m) in secrets::list(Path::new(SECRET_ROOT), &args[0])? {
        println!("{n}\t{}\t{}", m.scope, m.kind)
    }
    Ok(())
}
fn secret_delete(args: &[String]) -> Result<()> {
    root_only()?;
    if args.len() != 2 {
        bail!("usage: secret-delete <project> <name>")
    }
    let _lock = project_lock::ProjectLocks::acquire(
        Path::new(LOCKS),
        &[args[0].clone()],
        project_lock::Mode::Shared,
        false,
    )?;
    secrets::delete(Path::new(SECRET_ROOT), &args[0], &args[1])?;
    println!("Secret {} deleted for {}", args[1], args[0]);
    Ok(())
}
fn lock_run(args: &[String]) -> Result<()> {
    root_only()?;
    let code = run_under_lock(Path::new(LOCKS), args)?;
    std::process::exit(code)
}

pub fn run_under_lock(root: &Path, args: &[String]) -> Result<i32> {
    use std::os::unix::process::ExitStatusExt;
    let split = args
        .iter()
        .position(|v| v == "--")
        .context("usage: project-lock-run [--exclusive] <project> -- <command>")?;
    let (exclusive, names) = if args.first().is_some_and(|v| v == "--exclusive") {
        (true, &args[1..split])
    } else {
        (false, &args[..split])
    };
    let owned_names = names.to_vec();
    project_lock::provision(root, &owned_names)?;
    let _locks = project_lock::ProjectLocks::acquire(
        root,
        names,
        if exclusive {
            project_lock::Mode::Exclusive
        } else {
            project_lock::Mode::Shared
        },
        false,
    )?;
    let status = Command::new(&args[split + 1])
        .args(&args[split + 2..])
        .status()?;
    Ok(status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)))
}
fn webhook(args: &[String]) -> Result<()> {
    root_only()?;
    if args.len() != 1 {
        bail!("usage: project-webhook-set <project>")
    }
    project_lock::validate_name(&args[0])?;
    let _lock = project_lock::ProjectLocks::acquire(
        Path::new(LOCKS),
        args,
        project_lock::Mode::Shared,
        false,
    )?;
    let cfg = read_json(&Path::new(CONFIG).join(format!("{}.json", args[0])))?;
    let expected = Path::new(SECRET_ROOT).join(format!("{}.discord-webhook", args[0]));
    if cfg["project"] != args[0]
        || cfg["discord"]["webhook_file"] != expected.to_string_lossy().as_ref()
    {
        bail!("project configuration mismatch")
    }
    let mut input = String::new();
    io::stdin().take(8193).read_to_string(&mut input)?;
    let value = input.trim();
    let valid_webhook = Regex::new(
        r"^https://(?:discord\.com|canary\.discord\.com|ptb\.discord\.com|discordapp\.com)/api/webhooks/[0-9]+/[^/\s]+/?$",
    )?;
    if !valid_webhook.is_match(value) {
        bail!("invalid Discord webhook URL")
    }
    atomic::write(&expected, format!("{value}\n").as_bytes(), 0o640)?;
    println!("Discord webhook configured for {}", args[0]);
    Ok(())
}
fn active_jobs(project: &str) -> Vec<String> {
    ["incoming", "running"]
        .into_iter()
        .flat_map(|q| {
            fs::read_dir(format!("/var/lib/kilnr/queue/{q}"))
                .into_iter()
                .flatten()
                .flatten()
        })
        .filter_map(|e| {
            read_json(&e.path())
                .ok()
                .filter(|v| v["project"] == project)
                .map(|_| e.file_name().to_string_lossy().into())
        })
        .collect()
}
fn project_delete(args: &[String]) -> Result<()> {
    root_only()?;
    if args.len() != 1 {
        bail!("usage: project-delete <project>")
    }
    project_lock::validate_name(&args[0])?;
    let _lock = project_lock::ProjectLocks::acquire(
        Path::new(LOCKS),
        args,
        project_lock::Mode::Exclusive,
        false,
    )?;
    let cfg_path = Path::new(CONFIG).join(format!("{}.json", args[0]));
    let cfg = read_json(&cfg_path)?;
    let repo = Path::new(GIT).join(format!("{}.git", args[0]));
    if cfg["project"] != args[0] || cfg["repository"] != repo.to_string_lossy().as_ref() {
        bail!("project configuration mismatch")
    }
    if !active_jobs(&args[0]).is_empty() {
        bail!("project still has active jobs")
    }
    if repo.exists() {
        if fs::symlink_metadata(&repo)?.file_type().is_symlink()
            || !["HEAD", "objects", "refs"]
                .iter()
                .all(|p| repo.join(p).exists())
        {
            bail!("refusing deletion: path is not a bare Git repository")
        }
        fs::remove_dir_all(repo)?
    }
    fs::remove_file(cfg_path)?;
    let _ = fs::remove_file(Path::new(SECRET_ROOT).join(format!("{}.discord-webhook", args[0])));
    let dir = Path::new(SECRET_ROOT).join(&args[0]);
    if dir.exists() {
        fs::remove_dir_all(dir)?
    }
    println!(
        "Deleted Kilnr project: {}\nBuild history was preserved.",
        args[0]
    );
    Ok(())
}
fn rewrite_json_paths(value: &mut Value, old: &str, new: &str) {
    match value {
        Value::String(s) => {
            *s = s
                .replace(&format!("/{old}.git"), &format!("/{new}.git"))
                .replace(&format!("/{old}.json"), &format!("/{new}.json"))
                .replace(
                    &format!("/{old}.discord-webhook"),
                    &format!("/{new}.discord-webhook"),
                );
            if s == old {
                *s = new.into()
            }
        }
        Value::Array(a) => {
            for v in a {
                rewrite_json_paths(v, old, new)
            }
        }
        Value::Object(o) => {
            for v in o.values_mut() {
                rewrite_json_paths(v, old, new)
            }
        }
        _ => {}
    }
}

#[derive(Clone)]
struct RenameMove {
    source: PathBuf,
    destination: PathBuf,
}

#[derive(Clone)]
struct RenameWrite {
    path: PathBuf,
    before: Vec<u8>,
    after: Vec<u8>,
    mode: u32,
}

fn checked_move(source: PathBuf, destination: PathBuf, moves: &mut Vec<RenameMove>) -> Result<()> {
    if destination.exists() {
        bail!("destination already exists: {}", destination.display())
    }
    let source_meta = fs::symlink_metadata(&source)?;
    if source_meta.file_type().is_symlink() {
        bail!("refusing symlink: {}", source.display())
    }
    let source_device = source_meta.dev();
    let parent_device =
        fs::metadata(destination.parent().context("destination has no parent")?)?.dev();
    if source_device != parent_device {
        bail!(
            "cross-filesystem rename is not atomic: {}",
            source.display()
        )
    }
    moves.push(RenameMove {
        source,
        destination,
    });
    Ok(())
}

fn project_rename(args: &[String]) -> Result<()> {
    if args.len() != 2 {
        bail!("usage: project-rename <old> <new>")
    }
    root_only()?;
    project_lock::validate_name(&args[0])?;
    project_lock::validate_name(&args[1])?;
    project_lock::provision(Path::new(LOCKS), &[args[1].clone()])?;
    let _locks = project_lock::ProjectLocks::acquire(
        Path::new(LOCKS),
        args,
        project_lock::Mode::Exclusive,
        false,
    )?;
    let roots = rename::Roots {
        git: PathBuf::from(GIT),
        config: PathBuf::from(CONFIG),
        secrets: PathBuf::from(SECRET_ROOT),
        state: PathBuf::from("/var/lib/kilnr"),
        locks: PathBuf::from(LOCKS),
        managed_hook: Some(PathBuf::from(format!("{LIBEXEC}/git-hooks/post-receive"))),
    };
    let inventory = rename::inventory_rename(&roots, &args[0], &args[1])?;
    let prepared = rename::prepare_rename(inventory)?;
    rename::commit_rename(&prepared)?;
    println!("Renamed Kilnr project: {} -> {}", args[0], args[1]);
    Ok(())
}

#[allow(dead_code)]
fn project_rename_legacy(args: &[String]) -> Result<()> {
    root_only()?;
    if args.len() != 2 {
        bail!("usage: project-rename <old> <new>")
    }
    for n in args {
        project_lock::validate_name(n)?;
    }
    if args[0] == args[1] {
        bail!("old and new project names must be different")
    }
    project_lock::provision(Path::new(LOCKS), &[args[1].clone()])?;
    let _locks = project_lock::ProjectLocks::acquire(
        Path::new(LOCKS),
        args,
        project_lock::Mode::Exclusive,
        false,
    )?;
    if !active_jobs(&args[0]).is_empty() {
        bail!("source project still has active jobs")
    }
    let old_repo = Path::new(GIT).join(format!("{}.git", args[0]));
    let new_repo = Path::new(GIT).join(format!("{}.git", args[1]));
    let old_cfg = Path::new(CONFIG).join(format!("{}.json", args[0]));
    let new_cfg = Path::new(CONFIG).join(format!("{}.json", args[1]));
    if !old_cfg.is_file() || !old_repo.is_dir() {
        bail!("source project is incomplete")
    }
    if new_cfg.exists() || new_repo.exists() {
        bail!("destination project already exists")
    }
    let mut moves = vec![];
    let mut writes = vec![];
    checked_move(old_repo, new_repo, &mut moves)?;
    checked_move(old_cfg.clone(), new_cfg.clone(), &mut moves)?;
    for (old, new) in [
        (
            Path::new(SECRET_ROOT).join(format!("{}.discord-webhook", args[0])),
            Path::new(SECRET_ROOT).join(format!("{}.discord-webhook", args[1])),
        ),
        (
            Path::new(SECRET_ROOT).join(&args[0]),
            Path::new(SECRET_ROOT).join(&args[1]),
        ),
        (
            Path::new("/var/lib/kilnr/cache").join(&args[0]),
            Path::new("/var/lib/kilnr/cache").join(&args[1]),
        ),
    ] {
        if old.exists() {
            checked_move(old, new, &mut moves)?
        }
    }
    let build_pattern = Regex::new(&format!(
        r"^(\d{{8}}T\d{{12}}Z)-{}-([0-9a-f]{{7}})-([0-9a-f]{{8}})$",
        regex::escape(&args[0])
    ))?;
    for entry in fs::read_dir(BUILDS)? {
        let path = entry?.path();
        let Some(id) = path.file_name().and_then(|v| v.to_str()) else {
            continue;
        };
        let Some(captures) = build_pattern.captures(id) else {
            continue;
        };
        let new_id = format!(
            "{}-{}-{}-{}",
            &captures[1], args[1], &captures[2], &captures[3]
        );
        let destination = Path::new(BUILDS).join(&new_id);
        checked_move(path.clone(), destination.clone(), &mut moves)?;
        for filename in ["job.json", "status.json", "runtime.json"] {
            let source = path.join(filename);
            if !source.exists() {
                continue;
            }
            let before = fs::read(&source)?;
            let mut value: Value = serde_json::from_slice(&before)?;
            if value.get("project").and_then(Value::as_str) != Some(&args[0]) {
                bail!("project identity mismatch in {}", source.display())
            }
            rewrite_json_paths(&mut value, &args[0], &args[1]);
            for key in ["id", "job_id", "build_id"] {
                if value.get(key).and_then(Value::as_str) == Some(id) {
                    value[key] = json!(new_id)
                }
            }
            if let Some(pin) = value.get_mut("pin_ref") {
                if pin.as_str() == Some(&format!("refs/kilnr/jobs/{id}")) {
                    *pin = json!(format!("refs/kilnr/jobs/{new_id}"))
                }
            }
            let mut after = serde_json::to_vec_pretty(&value)?;
            after.push(b'\n');
            writes.push(RenameWrite {
                path: destination.join(filename),
                before,
                after,
                mode: 0o640,
            });
        }
        let make = path.join("pipeline.mk");
        if make.exists() {
            let before = fs::read(&make)?;
            let text = String::from_utf8(before.clone())?;
            let after = text
                .replace(&format!("/execute {id} "), &format!("/execute {new_id} "))
                .into_bytes();
            writes.push(RenameWrite {
                path: destination.join("pipeline.mk"),
                before,
                after,
                mode: 0o640,
            });
        }
    }
    let mut config = read_json(&old_cfg)?;
    rewrite_json_paths(&mut config, &args[0], &args[1]);
    let config_before = fs::read(&old_cfg)?;
    let mut config_after = serde_json::to_vec_pretty(&config)?;
    config_after.push(b'\n');
    writes.push(RenameWrite {
        path: new_cfg,
        before: config_before,
        after: config_after,
        mode: 0o644,
    });
    let mut completed = vec![];
    let result = (|| -> Result<()> {
        for item in &moves {
            fs::rename(&item.source, &item.destination)?;
            completed.push(item.clone())
        }
        for item in &writes {
            atomic::write(&item.path, &item.after, item.mode)?
        }
        Ok(())
    })();
    if let Err(primary) = result {
        let mut failures = vec![];
        for write in writes.iter().rev() {
            if write.path.exists() {
                if let Err(e) = atomic::write(&write.path, &write.before, write.mode) {
                    failures.push(e.to_string())
                }
            }
        }
        for item in completed.iter().rev() {
            if let Err(e) = fs::rename(&item.destination, &item.source) {
                failures.push(e.to_string())
            }
        }
        if failures.is_empty() {
            return Err(primary);
        }
        bail!(
            "rename failed: {primary}; rollback failures: {}",
            failures.join("; ")
        )
    }
    let repository = Path::new(GIT).join(format!("{}.git", args[1]));
    if let Ok(output) = Command::new("/usr/bin/git")
        .args([
            format!("--git-dir={}", repository.display()),
            "for-each-ref".into(),
            "--format=%(refname)".into(),
            "refs/kilnr/jobs".into(),
        ])
        .output()
    {
        if output.status.success() {
            for reference in String::from_utf8_lossy(&output.stdout).lines() {
                let Some(old_id) = reference.strip_prefix("refs/kilnr/jobs/") else {
                    continue;
                };
                let marker = format!("-{}-", args[0]);
                let Some(index) = old_id.find(&marker) else {
                    continue;
                };
                let new_id = format!(
                    "{}-{}-{}",
                    &old_id[..index],
                    args[1],
                    &old_id[index + marker.len()..]
                );
                let new_ref = format!("refs/kilnr/jobs/{new_id}");
                let Ok(value) = git(&repository, &["rev-parse", reference]) else {
                    continue;
                };
                let _ = Command::new("/usr/bin/git")
                    .args([
                        format!("--git-dir={}", repository.display()),
                        "update-ref".into(),
                        new_ref,
                        value.clone(),
                    ])
                    .status();
                let _ = Command::new("/usr/bin/git")
                    .args([
                        format!("--git-dir={}", repository.display()),
                        "update-ref".into(),
                        "-d".into(),
                        reference.into(),
                        value,
                    ])
                    .status();
            }
        }
    }
    println!("Renamed Kilnr project: {} -> {}", args[0], args[1]);
    Ok(())
}
fn rerun(args: &[String]) -> Result<()> {
    root_only()?;
    if args.len() != 1 || !valid_build(&args[0]) {
        bail!("usage: rerun <build-id>")
    }
    let structured =
        Regex::new(r"^\d{8}T\d{12}Z-([a-z0-9][a-z0-9_-]{0,62})-[0-9a-f]{7}-[0-9a-f]{8}$")?;
    let expected_project = structured
        .captures(&args[0])
        .and_then(|captures| captures.get(1))
        .map(|capture| capture.as_str().to_owned())
        .context("invalid structured build id")?;
    let _lock = project_lock::ProjectLocks::acquire(
        Path::new(LOCKS),
        std::slice::from_ref(&expected_project),
        project_lock::Mode::Shared,
        false,
    )?;
    let s = read_json(&Path::new(BUILDS).join(&args[0]).join("status.json"))?;
    if s["project"] != expected_project || s["build_id"] != args[0] {
        bail!("build identity mismatch")
    }
    if s["type"] != "ci" {
        bail!("release builds cannot be rerun")
    }
    let project = s["project"].as_str().context("invalid project")?;
    let sha = s["sha"].as_str().context("invalid SHA")?;
    let reference = s["ref"].as_str().context("invalid ref")?;
    if !valid_oid(sha) || !reference.starts_with("refs/heads/") {
        bail!("invalid CI rerun metadata")
    }
    let output = Command::new("/usr/sbin/runuser")
        .args([
            "-u",
            "git",
            "--",
            &format!("{LIBEXEC}/enqueue"),
            project,
            sha,
            sha,
            reference,
        ])
        .output()?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr))
    }
    println!(
        "Queued rerun: {}\nSHA: {sha}",
        String::from_utf8_lossy(&output.stdout).trim()
    );
    Ok(())
}
fn git_key_add() -> Result<()> {
    root_only()?;
    let mut key = String::new();
    io::stdin().take(16385).read_to_string(&mut key)?;
    let key = key.trim();
    if !Regex::new(r"^(ssh-ed25519|sk-ssh-ed25519@openssh\.com)\s+[A-Za-z0-9+/=]+(?:\s+.*)?$")
        .unwrap()
        .is_match(key)
    {
        bail!("invalid Ed25519 public key")
    }
    let path = Path::new("/srv/git/.ssh/authorized_keys");
    let existing = fs::read_to_string(path).unwrap_or_default();
    if existing
        .lines()
        .any(|l| l.split_whitespace().nth(1) == key.split_whitespace().nth(1))
    {
        bail!("key already exists")
    }
    let forced = format!("command=\"git-shell -c \\\"$SSH_ORIGINAL_COMMAND\\\"\",restrict {key}\n");
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(forced.as_bytes())?;
    f.sync_all()?;
    println!("Git key added");
    Ok(())
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("/usr/bin/git")
        .arg(format!("--git-dir={}", repo.display()))
        .args(args)
        .output()?;
    if !output.status.success() {
        bail!(
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn enqueue(args: &[String]) -> Result<()> {
    if args.len() != 4 {
        bail!("usage: enqueue <project|--receive> <old-oid> <new-oid> <ref>")
    }
    let (requested, old, new, reference) = (&args[0], &args[1], &args[2], &args[3]);
    if !valid_oid(old) || !valid_oid(new) {
        bail!("invalid object id")
    }
    let project = if requested == "--receive" {
        let output = Command::new("/usr/bin/git")
            .args(["rev-parse", "--absolute-git-dir"])
            .output()?;
        if !output.status.success() {
            bail!("cannot determine receive repository")
        }
        let repository = PathBuf::from(String::from_utf8(output.stdout)?.trim());
        receive_project(Path::new(GIT), &repository)?
    } else {
        project_lock::validate_name(requested)?.to_owned()
    };
    let _lock = project_lock::ProjectLocks::acquire(
        Path::new(LOCKS),
        std::slice::from_ref(&project),
        project_lock::Mode::Shared,
        false,
    )?;
    let config_path = Path::new(CONFIG).join(format!("{project}.json"));
    let config = read_json(&config_path)?;
    let repo = Path::new(GIT).join(format!("{project}.git"));
    if config["schema"] != 1
        || config["project"] != project
        || config["repository"] != repo.to_string_lossy().as_ref()
    {
        bail!("invalid project configuration")
    }
    if new.chars().all(|c| c == '0') {
        return Ok(());
    }
    let kind = classify_job(&config, old, new, reference)?;
    let Some(kind) = kind else { return Ok(()) };
    let sha = git(
        &repo,
        &["rev-parse", "--verify", &format!("{new}^{{commit}}")],
    )?;
    if !valid_oid(&sha) {
        bail!("invalid commit sha")
    }
    let timestamp = Utc::now().format("%Y%m%dT%H%M%S%6fZ");
    let entropy = SystemTime::now().duration_since(UNIX_EPOCH)?.subsec_nanos() ^ std::process::id();
    let id = format!("{timestamp}-{project}-{}-{entropy:08x}", &sha[..7]);
    let pin = format!("refs/kilnr/jobs/{id}");
    git(&repo, &["update-ref", &pin, &sha])?;
    let mut job = json!({"schema":1,"id":id,"project":project,"received_at":Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros,true),"old_sha":old,"new_sha":new,"sha":sha,"ref":reference,"type":kind,"event":if kind=="ci"{"push"}else{"tag"},"pin_ref":pin});
    if kind == "ci" {
        job["branch"] = json!(reference.trim_start_matches("refs/heads/"));
    } else {
        job["tag"] = json!(reference.trim_start_matches("refs/tags/"));
    }
    let result = atomic::write_json(
        &Path::new("/var/lib/kilnr/queue/incoming").join(format!("{id}.json")),
        &job,
        0o640,
    );
    if result.is_err() {
        let _ = git(&repo, &["update-ref", "-d", &pin]);
    }
    result?;
    println!("{id}");
    Ok(())
}

fn cleanup(args: &[String]) -> Result<()> {
    let mut dry_run = false;
    let mut project = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--dry-run" => dry_run = true,
            "--project" if index + 1 < args.len() => {
                project = Some(args[index + 1].clone());
                index += 1;
            }
            _ => bail!("usage: cleanup [--dry-run] [--project <project>]"),
        }
        index += 1;
    }
    let report = retention::cleanup(
        &retention::Roots {
            state: PathBuf::from("/var/lib/kilnr"),
            config: PathBuf::from(CONFIG),
            git: PathBuf::from(GIT),
        },
        &retention::CleanupOptions {
            project,
            dry_run,
            now: Utc::now(),
        },
    )?;
    for line in report.lines {
        println!("{line}");
    }
    if report.code == 0 {
        Ok(())
    } else {
        bail!("one or more projects were refused")
    }
}

#[allow(dead_code)]
fn cleanup_legacy(args: &[String]) -> Result<()> {
    let dry_run = args.iter().any(|v| v == "--dry-run");
    let project = args
        .windows(2)
        .find(|v| v[0] == "--project")
        .map(|v| v[1].clone());
    if let Some(ref name) = project {
        project_lock::validate_name(name)?;
    }
    let controller = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open("/var/lib/kilnr/locks/controller.lock")?;
    if fs2::FileExt::try_lock_exclusive(&controller).is_err() {
        println!("Deferred: controller is active");
        return Ok(());
    }
    if let Ok(mountinfo) = fs::read_to_string("/proc/self/mountinfo") {
        let root = Path::new(BUILDS).canonicalize()?;
        for line in mountinfo.lines() {
            if let Some(raw) = line.split_whitespace().nth(4) {
                let decoded = raw
                    .replace("\\040", " ")
                    .replace("\\011", "\t")
                    .replace("\\012", "\n")
                    .replace("\\134", "\\");
                let mount = Path::new(&decoded);
                if mount != root && mount.starts_with(&root) {
                    bail!("nested mount below builds root: {decoded}")
                }
            }
        }
    }
    for entry in fs::read_dir(BUILDS)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let Some(id) = name.strip_prefix(".cleanup-") else {
            continue;
        };
        let entries = fs::read_dir(&path)?
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect::<Vec<_>>();
        if entries
            .iter()
            .any(|entry| entry != "record.json" && entry != "build")
        {
            bail!("unexpected cleanup transaction entries: {name}")
        }
        println!(
            "{} interrupted cleanup: {id}",
            if dry_run { "Would finish" } else { "Finishing" }
        );
        if !dry_run {
            let payload = path.join("build");
            if payload.exists() {
                fs::remove_dir_all(payload)?;
            }
            let record = path.join("record.json");
            if record.exists() {
                fs::remove_file(record)?;
            }
            fs::remove_dir(path)?;
        }
    }
    let mut grouped: std::collections::BTreeMap<String, Vec<(PathBuf, Value, DateTime<Utc>)>> =
        Default::default();
    for path in build_dirs() {
        let Ok(status) = read_json(&path.join("status.json")) else {
            continue;
        };
        let Some(name) = status["project"].as_str() else {
            continue;
        };
        if project.as_deref().is_some_and(|p| p != name)
            || !matches!(
                status["state"].as_str(),
                Some("success" | "failed" | "aborted")
            )
        {
            continue;
        }
        let Some(finished) = status["finished_at"]
            .as_str()
            .and_then(|v| DateTime::parse_from_rfc3339(v).ok())
            .map(|v| v.with_timezone(&Utc))
        else {
            continue;
        };
        grouped
            .entry(name.into())
            .or_default()
            .push((path, status, finished));
    }
    for (name, mut entries) in grouped {
        let _lock = match project_lock::ProjectLocks::acquire(
            Path::new(LOCKS),
            std::slice::from_ref(&name),
            project_lock::Mode::Exclusive,
            true,
        ) {
            Ok(v) => v,
            Err(_) => {
                println!("Deferred: project {name} is busy");
                continue;
            }
        };
        let cfg = read_json(&Path::new(CONFIG).join(format!("{name}.json")))?;
        let retention = cfg
            .get("retention")
            .cloned()
            .unwrap_or(json!({"max_age_days":30,"max_builds_per_ref":10,"keep_releases":true}));
        let age = retention["max_age_days"].as_u64();
        let max = retention["max_builds_per_ref"].as_u64().map(|v| v as usize);
        let keep_releases = retention["keep_releases"].as_bool().unwrap_or(true);
        entries.sort_by_key(|a| std::cmp::Reverse(a.2));
        let mut per_ref = std::collections::HashMap::new();
        for (path, status, finished) in entries {
            let count = per_ref
                .entry(status["ref"].as_str().unwrap_or("").to_owned())
                .or_insert(0usize);
            *count += 1;
            let too_many = max.is_some_and(|m| *count > m);
            let too_old = age
                .is_some_and(|d| Utc::now().signed_duration_since(finished).num_days() >= d as i64);
            if !(too_many || too_old) || (keep_releases && status["type"] == "release") {
                continue;
            }
            let id = path.file_name().unwrap().to_string_lossy();
            println!(
                "{} {id} project={name}",
                if dry_run { "Would delete" } else { "Deleting" }
            );
            if !dry_run {
                let meta = fs::symlink_metadata(&path)?;
                if meta.file_type().is_symlink()
                    || !meta.is_dir()
                    || meta.uid() != unsafe { libc::geteuid() }
                {
                    bail!("unsafe build directory: {}", path.display())
                }
                let id = path.file_name().unwrap().to_string_lossy().into_owned();
                let structured = Regex::new(&format!(
                    r"^\d{{8}}T\d{{12}}Z-{}-[0-9a-f]{{7}}-[0-9a-f]{{8}}$",
                    regex::escape(&name)
                ))?;
                if !structured.is_match(&id) {
                    bail!("invalid structured build id: {id}")
                }
                let job = read_json(&path.join("job.json"))?;
                let fresh = read_json(&path.join("status.json"))?;
                if job["id"] != id
                    || job["project"] != name
                    || fresh["build_id"] != id
                    || fresh["project"] != name
                {
                    bail!("build metadata identity mismatch: {id}")
                }
                let status_lock = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .mode(0o640)
                    .open(path.join("status.lock"))?;
                if fs2::FileExt::try_lock_exclusive(&status_lock).is_err() {
                    println!("Deferred: build {id} is busy");
                    continue;
                }
                let transaction = Path::new(BUILDS).join(format!(".cleanup-{id}"));
                if transaction.exists() {
                    bail!(
                        "cleanup transaction already exists: {}",
                        transaction.display()
                    )
                }
                fs::create_dir(&transaction)?;
                atomic::write_json(
                    &transaction.join("record.json"),
                    &json!({"schema":1,"job":job,"status":fresh}),
                    0o640,
                )?;
                fs::rename(&path, transaction.join("build"))?;
                File::open(BUILDS)?.sync_all()?;
                fs::remove_dir_all(transaction.join("build"))?;
                fs::remove_file(transaction.join("record.json"))?;
                fs::remove_dir(transaction)?;
                File::open(BUILDS)?.sync_all()?;
            }
        }
    }
    Ok(())
}

pub fn render_notification(status: &Value) -> String {
    let success = status["state"] == "success";
    let icon = if success { "✅" } else { "❌" };
    let kind = if status["type"] == "release" {
        "release"
    } else {
        "build"
    };
    let adjective = if success {
        "successful"
    } else {
        status["state"].as_str().unwrap_or("failed")
    };
    let mut text = format!(
        "{icon} **{} — {kind} {adjective}**\n\n{} · `{}`\n",
        status["project"],
        short_ref(status["ref"].as_str().unwrap_or("")),
        &status["sha"].as_str().unwrap_or("")[..7.min(status["sha"].as_str().unwrap_or("").len())]
    );
    if let Some(jobs) = status["pipeline"]["jobs"].as_object() {
        let mut groups = std::collections::BTreeMap::<String, Vec<(&String, &Value)>>::new();
        for (name, job) in jobs {
            groups
                .entry(job["group"].as_str().unwrap_or("Other").to_owned())
                .or_default()
                .push((name, job));
        }
        for (group, jobs) in groups {
            let mut characters = group.chars();
            let heading = characters
                .next()
                .map(|first| first.to_uppercase().collect::<String>() + characters.as_str())
                .unwrap_or(group);
            text += &format!("\n**{heading}**");
            for (name, job) in jobs {
                text += &format!(
                    "\n{} {name}",
                    match job["state"].as_str() {
                        Some("success") => "✅",
                        Some("failed") => "❌",
                        Some("skipped") => "⏭️",
                        _ => "•",
                    }
                )
            }
        }
    }
    text += &format!("\n\nDuration: {}", duration(&status["duration_seconds"]));
    text.truncate(text.len().min(1900));
    text
}
fn notify(args: &[String]) -> Result<()> {
    let (render, id) = match args {
        [flag, id] if flag == "--render" => (true, id),
        [id] => (false, id),
        _ => bail!("usage: notify-discord [--render] <build-id>"),
    };
    let status = read_json(&Path::new(BUILDS).join(id).join("status.json"))?;
    let content = render_notification(&status);
    if render {
        println!("{content}");
        return Ok(());
    }
    let cfg = read_json(
        &Path::new(CONFIG).join(format!("{}.json", status["project"].as_str().unwrap())),
    )?;
    let Some(path) = cfg["discord"]["webhook_file"].as_str() else {
        println!("kilnr notify: Discord not configured");
        return Ok(());
    };
    let webhook = fs::read_to_string(path).unwrap_or_default();
    if webhook.trim().is_empty() {
        println!("kilnr notify: Discord not configured");
        return Ok(());
    }
    ureq::post(webhook.trim())
        .header("Content-Type", "application/json")
        .header("User-Agent", "Kilnr-CI/1")
        .send(serde_json::to_vec(&json!({"content":content}))?)?;
    println!("kilnr notify: notification sent");
    Ok(())
}
