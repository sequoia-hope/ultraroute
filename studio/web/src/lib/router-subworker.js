// Sub-worker for parallel WASM A* routing and WebGPU batch cost-field relaxation.
// Communicates with router-worker.js through a SharedArrayBuffer + Atomics.
import init, { route_packed_job } from '../router/router.js';

const SLOTS_WORD_OFFSET = 64;
const SLOT_STRIDE = 4096;
// Written in a slot's length word when a path does not fit the slot: the router routes that
// job itself.
const TOO_LONG = 0xffffffff;
const INPUT_WORD_OFFSET = SLOTS_WORD_OFFSET + 32 * SLOT_STRIDE;
const UNREACHED = 0xffffffff;
const PATH_STRIDE = 8192;
const TARGET_OFFSET = 4096;
const MAX_TARGETS = 4095;
const WORKGROUP = 256;
const MAX_GROUPS_X = 65535;
const DISPATCHES_PER_BATCH = 32;

const WGSL_SHADER = `
struct Params {
    nx: u32,
    ny: u32,
    layers: u32,
    via_cost: u32,
    via_allowed: u32,
    fields: u32,
};

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read_write> blocked: array<atomic<u32>>;
@group(0) @binding(2) var<storage, read> via_blocked: array<u32>;
@group(0) @binding(3) var<storage, read_write> cost: array<atomic<u32>>;
@group(0) @binding(4) var<storage, read_write> state: array<atomic<u32>>;
@group(0) @binding(5) var<storage, read> penalty: array<u32>;
@group(0) @binding(6) var<storage, read_write> paths: array<u32>;

const UNREACHED: u32 = 0xffffffffu;
const PATH_STRIDE: u32 = 8192u;
const TARGET_OFFSET: u32 = 4096u;
const MAX_PATH_LEN: u32 = 4095u;

fn offer(field: u32, n: u32, value: u32) {
    let b = atomicLoad(&blocked[n]);
    if ((b & 1u) != 0u) {
        return;
    }
    let entered = value + penalty[n];
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
        offer(field, base + layer * cells + u32(nj) * params.nx + u32(ni), c + step[k]);
    }
    if (params.via_allowed != 0u && via_blocked[field * cells + cell] == 0u) {
        for (var l = 0u; l < params.layers; l++) {
            if (l != layer) {
                offer(field, base + l * cells + cell, c + params.via_cost);
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
            if (cn != UNREACHED && cn + step[k] + pen_at == c_at) {
                prev = n;
                found = true;
                break;
            }
        }
        if (!found && via_blocked[field * cells + cell] == 0u) {
            for (var l = 0u; l < params.layers; l++) {
                if (l != layer) {
                    let n = l * cells + cell;
                    let cn = atomicLoad(&cost[base + n]);
                    if (cn != UNREACHED && cn + params.via_cost + pen_at == c_at) {
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
`;

let wasmReady = init();
let gpuCtx = null;

async function initWebGPU() {
  if (typeof navigator === 'undefined' || !navigator.gpu) return null;
  try {
    const adapter = await navigator.gpu.requestAdapter({ powerPreference: 'high-performance' });
    if (!adapter) return null;
    const device = await adapter.requestDevice({
      requiredLimits: {
        maxStorageBufferBindingSize: adapter.limits.maxStorageBufferBindingSize,
        maxBufferSize: adapter.limits.maxBufferSize,
      },
    });
    const layout = device.createBindGroupLayout({
      entries: [
        { binding: 0, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'uniform' } },
        { binding: 1, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'storage' } },
        { binding: 2, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'read-only-storage' } },
        { binding: 3, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'storage' } },
        { binding: 4, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'storage' } },
        { binding: 5, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'read-only-storage' } },
        { binding: 6, visibility: GPUShaderStage.COMPUTE, buffer: { type: 'storage' } },
      ],
    });
    const pipelineLayout = device.createPipelineLayout({ bindGroupLayouts: [layout] });
    const module = device.createShaderModule({ code: WGSL_SHADER });
    const relaxPipeline = device.createComputePipeline({
      layout: pipelineLayout,
      compute: { module, entryPoint: 'relax' },
    });
    const tracePipeline = device.createComputePipeline({
      layout: pipelineLayout,
      compute: { module, entryPoint: 'trace_paths' },
    });
    return {
      device,
      layout,
      relaxPipeline,
      tracePipeline,
      buffers: null,
      scratchBlocked: new Uint32Array(0),
      scratchVia: new Uint32Array(0),
      scratchPenalty: new Uint32Array(0),
      scratchCost: new Uint32Array(0),
      scratchPaths: new Uint32Array(0),
    };
  } catch (_) {
    return null;
  }
}

function ensureGpuBuffers(ctx, nodes, cellsTotal, fields) {
  const b = ctx.buffers;
  if (b && nodes <= b.nodesCap && cellsTotal <= b.cellsCap && fields <= b.fieldsCap) {
    return b;
  }
  if (b) {
    b.uniform.destroy();
    b.blocked.destroy();
    b.viaBlocked.destroy();
    b.cost.destroy();
    b.state.destroy();
    b.penalty.destroy();
    b.paths.destroy();
    b.stateStaging.destroy();
    b.pathsStaging.destroy();
  }
  const nodesCap = Math.max(nodes, 1);
  const cellsCap = Math.max(cellsTotal, 1);
  const fieldsCap = Math.max(fields, 16);
  const { device, layout } = ctx;
  const storageDst = GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST;
  const storageRw = storageDst | GPUBufferUsage.COPY_SRC;
  const uniform = device.createBuffer({ size: 32, usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST });
  const blocked = device.createBuffer({ size: nodesCap * 4, usage: storageDst });
  const viaBlocked = device.createBuffer({ size: cellsCap * 4, usage: storageDst });
  const cost = device.createBuffer({ size: nodesCap * 4, usage: storageRw });
  const state = device.createBuffer({ size: (fieldsCap + 1) * 4, usage: storageRw });
  const penalty = device.createBuffer({ size: nodesCap * 4, usage: storageDst });
  const paths = device.createBuffer({ size: fieldsCap * PATH_STRIDE * 4, usage: storageRw });
  const stateStaging = device.createBuffer({ size: 16, usage: GPUBufferUsage.MAP_READ | GPUBufferUsage.COPY_DST });
  const pathsStaging = device.createBuffer({ size: fieldsCap * PATH_STRIDE * 4, usage: GPUBufferUsage.MAP_READ | GPUBufferUsage.COPY_DST });
  const bindGroup = device.createBindGroup({
    layout,
    entries: [
      { binding: 0, resource: { buffer: uniform } },
      { binding: 1, resource: { buffer: blocked } },
      { binding: 2, resource: { buffer: viaBlocked } },
      { binding: 3, resource: { buffer: cost } },
      { binding: 4, resource: { buffer: state } },
      { binding: 5, resource: { buffer: penalty } },
      { binding: 6, resource: { buffer: paths } },
    ],
  });
  ctx.buffers = {
    nodesCap,
    cellsCap,
    fieldsCap,
    uniform,
    blocked,
    viaBlocked,
    cost,
    state,
    penalty,
    paths,
    stateStaging,
    pathsStaging,
    bindGroup,
  };
  return ctx.buffers;
}

async function runWebGpuBatch(ctx, words, u32) {
  const fields = words[0];
  const nx = words[1];
  const ny = words[2];
  const layers = words[3];
  const viaCost = words[4];
  const viaAllowed = words[5];
  const cells = nx * ny;
  const perField = cells * layers;
  const nodes = perField * fields;
  const cellsTotal = cells * fields;
  const blockedWords = (perField + 31) >>> 5;
  const viaWords = (cells + 31) >>> 5;

  const bufs = ensureGpuBuffers(ctx, nodes, cellsTotal, fields);
  if (ctx.scratchBlocked.length < nodes) ctx.scratchBlocked = new Uint32Array(nodes);
  if (ctx.scratchVia.length < cellsTotal) ctx.scratchVia = new Uint32Array(cellsTotal);
  if (ctx.scratchPenalty.length < nodes) ctx.scratchPenalty = new Uint32Array(nodes);
  if (ctx.scratchCost.length < nodes) ctx.scratchCost = new Uint32Array(nodes);
  if (ctx.scratchPaths.length < fields * PATH_STRIDE) ctx.scratchPaths = new Uint32Array(fields * PATH_STRIDE);

  const sBlocked = ctx.scratchBlocked.subarray(0, nodes);
  const sVia = ctx.scratchVia.subarray(0, cellsTotal);
  const sPenalty = ctx.scratchPenalty.subarray(0, nodes);
  const sCost = ctx.scratchCost.subarray(0, nodes);
  const sPaths = ctx.scratchPaths.subarray(0, fields * PATH_STRIDE);
  sCost.fill(UNREACHED);
  sPaths.fill(0);

  const initState = new Uint32Array(fields + 1);
  initState.fill(UNREACHED);
  initState[0] = 0;

  for (let f = 0; f < fields; f++) {
    let pos = words[6 + f];
    const base = f * perField;
    for (let i = 0; i < perField; i++) {
      sBlocked[base + i] = (words[pos + (i >>> 5)] >>> (i & 31)) & 1;
    }
    pos += blockedWords;

    const vbase = f * cells;
    for (let i = 0; i < cells; i++) {
      sVia[vbase + i] = (words[pos + (i >>> 5)] >>> (i & 31)) & 1;
    }
    pos += viaWords;

    const penMode = words[pos++];
    if (penMode === 0) {
      sPenalty.fill(0, base, base + perField);
    } else if (penMode === 1) {
      sPenalty.set(words.subarray(pos, pos + perField), base);
      pos += perField;
    } else if (penMode === 2) {
      sPenalty.fill(0, base, base + perField);
      const nz = words[pos++];
      for (let k = 0; k < nz; k++) {
        const idx = words[pos + k * 2];
        const val = words[pos + k * 2 + 1];
        if (idx < perField) sPenalty[base + idx] = val;
      }
      pos += nz * 2;
    }

    const sLen = words[pos++];
    const sourcesStart = pos;
    pos += sLen;

    const tLen = words[pos++];
    const targetsStart = pos;
    pos += tLen;

    const pbase = f * PATH_STRIDE;
    let tCount = 0;
    for (let k = 0; k < tLen; k++) {
      const t = words[targetsStart + k];
      if ((sBlocked[base + t] & 1) === 0) {
        sBlocked[base + t] |= 2;
        if (tCount < MAX_TARGETS) {
          sPaths[pbase + TARGET_OFFSET + 1 + tCount] = t;
          tCount++;
        }
      }
    }
    sPaths[pbase + TARGET_OFFSET] = tCount;
    if (tCount === 0) initState[1 + f] = 0;

    let hasSource = false;
    for (let k = 0; k < sLen; k++) {
      const s = words[sourcesStart + k];
      if ((sBlocked[base + s] & 1) === 0) {
        hasSource = true;
        sCost[base + s] = 0;
        sBlocked[base + s] |= 4;
        if ((sBlocked[base + s] & 2) !== 0) {
          initState[1 + f] = 0;
        }
      }
    }
    if (!hasSource) initState[1 + f] = 0;
  }

  const { device, relaxPipeline, tracePipeline } = ctx;
  const params = new Uint32Array([nx, ny, layers, viaCost, viaAllowed, fields, 0, 0]);
  device.queue.writeBuffer(bufs.uniform, 0, params);
  device.queue.writeBuffer(bufs.blocked, 0, sBlocked);
  device.queue.writeBuffer(bufs.viaBlocked, 0, sVia);
  device.queue.writeBuffer(bufs.cost, 0, sCost);
  device.queue.writeBuffer(bufs.state, 0, initState);
  device.queue.writeBuffer(bufs.penalty, 0, sPenalty);
  device.queue.writeBuffer(bufs.paths, 0, sPaths);

  const groups = Math.ceil(nodes / WORKGROUP);
  const gx = groups <= MAX_GROUPS_X ? groups : MAX_GROUPS_X;
  const gy = groups <= MAX_GROUPS_X ? 1 : Math.ceil(groups / MAX_GROUPS_X);
  const zeroWord = new Uint32Array([0]);

  while (true) {
    device.queue.writeBuffer(bufs.state, 0, zeroWord);
    const encoder = device.createCommandEncoder();
    const pass = encoder.beginComputePass();
    pass.setPipeline(relaxPipeline);
    pass.setBindGroup(0, bufs.bindGroup);
    for (let d = 0; d < DISPATCHES_PER_BATCH; d++) {
      pass.dispatchWorkgroups(gx, gy, 1);
    }
    pass.end();
    encoder.copyBufferToBuffer(bufs.state, 0, bufs.stateStaging, 0, 4);
    device.queue.submit([encoder.finish()]);
    await bufs.stateStaging.mapAsync(GPUMapMode.READ, 0, 4);
    const changed = new Uint32Array(bufs.stateStaging.getMappedRange(0, 4))[0];
    bufs.stateStaging.unmap();
    if (changed === 0) break;
  }

  const readBytes = fields * PATH_STRIDE * 4;
  const encoder = device.createCommandEncoder();
  const pass = encoder.beginComputePass();
  pass.setPipeline(tracePipeline);
  pass.setBindGroup(0, bufs.bindGroup);
  pass.dispatchWorkgroups(Math.ceil(fields / 64), 1, 1);
  pass.end();
  encoder.copyBufferToBuffer(bufs.paths, 0, bufs.pathsStaging, 0, readBytes);
  device.queue.submit([encoder.finish()]);
  await bufs.pathsStaging.mapAsync(GPUMapMode.READ, 0, readBytes);
  const pathWords = new Uint32Array(bufs.pathsStaging.getMappedRange(0, readBytes));

  for (let f = 0; f < fields; f++) {
    const pbase = f * PATH_STRIDE;
    const slotBase = SLOTS_WORD_OFFSET + f * SLOT_STRIDE;
    const len = pathWords[pbase];
    if (len >= SLOT_STRIDE) {
      u32[slotBase] = TOO_LONG;
    } else if (len === 0) {
      u32[slotBase] = 0;
    } else {
      u32[slotBase] = len;
      for (let k = 0; k < len; k++) {
        u32[slotBase + 1 + k] = pathWords[pbase + len - k];
      }
    }
  }
  bufs.pathsStaging.unmap();
}

self.onmessage = async ({ data }) => {
  await wasmReady;
  const { sab, workerId, enableGpu } = data;
  const ctrl = new Int32Array(sab, 0, 64);
  const u32 = new Uint32Array(sab);

  if (workerId === 0 && enableGpu) {
    gpuCtx = await initWebGPU();
  }
  self.postMessage({ ready: true, workerId, hasGpu: Boolean(gpuCtx) });

  let lastEpoch = 0;
  while (true) {
    const waitRes = Atomics.wait(ctrl, 0, lastEpoch);
    if (waitRes === 'ok' || waitRes === 'not-equal') {
      const epoch = Atomics.load(ctrl, 0);
      if (epoch < 0) break;
      if (epoch === lastEpoch) continue;
      lastEpoch = epoch;

      const numJobs = Atomics.load(ctrl, 1);
      const inputLen = Atomics.load(ctrl, 5);
      const mode = Atomics.load(ctrl, 6);
      const words = u32.subarray(INPUT_WORD_OFFSET, INPUT_WORD_OFFSET + inputLen);

      if (mode === 1 && workerId === 0 && gpuCtx) {
        try {
          await runWebGpuBatch(gpuCtx, words, u32);
          Atomics.store(ctrl, 4, 1);
          Atomics.notify(ctrl, 4, 1);
          continue;
        } catch (_) {
          // Fall back to parallel WASM worker pool if WebGPU fails mid-batch.
        }
      }
      if (mode === 1 && workerId !== 0) {
        continue;
      }

      while (true) {
        const k = Atomics.add(ctrl, 2, 1);
        if (k >= numJobs) break;
        const path = route_packed_job(words, k);
        const slotBase = SLOTS_WORD_OFFSET + k * SLOT_STRIDE;
        if (path.length > SLOT_STRIDE) {
          u32[slotBase] = TOO_LONG;
        } else {
          u32.set(path, slotBase);
        }
        if (Atomics.add(ctrl, 3, 1) + 1 === numJobs) {
          Atomics.store(ctrl, 4, 1);
          Atomics.notify(ctrl, 4, 1);
        }
      }
    }
  }
};
