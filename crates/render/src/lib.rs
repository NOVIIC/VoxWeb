//! VoxWeb WebGPU 渲染层。
//!
//! Phase 1：单 Pass（OpaquePass）渲染体素网格 + 深度测试。
//! 客户端持有 `Renderer` 并在主循环里：
//!   1. 调 [`Renderer::resize`] 同步 surface 尺寸；
//!   2. 调 [`Renderer::upload_chunk_mesh`] 把 [`chunk_mesh::ChunkMeshCpu`] 写到 GPU；
//!   3. 调 [`Renderer::acquire_frame`] 取得本帧 surface texture；
//!   4. 在自己的 CommandEncoder 上调 [`Renderer::render_world`] 编码方块绘制；
//!   5. 接着客户端可继续编码 egui Pass（不归本 crate 管）；
//!   6. submit + present。

pub mod chunk_mesh;
pub mod device;
pub mod graph;
pub mod passes;
pub mod texture;
pub mod vertex;

use std::collections::HashMap;
use std::sync::Arc;

use glam::{Mat4, Vec3};
use voxweb_core::ChunkPos;
use voxweb_core::chunk::Position;

use crate::chunk_mesh::ChunkMeshCpu;
use crate::passes::opaque::{ChunkMeshGpu, GlobalsUniform, OpaquePass};
use crate::passes::selection::{SelectionGlobals, SelectionPass};

/// 深度纹理格式（与 OpaquePass 中的 DepthStencilState 对齐）。
pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth24Plus;

/// 顶层渲染器。持有 wgpu 设备、Surface、Pipeline、所有 Chunk 网格 GPU 资源。
pub struct Renderer {
    pub device: Arc<wgpu::Device>,
    pub queue: Arc<wgpu::Queue>,
    pub surface: wgpu::Surface<'static>,
    pub surface_format: wgpu::TextureFormat,

    width: u32,
    height: u32,

    depth_texture: wgpu::Texture,
    depth_view: wgpu::TextureView,

    opaque_pass: OpaquePass,
    selection_pass: SelectionPass,
    chunk_meshes: HashMap<ChunkPos, ChunkMeshGpu>,
}

impl Renderer {
    /// 异步初始化：协商 wgpu Adapter/Device，配置 Surface，构建 OpaquePass。
    pub async fn new(canvas: &web_sys::HtmlCanvasElement) -> Result<Self, String> {
        let ctx = device::init_device(canvas).await?;
        let (w, h) = canvas_size(canvas);
        device::configure_surface(&ctx.surface, &ctx.device, ctx.surface_format, w, h);

        let (depth_texture, depth_view) = create_depth(&ctx.device, w, h);
        let opaque_pass = OpaquePass::new(&ctx.device, ctx.surface_format, DEPTH_FORMAT);
        let selection_pass = SelectionPass::new(&ctx.device, ctx.surface_format, DEPTH_FORMAT);

        Ok(Self {
            device: ctx.device,
            queue: ctx.queue,
            surface: ctx.surface,
            surface_format: ctx.surface_format,
            width: w,
            height: h,
            depth_texture,
            depth_view,
            opaque_pass,
            selection_pass,
            chunk_meshes: HashMap::new(),
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// canvas 尺寸变化时调用：重配 Surface + 重建 depth texture。
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        if self.width == width && self.height == height {
            return;
        }
        self.width = width;
        self.height = height;
        device::configure_surface(
            &self.surface,
            &self.device,
            self.surface_format,
            width,
            height,
        );
        let (tex, view) = create_depth(&self.device, width, height);
        self.depth_texture = tex;
        self.depth_view = view;
    }

    /// 把一个 Chunk 的 CPU 网格上传到 GPU。空网格会显式从缓存中删除。
    pub fn upload_chunk_mesh(&mut self, pos: ChunkPos, mesh: &ChunkMeshCpu) {
        match self
            .opaque_pass
            .upload_chunk_mesh(&self.device, &mesh.vertices)
        {
            Some(gpu) => {
                self.chunk_meshes.insert(pos, gpu);
            }
            None => {
                self.chunk_meshes.remove(&pos);
            }
        }
    }

    /// 卸载某个 Chunk 的 GPU 网格（玩家走远时调用）。
    pub fn drop_chunk_mesh(&mut self, pos: ChunkPos) {
        self.chunk_meshes.remove(&pos);
    }

    /// 查询某个 chunk 是否已有 GPU mesh。
    /// Phase 2 ChunkLoader 用其决定邻居是否需重网格化（跨区块剔除生效条件）。
    pub fn has_chunk_mesh(&self, pos: ChunkPos) -> bool {
        self.chunk_meshes.contains_key(&pos)
    }

    /// 已上传的 chunk 数量。
    pub fn loaded_chunk_count(&self) -> usize {
        self.chunk_meshes.len()
    }

    /// 取得本帧 surface texture。失败（Outdated/Lost）时自动重配 Surface 并返回 None。
    pub fn acquire_frame(&mut self) -> Option<wgpu::SurfaceTexture> {
        match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => Some(t),
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                device::configure_surface(
                    &self.surface,
                    &self.device,
                    self.surface_format,
                    self.width,
                    self.height,
                );
                None
            }
            wgpu::CurrentSurfaceTexture::Timeout
            | wgpu::CurrentSurfaceTexture::Occluded
            | wgpu::CurrentSurfaceTexture::Validation => None,
        }
    }

    /// 用于 egui Pass 在 world Pass 之后载入到同一 depth attachment（Phase 1 不需要）。
    pub fn depth_view(&self) -> &wgpu::TextureView {
        &self.depth_view
    }

    /// 渲染世界（OpaquePass）。
    /// 该 Pass 会清屏 + 清深度，将所有已上传的 chunk 网格绘制到 `color_view`。
    /// 调用者负责管理 `encoder` 生命周期；egui Pass 由客户端在此之后继续编码。
    pub fn render_world(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        color_view: &wgpu::TextureView,
        view_proj: Mat4,
        clear_color: [f64; 4],
    ) {
        let pass_label = "opaque_pass";

        // 每个 chunk 上传不同的 chunk_origin → 多次 draw 之间需要刷新 globals
        // Phase 1：chunk 数量极少（演示用 1 个），简单地循环上传 + 单 draw
        if self.chunk_meshes.is_empty() {
            // 仍然要"清屏 + 清深度"，否则 surface 上会留旧帧像素
            let _ = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some(pass_label),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: color_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: clear_color[0],
                            g: clear_color[1],
                            b: clear_color[2],
                            a: clear_color[3],
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            return;
        }

        // 取出 (pos, gpu) 列表的"键引用"：原 HashMap 借用必须早于 begin_render_pass 之前结束
        // —— 为简化生命周期，先收集到 Vec<(ChunkPos, &ChunkMeshGpu)>
        let entries: Vec<(ChunkPos, &ChunkMeshGpu)> =
            self.chunk_meshes.iter().map(|(p, m)| (*p, m)).collect();

        // 每个 chunk 自带一个 globals uniform buffer，避免 queue.write_buffer 在 submit 前
        // 被合并到最后一次写入（那会让所有 chunk 用同一个 chunk_origin → 视觉上"全叠在一起"）。
        // 仍然每个 chunk 一个独立 RenderPass：第一个清屏，后续 Load。

        for (i, (pos, mesh)) in entries.iter().enumerate() {
            let chunk_origin_world = Vec3::new(
                pos.x as f32 * voxweb_core::CHUNK_X as f32,
                0.0,
                pos.z as f32 * voxweb_core::CHUNK_Z as f32,
            );
            let globals = GlobalsUniform {
                view_proj: view_proj.to_cols_array_2d(),
                chunk_origin: [
                    chunk_origin_world.x,
                    chunk_origin_world.y,
                    chunk_origin_world.z,
                    0.0,
                ],
            };
            // 写到该 chunk 自己的 uniform buffer
            self.queue
                .write_buffer(&mesh.globals_buffer, 0, bytemuck::bytes_of(&globals));

            let load_op = if i == 0 {
                wgpu::LoadOp::Clear(wgpu::Color {
                    r: clear_color[0],
                    g: clear_color[1],
                    b: clear_color[2],
                    a: clear_color[3],
                })
            } else {
                wgpu::LoadOp::Load
            };
            let depth_load = if i == 0 {
                wgpu::LoadOp::Clear(1.0)
            } else {
                wgpu::LoadOp::Load
            };

            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some(pass_label),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: color_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: load_op,
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: depth_load,
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.opaque_pass.pipeline);
            pass.set_bind_group(0, &mesh.globals_bind_group, &[]);
            pass.set_vertex_buffer(0, mesh.vertex_buffer.slice(..));
            pass.draw(0..mesh.vertex_count, 0..1);
        }
    }

    /// 渲染选中方块的线框。`block_pos = None` 时跳过（玩家未瞄准任何方块）。
    /// 必须在 `render_world` 之后调用，共享同一份 depth view 但不写深度。
    pub fn render_selection(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        color_view: &wgpu::TextureView,
        view_proj: Mat4,
        block_pos: Option<Position>,
    ) {
        let Some(pos) = block_pos else {
            return;
        };
        let globals = SelectionGlobals {
            view_proj: view_proj.to_cols_array_2d(),
            block_origin: [pos.x as f32, pos.y as f32, pos.z as f32, 0.0],
        };
        self.queue.write_buffer(
            &self.selection_pass.globals_buffer,
            0,
            bytemuck::bytes_of(&globals),
        );

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("selection_pass"),
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
                view: &self.depth_view,
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
        pass.set_pipeline(&self.selection_pass.pipeline);
        pass.set_bind_group(0, &self.selection_pass.globals_bind_group, &[]);
        pass.set_vertex_buffer(0, self.selection_pass.vertex_buffer.slice(..));
        pass.draw(0..SelectionPass::VERTEX_COUNT, 0..1);
    }
}

/// 读取 canvas 的 client width/height，作为 surface 的 logical pixel 尺寸。
fn canvas_size(canvas: &web_sys::HtmlCanvasElement) -> (u32, u32) {
    let w = (canvas.client_width().max(1)) as u32;
    let h = (canvas.client_height().max(1)) as u32;
    (w, h)
}

/// 创建匹配指定尺寸的 Depth24Plus 纹理 + view。
fn create_depth(
    device: &wgpu::Device,
    width: u32,
    height: u32,
) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("renderer.depth"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}
