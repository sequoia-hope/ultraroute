//! The GPU's cost field is the CPU's, node for node, on a real board: priced, with routing
//! regions' costs, and with two-layer boards' directional steps. Built only with the `gpu`
//! feature; it passes, saying so, on a machine with no GPU.
#![cfg(feature = "gpu")]

use std::path::PathBuf;

use board::Board;
use ::grid::*;

fn earring_grid() -> RouteGrid {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/earring.kicad_pcb");
    let (board, _) = Board::load(&path).unwrap();
    let net = 1;
    ObstacleMap::new(&board, Profile::for_net(&board, net), 200_000, true).view(net)
}

/// A free node near each end of the grid, as sources.
fn sources(grid: &RouteGrid) -> Vec<usize> {
    let free: Vec<usize> = (0..grid.blocked.len()).filter(|&n| !grid.blocked[n]).collect();
    vec![free[free.len() / 3], free[2 * free.len() / 3]]
}

#[test]
fn the_gpus_field_is_the_cpus() {
    let Some(mut gpu) = GpuSolver::new() else {
        eprintln!("no GPU here: nothing to compare");
        return;
    };
    let mut grid = earring_grid();
    let s = sources(&grid);
    // Priced: a deterministic scatter of penalties.
    grid.penalty = (0..grid.blocked.len()).map(|n| ((n * 2_654_435_761) >> 7) as u32 % 50).collect();
    let cpu = CpuSolver.solve(&grid, &s, 120);
    assert_eq!(gpu.solve(&grid, &s, 120), cpu, "priced");
    // With a region's cost on top, and directional steps.
    grid.region_cost = (0..grid.blocked.len()).map(|n| if (n / 97) % 5 == 0 { 300 } else { 0 }).collect();
    grid.directional = true;
    let cpu = CpuSolver.solve(&grid, &s, 120);
    assert_eq!(gpu.solve(&grid, &s, 120), cpu, "with region costs and directional steps");
}
