//! 半透明方块 Pass：渲染水、玻璃等需要 alpha blending 的方块。

use super::super::graph::RenderPass;

/// Transparent Pass 占位实现（Phase 8 填充）。
pub struct TransparentPass {
    // Phase 8: 透明顶点缓冲、距离排序等
}

impl TransparentPass {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for TransparentPass {
    fn default() -> Self {
        Self::new()
    }
}

impl RenderPass for TransparentPass {
    fn name(&self) -> &'static str {
        "transparent"
    }

    fn execute(
        &self,
        _device: &wgpu::Device,
        _queue: &wgpu::Queue,
        _encoder: &mut wgpu::CommandEncoder,
        _output_view: &wgpu::TextureView,
        _depth_view: Option<&wgpu::TextureView>,
    ) {
        // Phase 8 实现
    }
}
