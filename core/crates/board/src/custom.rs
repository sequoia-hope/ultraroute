//! The board's custom rules (its `.kicad_dru`), asked about board items and about copper the
//! router has yet to lay, as KiCad's DRC asks them: [`Board::rule_min`] gives the minimum a
//! constraint sets between two things, or between one thing and the board edge.
//!
//! What a rule asks that the router cannot answer (an area by name, a footprint's courtyard, a
//! fabrication property) is unknown, and a rule that hangs on it may hold: the strictest
//! minimum it could be is taken, so the router keeps at least what KiCad will check.

use kicad_pcb::dru::{eval, wildcard, Object, Resolved, RuleSet, Severity, Tri, Value};

use crate::model::{Board, Item, ItemId, Kind, DRAWN};
use crate::geom::{nm, Nm};

/// The constraints the router keeps to; a rule with only others is reported as not applied,
/// unless it is about something the router never lays (silkscreen, text, courtyards, parts).
/// (`hole_size`, `via_diameter` and `annular_width` are kept by the router's vias; on the board's
/// own pads they are the designer's.)
pub const APPLIED: [&str; 10] = ["clearance", "hole_clearance", "hole_to_hole", "edge_clearance", "physical_hole_clearance", "track_width", "disallow", "hole_size", "via_diameter", "annular_width"];

/// Constraints the router checks once it has routed and reports, but does not yet route to.
pub const REPORTED: [&str; 2] = ["length", "via_count"];

/// Constraints on things the router neither lays nor moves: KiCad checks them as before.
const ELSEWHERE: [&str; 9] = ["silk_clearance", "text_height", "text_thickness", "courtyard_clearance", "assertion", "solder_mask_expansion", "solder_paste_abs_margin", "solder_paste_rel_margin", "creepage"];

/// Something a rule is asked about.
pub enum Thing<'a> {
    /// An item on the board.
    Item(&'a Board, ItemId, &'a Item),
    /// A track (`drill` 0) or via the router may lay, of the given width or diameter: of a net
    /// of `class` (KiCad's name for the net's classes), and of `net` where the rules name nets
    /// (see [`Board::rules_name_nets`]); with no net it is of a net other than any it meets.
    /// A track lies on `layer` where that is known; `area` says which of the board's named
    /// areas it would cross.
    New { board: &'a Board, via: bool, net: Option<u32>, class: &'a str, width: Nm, drill: Nm, layer: Option<usize>, area: Area },
    /// A primitive of the board's edge: KiCad's name for its shape (`Segment` for any run of
    /// lines, `Arc`, `Circle`) and its layer (`Edge.Cuts`, `Margin`).
    Edge { shape: &'a str, layer: &'a str },
    /// Copper of any kind on any one copper layer, not known which: an item an edge clearance
    /// is asked for before the item is known.
    Copper(&'a Board),
}

/// Which of the board's named areas ([`Board::areas`]) new copper would cross.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Area {
    /// Not known: wherever it may go.
    Unknown,
    /// None of them.
    Outside,
    /// This one alone.
    In(usize),
}

impl Board {
    /// Whether the custom rules forbid `thing`, an object of KiCad's disallow kind `word`
    /// (`track`, `via`), on `layer`: a rule with a `disallow` naming it whose layer clause takes
    /// the layer and whose condition holds. Any such rule forbids it, not only the last.
    pub fn disallowed(&self, word: &str, thing: &Thing, layer: usize) -> Tri {
        let name = self.layers.get(layer).map_or("", String::as_str);
        let mut out = Tri::False;
        for rule in self.custom.rules.iter().filter(|r| r.severity != Severity::Ignore) {
            if !rule.constraints.iter().any(|c| c.kind == "disallow" && c.words.iter().any(|w| w == word)) {
                continue;
            }
            if !rule.layer.as_ref().is_none_or(|l| l.takes(name, self.layers.len())) {
                continue;
            }
            match rule.condition.as_ref().map_or(Tri::True, |c| eval(c, thing, None)) {
                Tri::True => return Tri::True,
                Tri::Unknown => out = Tri::Unknown,
                Tri::False => {}
            }
        }
        out
    }

    /// The minimum the custom rules set for a constraint `kind` between `a` and `b` (the edge for
    /// `edge_clearance`) on `layer`, where `implicit` is what the net classes and the board's own
    /// rules ask: the last rule that holds replaces it, as in KiCad, lower or higher; a rule
    /// that only may hold raises it.
    pub fn rule_min(&self, kind: &str, a: &Thing, b: Option<&Thing>, layer: usize, implicit: Nm) -> Nm {
        if !self.custom.has(kind) {
            return implicit;
        }
        let layer = self.layers.get(layer).map_or("", String::as_str);
        let r = self.custom.resolve(kind, a, b.map(|b| b as &dyn Object), layer, self.layers.len());
        strictest(&r, implicit)
    }

    /// The largest minimum any custom rule sets for constraint `kind`: how far to look for what
    /// a rule might hold apart.
    pub fn rule_reach(&self, kind: &str) -> Nm {
        self.custom.rules.iter().flat_map(|r| &r.constraints).filter(|c| c.kind == kind).filter_map(|c| c.min).map(nm).max().unwrap_or(0)
    }

    /// The narrowest track `net` may have on any layer: the board minimum, or a custom rule's
    /// `track_width` for a track of the net where that is wider (a rule for one layer holds on
    /// all, which only ever widens).
    pub fn min_track_width_of(&self, net: u32) -> Nm {
        let floor = nm(self.rules.min_track_width);
        if !self.custom.has("track_width") {
            return floor;
        }
        let class = self.class_names.get(net as usize).map_or("", String::as_str);
        (0..self.layers.len())
            .map(|l| self.rule_min("track_width", &Thing::New { board: self, via: false, net: Some(net), class, width: 0, drill: 0, layer: Some(l), area: Area::Unknown }, None, l, floor))
            .max()
            .unwrap_or(floor)
    }

    /// The nets whose copper breaks a custom rule's `length` or `via_count` maximum, one line
    /// each with the net's name: its tracks' total length (vias' heights not counted), its vias.
    pub fn rule_limits(&self) -> Vec<(String, String)> {
        let (length, vias) = (self.custom.has("length"), self.custom.has("via_count"));
        if !length && !vias {
            return Vec::new();
        }
        let mut per_net: std::collections::BTreeMap<u32, (f64, usize)> = std::collections::BTreeMap::new();
        for (_, it) in self.items().filter(|(_, it)| it.net != 0) {
            match (&it.kind, &it.shape) {
                (Kind::Track, crate::geom::Shape::Capsule { a, b, .. }) => per_net.entry(it.net).or_default().0 += a.distance(*b) * 1e-6,
                (Kind::Via, _) => per_net.entry(it.net).or_default().1 += 1,
                _ => {}
            }
        }
        let mut out = Vec::new();
        for (net, (mm, count)) in per_net {
            let class = self.class_name(net);
            let thing = Thing::New { board: self, via: false, net: Some(net), class, width: 0, drill: 0, layer: None, area: Area::Unknown };
            let sure = |kind: &str| self.custom.resolve(kind, &thing, None, "", self.layers.len()).sure.filter(|(r, _)| r.severity != Severity::Ignore);
            let name = self.net_name(net);
            if let Some((rule, c)) = sure("length").filter(|_| length) {
                if let Some(max) = c.max.filter(|max| mm > max * 1e3 + 1e-6) {
                    out.push((format!("length: {name} is {mm:.2} mm, over rule \"{}\"'s {:.2} mm", rule.name, max * 1e3), name.to_string()));
                }
            }
            if let Some((rule, c)) = sure("via_count").filter(|_| vias) {
                if let Some(max) = c.max.filter(|max| count as f64 > *max + 1e-9) {
                    out.push((format!("vias: {name} has {count}, over rule \"{}\"'s {max}", rule.name), name.to_string()));
                }
            }
        }
        out
    }

    /// Whether a rule tells nets apart by name (`A.NetName`, `A.inDiffPair`), so copper the
    /// router lays must be asked about as its own net, not only as one of its class.
    pub fn rules_name_nets(&self) -> bool {
        self.names_nets
    }

    /// KiCad's name for the classes of `net` (`A,B,Default`).
    pub fn class_name(&self, net: u32) -> &str {
        self.class_names.get(net as usize).map_or("", String::as_str)
    }

    /// The narrowest track the custom rules allow whatever the net, on any layer: the board's
    /// minimum, or the last `track_width` rule that surely holds for a track of no net in
    /// particular (a fab's blanket 0.15 mm). A forced neck may break a net's own width rule, and
    /// says so, but keeps to this.
    pub fn min_track_width_any(&self) -> Nm {
        let floor = nm(self.rules.min_track_width);
        if !self.custom.has("track_width") {
            return floor;
        }
        (0..self.layers.len())
            .map(|l| {
                let track = Thing::New { board: self, via: false, net: None, class: "", width: 0, drill: 0, layer: Some(l), area: Area::Unknown };
                let r = self.custom.resolve("track_width", &track, None, &self.layers[l], self.layers.len());
                match r.sure {
                    Some((rule, c)) if rule.severity != Severity::Ignore => c.min.map_or(floor, nm),
                    _ => floor,
                }
            })
            .max()
            .unwrap_or(floor)
    }

    /// Warnings for the custom rules the router does not keep to: those it could not read, and
    /// those whose every constraint is one it does not model, on copper it lays.
    pub(crate) fn custom_warnings(set: &RuleSet, source: &str) -> Vec<String> {
        let mut out: Vec<String> = set.errors.iter().map(|e| format!("{source}: {e}; not applied")).collect();
        for rule in &set.rules {
            let kinds: Vec<&str> = rule.constraints.iter().map(|c| c.kind.as_str()).filter(|k| !APPLIED.contains(k) && !ELSEWHERE.contains(k)).collect();
            let (reported, dropped): (Vec<&str>, Vec<&str>) = kinds.into_iter().partition(|k| REPORTED.contains(k));
            if !dropped.is_empty() {
                out.push(format!("{source}: custom rule \"{}\": {} not applied", rule.name, dropped.join(", ")));
            }
            if !reported.is_empty() {
                out.push(format!("{source}: custom rule \"{}\": {} checked after routing, not routed to", rule.name, reported.join(", ")));
            }
        }
        out
    }
}

pub(crate) fn names_nets(set: &RuleSet) -> bool {
    use kicad_pcb::dru::Expr;
    let names = |e: &Expr| matches!(e, Expr::Prop(_, p) if p == "netname") || matches!(e, Expr::Call(_, f, _) if f == "indiffpair" || f == "fromto");
    set.rules.iter().filter_map(|r| r.condition.as_ref()).any(|c| c.any(&names))
}

fn strictest(r: &Resolved, implicit: Nm) -> Nm {
    nm(r.strictest_min(implicit as f64 / 1e9))
}

impl Thing<'_> {
    fn board(&self) -> Option<&Board> {
        match self {
            Thing::Item(b, ..) | Thing::New { board: b, .. } => Some(b),
            _ => None,
        }
    }

    fn net(&self) -> Option<u32> {
        match self {
            Thing::Item(_, _, it) => Some(it.net),
            Thing::New { net, .. } => *net,
            _ => None,
        }
    }

    /// KiCad's name for the classes of the thing's net.
    fn class(&self) -> Option<String> {
        match self {
            Thing::New { class, .. } => Some(class.to_string()),
            Thing::Item(board, _, it) => board.class_names.get(it.net as usize).cloned(),
            _ => None,
        }
    }

    fn layer_names(&self) -> Option<Vec<String>> {
        let board = self.board()?;
        let mask = match self {
            Thing::Item(_, _, it) => it.layers,
            Thing::New { via: true, .. } => (1u64 << board.layers.len()) - 1,
            Thing::New { layer: Some(l), .. } => 1 << l,
            _ => return None,
        };
        Some((0..board.layers.len()).filter(|l| mask & (1 << l) != 0).map(|l| board.layers[l].clone()).collect())
    }
}

impl Object for Thing<'_> {
    fn property(&self, name: &str) -> Value {
        let str = |s: &str| Value::Str(s.to_string());
        if let Thing::Edge { shape, layer } = self {
            return match name {
                "layer" => str(layer),
                // Lines may have been drawn as a rectangle or a polygon: the board keeps no word.
                "shape" if *shape == "Segment" => Value::Set(["Segment", "Rectangle", "Polygon"].map(String::from).to_vec()),
                "shape" => str(shape),
                "type" => str("Graphic"),
                "net" => Value::Num(0.0),
                "netname" | "netclass" => str(""),
                _ => Value::Unknown,
            };
        }
        if let Thing::Copper(board) = self {
            return match name {
                "layer" => Value::OneOf(board.layers.clone()),
                "type" => Value::OneOf(["Track", "Arc", "Via", "Pad", "Zone"].map(String::from).to_vec()),
                _ => Value::Unknown,
            };
        }
        let Some(board) = self.board() else { return Value::Unknown };
        // Copper of no net in particular: of a net other than any it meets.
        let Some(net) = self.net() else {
            return match name {
                "net" => Value::Num(-1.0),
                "netname" => Value::Unknown,
                "netclass" => self.class().map_or(Value::Unknown, |c| Value::Set(std::iter::once(c.clone()).chain(c.split(',').map(String::from)).collect())),
                _ => self.known(board, 0, name),
            };
        };
        self.known(board, net, name)
    }

    fn call(&self, name: &str, args: &[Value]) -> Value {
        let arg = match args.first() {
            Some(Value::Str(s)) => s.as_str(),
            _ => return Value::Unknown,
        };
        if let Thing::Edge { layer, .. } = self {
            return match name {
                "existsonlayer" => Value::Bool(wildcard(arg, layer, true)),
                "isplated" => Value::Bool(false),
                _ => Value::Unknown,
            };
        }
        if let Thing::Copper(board) = self {
            return match name {
                "existsonlayer" if !board.layers.iter().any(|l| wildcard(arg, l, true)) => Value::Bool(false),
                _ => Value::Unknown,
            };
        }
        let Some(board) = self.board() else { return Value::Unknown };
        match name {
            "isplated" => Value::Bool(match self {
                Thing::Item(_, _, it) => it.plated_hole(),
                Thing::New { via, .. } => *via,
                _ => return Value::Unknown,
            }),
            "existsonlayer" => self.layer_names().map_or(Value::Unknown, |ls| Value::Bool(ls.iter().any(|l| wildcard(arg, l, true)))),
            "hasnetclass" => self.class().map_or(Value::Unknown, |c| Value::Bool(c == arg || c.split(',').any(|n| n == arg))),
            "intersectsarea" | "insidearea" | "enclosedbyarea" => self.in_area(board, name, arg),
            "memberoffootprint" => match self {
                Thing::Item(_, _, Item { kind: Kind::Pad { footprint, .. }, .. }) => Value::Bool(wildcard(arg, &board.footprints[*footprint].reference, true)),
                Thing::Item(_, _, Item { kind: Kind::Track | Kind::Via, .. }) | Thing::New { .. } => Value::Bool(false),
                _ => Value::Unknown,
            },
            _ => Value::Unknown,
        }
    }
}

impl Thing<'_> {
    /// `intersectsArea(name)` and its kin: by the item's copper against the named areas on the
    /// layers they share, or by what new copper is said to cross.
    fn in_area(&self, board: &Board, function: &str, name: &str) -> Value {
        match self {
            Thing::New { area: Area::Unknown, .. } => Value::Unknown,
            Thing::New { area: Area::Outside, .. } => Value::Bool(false),
            Thing::New { area: Area::In(k), .. } => Value::Bool(board.areas.get(*k).is_some_and(|(n, ..)| n == name)),
            Thing::Item(_, _, it) => Value::Bool(board.areas.iter().any(|(n, shape, layers)| {
                n == name
                    && (layers & it.layers != 0 || it.layers == 0)
                    && if function == "enclosedbyarea" { crate::check::within(shape, &it.shape, 0) } else { it.shape.gap(shape) == 0.0 }
            })),
            _ => Value::Unknown,
        }
    }

    /// A property of a board item or new copper of `net` (0 for new copper of no net named).
    fn known(&self, board: &Board, net: u32, name: &str) -> Value {
        let str = |s: &str| Value::Str(s.to_string());
        match name {
            "type" => match self {
                Thing::Item(_, _, it) => match it.kind {
                    Kind::Pad { .. } => str("Pad"),
                    Kind::Track => str("Track"),
                    Kind::Via => str("Via"),
                    Kind::Pour { zone, .. } if zone != DRAWN => str("Zone"),
                    Kind::Spoke { .. } => str("Zone"),
                    Kind::Keepout { .. } => str("Rule Area"),
                    // Copper drawn on a copper layer: a line, a shape or text.
                    Kind::Pour { .. } => Value::Unknown,
                },
                Thing::New { via, .. } => str(if *via { "Via" } else { "Track" }),
                _ => Value::Unknown,
            },
            "net" => Value::Num(net as f64),
            "netname" => str(board.net_name(net)),
            // Equal to any class the net is in, and to the whole name (see `Rules::in_class`).
            "netclass" => self.class().map_or(str(""), |c| Value::Set(std::iter::once(c.clone()).chain(c.split(',').map(String::from)).collect())),
            // A track of no known layer is on one of them; something on several layers (a via,
            // a through-hole pad) compares with no layer at all, as in KiCad.
            "layer" => match self.layer_names() {
                None => Value::OneOf(board.layers.clone()),
                Some(ls) if ls.len() > 1 => Value::Opaque,
                Some(ls) => Value::Set(ls),
            },
            "padtype" => match self {
                Thing::Item(_, _, it @ Item { kind: Kind::Pad { plated, .. }, .. }) => match (it.hole, plated) {
                    (None, _) => str("SMD"),
                    (Some(_), true) => str("Through-hole"),
                    (Some(_), false) => str("NPTH, mechanical"),
                },
                _ => Value::Null,
            },
            "holesizex" | "holesizey" | "holesize" | "drill" => match self {
                Thing::Item(_, _, it) => it.hole.map_or(Value::Null, |(_, r)| Value::Len(2.0 * r as f64 / 1e9)),
                Thing::New { via: true, drill, .. } => Value::Len(*drill as f64 / 1e9),
                _ => Value::Null,
            },
            "width" => match self {
                Thing::Item(_, _, Item { kind: Kind::Track, shape: crate::geom::Shape::Capsule { r, .. }, .. }) => Value::Len(2.0 * *r as f64 / 1e9),
                Thing::New { via: false, width, .. } => Value::Len(*width as f64 / 1e9),
                _ => Value::Unknown,
            },
            "viatype" => match self {
                Thing::Item(_, _, Item { kind: Kind::Via, .. }) | Thing::New { via: true, .. } => str("Through"),
                _ => Value::Null,
            },
            "parent" | "reference" => match self {
                Thing::Item(_, _, Item { kind: Kind::Pad { footprint, .. }, .. }) => str(&board.footprints[*footprint].reference),
                Thing::Item(_, _, Item { kind: Kind::Keepout { footprint: Some(f), .. }, .. }) => str(&board.footprints[*f].reference),
                Thing::New { .. } | Thing::Item(_, _, Item { kind: Kind::Track | Kind::Via, .. }) => Value::Null,
                _ => Value::Unknown,
            },
            _ => Value::Unknown,
        }
    }
}
