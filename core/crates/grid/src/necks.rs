//! The router's tracks narrower than their net should be: under the width a custom rule sets
//! for the net, which KiCad's check reports, or a net that carries current thinner than the pin
//! it feeds. Both are forced necks ([`Profile::forced_necks`]), laid only where no leg within the
//! rules reaches a pin, so each is told: the designer widens the pin's approach, moves a part, or
//! writes the exception into the rules, the way KiCad allows a neck.

use std::collections::BTreeSet;

use board::{nm, Board, ItemId, Kind, Nm, Shape};

use crate::grid::Profile;
use crate::report::item_name;

/// Widths within this of a limit meet it: rounding to whole nanometres.
const SLACK: Nm = 2;

/// One line of the report, with the net and the part it is about.
#[derive(Debug, Clone, PartialEq)]
pub struct Neck {
    pub line: String,
    pub net: String,
    pub part: Option<String>,
}

/// One line per forced neck in `laid`, naming its net, width, length and pin, then one KiCad
/// rule per net and part that would allow the necks that break a width rule there.
pub fn report(board: &Board, laid: &[ItemId]) -> Vec<String> {
    necks(board, laid).into_iter().map(|n| n.line).collect()
}

/// [`report`]'s lines, each with the net and the part it is about.
pub fn necks(board: &Board, laid: &[ItemId]) -> Vec<Neck> {
    // To the micron, rounded down so a rule's minimum stays at or under the neck it allows.
    let mm = |v: Nm| (v / 1_000) as f64 / 1e3;
    let mut lines = Vec::new();
    let mut exceptions: BTreeSet<(String, String, Nm)> = BTreeSet::new();
    for &id in laid {
        let Some(it) = board.item(id) else { continue };
        let (Kind::Track, Shape::Capsule { a, b, r }) = (&it.kind, &it.shape) else { continue };
        let (a, b, width) = (*a, *b, 2 * r);
        let name = board.net_name(it.net).to_string();
        let profile = Profile::for_net(board, it.net);
        let rule_min = nm(board.rules.min_track_width_of(&name));
        let breaks_rule = rule_min > nm(board.rules.min_track_width) && width + SLACK < rule_min;
        // The pin it enters: a pad of its net on its layer under one of its ends.
        let layer = it.layers.trailing_zeros() as usize;
        let pin = [a, b].into_iter().find_map(|end| {
            board.near(layer, board::BBox::of([end])).into_iter().find(|&p| {
                board.item(p).is_some_and(|pad| matches!(pad.kind, Kind::Pad { .. }) && pad.net == it.net && pad.shape.contains(end))
            })
        });
        let Some(pin) = pin else { continue };
        let pad = board.item(pin).expect("a pad found on the board");
        let side = {
            let bb = pad.shape.bbox();
            (bb.max.x - bb.min.x).min(bb.max.y - bb.min.y)
        };
        // A net kept at its width into its pads (`Profile::narrow_half_width`) is one that carries current.
        let carries_current = profile.narrow_half_width == profile.half_width;
        let under_pin = carries_current && width + SLACK < side.min(2 * profile.half_width);
        if !breaks_rule && !under_pin {
            continue;
        }
        let why = if breaks_rule {
            format!("under the {} mm its width rule asks", mm(rule_min))
        } else {
            format!("thinner than the {} mm pin it feeds", mm(side))
        };
        let part = match &pad.kind {
            Kind::Pad { footprint, .. } => Some(board.footprints[*footprint].reference.clone()),
            _ => None,
        };
        let line = format!(
            "{name} {} mm wide for {:.2} mm into {}, {why}: no wider leg reaches the pin",
            mm(width),
            a.distance(b) / 1e6,
            item_name(board, pin)
        );
        lines.push(Neck { line, net: name.clone(), part: part.clone() });
        if breaks_rule {
            if let Some(part) = part {
                exceptions.insert((name, part, width));
            }
        }
    }
    // The narrowest neck into each part, first in the set's order, sets its exception.
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    for (net, part, width) in exceptions {
        if seen.insert((net.clone(), part.clone())) {
            let line = format!(
                "KiCad allows {net}'s necks into {part} with this rule after the net's own: (rule \"{net} neck into {part}\" (condition \"A.NetName == '{net}' && A.intersectsCourtyard('{part}')\") (constraint track_width (min {}mm)))",
                mm(width)
            );
            lines.push(Neck { line, net, part: Some(part) });
        }
    }
    lines
}
