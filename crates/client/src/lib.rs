//! VoxWeb 客户端入口（cdylib）。
//!
//! Phase 0：仅完成"WebGPU 清屏 + egui 居中文字"，不接入任何游戏逻辑。
//! 后续 Phase 会在 `Client` 上叠加 AppState、相机、网络、世界等。

pub mod app;
pub mod camera;
pub mod input;
pub mod interp;
pub mod physics;
pub mod prediction;
pub mod raycast;
pub mod storage;
pub mod ui;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

use voxweb_render::device::{configure_surface, init_device};

/// Phase 0 运行时上下文：持有 surface / device / queue / egui 渲染器。
/// Phase 1+ 会被 `Client` 替换为完整的 AppState 容器。
struct Phase0Runtime {
    canvas: HtmlCanvasElement,
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    surface: wgpu::Surface<'static>,
    surface_format: wgpu::TextureFormat,
    width: u32,
    height: u32,
    egui_ctx: egui::Context,
    egui_renderer: egui_wgpu::Renderer,
}

/// wasm-bindgen 入口：在 WASM 加载完成后由 trunk 注入的 JS 胶水自动调用。
#[wasm_bindgen(start)]
pub async fn start() -> Result<(), JsValue> {
    // ── 1. 基础诊断设施 ──────────────────────────
    // panic 走 console.error
    console_error_panic_hook::set_once();
    // log/tracing 走浏览器 console
    tracing_wasm::set_as_global_default();

    log::info!("VoxWeb 客户端启动（Phase 0：清屏 + Hello VoxWeb）");

    // ── 2. 拿到挂载在 DOM 上的 canvas ────────────
    let window = web_sys::window().ok_or_else(|| JsValue::from_str("无 window"))?;
    let document = window
        .document()
        .ok_or_else(|| JsValue::from_str("无 document"))?;
    let canvas: HtmlCanvasElement = document
        .get_element_by_id("game")
        .ok_or_else(|| JsValue::from_str("未找到 #game canvas"))?
        .dyn_into()
        .map_err(|_| JsValue::from_str("#game 不是 <canvas>"))?;

    // ── 3. 让 canvas bitmap 尺寸 = CSS 像素尺寸 ──
    // 不做这步会导致 wgpu surface 与 CSS 拉伸不一致 → 模糊
    let (w, h) = sync_canvas_size(&canvas);

    // ── 4. wgpu 设备 + Surface ──────────────────
    let ctx = init_device(&canvas)
        .await
        .map_err(|e| JsValue::from_str(&e))?;
    configure_surface(&ctx.surface, &ctx.device, ctx.surface_format, w, h);

    // ── 5. egui 上下文 + egui-wgpu 渲染器 ───────
    let egui_ctx = egui::Context::default();
    let egui_renderer = egui_wgpu::Renderer::new(
        &ctx.device,
        ctx.surface_format,
        egui_wgpu::RendererOptions::default(),
    );

    let runtime = Rc::new(RefCell::new(Phase0Runtime {
        canvas,
        device: ctx.device,
        queue: ctx.queue,
        surface: ctx.surface,
        surface_format: ctx.surface_format,
        width: w,
        height: h,
        egui_ctx,
        egui_renderer,
    }));

    // ── 6. 启动 RAF 主循环 ──────────────────────
    spawn_raf_loop(runtime);

    Ok(())
}

/// 把 canvas 的位图尺寸同步到 CSS 显示尺寸，返回 (width, height)。
fn sync_canvas_size(canvas: &HtmlCanvasElement) -> (u32, u32) {
    let w = (canvas.client_width().max(1)) as u32;
    let h = (canvas.client_height().max(1)) as u32;
    if canvas.width() != w {
        canvas.set_width(w);
    }
    if canvas.height() != h {
        canvas.set_height(h);
    }
    (w, h)
}

/// 创建一个自我重排队的 RAF 闭包链：每帧渲染后再排下一帧。
fn spawn_raf_loop(runtime: Rc<RefCell<Phase0Runtime>>) {
    // 经典做法：用两个 Rc 把闭包"挂在自己身上"以便每次 RAF 后再排队
    let cell: Rc<RefCell<Option<Closure<dyn FnMut()>>>> = Rc::new(RefCell::new(None));
    let cell_outer = cell.clone();

    *cell.borrow_mut() = Some(Closure::wrap(Box::new(move || {
        if let Err(e) = render_frame(&runtime) {
            // SurfaceError::Lost / Outdated 时忽略，下一帧会重配置
            log::warn!("帧渲染失败: {e:?}");
        }
        // 重新排队
        if let Some(closure) = cell_outer.borrow().as_ref() {
            request_animation_frame(closure);
        }
    }) as Box<dyn FnMut()>));

    if let Some(closure) = cell.borrow().as_ref() {
        request_animation_frame(closure);
    }
}

fn request_animation_frame(closure: &Closure<dyn FnMut()>) {
    let _ = web_sys::window()
        .expect("no window")
        .request_animation_frame(closure.as_ref().unchecked_ref());
}

/// 单帧渲染：清屏 + egui 居中标签。
fn render_frame(runtime: &Rc<RefCell<Phase0Runtime>>) -> Result<(), String> {
    let mut rt = runtime.borrow_mut();
    // Arc 克隆代价低，且能避开 RefMut 上的 split-borrow 难题
    let device = rt.device.clone();
    let queue = rt.queue.clone();

    // ── 1. canvas 尺寸变化 → 重配 surface ──
    let cw = (rt.canvas.client_width().max(1)) as u32;
    let ch = (rt.canvas.client_height().max(1)) as u32;
    if cw != rt.width || ch != rt.height {
        rt.canvas.set_width(cw);
        rt.canvas.set_height(ch);
        configure_surface(&rt.surface, &device, rt.surface_format, cw, ch);
        rt.width = cw;
        rt.height = ch;
    }

    // ── 2. 跑一遍 egui ──
    // Phase 0：不收集任何输入，只摆一个静态文字
    let raw_input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(rt.width as f32, rt.height as f32),
        )),
        ..Default::default()
    };
    let full_output = rt.egui_ctx.run_ui(raw_input, |ui| {
        // 占满整块画布（egui 默认根 Ui 即整个 screen_rect），
        // 在中心放一行大字
        ui.centered_and_justified(|ui| {
            ui.label(
                egui::RichText::new("Hello VoxWeb")
                    .color(egui::Color32::from_rgb(240, 240, 245))
                    .size(56.0),
            );
        });
    });
    let pixels_per_point = full_output.pixels_per_point;
    let paint_jobs = rt.egui_ctx.tessellate(full_output.shapes, pixels_per_point);

    // 上传/释放 egui 纹理（字形图集等）
    for (id, image_delta) in &full_output.textures_delta.set {
        rt.egui_renderer
            .update_texture(&device, &queue, *id, image_delta);
    }
    for id in &full_output.textures_delta.free {
        rt.egui_renderer.free_texture(id);
    }

    // ── 3. 取得本帧 surface texture ──
    let surface_texture = match rt.surface.get_current_texture() {
        wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
        wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
            // 让下一帧重配 surface（这里仅丢弃本帧）
            configure_surface(&rt.surface, &device, rt.surface_format, rt.width, rt.height);
            return Ok(());
        }
        wgpu::CurrentSurfaceTexture::Timeout
        | wgpu::CurrentSurfaceTexture::Occluded
        | wgpu::CurrentSurfaceTexture::Validation => return Ok(()),
    };
    let view = surface_texture
        .texture
        .create_view(&wgpu::TextureViewDescriptor::default());

    let screen_descriptor = egui_wgpu::ScreenDescriptor {
        size_in_pixels: [rt.width, rt.height],
        pixels_per_point,
    };

    // ── 4. 编码命令 ──
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("phase0_frame"),
    });

    // egui 顶点/索引缓冲上传，可能产生额外 CommandBuffer（用于纹理上传等）
    let extra_cmds = rt.egui_renderer.update_buffers(
        &device,
        &queue,
        &mut encoder,
        &paint_jobs,
        &screen_descriptor,
    );

    {
        let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("phase0_clear+egui"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.06,
                        g: 0.06,
                        b: 0.10,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        // egui_wgpu::Renderer::render 要求 'static 生命周期
        let mut pass = pass.forget_lifetime();
        rt.egui_renderer
            .render(&mut pass, &paint_jobs, &screen_descriptor);
    }

    // ── 5. 提交 + Present ──
    queue.submit(
        extra_cmds
            .into_iter()
            .chain(std::iter::once(encoder.finish())),
    );
    surface_texture.present();

    Ok(())
}
