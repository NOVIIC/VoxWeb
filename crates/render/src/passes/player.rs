//! 远端玩家与动态 FreeObject 渲染 Pass。
//!
//! - 玩家 / 硬材质 FreeObject：AABB 立方体实例
//! - 软材质颗粒（sand/dirt/grass）：单位球实例，避免下落时出现方块感
//!
//! 使用 instanced rendering：固定 vertex buffer + 动态 instance buffer。
//! 必须放在 OpaquePass 之后、SelectionPass 之前调用；LoadOp=Load, 共享 depth。

use std::num::NonZeroU64;

use bytemuck::{Pod, Zeroable};
use glam::Mat4;
use wgpu::util::DeviceExt;

/// 单位 box 顶点（36 个，12 个三角形）。
/// 实际尺寸由 instance.size 控制。玩家实例传入 0.6×1.8×0.6；
/// 硬材质 FreeObject sample 传入 1×1×1。
const CUBE_VERTICES: &[[f32; 3]] = &[
    // +Y (top, y=1) — CCW from +Y
    [0.0, 1.0, 1.0],
    [1.0, 1.0, 1.0],
    [1.0, 1.0, 0.0],
    [0.0, 1.0, 1.0],
    [1.0, 1.0, 0.0],
    [0.0, 1.0, 0.0],
    // -Y (bottom, y=0) — CCW from -Y
    [0.0, 0.0, 1.0],
    [1.0, 0.0, 0.0],
    [1.0, 0.0, 1.0],
    [0.0, 0.0, 1.0],
    [0.0, 0.0, 0.0],
    [1.0, 0.0, 0.0],
    // +Z (front)
    [0.0, 0.0, 1.0],
    [1.0, 0.0, 1.0],
    [1.0, 1.0, 1.0],
    [0.0, 0.0, 1.0],
    [1.0, 1.0, 1.0],
    [0.0, 1.0, 1.0],
    // -Z (back)
    [1.0, 0.0, 0.0],
    [0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0],
    [1.0, 0.0, 0.0],
    [0.0, 1.0, 0.0],
    [1.0, 1.0, 0.0],
    // +X (right)
    [1.0, 0.0, 1.0],
    [1.0, 0.0, 0.0],
    [1.0, 1.0, 0.0],
    [1.0, 0.0, 1.0],
    [1.0, 1.0, 0.0],
    [1.0, 1.0, 1.0],
    // -X (left)
    [0.0, 0.0, 0.0],
    [0.0, 0.0, 1.0],
    [0.0, 1.0, 1.0],
    [0.0, 0.0, 0.0],
    [0.0, 1.0, 1.0],
    [0.0, 1.0, 0.0],
];
const MAX_INSTANCE_COUNT: usize = 1024;

/// 每帧上传的 globals uniform（与 OpaquePass 同结构，方便复用）。
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Default)]
pub struct PlayerGlobals {
    pub view_proj: [[f32; 4]; 4],
    /// padding 64 字节
    pub _pad: [[f32; 4]; 3],
}

/// GPU 实例数据（std140 对齐，48 字节/实例）。
///
/// - 立方体：`position` 是最小角，`size` 是边长
/// - 球体：`position` 是球心，`size` 是直径（xyz 通常相等）
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
pub struct PlayerInstance {
    pub position: [f32; 3],
    pub _pad0: f32,
    pub size: [f32; 3],
    pub _pad_size: f32,
    pub color: [f32; 3],
    pub _pad1: f32,
}

/// 远端玩家 / FreeObject 渲染 Pass。
pub struct PlayerPass {
    pub box_pipeline: wgpu::RenderPipeline,
    pub sphere_pipeline: wgpu::RenderPipeline,
    pub box_vertex_buffer: wgpu::Buffer,
    pub sphere_vertex_buffer: wgpu::Buffer,
    pub sphere_vertex_count: u32,
    pub box_instance_buffer: wgpu::Buffer,
    pub sphere_instance_buffer: wgpu::Buffer,
    pub globals_buffer: wgpu::Buffer,
    pub globals_bind_group: wgpu::BindGroup,
    box_instance_count: u32,
    sphere_instance_count: u32,
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

        let box_vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("player.box_vbuf"),
            contents: bytemuck::cast_slice(CUBE_VERTICES),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let sphere_vertices = unit_sphere_triangles();
        let sphere_vertex_count = sphere_vertices.len() as u32;
        let sphere_vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("player.sphere_vbuf"),
            contents: bytemuck::cast_slice(&sphere_vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let box_instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("player.box_ibuf"),
            size: MAX_INSTANCE_COUNT as u64 * std::mem::size_of::<PlayerInstance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let sphere_instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("player.sphere_ibuf"),
            size: MAX_INSTANCE_COUNT as u64 * std::mem::size_of::<PlayerInstance>() as u64,
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
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x3,
                offset: 32,
                shader_location: 3,
            },
        ];
        let buffers = [
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
        ];

        let depth_stencil = wgpu::DepthStencilState {
            format: depth_format,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::Less),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        };
        let primitive = wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: Some(wgpu::Face::Back),
            polygon_mode: wgpu::PolygonMode::Fill,
            unclipped_depth: false,
            conservative: false,
        };
        let fragment = wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format: color_format,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        };

        let box_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("player.box_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_box"),
                buffers: &buffers,
                compilation_options: Default::default(),
            },
            primitive,
            depth_stencil: Some(depth_stencil.clone()),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(fragment.clone()),
            multiview_mask: None,
            cache: None,
        });

        let sphere_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("player.sphere_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_sphere"),
                buffers: &buffers,
                compilation_options: Default::default(),
            },
            primitive,
            depth_stencil: Some(depth_stencil),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(fragment),
            multiview_mask: None,
            cache: None,
        });

        Self {
            box_pipeline,
            sphere_pipeline,
            box_vertex_buffer,
            sphere_vertex_buffer,
            sphere_vertex_count,
            box_instance_buffer,
            sphere_instance_buffer,
            globals_buffer,
            globals_bind_group,
            box_instance_count: 0,
            sphere_instance_count: 0,
        }
    }

    /// 上传立方体实例（玩家 AABB / 硬 FreeObject）。
    pub fn upload_box_instances(&mut self, queue: &wgpu::Queue, instances: &[PlayerInstance]) {
        self.box_instance_count = write_instances(queue, &self.box_instance_buffer, instances);
    }

    /// 上传球体实例（软材质颗粒）。
    pub fn upload_sphere_instances(&mut self, queue: &wgpu::Queue, instances: &[PlayerInstance]) {
        self.sphere_instance_count =
            write_instances(queue, &self.sphere_instance_buffer, instances);
    }

    /// 兼容旧调用：全部当作立方体，并清空球体实例。
    pub fn upload_instances(&mut self, queue: &wgpu::Queue, instances: &[PlayerInstance]) {
        self.upload_box_instances(queue, instances);
        self.sphere_instance_count = 0;
    }

    /// 编码玩家 / FreeObject 渲染 Pass。LoadOp=Load（不清屏/不清深度）。
    pub fn render(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        color_view: &wgpu::TextureView,
        depth_view: &wgpu::TextureView,
    ) {
        if self.box_instance_count == 0 && self.sphere_instance_count == 0 {
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
        pass.set_bind_group(0, &self.globals_bind_group, &[]);

        if self.box_instance_count > 0 {
            pass.set_pipeline(&self.box_pipeline);
            pass.set_vertex_buffer(0, self.box_vertex_buffer.slice(..));
            pass.set_vertex_buffer(1, self.box_instance_buffer.slice(..));
            pass.draw(0..36, 0..self.box_instance_count);
        }
        if self.sphere_instance_count > 0 {
            pass.set_pipeline(&self.sphere_pipeline);
            pass.set_vertex_buffer(0, self.sphere_vertex_buffer.slice(..));
            pass.set_vertex_buffer(1, self.sphere_instance_buffer.slice(..));
            pass.draw(0..self.sphere_vertex_count, 0..self.sphere_instance_count);
        }
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

fn write_instances(
    queue: &wgpu::Queue,
    buffer: &wgpu::Buffer,
    instances: &[PlayerInstance],
) -> u32 {
    if instances.is_empty() {
        return 0;
    }
    let count = instances.len().min(MAX_INSTANCE_COUNT);
    queue.write_buffer(buffer, 0, bytemuck::cast_slice(&instances[..count]));
    count as u32
}

/// 由正八面体细分两次得到的单位球三角网（半径 0.5，中心在原点）。
fn unit_sphere_triangles() -> Vec<[f32; 3]> {
    let mut verts = vec![
        [0.0, 0.5, 0.0],
        [0.5, 0.0, 0.0],
        [0.0, 0.0, 0.5],
        [-0.5, 0.0, 0.0],
        [0.0, 0.0, -0.5],
        [0.0, -0.5, 0.0],
    ];
    let mut faces: Vec<[usize; 3]> = vec![
        [0, 1, 2],
        [0, 2, 3],
        [0, 3, 4],
        [0, 4, 1],
        [5, 2, 1],
        [5, 3, 2],
        [5, 4, 3],
        [5, 1, 4],
    ];

    for _ in 0..2 {
        let mut next_faces = Vec::with_capacity(faces.len() * 4);
        let mut midpoints = std::collections::HashMap::<(usize, usize), usize>::new();
        let mut midpoint = |verts: &mut Vec<[f32; 3]>, a: usize, b: usize| {
            let key = if a < b { (a, b) } else { (b, a) };
            if let Some(&idx) = midpoints.get(&key) {
                return idx;
            }
            let pa = verts[a];
            let pb = verts[b];
            let mut m = [
                (pa[0] + pb[0]) * 0.5,
                (pa[1] + pb[1]) * 0.5,
                (pa[2] + pb[2]) * 0.5,
            ];
            let len = (m[0] * m[0] + m[1] * m[1] + m[2] * m[2]).sqrt().max(1e-6);
            m[0] = m[0] / len * 0.5;
            m[1] = m[1] / len * 0.5;
            m[2] = m[2] / len * 0.5;
            let idx = verts.len();
            verts.push(m);
            midpoints.insert(key, idx);
            idx
        };
        for [a, b, c] in faces {
            let ab = midpoint(&mut verts, a, b);
            let bc = midpoint(&mut verts, b, c);
            let ca = midpoint(&mut verts, c, a);
            next_faces.push([a, ab, ca]);
            next_faces.push([b, bc, ab]);
            next_faces.push([c, ca, bc]);
            next_faces.push([ab, bc, ca]);
        }
        faces = next_faces;
    }

    let mut out = Vec::with_capacity(faces.len() * 3);
    for [a, b, c] in faces {
        out.push(verts[a]);
        out.push(verts[b]);
        out.push(verts[c]);
    }
    out
}

/// WGSL shader。立方体用法线查表；球体用顶点方向作法线。
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

// 36 个顶点按 6 面排列，每面 6 顶点（2 个三角形）。
// 0-5:+Y  6-11:-Y  12-17:+Z  18-23:-Z  24-29:+X  30-35:-X
fn face_normal(vi: u32) -> vec3<f32> {
    switch vi / 6u {
        case 0u: { return vec3<f32>( 0.0,  1.0,  0.0); } // +Y
        case 1u: { return vec3<f32>( 0.0, -1.0,  0.0); } // -Y
        case 2u: { return vec3<f32>( 0.0,  0.0,  1.0); } // +Z
        case 3u: { return vec3<f32>( 0.0,  0.0, -1.0); } // -Z
        case 4u: { return vec3<f32>( 1.0,  0.0,  0.0); } // +X
        default: { return vec3<f32>(-1.0,  0.0,  0.0); } // -X
    }
}

@vertex
fn vs_box(
    @location(0) vert_pos: vec3<f32>,
    @location(1) inst_pos: vec3<f32>,
    @location(2) inst_size: vec3<f32>,
    @location(3) inst_color: vec3<f32>,
    @builtin(vertex_index) vi: u32,
) -> VertexOutput {
    var out: VertexOutput;
    let world_pos = inst_pos + vert_pos * inst_size;
    out.clip_position = globals.view_proj * vec4<f32>(world_pos, 1.0);
    out.world_normal = face_normal(vi);
    out.color = inst_color;
    return out;
}

@vertex
fn vs_sphere(
    @location(0) vert_pos: vec3<f32>,
    @location(1) inst_pos: vec3<f32>,
    @location(2) inst_size: vec3<f32>,
    @location(3) inst_color: vec3<f32>,
) -> VertexOutput {
    var out: VertexOutput;
    // vert_pos 已是半径 0.5 的单位球；乘 diameter(=size) 得到实际半径。
    let world_pos = inst_pos + vert_pos * inst_size;
    out.clip_position = globals.view_proj * vec4<f32>(world_pos, 1.0);
    out.world_normal = normalize(vert_pos);
    out.color = inst_color;
    return out;
}

// 简单 Lambert：固定方向光 light_dir + ambient 0.3 / diffuse 0.7
@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let light_dir = normalize(vec3<f32>(0.6, 0.8, 0.2));
    let n = normalize(in.world_normal);
    let ndotl = max(dot(n, light_dir), 0.0);
    let lit = 0.35 + 0.65 * ndotl;
    return vec4<f32>(in.color * lit, 1.0);
}
"#;

#[cfg(test)]
mod player_tests {
    use super::*;

    #[test]
    fn cube_vertices_count_36() {
        assert_eq!(CUBE_VERTICES.len(), 36);
    }

    #[test]
    fn sphere_mesh_is_closed_and_centered() {
        let tris = unit_sphere_triangles();
        assert!(tris.len() >= 3 * 32);
        assert_eq!(tris.len() % 3, 0);
        let mut min = [f32::INFINITY; 3];
        let mut max = [f32::NEG_INFINITY; 3];
        for v in &tris {
            for i in 0..3 {
                min[i] = min[i].min(v[i]);
                max[i] = max[i].max(v[i]);
            }
        }
        for i in 0..3 {
            assert!((min[i] + 0.5).abs() < 0.02, "min[{i}]={}", min[i]);
            assert!((max[i] - 0.5).abs() < 0.02, "max[{i}]={}", max[i]);
        }
    }

    #[test]
    fn player_instance_layout_is_48_bytes() {
        assert_eq!(std::mem::size_of::<PlayerInstance>(), 48);
    }
}
