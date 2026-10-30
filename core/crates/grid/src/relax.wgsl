// One Bellman-Ford relaxation over a routing grid: every reached node offers cost + step to its
// eight neighbours on its layer, and through a via to the same cell on the other layers.
// atomicMin keeps the field monotone, so repeated dispatches converge to the exact integer
// shortest-path costs the CPU reference computes.

struct Params {
    nx: u32,
    ny: u32,
    layers: u32,
    via_cost: u32,
    via_allowed: u32,
    // Independent grids of the same shape laid end to end, one per net routed at once.
    fields: u32,
    // Non-zero: steps along a layer's own direction cost less than across it, as on the CPU
    // (search.rs `step_cost`): horizontal on even layers, vertical on odd ones.
    directional: u32,
};

@group(0) @binding(0) var<uniform> params: Params;
// Bit 0: node is blocked. Bit 1: node is a target. Bit 2: node cost improved (active frontier).
@group(0) @binding(1) var<storage, read_write> blocked: array<atomic<u32>>;
// Per field and cell: UNREACHED where no via may stand, else the extra cost of one there.
@group(0) @binding(2) var<storage, read> via_blocked: array<u32>;
@group(0) @binding(3) var<storage, read_write> cost: array<atomic<u32>>;
// state[0] is the changed flag; state[1 + field] is the best target cost reached so far.
@group(0) @binding(4) var<storage, read_write> state: array<atomic<u32>>;
@group(0) @binding(5) var<storage, read> penalty: array<u32>;
// Per field (stride PATH_STRIDE):
// - [0..4096]: traced path length followed by local node indices (target back to source)
// - [4096..8192]: target count followed by target local node indices
@group(0) @binding(6) var<storage, read_write> paths: array<u32>;

const UNREACHED: u32 = 0xffffffffu;
const PATH_STRIDE: u32 = 8192u;
const TARGET_OFFSET: u32 = 4096u;
const MAX_PATH_LEN: u32 = 4095u;

// a + b, held at UNREACHED rather than wrapping: a wrapped sum would look like a cheap path.
fn sat_add(a: u32, b: u32) -> u32 {
    if (a > UNREACHED - b) {
        return UNREACHED;
    }
    return a + b;
}

// What a step on `layer` along (di, dj) costs, `base` being the plain grid's 10 or 14.
fn step_cost(layer: u32, di: i32, dj: i32, base: u32) -> u32 {
    if (params.directional == 0u) {
        return base;
    }
    if (di != 0 && dj != 0) {
        return 15u;
    }
    if (((layer & 1u) == 0u) == (dj == 0)) {
        return 10u;
    }
    return 12u;
}

fn offer(field: u32, n: u32, value: u32) {
    let b = atomicLoad(&blocked[n]);
    if ((b & 1u) != 0u) {
        return;
    }
    let entered = sat_add(value, penalty[n]);
    if (entered > atomicLoad(&state[1u + field])) {
        return;
    }
    let old = atomicMin(&cost[n], entered);
    if (entered < old) {
        atomicOr(&blocked[n], 4u);
        atomicStore(&state[0], 1u);
        if ((b & 2u) != 0u) {
            atomicMin(&state[1u + field], entered);
        }
    }
}

@compute @workgroup_size(256)
fn relax(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(num_workgroups) groups: vec3<u32>) {
    let node = gid.x + gid.y * groups.x * 256u;
    let cells = params.nx * params.ny;
    let per_field = cells * params.layers;
    if (node >= per_field * params.fields) {
        return;
    }
    let b = atomicLoad(&blocked[node]);
    if ((b & 4u) == 0u) {
        return;
    }
    atomicAnd(&blocked[node], ~4u);
    let c = atomicLoad(&cost[node]);
    if (c == UNREACHED) {
        return;
    }
    let field = node / per_field;
    if (c >= atomicLoad(&state[1u + field])) {
        return;
    }
    let base = field * per_field;
    let local = node % per_field;
    let layer = local / cells;
    let cell = local % cells;
    let i = i32(cell % params.nx);
    let j = i32(cell / params.nx);
    let di = array<i32, 8>(1, -1, 0, 0, 1, 1, -1, -1);
    let dj = array<i32, 8>(0, 0, 1, -1, 1, -1, 1, -1);
    let step = array<u32, 8>(10u, 10u, 10u, 10u, 14u, 14u, 14u, 14u);
    for (var k = 0; k < 8; k++) {
        let ni = i + di[k];
        let nj = j + dj[k];
        if (ni < 0 || nj < 0 || ni >= i32(params.nx) || nj >= i32(params.ny)) {
            continue;
        }
        offer(field, base + layer * cells + u32(nj) * params.nx + u32(ni), sat_add(c, step_cost(layer, di[k], dj[k], step[k])));
    }
    let vp = via_blocked[field * cells + cell];
    if (params.via_allowed != 0u && vp != UNREACHED) {
        for (var l = 0u; l < params.layers; l++) {
            if (l != layer) {
                offer(field, base + l * cells + cell, sat_add(sat_add(c, params.via_cost), vp));
            }
        }
    }
}

@compute @workgroup_size(64)
fn trace_paths(@builtin(global_invocation_id) gid: vec3<u32>) {
    let field = gid.x;
    if (field >= params.fields) {
        return;
    }
    let cells = params.nx * params.ny;
    let per_field = cells * params.layers;
    let base = field * per_field;
    let pbase = field * PATH_STRIDE;
    let num_targets = paths[pbase + TARGET_OFFSET];
    var best_cost = UNREACHED;
    var at = 0u;
    for (var k = 0u; k < num_targets; k++) {
        let t = paths[pbase + TARGET_OFFSET + 1u + k];
        let ct = atomicLoad(&cost[base + t]);
        if (ct < best_cost) {
            best_cost = ct;
            at = t;
        }
    }
    if (best_cost == UNREACHED) {
        paths[pbase] = 0u;
        return;
    }
    var len = 1u;
    paths[pbase + 1u] = at;
    var c_at = best_cost;
    let di = array<i32, 8>(1, -1, 0, 0, 1, 1, -1, -1);
    let dj = array<i32, 8>(0, 0, 1, -1, 1, -1, 1, -1);
    let step = array<u32, 8>(10u, 10u, 10u, 10u, 14u, 14u, 14u, 14u);
    while (c_at > 0u) {
        if (len >= MAX_PATH_LEN) {
            paths[pbase] = UNREACHED;
            return;
        }
        let layer = at / cells;
        let cell = at % cells;
        let i = i32(cell % params.nx);
        let j = i32(cell / params.nx);
        let pen_at = penalty[base + at];
        var found = false;
        var prev = 0u;
        for (var k = 0; k < 8; k++) {
            let ni = i + di[k];
            let nj = j + dj[k];
            if (ni < 0 || nj < 0 || ni >= i32(params.nx) || nj >= i32(params.ny)) {
                continue;
            }
            let n = layer * cells + u32(nj) * params.nx + u32(ni);
            let cn = atomicLoad(&cost[base + n]);
            if (cn != UNREACHED && sat_add(sat_add(cn, step_cost(layer, di[k], dj[k], step[k])), pen_at) == c_at) {
                prev = n;
                found = true;
                break;
            }
        }
        let vp = via_blocked[field * cells + cell];
        if (!found && vp != UNREACHED) {
            for (var l = 0u; l < params.layers; l++) {
                if (l != layer) {
                    let n = l * cells + cell;
                    let cn = atomicLoad(&cost[base + n]);
                    if (cn != UNREACHED && sat_add(sat_add(sat_add(cn, params.via_cost), vp), pen_at) == c_at) {
                        prev = n;
                        found = true;
                        break;
                    }
                }
            }
        }
        if (!found) {
            paths[pbase] = 0u;
            return;
        }
        at = prev;
        c_at = atomicLoad(&cost[base + at]);
        len += 1u;
        paths[pbase + len] = at;
    }
    paths[pbase] = len;
}
