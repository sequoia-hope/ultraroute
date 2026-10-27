//! Routing regions: areas of the board with rules for which nets may run there, as a planner
//! (circuit-composer's constraint compiler) hands them to the router beside the board. A rule
//! names the nets it holds for and says `never` (keep out), `must` (stay within, on the region's
//! layers), `avoid` or `prefer` (a cost per length inside, or outside), for tracks or for vias.
//! The board holds them resolved to net numbers; the grid applies them to each net's view.

use std::collections::HashSet;

use crate::geom::Shape;

#[derive(Debug, Clone, PartialEq)]
pub struct Region {
    /// The planner's name for it, said wherever a rule of it blocks or prices copper.
    pub name: String,
    pub shape: Shape,
    /// The copper layers it holds on, one bit per index into `Board::layers`.
    pub layers: u64,
    pub rules: Vec<RegionRule>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RegionRule {
    /// The nets it holds for; every net when `None`.
    pub nets: Option<HashSet<u32>>,
    pub kind: RegionKind,
    /// A rule for vias rather than tracks.
    pub vias: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RegionKind {
    /// No copper of the nets in the region on its layers.
    Never,
    /// The nets' copper on the region's layers stays within it.
    Must,
    /// A track pays `weight` times its length inside the region on top of the length itself; a
    /// via there pays `weight` millimetres of track.
    Avoid(f64),
    /// As `Avoid`, outside the region.
    Prefer(f64),
}

impl RegionRule {
    pub fn holds_for(&self, net: u32) -> bool {
        self.nets.as_ref().is_none_or(|n| n.contains(&net))
    }
}
