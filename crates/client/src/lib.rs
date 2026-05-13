//! VoxWeb 客户端入口（cdylib）。
//!
//! Phase 3：
//! - InGame：物理（Walk/Fly）、DDA 射线、挖放动作、Hotbar、选中线框、ActionAck rollback、PlayerInput 上报。
//! - 主循环按 AppState 分流：Lobby（仅 egui） / InGame（完整 server tick + 物理 + 网格化 + 渲染）。

pub mod app;
pub mod camera;
pub mod chunk_loader;
pub mod hotbar;
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

use voxweb_core::block::BlockID;
use voxweb_core::chunk::Position;
use voxweb_core::protocol::{ClientMessage, ServerMessage};
use voxweb_render::Renderer;

use crate::app::{AppState, Game, GameSettings};
use crate::camera::CameraMode;
use crate::chunk_loader::affected_chunks;
use crate::input::InputState;
use crate::mesh_jobs::MeshPriority;
use crate::prediction::{PendingAction, PendingKind};
use crate::raycast::raycast;
use crate::ui::lobby::{LobbyAction, LobbyState, draw_lobby};

/// 玩家眼睛到目标方块的最大射程（与 server::physics::MAX_REACH 对齐）。
const MAX_REACH: f32 = 6.0;

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

    log::info!("VoxWeb 启动（Phase 3：物理与交互）");

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
            let mut s = input_clone.borrow_mut();
            // 锁状态切换时清掉所有 held 输入。
            // ESC 释放锁的瞬间浏览器焦点会变化，期间按住的键松开时
            // keyup 事件可能丢失（document 收不到），下次恢复锁时会出现
            // "卡键"自动飞 / 走 / 挖。这里在切换时统一复位。
            if s.pointer_locked != locked {
                s.clear_held();
            }
            s.pointer_locked = locked;
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
                input_clone.borrow_mut().on_key_down(key, now_ms());
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
    // InGame：转给 InputState；Lobby：转 egui PointerButton 事件。
    {
        let input_clone = input.clone();
        let egui_events_clone = egui_events.clone();
        let app_clone = app.clone();
        let on_mousedown = Closure::<dyn FnMut(_)>::new(move |e: web_sys::MouseEvent| {
            // 防止右键弹出浏览器上下文菜单（仅在 InGame 锁定指针时）
            let is_ingame = app_clone.borrow().state == AppState::InGame;
            if is_ingame {
                input_clone.borrow_mut().on_mouse_down(e.button() as u16);
                if e.button() == 2 {
                    e.prevent_default();
                }
            } else if let Some(button) = map_pointer_button(e.button()) {
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

    // —— 鼠标松开：InGame 转给 InputState、Lobby 转 egui ——
    {
        let input_clone = input.clone();
        let egui_events_clone = egui_events.clone();
        let app_clone = app.clone();
        let on_mouseup = Closure::<dyn FnMut(_)>::new(move |e: web_sys::MouseEvent| {
            let is_ingame = app_clone.borrow().state == AppState::InGame;
            if is_ingame {
                input_clone.borrow_mut().on_mouse_up(e.button() as u16);
            } else if let Some(button) = map_pointer_button(e.button()) {
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

    // —— 阻止右键上下文菜单（InGame 时）——
    {
        let app_clone = app.clone();
        let on_contextmenu = Closure::<dyn FnMut(_)>::new(move |e: web_sys::MouseEvent| {
            if app_clone.borrow().state == AppState::InGame {
                e.prevent_default();
            }
        });
        canvas.add_event_listener_with_callback(
            "contextmenu",
            on_contextmenu.as_ref().unchecked_ref(),
        )?;
        on_contextmenu.forget();
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
        "Digit1" => KeyCode::Digit1,
        "Digit2" => KeyCode::Digit2,
        "Digit3" => KeyCode::Digit3,
        "Digit4" => KeyCode::Digit4,
        "Digit5" => KeyCode::Digit5,
        "Digit6" => KeyCode::Digit6,
        "Digit7" => KeyCode::Digit7,
        "Digit8" => KeyCode::Digit8,
        "Digit9" => KeyCode::Digit9,
        _ => return None,
    })
}

// ============================================================
// 帧分发
// ============================================================

fn render_frame(app: &Rc<RefCell<App>>) -> Result<(), String> {
    let dt = update_clock(app);

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
        AppState::InGame => render_game_frame(app, dt, cw, ch),
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

    // 相机 yaw/pitch 用默认值（physics 驱动 position）
    game.camera.position = game.physics.eye_position();
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
        let mut msgs = Vec::new();
        while let Some(msg) = game.net.try_recv_server_message() {
            msgs.push(msg);
        }
        for msg in msgs {
            apply_server_message(game, msg);
        }
    }

    // —— 3. 输入 → 相机朝向 + 物理 + 动作 ——
    let (camera_pos, view_proj, fps_display, mesh_budget, current_hit_pos) = {
        let mut a = app.borrow_mut();
        let fps_display = a.fps_display;
        let input_rc = a.input.clone();
        let Some(game) = a.game.as_mut() else {
            return Ok(());
        };
        game.camera.aspect = cw as f32 / ch.max(1) as f32;

        let mut input = input_rc.borrow_mut();

        // 鼠标转向
        if input.pointer_locked && (input.mouse_dx != 0.0 || input.mouse_dy != 0.0) {
            game.camera.apply_mouse(
                input.mouse_dx,
                input.mouse_dy,
                game.settings.mouse_sensitivity,
            );
        }

        // Hotbar 切换
        if let Some(idx) = input.hotbar_request.take() {
            game.hotbar.select(idx);
        }

        // 双击空格切换 Fly/Walk
        if input.fly_toggle_pending {
            game.physics.toggle_mode();
            log::info!("模式切换 → {:?}", game.physics.mode);
        }

        // 物理 step（指针锁定状态下才接受 WASD 输入；否则只跑重力）
        let world_ref = game.server.clone();
        {
            let server_borrow = world_ref.borrow();
            let getter = |x: i32, y: i32, z: i32| server_borrow.world.get_block_world(x, y, z);
            // 不锁定时不接受方向输入，避免后台移动；但仍然跑物理（让重力工作）
            if input.pointer_locked {
                game.physics.step(&getter, &game.camera, &input, dt);
            } else {
                let neutral = neutral_input(&input);
                game.physics.step(&getter, &game.camera, &neutral, dt);
            }
        }
        game.camera.position = game.physics.eye_position();

        // 60Hz 逻辑帧
        game.frame_clock.accumulate(dt);
        let mut steps_consumed: u32 = 0;
        while game.frame_clock.consume_logic_step() {
            game.server.borrow_mut().tick();
            steps_consumed += 1;
        }

        // 每个 logic step 上报一条 PlayerInput（Phase 3：让 server 知道玩家位置以做范围/重叠校验）
        if steps_consumed > 0 && game.entity_id != 0 {
            let tick = game.server.borrow().tick;
            game.net.send_client_message(ClientMessage::PlayerInput {
                tick,
                position: game.physics.feet_position,
                yaw: game.camera.yaw,
                pitch: game.camera.pitch,
            });
        }

        // DDA 射线检测（每帧）
        let hit = {
            let server_borrow = world_ref.borrow();
            let getter = |x: i32, y: i32, z: i32| server_borrow.world.get_block_world(x, y, z);
            raycast(
                game.camera.position,
                game.camera.forward(),
                MAX_REACH,
                &getter,
            )
        };
        game.current_hit = hit;

        // 挖放动作（仅在指针锁定时启用，防止 lobby/UI 误触）
        if input.pointer_locked {
            dispatch_actions(game, &input);
        }

        // 帧末清掉边沿
        input.reset_delta();
        drop(input);

        (
            game.camera.position,
            game.camera.vp_matrix(),
            fps_display,
            game.settings.mesh_budget_ms,
            game.current_hit.map(|h| h.pos),
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
        let game = a.game.as_ref();
        let yaw_deg = game.map(|g| g.camera.yaw.to_degrees()).unwrap_or(0.0);
        let pitch_deg = game.map(|g| g.camera.pitch.to_degrees()).unwrap_or(0.0);
        let pos = game.map(|g| g.camera.position).unwrap_or_default();
        let loaded_chunks = game.map(|g| g.chunk_loader.loaded.len()).unwrap_or(0);
        let mesh_pending = game.map(|g| g.mesh_jobs.len()).unwrap_or(0);
        let mode = game.map(|g| g.physics.mode).unwrap_or(CameraMode::Walk);
        let on_ground = game.map(|g| g.physics.on_ground).unwrap_or(false);
        let hotbar_items = game.map(|g| g.hotbar.items).unwrap_or([BlockID::AIR; 9]);
        let hotbar_selected = game.map(|g| g.hotbar.selected).unwrap_or(0);
        let full_output = a.egui_ctx.run_ui(raw_input, |ui| {
            draw_hud(
                ui.ctx(),
                HudData {
                    fps: fps_display,
                    pos: (pos.x, pos.y, pos.z),
                    yaw_deg,
                    pitch_deg,
                    pointer_locked,
                    loaded_chunks,
                    mesh_pending,
                    mode,
                    on_ground,
                    hotbar_items,
                    hotbar_selected,
                },
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

        // 选中方块线框（命中时）
        a.renderer
            .render_selection(&mut encoder, &view, view_proj, current_hit_pos);

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

/// 把方向输入清零的输入快照副本（用于失去指针锁时仍跑物理但不响应方向）。
fn neutral_input(orig: &InputState) -> InputState {
    let mut n = orig.clone();
    n.forward = false;
    n.backward = false;
    n.left = false;
    n.right = false;
    n.jump_held = false;
    n.jump_just_pressed = false;
    n.sneak = false;
    n
}

/// 鼠标左键挖、右键放：检查冷却 + 边沿 + raycast 命中，构造乐观更新 + 发消息。
fn dispatch_actions(game: &mut Game, input: &InputState) {
    let now = now_ms();
    let cooldown = game.settings.min_action_interval_ms;

    // —— 挖（连续触发：held + 冷却）——
    if input.break_held
        && now - game.last_break_at_ms >= cooldown
        && let Some(hit) = game.current_hit
    {
        let pos = hit.pos;
        let backup = {
            let server = game.server.borrow();
            server.world.get_block(pos)
        };
        // Local 模式 client 和 server 共享同一份 world，乐观更新会干扰 server 校验
        // （server 读回 AIR 误判 BlockNotEmpty 拒绝）。因此跳过乐观 set_block；
        // BlockUpdate 返回后再重 mesh。Phase 5 Remote 端加独立 WorldView 时再加乐观路径。
        let request_id = game.pending.next_request_id();
        game.pending.insert(
            request_id,
            PendingAction {
                kind: PendingKind::Break,
                pos,
                backup,
            },
        );
        game.net
            .send_client_message(ClientMessage::Break { pos, request_id });
        game.last_break_at_ms = now;
    }

    // —— 放（一次性：just_pressed）——
    if input.place_just_pressed
        && let Some(hit) = game.current_hit
    {
        let neighbor = Position::new(
            hit.pos.x + hit.normal.x,
            hit.pos.y + hit.normal.y,
            hit.pos.z + hit.normal.z,
        );
        let block = game.hotbar.current();
        // 本地预检：放置位置与玩家 AABB 重叠 → 拒绝
        let block_aabb = voxweb_core::Aabb::block_at(neighbor);
        let player_box = voxweb_core::player_aabb(game.physics.feet_position);
        if player_box.intersects(&block_aabb) {
            log::info!("放置位置与玩家重叠，本地拒绝");
            return;
        }
        let backup = {
            let server = game.server.borrow();
            server.world.get_block(neighbor)
        };
        if backup != BlockID::AIR {
            return;
        }
        let request_id = game.pending.next_request_id();
        game.pending.insert(
            request_id,
            PendingAction {
                kind: PendingKind::Place(block),
                pos: neighbor,
                backup,
            },
        );
        game.net.send_client_message(ClientMessage::Place {
            pos: neighbor,
            block,
            request_id,
        });
    }
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
        ServerMessage::ActionAck {
            request_id,
            accepted,
            reason,
        } => {
            if let Some(rolled) = game.pending.resolve(request_id, accepted) {
                // server 拒绝 → 写回 backup + 重 mesh
                log::warn!(
                    "ActionAck rejected: id={request_id} reason={reason:?} pos={:?}",
                    rolled.pos
                );
                game.server
                    .borrow_mut()
                    .world
                    .set_block(rolled.pos, rolled.backup);
                for cp in affected_chunks(rolled.pos) {
                    game.mesh_jobs.enqueue(cp, MeshPriority::High);
                }
            }
        }
        ServerMessage::BlockUpdate { pos, .. } => {
            // Local 模式 server 已经写过 world；这里只需重 mesh 受影响 chunk。
            // Phase 5 远端 BlockUpdate 也走这条路径，到时 world 由这里同步。
            for cp in affected_chunks(pos) {
                game.mesh_jobs.enqueue(cp, MeshPriority::High);
            }
        }
        _ => {
            // Phase 5+ 才处理 PlayerTick / PeerJoined / Chat 等
        }
    }
}

// ============================================================
// HUD（egui）
// ============================================================

struct HudData {
    fps: f32,
    pos: (f32, f32, f32),
    yaw_deg: f32,
    pitch_deg: f32,
    pointer_locked: bool,
    loaded_chunks: usize,
    mesh_pending: usize,
    mode: CameraMode,
    on_ground: bool,
    hotbar_items: [BlockID; 9],
    hotbar_selected: usize,
}

fn draw_hud(ctx: &egui::Context, data: HudData) {
    // 左上角 stat
    egui::Area::new(egui::Id::new("hud_topleft"))
        .anchor(egui::Align2::LEFT_TOP, egui::vec2(12.0, 12.0))
        .show(ctx, |ui| {
            egui::Frame::default()
                .fill(egui::Color32::from_rgba_unmultiplied(0, 0, 0, 140))
                .inner_margin(egui::Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 230, 235),
                        format!("FPS  {:>5.1}", data.fps),
                    );
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 230, 235),
                        format!(
                            "POS  x {:+8.2}  y {:+8.2}  z {:+8.2}",
                            data.pos.0, data.pos.1, data.pos.2
                        ),
                    );
                    ui.colored_label(
                        egui::Color32::from_rgb(180, 190, 200),
                        format!("YAW {:+6.1}°  PITCH {:+5.1}°", data.yaw_deg, data.pitch_deg),
                    );
                    let mode_str = match data.mode {
                        CameraMode::Walk => "Walk",
                        CameraMode::Fly => "Fly",
                    };
                    ui.colored_label(
                        egui::Color32::from_rgb(180, 200, 180),
                        format!(
                            "MODE {}  {}",
                            mode_str,
                            if data.on_ground { "[ground]" } else { "" }
                        ),
                    );
                    ui.colored_label(
                        egui::Color32::from_rgb(160, 175, 190),
                        format!(
                            "CHUNKS {}  MESH_Q {}",
                            data.loaded_chunks, data.mesh_pending
                        ),
                    );
                });
        });

    // 准星
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

    // 提示栏
    egui::Area::new(egui::Id::new("hud_hint"))
        .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -80.0))
        .show(ctx, |ui| {
            let msg = if data.pointer_locked {
                "WASD walk | Space jump (×2 = fly) | LMB break | RMB place | 1-9 hotbar | ESC release"
            } else {
                "Click to enter camera control"
            };
            egui::Frame::default()
                .fill(egui::Color32::from_rgba_unmultiplied(0, 0, 0, 110))
                .inner_margin(egui::Margin::symmetric(14, 6))
                .show(ui, |ui| {
                    // 用 Extend 模式强制按文本自然宽度撑开，避免暂停→返回时上一帧
                    // 残留的小 available_width 让长提示被换行/截断
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(msg)
                                .color(egui::Color32::from_rgb(230, 235, 240)),
                        )
                        .wrap_mode(egui::TextWrapMode::Extend),
                    );
                });
        });

    // Hotbar：屏幕底部居中的 9 格
    egui::Area::new(egui::Id::new("hud_hotbar"))
        .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -16.0))
        .show(ctx, |ui| {
            egui::Frame::default()
                .fill(egui::Color32::from_rgba_unmultiplied(0, 0, 0, 140))
                .inner_margin(egui::Margin::symmetric(8, 6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        for (i, block) in data.hotbar_items.iter().enumerate() {
                            let selected = i == data.hotbar_selected;
                            let label = crate::hotbar::block_label(*block);
                            let txt = format!("{}\n{}", i + 1, label);
                            let bg = if selected {
                                egui::Color32::from_rgba_unmultiplied(240, 200, 80, 220)
                            } else {
                                egui::Color32::from_rgba_unmultiplied(60, 70, 80, 200)
                            };
                            let fg = if selected {
                                egui::Color32::BLACK
                            } else {
                                egui::Color32::from_rgb(230, 235, 240)
                            };
                            // 用 allocate_ui_with_layout 显式给每格固定 54×36 的空间，
                            // 避免在 horizontal 内嵌套 vertical_centered 时取走全部可用宽度
                            // 导致 9 个格子全部叠在同一位置只显示一个的现象。
                            egui::Frame::default()
                                .fill(bg)
                                .inner_margin(egui::Margin::symmetric(8, 4))
                                .show(ui, |ui| {
                                    ui.allocate_ui_with_layout(
                                        egui::vec2(54.0, 36.0),
                                        egui::Layout::top_down(egui::Align::Center),
                                        |ui| {
                                            ui.colored_label(
                                                fg,
                                                egui::RichText::new(&txt).size(11.0),
                                            );
                                        },
                                    );
                                });
                        }
                    });
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
