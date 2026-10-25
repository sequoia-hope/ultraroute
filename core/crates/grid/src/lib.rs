//! Grid routing on the core board model: a conservative obstacle map per net class, an integer
//! cost field from the sources and a walk back down it, laid as tracks and vias.

#[cfg(feature = "gpu")]
pub mod gpu;
pub mod blame;
pub mod cleanup;
pub mod clock;
pub mod grid;
pub mod negotiate;
pub mod report;
pub mod nudge;
pub mod push;
pub mod ripup;
pub mod route;
pub mod search;
#[cfg(feature = "gpu")]
pub mod sweep;

pub use blame::{blame, Blame, BlameOptions, BlameReport};
pub use cleanup::{pull_tight, CleanStats};
pub use grid::{ObstacleMap, Profile, RouteGrid, ViaSpec};
pub use nudge::{nudge, NudgeOptions, NudgeStats};
pub use push::{push, rip_broken, Field, PushOptions, PushStats};
pub use report::{item_name, Activity, Focus, Log};
pub use negotiate::{route_negotiated, NegotiationOptions, NegotiationStats, Trial};
pub use ripup::{rip_up, RipOptions, RipStats};
pub use route::{edits, route_open, CpuSolver, FieldSolver, Options, Stats};
#[cfg(feature = "gpu")]
pub use gpu::GpuSolver;
#[cfg(feature = "gpu")]
pub use sweep::GpuSweepSolver;
