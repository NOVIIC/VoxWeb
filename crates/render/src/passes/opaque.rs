//! 不透明方块 Pass：渲染实体方块。
//!
//! Phase 1：单 Pass 渲染管线，从 `chunk.wgsl` 加载 vs/fs。
//! 不参与 `RenderGraph` 调度（Phase 8 再接入），调用者直接持有并在主循环里 draw。

use std::num::NonZeroU64;

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

use crate::vertex::{PackedVertex, vertex_buffer_layout};

/// 每帧上传的全局 uniform：view-projection 矩阵 + 当前绘制 chunk 的 origin。
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Default)]
pub struct GlobalsUniform {
    pub view_proj: [[f32; 4]; 4],
    /// xyz = chunk_origin (世界坐标，单位：方块)，w = 0
    pub chunk_origin: [f32; 4],
}

/// 一个 Chunk 的 GPU 网格资源：顶点缓冲 + 顶点计数。
pub struct ChunkMeshGpu {
    pub vertex_buffer: wgpu::Buffer,
    pub vertex_count: u32,
}

/// 不透明方块渲染 Pass（Pipeline + uniform 资源）。
pub struct OpaquePass {
    pub pipeline: wgpu::RenderPipeline,
    pub globals_layout: wgpu::BindGroupLayout,
    pub globals_buffer: wgpu::Buffer,
    pub globals_bind_group: wgpu::BindGroup,
}

impl OpaquePass {
    pub fn new(
        device: &wgpu::Device,
        color_format: wgpu::TextureFormat,
        depth_format: wgpu::TextureFormat,
    ) -> Self {
        // —— uniform buffer + bind group ——
        let globals_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("opaque.globals"),
            size: std::mem::size_of::<GlobalsUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("opaque.globals_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: NonZeroU64::new(std::mem::size_of::<GlobalsUniform>() as u64),
                },
                count: None,
            }],
        });

        let globals_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("opaque.globals_bg"),
            layout: &globals_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buffer.as_entire_binding(),
            }],
        });

        // —— shader ——
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("chunk.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/chunk.wgsl").into()),
        });

        // —— pipeline layout ——
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("opaque.layout"),
            bind_group_layouts: &[Some(&globals_layout)],
            immediate_size: 0,
        });

        // —— render pipeline ——
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("opaque.pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[vertex_buffer_layout()],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                // Phase 1 暂不开 face culling，避免 winding 调试干扰
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: depth_format,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: color_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview_mask: None,
            cache: None,
        });

        Self {
            pipeline,
            globals_layout,
            globals_buffer,
            globals_bind_group,
        }
    }

    /// 把当前帧的全局 uniform 写到 GPU。
    pub fn upload_globals(&self, queue: &wgpu::Queue, globals: &GlobalsUniform) {
        queue.write_buffer(&self.globals_buffer, 0, bytemuck::bytes_of(globals));
    }

    /// 把 CPU 顶点列表打包成 GPU 资源。
    pub fn upload_chunk_mesh(
        &self,
        device: &wgpu::Device,
        vertices: &[PackedVertex],
    ) -> Option<ChunkMeshGpu> {
        if vertices.is_empty() {
            return None;
        }
        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("chunk.vbuf"),
            contents: bytemuck::cast_slice(vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        Some(ChunkMeshGpu {
            vertex_buffer,
            vertex_count: vertices.len() as u32,
        })
    }
}
