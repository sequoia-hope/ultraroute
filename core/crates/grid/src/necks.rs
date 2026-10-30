//! The router's tracks narrower than their net should be: under the width a custom rule sets
//! for the net, which KiCad's check reports, or a net that carries current thinner than the pin
//! it feeds. Both are forced necks ([`Profile::forced_necks`]), laid only where no leg within the
//! rules reaches a pin, so each is told: the designer widens the pin's approach, moves a part, or
//! writes the exception into the rules, the way KiCad allows a neck.

use std::collections::{BTreeSet, HashMap};

use board::{nm, Board, ItemId, Kind, Nm, Shape};

use crate::grid::Profile;
use crate::report::item_name;

/// Widths within this of a limit meet it: rounding to whole nanometres.
const SLACK: Nm = 2;

/// One line per forced neck in `laid`, naming its net, width, length and pin, then one KiCad
/// rule per net and part that would allow the necks that break a width rule there.
pub fn report(board: &Board, laid: &[ItemId]) -> Vec<String> {
    // To the micron, rounded down so a rule's minimum stays at or under the neck it allows.
    let mm = |v: Nm| (v / 1_000) as f64 / 1e3;
    let mut lines = Vec::new();
    let mut exceptions: BTreeSet<(String, String, Nm)> = BTreeSet::new();
    // A differential pair runs at its coupled width, narrower than its class's track as often as
    // not: that is the pair, not a neck.
    let pair_width: HashMap<u32, Nm> = crate::pairs::diff_pairs(board).iter().flat_map(|p| [(p.p, p.width), (p.n, p.width)]).collect();
    for &id in laid {
        let Some(it) = board.item(id) else { continue };
        let (Kind::Track, Shape::Capsule { a, b, r }) = (&it.kind, &it.shape) else { continue };
        let (a, b, width) = (*a, *b, 2 * r);
        if pair_width.get(&it.net).is_some_and(|&w| width + SLACK >= w) {
            continue;
        }
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
        lines.push(format!(
            "{name} {} mm wide for {:.2} mm into {}, {why}: no wider leg reaches the pin",
            mm(width),
            a.distance(b) / 1e6,
            item_name(board, pin)
        ));
        if breaks_rule {
            if let Kind::Pad { footprint, .. } = &pad.kind {
                exceptions.insert((name, board.footprints[*footprint].reference.clone(), width));
            }
        }
    }
    // The narrowest neck into each part, first in the set's order, sets its exception.
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    for (net, part, width) in exceptions {
        if seen.insert((net.clone(), part.clone())) {
            lines.push(format!(
                "KiCad allows {net}'s necks into {part} with this rule after the net's own: (rule \"{net} neck into {part}\" (condition \"A.NetName == '{net}' && A.intersectsCourtyard('{part}')\") (constraint track_width (min {}mm)))",
                mm(width)
            ));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use board::{Item, Point};

    /// A USB pair whose class asks 0.5 mm tracks and 0.2 mm paired, and a 1 mm pad of D-.
    fn board() -> Board {
        let text = r#"(kicad_pcb (version 20241229) (generator "pcbnew") (general (thickness 1.6))
          (layers (0 "F.Cu" signal) (2 "B.Cu" signal) (25 "Edge.Cuts" user))
          (net 0 "") (net 1 "USB_D+") (net 2 "USB_D-")
          (footprint "t:P" (layer "F.Cu") (uuid "22222222-2222-4222-8222-222222222221") (at 10 10)
            (property "Reference" "J1" (at 0 -2 0) (layer "F.SilkS") (uuid "22222222-2222-4222-8222-222222222222"))
            (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 2 "USB_D-")))
          (gr_rect (start 0 0) (end 30 30) (layer "Edge.Cuts") (width 0.1)))"#;
        let (pcb, copper) = kicad_pcb::parse_board(text).unwrap();
        let rules = r#"{"net_settings":{"classes":[{"name":"Default","clearance":0.15,"track_width":0.15},{"name":"USB","clearance":0.15,"track_width":0.5,"diff_pair_width":0.2,"diff_pair_gap":0.15,"priority":0}],"netclass_patterns":[{"netclass":"USB","pattern":"USB_D*"}]}}"#;
        Board::new(&pcb, &copper, kicad_pcb::Rules::parse(rules).unwrap())
    }

    fn track_into_pad(b: &mut Board, width_mm: f64) -> ItemId {
        b.insert(Item {
            kind: Kind::Track,
            net: 2,
            layers: 1,
            shape: Shape::Capsule { a: Point::new(10_000_000, 10_000_000), b: Point::new(20_000_000, 10_000_000), r: (width_mm * 5e5) as Nm },
            hole: None,
            locked: false,
            span: None,
        })
    }

    #[test]
    fn a_pair_at_its_coupled_width_is_no_neck() {
        let mut b = board();
        let paired = track_into_pad(&mut b, 0.2);
        assert!(report(&b, &[paired]).is_empty(), "{:?}", report(&b, &[paired]));
        let thinner = track_into_pad(&mut b, 0.1);
        assert_eq!(report(&b, &[thinner]).len(), 1, "narrower than the pair is still a neck");
    }
}
