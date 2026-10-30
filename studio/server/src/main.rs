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

use serde_json::{Map, Value, json};
use tungstenite::Message;

mod project;

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
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
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
    if path == "/api/project" {
        let (status, body) = match project_boards(&query_value(query, "project").unwrap_or_default(), query) {
            Ok(body) => (200, body),
            Err(error) => (400, json!({"error": error})),
        };
        return respond(&mut stream, status, "application/json", body.to_string().as_bytes());
    }
    // Boardroom opens a project's board at the server's root, as the platform spec has it.
    if path == "/" && query_value(query, "project").is_some() {
        return redirect(&mut stream, &format!("studio/?{query}"));
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
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCross-Origin-Opener-Policy: same-origin\r\nCross-Origin-Embedder-Policy: require-corp\r\nCross-Origin-Resource-Policy: same-origin\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)
}

fn redirect(stream: &mut TcpStream, to: &str) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 301 Moved Permanently\r\nLocation: {to}\r\nContent-Length: 0\r\nCross-Origin-Opener-Policy: same-origin\r\nCross-Origin-Embedder-Policy: require-corp\r\nCross-Origin-Resource-Policy: same-origin\r\nConnection: close\r\n\r\n"
    )
}

/// A query parameter's value, decoded as a form's: `+` is a space.
fn query_value(query: &str, name: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('=').or(Some((pair, ""))))
        .find(|(key, _)| percent_decode(key) == name)
        .map(|(_, value)| percent_decode(&value.replace('+', " ")))
        .filter(|v| !v.is_empty())
}

/// A project's boards, and the one the query names when it names one, or when there is one.
fn project_boards(path: &str, query: &str) -> Result<Value, String> {
    let dir = project::open(path)?;
    let boards = project::boards(&dir);
    let chosen = project::choose(&boards, query_value(query, "board").as_deref(), query_value(query, "variant").as_deref());
    Ok(json!({
        "project": dir,
        "name": dir.file_name().map(|n| n.to_string_lossy()),
        "boards": boards.iter().map(project::Board::summary).collect::<Vec<_>>(),
        "chosen": chosen.as_ref().ok().map(|b| b.summary()),
        "choose": chosen.err(),
    }))
}

/// The project board a request names, which must be exactly one of the project's boards.
fn project_board(request: &Value) -> Result<(PathBuf, project::Board), String> {
    let name = |key: &str| match request[key].as_str() {
        Some(n) if project::plain_name(n) => Ok(Some(n)),
        Some(n) => Err(format!("bad {key} name {n}")),
        None => Ok(None),
    };
    let (board, variant) = (name("board")?, name("variant")?);
    let dir = project::open(request["project"].as_str().ok_or("no project")?)?;
    let board = project::choose(&project::boards(&dir), board, variant)?.clone();
    Ok((dir, board))
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
            "order": job_settings(&dir)["order"],
            "strip": job_settings(&dir)["strip"],
            "args": job_settings(&dir)["args"],
            "presets": presets,
            "notes": dir.join("README.md").is_file(),
        }));
    }
    Value::Array(out)
}

/// A job's `job.json`: whether the studio offers it (`studio`), what copper to take out before
/// routing (`strip`: true for unlocked tracks and vias, "all" for every one) and arguments the
/// core router always gets for it (`args`, such as the seed that routes it best).
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
    // The run being relayed, the open connections its last `done` frame gave, the project board
    // as this page last loaded it (its file and fingerprint), and the last finished project
    // route, which is what a save writes.
    let mut started: Option<Started> = None;
    let mut open: Option<u64> = None;
    let mut base: Option<(PathBuf, String)> = None;
    let mut routed: Option<Routed> = None;
    loop {
        match ws.read() {
            Ok(Message::Text(text)) => {
                let request: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                match request["cmd"].as_str() {
                    Some("run" | "load") => {
                        relay = None;
                        runner.stop();
                        (started, open, routed) = (None, None, None);
                        match start(&request, config, runner, owner) {
                            Ok((rx, run)) => {
                                if let (true, Some(p)) = (run.load, &run.project) {
                                    base = Some((p.board.pcb.clone(), p.read.clone()));
                                }
                                relay = Some(rx);
                                started = Some(run);
                            }
                            Err(error) => send(&mut ws, &json!({"t": "error", "message": error}).to_string())?,
                        }
                    }
                    Some("save") => {
                        let reply = if relay.is_some() {
                            Err("the router is still running: save when it has finished".to_string())
                        } else {
                            save_routed(routed.as_ref(), &mut base, config)
                        };
                        let reply = reply.unwrap_or_else(|error| json!({"t": "saved", "ok": false, "message": error}));
                        send(&mut ws, &reply.to_string())?;
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
                        if line.ends_with("\"t\":\"done\"}") {
                            open = serde_json::from_str::<Value>(&line).ok().and_then(|v| v["open"].as_u64()).or(open);
                        }
                        ws.write(Message::text(line)).map_err(|e| std::io::Error::other(e.to_string()))?;
                        sent = true;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        relay = None;
                        if let Some(run) = started.take()
                            && !run.load
                            && let Some(p) = run.project
                        {
                            let done = Routed { project: p, output: run.output, open, at: project::now_iso() };
                            let fields = studio_status(&done, false, config);
                            let status = project::update_status(&done.project.dir, &done.project.board.board, fields, project::LOCK_WAIT).err();
                            let saveable = done.output.metadata().is_ok_and(|m| m.len() > 0);
                            let reply = json!({"t": "routed", "saveable": saveable, "open": open, "status_error": status});
                            ws.write(Message::text(reply.to_string())).map_err(|e| std::io::Error::other(e.to_string()))?;
                            sent = true;
                            routed = saveable.then_some(done);
                        }
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

/// A project board a run was started on, and the board file's fingerprint as the run began.
struct ProjectRun {
    dir: PathBuf,
    board: project::Board,
    read: String,
}

/// What a socket keeps of the run it started: where its board goes and what it came from.
struct Started {
    output: PathBuf,
    load: bool,
    project: Option<ProjectRun>,
}

/// A finished project route: what a save writes back, and what the status file is told.
struct Routed {
    project: ProjectRun,
    output: PathBuf,
    open: Option<u64>,
    at: String,
}

/// Where a core run's report goes: beside its board.
fn report_path(output: &std::path::Path) -> PathBuf {
    output.with_extension("report.json")
}

/// The studio's entry for a board in `status.json`: what is known of it, nothing made up. A core
/// run's report gives the opens, the findings by severity and the router's commit, and its path
/// for the findings themselves.
fn studio_status(routed: &Routed, saved: bool, config: &Config) -> Map<String, Value> {
    let mut fields = Map::new();
    let report_file = report_path(&routed.output);
    let report: Option<Value> = std::fs::read_to_string(&report_file).ok().and_then(|t| serde_json::from_str(&t).ok());
    let opens = report.as_ref().and_then(|r| r["summary"]["opens"].as_u64()).or(routed.open);
    if let Some(open) = opens {
        fields.insert("opens".into(), json!(open));
    }
    if let Some(r) = &report {
        let findings = r["findings"].as_array().cloned().unwrap_or_default();
        let count = |severity: &str| findings.iter().filter(|x| x["severity"] == severity).count();
        fields.insert("findings".into(), json!({"error": count("error"), "warning": count("warning")}));
        for key in ["connections", "routed"] {
            if let Some(v) = r["summary"][key].as_u64() {
                fields.insert(key.into(), json!(v));
            }
        }
        // A run that crashed, or that the clock stopped with connections left open, needs the
        // person: Boardroom's inbox raises it.
        let crashed = findings.iter().any(|f| f["kind"] == "crash");
        let cut = r["deadline"]["limited"].as_array().is_some_and(|l| !l.is_empty()) || !r["deadline"]["hard_passed_in"].is_null();
        let failed = if crashed {
            json!("crash")
        } else if cut && opens.unwrap_or(0) > 0 {
            json!("deadline")
        } else {
            Value::Null
        };
        fields.insert("failed".into(), failed);
        // The full report goes into the project, in the studio's own directory, so the checks
        // panel can list its findings.
        let relative = PathBuf::from("route").join(&routed.project.board.board).join("report.json");
        let into = routed.project.dir.join(&relative);
        if into.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok()) && std::fs::copy(&report_file, &into).is_ok() {
            fields.insert("report".into(), json!(relative.display().to_string()));
        }
    }
    fields.insert("last_route".into(), json!(routed.at));
    let commit = report.as_ref().and_then(|r| r["router"]["commit"].as_str().map(String::from)).or_else(|| project::router_commit(&config.root));
    if let Some(commit) = commit {
        fields.insert("router".into(), json!(commit));
    }
    fields.insert("saved".into(), json!(saved));
    fields.insert("updated".into(), json!(project::now_iso()));
    fields
}

/// Writes the last finished route over the project's board, if that board is still the one this
/// page loaded, then records the save in the project's status.
fn save_routed(routed: Option<&Routed>, base: &mut Option<(PathBuf, String)>, config: &Config) -> Result<Value, String> {
    let routed = routed.ok_or("nothing routed to save: route the project's board first")?;
    let board = &routed.project.board;
    let loaded = base.as_ref().filter(|(pcb, _)| *pcb == board.pcb).ok_or("load the project's board before saving to it")?;
    let now = project::save(board, &routed.output, &loaded.1)?;
    *base = Some((board.pcb.clone(), now));
    let fields = studio_status(routed, true, config);
    let status = project::update_status(&routed.project.dir, &board.board, fields, project::LOCK_WAIT).err();
    Ok(json!({"t": "saved", "ok": true, "path": board.pcb, "status_error": status}))
}

fn send(ws: &mut tungstenite::WebSocket<TcpStream>, text: &str) -> std::io::Result<()> {
    ws.send(Message::text(text.to_string())).map_err(|e| std::io::Error::other(e.to_string()))
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|s| s.as_str().map(str::to_string)).collect()
}

/// Starts the router on the job, or on a project's board, with the client's arguments,
/// environment and placements. A `load` is a run that only shows the board; its session is kept
/// apart from the routed ones.
fn start(request: &Value, config: &Config, runner: &Arc<Runner>, owner: u64) -> Result<(Receiver<String>, Started), String> {
    let load = request["cmd"].as_str() == Some("load");
    let core = request["engine"].as_str() == Some("core");
    // A project's board is routed where it lies; its runs are kept in the studio, by project.
    let (job, input, settings, runs, from_project) = if request["project"].is_string() {
        let (dir, board) = project_board(request)?;
        if !core {
            return Err("a project's board is routed by the core router".into());
        }
        let read = project::fingerprint(&std::fs::read(&board.pcb).map_err(|e| format!("{}: {e}", board.pcb.display()))?);
        let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let key = &project::fingerprint(dir.as_os_str().as_encoded_bytes())[..8];
        let runs = config.runs.join("projects").join(format!("{name}-{key}")).join(format!("{}-{}", board.board, board.variant));
        let input = Input { dsn: None, pcb: Some(board.pcb.clone()), project: board.project.clone() };
        let job = format!("{name} {}/{}", board.board, board.variant);
        (job, input, Value::Null, runs, Some(ProjectRun { dir, board, read }))
    } else {
        let job = request["job"].as_str().ok_or("no job")?;
        if job.contains('/') || job.starts_with('.') {
            return Err(format!("bad job name {job}"));
        }
        let dir = config.root.join("jobs").join(job);
        let input = job_input(&dir).ok_or(format!("job {job} has no input board"))?;
        (job.to_string(), input, job_settings(&dir), config.runs.join(job), None)
    };
    std::fs::create_dir_all(&runs).map_err(|e| e.to_string())?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
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
        if let Some(rules) = from_project.as_ref().and_then(|p| p.board.rules.as_ref()) {
            std::fs::copy(rules, output.with_extension("kicad_dru")).map_err(|e| e.to_string())?;
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
            match &settings["strip"] {
                Value::String(s) if s == "all" => args.push("--strip-all".into()),
                Value::Bool(true) => args.push("--strip".into()),
                _ => {}
            }
        }
        args.extend(settings["args"].as_str().unwrap_or("").split_whitespace().map(String::from));
        if let Some(regions) = from_project.as_ref().and_then(|p| p.board.regions.as_ref()) {
            args.extend(["--regions".into(), regions.display().to_string()]);
        }
        if load {
            args.push("--no-route".into());
        } else {
            // The run's numbers and findings, for the project's status (Boardroom reads them).
            args.extend(["--report".into(), report_path(&output).display().to_string()]);
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
    Ok((rx, Started { output, load, project: from_project }))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_values_decode_as_a_form() {
        let query = "project=%2Fhome%2Fme%2Fmy+board&board=easy2350&variant=&flag";
        assert_eq!(query_value(query, "project").as_deref(), Some("/home/me/my board"));
        assert_eq!(query_value(query, "board").as_deref(), Some("easy2350"));
        assert_eq!(query_value(query, "variant"), None);
        assert_eq!(query_value(query, "flag"), None);
        assert_eq!(query_value("", "project"), None);
    }

    #[test]
    fn a_core_runs_report_fills_the_studios_status_and_lands_in_the_project() {
        let dir = std::env::temp_dir().join(format!("studio-report-{}", std::process::id()));
        let project = dir.join("proj");
        std::fs::create_dir_all(project.join("design")).unwrap();
        let output = dir.join("run.kicad_pcb");
        let report = |opens: u64, limited: Value, crash: bool| {
            let mut findings = vec![json!({"severity": "error", "kind": "open"}), json!({"severity": "warning", "kind": "neck"})];
            if crash {
                findings.push(json!({"severity": "error", "kind": "crash"}));
            }
            let r = json!({"router": {"commit": "abc1234"}, "summary": {"connections": 414, "routed": 414 - opens, "opens": opens},
                           "deadline": {"limited": limited, "hard_passed_in": null}, "findings": findings});
            std::fs::write(report_path(&output), r.to_string()).unwrap();
        };
        let config = Config { root: dir.clone(), router: dir.clone(), core: dir.clone(), runs: dir.clone() };
        let board = project::Board { board: "b".into(), variant: "a".into(), pcb: project.join("hw/b/a/b.kicad_pcb"), project: None, rules: None, regions: None };
        let routed = Routed { project: ProjectRun { dir: project.clone(), board, read: String::new() }, output: output.clone(), open: Some(9), at: "t".into() };

        report(2, json!(["negotiation"]), false);
        let f = studio_status(&routed, false, &config);
        assert_eq!((f["opens"].clone(), f["connections"].clone(), f["routed"].clone()), (json!(2), json!(414), json!(412)), "the report's, not the live line's");
        assert_eq!(f["findings"], json!({"error": 1, "warning": 1}));
        assert_eq!(f["failed"], json!("deadline"), "cut short with connections open");
        assert_eq!(f["router"], json!("abc1234"));
        assert_eq!(f["report"], json!("route/b/report.json"));
        assert!(project.join("route/b/report.json").is_file());

        report(0, json!(["negotiation"]), false);
        assert_eq!(studio_status(&routed, false, &config)["failed"], Value::Null, "cut short but finished: nothing to raise");
        report(0, json!([]), true);
        assert_eq!(studio_status(&routed, false, &config)["failed"], json!("crash"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_request_names_a_project_board_by_plain_names() {
        let error = project_board(&json!({"project": "/nowhere", "board": "../etc"})).unwrap_err();
        assert!(error.contains("bad board name"), "{error}");
        assert!(project_board(&json!({"board": "b"})).unwrap_err().contains("no project"));
    }
}
