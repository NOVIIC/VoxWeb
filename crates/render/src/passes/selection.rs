//! 选中方块线框 Pass：用 line-list 拓扑画一个单位立方体的 12 条边，
//! 通过 uniform `block_origin` 平移到命中方块的世界坐标。
//!
//! Phase 3 与不透明 Pass 并存（在其后绘制），共享同一张 depth view 但不写深度，
//! 避免线框污染其它几何体的深度测试。半透明黑边，alpha blend 输出。

use std::num::NonZeroU64;

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

/// 与 opaque GlobalsUniform 同布局前缀，便于 shader 侧保持相同矩阵输入。
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Default)]
pub struct SelectionGlobals {
    pub view_proj: [[f32; 4]; 4],
    /// 选中体积的世界坐标 min 角；w=0 padding
    pub box_min: [f32; 4],
    /// 选中体积的尺寸；w=0 padding
    pub box_size: [f32; 4],
}

/// 单位立方体 12 条边的端点（24 个 vec3）。
/// 顶点为单位坐标 (0..=1)^3 的角点；shader 中 + block_origin 得到世界坐标。
#[rustfmt::skip]
const CUBE_EDGE_VERTICES: &[[f32; 3]] = &[
    // 底面 4 条边（y = 0）
    [0.0, 0.0, 0.0], [1.0, 0.0, 0.0],
    [1.0, 0.0, 0.0], [1.0, 0.0, 1.0],
    [1.0, 0.0, 1.0], [0.0, 0.0, 1.0],
    [0.0, 0.0, 1.0], [0.0, 0.0, 0.0],
    // 顶面 4 条边（y = 1）
    [0.0, 1.0, 0.0], [1.0, 1.0, 0.0],
    [1.0, 1.0, 0.0], [1.0, 1.0, 1.0],
    [1.0, 1.0, 1.0], [0.0, 1.0, 1.0],
    [0.0, 1.0, 1.0], [0.0, 1.0, 0.0],
    // 4 条竖边
    [0.0, 0.0, 0.0], [0.0, 1.0, 0.0],
    [1.0, 0.0, 0.0], [1.0, 1.0, 0.0],
    [1.0, 0.0, 1.0], [1.0, 1.0, 1.0],
    [0.0, 0.0, 1.0], [0.0, 1.0, 1.0],
];

/// 选中线框渲染 Pass。
pub struct SelectionPass {
    pub pipeline: wgpu::RenderPipeline,
    pub vertex_buffer: wgpu::Buffer,
    pub globals_buffer: wgpu::Buffer,
    pub globals_bind_group: wgpu::BindGroup,
}

impl SelectionPass {
    pub fn new(
        device: &wgpu::Device,
        color_format: wgpu::TextureFormat,
        depth_format: wgpu::TextureFormat,
    ) -> Self {
        // —— bind group layout：单一 uniform buffer ——
        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("selection.globals_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: NonZeroU64::new(
                        std::mem::size_of::<SelectionGlobals>() as u64
                    ),
                },
                count: None,
            }],
        });

        // —— shader ——
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("selection.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/selection.wgsl").into()),
        });

        // —— pipeline layout ——
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("selection.layout"),
            bind_group_layouts: &[Some(&globals_layout)],
            immediate_size: 0,
        });

        // —— vertex buffer：24 端点 × 12 字节 ——
        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("selection.vbuf"),
            contents: bytemuck::cast_slice(CUBE_EDGE_VERTICES),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let vertex_attrs: &[wgpu::VertexAttribute] = &[wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x3,
            offset: 0,
            shader_location: 0,
        }];
        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: 12,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: vertex_attrs,
        };

        // —— uniform buffer ——
        let globals_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("selection.globals"),
            size: std::mem::size_of::<SelectionGlobals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let globals_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("selection.globals_bg"),
            layout: &globals_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buffer.as_entire_binding(),
            }],
        });

        // —— pipeline：LineList 拓扑、半透明黑、不写深度 ——
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("selection.pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[vertex_layout],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::LineList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: depth_format,
                // 不写深度：线框只用来"覆盖渲染"，不污染主几何深度
                depth_write_enabled: Some(false),
                // LessEqual：与方块表面共面的线框依然可见
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: color_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview_mask: None,
            cache: None,
        });

        Self {
            pipeline,
            vertex_buffer,
            globals_buffer,
            globals_bind_group,
        }
    }

    /// 顶点数（恒为 24 = 12 条边 × 2 端点）。
    pub const VERTEX_COUNT: u32 = 24;
}
