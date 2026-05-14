//! 远端玩家实体渲染 Pass（Phase 5）。
//!
//! 每个远端玩家是一个 AABB 形状的实心立方体（0.6×1.8×0.6），颜色按 entity_id 派生。
//! 使用 instanced rendering：一组固定 vertex buffer（36 verts 单位 cube）+ 动态 instance buffer。
//!
//! 必须放在 OpaquePass 之后、SelectionPass 之前调用；LoadOp=Load, 共享 depth。

use std::num::NonZeroU64;

use bytemuck::{Pod, Zeroable};
use glam::Mat4;
use wgpu::util::DeviceExt;

/// 玩家 AABB box 顶点（36 个，12 个三角形）。
/// 以玩家脚底 (0, 0, 0) 为原点，尺寸 0.6×1.8×0.6。
const CUBE_VERTICES: &[[f32; 3]] = &[
    // +Y (top, y=1.8)
    [-0.3, 1.8, -0.3],
    [0.3, 1.8, -0.3],
    [0.3, 1.8, 0.3],
    [-0.3, 1.8, -0.3],
    [0.3, 1.8, 0.3],
    [-0.3, 1.8, 0.3],
    // -Y (bottom, y=0)
    [-0.3, 0.0, 0.3],
    [0.3, 0.0, 0.3],
    [0.3, 0.0, -0.3],
    [-0.3, 0.0, 0.3],
    [0.3, 0.0, -0.3],
    [-0.3, 0.0, -0.3],
    // +Z (front)
    [-0.3, 0.0, 0.3],
    [0.3, 0.0, 0.3],
    [0.3, 1.8, 0.3],
    [-0.3, 0.0, 0.3],
    [0.3, 1.8, 0.3],
    [-0.3, 1.8, 0.3],
    // -Z (back)
    [0.3, 0.0, -0.3],
    [-0.3, 0.0, -0.3],
    [-0.3, 1.8, -0.3],
    [0.3, 0.0, -0.3],
    [-0.3, 1.8, -0.3],
    [0.3, 1.8, -0.3],
    // +X (right)
    [0.3, 0.0, 0.3],
    [0.3, 0.0, -0.3],
    [0.3, 1.8, -0.3],
    [0.3, 0.0, 0.3],
    [0.3, 1.8, -0.3],
    [0.3, 1.8, 0.3],
    // -X (left)
    [-0.3, 0.0, -0.3],
    [-0.3, 0.0, 0.3],
    [-0.3, 1.8, 0.3],
    [-0.3, 0.0, -0.3],
    [-0.3, 1.8, 0.3],
    [-0.3, 1.8, -0.3],
];

/// 每帧上传的 globals uniform（与 OpaquePass 同结构，方便复用）。
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Default)]
pub struct PlayerGlobals {
    pub view_proj: [[f32; 4]; 4],
    /// padding 64 字节
    pub _pad: [[f32; 4]; 3],
}

/// GPU 实例数据（std140 对齐，32 字节/实例）。
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
pub struct PlayerInstance {
    pub position: [f32; 3],
    pub _pad0: f32,
    pub color: [f32; 3],
    pub _pad1: f32,
}

/// 远端玩家渲染 Pass。
pub struct PlayerPass {
    pub pipeline: wgpu::RenderPipeline,
    pub vertex_buffer: wgpu::Buffer,
    pub instance_buffer: wgpu::Buffer,
    pub globals_buffer: wgpu::Buffer,
    pub globals_bind_group: wgpu::BindGroup,
    instance_count: u32,
}

impl PlayerPass {
    pub fn new(
        device: &wgpu::Device,
        color_format: wgpu::TextureFormat,
        depth_format: wgpu::TextureFormat,
    ) -> Self {
        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("player.globals_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: NonZeroU64::new(std::mem::size_of::<PlayerGlobals>() as u64),
                },
                count: None,
            }],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("player.wgsl"),
            source: wgpu::ShaderSource::Wgsl(PLAYER_SHADER.into()),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("player.layout"),
            bind_group_layouts: &[Some(&globals_layout)],
            immediate_size: 0,
        });

        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("player.vbuf"),
            contents: bytemuck::cast_slice(CUBE_VERTICES),
            usage: wgpu::BufferUsages::VERTEX,
        });

        // 预留 16 个实例 × 32 字节
        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("player.ibuf"),
            size: 16 * std::mem::size_of::<PlayerInstance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let globals_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("player.gbuf"),
            size: std::mem::size_of::<PlayerGlobals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let globals_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("player.gbg"),
            layout: &globals_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buffer.as_entire_binding(),
            }],
        });

        let vertex_attrs: &[wgpu::VertexAttribute] = &[wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x3,
            offset: 0,
            shader_location: 0,
        }];
        let instance_attrs: &[wgpu::VertexAttribute] = &[
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x3,
                offset: 0,
                shader_location: 1,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x3,
                offset: 16,
                shader_location: 2,
            },
        ];

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("player.pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[
                    wgpu::VertexBufferLayout {
                        array_stride: 12,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: vertex_attrs,
                    },
                    wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<PlayerInstance>() as u64,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: instance_attrs,
                    },
                ],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: Some(wgpu::Face::Back),
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
                    blend: Some(wgpu::BlendState::REPLACE),
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
            instance_buffer,
            globals_buffer,
            globals_bind_group,
            instance_count: 0,
        }
    }

    /// 上传实例缓冲（每帧调用一次）。无玩家时 instance_count = 0，render 自动跳过。
    pub fn upload_instances(&mut self, queue: &wgpu::Queue, instances: &[PlayerInstance]) {
        if instances.is_empty() {
            self.instance_count = 0;
            return;
        }
        self.instance_count = instances.len() as u32;
        queue.write_buffer(&self.instance_buffer, 0, bytemuck::cast_slice(instances));
    }

    /// 编码玩家渲染 Pass。LoadOp=Load（不清屏/不清深度）。
    pub fn render(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        color_view: &wgpu::TextureView,
        depth_view: &wgpu::TextureView,
    ) {
        if self.instance_count == 0 {
            return;
        }
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("player_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: color_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.globals_bind_group, &[]);
        pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
        pass.set_vertex_buffer(1, self.instance_buffer.slice(..));
        pass.draw(0..36, 0..self.instance_count);
    }

    /// 上传 view_proj 到 globals uniform（每帧一次）。
    pub fn write_globals(&self, queue: &wgpu::Queue, view_proj: Mat4) {
        let g = PlayerGlobals {
            view_proj: view_proj.to_cols_array_2d(),
            _pad: [[0.0; 4]; 3],
        };
        queue.write_buffer(&self.globals_buffer, 0, bytemuck::bytes_of(&g));
    }
}

/// WGSL shader。
const PLAYER_SHADER: &str = r#"
struct Globals {
    view_proj: mat4x4<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) world_normal: vec3<f32>,
    @location(1) color: vec3<f32>,
};

// 用本顶点坐标推断 cube 的面法线。因为 cube 是轴对齐的，
// 坐标绝对值最大的轴对应法线方向。
fn infer_normal(v: vec3<f32>) -> vec3<f32> {
    let ax = abs(v);
    if ax.x >= ax.y && ax.x >= ax.z {
        return vec3<f32>(sign(v.x), 0.0, 0.0);
    }
    if ax.y >= ax.z {
        return vec3<f32>(0.0, sign(v.y), 0.0);
    }
    return vec3<f32>(0.0, 0.0, sign(v.z));
}

@vertex
fn vs_main(
    @location(0) vert_pos: vec3<f32>,
    @location(1) inst_pos: vec3<f32>,
    @location(2) inst_color: vec3<f32>,
) -> VertexOutput {
    var out: VertexOutput;
    let world_pos = vert_pos + inst_pos;
    out.clip_position = globals.view_proj * vec4<f32>(world_pos, 1.0);
    out.world_normal = infer_normal(vert_pos);
    out.color = inst_color;
    return out;
}

// 简单 Lambert：固定方向光 light_dir + ambient 0.3 / diffuse 0.7
@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let light_dir = normalize(vec3<f32>(0.6, 0.8, 0.2));
    let n = normalize(in.world_normal);
    let ndotl = max(dot(n, light_dir), 0.0);
    let lit = 0.3 + 0.7 * ndotl;
    return vec4<f32>(in.color * lit, 1.0);
}
"#;

#[cfg(test)]
mod player_tests {
    use super::*;

    #[test]
    fn player_instance_layout_is_32_bytes() {
        assert_eq!(std::mem::size_of::<PlayerInstance>(), 32);
    }

    #[test]
    fn cube_vertices_count_36() {
        assert_eq!(CUBE_VERTICES.len(), 36);
    }
}
