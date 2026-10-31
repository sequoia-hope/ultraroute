//! What crossing a pad's thermal spokes costs a search. A pad keeps room for every spoke that
//! fits and needs only some (see `board::spokes`): the checker refuses the cut that would leave
//! too few; before that, a track or via of another net there pays a price, so a search goes round
//! where it can. Spokes cover little of a board, so the prices are kept sparse, once per obstacle
//! map, and every net's view shares them: a mark per cell where any spoke is near, and the nets
//! whose spokes those are, which do not pay for their own.

use std::collections::HashMap;
use std::sync::Arc;

use board::{Board, Kind};

use crate::grid::ObstacleMap;
use crate::search::STRAIGHT;

/// What crossing a spoke a pad can spare costs: a track as if each step there were this much
/// longer again, a via as this many millimetres more.
const SPOKE_COST: f64 = 1.0;

#[derive(Debug, Default)]
pub struct SpokePrices {
    /// Per routing layer and cell, and per cell: whether a track's (a via's) copper there would
    /// come within clearance of a spoke a pad can spare.
    track: Vec<bool>,
    via: Vec<bool>,
    /// The nets of those spokes, once for each spoke, where marked.
    track_nets: HashMap<u32, Vec<u32>>,
    via_nets: HashMap<u32, Vec<u32>>,
    track_cost: u32,
    via_cost: u32,
}

impl SpokePrices {
    /// What a track of `net` pays for entering `index` (layer and cell).
    pub fn track(&self, index: usize, net: u32) -> u32 {
        if !self.track.get(index).copied().unwrap_or(false) {
            return 0;
        }
        self.track_nets.get(&(index as u32)).map_or(0, |nets| nets.iter().filter(|&&n| n != net).count() as u32 * self.track_cost)
    }

    /// What a via of `net` pays for standing on `cell`.
    pub fn via(&self, cell: usize, net: u32) -> u32 {
        if !self.via.get(cell).copied().unwrap_or(false) {
            return 0;
        }
        self.via_nets.get(&(cell as u32)).map_or(0, |nets| nets.iter().filter(|&&n| n != net).count() as u32 * self.via_cost)
    }
}

impl ObstacleMap {
    /// The prices of the spokes on this map's cells; none where no pad has a spoke to spare.
    pub(crate) fn spoke_prices(&self, board: &Board) -> Option<Arc<SpokePrices>> {
        let spare: Vec<&board::Item> = board.items().map(|(_, it)| it).filter(|it| matches!(it.kind, Kind::Spoke { spare, .. } if spare > 0)).collect();
        if spare.is_empty() {
            return None;
        }
        let cells = self.nx * self.ny;
        let mut p = SpokePrices {
            track: vec![false; self.layers * cells],
            via: vec![false; cells],
            track_cost: (SPOKE_COST * STRAIGHT as f64).round() as u32,
            via_cost: (SPOKE_COST * 1e6 / self.h as f64 * STRAIGHT as f64).round() as u32,
            ..SpokePrices::default()
        };
        for it in spare {
            let Kind::Spoke { clearance: zone, .. } = it.kind else { continue };
            let clearance = self.clearance_to(board, it.net).max(zone);
            let mut near = Vec::new();
            self.cells_near(&it.shape, self.profile.half_width + clearance + self.reach(), &mut |c| near.push(c));
            for l in (0..self.layers).filter(|l| it.on(*l)) {
                for &c in &near {
                    let i = l * cells + c;
                    p.track[i] = true;
                    p.track_nets.entry(i as u32).or_default().push(it.net);
                }
            }
            near.clear();
            self.cells_near(&it.shape, self.profile.via_radius + clearance, &mut |c| near.push(c));
            for c in near {
                p.via[c] = true;
                p.via_nets.entry(c as u32).or_default().push(it.net);
            }
        }
        if p.track_nets.is_empty() && p.via_nets.is_empty() {
            return None;
        }
        Some(Arc::new(p))
    }
}
