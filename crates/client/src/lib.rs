//! VoxWeb 客户端入口（cdylib）。
//!
//! Phase 2：
//! - Lobby UI：单机模式按钮 + 种子输入
//! - InGame：Local 模式 Server + NetEndpoint::Local + ChunkLoader 滚动 + MeshJobQueue
//! - 主循环按 AppState 分流：Lobby（仅 egui） / InGame（完整 server tick + 网格化 + 渲染）

pub mod app;
pub mod camera;
pub mod chunk_loader;
pub mod input;
pub mod interp;
pub mod mesh_jobs;
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

use voxweb_core::protocol::{ClientMessage, ServerMessage};
use voxweb_render::Renderer;

use crate::app::{AppState, Game, GameSettings};
use crate::input::InputState;
use crate::ui::lobby::{LobbyAction, LobbyState, draw_lobby};

/// 全局 App：跨 state 持有 renderer / egui / input；InGame 时持有 Game。
struct App {
    canvas: HtmlCanvasElement,
    renderer: Renderer,
    egui_ctx: egui::Context,
    egui_renderer: egui_wgpu::Renderer,

    input: Rc<RefCell<InputState>>,
    /// egui 鼠标事件累加器：浏览器 mousemove/down/up 转 egui::Event 推这里，
    /// render_*_frame 每帧 drain 入 RawInput.events，让按钮收得到点击。
    egui_events: Rc<RefCell<Vec<egui::Event>>>,

    state: AppState,
    lobby_state: LobbyState,
    game: Option<Game>,

    /// 上一帧 performance.now()（毫秒）
    last_time_ms: f64,
    /// FPS 滑动平均
    fps_frames: u32,
    fps_accum: f32,
    fps_display: f32,

    /// 标志：下次 InGame 渲染前请求一次指针锁（点击 Lobby 按钮触发）
    request_pointer_lock_next: bool,
}

#[wasm_bindgen(start)]
pub async fn start() -> Result<(), JsValue> {
    console_error_panic_hook::set_once();
    tracing_wasm::set_as_global_default();

    log::info!("VoxWeb 启动（Phase 2：体素单人）");

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

    let mut renderer = Renderer::new(&canvas)
        .await
        .map_err(|e| JsValue::from_str(&format!("Renderer init: {e}")))?;
    let (cw, ch) = sync_canvas_size(&canvas);
    renderer.resize(cw, ch);

    let egui_ctx = egui::Context::default();
    let egui_renderer = egui_wgpu::Renderer::new(
        &renderer.device,
        renderer.surface_format,
        egui_wgpu::RendererOptions::default(),
    );

    let input = Rc::new(RefCell::new(InputState::default()));
    let egui_events: Rc<RefCell<Vec<egui::Event>>> = Rc::new(RefCell::new(Vec::new()));

    let app = Rc::new(RefCell::new(App {
        canvas: canvas.clone(),
        renderer,
        egui_ctx,
        egui_renderer,
        input: input.clone(),
        egui_events: egui_events.clone(),
        state: AppState::Lobby,
        lobby_state: LobbyState::default(),
        game: None,
        last_time_ms: now_ms(),
        fps_frames: 0,
        fps_accum: 0.0,
        fps_display: 0.0,
        request_pointer_lock_next: false,
    }));

    install_event_listeners(&canvas, &document, input.clone(), egui_events, app.clone())?;
    spawn_raf_loop(app);

    Ok(())
}

// ============================================================
// 主循环 & 事件
// ============================================================

fn spawn_raf_loop(app: Rc<RefCell<App>>) {
    // wasm RAF 闭包链需要这种嵌套类型来支持自我引用调度；type_complexity 抑制是惯用做法
    #[allow(clippy::type_complexity)]
    let cell: Rc<RefCell<Option<Closure<dyn FnMut()>>>> = Rc::new(RefCell::new(None));
    let cell_outer = cell.clone();

    *cell.borrow_mut() = Some(Closure::wrap(Box::new(move || {
        if let Err(e) = render_frame(&app) {
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
    egui_events: Rc<RefCell<Vec<egui::Event>>>,
    app: Rc<RefCell<App>>,
) -> Result<(), JsValue> {
    // —— 点击 canvas → 请求指针锁（仅在 InGame 时）——
    {
        let canvas_clone = canvas.clone();
        let app_clone = app.clone();
        let on_click = Closure::<dyn FnMut(_)>::new(move |_e: web_sys::MouseEvent| {
            if app_clone.borrow().state == AppState::InGame {
                canvas_clone.request_pointer_lock();
            }
        });
        canvas.add_event_listener_with_callback("click", on_click.as_ref().unchecked_ref())?;
        on_click.forget();
    }

    // —— pointerlockchange ——
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
        let app_clone = app.clone();
        let on_keydown = Closure::<dyn FnMut(_)>::new(move |e: web_sys::KeyboardEvent| {
            // Lobby 时让 egui 接管文本输入（不消费 WASD）
            if app_clone.borrow().state != AppState::InGame {
                return;
            }
            if let Some(key) = map_key(&e.code()) {
                input_clone.borrow_mut().on_key_down(key);
            }
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
        let app_clone = app.clone();
        let on_keyup = Closure::<dyn FnMut(_)>::new(move |e: web_sys::KeyboardEvent| {
            if app_clone.borrow().state != AppState::InGame {
                return;
            }
            if let Some(key) = map_key(&e.code()) {
                input_clone.borrow_mut().on_key_up(key);
            }
        });
        document.add_event_listener_with_callback("keyup", on_keyup.as_ref().unchecked_ref())?;
        on_keyup.forget();
    }

    // —— 鼠标移动 ——
    // 指针锁时累积 dx/dy 给相机；否则上报位置给 egui（UI 命中测试）。
    {
        let input_clone = input.clone();
        let egui_events_clone = egui_events.clone();
        let on_mousemove = Closure::<dyn FnMut(_)>::new(move |e: web_sys::MouseEvent| {
            let mut s = input_clone.borrow_mut();
            if s.pointer_locked {
                s.on_mouse_move(e.movement_x() as f32, e.movement_y() as f32);
            } else {
                egui_events_clone
                    .borrow_mut()
                    .push(egui::Event::PointerMoved(egui::pos2(
                        e.client_x() as f32,
                        e.client_y() as f32,
                    )));
            }
        });
        document
            .add_event_listener_with_callback("mousemove", on_mousemove.as_ref().unchecked_ref())?;
        on_mousemove.forget();
    }

    // —— 鼠标按下 ——
    // InGame：转给 InputState（Phase 3 才用）；Lobby：转 egui PointerButton 事件。
    {
        let input_clone = input.clone();
        let egui_events_clone = egui_events.clone();
        let app_clone = app.clone();
        let on_mousedown = Closure::<dyn FnMut(_)>::new(move |e: web_sys::MouseEvent| {
            input_clone.borrow_mut().on_mouse_down(e.button() as u16);
            if app_clone.borrow().state != AppState::InGame
                && let Some(button) = map_pointer_button(e.button())
            {
                egui_events_clone
                    .borrow_mut()
                    .push(egui::Event::PointerButton {
                        pos: egui::pos2(e.client_x() as f32, e.client_y() as f32),
                        button,
                        pressed: true,
                        modifiers: egui::Modifiers::default(),
                    });
            }
        });
        canvas
            .add_event_listener_with_callback("mousedown", on_mousedown.as_ref().unchecked_ref())?;
        on_mousedown.forget();
    }

    // —— 鼠标松开（只为 egui 服务；InGame 端目前不区分 down/up）——
    {
        let egui_events_clone = egui_events.clone();
        let app_clone = app.clone();
        let on_mouseup = Closure::<dyn FnMut(_)>::new(move |e: web_sys::MouseEvent| {
            if app_clone.borrow().state != AppState::InGame
                && let Some(button) = map_pointer_button(e.button())
            {
                egui_events_clone
                    .borrow_mut()
                    .push(egui::Event::PointerButton {
                        pos: egui::pos2(e.client_x() as f32, e.client_y() as f32),
                        button,
                        pressed: false,
                        modifiers: egui::Modifiers::default(),
                    });
            }
        });
        document
            .add_event_listener_with_callback("mouseup", on_mouseup.as_ref().unchecked_ref())?;
        on_mouseup.forget();
    }

    Ok(())
}

/// 浏览器 MouseEvent.button() 数值 → egui PointerButton。
fn map_pointer_button(button: i16) -> Option<egui::PointerButton> {
    match button {
        0 => Some(egui::PointerButton::Primary),
        1 => Some(egui::PointerButton::Middle),
        2 => Some(egui::PointerButton::Secondary),
        _ => None,
    }
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
// 帧分发
// ============================================================

fn render_frame(app: &Rc<RefCell<App>>) -> Result<(), String> {
    let _dt = update_clock(app);

    // 同步 canvas 尺寸
    let (cw, ch) = {
        let app_borrow = app.borrow();
        sync_canvas_size(&app_borrow.canvas)
    };
    {
        let mut a = app.borrow_mut();
        a.renderer.resize(cw, ch);
    }

    // 按 state 分流
    let state = app.borrow().state.clone();
    match state {
        AppState::InGame => render_game_frame(app, _dt, cw, ch),
        // Loading / Lobby / 其它态：渲染大厅 UI（其它态待后续 Phase）
        _ => render_lobby_frame(app, cw, ch),
    }
}

fn update_clock(app: &Rc<RefCell<App>>) -> f32 {
    let mut a = app.borrow_mut();
    let now = now_ms();
    let dt_ms = (now - a.last_time_ms).max(0.0);
    a.last_time_ms = now;
    let dt = (dt_ms / 1000.0) as f32;
    a.fps_frames += 1;
    a.fps_accum += dt;
    if a.fps_accum >= 0.5 {
        a.fps_display = a.fps_frames as f32 / a.fps_accum;
        a.fps_frames = 0;
        a.fps_accum = 0.0;
    }
    dt
}

// ============================================================
// Lobby 帧
// ============================================================

fn render_lobby_frame(app: &Rc<RefCell<App>>, cw: u32, ch: u32) -> Result<(), String> {
    // —— 跑 egui Lobby UI ——
    let (action, paint_jobs, pixels_per_point, textures_delta) = {
        let mut a = app.borrow_mut();
        // 收集本帧累积的鼠标事件（mousemove / mousedown / mouseup）。
        let events: Vec<egui::Event> = std::mem::take(&mut *a.egui_events.borrow_mut());
        let raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(cw as f32, ch as f32),
            )),
            events,
            ..Default::default()
        };
        let App {
            ref egui_ctx,
            ref mut lobby_state,
            ..
        } = *a;
        let mut act: Option<LobbyAction> = None;
        let full_output = egui_ctx.run_ui(raw_input, |ui| {
            act = draw_lobby(ui.ctx(), lobby_state);
        });
        let ppp = full_output.pixels_per_point;
        let jobs = egui_ctx.tessellate(full_output.shapes, ppp);
        (act, jobs, ppp, full_output.textures_delta)
    };

    // —— 处理动作（开始游戏）——
    if let Some(LobbyAction::StartSinglePlayer { seed }) = action {
        start_single_player(app, seed);
        // 进入 InGame 后下一帧才走 game 路径；本帧仍渲染 lobby（避免 game 未初始化的纹理上传）
    }

    // —— 上传 egui 纹理 + 渲染 ——
    {
        let mut a = app.borrow_mut();
        let device = a.renderer.device.clone();
        let queue = a.renderer.queue.clone();
        for (id, image_delta) in &textures_delta.set {
            a.egui_renderer
                .update_texture(&device, &queue, *id, image_delta);
        }
        for id in &textures_delta.free {
            a.egui_renderer.free_texture(id);
        }

        let Some(surface_texture) = a.renderer.acquire_frame() else {
            return Ok(());
        };
        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("lobby_frame"),
        });

        // 清屏（暗蓝色背景）
        {
            let _clear_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("lobby_clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.05,
                            g: 0.07,
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
        }

        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [cw, ch],
            pixels_per_point,
        };
        let extra_cmds = a.egui_renderer.update_buffers(
            &device,
            &queue,
            &mut encoder,
            &paint_jobs,
            &screen_descriptor,
        );
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("lobby_egui"),
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
            a.egui_renderer
                .render(&mut pass, &paint_jobs, &screen_descriptor);
        }

        queue.submit(
            extra_cmds
                .into_iter()
                .chain(std::iter::once(encoder.finish())),
        );
        surface_texture.present();
    }
    Ok(())
}

fn start_single_player(app: &Rc<RefCell<App>>, seed: Option<u64>) {
    let seed = seed.unwrap_or_else(random_seed);
    log::info!("启动单机游戏，seed = {seed}");

    let settings = GameSettings::default();
    let mut game = Game::new_local(seed, settings);

    // 发 Hello，driver 下一帧消费
    game.net.send_client_message(ClientMessage::Hello {
        display_name: "Player".into(),
        version: 1,
    });

    // 把 spawn 位置塞进相机（先看一眼地形）
    game.camera.position = glam::Vec3::new(8.0, 100.0, 8.0);
    game.camera.pitch = -0.4;

    let mut a = app.borrow_mut();
    a.game = Some(game);
    a.state = AppState::InGame;
    a.request_pointer_lock_next = true;
}

/// 用 getrandom 生成一个 u64 随机种子。失败时退化为 0。
fn random_seed() -> u64 {
    let mut buf = [0u8; 8];
    let _ = getrandom::getrandom(&mut buf);
    u64::from_le_bytes(buf)
}

// ============================================================
// InGame 帧
// ============================================================

fn render_game_frame(app: &Rc<RefCell<App>>, dt: f32, cw: u32, ch: u32) -> Result<(), String> {
    // —— 1. drain Local 通道（Client→Server）→ Server::handle_message → 推回 Server→Client ——
    {
        let mut a = app.borrow_mut();
        let Some(game) = a.game.as_mut() else {
            return Ok(());
        };
        let mut pending = Vec::new();
        while let Some(msg) = game.server_inbox.try_recv_client_message() {
            pending.push(msg);
        }
        let entity_id = if game.entity_id == 0 {
            1
        } else {
            game.entity_id
        };
        for msg in pending {
            let replies = game.server.borrow_mut().handle_message(entity_id, msg);
            for reply in replies {
                game.server_inbox.send_server_message(reply);
            }
        }
    }

    // —— 2. drain Server→Client → 应用 ——
    {
        let mut a = app.borrow_mut();
        let Some(game) = a.game.as_mut() else {
            return Ok(());
        };
        while let Some(msg) = game.net.try_recv_server_message() {
            apply_server_message(game, msg);
        }
    }

    // —— 3. 输入 → 相机 + 4. 逻辑帧 ——
    let (camera_pos, view_proj, fps_display, mesh_budget) = {
        let mut a = app.borrow_mut();
        let fps_display = a.fps_display;
        // 先克隆 input 的 Rc，再 mut-borrow game，避免对 `a` 的双重借用
        let input_rc = a.input.clone();
        let Some(game) = a.game.as_mut() else {
            return Ok(());
        };
        game.camera.aspect = cw as f32 / ch.max(1) as f32;

        let mut input = input_rc.borrow_mut();
        if input.pointer_locked && (input.mouse_dx != 0.0 || input.mouse_dy != 0.0) {
            game.camera.apply_mouse(
                input.mouse_dx,
                input.mouse_dy,
                game.settings.mouse_sensitivity,
            );
        }
        game.camera
            .apply_fly_input(&input, game.settings.fly_speed, dt);
        input.reset_delta();
        drop(input);

        game.frame_clock.accumulate(dt);
        while game.frame_clock.consume_logic_step() {
            game.server.borrow_mut().tick();
        }

        (
            game.camera.position,
            game.camera.vp_matrix(),
            fps_display,
            game.settings.mesh_budget_ms,
        )
    };

    // —— 5. ChunkLoader 滚动 ——
    {
        let mut a = app.borrow_mut();
        let App {
            ref mut renderer,
            ref mut game,
            ..
        } = *a;
        let Some(game) = game.as_mut() else {
            return Ok(());
        };
        let mut server_mut = game.server.borrow_mut();
        game.chunk_loader
            .update(camera_pos, &mut server_mut, &mut game.mesh_jobs, renderer);
    }

    // —— 6. mesh_jobs run_until_budget ——
    {
        let mut a = app.borrow_mut();
        let App {
            ref mut renderer,
            ref mut game,
            ..
        } = *a;
        let Some(game) = game.as_mut() else {
            return Ok(());
        };
        let server_borrow = game.server.borrow();
        game.mesh_jobs
            .run_until_budget(mesh_budget, &server_borrow, renderer, &now_ms);
    }

    // —— 7. egui HUD ——
    let pointer_locked = app.borrow().input.borrow().pointer_locked;
    let (paint_jobs, pixels_per_point, textures_delta) = {
        let a = app.borrow();
        let raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(cw as f32, ch as f32),
            )),
            ..Default::default()
        };
        let yaw_deg = a
            .game
            .as_ref()
            .map(|g| g.camera.yaw.to_degrees())
            .unwrap_or(0.0);
        let pitch_deg = a
            .game
            .as_ref()
            .map(|g| g.camera.pitch.to_degrees())
            .unwrap_or(0.0);
        let pos = a
            .game
            .as_ref()
            .map(|g| g.camera.position)
            .unwrap_or_default();
        let loaded_chunks = a
            .game
            .as_ref()
            .map(|g| g.chunk_loader.loaded.len())
            .unwrap_or(0);
        let mesh_pending = a.game.as_ref().map(|g| g.mesh_jobs.len()).unwrap_or(0);
        let full_output = a.egui_ctx.run_ui(raw_input, |ui| {
            draw_hud(
                ui.ctx(),
                fps_display,
                (pos.x, pos.y, pos.z),
                yaw_deg,
                pitch_deg,
                pointer_locked,
                loaded_chunks,
                mesh_pending,
            );
        });
        let ppp = full_output.pixels_per_point;
        let jobs = a.egui_ctx.tessellate(full_output.shapes, ppp);
        (jobs, ppp, full_output.textures_delta)
    };

    // —— 8. 渲染 + present ——
    {
        let mut a = app.borrow_mut();
        let device = a.renderer.device.clone();
        let queue = a.renderer.queue.clone();
        for (id, image_delta) in &textures_delta.set {
            a.egui_renderer
                .update_texture(&device, &queue, *id, image_delta);
        }
        for id in &textures_delta.free {
            a.egui_renderer.free_texture(id);
        }

        let Some(surface_texture) = a.renderer.acquire_frame() else {
            return Ok(());
        };
        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("game_frame"),
        });

        // 世界 Pass
        a.renderer
            .render_world(&mut encoder, &view, view_proj, [0.55, 0.78, 0.93, 1.0]);

        // egui Pass
        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [cw, ch],
            pixels_per_point,
        };
        let extra_cmds = a.egui_renderer.update_buffers(
            &device,
            &queue,
            &mut encoder,
            &paint_jobs,
            &screen_descriptor,
        );
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("game_egui"),
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
            a.egui_renderer
                .render(&mut pass, &paint_jobs, &screen_descriptor);
        }

        queue.submit(
            extra_cmds
                .into_iter()
                .chain(std::iter::once(encoder.finish())),
        );
        surface_texture.present();

        // 进入 InGame 后请求指针锁（必须在用户手势后；首次的"开始游戏"按钮点击算手势）
        if a.request_pointer_lock_next {
            a.canvas.request_pointer_lock();
            a.request_pointer_lock_next = false;
        }
    }

    Ok(())
}

fn apply_server_message(game: &mut Game, msg: ServerMessage) {
    match msg {
        ServerMessage::Welcome {
            entity_id,
            world_seed,
            ..
        } => {
            game.entity_id = entity_id;
            log::info!("Welcome: entity_id={entity_id}, seed={world_seed}");
        }
        _ => {
            // Phase 3+ 才处理 BlockUpdate / PlayerTick 等
        }
    }
}

// ============================================================
// HUD（egui）
// ============================================================

#[allow(clippy::too_many_arguments)]
fn draw_hud(
    ctx: &egui::Context,
    fps: f32,
    pos: (f32, f32, f32),
    yaw_deg: f32,
    pitch_deg: f32,
    pointer_locked: bool,
    loaded_chunks: usize,
    mesh_pending: usize,
) {
    egui::Area::new(egui::Id::new("hud_topleft"))
        .anchor(egui::Align2::LEFT_TOP, egui::vec2(12.0, 12.0))
        .show(ctx, |ui| {
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
                    ui.colored_label(
                        egui::Color32::from_rgb(160, 175, 190),
                        format!("CHUNKS {loaded_chunks}  MESH_Q {mesh_pending}"),
                    );
                });
        });

    egui::Area::new(egui::Id::new("hud_crosshair"))
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.label(
                egui::RichText::new("+")
                    .color(egui::Color32::from_rgba_unmultiplied(255, 255, 255, 220))
                    .size(22.0)
                    .strong(),
            );
        });

    egui::Area::new(egui::Id::new("hud_bottom"))
        .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -16.0))
        .show(ctx, |ui| {
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
