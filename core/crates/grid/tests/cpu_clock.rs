//! The CPU clock is process-wide, so it gets a test binary of its own: in the library's tests it
//! would stop the wall clock for every other test running beside it.

use std::time::Duration;

use grid::clock::{self, Instant};
use grid::Deadline;

/// Sleeping uses no CPU, so on the CPU clock a deadline does not pass while the process sleeps,
/// and passes once it works.
#[test]
fn a_cpu_deadline_waits_out_a_sleep_and_not_work() {
    clock::use_cpu_time();
    let d = Deadline::max_seconds(Some(0.05));
    let wall = Instant::now();
    std::thread::sleep(Duration::from_millis(200));
    assert!(wall.elapsed() >= Duration::from_millis(200));
    assert!(!d.expired(), "200 ms asleep is not 50 ms of CPU");
    let t = clock::now();
    let mut x = 0u64;
    while !d.expired() {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        assert!(wall.elapsed() < Duration::from_secs(10), "the CPU clock never moved");
    }
    std::hint::black_box(x);
    assert!(clock::since(t) > Duration::ZERO);
}
