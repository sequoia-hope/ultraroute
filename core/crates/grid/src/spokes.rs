//! What crossing a pad's thermal spokes costs a search. A pad keeps room for every spoke that
//! fits and needs only some (see `board::spokes`): the checker refuses the cut that would leave
//! too few; before that, a track or via of another net there pays a price, so a search goes round
//! where it can. Spokes cover little of a board, so the prices are kept sparse, once per obstacle
//! map, and every net's view shares them: a mark per cell where any spoke is near, and the nets
//! whose spokes those are, which do not pay for their own.

use std::sync::Arc;

use board::{Board, Kind};

use crate::fx::FxHashMap;
use crate::grid::ObstacleMap;
use crate::search::STRAIGHT;

/// What crossing a spoke a pad can spare costs: a track as if each step there were this much
/// longer again, a via as this many millimetres more.
const SPOKE_COST: f64 = 1.0;

#[derive(Debug, Default)]
pub struct SpokePrices {
    /// Per routing layer and cell, and per cell: whether a track's (a via's) copper there would
    /// come within clearance of a spoke a pad can spare.
    pub(crate) track: Vec<bool>,
    pub(crate) via: Vec<bool>,
    /// The nets of those spokes, once for each spoke, where marked.
    track_nets: FxHashMap<u32, Vec<u32>>,
    via_nets: FxHashMap<u32, Vec<u32>>,
    track_cost: u32,
    via_cost: u32,
}

impl SpokePrices {
    /// What a track of `net` pays for entering `index` (layer and cell).
    #[inline]
    pub fn track(&self, index: usize, net: u32) -> u32 {
        if !self.track.get(index).copied().unwrap_or(false) {
            return 0;
        }
        self.track_hit(index, net)
    }

    #[cold]
    #[inline(never)]
    pub(crate) fn track_hit(&self, index: usize, net: u32) -> u32 {
        self.track_nets.get(&(index as u32)).map_or(0, |nets| nets.iter().filter(|&&n| n != net).count() as u32 * self.track_cost)
    }

    /// What a via of `net` pays for standing on `cell`.
    #[inline]
    pub fn via(&self, cell: usize, net: u32) -> u32 {
        if !self.via.get(cell).copied().unwrap_or(false) {
            return 0;
        }
        self.via_hit(cell, net)
    }

    #[cold]
    #[inline(never)]
    pub(crate) fn via_hit(&self, cell: usize, net: u32) -> u32 {
        self.via_nets.get(&(cell as u32)).map_or(0, |nets| nets.iter().filter(|&&n| n != net).count() as u32 * self.via_cost)
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        nodes: usize,
        cells: usize,
        track_hits: &[(usize, u32)],
        via_hits: &[(usize, u32)],
        track_cost: u32,
        via_cost: u32,
    ) -> Self {
        let mut p = Self {
            track: vec![false; nodes],
            via: vec![false; cells],
            track_cost,
            via_cost,
            ..Self::default()
        };
        for &(idx, net) in track_hits {
            p.track[idx] = true;
            p.track_nets.entry(idx as u32).or_default().push(net);
        }
        for &(cell, net) in via_hits {
            p.via[cell] = true;
            p.via_nets.entry(cell as u32).or_default().push(net);
        }
        p
    }
}

impl ObstacleMap {
    /// The prices of the spokes on this map's cells; none where no pad has a spoke to spare.
    pub(crate) fn spoke_prices(&self, board: &Board) -> Option<Arc<SpokePrices>> {
        let spare = board.spare_spokes();
        if spare.is_empty() || self.nx == 0 || self.ny == 0 {
            return None;
        }
        let reach = self.reach();
        let (x_end, y_end) = (self.x0 + self.nx as board::Nm * self.h, self.y0 + self.ny as board::Nm * self.h);
        use std::hash::Hash;
        let key = crate::fx::hash128(|h| {
            (self.x0, self.y0, self.h, self.nx, self.ny, self.layers, self.profile.half_width, self.profile.via_radius).hash(h);
            for &id in spare {
                let Some(it) = board.item(id) else { continue };
                let Kind::Spoke { clearance: zone, .. } = it.kind else { continue };
                let clearance = self.clearance_to(board, it.net).max(zone);
                let margin = (self.profile.half_width + clearance + reach).max(self.profile.via_radius + clearance);
                let sb = it.shape.bbox();
                if sb.max.x + margin < self.x0 || sb.min.x - margin > x_end || sb.max.y + margin < self.y0 || sb.min.y - margin > y_end {
                    continue;
                }
                (id, it.net, it.layers, clearance).hash(h);
                match &it.shape {
                    board::Shape::Capsule { a, b, r } => (0u8, a, b, r).hash(h),
                    board::Shape::Polygon(pts) => (1u8, pts).hash(h),
                }
            }
        });
        const KEPT: usize = 32;
        thread_local! {
            static SPOKE_CACHE: std::cell::RefCell<Vec<(u128, Option<Arc<SpokePrices>>)>> =
                const { std::cell::RefCell::new(Vec::new()) };
        }
        let cached = SPOKE_CACHE.with(|cache| {
            let mut borrow = cache.borrow_mut();
            let pos = borrow.iter().rposition(|(k, _)| *k == key)?;
            let out = borrow[pos].1.clone();
            if pos + 1 < borrow.len() {
                let entry = borrow.remove(pos);
                borrow.push(entry);
            }
            Some(out)
        });
        if let Some(hit) = cached {
            return hit;
        }
        let cells = self.nx * self.ny;
        let mut p = SpokePrices {
            track: vec![false; self.layers * cells],
            via: vec![false; cells],
            track_cost: (SPOKE_COST * STRAIGHT as f64).round() as u32,
            via_cost: (SPOKE_COST * 1e6 / self.h as f64 * STRAIGHT as f64).round() as u32,
            ..SpokePrices::default()
        };
        let mut near = Vec::new();
        for &id in spare {
            let Some(it) = board.item(id) else { continue };
            let Kind::Spoke { clearance: zone, .. } = it.kind else { continue };
            let clearance = self.clearance_to(board, it.net).max(zone);
            let margin = (self.profile.half_width + clearance + reach).max(self.profile.via_radius + clearance);
            let sb = it.shape.bbox();
            if sb.max.x + margin < self.x0 || sb.min.x - margin > x_end || sb.max.y + margin < self.y0 || sb.min.y - margin > y_end {
                continue;
            }
            near.clear();
            self.cells_near(&it.shape, self.profile.half_width + clearance + reach, &mut |c| near.push(c));
            for l in (0..self.layers).filter(|l| it.on(*l)) {
                for &c in &near {
                    let i = l * cells + c;
                    p.track[i] = true;
                    p.track_nets.entry(i as u32).or_default().push(it.net);
                }
            }
            near.clear();
            self.cells_near(&it.shape, self.profile.via_radius + clearance, &mut |c| near.push(c));
            for &c in &near {
                p.via[c] = true;
                p.via_nets.entry(c as u32).or_default().push(it.net);
            }
        }
        let out = if p.track_nets.is_empty() && p.via_nets.is_empty() { None } else { Some(Arc::new(p)) };
        SPOKE_CACHE.with(|cache| {
            let mut borrow = cache.borrow_mut();
            if borrow.len() >= KEPT {
                borrow.remove(0);
            }
            borrow.push((key, out.clone()));
        });
        out
    }
}
