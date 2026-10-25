//! Serves the project's pages and the studio, lists the route jobs, and runs the router for a
//! browser: one run at a time, its live frames relayed over a WebSocket.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tungstenite::Message;

const STOP_GRACE: Duration = Duration::from_secs(8);
/// How long past its time limit a run may go before it is stopped from outside.
const OVERRUN: Duration = Duration::from_secs(20);
const RELAY_DEPTH: usize = 1024;

struct Config {
    root: PathBuf,
    router: PathBuf,
    /// The MIT core's router, which reads `.kicad_pcb` boards.
    core: PathBuf,
    runs: PathBuf,
}

struct Run {
    id: u64,
    owner: u64,
    child: Child,
    stdin: Option<ChildStdin>,
}

type Spawn = (Command, std::sync::mpsc::Sender<std::io::Result<Child>>);

struct Runner {
    current: Mutex<Option<Run>>,
    next_id: std::sync::atomic::AtomicU64,
    spawner: Mutex<std::sync::mpsc::Sender<Spawn>>,
}

impl Runner {
    /// Routers are started from one thread that lives as long as the server, and die with it:
    /// the kernel's parent-death signal follows the thread that forked, not the process.
    fn new() -> Runner {
        let (tx, rx) = std::sync::mpsc::channel::<Spawn>();
        std::thread::spawn(move || {
            for (mut command, reply) in rx {
                unsafe {
                    command.pre_exec(|| {
                        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
                let _ = reply.send(command.spawn());
            }
        });
        Runner { current: Mutex::new(None), next_id: Default::default(), spawner: Mutex::new(tx) }
    }

    fn spawn(&self, command: Command) -> std::io::Result<Child> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.spawner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send((command, tx))
            .map_err(|_| std::io::Error::other("the spawner thread is gone"))?;
        rx.recv().map_err(|_| std::io::Error::other("the spawner thread is gone"))?
    }

    /// Reaps the run as soon as it exits, so a finished router never lingers as a zombie.
    fn reap(self: &Arc<Self>, id: u64) {
        let runner = self.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(200));
                let mut current = runner.current.lock().unwrap_or_else(|e| e.into_inner());
                match current.as_mut() {
                    Some(run) if run.id == id => {
                        if !matches!(run.child.try_wait(), Ok(None)) {
                            *current = None;
                            return;
                        }
                    }
                    _ => return,
                }
            }
        });
    }

    fn stop_owned_by(self: &Arc<Self>, owner: u64) {
        let owned = self.current.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|r| r.owner == owner);
        if owned {
            self.request_stop();
        }
    }

    /// Asks the run to stop (it still writes its session), and kills it if it does not.
    fn stop(&self) {
        let Some(mut run) = self.current.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        if let Some(mut stdin) = run.stdin.take() {
            let _ = stdin.write_all(b"stop\n");
        }
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline {
            if matches!(run.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = run.child.kill();
        let _ = run.child.wait();
    }

    /// Asks the run to stop, and kills it if it is still going after the grace period.
    fn request_stop(self: &Arc<Self>) {
        let id = {
            let mut current = self.current.lock().unwrap_or_else(|e| e.into_inner());
            let Some(run) = current.as_mut() else { return };
            if let Some(stdin) = run.stdin.as_mut() {
                let _ = stdin.write_all(b"stop\n");
            }
            run.id
        };
        let runner = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(STOP_GRACE);
            runner.kill_if(id);
        });
    }

    fn kill_if(&self, id: u64) {
        let mut current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(run) = current.as_mut()
            && run.id == id
            && matches!(run.child.try_wait(), Ok(None))
        {
            let _ = run.child.kill();
        }
    }

    fn watch_deadline(self: &Arc<Self>, id: u64, limit: Duration) {
        let runner = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(limit + OVERRUN);
            let current = runner.current.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|r| r.id);
            if current == Some(id) {
                runner.request_stop();
            }
        });
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let root = PathBuf::from(flag("--root").unwrap_or_else(|| ".".into()));
    let root = root.canonicalize().unwrap_or_else(|e| fail(&format!("--root {}: {e}", root.display())));
    if let Some(path) = flag("--dump-jobs") {
        let written = std::fs::write(&path, jobs(&root).to_string());
        if let Err(error) = written {
            fail(&format!("--dump-jobs {path}: {error}"));
        }
        return;
    }
    let router = flag("--router").map_or_else(|| root.join("target-topo/release/copperroute"), PathBuf::from);
    if !router.is_file() {
        fail(&format!("no router at {} (build it, or pass --router)", router.display()));
    }
    let port = flag("--port")
        .or_else(|| std::env::var("PORT").ok())
        .or_else(|| registry_port(&root))
        .unwrap_or_else(|| fail("no port: pass --port, set $PORT, or register the project with `proj add`"));
    let core = flag("--core").map_or_else(|| root.join("core/target/release/ultraroute"), PathBuf::from);
    if !core.is_file() {
        eprintln!("studio-server: no core router at {} (build it, or pass --core); only copperroute will run", core.display());
    }
    let config = Arc::new(Config { runs: root.join("studio/runs"), root, router, core });
    let runner = Arc::new(Runner::new());
    let listener = TcpListener::bind(format!("0.0.0.0:{port}")).unwrap_or_else(|e| fail(&format!("port {port}: {e}")));
    eprintln!("studio on http://localhost:{port}/studio/ serving {}", config.root.display());
    for stream in listener.incoming().flatten() {
        let (config, runner) = (config.clone(), runner.clone());
        std::thread::spawn(move || {
            if let Err(error) = connection(stream, &config, &runner) {
                eprintln!("connection: {error}");
            }
        });
    }
}

fn fail(message: &str) -> ! {
    eprintln!("studio-server: {message}");
    std::process::exit(2);
}

fn registry_port(root: &Path) -> Option<String> {
    let fallback = std::env::var("HOME").map(|h| PathBuf::from(h).join("scripts/proj")).ok()?;
    let output = Command::new("proj")
        .arg("port")
        .current_dir(root)
        .output()
        .or_else(|_| Command::new(&fallback).arg("port").current_dir(root).output())
        .ok()?;
    let port = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (output.status.success() && !port.is_empty()).then_some(port)
}

fn connection(stream: TcpStream, config: &Config, runner: &Arc<Runner>) -> std::io::Result<()> {
    let mut head = vec![0u8; 8192];
    let mut seen = 0;
    for _ in 0..100 {
        seen = stream.peek(&mut head)?;
        if seen == 0 || head[..seen].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let head = String::from_utf8_lossy(&head[..seen]).to_string();
    let mut words = head.split_whitespace();
    let (method, target) = (words.next().unwrap_or(""), words.next().unwrap_or("/"));
    let path = target.split('?').next().unwrap_or("/");
    if path == "/ws" {
        return socket(stream, config, runner);
    }
    let mut stream = stream;
    let mut sink = [0u8; 8192];
    let _ = stream.read(&mut sink)?;
    if method != "GET" && method != "HEAD" {
        return respond(&mut stream, 405, "text/plain", b"GET only");
    }
    if path == "/api/jobs" {
        return respond(&mut stream, 200, "application/json", jobs(&config.root).to_string().as_bytes());
    }
    let file = match path.strip_prefix("/studio") {
        Some("") => return redirect(&mut stream, "/studio/"),
        Some(rest) => resolve(&config.root.join("studio/web/dist"), rest),
        None => resolve(&config.root, path),
    };
    match file {
        Some(file) if file.is_dir() => {
            if !path.ends_with('/') {
                return redirect(&mut stream, &format!("{path}/"));
            }
            let index = file.join("index.html");
            if index.is_file() {
                return respond(&mut stream, 200, "text/html; charset=utf-8", &std::fs::read(index)?);
            }
            respond(&mut stream, 200, "text/html; charset=utf-8", listing(&file, path).as_bytes())
        }
        Some(file) if file.is_file() => {
            let body = std::fs::read(&file)?;
            respond(&mut stream, 200, content_type(&file), if method == "HEAD" { &[] } else { &body })
        }
        _ => respond(&mut stream, 404, "text/plain", b"not found"),
    }
}

fn resolve(base: &Path, path: &str) -> Option<PathBuf> {
    let decoded = percent_decode(path);
    let relative = Path::new(decoded.trim_start_matches('/'));
    if relative.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(base.join(relative))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn listing(dir: &Path, path: &str) -> String {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if e.path().is_dir() { format!("{name}/") } else { name }
        })
        .collect();
    names.sort();
    let links: String = names.iter().map(|n| format!("<li><a href=\"{n}\">{n}</a></li>")).collect();
    format!("<!doctype html><meta charset=utf-8><title>{path}</title><h1>{path}</h1><ul>{links}</ul>")
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "json" => "application/json",
        "wasm" => "application/wasm",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "csv" | "txt" | "log" | "md" | "ses" | "dsn" | "jsonl" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn respond(stream: &mut TcpStream, status: u16, kind: &str, body: &[u8]) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        301 => "Moved Permanently",
        404 => "Not Found",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)
}

fn redirect(stream: &mut TcpStream, to: &str) -> std::io::Result<()> {
    write!(stream, "HTTP/1.1 301 Moved Permanently\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
}

/// Every job with a board, and the recipes from its config files as presets, with the score
/// each got on the leaderboard.
fn jobs(root: &Path) -> Value {
    let mut out = Vec::new();
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root.join("jobs")).into_iter().flatten().flatten().map(|e| e.path()).collect();
    dirs.sort();
    for dir in dirs {
        let Some(input) = job_input(&dir) else { continue };
        let mut presets = BTreeMap::new();
        for row in leaderboard(&dir.join("leaderboard.csv")) {
            if row.get("router").map(String::as_str) != Some("copperroute") {
                continue;
            }
            let Some(name) = row.get("name") else { continue };
            let args: Vec<&str> = row.get("args").map(|a| a.split_whitespace().collect()).unwrap_or_default();
            let env = row.get("env").and_then(|e| serde_json::from_str::<Value>(e).ok()).unwrap_or(json!({}));
            let number = |key: &str| row.get(key).and_then(|v| v.parse::<f64>().ok());
            presets.insert(
                name.clone(),
                json!({"name": name, "args": args, "env": env, "open": number("unconnected"), "new_errors": number("new_violations"), "seconds": number("wall_s")}),
            );
        }
        let mut configs: Vec<PathBuf> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("configs") && n.ends_with(".json")))
            .collect();
        configs.sort();
        for path in configs {
            let Ok(list) = std::fs::read_to_string(&path).map(|t| serde_json::from_str::<Value>(&t).unwrap_or(Value::Null)) else {
                continue;
            };
            for config in list.as_array().into_iter().flatten() {
                if config["router"].as_str() != Some("copperroute") {
                    continue;
                }
                let Some(name) = config["name"].as_str() else { continue };
                presets
                    .entry(name.to_string())
                    .or_insert_with(|| json!({"name": name, "args": config["args"], "env": config["env"]}));
            }
        }
        let mut presets: Vec<Value> = presets.into_values().collect();
        presets.sort_by(|a, b| {
            let key = |v: &Value| {
                let n = |k: &str| v[k].as_f64().unwrap_or(f64::MAX);
                [n("open"), n("new_errors"), n("seconds")]
            };
            let (a, b) = (key(a), key(b));
            a.iter().zip(&b).map(|(x, y)| x.total_cmp(y)).find(|o| o.is_ne()).unwrap_or(std::cmp::Ordering::Equal)
        });
        out.push(json!({
            "name": dir.file_name().map(|n| n.to_string_lossy()),
            "dsn": input.dsn.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy()),
            "kicad_pcb": input.pcb.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy()),
            "kicad_project": input.project.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy()),
            "studio": job_settings(&dir)["studio"].as_bool().unwrap_or(false),
            "strip": job_settings(&dir)["strip"],
            "presets": presets,
            "notes": dir.join("README.md").is_file(),
        }));
    }
    Value::Array(out)
}

/// A job's `job.json`: whether the studio offers it (`studio`) and what copper to take out before
/// routing (`strip`: true for unlocked tracks and vias, "all" for every one).
fn job_settings(dir: &Path) -> Value {
    std::fs::read_to_string(dir.join("job.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null)
}

/// A job's boards: a Specctra `.dsn` for copperroute, a `.kicad_pcb` for the core, and the
/// project's rules.
struct Input {
    dsn: Option<PathBuf>,
    pcb: Option<PathBuf>,
    project: Option<PathBuf>,
}

fn job_input(dir: &Path) -> Option<Input> {
    let input = dir.join("input");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&input).ok()?.flatten().map(|e| e.path()).collect();
    files.sort();
    let with = |ext: &str| files.iter().find(|p| p.extension().is_some_and(|e| e == ext)).cloned();
    let found = Input { dsn: with("dsn"), pcb: with("kicad_pcb"), project: with("kicad_pro") };
    (found.dsn.is_some() || found.pcb.is_some()).then_some(found)
}

fn leaderboard(path: &Path) -> Vec<BTreeMap<String, String>> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let mut rows = csv(&text).into_iter();
    let header = rows.next().unwrap_or_default();
    rows.map(|cells| header.iter().cloned().zip(cells).collect()).collect()
}

fn csv(text: &str) -> Vec<Vec<String>> {
    let (mut rows, mut row, mut cell) = (Vec::new(), Vec::new(), String::new());
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                cell.push('"');
                chars.next();
            }
            ('"', _) => quoted = !quoted,
            (',', false) => row.push(std::mem::take(&mut cell)),
            ('\n', false) => {
                row.push(std::mem::take(&mut cell));
                rows.push(std::mem::take(&mut row));
            }
            ('\r', false) => {}
            (c, _) => cell.push(c),
        }
    }
    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        rows.push(row);
    }
    rows
}

/// A closed page's run is stopped: nothing is left routing for nobody.
fn socket(stream: TcpStream, config: &Config, runner: &Arc<Runner>) -> std::io::Result<()> {
    static CONNECTIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let owner = CONNECTIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let result = serve_socket(stream, config, runner, owner);
    runner.stop_owned_by(owner);
    result
}

fn serve_socket(stream: TcpStream, config: &Config, runner: &Arc<Runner>, owner: u64) -> std::io::Result<()> {
    let mut ws = tungstenite::accept(stream).map_err(|e| std::io::Error::other(e.to_string()))?;
    ws.get_mut().set_read_timeout(Some(Duration::from_millis(8)))?;
    ws.get_mut().set_nodelay(true)?;
    let mut relay: Option<Receiver<String>> = None;
    loop {
        match ws.read() {
            Ok(Message::Text(text)) => {
                let request: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                match request["cmd"].as_str() {
                    Some("run" | "load") => {
                        relay = None;
                        runner.stop();
                        match start(&request, config, runner, owner) {
                            Ok(rx) => relay = Some(rx),
                            Err(error) => send(&mut ws, &json!({"t": "error", "message": error}).to_string())?,
                        }
                    }
                    Some("stop") => runner.request_stop(),
                    _ => {}
                }
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => return Ok(()),
            Err(e) => return Err(std::io::Error::other(e.to_string())),
        }
        if let Some(rx) = &relay {
            let mut sent = false;
            loop {
                match rx.try_recv() {
                    Ok(line) => {
                        ws.write(Message::text(line)).map_err(|e| std::io::Error::other(e.to_string()))?;
                        sent = true;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        relay = None;
                        break;
                    }
                }
            }
            if sent {
                ws.flush().map_err(|e| std::io::Error::other(e.to_string()))?;
            }
        }
    }
}

fn send(ws: &mut tungstenite::WebSocket<TcpStream>, text: &str) -> std::io::Result<()> {
    ws.send(Message::text(text.to_string())).map_err(|e| std::io::Error::other(e.to_string()))
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|s| s.as_str().map(str::to_string)).collect()
}

/// Starts the router on the job, with the client's arguments, environment and placements. A
/// `load` is a run that only shows the board; its session is kept apart from the routed ones.
fn start(request: &Value, config: &Config, runner: &Arc<Runner>, owner: u64) -> Result<Receiver<String>, String> {
    let job = request["job"].as_str().ok_or("no job")?;
    if job.contains('/') || job.starts_with('.') {
        return Err(format!("bad job name {job}"));
    }
    let dir = config.root.join("jobs").join(job);
    let input = job_input(&dir).ok_or(format!("job {job} has no input board"))?;
    let runs = config.runs.join(job);
    std::fs::create_dir_all(&runs).map_err(|e| e.to_string())?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let load = request["cmd"].as_str() == Some("load");
    let core = request["engine"].as_str() == Some("core");
    let extension = if core { "kicad_pcb" } else { "ses" };
    let output = if load {
        std::fs::create_dir_all(runs.join("loaded")).map_err(|e| e.to_string())?;
        runs.join("loaded").join(format!("{stamp}.{extension}"))
    } else {
        runs.join(format!("{stamp}.{extension}"))
    };

    let mut args: Vec<String> = vec!["-n".into(), "15".into()];
    if core {
        let pcb = input.pcb.as_ref().ok_or(format!("job {job} has no .kicad_pcb for the core router"))?;
        // The core reads the rules from the project beside its board, so every result gets one.
        if let Some(project) = &input.project {
            std::fs::copy(project, output.with_extension("kicad_pro")).map_err(|e| e.to_string())?;
        }
        let from = if !load && request["continue"].as_bool() == Some(true) {
            latest_session(&runs, &output, "kicad_pcb").unwrap_or_else(|| pcb.clone())
        } else {
            pcb.clone()
        };
        args.extend([config.core.display().to_string(), "route".into(), from.display().to_string()]);
        args.extend(["-o".into(), output.display().to_string()]);
        // A run continuing from an earlier result keeps that result's copper.
        if from == *pcb {
            match &job_settings(&dir)["strip"] {
                Value::String(s) if s == "all" => args.push("--strip-all".into()),
                Value::Bool(true) => args.push("--strip".into()),
                _ => {}
            }
        }
        if load {
            args.push("--no-route".into());
        }
    } else {
        let dsn = input.dsn.as_ref().ok_or(format!("job {job} has no .dsn for copperroute"))?;
        args.extend([config.router.display().to_string(), "route".into(), dsn.display().to_string()]);
        args.extend(["-o".into(), output.display().to_string()]);
        if let Some(project) = &input.project {
            args.extend(["--kicad-project".into(), project.display().to_string()]);
        }
        if !load
            && request["continue"].as_bool() == Some(true)
            && let Some(last) = latest_session(&runs, &output, "ses")
        {
            args.extend(["--ses".into(), last.display().to_string()]);
        }
    }
    if let Some(timeout) = request["timeout"].as_u64() {
        args.extend(["--timeout".into(), timeout.to_string()]);
    }
    args.extend(["--live".into(), "-".into()]);
    if let Some(fps) = request["fps"].as_f64() {
        args.extend(["--live-fps".into(), fps.to_string()]);
    }
    for (name, at) in request["place"].as_object().into_iter().flatten() {
        let at: Vec<f64> = at.as_array().into_iter().flatten().filter_map(Value::as_f64).collect();
        if at.len() == 2 {
            args.push(format!("--place={name}={:.4},{:.4}", at[0], at[1]));
        }
    }
    args.extend(strings(&request["args"]));

    let mut command = Command::new("nice");
    command.args(&args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (key, value) in request["env"].as_object().into_iter().flatten() {
        if let Some(value) = value.as_str() {
            command.env(key, value);
        }
    }
    let mut child = runner.spawn(command).map_err(|e| format!("couldn't start the router: {e}"))?;
    let (tx, rx) = sync_channel::<String>(RELAY_DEPTH);
    let _ = tx.send(json!({"t": "run", "args": args, "env": request["env"], "ses": output}).to_string());
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let stderr = child.stderr.take().ok_or("no stderr")?;
    let stdin = child.stdin.take();
    relay_lines(stdout, tx.clone(), |line| line);
    relay_lines(stderr, tx.clone(), |line| json!({"t": "log", "line": line}).to_string());
    let id = runner.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    *runner.current.lock().unwrap_or_else(|e| e.into_inner()) = Some(Run { id, owner, child, stdin });
    runner.reap(id);
    if let Some(timeout) = request["timeout"].as_u64() {
        runner.watch_deadline(id, Duration::from_secs(timeout));
    }
    Ok(rx)
}

fn relay_lines(source: impl Read + Send + 'static, tx: SyncSender<String>, wrap: impl Fn(String) -> String + Send + 'static) {
    std::thread::spawn(move || {
        let mut alive = true;
        for line in BufReader::new(source).lines() {
            let Ok(line) = line else { break };
            if alive && tx.send(wrap(line)).is_err() {
                alive = false;
            }
        }
        if alive {
            let _ = tx.send(json!({"t": "eof"}).to_string());
        }
    });
}

fn latest_session(runs: &Path, except: &Path, extension: &str) -> Option<PathBuf> {
    std::fs::read_dir(runs)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p != except && p.extension().is_some_and(|e| e == extension) && p.metadata().is_ok_and(|m| m.len() > 0))
        .max_by_key(|p| p.metadata().and_then(|m| m.modified()).ok())
}
