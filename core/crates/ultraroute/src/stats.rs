//! What a run did, as one record: each stage's time and work, the run's settings and provenance,
//! the connections given up, and the findings a designer acts on (connections left open, forced
//! necks, broken region rules and length limits, stages the clock cut short). `--stats FILE`
//! (or `--report FILE`) writes it as JSON; `--verbose` prints its table.

use serde::Serialize;
use serde_json::Value;

/// The router's version, and the commit it was built from when the build could ask git.
pub fn version() -> (&'static str, Option<&'static str>) {
    let git = option_env!("ULTRAROUTE_GIT").filter(|g| !g.is_empty());
    (env!("CARGO_PKG_VERSION"), git)
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RunStats {
    pub router: Router,
    pub input: Input,
    pub summary: Summary,
    pub findings: Vec<Finding>,
    pub deadline: DeadlineStats,
    pub stages: Vec<StageStats>,
    /// Each connection a stage gave up on, and why.
    pub failures: Vec<Failure>,
    pub settings: Settings,
    pub memory: Memory,
    /// The run's own bookkeeping: what counting stages' open connections and copper took.
    pub stats_seconds: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Router {
    pub version: String,
    /// The commit, `None` when built without git.
    pub commit: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Input {
    pub path: String,
    pub sha256: String,
    pub bytes: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Summary {
    /// Connections open after set-up (what the run had to make) and at the end.
    pub connections: usize,
    pub opens: usize,
    /// Connections the run made: `connections` less `opens`.
    pub routed: usize,
    /// Connections routing one at a time gave up on after negotiation (the `failed` of the
    /// `routed N more` line); later stages may still have made some of them.
    pub failed: usize,
    /// Vias on the board at the end.
    pub vias: usize,
    pub seconds: f64,
}

/// Something about the board as routed for its designer: `severity` is `error` (the board is
/// not done), `warning` (done, but look) or `info`.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub severity: &'static str,
    /// `open`, `neck`, `region`, `length`, `deadline`, or `crash`.
    pub kind: &'static str,
    pub message: String,
    /// The parts it is about, by reference, and the nets, by name.
    pub refs: Vec<String>,
    pub nets: Vec<String>,
    /// An open connection's two ends, as `grid::item_name` names them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ends: Option<[String; 2]>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DeadlineStats {
    /// The stages whose time ran out, in the order they ran.
    pub limited: Vec<String>,
    /// The stage running when the run's hard deadline passed, if it did (not by cancelling).
    pub hard_passed_in: Option<String>,
    /// How far past `--timeout` the run ended (negative: within it), when the hard deadline passed.
    pub overran_seconds: Option<f64>,
    pub cancelled: bool,
}

impl DeadlineStats {
    /// The `deadline:` lines the router prints, which the benchmark reads.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.limited.is_empty() {
            out.push(format!("deadline: time ran out in {}", self.limited.join(", ")));
        }
        if let Some(stage) = &self.hard_passed_in {
            let over = self.overran_seconds.unwrap_or(0.0);
            let by = if over > 0.0 { format!("the run overran --timeout by {over:.1} s") } else { "the run still ended within --timeout".to_string() };
            out.push(format!("deadline: the run's hard limit passed during {stage}; {by}"));
        }
        out
    }
}

/// One stage of the run.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StageStats {
    pub name: &'static str,
    /// Seconds since the run began when it started, and how long it took.
    pub started: f64,
    pub seconds: f64,
    /// Whether the clock stopped it short.
    pub cut: bool,
    pub open_before: usize,
    pub open_after: usize,
    /// Tracks and vias that came and went (a piece moved counts both ways), and the vias on the
    /// board after.
    pub laid: usize,
    pub ripped: usize,
    pub vias: usize,
    pub work: Work,
    /// The stage's own figures, as it reports them.
    pub detail: Value,
}

/// The work counted while a stage ran (`grid::work`): the process's, so a process routing two
/// boards at once counts both.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Work {
    pub astar: u64,
    pub astar_expanded: u64,
    pub fields: u64,
    pub field_settled: u64,
    pub maps: u64,
    pub checks: u64,
    pub connectivity: u64,
}

impl From<grid::work::Work> for Work {
    fn from(w: grid::work::Work) -> Work {
        Work {
            astar: w.astar,
            astar_expanded: w.astar_expanded,
            fields: w.fields,
            field_settled: w.field_settled,
            maps: w.maps,
            checks: w.checks,
            connectivity: w.connectivity,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Failure {
    pub stage: &'static str,
    pub net: String,
    pub from: String,
    pub to: String,
    pub reason: &'static str,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Settings {
    /// The command line as the router read it, the recipe's flags put in.
    pub flags: Vec<String>,
    pub seed: u64,
    pub uuid_seed: Option<u64>,
    pub threads: usize,
    pub cell_mm: f64,
    pub timeout: Option<f64>,
    /// Grid nodes at that cell over every copper layer.
    pub nodes: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Memory {
    pub estimate_mb: u64,
    /// The most the process held, where the system says.
    pub peak_mb: Option<u64>,
}

impl RunStats {
    /// The table `--verbose` prints, one stage a line.
    pub fn table(&self) -> Vec<String> {
        let mut out = vec![format!(
            "stats: {:<16} {:>8} {:>6} {:>6} {:>6} {:>6} {:>5} {:>8} {:>11} {:>8} {:>5} {:>4}",
            "stage", "seconds", "open<", "open>", "laid", "ripped", "vias", "searches", "expanded", "checks", "maps", "cut"
        )];
        for s in &self.stages {
            out.push(format!(
                "stats: {:<16} {:>8.2} {:>6} {:>6} {:>6} {:>6} {:>5} {:>8} {:>11} {:>8} {:>5} {:>4}",
                s.name,
                s.seconds,
                s.open_before,
                s.open_after,
                s.laid,
                s.ripped,
                s.vias,
                s.work.astar + s.work.fields,
                s.work.astar_expanded + s.work.field_settled,
                s.work.checks,
                s.work.maps,
                if s.cut { "yes" } else { "" }
            ));
        }
        let m = &self.summary;
        out.push(format!(
            "stats: {} of {} connections routed, {} open, {} vias, {} findings, {:.1} s ({:.2} s counting)",
            m.routed,
            m.connections,
            m.opens,
            m.vias,
            self.findings.len(),
            m.seconds,
            self.stats_seconds
        ));
        out
    }
}

/// The SHA-256 of `data`, in hex.
pub fn sha256(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
        0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
        0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            (hh, g, f, e, d, c, b, a) = (g, f, e, d.wrapping_add(t1), c, b, a, t1.wrapping_add(t2));
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_standard_vectors() {
        assert_eq!(sha256(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha256(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(sha256(&[b'a'; 1000]), "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3");
    }

    #[test]
    fn the_deadline_lines_read_as_the_benchmark_expects() {
        let d = DeadlineStats { limited: vec!["negotiation".into(), "rip-up".into()], hard_passed_in: Some("shove".into()), overran_seconds: Some(1.26), cancelled: false };
        assert_eq!(d.lines(), ["deadline: time ran out in negotiation, rip-up", "deadline: the run's hard limit passed during shove; the run overran --timeout by 1.3 s"]);
        let within = DeadlineStats { hard_passed_in: Some("vias".into()), overran_seconds: Some(-0.5), ..Default::default() };
        assert_eq!(within.lines(), ["deadline: the run's hard limit passed during vias; the run still ended within --timeout"]);
        assert!(DeadlineStats::default().lines().is_empty());
    }
}
