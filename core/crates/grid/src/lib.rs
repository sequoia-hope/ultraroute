//! Grid routing on the core board model: a conservative obstacle map per net class, an integer
//! cost field from the sources and a walk back down it, laid as tracks and vias.

pub mod grid;
pub mod route;
pub mod search;

pub use grid::{ObstacleMap, Profile, RouteGrid, ViaSpec};
pub use route::{edits, route_open, CpuSolver, FieldSolver, Options, Stats};
