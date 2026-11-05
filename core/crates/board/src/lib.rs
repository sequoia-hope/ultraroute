//! The routing board model in integer nanometres, built from a KiCad board: copper shapes per
//! layer in a spatial index, connectivity, and the design-rule checks a router must keep.

pub mod model;
pub mod check;
pub mod custom;
pub mod connect;
pub mod geom;
pub mod fill;
pub mod regions;
pub mod work;
pub mod spokes;

pub mod probe;
pub use model::{arc_chords, Block, Board, Crowd, FootprintInfo, Item, ItemId, Kind, LoadError, ANY_ONE_NET, DRAWN};
pub use custom::Thing;
pub use check::{check, check_item, check_item_with_pours, intrudes, mask_gap, mask_pair_gap, within, Rule, Violation, MASK_MARGIN};
pub use connect::{anchors, connectivity, connectivity_without_pours, end_neck, firmly_joined, joined, joint_neck, meets_squarely, FULL_JOINT, net_connectivity, Airline, Connectivity};
pub use fill::{fill_zones, Fill};
pub use regions::{broken_region, region_price, Antipad, ClearUnder, Region, RegionKind, RegionRule, ZoneArea};
pub use geom::{inside, nm, point_segment, BBox, Nm, Point, Shape};
