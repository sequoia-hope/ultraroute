//! Grid routing on the core board model: a conservative obstacle map per net class, an integer
//! cost field from the sources and a walk back down it, laid as tracks and vias.

#[cfg(feature = "gpu")]
pub mod gpu;
pub mod blame;
pub mod chase;
pub mod cleanup;
pub mod clock;
pub mod grid;
pub mod jitter;
pub mod negotiate;
pub mod prefanout;
pub mod report;
pub mod nudge;
pub mod pairs;
pub mod planes;
pub mod push;
pub mod ripup;
pub mod route;
mod regions;
pub mod search;
pub mod shove;
pub mod tune;
#[cfg(feature = "gpu")]
pub mod sweep;

pub use blame::{blame, Blame, BlameOptions, BlameReport};
pub use chase::{chase, ChaseOptions, ChaseStats};
pub use cleanup::{drop_duplicates, drop_duplicates_watch, prune_dangling, prune_dangling_watch, pull_tight, pull_tight_watch, CleanStats};
pub use grid::{ObstacleMap, Profile, RouteGrid, ViaSpec};
pub use nudge::{nudge, nudge_watch, NudgeEvent, NudgeOptions, NudgeStats};
pub use push::{push, push_watch, rip_broken, rip_broken_items, Field, PushOptions, PushStats, PushStep};
pub use planes::{join_planes, PlaneStats};
pub use pairs::{diff_pairs, route_pairs, route_pairs_watch, shares_layer, Pair, PairOptions, PairRun, PairStats};
pub use prefanout::{prefanout, prune_closed_escapes, EscapeMode, EscapeOrder, PrefanoutOptions, PrefanoutStats};
pub use report::{item_name, Activity, Focus, Log};
pub use negotiate::{price_into, route_negotiated, route_negotiated_rounds, via_price_into, NegotiationOptions, NegotiationStats, RoundView, Trial};
pub use ripup::{rip_up, rip_up_watch, RipAttempt, RipOptions, RipStats};
pub use route::{edits, keep_orphans, route_open, terminal_cells, CpuSolver, FieldSolver, Options, PricedJob, RoundPrices, Stats};
pub use tune::{match_lengths, match_lengths_watch, pair_lengths, TuneBump, TuneStats};
pub use shove::{plough_fields, shove_escape, shove_escape_steps, Plough, ShoveEvent, ShoveOptions, ShoveRound, ShoveStats, ShoveTry};
#[cfg(feature = "gpu")]
pub use gpu::GpuSolver;
#[cfg(feature = "gpu")]
pub use sweep::GpuSweepSolver;
