//! The routing board model in integer nanometres, built from a KiCad board: copper shapes per
//! layer in a spatial index, connectivity, and the design-rule checks a router must keep.

pub mod model;
pub mod check;
pub mod connect;
pub mod geom;

pub use model::{Board, FootprintInfo, Item, ItemId, Kind, LoadError};
pub use check::{check, Rule, Violation};
pub use connect::{connectivity, Airline, Connectivity};
pub use geom::{nm, BBox, Nm, Point, Shape};
