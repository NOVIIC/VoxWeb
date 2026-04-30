//! VoxWeb WebGPU 渲染层。
//! 负责 Render Graph 调度、多 Pass 执行、贪婪网格化、顶点/纹理管理。

pub mod chunk_mesh;
pub mod device;
pub mod graph;
pub mod passes;
pub mod texture;
pub mod vertex;

use std::sync::Arc;

use voxweb_core::ChunkPos;
use wgpu::TextureFormat;

/// 渲染器顶层结构体。
/// 持有 wgpu Device/Queue/Surface 和 Pass 执行上下文。
pub struct Renderer {
    pub device: Arc<wgpu::Device>,
    pub queue: Arc<wgpu::Queue>,
    pub surface: wgpu::Surface<'static>,
    pub config: RendererConfig,
    pub surface_format: TextureFormat,
}

/// 渲染器初始化配置。
#[derive(Clone, Debug)]
pub struct RendererConfig {
    pub render_distance: u32,
    pub show_stats: bool,
    pub enable_depth_prepass: bool,
}

impl Default for RendererConfig {
    fn default() -> Self {
        Self {
            render_distance: 6,
            show_stats: false,
            enable_depth_prepass: false,
        }
    }
}

/// 一次渲染调用中需要渲染的 Chunk 列表。
/// 由 client 主循环在每帧更新。
#[derive(Default)]
pub struct ChunkVisibility {
    pub opaque: Vec<ChunkPos>,
    pub transparent: Vec<ChunkPos>,
}
