//! Design rules and net classes from a `.kicad_pro`, in metres, and the text of the `.kicad_dru`
//! beside it, which the board reads with [`crate::dru`].

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
    /// The custom rules file beside the project, as `(path, text)`: the board reads it
    /// (`board::custom`).
    #[serde(default)]
    pub custom: Option<(String, String)>,
    /// How far other nets' copper keeps outside a solder mask opening (KiCad's "solder mask to
    /// copper clearance"); 0, KiCad's default, where the project leaves it out.
    #[serde(default)]
    pub solder_mask_to_copper_clearance: f64,
    /// How many of a pad's thermal spokes must reach a pour that joins it by reliefs (KiCad's
    /// "minimum thermal relief spoke count"); 2, KiCad's default, where the project leaves it out.
    #[serde(default = "two")]
    pub min_resolved_spokes: f64,
    pub warnings: Vec<String>,
}

fn two() -> f64 {
    2.0
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
            custom: None,
            solder_mask_to_copper_clearance: rules["solder_mask_to_copper_clearance"].as_f64().unwrap_or(0.0) * MM,
            min_resolved_spokes: rules["min_resolved_spokes"].as_f64().unwrap_or(2.0),
            warnings,
        })
    }

    /// Takes a `.kicad_dru`'s text as the project's custom rules; `source` names it in warnings.
    pub fn apply_custom(&mut self, text: &str, source: &str) {
        self.custom = Some((source.to_string(), text.to_string()));
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
        assert!(Rules::read(&pro).unwrap().custom.is_some_and(|(_, text)| text.contains("VBUS")));
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
    fn the_projects_minimum_spoke_count_is_read() {
        assert_eq!(Rules::parse("{}").unwrap().min_resolved_spokes, 2.0);
        assert_eq!(Rules::parse(r#"{"board":{"design_settings":{"rules":{"min_resolved_spokes":1}}}}"#).unwrap().min_resolved_spokes, 1.0);
    }

}
