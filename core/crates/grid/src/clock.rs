//! The clock the router keeps time by: the standard one, except in a browser, where the standard
//! one panics and the page's own is used instead.

#[cfg(not(target_arch = "wasm32"))]
pub use std::time::Instant;
#[cfg(target_arch = "wasm32")]
pub use web_time::Instant;
