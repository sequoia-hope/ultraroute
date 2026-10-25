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
};

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> blocked: array<u32>;
@group(0) @binding(2) var<storage, read> via_blocked: array<u32>;
@group(0) @binding(3) var<storage, read_write> cost: array<atomic<u32>>;
@group(0) @binding(4) var<storage, read_write> changed: atomic<u32>;
@group(0) @binding(5) var<storage, read> penalty: array<u32>;

const UNREACHED: u32 = 0xffffffffu;

fn offer(n: u32, value: u32) {
    if (blocked[n] != 0u) {
        return;
    }
    let entered = value + penalty[n];
    let old = atomicMin(&cost[n], entered);
    if (entered < old) {
        atomicStore(&changed, 1u);
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
    if (blocked[node] != 0u) {
        return;
    }
    let c = atomicLoad(&cost[node]);
    if (c == UNREACHED) {
        return;
    }
    let field = node / per_field;
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
        offer(base + layer * cells + u32(nj) * params.nx + u32(ni), c + step[k]);
    }
    if (params.via_allowed != 0u && via_blocked[field * cells + cell] == 0u) {
        for (var l = 0u; l < params.layers; l++) {
            if (l != layer) {
                offer(base + l * cells + cell, c + params.via_cost);
            }
        }
    }
}
