//! Grid routing on the core board model: a conservative obstacle map per net class, an integer
//! cost field from the sources and a walk back down it, laid as tracks and vias.

#[cfg(feature = "gpu")]
pub mod gpu;
pub mod grid;
pub mod negotiate;
pub mod route;
pub mod search;
#[cfg(feature = "gpu")]
pub mod sweep;

pub use grid::{ObstacleMap, Profile, RouteGrid, ViaSpec};
pub use negotiate::{route_negotiated, NegotiationOptions, NegotiationStats};
pub use route::{edits, route_open, CpuSolver, FieldSolver, Options, Stats};
#[cfg(feature = "gpu")]
pub use gpu::GpuSolver;
#[cfg(feature = "gpu")]
pub use sweep::GpuSweepSolver;
