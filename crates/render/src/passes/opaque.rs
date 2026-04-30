//! 不透明方块 Pass：渲染实体方块（石头、泥土、草等）。

use super::super::graph::RenderPass;

/// Opaque Pass 占位实现（Phase 1 填充）。
pub struct OpaquePass {
    // Phase 1: 顶点缓冲、uniform、pipeline 等
}

impl OpaquePass {
    pub fn new() -> Self {
        Self {}
    }
}

impl RenderPass for OpaquePass {
    fn name(&self) -> &'static str {
        "opaque"
    }

    fn execute(
        &self,
        _device: &wgpu::Device,
        _queue: &wgpu::Queue,
        _encoder: &mut wgpu::CommandEncoder,
        _output_view: &wgpu::TextureView,
        _depth_view: Option<&wgpu::TextureView>,
    ) {
        // Phase 1 实现：编码不透明方块 draw calls
    }
}
