//! Moving parts on a routed board and letting the copper flow with them.

pub mod compact;
pub mod escape;
pub mod flow;
pub mod scale;
pub mod warp;

pub use compact::{compact, dfm_counts, AcidTraps, CompactOptions, CompactReport, StepReport};
pub use escape::{escape_room, fine_pitch, shift_copper};
pub use flow::{flow, warp_for, FlowStats};
pub use scale::{scale, ScaleOptions, ScaleReport};
pub use warp::WarpField;
