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

impl Rules {
    /// Reads a `.kicad_pro`, noting a `.kicad_dru` beside it.
    pub fn read(path: &Path) -> Result<Rules, RulesError> {
        let text = std::fs::read_to_string(path).map_err(|e| RulesError::Io(path.display().to_string(), e.to_string()))?;
        let mut rules = Rules::parse(&text)?;
        let dru = path.with_extension("kicad_dru");
        if let Ok(custom) = std::fs::read_to_string(&dru) {
            rules.apply_custom(&custom, &dru.display().to_string());
        }
        Ok(rules)
    }

    pub fn parse(text: &str) -> Result<Rules, RulesError> {
        let project: Value = serde_json::from_str(text).map_err(|e| RulesError::Json(e.to_string()))?;
        let rules = &project["board"]["design_settings"]["rules"];
        let mm = |v: &Value, name: &str| v[name].as_f64().unwrap_or(0.0) * MM;
        let net = &project["net_settings"];
        let classes = net["classes"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| NetClass {
                name: c["name"].as_str().unwrap_or("").to_string(),
                clearance: mm(c, "clearance"),
                track_width: mm(c, "track_width"),
                via_diameter: mm(c, "via_diameter"),
                via_drill: mm(c, "via_drill"),
                diff_pair_width: mm(c, "diff_pair_width"),
                diff_pair_gap: mm(c, "diff_pair_gap"),
                diff_pair_via_gap: mm(c, "diff_pair_via_gap"),
                priority: c["priority"].as_i64().unwrap_or(i64::MAX),
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
                .filter_map(|(net, class)| {
                    let class = class.as_str().or_else(|| class.as_array()?.first()?.as_str())?;
                    Some((net.clone(), class.to_string()))
                })
                .collect(),
            _ => Vec::new(),
        };
        Ok(Rules {
            min_clearance: mm(rules, "min_clearance"),
            min_track_width: mm(rules, "min_track_width"),
            min_via_diameter: mm(rules, "min_via_diameter"),
            min_via_drill: mm(rules, "min_through_hole_diameter"),
            min_via_annular_width: mm(rules, "min_via_annular_width"),
            min_copper_edge_clearance: mm(rules, "min_copper_edge_clearance"),
            min_hole_clearance: mm(rules, "min_hole_clearance"),
            min_hole_to_hole: mm(rules, "min_hole_to_hole"),
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
            warnings: Vec::new(),
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

    /// The class a net belongs to: assigned by name, else the matching pattern of the lowest
    /// priority, else `Default`.
    pub fn class_of(&self, net: &str) -> Option<&NetClass> {
        let named = |name: &str| self.classes.iter().find(|c| c.name == name);
        if let Some((_, class)) = self.assignments.iter().find(|(n, _)| n == net) {
            return named(class);
        }
        self.patterns
            .iter()
            .filter(|(pattern, _)| wildcard(pattern, net))
            .filter_map(|(_, class)| named(class))
            .min_by_key(|c| c.priority)
            .or_else(|| named("Default"))
    }

    /// The narrowest track `net` may have: the board minimum, or a custom rule's for the net or
    /// its class where that is wider.
    pub fn min_track_width_of(&self, net: &str) -> f64 {
        let class = self.class_of(net).map(|c| c.name.as_str());
        self.min_widths
            .iter()
            .filter(|(s, _)| match s {
                Select::Net(n) => n == net,
                Select::Class(c) => Some(c.as_str()) == class,
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
    fn custom_rules_for_holes_and_smd_pads_are_applied_and_others_reported() {
        let dir = std::env::temp_dir().join(format!("kicad-rules-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pro = dir.join("b.kicad_pro");
        std::fs::write(&pro, r#"{"board":{"design_settings":{"rules":{"min_clearance":0.2}}},"net_settings":{"classes":[]}}"#).unwrap();
        assert!(Rules::read(&pro).unwrap().warnings.is_empty());
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
        assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
        assert!(r.warnings[0].contains("Something else"));
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
        assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
        assert!(r.warnings[0].contains("rectangles"));
        r.apply_custom("(rule \"all\" (constraint edge_clearance (min 0.35mm)))", "test");
        near(r.min_copper_edge_clearance, 0.35e-3);
        near(r.edge_clearance_of("Segment"), 0.35e-3);
        near(r.edge_clearance_of("Circle"), 0.5e-3);
    }
}
