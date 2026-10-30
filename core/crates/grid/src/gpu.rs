//! The cost field on the GPU: `relax.wgsl` dispatched until nothing improves, in bounded
//! batches so no single submission holds the GPU long enough to stall the desktop.

use crate::grid::RouteGrid;
use crate::route::{CpuSolver, FieldSolver};
use crate::search::{trace_back, walled_in, GridPath, UNREACHED};

/// Relaxation sweeps per submission; the changed flag is read back between submissions.
const DISPATCHES_PER_BATCH: u32 = 32;
const WORKGROUP: u32 = 256;
const MAX_GROUPS_X: u32 = 65_535;
const PATH_STRIDE: usize = 8192;
const TARGET_OFFSET: usize = 4096;
const MAX_TARGETS: usize = 4095;

struct CachedBuffers {
    nodes_cap: usize,
    cells_cap: usize,
    fields_cap: usize,
    uniform: wgpu::Buffer,
    blocked: wgpu::Buffer,
    via_blocked: wgpu::Buffer,
    cost: wgpu::Buffer,
    state: wgpu::Buffer,
    penalty: wgpu::Buffer,
    paths: wgpu::Buffer,
    state_staging: wgpu::Buffer,
    paths_staging: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

pub struct GpuSolver {
    device: wgpu::Device,
    queue: wgpu::Queue,
    layout: wgpu::BindGroupLayout,
    relax_pipeline: wgpu::ComputePipeline,
    trace_pipeline: wgpu::ComputePipeline,
    buffers: Option<CachedBuffers>,
    scratch_blocked: Vec<u32>,
    scratch_via: Vec<u32>,
    scratch_penalty: Vec<u32>,
    scratch_cost: Vec<u32>,
    scratch_paths: Vec<u32>,
    pub adapter_name: String,
    pub dispatches: u64,
}

/// The first discrete or integrated adapter on Vulkan, Metal or DX12 (asking for every backend
/// can hide a discrete GPU's Vulkan adapter behind its GL twin); `None` without one.
pub(crate) fn open_device() -> Option<(wgpu::Device, wgpu::Queue, String)> {
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
    descriptor.backends = wgpu::Backends::PRIMARY;
    let instance = wgpu::Instance::new(descriptor);
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
        ..Default::default()
    }))
    .ok()?;
    let adapter_name = adapter.get_info().name;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("copper-gpu"),
        required_features: wgpu::Features::empty(),
        required_limits: adapter.limits(),
        ..Default::default()
    }))
    .ok()?;
    Some((device, queue, adapter_name))
}

/// Copies `size` bytes of `buffer` back to the host and waits for them.
pub(crate) fn read_back(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    buffer: &wgpu::Buffer,
    size: u64,
) -> Vec<u8> {
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("staging"),
        size,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    encoder.copy_buffer_to_buffer(buffer, 0, &staging, 0, size);
    queue.submit([encoder.finish()]);
    let slice = staging.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    let _ = device.poll(wgpu::PollType::wait_indefinitely());
    let bytes = slice
        .get_mapped_range()
        .map(|view| view.to_vec())
        .unwrap_or_default();
    staging.unmap();
    bytes
}

/// Whether no source can reach the targets, by `route_astar`'s own check ([`walled_in`]): they
/// sit in a small pocket of free cells, and there is no field to relax for them.
fn pocket_trapped(grid: &RouteGrid, sources: &[usize], targets: &[usize]) -> bool {
    use std::collections::HashSet;
    let src: HashSet<usize> = sources.iter().copied().filter(|&s| !grid.blocked[s]).collect();
    let mut queue: Vec<usize> = targets.iter().copied().filter(|&t| !grid.blocked[t]).collect();
    queue.sort_unstable();
    queue.dedup();
    if queue.is_empty() {
        return true;
    }
    if queue.iter().any(|t| src.contains(t)) {
        return false;
    }
    let mut seen: HashSet<usize> = queue.iter().copied().collect();
    walled_in(grid, &mut queue, &mut |n| if src.contains(&n) { None } else { Some(seen.insert(n)) })
}

fn as_bytes(words: &[u32]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(words.as_ptr() as *const u8, words.len() * 4) }
}

impl GpuSolver {
    pub fn new() -> Option<GpuSolver> {
        let (device, queue, adapter_name) = open_device()?;
        let storage = |binding, read_only| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("relax_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                storage(1, false),
                storage(2, true),
                storage(3, false),
                storage(4, false),
                storage(5, true),
                storage(6, false),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("relax_pipeline_layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("relax"),
            source: wgpu::ShaderSource::Wgsl(include_str!("relax.wgsl").into()),
        });
        let make_pipeline = |entry: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&pipeline_layout),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let relax_pipeline = make_pipeline("relax");
        let trace_pipeline = make_pipeline("trace_paths");
        Some(GpuSolver {
            device,
            queue,
            layout,
            relax_pipeline,
            trace_pipeline,
            buffers: None,
            scratch_blocked: Vec::new(),
            scratch_via: Vec::new(),
            scratch_penalty: Vec::new(),
            scratch_cost: Vec::new(),
            scratch_paths: Vec::new(),
            adapter_name,
            dispatches: 0,
        })
    }

    fn ensure_buffers(&mut self, nodes: usize, cells_total: usize, fields: usize) {
        let needs_realloc = match &self.buffers {
            Some(b) => nodes > b.nodes_cap || cells_total > b.cells_cap || fields > b.fields_cap,
            None => true,
        };
        if !needs_realloc {
            return;
        }
        let nodes_cap = nodes.max(1);
        let cells_cap = cells_total.max(1);
        let fields_cap = fields.max(BATCH);
        let storage_dst = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST;
        let storage_rw = storage_dst | wgpu::BufferUsages::COPY_SRC;
        let create = |label, size: u64, usage| {
            self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage,
                mapped_at_creation: false,
            })
        };
        let uniform = create("params", 32, wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST);
        let blocked = create("blocked", (nodes_cap * 4) as u64, storage_dst);
        let via_blocked = create("via_blocked", (cells_cap * 4) as u64, storage_dst);
        let cost = create("cost", (nodes_cap * 4) as u64, storage_rw);
        let state = create("state", ((fields_cap + 1) * 4) as u64, storage_rw);
        let penalty = create("penalty", (nodes_cap * 4) as u64, storage_dst);
        let paths = create("paths", (fields_cap * PATH_STRIDE * 4) as u64, storage_rw);
        let state_staging = create(
            "state_staging",
            16,
            wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        );
        let paths_staging = create(
            "paths_staging",
            (fields_cap * PATH_STRIDE * 4) as u64,
            wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        );
        let resources = [
            uniform.as_entire_binding(),
            blocked.as_entire_binding(),
            via_blocked.as_entire_binding(),
            cost.as_entire_binding(),
            state.as_entire_binding(),
            penalty.as_entire_binding(),
            paths.as_entire_binding(),
        ];
        let entries: Vec<wgpu::BindGroupEntry> = resources
            .into_iter()
            .enumerate()
            .map(|(binding, resource)| wgpu::BindGroupEntry {
                binding: binding as u32,
                resource,
            })
            .collect();
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("relax_bg"),
            layout: &self.layout,
            entries: &entries,
        });
        self.buffers = Some(CachedBuffers {
            nodes_cap,
            cells_cap,
            fields_cap,
            uniform,
            blocked,
            via_blocked,
            cost,
            state,
            penalty,
            paths,
            state_staging,
            paths_staging,
            bind_group,
        });
    }

    fn read_staging_u32(device: &wgpu::Device, queue: &wgpu::Queue, src: &wgpu::Buffer, staging: &wgpu::Buffer, size: u64) -> Vec<u32> {
        let mut encoder = device.create_command_encoder(&Default::default());
        encoder.copy_buffer_to_buffer(src, 0, staging, 0, size);
        queue.submit([encoder.finish()]);
        let slice = staging.slice(..size);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
        let words = slice
            .get_mapped_range()
            .map(|view| {
                view.chunks_exact(4)
                    .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect()
            })
            .unwrap_or_default();
        staging.unmap();
        words
    }
}

/// Fields solved together; each holds a few tens of megabytes on the device.
const BATCH: usize = 16;

impl FieldSolver for GpuSolver {
    fn solve(&mut self, grid: &RouteGrid, sources: &[usize], via_cost: u32) -> Vec<u32> {
        self.solve_batch(&[(grid, sources)], via_cost).remove(0)
    }

    fn solve_batch(&mut self, jobs: &[(&RouteGrid, &[usize])], via_cost: u32) -> Vec<Vec<u32>> {
        GpuSolver::solve_batch(self, jobs, via_cost)
    }

    fn route(&mut self, grid: &RouteGrid, sources: &[usize], targets: &[usize], via_cost: u32) -> Option<GridPath> {
        self.route_batch(&[(grid, sources, targets)], via_cost).remove(0)
    }

    fn route_batch(&mut self, jobs: &[(&RouteGrid, &[usize], &[usize])], via_cost: u32) -> Vec<Option<GridPath>> {
        GpuSolver::route_batch(self, jobs, via_cost)
    }

    fn batch_size(&self) -> usize {
        BATCH
    }
}

/// How a batch's relaxation ended.
#[derive(PartialEq)]
enum Relaxed {
    Done,
    /// The grids' deadline passed first: no field is complete.
    Cut,
    /// The device did not give its results back: the CPU solves the batch instead.
    Failed,
}

impl GpuSolver {
    /// Relaxes the batch's fields until nothing changes.
    fn run_relax(&mut self, jobs: &[(&RouteGrid, &[usize], &[usize])], via_cost: u32, with_targets: bool) -> Relaxed {
        let (first, _, _) = jobs[0];
        let per_field = first.blocked.len();
        let cells = first.nx * first.ny;
        let fields = jobs.len();
        let nodes = per_field * fields;
        let cells_total = cells * fields;
        self.ensure_buffers(nodes, cells_total, fields);

        self.scratch_blocked.clear();
        self.scratch_blocked.reserve(nodes);
        self.scratch_via.clear();
        self.scratch_via.reserve(cells_total);
        self.scratch_penalty.clear();
        self.scratch_penalty.reserve(nodes);
        self.scratch_cost.clear();
        self.scratch_cost.resize(nodes, UNREACHED);
        let mut init_state = vec![UNREACHED; fields + 1];
        init_state[0] = 0;

        if with_targets {
            self.scratch_paths.clear();
            self.scratch_paths.resize(fields * PATH_STRIDE, 0);
        }

        for (f_idx, &(grid, sources, targets)) in jobs.iter().enumerate() {
            assert_eq!(grid.blocked.len(), per_field, "batched grids share one shape");
            let base = f_idx * per_field;
            self.scratch_blocked.extend(grid.blocked.iter().map(|&b| u32::from(b)));
            if with_targets {
                let pbase = f_idx * PATH_STRIDE;
                let mut t_count = 0usize;
                for &t in targets {
                    if !grid.blocked[t] {
                        self.scratch_blocked[base + t] |= 2u32;
                        if t_count < MAX_TARGETS {
                            self.scratch_paths[pbase + TARGET_OFFSET + 1 + t_count] = t as u32;
                            t_count += 1;
                        }
                    }
                }
                self.scratch_paths[pbase + TARGET_OFFSET] = t_count as u32;
                if t_count == 0 {
                    init_state[1 + f_idx] = 0;
                }
            }
            self.scratch_via.extend((0..cells).map(|c| if grid.via_blocked[c] { UNREACHED } else { grid.via_penalty_at(c).min(UNREACHED - 1) }));
            // Each node's whole price, as the CPU reads it: the stage's penalty and the regions' cost.
            if grid.penalty.is_empty() && grid.region_cost.is_empty() {
                self.scratch_penalty.resize(self.scratch_penalty.len() + per_field, 0);
            } else {
                self.scratch_penalty.extend((0..per_field).map(|n| grid.penalty_at(n)));
            }
            let mut has_source = false;
            for &s in sources {
                if !grid.blocked[s] {
                    has_source = true;
                    self.scratch_cost[base + s] = 0;
                    self.scratch_blocked[base + s] |= 4u32;
                    if with_targets && (self.scratch_blocked[base + s] & 2u32) != 0 {
                        init_state[1 + f_idx] = 0;
                    }
                }
            }
            if !has_source {
                init_state[1 + f_idx] = 0;
            } else if with_targets && init_state[1 + f_idx] != 0 && pocket_trapped(grid, sources, targets) {
                init_state[1 + f_idx] = 0;
                for &s in sources {
                    if !grid.blocked[s] {
                        self.scratch_blocked[base + s] &= !4u32;
                    }
                }
            }
        }

        let params = [
            first.nx as u32,
            first.ny as u32,
            first.layers.len() as u32,
            via_cost,
            u32::from(first.via.is_some()),
            fields as u32,
            u32::from(first.directional),
            0,
        ];
        let bufs = self.buffers.as_ref().unwrap();
        self.queue.write_buffer(&bufs.uniform, 0, as_bytes(&params));
        self.queue.write_buffer(&bufs.blocked, 0, as_bytes(&self.scratch_blocked));
        self.queue.write_buffer(&bufs.via_blocked, 0, as_bytes(&self.scratch_via));
        self.queue.write_buffer(&bufs.cost, 0, as_bytes(&self.scratch_cost));
        self.queue.write_buffer(&bufs.state, 0, as_bytes(&init_state));
        self.queue.write_buffer(&bufs.penalty, 0, as_bytes(&self.scratch_penalty));
        if with_targets {
            self.queue.write_buffer(&bufs.paths, 0, as_bytes(&self.scratch_paths));
        }

        let groups = (nodes as u32).div_ceil(WORKGROUP);
        let (gx, gy) = if groups <= MAX_GROUPS_X {
            (groups, 1)
        } else {
            (MAX_GROUPS_X, groups.div_ceil(MAX_GROUPS_X))
        };
        loop {
            self.queue.write_buffer(&bufs.state, 0, &[0u8; 4]);
            let mut encoder = self.device.create_command_encoder(&Default::default());
            {
                let mut pass = encoder.begin_compute_pass(&Default::default());
                pass.set_pipeline(&self.relax_pipeline);
                pass.set_bind_group(0, &bufs.bind_group, &[]);
                for _ in 0..DISPATCHES_PER_BATCH {
                    pass.dispatch_workgroups(gx, gy, 1);
                }
            }
            self.queue.submit([encoder.finish()]);
            self.dispatches += u64::from(DISPATCHES_PER_BATCH);
            let changed = Self::read_staging_u32(&self.device, &self.queue, &bufs.state, &bufs.state_staging, 4);
            match changed.first() {
                None => return Relaxed::Failed,
                Some(0) => return Relaxed::Done,
                Some(_) => {}
            }
            // The batch's grids share their stage's deadline; a relaxation cut short finds nothing.
            if jobs[0].0.deadline.expired() {
                return Relaxed::Cut;
            }
        }
    }

    /// Cost fields for several grids of one shape at once, one per net: the GPU only fills up
    /// with many fields in flight.
    pub fn solve_batch(&mut self, jobs: &[(&RouteGrid, &[usize])], via_cost: u32) -> Vec<Vec<u32>> {
        if jobs.is_empty() {
            return Vec::new();
        }
        let per_field = jobs[0].0.blocked.len();
        let nodes = per_field * jobs.len();
        let full_jobs: Vec<(&RouteGrid, &[usize], &[usize])> = jobs.iter().map(|&(g, s)| (g, s, &[][..])).collect();
        let cpu = || jobs.iter().map(|&(g, s)| CpuSolver.solve(g, s, via_cost)).collect();
        match self.run_relax(&full_jobs, via_cost, false) {
            Relaxed::Cut => return vec![vec![UNREACHED; per_field]; jobs.len()],
            Relaxed::Failed => return cpu(),
            Relaxed::Done => {}
        }
        let bufs = self.buffers.as_ref().unwrap();
        let values: Vec<u32> = read_back(&self.device, &self.queue, &bufs.cost, (nodes * 4) as u64)
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        if values.len() != nodes {
            return cpu();
        }
        values.chunks(per_field).map(<[u32]>::to_vec).collect()
    }

    /// Routes for several grids of one shape at once, pruning beyond target costs and tracing back
    /// on the GPU so only compact paths are read back to the host.
    pub fn route_batch(&mut self, jobs: &[(&RouteGrid, &[usize], &[usize])], via_cost: u32) -> Vec<Option<GridPath>> {
        if jobs.is_empty() {
            return Vec::new();
        }
        let (first, _, _) = jobs[0];
        let per_field = first.blocked.len();
        let cells = first.cell_count();
        let fields = jobs.len();
        let cpu = || jobs.iter().map(|&(g, s, t)| CpuSolver.route(g, s, t, via_cost)).collect();
        match self.run_relax(jobs, via_cost, true) {
            Relaxed::Cut => return vec![None; fields],
            Relaxed::Failed => return cpu(),
            Relaxed::Done => {}
        }
        let bufs = self.buffers.as_ref().unwrap();
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.trace_pipeline);
            pass.set_bind_group(0, &bufs.bind_group, &[]);
            pass.dispatch_workgroups((fields as u32).div_ceil(64), 1, 1);
        }
        self.queue.submit([encoder.finish()]);
        let path_words = Self::read_staging_u32(
            &self.device,
            &self.queue,
            &bufs.paths,
            &bufs.paths_staging,
            (fields * PATH_STRIDE * 4) as u64,
        );
        if path_words.len() != fields * PATH_STRIDE {
            return cpu();
        }
        let mut out = Vec::with_capacity(fields);
        let mut full_costs: Option<Vec<u32>> = None;
        for (f_idx, &(grid, _, targets)) in jobs.iter().enumerate() {
            let pbase = f_idx * PATH_STRIDE;
            let len = path_words[pbase] as usize;
            if len == 0 {
                out.push(None);
            } else if len == UNREACHED as usize || targets.len() > MAX_TARGETS {
                let costs = full_costs.get_or_insert_with(|| {
                    read_back(&self.device, &self.queue, &bufs.cost, (per_field * fields * 4) as u64)
                        .chunks_exact(4)
                        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                        .collect()
                });
                let base = f_idx * per_field;
                if costs.len() < base + per_field {
                    out.push(CpuSolver.route(grid, jobs[f_idx].1, targets, via_cost));
                    continue;
                }
                out.push(trace_back(grid, &costs[base..base + per_field], targets, via_cost));
            } else {
                let slice = &path_words[pbase + 1..pbase + 1 + len];
                let path: GridPath = slice
                    .iter()
                    .rev()
                    .map(|&idx| {
                        let index = idx as usize;
                        let cell = index % cells;
                        (index / cells, cell % grid.nx, cell / grid.nx)
                    })
                    .collect();
                out.push(Some(path));
            }
        }
        out
    }
}
