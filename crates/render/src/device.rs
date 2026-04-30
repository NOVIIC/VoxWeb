//! wgpu Device / Surface 初始化，与浏览器 canvas 绑定。

use std::sync::Arc;

use wgpu::{
    DeviceDescriptor, Instance, InstanceDescriptor, MemoryHints, PowerPreference, PresentMode,
    RequestAdapterOptions, SurfaceCapabilities, TextureFormat, TextureUsages,
};

/// 初始化 wgpu 所需的上下文：从一块 HTML canvas 获取 GPU 设备和 Surface。
pub struct DeviceContext {
    pub surface: wgpu::Surface<'static>,
    pub device: Arc<wgpu::Device>,
    pub queue: Arc<wgpu::Queue>,
    pub surface_format: TextureFormat,
}

/// 创建 wgpu Instance、Adapter、Device 和 Surface。
/// `canvas` 必须是已挂载到 DOM 的 `<canvas>` 元素。
pub async fn init_device(canvas: &web_sys::HtmlCanvasElement) -> Result<DeviceContext, String> {
    // 创建 WGPU 实例（仅 WebGPU 后端）
    let instance_desc = InstanceDescriptor {
        backends: wgpu::Backends::BROWSER_WEBGPU,
        flags: wgpu::InstanceFlags::default(),
        backend_options: wgpu::BackendOptions::default(),
        display: None,
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
    };
    let instance = Instance::new(instance_desc);

    // 从 canvas 创建 surface
    let surface = instance
        .create_surface(wgpu::SurfaceTarget::Canvas(canvas.clone()))
        .map_err(|e| format!("创建 Surface 失败: {e}"))?;

    // 请求 GPU 适配器
    let adapter_options = RequestAdapterOptions {
        power_preference: PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: Some(&surface),
    };
    let adapter = instance
        .request_adapter(&adapter_options)
        .await
        .map_err(|e| format!("无法获取 GPU 适配器：{e}"))?;

    // 获取 surface 能力
    let caps = surface.get_capabilities(&adapter);
    let surface_format = select_format(&caps);

    // 创建设备 + 队列
    let device_desc = DeviceDescriptor {
        label: None,
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::downlevel_defaults(),
        memory_hints: MemoryHints::Performance,
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        trace: Default::default(),
    };
    let (device, queue) = adapter
        .request_device(&device_desc)
        .await
        .map_err(|e| format!("创建设备失败: {e}"))?;
    let device = Arc::new(device);
    let queue = Arc::new(queue);

    Ok(DeviceContext {
        surface,
        device,
        queue,
        surface_format,
    })
}

/// 从 SurfaceCapabilities 中选择首选纹理格式。
fn select_format(caps: &SurfaceCapabilities) -> TextureFormat {
    let preferred = &caps.formats;
    for fmt in [
        wgpu::TextureFormat::Bgra8UnormSrgb,
        wgpu::TextureFormat::Bgra8Unorm,
    ] {
        if preferred.contains(&fmt) {
            return fmt;
        }
    }
    preferred[0]
}

/// 配置 Surface（尺寸、格式、呈现模式）。
pub fn configure_surface(
    surface: &wgpu::Surface<'static>,
    device: &wgpu::Device,
    format: TextureFormat,
    width: u32,
    height: u32,
) {
    surface.configure(
        device,
        &wgpu::SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode: PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        },
    );
}
