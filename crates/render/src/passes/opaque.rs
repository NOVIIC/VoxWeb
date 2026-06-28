//! 不透明方块 Pass：渲染实体方块。
//!
//! Phase 1：单 Pass 渲染管线，从 `chunk.wgsl` 加载 vs/fs。
//! 调用者直接持有并在主循环里按固定 pass 顺序 draw。

use std::num::NonZeroU64;

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

use voxweb_core::Aabb;

use crate::vertex::{
    PackedVertex, SmoothVertex, smooth_vertex_buffer_layout, vertex_buffer_layout,
};

/// 每个 chunk draw call 上传的 uniform。
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Default)]
pub struct GlobalsUniform {
    pub view_proj: [[f32; 4]; 4],
    /// xyz = chunk_origin (世界坐标，单位：方块)，w = 0
    pub chunk_origin: [f32; 4],
    /// xyz = camera position，w = 0
    pub camera_pos: [f32; 4],
    /// xyz = fog color，w = 0
    pub fog_color: [f32; 4],
    /// x = fog_start, y = fog_end, z = haze strength, w = time seconds
    pub fog_params: [f32; 4],
    /// xyz = normalized sun direction，w = 0
    pub sun_dir: [f32; 4],
}

/// 一个 Chunk 的 GPU 网格资源：顶点缓冲 + 顶点计数 + 该 chunk 专属的 globals uniform。
///
/// 关键：globals buffer 必须**按 chunk 分别持有**，否则在单次 submit 中多次 `queue.write_buffer`
/// 同一 buffer 会被合并到最后一次写入，导致所有 chunk 用同一个 chunk_origin 渲染（视觉上"全叠在一起"）。
pub struct ChunkMeshGpu {
    pub vertex_buffer: Option<wgpu::Buffer>,
    pub index_buffer: Option<wgpu::Buffer>,
    pub vertex_count: u32,
    pub index_count: u32,
    pub smooth_vertex_buffer: Option<wgpu::Buffer>,
    pub smooth_index_buffer: Option<wgpu::Buffer>,
    pub smooth_vertex_count: u32,
    pub smooth_index_count: u32,
    pub bounds: Aabb,
    pub globals_buffer: wgpu::Buffer,
    pub globals_bind_group: wgpu::BindGroup,
}

/// 不透明方块渲染 Pass（Pipeline + bind group layout）。
pub struct OpaquePass {
    pub pipeline: wgpu::RenderPipeline,
    pub smooth_pipeline: wgpu::RenderPipeline,
    pub depth_pipeline: wgpu::RenderPipeline,
    pub smooth_depth_pipeline: wgpu::RenderPipeline,
    pub globals_layout: wgpu::BindGroupLayout,
}

impl OpaquePass {
    pub fn new(
        device: &wgpu::Device,
        color_format: wgpu::TextureFormat,
        depth_format: wgpu::TextureFormat,
        atlas_layout: &wgpu::BindGroupLayout,
    ) -> Self {
        // —— bind group layout（pipeline 创建需要，但 globals buffer 由每个 ChunkMeshGpu 自带）——
        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("opaque.globals_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: NonZeroU64::new(std::mem::size_of::<GlobalsUniform>() as u64),
                },
                count: None,
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
            bind_group_layouts: &[Some(&globals_layout), Some(atlas_layout)],
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
        let smooth_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("opaque.smooth_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_smooth"),
                buffers: &[smooth_vertex_buffer_layout()],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
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
        let depth_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("opaque.depth_prepass_pipeline"),
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
            fragment: None,
            multiview_mask: None,
            cache: None,
        });
        let smooth_depth_pipeline =
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("opaque.smooth_depth_prepass_pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_smooth"),
                    buffers: &[smooth_vertex_buffer_layout()],
                    compilation_options: Default::default(),
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    strip_index_format: None,
                    front_face: wgpu::FrontFace::Ccw,
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
                fragment: None,
                multiview_mask: None,
                cache: None,
            });

        Self {
            pipeline,
            smooth_pipeline,
            depth_pipeline,
            smooth_depth_pipeline,
            globals_layout,
        }
    }

    /// 把 CPU 顶点列表打包成 GPU 资源（含该 chunk 专属的 globals buffer + bind group）。
    pub fn upload_chunk_mesh(
        &self,
        device: &wgpu::Device,
        vertices: &[PackedVertex],
        indices: &[u32],
        smooth_vertices: &[SmoothVertex],
        smooth_indices: &[u32],
        bounds: Aabb,
    ) -> Option<ChunkMeshGpu> {
        let has_blocky = !vertices.is_empty() && !indices.is_empty();
        let has_smooth = !smooth_vertices.is_empty() && !smooth_indices.is_empty();
        if !has_blocky && !has_smooth {
            return None;
        }
        let vertex_buffer = has_blocky.then(|| {
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("chunk.vbuf"),
                contents: bytemuck::cast_slice(vertices),
                usage: wgpu::BufferUsages::VERTEX,
            })
        });
        let index_buffer = has_blocky.then(|| {
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("chunk.ibuf"),
                contents: bytemuck::cast_slice(indices),
                usage: wgpu::BufferUsages::INDEX,
            })
        });
        let smooth_vertex_buffer = has_smooth.then(|| {
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("chunk.smooth_vbuf"),
                contents: bytemuck::cast_slice(smooth_vertices),
                usage: wgpu::BufferUsages::VERTEX,
            })
        });
        let smooth_index_buffer = has_smooth.then(|| {
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("chunk.smooth_ibuf"),
                contents: bytemuck::cast_slice(smooth_indices),
                usage: wgpu::BufferUsages::INDEX,
            })
        });
        let globals_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("chunk.globals"),
            size: std::mem::size_of::<GlobalsUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("chunk.globals_bg"),
            layout: &self.globals_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buffer.as_entire_binding(),
            }],
        });
        Some(ChunkMeshGpu {
            vertex_buffer,
            index_buffer,
            vertex_count: vertices.len() as u32,
            index_count: indices.len() as u32,
            smooth_vertex_buffer,
            smooth_index_buffer,
            smooth_vertex_count: smooth_vertices.len() as u32,
            smooth_index_count: smooth_indices.len() as u32,
            bounds,
            globals_buffer,
            globals_bind_group,
        })
    }
}
