//! 天空盒 Pass：渲染程序化天空 + 太阳。

use super::super::graph::RenderPass;

/// Skybox Pass 占位实现（Phase 8 填充）。
pub struct SkyboxPass {
    // Phase 8: 天空着色器 uniform、pipeline 等
}

impl SkyboxPass {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for SkyboxPass {
    fn default() -> Self {
        Self::new()
    }
}

impl RenderPass for SkyboxPass {
    fn name(&self) -> &'static str {
        "skybox"
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
