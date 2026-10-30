//! Design rules and net classes from a `.kicad_pro`, in metres, and the custom rules of a
//! `.kicad_dru` beside it that the router understands: the stricter clearances a fab sets for
//! through-hole pad holes and between SMD pads, the narrowest track a net or net class may
//! have, and the edge clearance of Edge.Cuts circles or arcs. [`Rules::read`] warns of any other
//! custom rule, which is not applied.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

const MM: f64 = 1e-3;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rules {
    pub min_clearance: f64,
    pub min_track_width: f64,
    pub min_via_diameter: f64,
    pub min_via_drill: f64,
    pub min_via_annular_width: f64,
    pub min_copper_edge_clearance: f64,
    pub min_hole_clearance: f64,
    pub min_hole_to_hole: f64,
    pub allow_blind_buried_vias: bool,
    pub allow_microvias: bool,
    pub classes: Vec<NetClass>,
    /// Name patterns (`*` and `?` wildcards) assigning nets to classes.
    pub patterns: Vec<(String, String)>,
    /// Nets assigned to a class by name.
    pub assignments: Vec<(String, String)>,
    /// From custom rules: hole clearance where either item is a through-hole pad, hole-to-hole
    /// between two through-hole pads, and clearance between two SMD pads; 0 where unset.
    #[serde(default)]
    pub pth_hole_clearance: f64,
    #[serde(default)]
    pub pth_hole_to_hole: f64,
    #[serde(default)]
    pub smd_pad_clearance: f64,
    /// From custom rules: the narrowest track of a net (`Net`) or of a class's nets (`Class`).
    #[serde(default)]
    pub min_widths: Vec<(Select, f64)>,
    /// From custom rules: the edge clearance of Edge.Cuts primitives of one shape, by KiCad's
    /// name for it (`Circle` or `Arc`), where it is more than the board's.
    #[serde(default)]
    pub edge_clearances: Vec<(String, f64)>,
    /// How far other nets' copper keeps outside a solder mask opening (KiCad's "solder mask to
    /// copper clearance"); 0, KiCad's default, where the project leaves it out.
    #[serde(default)]
    pub solder_mask_to_copper_clearance: f64,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetClass {
    pub name: String,
    pub clearance: f64,
    pub track_width: f64,
    pub via_diameter: f64,
    pub via_drill: f64,
    /// Each track of a differential pair, the gap between them, and between their vias; 0 unset.
    pub diff_pair_width: f64,
    pub diff_pair_gap: f64,
    pub diff_pair_via_gap: f64,
    /// Lower wins when several classes match one net.
    pub priority: i64,
    /// The fields above the project leaves out of this class (bits in the order they are listed,
    /// `clearance` first), which a net takes from its next class instead, as KiCad 9 does.
    #[serde(default)]
    pub unset: u8,
}

impl NetClass {
    const FIELDS: usize = 7;

    fn field(&mut self, k: usize) -> &mut f64 {
        match k {
            0 => &mut self.clearance,
            1 => &mut self.track_width,
            2 => &mut self.via_diameter,
            3 => &mut self.via_drill,
            4 => &mut self.diff_pair_width,
            5 => &mut self.diff_pair_gap,
            _ => &mut self.diff_pair_via_gap,
        }
    }

    fn get(&self, k: usize) -> f64 {
        [self.clearance, self.track_width, self.via_diameter, self.via_drill, self.diff_pair_width, self.diff_pair_gap, self.diff_pair_via_gap][k]
    }
}

/// The nets a custom rule holds for: one by name, or every net of a class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Select {
    Net(String),
    Class(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RulesError {
    #[error("{0}: {1}")]
    Io(String, String),
    #[error("not a KiCad project: {0}")]
    Json(String),
}

/// KiCad 9's board rules where a project file has none, in millimetres: what a new board has
/// (read from pcbnew 9.0.8's `BOARD().GetDesignSettings()`).
const KICAD_RULES: [(&str, f64); 8] = [
    ("min_clearance", 0.0),
    ("min_track_width", 0.0),
    ("min_via_diameter", 0.5),
    ("min_through_hole_diameter", 0.3),
    ("min_via_annular_width", 0.1),
    ("min_copper_edge_clearance", 0.5),
    ("min_hole_clearance", 0.25),
    ("min_hole_to_hole", 0.25),
];

/// KiCad 9's Default net class, for a project that has none.
const KICAD_DEFAULT_CLASS: NetClass = NetClass {
    name: String::new(),
    clearance: 0.2 * MM,
    track_width: 0.2 * MM,
    via_diameter: 0.6 * MM,
    via_drill: 0.3 * MM,
    diff_pair_width: 0.2 * MM,
    diff_pair_gap: 0.25 * MM,
    diff_pair_via_gap: 0.25 * MM,
    priority: i64::MAX,
    unset: 0,
};

impl Rules {
    /// Reads a `.kicad_pro`, noting a `.kicad_dru` beside it. With no project there, the rules
    /// are KiCad's defaults, as KiCad opens a board alone.
    pub fn read(path: &Path) -> Result<Rules, RulesError> {
        let mut rules = match std::fs::read_to_string(path) {
            Ok(text) => Rules::parse(&text)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut rules = Rules::parse("{}")?;
                rules.warnings = vec![format!("{}: not found; KiCad's default rules are used", path.display())];
                rules
            }
            Err(e) => return Err(RulesError::Io(path.display().to_string(), e.to_string())),
        };
        let dru = path.with_extension("kicad_dru");
        if let Ok(custom) = std::fs::read_to_string(&dru) {
            rules.apply_custom(&custom, &dru.display().to_string());
        }
        Ok(rules)
    }

    pub fn parse(text: &str) -> Result<Rules, RulesError> {
        let project: Value = serde_json::from_str(text).map_err(|e| RulesError::Json(e.to_string()))?;
        let rules = &project["board"]["design_settings"]["rules"];
        // A rule the project leaves out is KiCad's, as its own check takes it, not zero.
        let mut assumed: Vec<String> = Vec::new();
        let mut rule = |name: &str| match rules[name].as_f64() {
            Some(v) => v * MM,
            None => {
                let v = KICAD_RULES.iter().find(|(n, _)| *n == name).map_or(0.0, |(_, v)| *v);
                assumed.push(format!("{name} {v} mm"));
                v * MM
            }
        };
        let net = &project["net_settings"];
        const FIELDS: [&str; NetClass::FIELDS] = ["clearance", "track_width", "via_diameter", "via_drill", "diff_pair_width", "diff_pair_gap", "diff_pair_via_gap"];
        let mut classes: Vec<NetClass> = net["classes"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| {
                let name = c["name"].as_str().unwrap_or("").to_string();
                let mut class = NetClass { name, priority: c["priority"].as_i64().unwrap_or(i64::MAX), unset: 0, ..KICAD_DEFAULT_CLASS };
                for (k, field) in FIELDS.iter().enumerate() {
                    match c[*field].as_f64() {
                        Some(v) => *class.field(k) = v * MM,
                        // Default's own gaps are KiCad's; another class's are its next class's.
                        None if class.name == "Default" => {}
                        None => {
                            *class.field(k) = 0.0;
                            class.unset |= 1 << k;
                        }
                    }
                }
                class
            })
            .collect();
        let pairs = |v: &Value, key: &str| -> Vec<(String, String)> {
            v.as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| Some((p[key].as_str()?.to_string(), p["netclass"].as_str()?.to_string())))
                .collect()
        };
        let assignments = match &net["netclass_assignments"] {
            Value::Object(map) => map
                .iter()
                .flat_map(|(net, class)| {
                    let names: Vec<&str> = match class {
                        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
                        v => v.as_str().into_iter().collect(),
                    };
                    names.into_iter().map(|c| (net.clone(), c.to_string())).collect::<Vec<_>>()
                })
                .collect(),
            _ => Vec::new(),
        };
        let (min_clearance, min_track_width, min_via_diameter, min_via_drill) =
            (rule("min_clearance"), rule("min_track_width"), rule("min_via_diameter"), rule("min_through_hole_diameter"));
        let (min_via_annular_width, min_copper_edge_clearance, min_hole_clearance, min_hole_to_hole) =
            (rule("min_via_annular_width"), rule("min_copper_edge_clearance"), rule("min_hole_clearance"), rule("min_hole_to_hole"));
        let mut warnings = Vec::new();
        if !assumed.is_empty() {
            warnings.push(format!("the project sets no {}: KiCad's values are used", assumed.join(", ")));
        }
        if !classes.iter().any(|c| c.name == "Default") {
            classes.push(NetClass { name: "Default".to_string(), ..KICAD_DEFAULT_CLASS });
            warnings.push("the project has no Default net class: KiCad's is used (clearance 0.2 mm, track 0.2 mm, via 0.6 mm drilled 0.3 mm)".to_string());
        }
        Ok(Rules {
            min_clearance,
            min_track_width,
            min_via_diameter,
            min_via_drill,
            min_via_annular_width,
            min_copper_edge_clearance,
            min_hole_clearance,
            min_hole_to_hole,
            allow_blind_buried_vias: rules["allow_blind_buried_vias"].as_bool().unwrap_or(false),
            allow_microvias: rules["allow_microvias"].as_bool().unwrap_or(false),
            classes,
            patterns: pairs(&net["netclass_patterns"], "pattern"),
            assignments,
            pth_hole_clearance: 0.0,
            pth_hole_to_hole: 0.0,
            smd_pad_clearance: 0.0,
            min_widths: Vec::new(),
            edge_clearances: Vec::new(),
            solder_mask_to_copper_clearance: rules["solder_mask_to_copper_clearance"].as_f64().unwrap_or(0.0) * MM,
            warnings,
        })
    }

    /// Applies the custom rules the router understands from a `.kicad_dru`'s text, and warns of
    /// each other rule, which is not applied. A rule is understood when its condition is one of
    /// these, spaces aside, with a `min` constraint of that kind:
    /// `A.Pad_Type == 'Through-hole'` with `hole_clearance`;
    /// `A.Pad_Type == 'Through-hole' && B.Pad_Type == 'Through-hole'` with `hole_to_hole`;
    /// `A.Pad_Type == 'SMD' && B.Pad_Type == 'SMD'` with `clearance`;
    /// `A.NetName == 'X'` or `A.NetClass == 'X'`, or several joined by `||`, with `track_width`;
    /// `B.Shape == 'Circle'` or `'Arc'`, with `B.Layer == 'Edge.Cuts'` or not, with
    /// `edge_clearance` (for Edge.Cuts primitives of that shape), or no condition at all, which
    /// raises the board's edge clearance.
    /// KiCad tries a rule both ways round, so the first holds wherever either item is a
    /// through-hole pad.
    pub fn apply_custom(&mut self, text: &str, source: &str) {
        for rule in forms(text, "rule") {
            let name = quoted(&rule).unwrap_or_default();
            let condition = forms(&rule, "condition").first().and_then(|c| quoted(c)).unwrap_or_default();
            let condition: String = condition.chars().filter(|c| !c.is_whitespace()).collect();
            let constraints = forms(&rule, "constraint");
            let mut understood = !constraints.is_empty();
            for constraint in &constraints {
                let kind = constraint.trim_start_matches("(constraint").split_whitespace().next().unwrap_or("").to_string();
                let min = forms(constraint, "min").first().and_then(|m| length(m.trim_start_matches("(min").trim_end_matches(')')));
                if kind == "edge_clearance" {
                    match (condition.as_str(), edge_shape(&condition), min) {
                        ("", _, Some(v)) => self.min_copper_edge_clearance = self.min_copper_edge_clearance.max(v),
                        (_, Some(shape), Some(v)) => self.edge_clearances.push((shape, v)),
                        _ => understood = false,
                    }
                    continue;
                }
                if kind == "track_width" {
                    match (selects(&condition), min) {
                        (Some(nets), Some(v)) => self.min_widths.extend(nets.into_iter().map(|n| (n, v))),
                        _ => understood = false,
                    }
                    continue;
                }
                let slot = match (condition.as_str(), kind.as_str()) {
                    ("A.Pad_Type=='Through-hole'", "hole_clearance") => Some(&mut self.pth_hole_clearance),
                    ("A.Pad_Type=='Through-hole'&&B.Pad_Type=='Through-hole'", "hole_to_hole") => Some(&mut self.pth_hole_to_hole),
                    ("A.Pad_Type=='SMD'&&B.Pad_Type=='SMD'", "clearance") => Some(&mut self.smd_pad_clearance),
                    _ => None,
                };
                match (slot, min) {
                    (Some(slot), Some(v)) => *slot = slot.max(v),
                    _ => understood = false,
                }
            }
            if !understood {
                self.warnings.push(format!("{source}: custom rule \"{name}\" is not applied"));
            }
        }
    }

    /// The classes a net is in, as KiCad 9 finds them: every class assigned to it by name and
    /// every class whose pattern matches it, the lowest priority first, ties in the project's
    /// order of classes. `Default` is not among them; it stands behind them all.
    pub fn classes_of(&self, net: &str) -> Vec<&NetClass> {
        let named = self.assignments.iter().filter(|(n, _)| n == net).map(|(_, c)| c);
        let matched = self.patterns.iter().filter(|(pattern, _)| wildcard(pattern, net)).map(|(_, c)| c);
        let mut found: Vec<(usize, &NetClass)> = Vec::new();
        for name in named.chain(matched) {
            if let Some(k) = self.classes.iter().position(|c| &c.name == name && c.name != "Default") {
                if !found.iter().any(|(j, _)| *j == k) {
                    found.push((k, &self.classes[k]));
                }
            }
        }
        found.sort_by_key(|&(k, c)| (c.priority, k));
        found.into_iter().map(|(_, c)| c).collect()
    }

    /// The class a net routes by, as KiCad 9 makes it: each field from the first of the net's
    /// classes that sets it, else from `Default`; named, as KiCad names it, by the net's classes
    /// joined with commas, `Default` last where it gave a field (alone for a net in no class).
    pub fn class_of(&self, net: &str) -> Option<NetClass> {
        let default = self.classes.iter().find(|c| c.name == "Default");
        let found = self.classes_of(net);
        let Some(first) = found.first() else { return default.cloned() };
        let mut class = (*first).clone();
        let mut names: Vec<&str> = found.iter().map(|c| c.name.as_str()).collect();
        let take = |class: &mut NetClass, next: &NetClass| {
            for k in 0..NetClass::FIELDS {
                if class.unset & !next.unset & (1 << k) != 0 {
                    *class.field(k) = next.get(k);
                    class.unset &= !(1 << k);
                }
            }
        };
        for next in &found[1..] {
            take(&mut class, next);
        }
        if let Some(default) = default.filter(|_| class.unset != 0) {
            take(&mut class, default);
            names.push("Default");
        }
        class.name = names.join(",");
        Some(class)
    }

    /// Whether `class` names a class of `net`, as a rule's `A.NetClass == 'X'` tests it in KiCad
    /// 9: any class the net is in, `Default` where it stands behind them, or the whole name.
    pub fn in_class(&self, net: &str, class: &str) -> bool {
        self.class_of(net).is_some_and(|c| c.name == class || c.name.split(',').any(|n| n == class))
    }

    /// The narrowest track `net` may have: the board minimum, or a custom rule's for the net or
    /// its class where that is wider.
    pub fn min_track_width_of(&self, net: &str) -> f64 {
        self.min_widths
            .iter()
            .filter(|(s, _)| match s {
                Select::Net(n) => n == net,
                Select::Class(c) => self.in_class(net, c),
            })
            .map(|(_, v)| *v)
            .fold(self.min_track_width, f64::max)
    }

    /// The clearance copper keeps from an Edge.Cuts primitive of KiCad's `shape` (`Segment`,
    /// `Arc`, `Circle`): the board's, or a custom rule's for the shape where that is more.
    pub fn edge_clearance_of(&self, shape: &str) -> f64 {
        self.edge_clearances.iter().filter(|(s, _)| s == shape).map(|(_, v)| *v).fold(self.min_copper_edge_clearance, f64::max)
    }

    /// The clearance the router must keep between two nets' copper: the larger of their
    /// classes' clearances, and never less than the board minimum.
    pub fn clearance(&self, a: &str, b: &str) -> f64 {
        let of = |net: &str| self.class_of(net).map_or(0.0, |c| c.clearance);
        of(a).max(of(b)).max(self.min_clearance)
    }
}

/// The nets a condition (spaces removed) selects: `A.NetName=='X'` and `A.NetClass=='X'` terms
/// joined by `||`; `None` for any other condition.
fn selects(condition: &str) -> Option<Vec<Select>> {
    condition
        .split("||")
        .map(|term| {
            let quoted = |prefix: &str| term.strip_prefix(prefix)?.strip_prefix('\'')?.strip_suffix('\'').map(String::from);
            quoted("A.NetName==").map(Select::Net).or_else(|| quoted("A.NetClass==").map(Select::Class))
        })
        .collect()
}

/// The Edge.Cuts shape a condition (spaces removed) picks: one `Shape=='Circle'` or
/// `Shape=='Arc'` term of either item, and at most a `Layer=='Edge.Cuts'` term besides, joined by
/// `&&`; `None` for any other condition. KiCad draws rectangles and polygons as lines here, so a
/// rule for those, or for `Segment`, cannot be told apart and is not taken.
fn edge_shape(condition: &str) -> Option<String> {
    let mut shape = None;
    for term in condition.split("&&") {
        let (_, rest) = term.split_once('.').filter(|(item, _)| matches!(*item, "A" | "B"))?;
        match rest.split_once("==")? {
            ("Layer", "'Edge.Cuts'") => {}
            ("Shape", v @ ("'Circle'" | "'Arc'")) if shape.is_none() => shape = Some(v.trim_matches('\'').to_string()),
            _ => return None,
        }
    }
    shape
}

/// `*` matches any run of characters, `?` any one.
fn wildcard(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Each `(head ...)` form in `text`, at any depth, outside strings.
fn forms(text: &str, head: &str) -> Vec<String> {
    let open = format!("({head}");
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(at) = text[i..].find(&open).map(|k| k + i) {
        let after = bytes.get(at + open.len()).copied().unwrap_or(b' ');
        if after.is_ascii_alphanumeric() || after == b'_' {
            i = at + open.len();
            continue;
        }
        let (mut depth, mut j, mut in_string) = (0i32, at, false);
        while j < bytes.len() {
            match bytes[j] {
                b'"' => in_string = !in_string,
                b'(' if !in_string => depth += 1,
                b')' if !in_string => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        out.push(text[at..=j.min(bytes.len() - 1)].to_string());
        i = at + open.len();
    }
    out
}

/// The first quoted string in `text`.
fn quoted(text: &str) -> Option<String> {
    let start = text.find('"')? + 1;
    let end = text[start..].find('"')? + start;
    Some(text[start..end].to_string())
}

/// A length such as `0.28mm`, `0.28 mm` or `11mil`, in metres.
fn length(text: &str) -> Option<f64> {
    let t = text.trim();
    let (number, unit) = t.split_at(t.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(t.len()));
    let v: f64 = number.trim().parse().ok()?;
    match unit.trim() {
        "" | "mm" => Some(v * MM),
        "mil" => Some(v * 25.4e-6),
        "in" => Some(v * 25.4e-3),
        "um" => Some(v * 1e-6),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards_match_runs_and_single_characters() {
        assert!(wildcard("USB_C_D*", "USB_C_D+"));
        assert!(wildcard("USB_C_D*", "USB_C_D"));
        assert!(!wildcard("USB_C_D*", "USB_C"));
        assert!(wildcard("V?US", "VBUS"));
        assert!(wildcard("GND", "GND"));
        assert!(!wildcard("GND", "AGND"));
        assert!(wildcard("*GND", "AGND"));
    }

    #[test]
    fn classes_come_from_patterns_then_default() {
        let rules = Rules::read(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/bluebuzz-rm2.kicad_pro")).unwrap();
        let near = |a: f64, b: f64| assert!((a - b).abs() < 1e-12, "{a} vs {b}");
        near(rules.min_clearance, 0.127e-3);
        near(rules.min_copper_edge_clearance, 0.3e-3);
        assert_eq!(rules.class_of("VBUS").unwrap().name, "Vbus");
        near(rules.class_of("VBUS").unwrap().track_width, 0.65e-3);
        assert_eq!(rules.class_of("GND").unwrap().name, "Power");
        assert_eq!(rules.class_of("SOMETHING_ELSE").unwrap().name, "Default");
        near(rules.clearance("GND", "VBUS"), 0.127e-3);
    }

    #[test]
    fn rules_a_project_leaves_out_are_kicads_and_said_so() {
        let r = Rules::parse("{}").unwrap();
        let near = |a: f64, b: f64| assert!((a - b).abs() < 1e-12, "{a} vs {b}");
        near(r.min_copper_edge_clearance, 0.5e-3);
        near(r.min_hole_clearance, 0.25e-3);
        near(r.min_hole_to_hole, 0.25e-3);
        near(r.min_via_diameter, 0.5e-3);
        near(r.min_via_drill, 0.3e-3);
        near(r.min_clearance, 0.0);
        let default = r.class_of("ANY").expect("a Default class");
        near(default.clearance, 0.2e-3);
        near(default.track_width, 0.2e-3);
        assert_eq!(r.warnings.len(), 2, "{:?}", r.warnings);
        assert!(r.warnings[0].contains("min_copper_edge_clearance 0.5 mm"), "{:?}", r.warnings);
        assert!(r.warnings[1].contains("no Default net class"), "{:?}", r.warnings);
        // A project that sets them all, with its own Default class, is taken as it is, silently.
        let full = r#"{"board":{"design_settings":{"rules":{"min_clearance":0.1,"min_track_width":0.1,"min_via_diameter":0.4,"min_through_hole_diameter":0.2,"min_via_annular_width":0.05,"min_copper_edge_clearance":0.3,"min_hole_clearance":0.2,"min_hole_to_hole":0.2}}},"net_settings":{"classes":[{"name":"Default","clearance":0.15,"track_width":0.15}]}}"#;
        let r = Rules::parse(full).unwrap();
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        near(r.min_copper_edge_clearance, 0.3e-3);
    }

    /// KiCad 9.0.8's own answers (`GetEffectiveNetClass`, and kicad-cli DRC for `A.NetClass`) on
    /// the same projects, 2026-10-30.
    #[test]
    fn a_net_in_several_classes_takes_each_field_from_the_first_that_sets_it() {
        let near = |a: f64, b: f64| assert!((a - b).abs() < 1e-12, "{a} vs {b}");
        let v4 = r#"{"net_settings":{"meta":{"version":4},"classes":[
            {"name":"Default","priority":2147483647,"clearance":0.2,"track_width":0.2,"via_diameter":0.4,"via_drill":0.2},
            {"name":"A","priority":0,"clearance":0.3},
            {"name":"B","priority":1,"clearance":0.1,"track_width":0.5,"via_diameter":0.8},
            {"name":"C","priority":5,"clearance":0.4,"track_width":0.35}],
            "netclass_patterns":[{"netclass":"B","pattern":"3V3"},{"netclass":"A","pattern":"3*"},{"netclass":"C","pattern":"BTN*"}],
            "netclass_assignments":{"BOOTSEL":["B","A"],"BTN_N":"B"}}}"#;
        let r = Rules::parse(v4).unwrap();
        for (net, name, clearance, track, via) in [
            ("3V3", "A,B,Default", 0.3, 0.5, 0.8),
            ("BOOTSEL", "A,B,Default", 0.3, 0.5, 0.8),
            ("BTN_N", "B,C,Default", 0.1, 0.5, 0.8),
            ("BOOTSEL_SW", "Default", 0.2, 0.2, 0.4),
        ] {
            let c = r.class_of(net).unwrap();
            assert_eq!(c.name, name, "{net}");
            near(c.clearance, clearance * MM);
            near(c.track_width, track * MM);
            near(c.via_diameter, via * MM);
        }
        near(r.class_of("3V3").unwrap().via_drill, 0.2 * MM);
        for (class, is) in [("A", true), ("B", true), ("Default", true), ("A,B,Default", true), ("A,B", false), ("C", false)] {
            assert_eq!(r.in_class("3V3", class), is, "3V3 in {class}");
        }
        // A KiCad 8 project has no priorities: ties go by the order of its classes, and a class
        // that sets every field leaves nothing to Default.
        let v3 = |first: &str, second: &str| {
            let class = |name: &str| match name {
                "Z" => r#"{"name":"Z","clearance":0.3,"track_width":0.25,"via_diameter":0.4,"via_drill":0.2,"diff_pair_width":0.2,"diff_pair_gap":0.25,"diff_pair_via_gap":0.25}"#,
                _ => r#"{"name":"B","clearance":0.1,"track_width":0.5,"via_diameter":0.4,"via_drill":0.2,"diff_pair_width":0.2,"diff_pair_gap":0.25,"diff_pair_via_gap":0.25}"#,
            };
            format!(
                r#"{{"net_settings":{{"meta":{{"version":3}},"classes":[{{"name":"Default","clearance":0.2,"track_width":0.2}},{},{}],
                "netclass_patterns":[{{"netclass":"B","pattern":"3V3"}},{{"netclass":"Z","pattern":"3*"}}]}}}}"#,
                class(first),
                class(second)
            )
        };
        let zb = Rules::parse(&v3("Z", "B")).unwrap().class_of("3V3").unwrap();
        assert_eq!(zb.name, "Z,B");
        near(zb.clearance, 0.3 * MM);
        let bz = Rules::parse(&v3("B", "Z")).unwrap().class_of("3V3").unwrap();
        assert_eq!(bz.name, "B,Z");
        near(bz.clearance, 0.1 * MM);
    }

    #[test]
    fn a_board_alone_takes_kicads_defaults_and_its_custom_rules() {
        let near = |a: f64, b: f64| assert!((a - b).abs() < 1e-12, "{a} vs {b}");
        let dir = std::env::temp_dir().join(format!("kicad-alone-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pro = dir.join("b.kicad_pro");
        let r = Rules::read(&pro).unwrap();
        near(r.min_copper_edge_clearance, 0.5e-3);
        near(r.class_of("GND").unwrap().clearance, 0.2e-3);
        assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
        assert!(r.warnings[0].contains("not found; KiCad's default rules"), "{:?}", r.warnings);
        std::fs::write(dir.join("b.kicad_dru"), "(version 1)\n(rule \"w\" (condition \"A.NetName == 'VBUS'\") (constraint track_width (min 0.5mm)))\n").unwrap();
        near(Rules::read(&pro).unwrap().min_track_width_of("VBUS"), 0.5e-3);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn solder_mask_to_copper_clearance_is_read() {
        let near = |a: f64, b: f64| assert!((a - b).abs() < 1e-12, "{a} vs {b}");
        near(Rules::parse("{}").unwrap().solder_mask_to_copper_clearance, 0.0);
        let r = Rules::parse(r#"{"board":{"design_settings":{"rules":{"solder_mask_to_copper_clearance":0.005}}}}"#).unwrap();
        near(r.solder_mask_to_copper_clearance, 0.005e-3);
    }

    #[test]
    fn custom_rules_for_holes_and_smd_pads_are_applied_and_others_reported() {
        let dir = std::env::temp_dir().join(format!("kicad-rules-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pro = dir.join("b.kicad_pro");
        std::fs::write(&pro, r#"{"board":{"design_settings":{"rules":{"min_clearance":0.2}}},"net_settings":{"classes":[]}}"#).unwrap();
        // Only the rules it leaves out are said, none of a custom rule.
        assert!(!Rules::read(&pro).unwrap().warnings.iter().any(|w| w.contains("custom rule")));
        std::fs::write(
            dir.join("b.kicad_dru"),
            r#"(version 1)
# A comment.
(rule "JLC PTH hole to copper"
  (condition "A.Pad_Type == 'Through-hole'")
  (constraint hole_clearance (min 0.28mm)))
(rule "JLC PTH hole to hole"
  (condition "A.Pad_Type == 'Through-hole' && B.Pad_Type == 'Through-hole'")
  (constraint hole_to_hole (min 0.45mm)))
(rule "JLC SMD pad to pad"
  (condition "A.Pad_Type == 'SMD' && B.Pad_Type == 'SMD'")
  (constraint clearance (min 0.15mm)))
(rule "Power width"
  (condition "A.NetClass == 'Power'")
  (constraint track_width (min 0.3mm)))
(rule "VBUS current width"
  (condition "A.NetName == 'VBUS' || A.NetName == 'VSYS'")
  (constraint track_width (min 0.2mm)))
(rule "Something else"
  (condition "A.NetName == 'X'")
  (constraint length (max 10mm)))"#,
        )
        .unwrap();
        let r = Rules::read(&pro).unwrap();
        let near = |a: f64, b: f64| assert!((a - b).abs() < 1e-12, "{a} vs {b}");
        near(r.pth_hole_clearance, 0.28e-3);
        near(r.pth_hole_to_hole, 0.45e-3);
        near(r.smd_pad_clearance, 0.15e-3);
        let custom: Vec<&String> = r.warnings.iter().filter(|w| w.contains("custom rule")).collect();
        assert_eq!(custom.len(), 1, "{:?}", r.warnings);
        assert!(custom[0].contains("Something else"));
        assert_eq!(r.min_widths.len(), 3);
        near(r.min_track_width_of("VBUS"), 0.2e-3);
        near(r.min_track_width_of("VSYS"), 0.2e-3);
        near(r.min_track_width_of("OTHER"), 0.0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn edge_clearance_rules_pick_edge_cuts_shapes() {
        let mut r = Rules::parse(r#"{"board":{"design_settings":{"rules":{"min_copper_edge_clearance":0.3}}}}"#).unwrap();
        r.apply_custom(
            r#"(version 1)
(rule "cast hole clearance"
  (constraint edge_clearance (min 0.5mm))
  (condition "B.Layer == 'Edge.Cuts' && B.Shape == 'Circle'"))
(rule "arcs"
  (condition "A.Shape == 'Arc'")
  (constraint edge_clearance (min 0.4mm)))
(rule "rectangles"
  (condition "B.Shape == 'Rectangle'")
  (constraint edge_clearance (min 0.6mm)))"#,
            "test",
        );
        let near = |a: f64, b: f64| assert!((a - b).abs() < 1e-12, "{a} vs {b}");
        near(r.edge_clearance_of("Circle"), 0.5e-3);
        near(r.edge_clearance_of("Arc"), 0.4e-3);
        near(r.edge_clearance_of("Segment"), 0.3e-3);
        let custom: Vec<&String> = r.warnings.iter().filter(|w| w.contains("custom rule")).collect();
        assert_eq!(custom.len(), 1, "{:?}", r.warnings);
        assert!(custom[0].contains("rectangles"));
        r.apply_custom("(rule \"all\" (constraint edge_clearance (min 0.35mm)))", "test");
        near(r.min_copper_edge_clearance, 0.35e-3);
        near(r.edge_clearance_of("Segment"), 0.35e-3);
        near(r.edge_clearance_of("Circle"), 0.5e-3);
    }
}
