// GAMER-style directional sweeps (Lin and Wong): each thread owns one line of one layer (a row,
// column or diagonal) and scans it in one direction, carrying min(cost, previous + step +
// penalty). A path with b bends settles in about b + 1 rounds of the eight directional sweeps
// plus a via pass, instead of one Bellman-Ford dispatch per cell of path length. Costs are the
// same integers as the CPU reference, so the converged field is identical.

struct Params {
    nx: u32,
    ny: u32,
    layers: u32,
    via_cost: u32,
    via_allowed: u32,
    // 0..8: +x, -x, +y, -y, +x+y, -x-y, +x-y, -x+y
    direction: u32,
};

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> blocked: array<u32>;
// Per cell: UNREACHED where no via may stand, else the extra cost of one there.
@group(0) @binding(2) var<storage, read> via_blocked: array<u32>;
@group(0) @binding(3) var<storage, read_write> cost: array<atomic<u32>>;
@group(0) @binding(4) var<storage, read_write> changed: atomic<u32>;
@group(0) @binding(5) var<storage, read> penalty: array<u32>;

const UNREACHED: u32 = 0xffffffffu;

fn node(layer: u32, i: i32, j: i32) -> u32 {
    return layer * params.nx * params.ny + u32(j) * params.nx + u32(i);
}

// Scans from (i, j) in steps of (di, dj) to the grid edge.
fn scan(layer: u32, i0: i32, j0: i32, di: i32, dj: i32, step: u32) {
    var i = i0;
    var j = j0;
    var carried = UNREACHED;
    loop {
        if (i < 0 || j < 0 || i >= i32(params.nx) || j >= i32(params.ny)) {
            break;
        }
        let n = node(layer, i, j);
        if (blocked[n] != 0u) {
            carried = UNREACHED;
        } else {
            var here = atomicLoad(&cost[n]);
            if (carried != UNREACHED) {
                let offered = carried + step + penalty[n];
                if (offered < here) {
                    atomicStore(&cost[n], offered);
                    atomicStore(&changed, 1u);
                    here = offered;
                }
            }
            carried = here;
        }
        i += di;
        j += dj;
    }
}

@compute @workgroup_size(64)
fn sweep(@builtin(global_invocation_id) gid: vec3<u32>) {
    let line = gid.x;
    let layer = gid.y;
    if (layer >= params.layers) {
        return;
    }
    let nx = i32(params.nx);
    let ny = i32(params.ny);
    let d = params.direction;
    if (d < 2u) {
        if (line >= params.ny) { return; }
        if (d == 0u) { scan(layer, 0, i32(line), 1, 0, 10u); } else { scan(layer, nx - 1, i32(line), -1, 0, 10u); }
        return;
    }
    if (d < 4u) {
        if (line >= params.nx) { return; }
        if (d == 2u) { scan(layer, i32(line), 0, 0, 1, 10u); } else { scan(layer, i32(line), ny - 1, 0, -1, 10u); }
        return;
    }
    // Diagonals: line k in 0..nx+ny-1 starts on the first column or the first/last row.
    if (line >= params.nx + params.ny - 1u) {
        return;
    }
    let k = i32(line);
    if (d == 4u) {
        // +x+y: starts on the left column (k < ny) going up, or the bottom row.
        if (k < ny) { scan(layer, 0, ny - 1 - k, 1, 1, 14u); } else { scan(layer, k - ny + 1, 0, 1, 1, 14u); }
    } else if (d == 5u) {
        if (k < ny) { scan(layer, nx - 1, k, -1, -1, 14u); } else { scan(layer, nx - 1 - (k - ny + 1), ny - 1, -1, -1, 14u); }
    } else if (d == 6u) {
        // +x-y: starts on the left column going down, or the top row.
        if (k < ny) { scan(layer, 0, k, 1, -1, 14u); } else { scan(layer, k - ny + 1, ny - 1, 1, -1, 14u); }
    } else {
        if (k < ny) { scan(layer, nx - 1, ny - 1 - k, -1, 1, 14u); } else { scan(layer, nx - 1 - (k - ny + 1), 0, -1, 1, 14u); }
    }
}

@compute @workgroup_size(256)
fn vias(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) groups: vec3<u32>) {
    let cell = gid.x + gid.y * groups.x * 256u;
    let cells = params.nx * params.ny;
    if (cell >= cells || params.via_allowed == 0u) {
        return;
    }
    let vp = via_blocked[cell];
    if (vp == UNREACHED) {
        return;
    }
    var best = UNREACHED;
    for (var l = 0u; l < params.layers; l++) {
        best = min(best, atomicLoad(&cost[l * cells + cell]));
    }
    if (best == UNREACHED) {
        return;
    }
    for (var l = 0u; l < params.layers; l++) {
        let n = l * cells + cell;
        if (blocked[n] != 0u) {
            continue;
        }
        let offered = best + params.via_cost + vp + penalty[n];
        let old = atomicMin(&cost[n], offered);
        if (offered < old) {
            atomicStore(&changed, 1u);
        }
    }
}
