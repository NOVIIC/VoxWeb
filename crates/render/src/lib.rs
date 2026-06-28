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
mod frustum;
pub mod passes;
pub mod texture;
pub mod vertex;

use std::collections::HashMap;
use std::sync::Arc;

use glam::{Mat4, Vec3};
use voxweb_core::{Aabb, ChunkPos};

use crate::chunk_mesh::ChunkMeshCpu;
use crate::frustum::Frustum;
use crate::passes::opaque::{ChunkMeshGpu, GlobalsUniform, OpaquePass};
use crate::passes::player::{PlayerInstance, PlayerPass};
use crate::passes::selection::{SelectionGlobals, SelectionPass};
use crate::passes::skybox::{SkyboxGlobals, SkyboxPass};
use crate::passes::transparent::{TransparentGlobals, TransparentMeshGpu, TransparentPass};
use crate::texture::TextureAtlas;

/// 深度纹理格式（与 OpaquePass 中的 DepthStencilState 对齐）。
pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth24Plus;

/// 本帧世界渲染统计。CPU 侧编码时统计，用于 Phase 7 HUD。
#[derive(Clone, Copy, Debug, Default)]
pub struct WorldRenderStats {
    pub total_chunks: usize,
    pub visible_chunks: usize,
    pub culled_chunks: usize,
    pub drawn_vertices: u32,
    pub drawn_indices: u32,
}

/// 一帧内所有视觉 pass 共享的自然光照 / 雾化参数。
#[derive(Clone, Copy, Debug)]
pub struct VisualFrame {
    pub camera_pos: Vec3,
    pub time_seconds: f32,
    pub sun_dir: Vec3,
    pub fog_color: Vec3,
    pub fog_start: f32,
    pub fog_end: f32,
    pub haze_strength: f32,
}

impl VisualFrame {
    pub fn new(camera_pos: Vec3, time_seconds: f32) -> Self {
        Self {
            camera_pos,
            time_seconds,
            sun_dir: Vec3::new(0.42, 0.82, 0.28).normalize(),
            fog_color: Vec3::new(0.72, 0.82, 0.86),
            fog_start: 70.0,
            fog_end: 210.0,
            haze_strength: 0.72,
        }
    }
}

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
    skybox_pass: SkyboxPass,
    transparent_pass: TransparentPass,
    player_pass: PlayerPass,
    selection_pass: SelectionPass,
    texture_atlas: TextureAtlas,
    chunk_meshes: HashMap<ChunkPos, ChunkMeshGpu>,
    transparent_meshes: HashMap<ChunkPos, TransparentMeshGpu>,
}

impl Renderer {
    /// 异步初始化：协商 wgpu Adapter/Device，配置 Surface，构建 OpaquePass。
    pub async fn new(canvas: &web_sys::HtmlCanvasElement) -> Result<Self, String> {
        let ctx = device::init_device(canvas).await?;
        let (w, h) = canvas_size(canvas);
        device::configure_surface(&ctx.surface, &ctx.device, ctx.surface_format, w, h);

        let (depth_texture, depth_view) = create_depth(&ctx.device, w, h);
        let texture_atlas = TextureAtlas::new(&ctx.device, &ctx.queue);
        let opaque_pass = OpaquePass::new(
            &ctx.device,
            ctx.surface_format,
            DEPTH_FORMAT,
            &texture_atlas.bind_group_layout,
        );
        let skybox_pass = SkyboxPass::new(&ctx.device, ctx.surface_format);
        let transparent_pass = TransparentPass::new(
            &ctx.device,
            ctx.surface_format,
            DEPTH_FORMAT,
            &texture_atlas.bind_group_layout,
        );
        let player_pass = PlayerPass::new(&ctx.device, ctx.surface_format, DEPTH_FORMAT);
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
            skybox_pass,
            transparent_pass,
            player_pass,
            selection_pass,
            texture_atlas,
            chunk_meshes: HashMap::new(),
            transparent_meshes: HashMap::new(),
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
        let chunk_origin_world = Vec3::new(
            pos.x as f32 * voxweb_core::CHUNK_X as f32,
            0.0,
            pos.z as f32 * voxweb_core::CHUNK_Z as f32,
        );
        let world_bounds = Aabb::new(
            mesh.bounds.min + chunk_origin_world,
            mesh.bounds.max + chunk_origin_world,
        );
        match self.opaque_pass.upload_chunk_mesh(
            &self.device,
            &mesh.vertices,
            &mesh.indices,
            &mesh.smooth_vertices,
            &mesh.smooth_indices,
            world_bounds,
        ) {
            Some(gpu) => {
                self.chunk_meshes.insert(pos, gpu);
            }
            None => {
                self.chunk_meshes.remove(&pos);
            }
        }
        match self.transparent_pass.upload_mesh(
            &self.device,
            &mesh.transparent_vertices,
            &mesh.transparent_indices,
        ) {
            Some(gpu) => {
                self.transparent_meshes.insert(pos, gpu);
            }
            None => {
                self.transparent_meshes.remove(&pos);
            }
        }
    }

    /// 卸载某个 Chunk 的 GPU 网格（玩家走远时调用）。
    pub fn drop_chunk_mesh(&mut self, pos: ChunkPos) {
        self.chunk_meshes.remove(&pos);
        self.transparent_meshes.remove(&pos);
    }

    /// 清空当前世界上传过的渲染缓存。
    ///
    /// Renderer 跨 AppState 常驻；退出世界只销毁 client::Game 不会自动释放这里的
    /// GPU 缓存，所以切换世界前必须显式清空，避免上一局内容继续参与绘制。
    pub fn clear_world_cache(&mut self) {
        self.chunk_meshes.clear();
        self.transparent_meshes.clear();
        self.player_pass.upload_instances(&self.queue, &[]);
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

    /// 已上传 chunk 的顶点总数（贪婪合并后）。
    pub fn uploaded_vertex_count(&self) -> u32 {
        self.chunk_meshes
            .values()
            .map(|mesh| mesh.vertex_count + mesh.smooth_vertex_count)
            .sum()
    }

    /// 已上传 chunk 的索引总数。
    pub fn uploaded_index_count(&self) -> u32 {
        self.chunk_meshes
            .values()
            .map(|mesh| mesh.index_count)
            .sum()
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
        visual: VisualFrame,
    ) -> WorldRenderStats {
        let pass_label = "opaque_pass";
        let mut stats = WorldRenderStats {
            total_chunks: self.chunk_meshes.len(),
            ..WorldRenderStats::default()
        };

        if self.chunk_meshes.is_empty() {
            // 仍然要"清屏 + 清深度"，否则 surface 上会留旧帧像素
            let _ = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some(pass_label),
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
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            return stats;
        }

        let frustum = Frustum::from_view_proj(view_proj);
        let entries: Vec<(ChunkPos, &ChunkMeshGpu)> = self
            .chunk_meshes
            .iter()
            .filter(|(_, mesh)| frustum.intersects_aabb(&mesh.bounds))
            .map(|(p, m)| (*p, m))
            .collect();

        stats.visible_chunks = entries.len();
        stats.culled_chunks = stats.total_chunks.saturating_sub(stats.visible_chunks);
        stats.drawn_vertices = entries
            .iter()
            .map(|(_, mesh)| mesh.vertex_count + mesh.smooth_vertex_count)
            .sum::<u32>();
        stats.drawn_indices = entries
            .iter()
            .map(|(_, mesh)| mesh.index_count + mesh.smooth_index_count)
            .sum::<u32>();

        for (pos, mesh) in &entries {
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
                camera_pos: [
                    visual.camera_pos.x,
                    visual.camera_pos.y,
                    visual.camera_pos.z,
                    0.0,
                ],
                fog_color: [
                    visual.fog_color.x,
                    visual.fog_color.y,
                    visual.fog_color.z,
                    0.0,
                ],
                fog_params: [
                    visual.fog_start,
                    visual.fog_end,
                    visual.haze_strength,
                    visual.time_seconds,
                ],
                sun_dir: [visual.sun_dir.x, visual.sun_dir.y, visual.sun_dir.z, 0.0],
            };
            // 写到该 chunk 自己的 uniform buffer
            self.queue
                .write_buffer(&mesh.globals_buffer, 0, bytemuck::bytes_of(&globals));
        }

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some(pass_label),
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
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.opaque_pass.pipeline);
        pass.set_bind_group(1, &self.texture_atlas.bind_group, &[]);
        for (_, mesh) in &entries {
            if mesh.index_count == 0 {
                continue;
            }
            let Some(vertex_buffer) = &mesh.vertex_buffer else {
                continue;
            };
            let Some(index_buffer) = &mesh.index_buffer else {
                continue;
            };
            pass.set_bind_group(0, &mesh.globals_bind_group, &[]);
            pass.set_vertex_buffer(0, vertex_buffer.slice(..));
            pass.set_index_buffer(index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..mesh.index_count, 0, 0..1);
        }
        pass.set_pipeline(&self.opaque_pass.smooth_pipeline);
        for (_, mesh) in entries {
            if mesh.smooth_index_count == 0 {
                continue;
            }
            let Some(vertex_buffer) = &mesh.smooth_vertex_buffer else {
                continue;
            };
            let Some(index_buffer) = &mesh.smooth_index_buffer else {
                continue;
            };
            pass.set_bind_group(0, &mesh.globals_bind_group, &[]);
            pass.set_vertex_buffer(0, vertex_buffer.slice(..));
            pass.set_index_buffer(index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..mesh.smooth_index_count, 0, 0..1);
        }
        stats
    }

    /// Depth Pre-Pass：只写深度，不写颜色。开启后 OpaquePass 仍会正常绘制颜色；
    /// 这里主要为复杂场景提供 Early-Z 热身，同时给 Phase 8 设置项一个真实 GPU pass。
    pub fn render_depth_prepass(&mut self, encoder: &mut wgpu::CommandEncoder, view_proj: Mat4) {
        if self.chunk_meshes.is_empty() {
            let _ = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("depth_prepass_clear"),
                color_attachments: &[],
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

        let frustum = Frustum::from_view_proj(view_proj);
        let entries: Vec<(ChunkPos, &ChunkMeshGpu)> = self
            .chunk_meshes
            .iter()
            .filter(|(_, mesh)| frustum.intersects_aabb(&mesh.bounds))
            .map(|(p, m)| (*p, m))
            .collect();
        for (pos, mesh) in &entries {
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
                ..GlobalsUniform::default()
            };
            self.queue
                .write_buffer(&mesh.globals_buffer, 0, bytemuck::bytes_of(&globals));
        }
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("depth_prepass"),
            color_attachments: &[],
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
        pass.set_pipeline(&self.opaque_pass.depth_pipeline);
        pass.set_bind_group(1, &self.texture_atlas.bind_group, &[]);
        for (_, mesh) in &entries {
            if mesh.index_count == 0 {
                continue;
            }
            let Some(vertex_buffer) = &mesh.vertex_buffer else {
                continue;
            };
            let Some(index_buffer) = &mesh.index_buffer else {
                continue;
            };
            pass.set_bind_group(0, &mesh.globals_bind_group, &[]);
            pass.set_vertex_buffer(0, vertex_buffer.slice(..));
            pass.set_index_buffer(index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..mesh.index_count, 0, 0..1);
        }
        pass.set_pipeline(&self.opaque_pass.smooth_depth_pipeline);
        for (_, mesh) in entries {
            if mesh.smooth_index_count == 0 {
                continue;
            }
            let Some(vertex_buffer) = &mesh.smooth_vertex_buffer else {
                continue;
            };
            let Some(index_buffer) = &mesh.smooth_index_buffer else {
                continue;
            };
            pass.set_bind_group(0, &mesh.globals_bind_group, &[]);
            pass.set_vertex_buffer(0, vertex_buffer.slice(..));
            pass.set_index_buffer(index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..mesh.smooth_index_count, 0, 0..1);
        }
    }

    /// 程序化天空：在世界几何前绘制，负责填满 color target。
    pub fn render_skybox(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        color_view: &wgpu::TextureView,
        view_proj: Mat4,
        visual: VisualFrame,
    ) {
        let inv = view_proj.inverse();
        let globals = SkyboxGlobals {
            inv_view_proj: inv.to_cols_array_2d(),
            sun_dir_time: [
                visual.sun_dir.x,
                visual.sun_dir.y,
                visual.sun_dir.z,
                visual.time_seconds,
            ],
            fog_color: [
                visual.fog_color.x,
                visual.fog_color.y,
                visual.fog_color.z,
                0.0,
            ],
        };
        self.queue.write_buffer(
            &self.skybox_pass.globals_buffer,
            0,
            bytemuck::bytes_of(&globals),
        );
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("skybox_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: color_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.skybox_pass.pipeline);
        pass.set_bind_group(0, &self.skybox_pass.globals_bind_group, &[]);
        pass.draw(0..3, 0..1);
    }

    /// 半透明方块：按 chunk 中心到相机距离从远到近绘制。
    pub fn render_transparent(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        color_view: &wgpu::TextureView,
        view_proj: Mat4,
        visual: VisualFrame,
    ) {
        if self.transparent_meshes.is_empty() {
            return;
        }
        let mut entries: Vec<(ChunkPos, f32)> = self
            .transparent_meshes
            .keys()
            .map(|pos| {
                let center = Vec3::new(
                    pos.x as f32 * voxweb_core::CHUNK_X as f32 + 8.0,
                    128.0,
                    pos.z as f32 * voxweb_core::CHUNK_Z as f32 + 8.0,
                );
                (*pos, center.distance_squared(visual.camera_pos))
            })
            .collect();
        entries.sort_by(|a, b| b.1.total_cmp(&a.1));

        for (pos, _) in &entries {
            let Some(mesh) = self.transparent_meshes.get(pos) else {
                continue;
            };
            let chunk_origin_world = Vec3::new(
                pos.x as f32 * voxweb_core::CHUNK_X as f32,
                0.0,
                pos.z as f32 * voxweb_core::CHUNK_Z as f32,
            );
            let globals = TransparentGlobals {
                view_proj: view_proj.to_cols_array_2d(),
                chunk_origin: [
                    chunk_origin_world.x,
                    chunk_origin_world.y,
                    chunk_origin_world.z,
                    0.0,
                ],
                camera_pos: [
                    visual.camera_pos.x,
                    visual.camera_pos.y,
                    visual.camera_pos.z,
                    0.0,
                ],
                fog_color: [
                    visual.fog_color.x,
                    visual.fog_color.y,
                    visual.fog_color.z,
                    0.0,
                ],
                fog_params: [
                    visual.fog_start,
                    visual.fog_end,
                    visual.haze_strength,
                    visual.time_seconds,
                ],
                sun_dir: [visual.sun_dir.x, visual.sun_dir.y, visual.sun_dir.z, 0.0],
            };
            self.queue
                .write_buffer(&mesh.globals_buffer, 0, bytemuck::bytes_of(&globals));
        }

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("transparent_pass"),
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
        pass.set_pipeline(&self.transparent_pass.pipeline);
        pass.set_bind_group(1, &self.texture_atlas.bind_group, &[]);
        for (pos, _) in entries {
            let Some(mesh) = self.transparent_meshes.get(&pos) else {
                continue;
            };
            pass.set_bind_group(0, &mesh.globals_bind_group, &[]);
            pass.set_vertex_buffer(0, mesh.vertex_buffer.slice(..));
            pass.set_index_buffer(mesh.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..mesh.index_count, 0, 0..1);
        }
    }

    /// 渲染选中体积的线框。`selection = None` 时跳过（玩家未瞄准任何方块）。
    /// 必须在 `render_world` 之后调用，共享同一份 depth view 但不写深度。
    pub fn render_selection(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        color_view: &wgpu::TextureView,
        view_proj: Mat4,
        selection: Option<Aabb>,
    ) {
        let Some(selection) = selection else {
            return;
        };
        let size = selection.max - selection.min;
        let globals = SelectionGlobals {
            view_proj: view_proj.to_cols_array_2d(),
            box_min: [selection.min.x, selection.min.y, selection.min.z, 0.0],
            box_size: [size.x.max(0.02), size.y.max(0.02), size.z.max(0.02), 0.0],
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

    /// 上传远端玩家实例缓冲（Phase 5）。无远端玩家时传空 slice 即可。
    pub fn upload_player_instances(&mut self, instances: &[PlayerInstance]) {
        self.player_pass.upload_instances(&self.queue, instances);
    }

    /// 渲染所有远端玩家实体（Phase 5）。
    /// 必须在 `render_world` 之后、`render_selection` 之前调用；LoadOp=Load。
    pub fn render_players(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        color_view: &wgpu::TextureView,
        view_proj: Mat4,
    ) {
        self.player_pass.write_globals(&self.queue, view_proj);
        self.player_pass
            .render(encoder, color_view, &self.depth_view);
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
