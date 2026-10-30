//! Boardroom projects: a folder with its KiCad boards under `hw/<board>/<variant>/` and its
//! headline state in `design/status.json`. The studio opens a board from there, saves the routed
//! board back over it, and records what it did under `apps.studio.<board>` in the status file.

use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

/// How long a status write waits for another app's lock before it gives up.
pub const LOCK_WAIT: Duration = Duration::from_secs(5);

/// One board of a project, and the files the router reads beside it.
#[derive(Clone, Debug, PartialEq)]
pub struct Board {
    pub board: String,
    pub variant: String,
    pub pcb: PathBuf,
    pub project: Option<PathBuf>,
    pub rules: Option<PathBuf>,
    /// Circuit Composer's `routing.json`, the router's `--regions`.
    pub regions: Option<PathBuf>,
}

impl Board {
    pub fn summary(&self) -> Value {
        let name = |p: &Option<PathBuf>| p.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
        json!({
            "board": self.board,
            "variant": self.variant,
            "kicad_pcb": name(&Some(self.pcb.clone())),
            "kicad_project": name(&self.project),
            "kicad_dru": name(&self.rules),
            "routing": name(&self.regions),
        })
    }
}

/// The project at an absolute path, as the folder it really is: one with an `hw/` of boards.
pub fn open(path: &str) -> Result<PathBuf, String> {
    let given = Path::new(path);
    if !given.is_absolute() {
        return Err(format!("project {path}: give an absolute path"));
    }
    let dir = given.canonicalize().map_err(|e| format!("project {path}: {e}"))?;
    if !dir.join("hw").is_dir() {
        return Err(format!("project {path}: no hw/ folder of boards"));
    }
    Ok(dir)
}

/// Every board under `hw/<board>/<variant>/`. KiCad's autosaves and backups are not boards, and
/// of several boards in one folder the one with a `.kicad_pro` beside it is the project's.
pub fn boards(project: &Path) -> Vec<Board> {
    let mut out = Vec::new();
    for board_dir in sorted_dirs(&project.join("hw")) {
        for variant_dir in sorted_dirs(&board_dir) {
            let board = file_name(&board_dir);
            let variant = file_name(&variant_dir);
            let pcbs: Vec<PathBuf> = sorted_files(&variant_dir)
                .into_iter()
                .filter(|p| p.extension().is_some_and(|e| e == "kicad_pcb"))
                .filter(|p| !file_name(p).starts_with(['_', '.']))
                .collect();
            let with_project: Vec<&PathBuf> = pcbs.iter().filter(|p| p.with_extension("kicad_pro").is_file()).collect();
            let pcb = with_project
                .iter()
                .find(|p| p.file_stem().is_some_and(|s| *s == *board))
                .or(with_project.first())
                .copied()
                .or(if pcbs.len() == 1 { pcbs.first() } else { None });
            let Some(pcb) = pcb else { continue };
            let beside = |ext: &str| Some(pcb.with_extension(ext)).filter(|p| p.is_file());
            let regions = Some(variant_dir.join("routing.json")).filter(|p| p.is_file());
            out.push(Board { project: beside("kicad_pro"), rules: beside("kicad_dru"), regions, pcb: pcb.clone(), board, variant });
        }
    }
    out
}

/// The board the person asked for: by name and variant, either left out when only one fits.
pub fn choose<'a>(boards: &'a [Board], board: Option<&str>, variant: Option<&str>) -> Result<&'a Board, String> {
    let fits: Vec<&Board> = boards
        .iter()
        .filter(|b| board.is_none_or(|n| b.board == n) && variant.is_none_or(|v| b.variant == v))
        .collect();
    match fits.as_slice() {
        [one] => Ok(one),
        [] if boards.is_empty() => Err("the project has no boards under hw/<board>/<variant>/".into()),
        [] => Err(format!("no board {}{} in the project", board.unwrap_or("*"), variant.map(|v| format!("/{v}")).unwrap_or_default())),
        _ => Err("the project has several boards: choose one".into()),
    }
}

/// A board's identity on disk, to tell whether it changed since it was loaded (FNV-1a, 64 bits,
/// with the length): stable across builds, which the standard library's hasher is not.
pub fn fingerprint(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}-{}", bytes.len())
}

/// Writes the routed board over the project's board, unless the board on disk is no longer the
/// one loaded (`base`, its fingerprint then): someone else's edit is never overwritten. Returns
/// the fingerprint of what is on disk now.
pub fn save(board: &Board, routed: &Path, base: &str) -> Result<String, String> {
    let shown = board.pcb.display();
    let now = std::fs::read(&board.pcb).map_err(|e| format!("{shown}: {e}"))?;
    if fingerprint(&now) != base {
        return Err(format!(
            "{shown} changed on disk since Studio loaded it, so nothing was written. Load the board again (Show the board as designed) and route it, or look at the change first."
        ));
    }
    let text = std::fs::read(routed).map_err(|e| format!("the routed board {}: {e}", routed.display()))?;
    if text.is_empty() {
        return Err("the routed board is empty; nothing was written".into());
    }
    replace(&board.pcb, &text)?;
    Ok(fingerprint(&text))
}

/// Replaces a file in one step, through a file beside it, so no reader sees half of it.
fn replace(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temporary = path.with_file_name(format!(".{}.studio-tmp", file_name(path)));
    let written = std::fs::write(&temporary, bytes).and_then(|()| std::fs::rename(&temporary, path));
    written.map_err(|e| {
        let _ = std::fs::remove_file(&temporary);
        format!("{}: {e}", path.display())
    })
}

/// Holds `design/status.json.lock` while it lives; made only by taking the lock.
struct Lock(PathBuf);

impl Lock {
    fn take(path: PathBuf, wait: Duration) -> Result<Lock, String> {
        let start = Instant::now();
        loop {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    let _ = writeln!(file, "studio {}", std::process::id());
                    return Ok(Lock(path));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if start.elapsed() >= wait {
                        return Err(format!(
                            "{} is held by another app; gave up after {:.0} s (remove it if nothing is writing status.json)",
                            path.display(),
                            wait.as_secs_f64()
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Merges `fields` into `apps.studio.<board>` of the project's `design/status.json`, read,
/// changed and written back under its lock file; a field given as null is taken out, so a
/// later run clears what an earlier one said. Every other key stays as it was, in its place,
/// whatever schema the file has. A project without a status file is left without one (false).
pub fn update_status(project: &Path, board: &str, fields: Map<String, Value>, wait: Duration) -> Result<bool, String> {
    let path = project.join("design/status.json");
    if !path.is_file() {
        return Ok(false);
    }
    let _lock = Lock::take(path.with_file_name("status.json.lock"), wait)?;
    let shown = path.display();
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{shown}: {e}"))?;
    let mut status: Value = serde_json::from_str(&text).map_err(|e| format!("{shown}: {e}"))?;
    let root = status.as_object_mut().ok_or(format!("{shown} is not a JSON object"))?;
    let mut entry = root;
    for key in ["apps", "studio", board] {
        entry = entry
            .entry(key)
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or(format!("{shown}: {key} is not an object, so it was left alone"))?;
    }
    for (key, value) in fields {
        if value.is_null() {
            entry.remove(&key);
        } else {
            entry.insert(key, value);
        }
    }
    let mut out = serde_json::to_string_pretty(&status).map_err(|e| e.to_string())?;
    if text.ends_with('\n') {
        out.push('\n');
    }
    replace(&path, out.as_bytes())?;
    Ok(true)
}

/// The short commit of the checkout the studio runs from, which built its router.
pub fn router_commit(root: &Path) -> Option<String> {
    let output = std::process::Command::new("git").args(["rev-parse", "--short", "HEAD"]).current_dir(root).output().ok()?;
    let commit = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (output.status.success() && !commit.is_empty()).then_some(commit)
}

/// The time now in UTC, as ISO 8601 to the second.
pub fn now_iso() -> String {
    iso(SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0))
}

fn iso(seconds: u64) -> String {
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    // Days to a civil date (Howard Hinnant's algorithm), from 1970-01-01.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rest / 3600, rest / 60 % 60, rest % 60)
}

/// A name that is one plain path component: no separators, no `.` or `..`.
pub fn plain_name(name: &str) -> bool {
    let mut parts = Path::new(name).components();
    matches!((parts.next(), parts.next()), (Some(Component::Normal(_)), None)) && !name.contains('/')
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn sorted_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    out.sort();
    out
}

fn sorted_files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_file()).collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh folder under the system's temporary directory, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Scratch {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("studio-project-test-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir.canonicalize().unwrap())
        }

        fn put(&self, relative: &str, text: &str) -> PathBuf {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// easy2350's status file as it is, in the schema before version 1: `needs_user` a string,
    /// no `schema`, keys in the order Boardroom wrote them.
    const OLD_STATUS: &str = r#"{
  "board": "easy2350",
  "phase": "10 release",
  "doing": "Routing moves to ultraroute.",
  "needs_user": "Approve the release.",
  "checks": {
    "DRC": "0 errors, 0 unconnected",
    "placement_check": "PASS"
  },
  "gates_passed": [
    0,
    1,
    9
  ],
  "links": [
    {
      "label": "Design intent",
      "href": "design/intent.md"
    }
  ],
  "updated": "2026-10-26"
}
"#;

    fn fields(opens: u64) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("opens".into(), json!(opens));
        m.insert("saved".into(), json!(false));
        m
    }

    #[test]
    fn finds_the_one_board_and_its_rules() {
        let p = Scratch::new();
        p.put("hw/easy2350/a/easy2350.kicad_pcb", "(kicad_pcb)");
        p.put("hw/easy2350/a/easy2350.kicad_pro", "{}");
        p.put("hw/easy2350/a/easy2350.kicad_dru", "(version 1)");
        p.put("hw/easy2350/a/routing.json", "{}");
        p.put("hw/easy2350/a/_autosave-easy2350.kicad_pcb", "(kicad_pcb)");
        p.put("hw/easy2350/a/route/s0.kicad_pcb", "(kicad_pcb)");
        let project = open(p.0.to_str().unwrap()).unwrap();
        let all = boards(&project);
        assert_eq!(all.len(), 1);
        let b = choose(&all, None, None).unwrap();
        assert_eq!((b.board.as_str(), b.variant.as_str()), ("easy2350", "a"));
        assert_eq!(b.pcb, project.join("hw/easy2350/a/easy2350.kicad_pcb"));
        assert!(b.project.is_some() && b.rules.is_some() && b.regions.is_some());
    }

    #[test]
    fn several_boards_need_a_choice() {
        let p = Scratch::new();
        for (board, variant) in [("main", "a"), ("main", "b"), ("aux", "a")] {
            p.put(&format!("hw/{board}/{variant}/{board}.kicad_pcb"), "(kicad_pcb)");
            p.put(&format!("hw/{board}/{variant}/{board}.kicad_pro"), "{}");
        }
        // A folder with two boards and no project for either has no board the studio can name.
        p.put("hw/odd/a/x.kicad_pcb", "");
        p.put("hw/odd/a/y.kicad_pcb", "");
        let all = boards(&p.0);
        assert_eq!(all.len(), 3);
        assert!(choose(&all, None, None).unwrap_err().contains("several"));
        assert!(choose(&all, Some("main"), None).unwrap_err().contains("several"));
        assert_eq!(choose(&all, Some("aux"), None).unwrap().variant, "a");
        assert_eq!(choose(&all, Some("main"), Some("b")).unwrap().variant, "b");
        assert!(choose(&all, Some("main"), Some("c")).unwrap_err().contains("no board main/c"));
        assert!(choose(&[], None, None).unwrap_err().contains("no boards"));
    }

    #[test]
    fn the_board_with_a_project_is_the_projects() {
        let p = Scratch::new();
        p.put("hw/b/v/old-copy.kicad_pcb", "");
        p.put("hw/b/v/real.kicad_pcb", "");
        p.put("hw/b/v/real.kicad_pro", "{}");
        assert!(boards(&p.0)[0].pcb.ends_with("real.kicad_pcb"));
    }

    #[test]
    fn a_project_is_an_absolute_folder_with_boards() {
        let p = Scratch::new();
        assert!(open(p.0.to_str().unwrap()).unwrap_err().contains("no hw/"));
        assert!(open("relative/path").unwrap_err().contains("absolute"));
        assert!(open("/no/such/project").is_err());
        assert!(plain_name("easy2350") && !plain_name("..") && !plain_name("a/b") && !plain_name("") && !plain_name("."));
    }

    #[test]
    fn status_merge_keeps_everything_else_where_it_was() {
        let p = Scratch::new();
        let path = p.put("design/status.json", OLD_STATUS);
        assert!(update_status(&p.0, "easy2350", fields(3), LOCK_WAIT).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        // The old file's text is untouched up to its last key; the studio's entry follows it.
        let old_body = OLD_STATUS.trim_end().trim_end_matches('}').trim_end();
        assert!(text.starts_with(old_body), "{text}");
        assert!(text.ends_with("}\n"));
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["needs_user"], "Approve the release.");
        assert!(v.get("schema").is_none());
        assert_eq!(v["updated"], "2026-10-26");
        assert_eq!(v["apps"]["studio"]["easy2350"], json!({"opens": 3, "saved": false}));
        assert!(!p.0.join("design/status.json.lock").exists());
    }

    #[test]
    fn a_null_field_takes_its_key_out() {
        let p = Scratch::new();
        let path = p.put("design/status.json", r#"{"apps": {"studio": {"b": {"opens": 1, "failed": "crash"}}}}"#);
        let mut fields = Map::new();
        fields.insert("opens".into(), json!(0));
        fields.insert("failed".into(), Value::Null);
        update_status(&p.0, "b", fields, LOCK_WAIT).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["apps"]["studio"]["b"], json!({"opens": 0}));
    }

    #[test]
    fn status_merge_leaves_other_apps_and_boards_alone() {
        let p = Scratch::new();
        let before = json!({
            "schema": 1,
            "needs_user": [{"id": "x", "kind": "signoff", "text": "Approve"}],
            "apps": {
                "composer": {"easy2350": {"stage": "handed_off"}},
                "studio": {"other": {"opens": 9}, "easy2350": {"opens": 7, "drc_ok": true}}
            },
            "updated": "2026-10-29T14:02:00Z"
        });
        let path = p.put("design/status.json", &serde_json::to_string_pretty(&before).unwrap());
        update_status(&p.0, "easy2350", fields(0), LOCK_WAIT).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let mut expected = before.clone();
        expected["apps"]["studio"]["easy2350"] = json!({"opens": 0, "drc_ok": true, "saved": false});
        assert_eq!(v, expected);
    }

    #[test]
    fn status_merge_gives_up_on_a_held_lock() {
        let p = Scratch::new();
        let path = p.put("design/status.json", OLD_STATUS);
        let lock = p.put("design/status.json.lock", "composer 1");
        let start = Instant::now();
        let error = update_status(&p.0, "easy2350", fields(1), Duration::from_millis(200)).unwrap_err();
        assert!(error.contains("held by another app"), "{error}");
        assert!(start.elapsed() >= Duration::from_millis(200));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), OLD_STATUS);
        // Someone else's lock is theirs to remove.
        assert_eq!(std::fs::read_to_string(&lock).unwrap(), "composer 1");
    }

    #[test]
    fn status_merge_refuses_what_it_would_have_to_clobber() {
        let p = Scratch::new();
        let path = p.put("design/status.json", r#"{"apps": {"studio": "not mine to replace"}}"#);
        assert!(update_status(&p.0, "b", fields(1), LOCK_WAIT).unwrap_err().contains("studio is not an object"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), r#"{"apps": {"studio": "not mine to replace"}}"#);
        assert!(!p.0.join("design/status.json.lock").exists());
    }

    #[test]
    fn no_status_file_is_not_made_one() {
        let p = Scratch::new();
        assert!(!update_status(&p.0, "b", fields(1), LOCK_WAIT).unwrap());
        assert!(!p.0.join("design/status.json").exists());
    }

    #[test]
    fn save_writes_over_the_board_it_loaded() {
        let p = Scratch::new();
        let pcb = p.put("hw/b/v/b.kicad_pcb", "(kicad_pcb unrouted)");
        let routed = p.put("runs/1.kicad_pcb", "(kicad_pcb routed)");
        let board = boards(&p.0).remove(0);
        let base = fingerprint(&std::fs::read(&pcb).unwrap());
        let after = save(&board, &routed, &base).unwrap();
        assert_eq!(std::fs::read_to_string(&pcb).unwrap(), "(kicad_pcb routed)");
        assert_eq!(after, fingerprint(b"(kicad_pcb routed)"));
        assert!(!p.0.join("hw/b/v/.b.kicad_pcb.studio-tmp").exists());
    }

    #[test]
    fn save_refuses_a_board_changed_on_disk() {
        let p = Scratch::new();
        let pcb = p.put("hw/b/v/b.kicad_pcb", "(kicad_pcb unrouted)");
        let routed = p.put("runs/1.kicad_pcb", "(kicad_pcb routed)");
        let board = boards(&p.0).remove(0);
        let base = fingerprint(&std::fs::read(&pcb).unwrap());
        std::fs::write(&pcb, "(kicad_pcb edited in KiCad)").unwrap();
        let error = save(&board, &routed, &base).unwrap_err();
        assert!(error.contains("changed on disk"), "{error}");
        assert_eq!(std::fs::read_to_string(&pcb).unwrap(), "(kicad_pcb edited in KiCad)");
    }

    #[test]
    fn fingerprints_and_times_are_stable() {
        assert_eq!(fingerprint(b""), "cbf29ce484222325-0");
        assert_ne!(fingerprint(b"ab"), fingerprint(b"ba"));
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(iso(1_709_164_800), "2024-02-29T00:00:00Z");
    }
}
