//! Render Graph：多 Pass 调度框架。
//!
//! 将渲染拆分为多个 RenderPass 实现，按顺序执行。
//! 每个 Pass 通过 trait 定义，Graph 持有 Pass 列表并按序调度。

/// 单个渲染 Pass 的 trait。
/// 每个 Pass 实现 `prepare`（准备 GPU 指令）和 `execute`（编码到 CommandEncoder）。
pub trait RenderPass {
    fn name(&self) -> &'static str;

    /// 每帧开始时调用，用于更新 uniform / buffer 等。
    fn prepare(&mut self, _device: &wgpu::Device, _queue: &wgpu::Queue) {}

    /// 编码本条 Pass 到给定 CommandEncoder。
    /// `output_view` 是当前帧的输出纹理。
    fn execute(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        output_view: &wgpu::TextureView,
        depth_view: Option<&wgpu::TextureView>,
    );
}

/// Render Graph：持有所有 Pass，按注册顺序执行。
pub struct RenderGraph {
    passes: Vec<Box<dyn RenderPass>>,
    // 可选：深度纹理（在 Pass 间共享）
    depth_texture: Option<wgpu::Texture>,
    depth_view: Option<wgpu::TextureView>,
}

impl RenderGraph {
    pub fn new() -> Self {
        Self {
            passes: Vec::new(),
            depth_texture: None,
            depth_view: None,
        }
    }

    /// 注册一个新的 Pass（如 Opaque、Skybox、Transparent、UI）。
    pub fn add_pass(&mut self, pass: Box<dyn RenderPass>) {
        self.passes.push(pass);
    }

    /// 确保深度纹理尺寸与 surface 匹配。
    pub fn ensure_depth_texture(
        &mut self,
        device: &wgpu::Device,
        width: u32,
        height: u32,
    ) {
        // 检查是否需要重建
        let needs_rebuild = match &self.depth_texture {
            Some(tex) => tex.width() != width || tex.height() != height,
            None => true,
        };
        if !needs_rebuild {
            return;
        }

        self.depth_texture = Some(device.create_texture(&wgpu::TextureDescriptor {
            label: Some("depth"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth24Plus,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        }));
        self.depth_view = self
            .depth_texture
            .as_ref()
            .map(|t| t.create_view(&wgpu::TextureViewDescriptor::default()));
    }

    /// 执行所有 Pass，生成一帧。
    pub fn execute(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        surface_texture: &wgpu::SurfaceTexture,
    ) {
        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let depth = self.depth_view.as_ref();

        // 准备阶段
        for pass in &mut self.passes {
            pass.prepare(device, queue);
        }

        // 编码阶段
        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });

        for pass in &self.passes {
            pass.execute(device, queue, &mut encoder, &view, depth);
        }

        queue.submit(std::iter::once(encoder.finish()));
    }
}

impl Default for RenderGraph {
    fn default() -> Self {
        Self::new()
    }
}
