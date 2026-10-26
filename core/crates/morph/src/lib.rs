//! Moving parts on a routed board and letting the copper flow with them.

pub mod escape;
pub mod flow;
pub mod warp;

pub use escape::{clear_lane, comb, edges, fine_pitch, poles_at, shift_copper, shift_poles, Edge, EscapeOptions, EscapeStats, Pole};
pub use flow::{flow, warp_for, FlowStats};
pub use warp::WarpField;
