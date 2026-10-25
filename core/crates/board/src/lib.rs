//! The routing board model in integer nanometres, built from a KiCad board: copper shapes per
//! layer in a spatial index, connectivity, and the design-rule checks a router must keep.

pub mod model;
pub mod check;
pub mod connect;
pub mod geom;

pub use model::{Block, Board, FootprintInfo, Item, ItemId, Kind, LoadError};
pub use check::{check, check_item, check_item_with_pours, intrudes, Rule, Violation};
pub use connect::{connectivity, connectivity_without_pours, joined, Airline, Connectivity};
pub use geom::{inside, nm, point_segment, BBox, Nm, Point, Shape};
