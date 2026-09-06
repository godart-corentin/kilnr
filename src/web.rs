use anyhow::{bail, Result};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

pub const MAX_BUILDS: usize = 100;
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

fn header(name: &'static str, value: &str) -> Header {
    Header::from_bytes(name, value).unwrap()
}

fn response(
    code: u16,
    body: Vec<u8>,
    content_type: &str,
    cache: &str,
) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_data(body).with_status_code(StatusCode(code))
        .with_header(header("Content-Type", content_type)).with_header(header("Cache-Control", cache))
        .with_header(header("X-Content-Type-Options", "nosniff")).with_header(header("X-Frame-Options", "DENY"))
        .with_header(header("Referrer-Policy", "no-referrer"))
        .with_header(header("Content-Security-Policy", "default-src 'self'; connect-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; object-src 'none'"))
}
fn json_response(code: u16, value: Value) -> Response<std::io::Cursor<Vec<u8>>> {
    response(
        code,
        serde_json::to_vec(&value).unwrap(),
        "application/json; charset=utf-8",
        "no-store",
    )
}
fn read_json(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn valid_build(id: &str) -> bool {
    !id.starts_with('.') && Regex::new(r"^[A-Za-z0-9_.-]+$").unwrap().is_match(id)
}

pub fn get_build(root: &Path, id: &str) -> Option<(PathBuf, Value)> {
    if !valid_build(id) {
        return None;
    }
    let path = root.join(id);
    let meta = fs::symlink_metadata(&path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return None;
    }
    let status = read_json(&path.join("status.json")).ok()?;
    (status["build_id"] == id).then_some((path, status))
}
pub fn builds(root: &Path) -> Value {
    let mut dirs = fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|v| v.to_str())
                .is_some_and(valid_build)
                && fs::symlink_metadata(p).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
        })
        .collect::<Vec<_>>();
    dirs.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    Value::Array(
        dirs.into_iter()
            .take(MAX_BUILDS)
            .filter_map(|p| {
                let s = read_json(&p.join("status.json")).ok()?;
                (s["build_id"] == p.file_name()?.to_str()?).then(|| {
                    let mut o = serde_json::Map::new();
                    for k in [
                        "build_id",
                        "project",
                        "sha",
                        "ref",
                        "type",
                        "state",
                        "created_at",
                        "started_at",
                        "finished_at",
                        "duration_seconds",
                    ] {
                        o.insert(k.into(), s.get(k).cloned().unwrap_or(Value::Null));
                    }
                    Value::Object(o)
                })
            })
            .collect(),
    )
}
pub fn artifacts(build: &Path) -> Value {
    let root = build.join("artifacts");
    let mut out = vec![];
    fn walk(root: &Path, p: &Path, out: &mut Vec<Value>) {
        if out.len() >= 500 {
            return;
        }
        if let Ok(entries) = fs::read_dir(p) {
            for e in entries.flatten() {
                let p = e.path();
                if let Ok(m) = fs::symlink_metadata(&p) {
                    if m.file_type().is_symlink() {
                        continue;
                    }
                    if m.is_dir() {
                        walk(root, &p, out)
                    } else if m.is_file() {
                        if let Ok(r) = p.strip_prefix(root) {
                            out.push(json!({"path":r.to_string_lossy().replace('\\',"/"),"size":m.len()}))
                        }
                    }
                }
            }
        }
    }
    walk(&root, &root, &mut out);
    Value::Array(out)
}
fn sanitize(text: &str) -> String {
    let ansi = Regex::new(r"\x1B(?:\[[0-?]*[ -/]*[@-~])").unwrap();
    ansi.replace_all(&text.replace("\r\n", "\n").replace('\r', "\n"), "")
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}
pub fn log_path(build: &Path, status: &Value, job: &str) -> Option<PathBuf> {
    let relative = if job == "pipeline" {
        "logs/pipeline.log".into()
    } else {
        if !Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$")
            .unwrap()
            .is_match(job)
        {
            return None;
        }
        let item = status.get("pipeline")?.get("jobs")?.get(job)?;
        item.get("log")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("logs/{job}.log"))
    };
    let p = build.join(relative).canonicalize().ok()?;
    let logs = build.join("logs").canonicalize().ok()?;
    (p.starts_with(logs) && p.is_file()).then_some(p)
}
pub fn log_snapshot(path: &Path) -> Result<Value> {
    let mut f = fs::File::open(path)?;
    let size = f.seek(SeekFrom::End(0))?;
    let start = size.saturating_sub(MAX_LOG_BYTES);
    f.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![];
    f.read_to_end(&mut bytes)?;
    if start > 0 {
        if let Some(i) = bytes.iter().position(|b| *b == b'\n') {
            bytes.drain(..=i);
        }
    }
    Ok(
        json!({"content":sanitize(&String::from_utf8_lossy(&bytes)),"offset":size,"truncated":start>0}),
    )
}

pub struct EventReader {
    receiver: Receiver<Vec<u8>>,
    pending: std::io::Cursor<Vec<u8>>,
}
impl Read for EventReader {
    fn read(&mut self, target: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let count = self.pending.read(target)?;
            if count > 0 {
                return Ok(count);
            }
            match self.receiver.recv() {
                Ok(bytes) => self.pending = std::io::Cursor::new(bytes),
                Err(_) => return Ok(0),
            }
        }
    }
}
fn security_headers(kind: &str) -> Vec<Header> {
    vec![
        header("Content-Type", kind),
        header("Cache-Control", "no-store"),
        header("X-Content-Type-Options", "nosniff"),
        header("X-Frame-Options", "DENY"),
        header("Referrer-Policy", "no-referrer"),
        header("X-Accel-Buffering", "no"),
    ]
}
fn sse(event: &str, value: Value) -> Vec<u8> {
    format!(
        "event: {event}\ndata: {}\n\n",
        serde_json::to_string(&value).unwrap()
    )
    .into_bytes()
}
fn status_jobs(status: &Value) -> Map<String, Value> {
    status
        .get("pipeline")
        .and_then(|v| v.get("jobs"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}
pub fn is_terminal(status: &Value, job: &str) -> bool {
    let state = if job == "pipeline" {
        status["state"].as_str()
    } else {
        status["pipeline"]["jobs"][job]["state"].as_str()
    };
    matches!(state, Some("success" | "failed" | "skipped" | "aborted"))
}
pub fn event_response(build: PathBuf, initial: Value) -> Response<EventReader> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut previous = initial;
        loop {
            thread::sleep(Duration::from_millis(350));
            let Ok(current) = read_json(&build.join("status.json")) else {
                let _ = tx.send(sse("end", json!({"state":"deleted"})));
                break;
            };
            for (event, data) in diff_status_events(&previous, &current) {
                let _ = tx.send(sse(&event, data));
            }
            if is_terminal(&current, "pipeline") {
                let _ = tx.send(sse("end", json!({"state":current["state"]})));
                break;
            }
            previous = current;
        }
    });
    Response::new(
        StatusCode(200),
        security_headers("text/event-stream; charset=utf-8"),
        EventReader {
            receiver: rx,
            pending: std::io::Cursor::new(vec![]),
        },
        None,
        None,
    )
}
pub fn read_chunk(path: &Path, offset: u64) -> Result<(u64, String)> {
    let mut file = fs::File::open(path)?;
    let size = file.seek(SeekFrom::End(0))?;
    let start = offset.min(size);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0; ((size - start).min(128 * 1024)) as usize];
    let count = file.read(&mut bytes)?;
    bytes.truncate(count);
    Ok((
        start + count as u64,
        sanitize(&String::from_utf8_lossy(&bytes)),
    ))
}

pub fn diff_status_events(previous: &Value, current: &Value) -> Vec<(String, Value)> {
    let old = status_jobs(previous);
    let mut events = vec![];
    for (name, item) in status_jobs(current) {
        if &item["state"]
            != old
                .get(&name)
                .map(|value| &value["state"])
                .unwrap_or(&Value::Null)
        {
            let mut data = json!({"name":name,"state":item["state"]});
            if let Some(duration) = item.get("duration_seconds") {
                data["duration_seconds"] = duration.clone();
            }
            events.push(("job".into(), data));
        }
    }
    if current["state"] != previous["state"] {
        let mut data = json!({"state":current["state"]});
        if let Some(duration) = current.get("duration_seconds") {
            data["duration_seconds"] = duration.clone();
        }
        events.push(("build".into(), data));
    }
    events
}
pub fn log_stream_response(
    build: PathBuf,
    initial: Value,
    job: String,
    path: PathBuf,
    mut offset: u64,
) -> Response<EventReader> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || loop {
        match read_chunk(&path, offset) {
            Ok((next, content)) => {
                if next != offset {
                    offset = next;
                    let _ = tx.send(sse("chunk", json!({"offset":offset,"content":content})));
                }
            }
            Err(_) => {
                let _ = tx.send(sse("end", json!({"offset":offset,"state":"deleted"})));
                break;
            }
        }
        let current = read_json(&build.join("status.json")).unwrap_or_else(|_| initial.clone());
        if is_terminal(&current, &job) {
            if let Ok((next, content)) = read_chunk(&path, offset) {
                if next != offset {
                    offset = next;
                    let _ = tx.send(sse("chunk", json!({"offset":offset,"content":content})));
                }
            }
            let state = if job == "pipeline" {
                current["state"].clone()
            } else {
                current["pipeline"]["jobs"][&job]["state"].clone()
            };
            let _ = tx.send(sse("end", json!({"offset":offset,"state":state})));
            break;
        }
        thread::sleep(Duration::from_millis(350));
    });
    Response::new(
        StatusCode(200),
        security_headers("text/event-stream; charset=utf-8"),
        EventReader {
            receiver: rx,
            pending: std::io::Cursor::new(vec![]),
        },
        None,
        None,
    )
}

fn handle(request: Request, builds_root: &Path, static_root: &Path) -> Result<()> {
    let method = request.method().clone();
    let url = request.url().to_owned();
    let raw = url.split('?').next().unwrap_or("/");
    if !matches!(method, Method::Get | Method::Head) {
        request.respond(json_response(
            405,
            json!({"error":"Kilnr Web is read-only"}),
        ))?;
        return Ok(());
    }
    let head = method == Method::Head;
    if let Some(rest) = raw.strip_prefix("/api/builds/") {
        let parts = rest.split('/').collect::<Vec<_>>();
        if let Some((dir, status)) = get_build(builds_root, parts[0]) {
            if parts.as_slice() == [parts[0], "events"] && !head {
                request.respond(event_response(dir, status))?;
                return Ok(());
            }
            if parts.len() == 4 && parts[1] == "logs" && parts[3] == "stream" && !head {
                let Some(path) = log_path(&dir, &status, parts[2]) else {
                    request.respond(json_response(404, json!({"error":"Log not found"})))?;
                    return Ok(());
                };
                let parsed = url
                    .split_once('?')
                    .and_then(|(_, query)| {
                        query
                            .split('&')
                            .find_map(|pair| pair.strip_prefix("offset="))
                    })
                    .unwrap_or("0")
                    .parse::<u64>();
                let Ok(offset) = parsed else {
                    request.respond(json_response(400, json!({"error":"Invalid offset"})))?;
                    return Ok(());
                };
                request.respond(log_stream_response(
                    dir,
                    status,
                    parts[2].into(),
                    path,
                    offset,
                ))?;
                return Ok(());
            }
        }
    }
    let reply = if raw == "/healthz" {
        response(
            200,
            if head { vec![] } else { b"ok\n".to_vec() },
            "text/plain; charset=utf-8",
            "no-store",
        )
    } else if raw == "/api/builds" {
        json_response(200, json!({"builds":builds(builds_root)}))
    } else if let Some(rest) = raw.strip_prefix("/api/builds/") {
        let parts = rest.split('/').collect::<Vec<_>>();
        match get_build(builds_root, parts[0]) {
            None => json_response(404, json!({"error":"Build not found"})),
            Some((dir, status)) => match parts.as_slice() {
                [_] => json_response(200, status),
                [_, "artifacts"] => json_response(200, json!({"artifacts":artifacts(&dir)})),
                [_, "logs", job] => {
                    match log_path(&dir, &status, job).and_then(|p| log_snapshot(&p).ok()) {
                        Some(mut v) => {
                            v["state"] = if *job == "pipeline" {
                                status["state"].clone()
                            } else {
                                status["pipeline"]["jobs"][job]["state"].clone()
                            };
                            json_response(200, v)
                        }
                        None => json_response(404, json!({"error":"Log not found"})),
                    }
                }
                _ => json_response(404, json!({"error":"Not found"})),
            },
        }
    } else if raw.starts_with("/api/") {
        json_response(404, json!({"error":"Not found"}))
    } else {
        let candidate = if raw == "/" {
            static_root.join("index.html")
        } else if let Some(asset) = raw.strip_prefix("/assets/") {
            static_root.join("assets").join(asset)
        } else {
            static_root.join("index.html")
        };
        let safe = candidate
            .canonicalize()
            .ok()
            .filter(|p| static_root.canonicalize().is_ok_and(|r| p.starts_with(r)) && p.is_file());
        match safe.and_then(|p| fs::read(&p).ok().map(|b| (p, b))) {
            Some((p, b)) => {
                let kind = match p.extension().and_then(|v| v.to_str()) {
                    Some("html") => "text/html; charset=utf-8",
                    Some("css") => "text/css; charset=utf-8",
                    Some("js") => "application/javascript; charset=utf-8",
                    Some("svg") => "image/svg+xml",
                    _ => "application/octet-stream",
                };
                response(
                    200,
                    b,
                    kind,
                    if raw.starts_with("/assets/") {
                        "public, max-age=31536000, immutable"
                    } else {
                        "no-store"
                    },
                )
            }
            None => json_response(404, json!({"error":"Not found"})),
        }
    };
    request.respond(if head {
        response(
            reply.status_code().0,
            vec![],
            reply
                .headers()
                .iter()
                .find(|h| h.field.equiv("Content-Type"))
                .map(|h| h.value.as_str())
                .unwrap_or("application/octet-stream"),
            "no-store",
        )
    } else {
        reply
    })?;
    Ok(())
}

pub fn serve() -> Result<()> {
    if unsafe { libc::geteuid() } == 0 {
        bail!("kilnr web: refusing to run as root")
    }
    let root = PathBuf::from(
        std::env::var("KILNR_WEB_BUILDS").unwrap_or_else(|_| "/var/lib/kilnr/builds".into()),
    );
    let static_root = PathBuf::from(
        std::env::var("KILNR_WEB_STATIC").unwrap_or_else(|_| "/opt/kilnr/static".into()),
    );
    let host = std::env::var("KILNR_WEB_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port = std::env::var("KILNR_WEB_PORT").unwrap_or_else(|_| "8088".into());
    let server =
        Server::http(format!("{host}:{port}")).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    println!("kilnr web: listening on http://{host}:{port}");
    for request in server.incoming_requests() {
        let root = root.clone();
        let static_root = static_root.clone();
        thread::spawn(move || {
            if let Err(error) = handle(request, &root, &static_root) {
                eprintln!("kilnr web: {error:#}")
            }
        });
    }
    Ok(())
}

pub fn healthcheck() -> Result<()> {
    let host = std::env::var("KILNR_WEB_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let host = if host == "0.0.0.0" {
        "127.0.0.1".to_owned()
    } else {
        host
    };
    let port = std::env::var("KILNR_WEB_PORT").unwrap_or_else(|_| "8088".into());
    let mut stream = TcpStream::connect(format!("{host}:{port}"))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
    stream.write_all(b"GET /healthz HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    if !response.starts_with("HTTP/1.1 200") && !response.starts_with("HTTP/1.0 200") {
        bail!("unhealthy response")
    }
    Ok(())
}
