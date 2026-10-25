//! The cost field by GAMER-style directional sweeps (`sweep.wgsl`): rounds of eight line scans
//! and a via pass, submitted a few rounds at a time with the changed flag read back between
//! submissions.

use wgpu::util::DeviceExt;

use crate::gpu::{open_device, read_back};
use crate::grid::RouteGrid;
use crate::route::FieldSolver;
use crate::search::UNREACHED;

const ROUNDS_PER_BATCH: u32 = 4;
const LINE_GROUP: u32 = 64;
const CELL_GROUP: u32 = 256;
const MAX_GROUPS_X: u32 = 65_535;

pub struct GpuSweepSolver {
    device: wgpu::Device,
    queue: wgpu::Queue,
    layout: wgpu::BindGroupLayout,
    sweep: wgpu::ComputePipeline,
    vias: wgpu::ComputePipeline,
    pub adapter_name: String,
    pub rounds: u64,
}

impl GpuSweepSolver {
    pub fn new() -> Option<GpuSweepSolver> {
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
            label: Some("sweep"),
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
                storage(1, true),
                storage(2, true),
                storage(3, false),
                storage(4, false),
                storage(5, true),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sweep"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("sweep"),
            source: wgpu::ShaderSource::Wgsl(include_str!("sweep.wgsl").into()),
        });
        let pipeline = |entry: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&pipeline_layout),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let (sweep, vias) = (pipeline("sweep"), pipeline("vias"));
        Some(GpuSweepSolver {
            device,
            queue,
            layout,
            sweep,
            vias,
            adapter_name,
            rounds: 0,
        })
    }
}

impl FieldSolver for GpuSweepSolver {
    fn solve(&mut self, grid: &RouteGrid, sources: &[usize], via_cost: u32) -> Vec<u32> {
        let nodes = grid.blocked.len();
        let words = |values: &mut dyn Iterator<Item = u32>| -> Vec<u8> {
            values.flat_map(u32::to_le_bytes).collect()
        };
        let mut initial = vec![UNREACHED; nodes];
        for &s in sources {
            if !grid.blocked[s] {
                initial[s] = 0;
            }
        }
        let buffer = |label, contents: &[u8], usage| {
            self.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some(label),
                    contents,
                    usage,
                })
        };
        let storage = wgpu::BufferUsages::STORAGE;
        let blocked = buffer(
            "blocked",
            &words(&mut grid.blocked.iter().map(|&b| u32::from(b))),
            storage,
        );
        let via_blocked = buffer(
            "via_blocked",
            &words(&mut grid.via_blocked.iter().map(|&b| u32::from(b))),
            storage,
        );
        let cost = buffer(
            "cost",
            &words(&mut initial.into_iter()),
            storage | wgpu::BufferUsages::COPY_SRC,
        );
        let changed = buffer(
            "changed",
            &[0u8; 4],
            storage | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
        );
        let penalty = if grid.penalty.is_empty() {
            buffer("penalty", &vec![0u8; nodes * 4], storage)
        } else {
            buffer(
                "penalty",
                &words(&mut grid.penalty.iter().copied()),
                storage,
            )
        };
        let params = |direction: u32| {
            let values = [
                grid.nx as u32,
                grid.ny as u32,
                grid.layers.len() as u32,
                via_cost,
                u32::from(grid.via.is_some()),
                direction,
                0,
                0,
            ];
            buffer(
                "params",
                &words(&mut values.into_iter()),
                wgpu::BufferUsages::UNIFORM,
            )
        };
        let groups: Vec<wgpu::BindGroup> = (0..8)
            .map(|direction| {
                let uniform = params(direction);
                let entries = [
                    uniform.as_entire_binding(),
                    blocked.as_entire_binding(),
                    via_blocked.as_entire_binding(),
                    cost.as_entire_binding(),
                    changed.as_entire_binding(),
                    penalty.as_entire_binding(),
                ];
                let entries: Vec<wgpu::BindGroupEntry> = entries
                    .into_iter()
                    .enumerate()
                    .map(|(binding, resource)| wgpu::BindGroupEntry {
                        binding: binding as u32,
                        resource,
                    })
                    .collect();
                self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("sweep"),
                    layout: &self.layout,
                    entries: &entries,
                })
            })
            .collect();
        let layers = grid.layers.len() as u32;
        let lines = |direction: u32| match direction {
            0 | 1 => grid.ny as u32,
            2 | 3 => grid.nx as u32,
            _ => (grid.nx + grid.ny - 1) as u32,
        };
        let cells = (grid.nx * grid.ny) as u32;
        let cell_groups = cells.div_ceil(CELL_GROUP);
        let (vx, vy) = if cell_groups <= MAX_GROUPS_X {
            (cell_groups, 1)
        } else {
            (MAX_GROUPS_X, cell_groups.div_ceil(MAX_GROUPS_X))
        };
        loop {
            self.queue.write_buffer(&changed, 0, &[0u8; 4]);
            let mut encoder = self.device.create_command_encoder(&Default::default());
            {
                let mut pass = encoder.begin_compute_pass(&Default::default());
                for _ in 0..ROUNDS_PER_BATCH {
                    pass.set_pipeline(&self.sweep);
                    for direction in 0..8 {
                        pass.set_bind_group(0, &groups[direction as usize], &[]);
                        pass.dispatch_workgroups(lines(direction).div_ceil(LINE_GROUP), layers, 1);
                    }
                    pass.set_pipeline(&self.vias);
                    pass.set_bind_group(0, &groups[0], &[]);
                    pass.dispatch_workgroups(vx, vy, 1);
                }
            }
            self.queue.submit([encoder.finish()]);
            self.rounds += u64::from(ROUNDS_PER_BATCH);
            if read_back(&self.device, &self.queue, &changed, 4)
                .iter()
                .all(|b| *b == 0)
            {
                break;
            }
        }
        read_back(&self.device, &self.queue, &cost, (nodes * 4) as u64)
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }
}
