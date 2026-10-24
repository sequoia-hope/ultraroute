//! Design rules and net classes from a `.kicad_pro`, in metres.
//!
//! Custom rules in a `.kicad_dru` file are not applied; [`Rules::read`] says when one is there.

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
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetClass {
    pub name: String,
    pub clearance: f64,
    pub track_width: f64,
    pub via_diameter: f64,
    pub via_drill: f64,
    /// Lower wins when several classes match one net.
    pub priority: i64,
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
        if path.with_extension("kicad_dru").is_file() {
            rules.warnings.push(format!(
                "{} has custom rules; they are not applied",
                path.with_extension("kicad_dru").display()
            ));
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
            warnings: Vec::new(),
        })
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

    /// The clearance the router must keep between two nets' copper: the larger of their
    /// classes' clearances, and never less than the board minimum.
    pub fn clearance(&self, a: &str, b: &str) -> f64 {
        let of = |net: &str| self.class_of(net).map_or(0.0, |c| c.clearance);
        of(a).max(of(b)).max(self.min_clearance)
    }
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
    fn a_custom_rules_file_is_reported() {
        let dir = std::env::temp_dir().join(format!("kicad-rules-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pro = dir.join("b.kicad_pro");
        std::fs::write(&pro, r#"{"board":{"design_settings":{"rules":{"min_clearance":0.2}}},"net_settings":{"classes":[]}}"#).unwrap();
        assert!(Rules::read(&pro).unwrap().warnings.is_empty());
        std::fs::write(dir.join("b.kicad_dru"), "(version 1)").unwrap();
        assert_eq!(Rules::read(&pro).unwrap().warnings.len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
