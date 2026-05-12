//! wgpu Device / Surface 初始化，与浏览器 canvas 绑定。
//!
//! 实际只在 wasm32 + WebGPU 后端运行（项目仅浏览器部署）；
//! desktop target 保留一个返回 Err 的存根，让 lib 单元测试能在桌面跑通编译。

use std::sync::Arc;

use wgpu::{SurfaceCapabilities, TextureFormat, TextureUsages};

/// 初始化 wgpu 所需的上下文：从一块 HTML canvas 获取 GPU 设备和 Surface。
pub struct DeviceContext {
    pub surface: wgpu::Surface<'static>,
    pub device: Arc<wgpu::Device>,
    pub queue: Arc<wgpu::Queue>,
    pub surface_format: TextureFormat,
}

/// 创建 wgpu Instance、Adapter、Device 和 Surface（wasm32 实现）。
#[cfg(target_arch = "wasm32")]
pub async fn init_device(canvas: &web_sys::HtmlCanvasElement) -> Result<DeviceContext, String> {
    use wgpu::{
        DeviceDescriptor, Instance, InstanceDescriptor, MemoryHints, PowerPreference,
        RequestAdapterOptions,
    };

    let instance_desc = InstanceDescriptor {
        backends: wgpu::Backends::BROWSER_WEBGPU,
        flags: wgpu::InstanceFlags::default(),
        backend_options: wgpu::BackendOptions::default(),
        display: None,
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
    };
    let instance = Instance::new(instance_desc);

    let surface = instance
        .create_surface(wgpu::SurfaceTarget::Canvas(canvas.clone()))
        .map_err(|e| format!("创建 Surface 失败: {e}"))?;

    let adapter_options = RequestAdapterOptions {
        power_preference: PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: Some(&surface),
    };
    let adapter = instance
        .request_adapter(&adapter_options)
        .await
        .map_err(|e| format!("无法获取 GPU 适配器：{e}"))?;

    let caps = surface.get_capabilities(&adapter);
    let surface_format = select_format(&caps);

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

/// Desktop 存根：直接返回错误。本项目只在浏览器跑，desktop 仅为单元测试编译目标。
#[cfg(not(target_arch = "wasm32"))]
pub async fn init_device(_canvas: &web_sys::HtmlCanvasElement) -> Result<DeviceContext, String> {
    Err("init_device 只在 wasm32 浏览器目标下可用".to_string())
}

/// 从 SurfaceCapabilities 中选择首选纹理格式。
#[allow(dead_code)] // desktop target 下 init_device 走存根分支，不调用本函数
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
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        },
    );
}
