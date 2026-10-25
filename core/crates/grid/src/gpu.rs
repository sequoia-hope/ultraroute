//! The cost field on the GPU: `relax.wgsl` dispatched until nothing improves, in bounded
//! batches so no single submission holds the GPU long enough to stall the desktop.

use wgpu::util::DeviceExt;

use crate::grid::RouteGrid;
use crate::route::FieldSolver;
use crate::search::UNREACHED;

/// Relaxation sweeps per submission; the changed flag is read back between submissions.
const DISPATCHES_PER_BATCH: u32 = 64;
const WORKGROUP: u32 = 256;
const MAX_GROUPS_X: u32 = 65_535;

pub struct GpuSolver {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
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

impl GpuSolver {
    pub fn new() -> Option<GpuSolver> {
        let (device, queue, adapter_name) = open_device()?;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("relax"),
            source: wgpu::ShaderSource::Wgsl(include_str!("relax.wgsl").into()),
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("relax"),
            layout: None,
            module: &module,
            entry_point: Some("relax"),
            compilation_options: Default::default(),
            cache: None,
        });
        Some(GpuSolver {
            device,
            queue,
            pipeline,
            adapter_name,
            dispatches: 0,
        })
    }

    fn read_back(&self, buffer: &wgpu::Buffer, size: u64) -> Vec<u8> {
        read_back(&self.device, &self.queue, buffer, size)
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

    fn batch_size(&self) -> usize {
        BATCH
    }
}

impl GpuSolver {
    /// Cost fields for several grids of one shape at once, one per net: the GPU only fills up
    /// with many fields in flight.
    pub fn solve_batch(&mut self, jobs: &[(&RouteGrid, &[usize])], via_cost: u32) -> Vec<Vec<u32>> {
        let Some((first, _)) = jobs.first() else {
            return Vec::new();
        };
        let per_field = first.blocked.len();
        let cells = first.nx * first.ny;
        let nodes = per_field * jobs.len();
        let mut blocked = Vec::with_capacity(nodes * 4);
        let mut via_blocked = Vec::with_capacity(cells * jobs.len() * 4);
        let mut penalty = Vec::with_capacity(nodes * 4);
        let mut initial = Vec::with_capacity(nodes * 4);
        for (grid, sources) in jobs {
            assert_eq!(
                grid.blocked.len(),
                per_field,
                "batched grids share one shape"
            );
            blocked.extend(
                grid.blocked
                    .iter()
                    .flat_map(|&b| u32::from(b).to_le_bytes()),
            );
            via_blocked.extend(
                grid.via_blocked
                    .iter()
                    .flat_map(|&b| u32::from(b).to_le_bytes()),
            );
            if grid.penalty.is_empty() {
                penalty.extend(std::iter::repeat_n(0u8, per_field * 4));
            } else {
                penalty.extend(grid.penalty.iter().flat_map(|v| v.to_le_bytes()));
            }
            let mut field = vec![UNREACHED; per_field];
            for &s in *sources {
                if !grid.blocked[s] {
                    field[s] = 0;
                }
            }
            initial.extend(field.iter().flat_map(|v| v.to_le_bytes()));
        }
        let params = [
            first.nx as u32,
            first.ny as u32,
            first.layers.len() as u32,
            via_cost,
            u32::from(first.via.is_some()),
            jobs.len() as u32,
            0,
            0,
        ];
        let buffer = |label, contents: &[u8], usage| {
            self.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some(label),
                    contents,
                    usage,
                })
        };
        let storage = wgpu::BufferUsages::STORAGE;
        let uniform = buffer(
            "params",
            &params
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<u8>>(),
            wgpu::BufferUsages::UNIFORM,
        );
        let blocked = buffer("blocked", &blocked, storage);
        let via_blocked = buffer("via_blocked", &via_blocked, storage);
        let cost = buffer("cost", &initial, storage | wgpu::BufferUsages::COPY_SRC);
        let changed = buffer(
            "changed",
            &[0u8; 4],
            storage | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
        );
        let penalty = buffer("penalty", &penalty, storage);
        let resources = [
            uniform.as_entire_binding(),
            blocked.as_entire_binding(),
            via_blocked.as_entire_binding(),
            cost.as_entire_binding(),
            changed.as_entire_binding(),
            penalty.as_entire_binding(),
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
            label: Some("relax"),
            layout: &self.pipeline.get_bind_group_layout(0),
            entries: &entries,
        });
        let groups = (nodes as u32).div_ceil(WORKGROUP);
        let (gx, gy) = if groups <= MAX_GROUPS_X {
            (groups, 1)
        } else {
            (MAX_GROUPS_X, groups.div_ceil(MAX_GROUPS_X))
        };
        loop {
            self.queue.write_buffer(&changed, 0, &[0u8; 4]);
            let mut encoder = self.device.create_command_encoder(&Default::default());
            {
                let mut pass = encoder.begin_compute_pass(&Default::default());
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &bind_group, &[]);
                for _ in 0..DISPATCHES_PER_BATCH {
                    pass.dispatch_workgroups(gx, gy, 1);
                }
            }
            self.queue.submit([encoder.finish()]);
            self.dispatches += u64::from(DISPATCHES_PER_BATCH);
            if self.read_back(&changed, 4).iter().all(|b| *b == 0) {
                break;
            }
        }
        let values: Vec<u32> = self
            .read_back(&cost, (nodes * 4) as u64)
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        values.chunks(per_field).map(<[u32]>::to_vec).collect()
    }
}
