//! The routing board model in integer nanometres, built from a KiCad board: copper shapes per
//! layer in a spatial index, connectivity, and the design-rule checks a router must keep.

pub mod model;
pub mod check;
pub mod connect;
pub mod geom;
pub mod fill;

pub use model::{Block, Board, FootprintInfo, Item, ItemId, Kind, LoadError, DRAWN};
pub use check::{check, check_item, check_item_with_pours, intrudes, mask_gap, Rule, Violation, MASK_MARGIN};
pub use connect::{anchors, connectivity, connectivity_without_pours, joined, net_connectivity, Airline, Connectivity};
pub use fill::{fill_zones, Fill};
pub use geom::{inside, nm, point_segment, BBox, Nm, Point, Shape};
