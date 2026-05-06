//! VoxWeb 客户端入口（cdylib）。
//!
//! Phase 1：渲染骨架。
//! - 渲染一个手工填充的 16×16×4 演示 Chunk（彩色方块）
//! - 第一人称 Fly 相机，WASD + 空格/Shift + 鼠标视角
//! - 指针锁：点击 canvas 进入；ESC 退出
//! - HUD：FPS + 玩家坐标 + 操作提示

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

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

use voxweb_core::{BlockID, CHUNK_X, CHUNK_Y, CHUNK_Z, Chunk};
use voxweb_render::Renderer;
use voxweb_render::chunk_mesh;

use crate::camera::Camera;
use crate::input::InputState;

/// Phase 1 运行时：把渲染、输入、相机、UI 拼起来。
struct Runtime {
    canvas: HtmlCanvasElement,
    renderer: Renderer,
    egui_ctx: egui::Context,
    egui_renderer: egui_wgpu::Renderer,

    camera: Camera,
    input: Rc<RefCell<InputState>>,

    /// 上一帧 performance.now()（毫秒），用于计算 dt
    last_time_ms: f64,
    /// FPS 滑动平均（每秒重置）
    fps_frames: u32,
    fps_accum: f32,
    fps_display: f32,
}

#[wasm_bindgen(start)]
pub async fn start() -> Result<(), JsValue> {
    console_error_panic_hook::set_once();
    tracing_wasm::set_as_global_default();

    log::info!("VoxWeb 启动（Phase 1：渲染骨架）");

    // ── 1. 拿 canvas ───────────────────────────
    let window = web_sys::window().ok_or_else(|| JsValue::from_str("无 window"))?;
    let document = window
        .document()
        .ok_or_else(|| JsValue::from_str("无 document"))?;
    let canvas: HtmlCanvasElement = document
        .get_element_by_id("game")
        .ok_or_else(|| JsValue::from_str("未找到 #game canvas"))?
        .dyn_into()
        .map_err(|_| JsValue::from_str("#game 不是 <canvas>"))?;
    sync_canvas_size(&canvas);

    // ── 2. 渲染器（创建 wgpu device + Surface + OpaquePass）─
    let mut renderer = Renderer::new(&canvas)
        .await
        .map_err(|e| JsValue::from_str(&format!("Renderer init: {e}")))?;

    // ── 3. egui 上下文 + egui-wgpu 渲染器（共享 device/format）─
    let egui_ctx = egui::Context::default();
    let egui_renderer = egui_wgpu::Renderer::new(
        &renderer.device,
        renderer.surface_format,
        egui_wgpu::RendererOptions::default(),
    );

    // ── 4. 演示用 Chunk：16×16 的草地 + 上面摆几列彩色方块 ─
    let demo_chunk = build_demo_chunk();
    let mesh = chunk_mesh::generate_opaque_mesh(&demo_chunk);
    log::info!("演示 chunk 顶点数: {}", mesh.vertex_count());
    renderer.upload_chunk_mesh(voxweb_core::ChunkPos::new(0, 0), &mesh);

    // ── 5. 相机 + 输入 ─
    let camera = Camera::default();
    let input = Rc::new(RefCell::new(InputState::default()));

    let runtime = Rc::new(RefCell::new(Runtime {
        canvas: canvas.clone(),
        renderer,
        egui_ctx,
        egui_renderer,
        camera,
        input: input.clone(),
        last_time_ms: now_ms(),
        fps_frames: 0,
        fps_accum: 0.0,
        fps_display: 0.0,
    }));

    // ── 6. 注册事件监听 ─
    install_event_listeners(&canvas, &document, input.clone(), runtime.clone())?;

    // ── 7. RAF 主循环 ─
    spawn_raf_loop(runtime);

    Ok(())
}

// ============================================================
// 主循环 & 事件
// ============================================================

fn spawn_raf_loop(runtime: Rc<RefCell<Runtime>>) {
    let cell: Rc<RefCell<Option<Closure<dyn FnMut()>>>> = Rc::new(RefCell::new(None));
    let cell_outer = cell.clone();

    *cell.borrow_mut() = Some(Closure::wrap(Box::new(move || {
        if let Err(e) = render_frame(&runtime) {
            log::warn!("帧渲染失败: {e:?}");
        }
        if let Some(c) = cell_outer.borrow().as_ref() {
            request_animation_frame(c);
        }
    }) as Box<dyn FnMut()>));

    if let Some(c) = cell.borrow().as_ref() {
        request_animation_frame(c);
    }
}

fn request_animation_frame(closure: &Closure<dyn FnMut()>) {
    let _ = web_sys::window()
        .expect("no window")
        .request_animation_frame(closure.as_ref().unchecked_ref());
}

fn install_event_listeners(
    canvas: &HtmlCanvasElement,
    document: &web_sys::Document,
    input: Rc<RefCell<InputState>>,
    runtime: Rc<RefCell<Runtime>>,
) -> Result<(), JsValue> {
    // —— 点击 canvas → 请求指针锁 ——
    {
        let canvas_clone = canvas.clone();
        let on_click = Closure::<dyn FnMut(_)>::new(move |_e: web_sys::MouseEvent| {
            // request_pointer_lock 必须在用户手势中触发
            canvas_clone.request_pointer_lock();
        });
        canvas.add_event_listener_with_callback("click", on_click.as_ref().unchecked_ref())?;
        on_click.forget();
    }

    // —— pointerlockchange → 同步 InputState.pointer_locked ——
    {
        let input_clone = input.clone();
        let document_clone = document.clone();
        let canvas_id = canvas.clone();
        let on_lock_change = Closure::<dyn FnMut()>::new(move || {
            let locked = document_clone
                .pointer_lock_element()
                .map(|el| el == *canvas_id.as_ref())
                .unwrap_or(false);
            input_clone.borrow_mut().pointer_locked = locked;
        });
        document.add_event_listener_with_callback(
            "pointerlockchange",
            on_lock_change.as_ref().unchecked_ref(),
        )?;
        on_lock_change.forget();
    }

    // —— 键盘 ——
    {
        let input_clone = input.clone();
        let on_keydown = Closure::<dyn FnMut(_)>::new(move |e: web_sys::KeyboardEvent| {
            if let Some(key) = map_key(&e.code()) {
                input_clone.borrow_mut().on_key_down(key);
            }
            // 防止浏览器吞掉空格滚动等默认行为（指针锁住时）
            if input_clone.borrow().pointer_locked {
                e.prevent_default();
            }
        });
        document
            .add_event_listener_with_callback("keydown", on_keydown.as_ref().unchecked_ref())?;
        on_keydown.forget();
    }
    {
        let input_clone = input.clone();
        let on_keyup = Closure::<dyn FnMut(_)>::new(move |e: web_sys::KeyboardEvent| {
            if let Some(key) = map_key(&e.code()) {
                input_clone.borrow_mut().on_key_up(key);
            }
        });
        document.add_event_listener_with_callback("keyup", on_keyup.as_ref().unchecked_ref())?;
        on_keyup.forget();
    }

    // —— 鼠标移动（仅在指针锁定时累积）——
    {
        let input_clone = input.clone();
        let on_mousemove = Closure::<dyn FnMut(_)>::new(move |e: web_sys::MouseEvent| {
            let mut s = input_clone.borrow_mut();
            if s.pointer_locked {
                s.on_mouse_move(e.movement_x() as f32, e.movement_y() as f32);
            }
        });
        document
            .add_event_listener_with_callback("mousemove", on_mousemove.as_ref().unchecked_ref())?;
        on_mousemove.forget();
    }

    // —— 鼠标按下（Phase 3 才接 Break/Place，这里仅占位）——
    {
        let input_clone = input.clone();
        let on_mousedown = Closure::<dyn FnMut(_)>::new(move |e: web_sys::MouseEvent| {
            input_clone.borrow_mut().on_mouse_down(e.button() as u16);
        });
        canvas
            .add_event_listener_with_callback("mousedown", on_mousedown.as_ref().unchecked_ref())?;
        on_mousedown.forget();
    }

    // —— ResizeObserver: canvas 尺寸变化 → 同步给 renderer ——
    // 简化做法：每帧 render_frame 内自己检测尺寸，省一个 ResizeObserver 闭包链
    let _ = runtime; // 主循环里 own runtime；此处不需要复制

    Ok(())
}

/// 把 web_sys::KeyboardEvent.code() 映射到 winit::KeyCode（输入层使用统一枚举）。
fn map_key(code: &str) -> Option<winit::keyboard::KeyCode> {
    use winit::keyboard::KeyCode;
    Some(match code {
        "KeyW" => KeyCode::KeyW,
        "KeyA" => KeyCode::KeyA,
        "KeyS" => KeyCode::KeyS,
        "KeyD" => KeyCode::KeyD,
        "KeyT" => KeyCode::KeyT,
        "Space" => KeyCode::Space,
        "ShiftLeft" => KeyCode::ShiftLeft,
        "ShiftRight" => KeyCode::ShiftRight,
        "Escape" => KeyCode::Escape,
        _ => return None,
    })
}

// ============================================================
// 单帧渲染
// ============================================================

fn render_frame(runtime: &Rc<RefCell<Runtime>>) -> Result<(), String> {
    let mut rt = runtime.borrow_mut();

    // —— 1. dt + FPS ——
    let now = now_ms();
    let dt_ms = (now - rt.last_time_ms).max(0.0);
    rt.last_time_ms = now;
    let dt = (dt_ms / 1000.0) as f32;
    rt.fps_frames += 1;
    rt.fps_accum += dt;
    if rt.fps_accum >= 0.5 {
        rt.fps_display = rt.fps_frames as f32 / rt.fps_accum;
        rt.fps_frames = 0;
        rt.fps_accum = 0.0;
    }

    // —— 2. canvas 尺寸同步 ——
    let (cw, ch) = sync_canvas_size(&rt.canvas);
    rt.renderer.resize(cw, ch);
    rt.camera.aspect = cw as f32 / ch.max(1) as f32;

    // —— 3. 输入 → 相机 ——
    {
        let input_rc = rt.input.clone();
        let mut input = input_rc.borrow_mut();
        let camera = &mut rt.camera;
        if input.pointer_locked && (input.mouse_dx != 0.0 || input.mouse_dy != 0.0) {
            camera.apply_mouse(input.mouse_dx, input.mouse_dy, 0.0025);
        }
        camera.apply_fly_input(&input, /*speed=*/ 12.0, dt);
        input.reset_delta();
    }

    // —— 4. egui 跑一遍 UI ——
    let raw_input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(cw as f32, ch as f32),
        )),
        ..Default::default()
    };
    let pos = rt.camera.position;
    let yaw_deg = rt.camera.yaw.to_degrees();
    let pitch_deg = rt.camera.pitch.to_degrees();
    let fps = rt.fps_display;
    let pointer_locked = rt.input.borrow().pointer_locked;
    let full_output = rt.egui_ctx.run_ui(raw_input, |ui| {
        draw_hud(
            ui,
            fps,
            (pos.x, pos.y, pos.z),
            yaw_deg,
            pitch_deg,
            pointer_locked,
        );
    });
    let pixels_per_point = full_output.pixels_per_point;
    let paint_jobs = rt.egui_ctx.tessellate(full_output.shapes, pixels_per_point);

    // 上传 / 释放 egui 纹理（字形图集等）
    {
        let device = rt.renderer.device.clone();
        let queue = rt.renderer.queue.clone();
        for (id, image_delta) in &full_output.textures_delta.set {
            rt.egui_renderer
                .update_texture(&device, &queue, *id, image_delta);
        }
        for id in &full_output.textures_delta.free {
            rt.egui_renderer.free_texture(id);
        }
    }

    // —— 5. 取得本帧 surface texture ——
    let Some(surface_texture) = rt.renderer.acquire_frame() else {
        return Ok(());
    };
    let view = surface_texture
        .texture
        .create_view(&wgpu::TextureViewDescriptor::default());

    // —— 6. 编码命令 ——
    let device = rt.renderer.device.clone();
    let queue = rt.renderer.queue.clone();

    let screen_descriptor = egui_wgpu::ScreenDescriptor {
        size_in_pixels: [cw, ch],
        pixels_per_point,
    };

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("frame"),
    });

    // 6a. 世界 Pass：清屏 + 深度 + 绘制方块
    let view_proj = rt.camera.vp_matrix();
    rt.renderer
        .render_world(&mut encoder, &view, view_proj, [0.55, 0.78, 0.93, 1.0]);

    // 6b. egui Pass（叠在世界之上，不写深度）
    let extra_cmds = rt.egui_renderer.update_buffers(
        &device,
        &queue,
        &mut encoder,
        &paint_jobs,
        &screen_descriptor,
    );
    {
        let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("egui_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        let mut pass = pass.forget_lifetime();
        rt.egui_renderer
            .render(&mut pass, &paint_jobs, &screen_descriptor);
    }

    queue.submit(
        extra_cmds
            .into_iter()
            .chain(std::iter::once(encoder.finish())),
    );
    surface_texture.present();

    Ok(())
}

// ============================================================
// HUD（egui）
// ============================================================

fn draw_hud(
    ui: &mut egui::Ui,
    fps: f32,
    pos: (f32, f32, f32),
    yaw_deg: f32,
    pitch_deg: f32,
    pointer_locked: bool,
) {
    let ctx = ui.ctx().clone();

    // 左上：性能 + 坐标
    egui::Area::new(egui::Id::new("hud_topleft"))
        .anchor(egui::Align2::LEFT_TOP, egui::vec2(12.0, 12.0))
        .show(&ctx, |ui| {
            egui::Frame::default()
                .fill(egui::Color32::from_rgba_unmultiplied(0, 0, 0, 140))
                .inner_margin(egui::Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 230, 235),
                        format!("FPS  {:>5.1}", fps),
                    );
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 230, 235),
                        format!("POS  x {:+8.2}  y {:+8.2}  z {:+8.2}", pos.0, pos.1, pos.2),
                    );
                    ui.colored_label(
                        egui::Color32::from_rgb(180, 190, 200),
                        format!("YAW {:+6.1}°  PITCH {:+5.1}°", yaw_deg, pitch_deg),
                    );
                });
        });

    // 中心准星：用 Area + Label("+") 简单实现
    egui::Area::new(egui::Id::new("hud_crosshair"))
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(&ctx, |ui| {
            ui.label(
                egui::RichText::new("+")
                    .color(egui::Color32::from_rgba_unmultiplied(255, 255, 255, 220))
                    .size(22.0)
                    .strong(),
            );
        });

    // 底部：操作提示（egui 默认字体不含 CJK，Phase 1 用 ASCII；Phase 6 接入字体后切回中文）
    egui::Area::new(egui::Id::new("hud_bottom"))
        .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -16.0))
        .show(&ctx, |ui| {
            let msg = if pointer_locked {
                "WASD move | Space up | Shift down | Mouse look | ESC release"
            } else {
                "Click to enter camera control"
            };
            egui::Frame::default()
                .fill(egui::Color32::from_rgba_unmultiplied(0, 0, 0, 110))
                .inner_margin(egui::Margin::symmetric(14, 8))
                .show(ui, |ui| {
                    ui.colored_label(egui::Color32::from_rgb(230, 235, 240), msg);
                });
        });
}

// ============================================================
// 演示 Chunk 构造
// ============================================================

/// 构造一个 16×16 的演示 chunk：
/// - y=0..2 全 STONE（基岩层）
/// - y=2..4 全 DIRT
/// - y=4 全 GRASS（草坪）
/// - 在草坪上零星摆几列彩色方块（每种 BlockID 一个柱子，演示颜色）
fn build_demo_chunk() -> Chunk {
    let mut chunk = Chunk::empty();
    // 地基
    for ly in 0..2 {
        for lz in 0..CHUNK_X {
            for lx in 0..CHUNK_Z {
                chunk.set(lx, ly, lz, BlockID::STONE);
            }
        }
    }
    for ly in 2..4 {
        for lz in 0..CHUNK_X {
            for lx in 0..CHUNK_Z {
                chunk.set(lx, ly, lz, BlockID::DIRT);
            }
        }
    }
    for lz in 0..CHUNK_X {
        for lx in 0..CHUNK_Z {
            chunk.set(lx, 4, lz, BlockID::GRASS);
        }
    }

    // 草坪上摆 6 个柱子，每个 3 高，颜色不同
    let columns = [
        (2, 2, BlockID::SAND),
        (5, 2, BlockID::WOOD),
        (8, 2, BlockID::LEAVES),
        (11, 2, BlockID::GLASS),
        (2, 8, BlockID::WATER),
        (8, 8, BlockID::STONE),
    ];
    for (lx, lz, block) in columns {
        for dy in 0..3 {
            chunk.set(lx, 5 + dy, lz, block);
        }
    }

    debug_assert!(CHUNK_Y >= 8);
    chunk
}

// ============================================================
// 工具
// ============================================================

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

fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0)
}
