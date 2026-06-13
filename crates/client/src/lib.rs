//! VoxWeb 客户端入口（cdylib）。
//!
//! Phase 3：
//! - InGame：物理（Walk/Fly）、DDA 射线、挖放动作、Hotbar、选中线框、ActionAck rollback、PlayerInput 上报。
//! - 主循环按 AppState 分流：Lobby（仅 egui） / InGame（完整 server tick + 物理 + 网格化 + 渲染）。

pub mod app;
pub mod camera;
pub mod chat;
pub mod chunk_assembler;
pub mod chunk_loader;
pub mod hotbar;
pub mod input;
pub mod interp;
pub mod mesh_jobs;
pub mod physics;
pub mod prediction;
pub mod raycast;
pub mod settings_storage;
pub mod storage;
pub mod ui;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

use voxweb_core::block::BlockID;
use voxweb_core::chunk::{ChunkPos, Position};
use voxweb_core::protocol::{
    ClientMessage, EntityId, PROTOCOL_VERSION, PlayerEntry, RoomEvent, ServerMessage,
};
use voxweb_render::Renderer;

use crate::app::{
    AppState, BASE_SENSITIVITY_RAD_PER_PIXEL, Game, GameMode, PreloadState, RemotePlayerState,
};
use crate::camera::CameraMode;
use crate::chunk_loader::{affected_chunks, chunk_pos_of};
use crate::input::InputState;
use crate::mesh_jobs::{MeshPriority, MeshRunStats};
use crate::prediction::{
    PendingAction, PendingKind, ReconcileResult, apply_pending_position_correction, reconcile_self,
};
use crate::raycast::raycast;
use crate::storage::{OpfsStorage, WorldStorage};
use crate::ui::lobby::{
    ConnectingAction, LobbyAction, LobbyState, draw_connecting, draw_lobby, generate_room_id,
    validate_room_id,
};

/// 玩家眼睛到目标方块的最大射程（与 server::physics::MAX_REACH 对齐）。
const MAX_REACH: f32 = 6.0;

/// Ping 间隔（毫秒）。
const PING_INTERVAL_MS: f64 = 5000.0;

/// 普通自动保存间隔（毫秒）。退出 / 手动保存有单独快路径。
const AUTO_SAVE_INTERVAL_MS: f64 = 3000.0;
const AUTO_SAVE_BATCH_CHUNKS: usize = 4;
const SAVE_NOW_BATCH_CHUNKS: usize = 16;

/// Pong 校时样本权重。Pong 带 RTT 信息，可信度高于裸 PlayerTick。
const CLOCK_PONG_ALPHA: f64 = 0.2;
/// PlayerTick 校时样本权重。它可能受单向排队延迟影响，只作为低权重补充。
const CLOCK_TICK_ALPHA: f64 = 0.05;

/// 信令服务 URL meta tag 名称。
const SIGNALING_META_NAME: &str = "signaling-url";

#[derive(Clone, Copy, Debug, Default)]
struct FramePerfStats {
    mesh_ms: f32,
    mesh_jobs: u32,
    mesh_vertices: u32,
    mesh_indices: u32,
    mesh_phase2_vertices: u32,
    world_pass_ms: f32,
    depth_pass_ms: f32,
    transparent_pass_ms: f32,
    player_pass_ms: f32,
    selection_pass_ms: f32,
    egui_pass_ms: f32,
    visible_chunks: usize,
    culled_chunks: usize,
    drawn_vertices: u32,
    drawn_indices: u32,
}

impl FramePerfStats {
    fn record_mesh(&mut self, stats: MeshRunStats) {
        self.mesh_ms = stats.elapsed_ms;
        self.mesh_jobs = stats.jobs_processed;
        self.mesh_vertices = stats.vertices_uploaded;
        self.mesh_indices = stats.indices_uploaded;
        self.mesh_phase2_vertices = stats.phase2_vertices;
    }

    fn mesh_reduction_percent(&self) -> Option<f32> {
        if self.mesh_phase2_vertices == 0 {
            return None;
        }
        Some(
            ((1.0 - self.mesh_vertices as f32 / self.mesh_phase2_vertices as f32) * 100.0).max(0.0),
        )
    }
}

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
    /// 当前正在 / 已 / 失败的连接尝试的房间号 & 模式（仅 Host/Remote 时有效）。
    connecting_mode: GameMode,
    connecting_room_id: String,
    connecting_error: Option<String>,
    /// 断开后的提示语，配合 AppState::Disconnected 显示。
    disconnect_reason: Option<String>,

    game: Option<Game>,
    /// 当前世界会话编号。进入 / 离开世界时递增，异步存档回写前用它识别过期结果。
    world_session_id: u64,

    /// 上一帧 performance.now()（毫秒）
    last_time_ms: f64,
    /// FPS 滑动平均
    fps_frames: u32,
    fps_accum: f32,
    fps_display: f32,

    /// 标志：下次 InGame 渲染前请求一次指针锁（点击 Lobby 按钮触发）
    request_pointer_lock_next: bool,

    /// 区块预载状态（Connecting 状态下使用，网络协商完成后开始加载出生点周围区块）。
    preload_state: Option<PreloadState>,

    /// 已被升级为信令中继的 peer 集合。仅供 UI 在玩家名牌 / 列表里加「中继中」徽标。
    relayed_peers: HashSet<u32>,

    /// Phase 7：上一帧的 CPU pass / 网格化统计，用于 HUD。
    perf: FramePerfStats,

    /// 游戏内通知队列（timestamp_ms, 消息）。用于在 InGame 状态下显示信令错误等浮窗提示。
    notifications: Vec<(f64, String)>,
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

    // ?room=abc123 → 预填 Lobby 输入框，方便分享链接直接落到正确的房间号
    let mut lobby_state = LobbyState::default();
    if let Some(initial_room) = read_query_param("room")
        && !initial_room.is_empty()
    {
        lobby_state.room_id_input = initial_room;
    }

    let app = Rc::new(RefCell::new(App {
        canvas: canvas.clone(),
        renderer,
        egui_ctx,
        egui_renderer,
        input: input.clone(),
        egui_events: egui_events.clone(),
        state: AppState::Lobby,
        lobby_state,
        connecting_mode: GameMode::Local,
        connecting_room_id: String::new(),
        connecting_error: None,
        disconnect_reason: None,
        game: None,
        world_session_id: 0,
        last_time_ms: now_ms(),
        fps_frames: 0,
        fps_accum: 0.0,
        fps_display: 0.0,
        request_pointer_lock_next: false,
        preload_state: None,
        relayed_peers: HashSet::new(),
        perf: FramePerfStats::default(),
        notifications: Vec::new(),
    }));

    install_event_listeners(&canvas, &document, input.clone(), egui_events, app.clone())?;
    install_debug_hooks(app.clone());
    spawn_raf_loop(app);

    Ok(())
}

/// 递增世界会话编号。所有跨 await 的世界相关异步任务都应捕获编号，回写前再次比对。
fn bump_world_session(a: &mut App) -> u64 {
    a.world_session_id = a.world_session_id.wrapping_add(1);
    a.world_session_id
}

/// 清掉当前世界运行时资源。Renderer 是跨状态常驻对象，必须显式清空世界 GPU 缓存。
fn clear_world_runtime(a: &mut App) -> u64 {
    let session_id = bump_world_session(a);
    a.game = None;
    a.preload_state = None;
    a.request_pointer_lock_next = false;
    a.relayed_peers.clear();
    a.notifications.clear();
    a.perf = FramePerfStats::default();
    a.egui_events.borrow_mut().clear();
    a.renderer.clear_world_cache();
    a.input.borrow_mut().clear_held();
    if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
        doc.exit_pointer_lock();
    }
    session_id
}

/// 开始新世界前的统一入口：先结束旧会话，再返回新会话编号给异步存档任务使用。
fn prepare_world_start(a: &mut App) -> u64 {
    let session_id = clear_world_runtime(a);
    a.disconnect_reason = None;
    a.connecting_error = None;
    session_id
}

/// 回大厅时让下一帧 Lobby 重新读取 OPFS 世界列表，避免显示退出前的旧缓存。
fn mark_lobby_saves_stale(a: &mut App) {
    a.lobby_state.saves_loaded = false;
    a.lobby_state.saves_loading = false;
    a.lobby_state.saved_worlds.clear();
    a.lobby_state.selected_save = None;
}

/// 所有“返回大厅”路径的统一收口。
fn return_to_lobby(a: &mut App) {
    clear_world_runtime(a);
    a.state = AppState::Lobby;
    a.disconnect_reason = None;
    a.connecting_error = None;
    a.connecting_room_id.clear();
    a.connecting_mode = GameMode::Local;
    mark_lobby_saves_stale(a);
}

fn push_notification(a: &mut App, message: impl Into<String>) {
    a.notifications.push((now_ms(), message.into()));
    if a.notifications.len() > 8 {
        a.notifications.remove(0);
    }
}

fn apply_persisted_accounting(
    game: &mut Game,
    encoded_sizes: &[(ChunkPos, u64)],
    refreshed_quota: Option<crate::storage::QuotaInfo>,
) {
    for (pos, size) in encoded_sizes.iter().copied() {
        game.known_persisted.insert(pos);
        game.loaded_persisted_chunks.insert(pos);
        let old_size = game.persisted_chunk_sizes.insert(pos, size).unwrap_or(0);
        if size >= old_size {
            game.current_world_bytes = game.current_world_bytes.saturating_add(size - old_size);
        } else {
            game.current_world_bytes = game.current_world_bytes.saturating_sub(old_size - size);
        }
    }
    if let Some(quota) = refreshed_quota {
        game.quota = Some(quota);
    }
}

fn flush_dirty_best_effort(app: &Rc<RefCell<App>>, reason: &'static str) {
    let maybe_job = {
        let mut a = app.borrow_mut();
        let session_id = a.world_session_id;
        let Some(g) = a.game.as_mut() else {
            return;
        };
        if matches!(g.mode, GameMode::Remote) {
            return;
        }
        let Some(storage) = g.storage.clone() else {
            return;
        };
        let tick = g.server.borrow().world.tick_count;
        let snapshot_positions = g.server.borrow_mut().world.snapshot_dirty(usize::MAX, tick);
        if snapshot_positions.is_empty() {
            return;
        }

        let server = g.server.clone();
        let mut encoded = Vec::new();
        let mut encoded_sizes = Vec::new();
        {
            let server_ref = server.borrow();
            for pos in &snapshot_positions {
                if let Some(chunk) = server_ref.world.chunks.get(pos) {
                    let bytes = voxweb_core::chunk::encode(chunk);
                    encoded_sizes.push((*pos, bytes.len() as u64));
                    encoded.push((*pos, bytes));
                }
            }
        }

        let positions: Vec<_> = encoded.iter().map(|(pos, _)| *pos).collect();
        let position_set: HashSet<_> = positions.iter().copied().collect();
        let missing_positions: Vec<_> = snapshot_positions
            .iter()
            .copied()
            .filter(|pos| !position_set.contains(pos))
            .collect();
        if !missing_positions.is_empty() {
            log::warn!(
                "[storage] {reason}: {} dirty chunks were missing from memory",
                missing_positions.len()
            );
            server.borrow_mut().world.commit_flushed(&missing_positions);
        }
        if encoded.is_empty() {
            return;
        }

        log::info!(
            "[storage] {reason}: best-effort flush {} chunks",
            encoded.len()
        );
        Some((
            storage,
            server,
            positions,
            encoded,
            encoded_sizes,
            tick,
            session_id,
        ))
    };

    let Some((storage, server, positions, encoded, encoded_sizes, tick, session_id)) = maybe_job
    else {
        return;
    };
    let app_ref = app.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let result = storage.save_chunks(encoded).await;
        let refreshed_quota = if result.is_ok() {
            storage.quota().await
        } else {
            None
        };
        let mut s = server.borrow_mut();
        match result {
            Ok(()) => {
                s.world.commit_flushed(&positions);
                drop(s);
                let mut a = app_ref.borrow_mut();
                if a.world_session_id == session_id
                    && let Some(g) = a.game.as_mut()
                {
                    apply_persisted_accounting(g, &encoded_sizes, refreshed_quota);
                }
            }
            Err(e) => {
                log::warn!("[storage] {reason} flush failed: {e:?}");
                s.world.record_flush_failure(&positions, tick);
            }
        }
    });
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
    // —— 点击 canvas → 请求指针锁（仅在 InGame 且无暂停/聊天叠加层时）——
    {
        let canvas_clone = canvas.clone();
        let app_clone = app.clone();
        let on_click = Closure::<dyn FnMut(_)>::new(move |_e: web_sys::MouseEvent| {
            if matches!(
                app_clone.borrow().state,
                AppState::InGame {
                    paused: false,
                    chat_open: false
                }
            ) {
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
        let app_clone = app.clone();
        let on_lock_change = Closure::<dyn FnMut()>::new(move || {
            let locked = document_clone
                .pointer_lock_element()
                .map(|el| el == *canvas_id.as_ref())
                .unwrap_or(false);
            let mut s = input_clone.borrow_mut();
            let was_locked = s.pointer_locked;
            if was_locked != locked {
                s.clear_held();
                // 当指针锁因为用户按 ESC 而无预期释放时（纯游戏态、未暂停未聊天），
                // 浏览器可能吞掉 ESC keydown 事件，导致 esc_menu 边沿永远不会被设。
                // 这里从 pointerlockchange 补设 esc_menu，保证暂停菜单能正常弹出。
                if !locked && was_locked {
                    let a = app_clone.borrow();
                    if matches!(
                        a.state,
                        AppState::InGame {
                            paused: false,
                            chat_open: false
                        }
                    ) {
                        drop(a);
                        s.esc_menu = true;
                    }
                }
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
    // 活跃游戏（InGame 且未暂停未聊天）：用 e.code() 映射物理键到 KeyCode 给物理/相机/hotbar；
    // 其它状态（Lobby/Connecting/InGame 暂停或聊天聚焦）：用 e.key() 转 egui::Event::Text / Event::Key，
    // 让 TextEdit 收到输入。
    {
        let input_clone = input.clone();
        let app_clone = app.clone();
        let egui_events_clone = egui_events.clone();
        let on_keydown = Closure::<dyn FnMut(_)>::new(move |e: web_sys::KeyboardEvent| {
            let forward_to_egui = !matches!(
                app_clone.borrow().state,
                AppState::InGame {
                    paused: false,
                    chat_open: false
                }
            );
            if forward_to_egui {
                forward_keydown_to_egui(&e, &egui_events_clone);
                // 注意：即便已转给 egui，依然让 InputState 接到边沿事件（ESC/T 等），
                // 让主循环能消费这些 edge-trigger 字段切换 paused / chat_open。
                if let Some(key) = map_key(&e.code()) {
                    input_clone.borrow_mut().on_key_down(key, now_ms());
                }
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
        let egui_events_clone = egui_events.clone();
        let on_keyup = Closure::<dyn FnMut(_)>::new(move |e: web_sys::KeyboardEvent| {
            let forward_to_egui = !matches!(
                app_clone.borrow().state,
                AppState::InGame {
                    paused: false,
                    chat_open: false
                }
            );
            if forward_to_egui {
                forward_keyup_to_egui(&e, &egui_events_clone);
                if let Some(key) = map_key(&e.code()) {
                    input_clone.borrow_mut().on_key_up(key);
                }
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
            let is_ingame_active = matches!(
                app_clone.borrow().state,
                AppState::InGame {
                    paused: false,
                    chat_open: false
                }
            );
            if is_ingame_active {
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
            let is_ingame_active = matches!(
                app_clone.borrow().state,
                AppState::InGame {
                    paused: false,
                    chat_open: false
                }
            );
            if is_ingame_active {
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
            if matches!(
                app_clone.borrow().state,
                AppState::InGame {
                    paused: false,
                    chat_open: false
                }
            ) {
                e.prevent_default();
            }
        });
        canvas.add_event_listener_with_callback(
            "contextmenu",
            on_contextmenu.as_ref().unchecked_ref(),
        )?;
        on_contextmenu.forget();
    }

    // —— 页面离开 / 进入 BFCache 前尽力保存剩余 dirty chunk ——
    if let Some(window) = web_sys::window() {
        let app_clone = app.clone();
        let on_pagehide = Closure::<dyn FnMut()>::new(move || {
            flush_dirty_best_effort(&app_clone, "pagehide");
        });
        window
            .add_event_listener_with_callback("pagehide", on_pagehide.as_ref().unchecked_ref())?;
        on_pagehide.forget();
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

/// 在 Lobby / Connecting 等 InGame 之外的状态下，把 keydown 事件转 egui Event。
/// - 可识别的功能键（Backspace / Enter / 箭头键 / Tab / Esc / Home / End …）→ Event::Key{pressed=true}
/// - 单字符 + 无 Ctrl/Alt/Meta → Event::Text，让 TextEdit 接收
fn forward_keydown_to_egui(
    e: &web_sys::KeyboardEvent,
    egui_events: &Rc<RefCell<Vec<egui::Event>>>,
) {
    let modifiers = egui::Modifiers {
        alt: e.alt_key(),
        ctrl: e.ctrl_key(),
        shift: e.shift_key(),
        mac_cmd: e.meta_key(),
        command: e.ctrl_key() || e.meta_key(),
    };
    let key_str = e.key();

    if let Some(egui_key) = map_web_key_to_egui(&key_str) {
        egui_events.borrow_mut().push(egui::Event::Key {
            key: egui_key,
            physical_key: None,
            pressed: true,
            repeat: e.repeat(),
            modifiers,
        });
        // 阻止浏览器默认行为：Tab 切换焦点、Backspace 后退、空格滚动等
        if matches!(
            egui_key,
            egui::Key::Backspace
                | egui::Key::Tab
                | egui::Key::ArrowUp
                | egui::Key::ArrowDown
                | egui::Key::ArrowLeft
                | egui::Key::ArrowRight
                | egui::Key::Space
        ) {
            e.prevent_default();
        }
        // egui 的 TextEdit 依赖 Text 事件插入字符；Space 同时也是功能键，
        // 因此需要在没有组合键修饰时额外补一个文本空格，聊天输入框才能输入空格。
        if egui_key == egui::Key::Space && !modifiers.ctrl && !modifiers.alt && !modifiers.mac_cmd {
            egui_events
                .borrow_mut()
                .push(egui::Event::Text(" ".to_string()));
        }
    } else if key_str.chars().count() == 1
        && !modifiers.ctrl
        && !modifiers.alt
        && !modifiers.mac_cmd
    {
        // 单个可见字符：作为文本输入
        let c = key_str.chars().next().unwrap();
        if !c.is_control() {
            egui_events.borrow_mut().push(egui::Event::Text(key_str));
        }
    }
}

/// 同上的 keyup 版本：只发 Key{pressed=false}（egui 用它跟踪按住状态）。
fn forward_keyup_to_egui(e: &web_sys::KeyboardEvent, egui_events: &Rc<RefCell<Vec<egui::Event>>>) {
    let modifiers = egui::Modifiers {
        alt: e.alt_key(),
        ctrl: e.ctrl_key(),
        shift: e.shift_key(),
        mac_cmd: e.meta_key(),
        command: e.ctrl_key() || e.meta_key(),
    };
    if let Some(egui_key) = map_web_key_to_egui(&e.key()) {
        egui_events.borrow_mut().push(egui::Event::Key {
            key: egui_key,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers,
        });
    }
}

/// `KeyboardEvent.key`（如 "Backspace", "ArrowLeft"）→ egui::Key。
/// 单字符键（如 "a", "1"）返回 None，由 Text 事件处理。
fn map_web_key_to_egui(key: &str) -> Option<egui::Key> {
    use egui::Key;
    Some(match key {
        "Backspace" => Key::Backspace,
        "Delete" => Key::Delete,
        "Enter" => Key::Enter,
        "Tab" => Key::Tab,
        "Escape" => Key::Escape,
        "ArrowLeft" => Key::ArrowLeft,
        "ArrowRight" => Key::ArrowRight,
        "ArrowUp" => Key::ArrowUp,
        "ArrowDown" => Key::ArrowDown,
        "Home" => Key::Home,
        "End" => Key::End,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        " " => Key::Space,
        _ => return None,
    })
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
        AppState::InGame { paused, chat_open } => {
            render_game_frame(app, dt, cw, ch, paused, chat_open)
        }
        AppState::Connecting => render_connecting_frame(app, cw, ch),
        AppState::Disconnected => render_disconnected_frame(app, cw, ch),
        // Loading / Lobby 走大厅
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
    // —— 异步加载存档列表（仅首次进入 Lobby 时触发）——
    {
        let mut a = app.borrow_mut();
        if !a.lobby_state.saves_loaded && !a.lobby_state.saves_loading {
            a.lobby_state.saves_loading = true;
            let app_ref = app.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = crate::storage::storage_overview().await;
                let mut a = app_ref.borrow_mut();
                a.lobby_state.saves_loading = false;
                a.lobby_state.saves_loaded = true;
                match result {
                    Ok(overview) => {
                        a.lobby_state.saved_worlds = overview.worlds;
                        a.lobby_state.storage_quota = overview.quota;
                    }
                    Err(e) => {
                        log::warn!("[lobby] 加载存档列表失败: {e:?}");
                        a.lobby_state.error_message = Some(format!("Failed to load saves: {e:?}"));
                    }
                }
            });
        }
    }

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

    // —— 处理动作 ——
    // 获取当前选中的存档 key
    let selected_save_key = app.borrow().lobby_state.selected_save.clone();

    match action {
        Some(LobbyAction::StartSinglePlayer { seed, display_name }) => {
            start_single_player(app, seed, &display_name, selected_save_key.as_deref());
        }
        Some(LobbyAction::CreateRoom {
            room_id,
            seed,
            display_name,
        }) => {
            // 空房间号 → 自动生成 6 位
            let final_room = if room_id.is_empty() {
                let g = generate_room_id();
                {
                    let mut a = app.borrow_mut();
                    a.lobby_state.room_id_input = g.clone();
                    a.lobby_state.last_generated_room = Some(g.clone());
                }
                g
            } else {
                match validate_room_id(&room_id) {
                    Ok(()) => {
                        {
                            let mut a = app.borrow_mut();
                            a.lobby_state.last_generated_room = None;
                        }
                        room_id
                    }
                    Err(err) => {
                        app.borrow_mut().lobby_state.error_message = Some(err);
                        return Ok(());
                    }
                }
            };
            start_host(
                app,
                &final_room,
                seed,
                &display_name,
                selected_save_key.as_deref(),
            );
        }
        Some(LobbyAction::JoinRoom {
            room_id,
            display_name,
        }) => {
            // validate_room_id 已在 UI 内做过；这里再保底
            if let Err(e) = validate_room_id(&room_id) {
                app.borrow_mut().lobby_state.error_message = Some(e);
                return Ok(());
            }
            start_remote(app, &room_id, &display_name);
        }
        Some(LobbyAction::SelectSave { key }) => {
            app.borrow_mut().lobby_state.selected_save = key;
        }
        Some(LobbyAction::DeleteSave { key }) => {
            let app_ref = app.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(e) = crate::storage::delete_world_by_key(&key).await {
                    log::warn!("[lobby] 删除存档失败: {e:?}");
                    let mut a = app_ref.borrow_mut();
                    a.lobby_state.error_message = Some(format!("Failed to delete save: {e:?}"));
                    return;
                }
                // 刷新列表
                let result = crate::storage::storage_overview().await;
                let mut a = app_ref.borrow_mut();
                match result {
                    Ok(overview) => {
                        a.lobby_state.saved_worlds = overview.worlds;
                        a.lobby_state.storage_quota = overview.quota;
                        a.lobby_state.selected_save = None; // 重置选择
                    }
                    Err(e) => {
                        log::warn!("[lobby] 刷新存档列表失败: {e:?}");
                        a.lobby_state.error_message =
                            Some(format!("Failed to refresh saves: {e:?}"));
                    }
                }
            });
        }
        Some(LobbyAction::RefreshSaves) => {
            let app_ref = app.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = crate::storage::storage_overview().await;
                let mut a = app_ref.borrow_mut();
                match result {
                    Ok(overview) => {
                        a.lobby_state.saved_worlds = overview.worlds;
                        a.lobby_state.storage_quota = overview.quota;
                    }
                    Err(e) => {
                        log::warn!("[lobby] 刷新存档列表失败: {e:?}");
                        a.lobby_state.error_message =
                            Some(format!("Failed to refresh saves: {e:?}"));
                    }
                }
            });
        }
        None => {}
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

fn start_single_player(
    app: &Rc<RefCell<App>>,
    seed: Option<u64>,
    display_name: &str,
    save_key: Option<&str>,
) {
    let seed = seed.unwrap_or_else(random_seed);
    log::info!(
        "启动单机游戏，seed = {seed}, display_name = {display_name}, save_key = {save_key:?}"
    );

    let settings = settings_storage::load().unwrap_or_default();
    let mut game = Game::new_local(seed, settings, display_name);

    // 相机 yaw/pitch 用默认值（physics 驱动 position）
    game.camera.position = game.physics.eye_position();
    game.camera.pitch = -0.4;

    let rd = game.settings.render_distance as i32;
    let total = ((2 * rd + 1) * (2 * rd + 1)) as usize;

    let session_id = {
        let mut a = app.borrow_mut();
        let session_id = prepare_world_start(&mut a);
        a.game = Some(game);
        a.connecting_mode = GameMode::Local;
        a.connecting_room_id = String::new();
        a.connecting_error = None;
        a.preload_state = Some(PreloadState {
            total,
            received: 0,
            meshed: 0,
            active: true,
        });
        a.state = AppState::Connecting;
        log::info!("[local] 进入加载界面，开始区块预载 (total={total})");
        session_id
    };

    // 如果有 save_key，使用 open_by_key 加载；否则创建新存档
    if let Some(key) = save_key {
        attach_storage_for_load(app.clone(), key.to_string(), session_id);
    } else {
        attach_storage_for_new(app.clone(), seed, session_id);
    }
}

fn install_debug_hooks(app: Rc<RefCell<App>>) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let obj = js_sys::Object::new();

    let fill_app = app.clone();
    let fill = Closure::wrap(Box::new(move |n: u32| {
        let mut a = fill_app.borrow_mut();
        let Some(g) = a.game.as_mut() else {
            return;
        };
        let count = n.min(20_000);
        for i in 0..count {
            let x = (i as i32 % 160) - 80;
            let z = (i as i32 / 160) - 80;
            let pos = voxweb_core::ChunkPos::new(x, z);
            g.server.borrow_mut().world.ensure_chunk_generated(pos);
            g.server.borrow_mut().world.persistence.mark_dirty(pos);
        }
        log::info!("[debug] filled dirty chunks: {count}");
    }) as Box<dyn FnMut(u32)>);
    let _ = js_sys::Reflect::set(&obj, &"fillDirty".into(), fill.as_ref().unchecked_ref());
    fill.forget();

    let quota = Closure::wrap(Box::new(move || {
        wasm_bindgen_futures::spawn_local(async move {
            let q = crate::storage::quota().await;
            log::info!("[debug] quota: {q:?}");
        });
    }) as Box<dyn FnMut()>);
    let _ = js_sys::Reflect::set(&obj, &"quota".into(), quota.as_ref().unchecked_ref());
    quota.forget();

    let _ = js_sys::Reflect::set(&window, &"voxwebDebug".into(), &obj);
}

/// 用 getrandom 生成一个 u64 随机种子。失败时退化为 0。
fn random_seed() -> u64 {
    let mut buf = [0u8; 8];
    let _ = getrandom::getrandom(&mut buf);
    u64::from_le_bytes(buf)
}

/// 从 `<meta name="signaling-url">` 读取信令服务 URL；
/// 若 URL 携带 `?signaling=...` query 参数，则优先用 query（方便本地开发切换地址）。
/// 未配置时返回 None。
fn signaling_url() -> Option<String> {
    // 1) ?signaling= 优先
    if let Some(from_query) = read_query_param("signaling")
        && !from_query.is_empty()
    {
        return Some(from_query);
    }

    // 2) <meta name="signaling-url">
    let window = web_sys::window()?;
    let document = window.document()?;
    let selector = format!("meta[name=\"{SIGNALING_META_NAME}\"]");
    let el = document.query_selector(&selector).ok()??;
    let meta = el.dyn_into::<web_sys::HtmlMetaElement>().ok()?;
    let content = meta.content();
    if content.is_empty() {
        None
    } else {
        Some(content)
    }
}

/// 读取 `window.location.search` 中的一个 query 参数（已 URL 解码）。
fn read_query_param(key: &str) -> Option<String> {
    let window = web_sys::window()?;
    let search = window.location().search().ok()?;
    let params = web_sys::UrlSearchParams::new_with_str(&search).ok()?;
    params.get(key)
}

/// 用 history.replaceState 在不刷新页面的情况下更新 URL 上的 `?room=` 参数，
/// 保留其它已有 query（如 ?signaling=）。失败静默，不影响功能。
fn set_room_in_url(room_id: &str) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Ok(history) = window.history() else {
        return;
    };
    let search = window.location().search().unwrap_or_default();
    let Ok(params) = web_sys::UrlSearchParams::new_with_str(&search) else {
        return;
    };
    params.set("room", room_id);
    let new_search: String = params.to_string().into();
    let new_url = if new_search.is_empty() {
        "?".to_string()
    } else {
        format!("?{new_search}")
    };
    let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(&new_url));
}

fn start_host(
    app: &Rc<RefCell<App>>,
    room_id: &str,
    seed: Option<u64>,
    display_name: &str,
    save_key: Option<&str>,
) {
    let seed = seed.unwrap_or_else(random_seed);
    let Some(url) = signaling_url() else {
        app.borrow_mut().lobby_state.error_message = Some(
            "Signaling URL not configured (missing ?signaling= param or <meta name=\"signaling-url\">)".into(),
        );
        return;
    };
    log::info!(
        "启动 Host：room={room_id}, signaling={url}, seed={seed}, name={display_name}, save_key={save_key:?}"
    );

    let settings = settings_storage::load().unwrap_or_default();
    let game_result = Game::new_host(seed, settings, &url, room_id, display_name);
    let mut a = app.borrow_mut();
    match game_result {
        Ok(mut game) => {
            // Phase 5：add_player 已在 Game::new_host 内完成，不再发 Hello。
            game.camera.position = game.physics.eye_position();
            game.camera.pitch = -0.4;

            let session_id = prepare_world_start(&mut a);
            a.game = Some(game);
            a.state = AppState::Connecting;
            a.connecting_mode = GameMode::Host;
            a.connecting_room_id = room_id.to_string();
            a.connecting_error = None;
            // 把房间号写回 URL（history.replaceState），方便用户直接复制 URL 分享
            set_room_in_url(room_id);
            drop(a);

            // 如果有 save_key，使用 open_by_key 加载；否则创建新存档
            if let Some(key) = save_key {
                attach_storage_for_load(app.clone(), key.to_string(), session_id);
            } else {
                // Host 新建存档仍由 OPFS 分配 world_key；已有世界通过大厅选中的 key 复用。
                attach_storage_async(app.clone(), room_id.to_string(), seed, session_id);
            }
        }
        Err(e) => {
            log::warn!("[host] new_host failed: {e:?}");
            a.lobby_state.error_message = Some(format!("Failed to host: {e:?}"));
        }
    }
}

fn start_remote(app: &Rc<RefCell<App>>, room_id: &str, display_name: &str) {
    let Some(url) = signaling_url() else {
        app.borrow_mut().lobby_state.error_message = Some(
            "Signaling URL not configured (missing ?signaling= param or <meta name=\"signaling-url\">)".into(),
        );
        return;
    };
    log::info!("启动 Remote：room={room_id}, signaling={url}, name={display_name}");

    let settings = settings_storage::load().unwrap_or_default();
    let display = display_name.to_string();
    let game_result = Game::new_remote(settings, &url, room_id, &display);
    let mut a = app.borrow_mut();
    match game_result {
        Ok(mut game) => {
            // Remote：DC open 后再 flush 出去（NetEndpoint::Remote 的 outbox 暂存）
            // Phase 5：Hello 由 Host 收到时触发 add_player；本端不调 add_player。
            game.net.send_client_message(ClientMessage::Hello {
                display_name: display.clone(),
                version: PROTOCOL_VERSION,
            });
            game.camera.position = game.physics.eye_position();
            game.camera.pitch = -0.4;

            prepare_world_start(&mut a);
            a.game = Some(game);
            a.state = AppState::Connecting;
            a.connecting_mode = GameMode::Remote;
            a.connecting_room_id = room_id.to_string();
            a.connecting_error = None;
            // 与 Host 一致：把当前房间号写回 URL，刷新后仍可重新加入
            set_room_in_url(room_id);
        }
        Err(e) => {
            log::warn!("[remote] new_remote failed: {e:?}");
            a.lobby_state.error_message = Some(format!("Failed to join: {e:?}"));
        }
    }
}

fn attach_storage_async(app: Rc<RefCell<App>>, room_id: String, seed: u64, session_id: u64) {
    wasm_bindgen_futures::spawn_local(async move {
        let _ = OpfsStorage::request_persistence().await;
        match OpfsStorage::open(&room_id, seed).await {
            Ok(storage) => {
                let (chunk_sizes, current_world_bytes, other_worlds_bytes) =
                    storage_size_snapshot(&storage).await;
                let known_set: HashSet<_> = chunk_sizes.keys().copied().collect();
                let spawn = crate::chunk_loader::chunk_pos_of(voxweb_server::DEFAULT_SPAWN);
                let mut loaded = Vec::new();
                for dx in -4..=4 {
                    for dz in -4..=4 {
                        let pos = voxweb_core::ChunkPos::new(spawn.x + dx, spawn.z + dz);
                        if !known_set.contains(&pos) {
                            continue;
                        }
                        match storage.load_chunk(pos).await {
                            Ok(Some(bytes)) => match voxweb_core::chunk::decode(&bytes) {
                                Ok(chunk) => loaded.push((pos, chunk)),
                                Err(e) => log::warn!("[storage] decode {pos:?} failed: {e:?}"),
                            },
                            Ok(None) => {}
                            Err(e) => log::warn!("[storage] load {pos:?} failed: {e:?}"),
                        }
                    }
                }
                let loaded_positions: HashSet<_> = loaded.iter().map(|(pos, _)| *pos).collect();
                let quota = storage.quota().await;
                let mut a = app.borrow_mut();
                if a.world_session_id != session_id {
                    log::debug!("[storage] 丢弃过期 Host 存档加载结果");
                    return;
                }
                if let Some(g) = a.game.as_mut() {
                    for (pos, chunk) in loaded {
                        g.server.borrow_mut().load_chunk_from_storage(pos, chunk);
                        g.mesh_jobs.enqueue(pos, MeshPriority::High);
                    }
                    g.known_persisted = known_set;
                    g.loaded_persisted_chunks = loaded_positions;
                    g.pending_persisted_loads.clear();
                    g.current_world_bytes = current_world_bytes;
                    g.other_worlds_bytes = other_worlds_bytes;
                    g.persisted_chunk_sizes = chunk_sizes;
                    g.quota = quota;
                    g.storage = Some(storage);
                    g.storage_error = None;
                }
            }
            Err(e) => {
                let mut a = app.borrow_mut();
                if a.world_session_id != session_id {
                    return;
                }
                if let Some(g) = a.game.as_mut() {
                    g.storage_error = Some(format!("{e:?}"));
                }
            }
        }
    });
}

/// 创建新存档（用当前时间戳 + seed 生成 key）
fn attach_storage_for_new(app: Rc<RefCell<App>>, seed: u64, session_id: u64) {
    wasm_bindgen_futures::spawn_local(async move {
        let _ = OpfsStorage::request_persistence().await;
        match OpfsStorage::create_new(seed).await {
            Ok(storage) => {
                let (chunk_sizes, current_world_bytes, other_worlds_bytes) =
                    storage_size_snapshot(&storage).await;
                let known_set: HashSet<_> = chunk_sizes.keys().copied().collect();
                let spawn = crate::chunk_loader::chunk_pos_of(voxweb_server::DEFAULT_SPAWN);
                let mut loaded = Vec::new();
                for dx in -4..=4 {
                    for dz in -4..=4 {
                        let pos = voxweb_core::ChunkPos::new(spawn.x + dx, spawn.z + dz);
                        if !known_set.contains(&pos) {
                            continue;
                        }
                        match storage.load_chunk(pos).await {
                            Ok(Some(bytes)) => match voxweb_core::chunk::decode(&bytes) {
                                Ok(chunk) => loaded.push((pos, chunk)),
                                Err(e) => log::warn!("[storage] decode {pos:?} failed: {e:?}"),
                            },
                            Ok(None) => {}
                            Err(e) => log::warn!("[storage] load {pos:?} failed: {e:?}"),
                        }
                    }
                }
                let loaded_positions: HashSet<_> = loaded.iter().map(|(pos, _)| *pos).collect();
                let quota = storage.quota().await;
                let mut a = app.borrow_mut();
                if a.world_session_id != session_id {
                    log::debug!("[storage] 丢弃过期新存档加载结果");
                    return;
                }
                if let Some(g) = a.game.as_mut() {
                    for (pos, chunk) in loaded {
                        g.server.borrow_mut().load_chunk_from_storage(pos, chunk);
                        g.mesh_jobs.enqueue(pos, MeshPriority::High);
                    }
                    g.known_persisted = known_set;
                    g.loaded_persisted_chunks = loaded_positions;
                    g.pending_persisted_loads.clear();
                    g.current_world_bytes = current_world_bytes;
                    g.other_worlds_bytes = other_worlds_bytes;
                    g.persisted_chunk_sizes = chunk_sizes;
                    g.quota = quota;
                    g.storage = Some(storage);
                    g.storage_error = None;
                }
            }
            Err(e) => {
                let mut a = app.borrow_mut();
                if a.world_session_id != session_id {
                    return;
                }
                if let Some(g) = a.game.as_mut() {
                    g.storage_error = Some(format!("{e:?}"));
                }
            }
        }
    });
}

/// 通过 key 加载已有存档
fn attach_storage_for_load(app: Rc<RefCell<App>>, key: String, session_id: u64) {
    wasm_bindgen_futures::spawn_local(async move {
        let _ = OpfsStorage::request_persistence().await;
        match OpfsStorage::open_by_key(&key).await {
            Ok(storage) => {
                let (chunk_sizes, current_world_bytes, other_worlds_bytes) =
                    storage_size_snapshot(&storage).await;
                let known_set: HashSet<_> = chunk_sizes.keys().copied().collect();
                let spawn = crate::chunk_loader::chunk_pos_of(voxweb_server::DEFAULT_SPAWN);
                let mut loaded = Vec::new();
                for dx in -4..=4 {
                    for dz in -4..=4 {
                        let pos = voxweb_core::ChunkPos::new(spawn.x + dx, spawn.z + dz);
                        if !known_set.contains(&pos) {
                            continue;
                        }
                        match storage.load_chunk(pos).await {
                            Ok(Some(bytes)) => match voxweb_core::chunk::decode(&bytes) {
                                Ok(chunk) => loaded.push((pos, chunk)),
                                Err(e) => log::warn!("[storage] decode {pos:?} failed: {e:?}"),
                            },
                            Ok(None) => {}
                            Err(e) => log::warn!("[storage] load {pos:?} failed: {e:?}"),
                        }
                    }
                }
                let loaded_positions: HashSet<_> = loaded.iter().map(|(pos, _)| *pos).collect();
                let quota = storage.quota().await;
                let mut a = app.borrow_mut();
                if a.world_session_id != session_id {
                    log::debug!("[storage] 丢弃过期已有存档加载结果");
                    return;
                }
                if let Some(g) = a.game.as_mut() {
                    for (pos, chunk) in loaded {
                        g.server.borrow_mut().load_chunk_from_storage(pos, chunk);
                        g.mesh_jobs.enqueue(pos, MeshPriority::High);
                    }
                    g.known_persisted = known_set;
                    g.loaded_persisted_chunks = loaded_positions;
                    g.pending_persisted_loads.clear();
                    g.current_world_bytes = current_world_bytes;
                    g.other_worlds_bytes = other_worlds_bytes;
                    g.persisted_chunk_sizes = chunk_sizes;
                    g.quota = quota;
                    g.storage = Some(storage);
                    g.storage_error = None;
                }
            }
            Err(e) => {
                let mut a = app.borrow_mut();
                if a.world_session_id != session_id {
                    return;
                }
                if let Some(g) = a.game.as_mut() {
                    g.storage_error = Some(format!("{e:?}"));
                }
            }
        }
    });
}

async fn storage_size_snapshot(
    storage: &OpfsStorage,
) -> (HashMap<voxweb_core::ChunkPos, u64>, u64, u64) {
    let chunk_sizes = storage.chunk_file_sizes().await.unwrap_or_default();
    let chunk_bytes = chunk_sizes.values().copied().sum::<u64>();
    let record_bytes = storage.world_record_size().await.unwrap_or(0);
    let current_world_bytes = record_bytes.saturating_add(chunk_bytes);
    let total_world_bytes = crate::storage::list_saved_worlds()
        .await
        .unwrap_or_default()
        .iter()
        .map(|world| world.used_bytes)
        .sum::<u64>();
    let other_worlds_bytes = total_world_bytes.saturating_sub(current_world_bytes);
    (chunk_sizes, current_world_bytes, other_worlds_bytes)
}

fn request_visible_persisted_loads(app: &Rc<RefCell<App>>, camera_pos: glam::Vec3) {
    let maybe_jobs = {
        let mut a = app.borrow_mut();
        let session_id = a.world_session_id;
        let Some(g) = a.game.as_mut() else {
            return;
        };
        if matches!(g.mode, GameMode::Remote) {
            return;
        }
        let Some(storage) = g.storage.clone() else {
            return;
        };

        let center = chunk_pos_of(camera_pos);
        let r = g.chunk_loader.render_distance.max(0);
        let mut positions = Vec::new();
        for dx in -r..=r {
            for dz in -r..=r {
                let pos = ChunkPos::new(center.x + dx, center.z + dz);
                if g.known_persisted.contains(&pos)
                    && !g.loaded_persisted_chunks.contains(&pos)
                    && g.pending_persisted_loads.insert(pos)
                {
                    positions.push(pos);
                }
            }
        }
        if positions.is_empty() {
            return;
        }
        Some((storage, positions, session_id))
    };

    let Some((storage, positions, session_id)) = maybe_jobs else {
        return;
    };
    for pos in positions {
        spawn_persisted_chunk_load(app.clone(), storage.clone(), pos, session_id);
    }
}

fn spawn_persisted_chunk_load(
    app: Rc<RefCell<App>>,
    storage: OpfsStorage,
    pos: ChunkPos,
    session_id: u64,
) {
    wasm_bindgen_futures::spawn_local(async move {
        let loaded = match storage.load_chunk(pos).await {
            Ok(Some(bytes)) => match voxweb_core::chunk::decode(&bytes) {
                Ok(chunk) => Some(Ok(chunk)),
                Err(e) => {
                    log::warn!("[storage] decode persisted chunk {pos:?} failed: {e:?}");
                    Some(Err(()))
                }
            },
            Ok(None) => None,
            Err(e) => {
                log::warn!("[storage] load persisted chunk {pos:?} failed: {e:?}");
                Some(Err(()))
            }
        };

        let mut a = app.borrow_mut();
        if a.world_session_id != session_id {
            return;
        }
        let Some(g) = a.game.as_mut() else {
            return;
        };
        g.pending_persisted_loads.remove(&pos);
        match loaded {
            Some(Ok(chunk)) => {
                let should_apply = {
                    let server = g.server.borrow();
                    !server.world.persistence.is_dirty_or_in_flight(pos)
                };
                g.loaded_persisted_chunks.insert(pos);
                if should_apply {
                    g.server.borrow_mut().load_chunk_from_storage(pos, chunk);
                    enqueue_chunk_and_neighbors(&mut g.mesh_jobs, pos, MeshPriority::High);
                } else {
                    log::debug!(
                        "[storage] skip persisted overwrite for dirty/in-flight chunk {pos:?}"
                    );
                }
            }
            Some(Err(())) => {
                g.loaded_persisted_chunks.insert(pos);
            }
            None => {
                g.known_persisted.remove(&pos);
                g.loaded_persisted_chunks.insert(pos);
            }
        }
    });
}

fn enqueue_chunk_and_neighbors(
    mesh_jobs: &mut crate::mesh_jobs::MeshJobQueue,
    pos: ChunkPos,
    priority: MeshPriority,
) {
    mesh_jobs.enqueue(pos, priority);
    for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
        mesh_jobs.enqueue(ChunkPos::new(pos.x + dx, pos.z + dz), priority);
    }
}

// ============================================================
// Connecting 帧（Host/Remote 协商中）
// ============================================================

fn render_connecting_frame(app: &Rc<RefCell<App>>, cw: u32, ch: u32) -> Result<(), String> {
    // —— 1. 推进网络状态机（与 InGame 复用 poll_net 路径）——
    poll_net(app);

    // —— 1b. drain Server→Client inbox（Remote 端收 ChunkSnapshot / Welcome 等）——
    {
        let mut a = app.borrow_mut();
        if let Some(game) = a.game.as_mut() {
            let mut msgs = Vec::new();
            while let Some(msg) = game.net.try_recv_server_message() {
                msgs.push(msg);
            }
            for msg in msgs {
                apply_server_message(game, msg);
            }
        }
    }

    // —— 2. 区块预载（网络协商完成后，加载出生点周围区块）——
    {
        let mut a = app.borrow_mut();
        let App {
            ref mut renderer,
            ref mut game,
            ref mut preload_state,
            ref mut state,
            ref mut request_pointer_lock_next,
            ..
        } = *a;

        if let Some(preload) = preload_state
            && preload.active
            && let Some(game) = game
        {
            let mode = game.mode;

            // Host/Local：首帧及后续帧生成+入队（update 内部在 last_center 未变时跳过）
            if mode != GameMode::Remote {
                let mut server_mut = game.server.borrow_mut();
                game.chunk_loader.update(
                    voxweb_server::DEFAULT_SPAWN,
                    &mut server_mut,
                    &mut game.mesh_jobs,
                    renderer,
                );
                drop(server_mut);
            } else if game.entity_id != 0 {
                let mut server_mut = game.server.borrow_mut();
                let requests = game.chunk_loader.update_remote(
                    voxweb_server::DEFAULT_SPAWN,
                    &mut server_mut,
                    &mut game.mesh_jobs,
                    renderer,
                );
                drop(server_mut);
                if !requests.is_empty() {
                    log::debug!("[remote] request {} spawn preload chunks", requests.len());
                    game.net.send_client_message(ClientMessage::ChunkRequest {
                        center: chunk_pos_of(voxweb_server::DEFAULT_SPAWN),
                        render_distance: game.chunk_loader.render_distance.max(0) as u32,
                        chunks: requests,
                    });
                }
            }

            // 运行网格化（预载期间用 16ms 预算，比正常 4ms 更大）
            let server_ref = game.server.borrow();
            game.mesh_jobs
                .run_until_budget(16.0, &server_ref, renderer, &now_ms);

            // 统计已接收和已网格化的区块数
            let spawn_center = crate::chunk_loader::chunk_pos_of(voxweb_server::DEFAULT_SPAWN);
            let r = game.chunk_loader.render_distance;
            preload.total = ((2 * r + 1) * (2 * r + 1)) as usize;
            let mut received = 0usize;
            let mut meshed = 0usize;
            for dx in -r..=r {
                for dz in -r..=r {
                    let pos = voxweb_core::ChunkPos::new(spawn_center.x + dx, spawn_center.z + dz);
                    if server_ref.world.chunks.contains_key(&pos) {
                        received += 1;
                        if renderer.has_chunk_mesh(pos) {
                            meshed += 1;
                        }
                    }
                }
            }
            drop(server_ref);

            preload.received = received;
            preload.meshed = meshed;

            // 完成条件：所有区块已接收 且 网格化队列为空（空 chunk 已被处理但不上传 mesh）
            if received >= preload.total && game.mesh_jobs.is_empty() {
                preload.active = false;
                *state = AppState::ingame_default();
                *request_pointer_lock_next = true;
                log::info!(
                    "[preload] 区块预载完成 (received={received} meshed={meshed} total={}) → InGame",
                    preload.total
                );
            }
        }
    }

    // —— 3. 构建步骤列表（网络步骤 + 区块预载步骤）——
    let (mode, room_id, steps, error) = {
        let a = app.borrow();
        let mode = a.connecting_mode;
        let game = a.game.as_ref();
        // Local 模式跳过网络步骤（无网络协商）
        let mut steps = if mode == GameMode::Local {
            Vec::new()
        } else {
            game.map(|g| g.net.session().loading_steps())
                .unwrap_or_default()
        };

        // 追加区块预载步骤
        let chunk_status = match &a.preload_state {
            Some(preload) if preload.active => voxweb_net::StepStatus::InProgress,
            Some(_) => voxweb_net::StepStatus::Done,
            None => voxweb_net::StepStatus::Pending,
        };
        let chunk_label = match &a.preload_state {
            Some(preload) if preload.active => {
                format!(
                    "Loading spawn chunks ({}/{})",
                    preload.received, preload.total
                )
            }
            _ => "Loading spawn chunks".to_string(),
        };
        steps.push(voxweb_net::LoadingStep {
            label: chunk_label,
            status: chunk_status,
        });

        (
            a.connecting_mode,
            a.connecting_room_id.clone(),
            steps,
            a.connecting_error.clone(),
        )
    };

    // —— 4. 跑 egui ——
    let (cancel, paint_jobs, pixels_per_point, textures_delta) = {
        let a = app.borrow_mut();
        let events: Vec<egui::Event> = std::mem::take(&mut *a.egui_events.borrow_mut());
        let raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(cw as f32, ch as f32),
            )),
            events,
            ..Default::default()
        };
        let mut act: Option<ConnectingAction> = None;
        let full_output = a.egui_ctx.run_ui(raw_input, |ui| {
            act = draw_connecting(ui.ctx(), mode, &room_id, &steps, error.as_deref());
        });
        let ppp = full_output.pixels_per_point;
        let jobs = a.egui_ctx.tessellate(full_output.shapes, ppp);
        (act, jobs, ppp, full_output.textures_delta)
    };

    if matches!(cancel, Some(ConnectingAction::Cancel)) {
        flush_dirty_best_effort(app, "return-to-lobby");
        let mut a = app.borrow_mut();
        return_to_lobby(&mut a);
        return Ok(());
    }

    // —— 5. 渲染（纯 egui + 暗色清屏）——
    paint_egui_only(app, paint_jobs, pixels_per_point, textures_delta, cw, ch);
    Ok(())
}

/// 复用 lobby/connecting 的"清屏 + egui Pass" 渲染路径。
fn paint_egui_only(
    app: &Rc<RefCell<App>>,
    paint_jobs: Vec<egui::epaint::ClippedPrimitive>,
    pixels_per_point: f32,
    textures_delta: egui::TexturesDelta,
    cw: u32,
    ch: u32,
) {
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
        return;
    };
    let view = surface_texture
        .texture
        .create_view(&wgpu::TextureViewDescriptor::default());

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("connecting_frame"),
    });

    // 清屏
    {
        let _clear_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("connecting_clear"),
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
            label: Some("connecting_egui"),
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

// ============================================================
// Disconnected 帧（Phase 6）
// ============================================================

/// 已断开连接页面：模型上和 Lobby 一样，纯 egui + 暗色清屏，没有 wgpu world pass。
/// 用户点击"返回大厅"后切回 [`AppState::Lobby`] 并清掉 [`App::disconnect_reason`]。
fn render_disconnected_frame(app: &Rc<RefCell<App>>, cw: u32, ch: u32) -> Result<(), String> {
    let reason = app.borrow().disconnect_reason.clone().unwrap_or_default();

    // —— 跑 egui ——
    let (action, paint_jobs, pixels_per_point, textures_delta) = {
        let a = app.borrow_mut();
        let events: Vec<egui::Event> = std::mem::take(&mut *a.egui_events.borrow_mut());
        let raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(cw as f32, ch as f32),
            )),
            events,
            ..Default::default()
        };
        let mut act = ui::disconnected::DisconnectedAction::None;
        let full_output = a.egui_ctx.run_ui(raw_input, |ui| {
            act = ui::disconnected::draw_disconnected(ui.ctx(), &reason);
        });
        let ppp = full_output.pixels_per_point;
        let jobs = a.egui_ctx.tessellate(full_output.shapes, ppp);
        (act, jobs, ppp, full_output.textures_delta)
    };

    if matches!(action, ui::disconnected::DisconnectedAction::BackToLobby) {
        flush_dirty_best_effort(app, "return-to-lobby");
        let mut a = app.borrow_mut();
        return_to_lobby(&mut a);
        return Ok(());
    }

    paint_egui_only(app, paint_jobs, pixels_per_point, textures_delta, cw, ch);
    Ok(())
}

/// 推进 Game.net 状态机：每帧调用一次。
/// 在 Host 模式下注入闭包处理 peer 来的 ClientMessage：
/// - Hello → 校验版本 → `server.add_player(...)` → 记录 (peer_id, eid) 待 poll 返回后注册
/// - 其他消息 → 从已建立的 peer_to_entity 映射查 eid → `server.handle_message(eid, msg)`
///
/// poll 完成后：
/// 1. 把新分配的 (peer_id, eid) 应用到 net（host_register_peer）
/// 2. drain server.outbox + 根据 mode 路由（Host：host_route_outbox / Local：折叠到 server_inbox）
/// 3. 应用 RoomEvent（其中 RemoteLeft 会再触发 server.remove_player，下帧 flush）
fn poll_net(app: &Rc<RefCell<App>>) {
    let now = now_ms();
    let (events, pending_registrations) = {
        let mut a = app.borrow_mut();
        let Some(game) = a.game.as_mut() else {
            return;
        };
        match game.mode {
            GameMode::Host => {
                // Host：每帧把 performance.now() 同步给 server.current_time_ms
                // （Pong / PlayerTick 中 server_time_ms 字段用）
                game.server.borrow_mut().set_clock(now as u64);

                let server_rc = game.server.clone();
                // 本帧的 peer_to_entity 快照（含本帧新增的 Hello 注册），供非-Hello 消息查 eid
                let live_map: Rc<RefCell<HashMap<u32, EntityId>>> =
                    Rc::new(RefCell::new(game.net.host_peer_to_entity_clone()));
                let live_map_for_closure = live_map.clone();
                // 本帧新注册的 (peer_id, eid)，poll 返回后写回 net
                let pending_regs: Rc<RefCell<Vec<(u32, EntityId)>>> =
                    Rc::new(RefCell::new(Vec::new()));
                let pending_regs_inner = pending_regs.clone();

                let mut handler = |peer_id: u32, msg: ClientMessage| match msg {
                    ClientMessage::Hello {
                        display_name,
                        version,
                    } => {
                        if version != PROTOCOL_VERSION {
                            log::warn!(
                                "[host] peer {peer_id} bad protocol version {version} (expected {PROTOCOL_VERSION})"
                            );
                            return;
                        }
                        let eid = server_rc.borrow_mut().add_player(display_name);
                        live_map_for_closure.borrow_mut().insert(peer_id, eid);
                        pending_regs_inner.borrow_mut().push((peer_id, eid));
                    }
                    other => {
                        let map = live_map_for_closure.borrow();
                        let Some(&eid) = map.get(&peer_id) else {
                            log::warn!(
                                "[host] peer {peer_id} sent message before Hello: {:?}",
                                std::mem::discriminant(&other)
                            );
                            return;
                        };
                        drop(map);
                        server_rc.borrow_mut().handle_message(eid, other);
                    }
                };
                let events = game.net.poll(Some(&mut handler));
                let regs = pending_regs.borrow().clone();
                (events, regs)
            }
            _ => (game.net.poll(None), Vec::new()),
        }
    };

    // 应用 Host 新注册的 peer_id → entity_id 映射
    if !pending_registrations.is_empty() {
        let mut a = app.borrow_mut();
        if let Some(game) = a.game.as_mut() {
            for (pid, eid) in pending_registrations {
                game.net.host_register_peer(pid, eid);
            }
        }
    }

    // 应用 RoomEvent（含 RemoteLeft → remove_player 入 outbox）
    for ev in events {
        apply_room_event(app, ev);
    }

    // 最终统一 flush outbox：把 handle_message / add_player / remove_player 产生的
    // 所有 ServerMessage 通过 net（Host）或 mpsc（Local）送出。
    {
        let mut a = app.borrow_mut();
        if let Some(game) = a.game.as_mut() {
            flush_server_outbox(game);
        }
    }
}

/// Phase 5：把 server.outbox 中累积的 OutboundMessage 路由到正确去向。
///
/// - **Local**：所有 Recipient 折叠为 server_inbox（自己是唯一玩家）。
/// - **Host**：调 `net.host_route_outbox`，按 Recipient 分发 peers DC + 自身 mpsc。
/// - **Remote**：理论上 outbox 永远空（不 tick / 不 handle_message），防御性 drain 后忽略。
fn flush_server_outbox(game: &mut Game) {
    let outbox = game.server.borrow_mut().drain_outbox();
    if outbox.is_empty() {
        return;
    }
    match game.mode {
        GameMode::Local => {
            for m in outbox {
                game.server_inbox.send_server_message(m.message);
            }
        }
        GameMode::Host => {
            let unsent = game.net.host_route_outbox(outbox, &game.server_inbox);
            // 流控阻塞的消息重新入队，下帧再试
            if !unsent.is_empty() {
                game.server.borrow_mut().reenqueue_outbox(unsent);
            }
        }
        GameMode::Remote => {
            log::warn!(
                "[client] Remote 模式不应该有 outbox（{} 条消息被丢弃）",
                outbox.len()
            );
        }
    }
}

fn apply_room_event(app: &Rc<RefCell<App>>, ev: RoomEvent) {
    let mut a = app.borrow_mut();
    match ev {
        RoomEvent::Connected => {
            // 网络连接完成，启动区块预载（不再直接进 InGame）
            if a.state == AppState::Connecting {
                if let Some(ref game) = a.game {
                    let rd = game.chunk_loader.render_distance;
                    let total = ((2 * rd + 1) * (2 * rd + 1)) as usize;
                    a.preload_state = Some(PreloadState {
                        total,
                        received: 0,
                        meshed: 0,
                        active: true,
                    });
                    log::info!("[net] Connected → 开始区块预载 (total={total})");
                } else {
                    // 无 game（不应发生），直接进 InGame 兜底
                    a.state = AppState::ingame_default();
                    a.request_pointer_lock_next = true;
                }
            }
        }
        RoomEvent::Disconnected { reason } => {
            log::warn!("[net] Disconnected: {reason}");
            clear_world_runtime(&mut a);
            a.disconnect_reason = Some(reason.clone());
            a.connecting_error = Some(reason);
            // Phase 6：Connecting / InGame 失联都跳到 Disconnected 页让用户看到原因
            a.state = AppState::Disconnected;
        }
        RoomEvent::RemoteLeft { peer_id } => {
            log::info!("[net] RemoteLeft: peer {peer_id}");
            a.relayed_peers.remove(&peer_id);
            // Host：从 net 拿 entity_id → 调 server.remove_player
            if let Some(ref mut game) = a.game
                && let Some(eid) = game.net.host_unregister_peer(peer_id)
            {
                game.server.borrow_mut().remove_player(eid);
            }
        }
        RoomEvent::SignalingError(msg) => {
            log::warn!("[net] signaling error: {msg}");
            // InGame 状态下将错误推入通知队列，让玩家在游戏内看到浮窗提示
            if matches!(a.state, AppState::InGame { .. }) {
                push_notification(&mut a, msg.clone());
            }
            a.connecting_error = Some(msg);
        }
        RoomEvent::PeerCount(n) => {
            log::info!("[net] peer count: {n}");
        }
        RoomEvent::PeerRelayed { peer_id } => {
            log::info!("[net] peer {peer_id} 已切换为中继模式");
            a.relayed_peers.insert(peer_id);
        }
    }
}

// ============================================================
// InGame 帧
// ============================================================

fn render_game_frame(
    app: &Rc<RefCell<App>>,
    dt: f32,
    cw: u32,
    ch: u32,
    paused: bool,
    chat_open: bool,
) -> Result<(), String> {
    // —— 0. 推进网络状态机（Host/Remote 协商 + Pong 处理） ——
    poll_net(app);

    // 当前 GameMode（决定后续是否跑 server tick / chunk_loader）
    let mode = app
        .borrow()
        .game
        .as_ref()
        .map(|g| g.mode)
        .unwrap_or(GameMode::Local);

    // —— 1. drain Local 通道（Client→Server）→ Server::handle_message → 推回 Server→Client ——
    //   Remote 模式不跑这步（server 仅占位，server_inbox 也是 dummy）
    if mode != GameMode::Remote {
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
            game.server.borrow_mut().handle_message(entity_id, msg);
        }
        // Phase 5：handle_message 不再返回 Vec，改为 enqueue 到 server.outbox；
        // 本帧 flush 保证 ActionAck 等能在下文的 apply_server_message 中收到。
        flush_server_outbox(game);
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

    // —— 2b. 5s Ping，记下发出时刻供 Pong 对齐 ——
    {
        let mut a = app.borrow_mut();
        let Some(game) = a.game.as_mut() else {
            return Ok(());
        };
        let now = now_ms();
        if game.mode != GameMode::Local
            && game.entity_id != 0
            && now - game.last_ping_sent_ms >= PING_INTERVAL_MS
        {
            let client_time_ms = now as u64;
            game.pending_pings.insert(client_time_ms, now);
            // 上限 16 个待办，避免长期不通时无限增长
            if game.pending_pings.len() > 16 {
                let oldest_key = *game.pending_pings.keys().min().unwrap();
                game.pending_pings.remove(&oldest_key);
            }
            game.net
                .send_client_message(ClientMessage::Ping { client_time_ms });
            game.last_ping_sent_ms = now;
        }
    }

    // —— 3. 输入 → 相机朝向 + 物理 + 动作 ——
    // Phase 6：当 paused / chat_open 任一为 true 时，本帧用 neutral 输入跑物理（重力继续生效），
    //          但不读取鼠标转向、hotbar、挖放等输入；同时消费 input.esc_menu / chat_open 边沿
    //          切换叠加层状态。最终把更新后的 (paused, chat_open) 写回 app.state。
    let mut next_paused = paused;
    let mut next_chat_open = chat_open;
    let mut request_pointer_lock_after = false;
    let mut request_exit_pointer_lock = false;

    let (camera_pos, view_proj, fps_display, mesh_budget, current_hit_pos) = {
        let mut a = app.borrow_mut();
        let fps_display = a.fps_display;
        let input_rc = a.input.clone();
        let Some(game) = a.game.as_mut() else {
            return Ok(());
        };
        game.camera.aspect = cw as f32 / ch.max(1) as f32;

        let mut input = input_rc.borrow_mut();

        // —— Phase 6：ESC / T 边沿优先消费，切换叠加层 ——
        // ESC 优先级：聊天 > 暂停 > 进入暂停
        if input.esc_menu {
            if next_chat_open {
                next_chat_open = false;
                game.chat.input_buffer.clear();
                request_pointer_lock_after = true;
            } else if next_paused {
                next_paused = false;
                request_pointer_lock_after = true;
            } else {
                next_paused = true;
                request_exit_pointer_lock = true;
            }
        }
        // T 仅在无叠加层时打开聊天
        if input.chat_open && !next_paused && !next_chat_open {
            next_chat_open = true;
            request_exit_pointer_lock = true;
        }

        // 是否仍处于"活跃游戏"（用本帧消费 ESC / T 之后的值判定）
        let active_play = !next_paused && !next_chat_open;

        // 鼠标转向（仅活跃游戏时消费）
        if active_play && input.pointer_locked && (input.mouse_dx != 0.0 || input.mouse_dy != 0.0) {
            game.camera.apply_mouse(
                input.mouse_dx,
                input.mouse_dy,
                game.settings.mouse_sensitivity * BASE_SENSITIVITY_RAD_PER_PIXEL,
            );
        }

        // Hotbar 切换（仅活跃游戏）
        if active_play && let Some(idx) = input.hotbar_request.take() {
            game.hotbar.select(idx);
        }

        // 双击空格切换 Fly/Walk（仅活跃游戏）
        if active_play && input.fly_toggle_pending {
            game.physics.toggle_mode();
            log::info!("模式切换 → {:?}", game.physics.mode);
        }

        // 物理 step：活跃 + 锁定 → 真实输入；否则 neutral（仅跑重力）
        let world_ref = game.server.clone();
        {
            let server_borrow = world_ref.borrow();
            let getter = |x: i32, y: i32, z: i32| server_borrow.world.get_block_world(x, y, z);
            if active_play && input.pointer_locked {
                game.physics.step(&getter, &game.camera, &input, dt);
            } else {
                let neutral = neutral_input(&input);
                game.physics.step(&getter, &game.camera, &neutral, dt);
            }
        }
        apply_pending_position_correction(
            &mut game.physics,
            &mut game.pending_position_correction,
            dt,
        );
        game.camera.position = game.physics.eye_position();

        // 60Hz 逻辑帧
        game.frame_clock.accumulate(dt);
        let mut steps_consumed: u32 = 0;
        let server_tick_allowed = matches!(game.mode, GameMode::Local | GameMode::Host);
        let mut last_input_tick_to_send = None;
        while game.frame_clock.consume_logic_step() {
            if server_tick_allowed {
                game.server.borrow_mut().tick();
            }
            // 每个逻辑步分配本地输入序号。Remote 不能借 dummy server tick，
            // 否则 Host 回播时无法把权威位置和同一条预测记录对齐。
            game.local_input_tick = game.local_input_tick.wrapping_add(1).max(1);
            game.input_history
                .push(game.local_input_tick, game.physics.feet_position);
            last_input_tick_to_send = Some(game.local_input_tick);
            steps_consumed += 1;
        }

        // 若本帧消费了一个或多个逻辑步，只上报最新位置；tick 仍对应最后一个本地输入序号。
        if steps_consumed > 0 && game.entity_id != 0 {
            let tick = last_input_tick_to_send.unwrap_or(game.local_input_tick);
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

        // 挖放动作（仅在活跃游戏 + 指针锁定时启用）
        if active_play && input.pointer_locked {
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
    // Local/Host 直接生成；Remote 只请求缺失 chunk，由 Host 回 ChunkSnapshot。
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
        if mode == GameMode::Remote {
            if game.entity_id != 0 {
                let requests = game.chunk_loader.update_remote(
                    camera_pos,
                    &mut server_mut,
                    &mut game.mesh_jobs,
                    renderer,
                );
                drop(server_mut);
                if !requests.is_empty() {
                    log::debug!("[remote] request {} chunks near camera", requests.len());
                    game.net.send_client_message(ClientMessage::ChunkRequest {
                        center: chunk_pos_of(camera_pos),
                        render_distance: game.chunk_loader.render_distance.max(0) as u32,
                        chunks: requests,
                    });
                }
            }
        } else {
            game.chunk_loader
                .update(camera_pos, &mut server_mut, &mut game.mesh_jobs, renderer);
        }
    }
    request_visible_persisted_loads(app, camera_pos);

    // —— 6. mesh_jobs run_until_budget ——
    let mesh_stats = {
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
            .run_until_budget(mesh_budget, &server_borrow, renderer, &now_ms)
    };
    app.borrow_mut().perf.record_mesh(mesh_stats);

    pump_persistence(app);

    // —— 7. egui HUD（Phase 6：含玩家列表 / 名牌 / 聊天浮窗 / 聊天框 / 暂停菜单） ——
    let pointer_locked = app.borrow().input.borrow().pointer_locked;
    // 本帧 egui 内可能触发的动作
    let mut chat_submission: Option<String> = None;
    let mut pause_exit_to_lobby = false;
    let (paint_jobs, pixels_per_point, textures_delta) = {
        let mut a = app.borrow_mut();
        let events: Vec<egui::Event> = std::mem::take(&mut *a.egui_events.borrow_mut());
        let raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(cw as f32, ch as f32),
            )),
            events,
            ..Default::default()
        };
        // 提前抓出本帧 HUD 用的只读快照（避免后续 closure 内对 game 同时持有可变和不可变借用）
        let perf = a.perf;
        let hud_data = a.game.as_ref().map(|g| HudData {
            fps: fps_display,
            pos: (
                g.camera.position.x,
                g.camera.position.y,
                g.camera.position.z,
            ),
            yaw_deg: g.camera.yaw.to_degrees(),
            pitch_deg: g.camera.pitch.to_degrees(),
            pointer_locked,
            loaded_chunks: g.chunk_loader.loaded.len(),
            mesh_pending: g.mesh_jobs.len(),
            mode: g.physics.mode,
            on_ground: g.physics.on_ground,
            hotbar_items: g.hotbar.items,
            hotbar_selected: g.hotbar.selected,
            game_mode: g.mode,
            rtt_ms: g.rtt_ms,
            room_id: g.room_id.clone(),
            relayed_peer_count: a.relayed_peers.len(),
            show_stats: g.settings.show_stats,
            depth_prepass_enabled: g.settings.depth_prepass_enabled,
            quota: g.quota,
            current_world_bytes: g.current_world_bytes,
            other_worlds_bytes: g.other_worlds_bytes,
            storage_error: g.storage_error.clone(),
            perf,
        });
        // 装配 PlayerListEntry：自己（is_me=true）+ 远端，按 entity_id 升序
        let player_list_entries: Vec<ui::players::PlayerListEntry> =
            if let Some(g) = a.game.as_ref() {
                let mut v: Vec<ui::players::PlayerListEntry> =
                    Vec::with_capacity(g.remote_players.len() + 1);
                v.push(ui::players::PlayerListEntry {
                    entity_id: g.entity_id,
                    display_name: g.display_name.clone(),
                    color_rgb: crate::app::entity_color(g.entity_id),
                    is_host: g.entity_id == g.host_entity_id,
                    is_me: true,
                });
                for (eid, rp) in &g.remote_players {
                    v.push(ui::players::PlayerListEntry {
                        entity_id: *eid,
                        display_name: rp.display_name.clone(),
                        color_rgb: rp.color_rgb,
                        is_host: *eid == g.host_entity_id,
                        is_me: false,
                    });
                }
                v.sort_by_key(|e| e.entity_id);
                v
            } else {
                Vec::new()
            };
        // 装配名牌：从 interp 拿当前 render-target 时刻的位置；自己不画
        let now_local = now_ms();
        let mut nameplate_entries: Vec<ui::players::NameplateEntry> = Vec::new();
        let mut view_proj_for_np = glam::Mat4::IDENTITY;
        if let Some(g) = a.game.as_mut() {
            view_proj_for_np = g.camera.vp_matrix();
            let render_target = now_local + g.server_clock_offset_ms - g.interp.delay_ms;
            let cam_pos = g.camera.position;
            let eids: Vec<EntityId> = g.interp.ids().collect();
            for eid in eids {
                if eid == g.entity_id {
                    continue;
                }
                let Some((pos, _yaw, _pitch)) = g.interp.advance(eid, render_target) else {
                    continue;
                };
                let dist = (pos - cam_pos).length();
                let head_pos = pos + glam::Vec3::new(0.0, voxweb_core::PLAYER_HEIGHT + 0.3, 0.0);
                let dir = (head_pos - cam_pos).normalize_or_zero();
                let occluded = {
                    let server = g.server.borrow();
                    let getter = |x: i32, y: i32, z: i32| server.world.get_block_world(x, y, z);
                    raycast(cam_pos, dir, dist.max(0.0), &getter)
                        .is_some_and(|hit| hit.distance + 0.15 < dist)
                };
                let name = g
                    .remote_players
                    .get(&eid)
                    .map(|r| r.display_name.clone())
                    .unwrap_or_else(|| format!("Player {eid}"));
                nameplate_entries.push(ui::players::NameplateEntry {
                    world_position: pos,
                    display_name: name,
                    distance: dist,
                    occluded,
                });
            }
        }

        // 提取游戏内通知（5 秒内有效），供 egui 闭包渲染浮窗
        let active_notifications: Vec<String> = a
            .notifications
            .iter()
            .filter(|(ts, _)| now_local - ts < 5000.0)
            .map(|(_, msg)| msg.clone())
            .collect();
        // 清理过期通知
        a.notifications.retain(|(ts, _)| now_local - ts < 5000.0);

        // —— 跑 egui：在同一 ctx.run 内绘制 HUD + 玩家列表 + 名牌 + 聊天浮窗 + 聊天框 + 暂停菜单 ——
        let App {
            ref egui_ctx,
            ref mut game,
            ..
        } = *a;
        let mut chat_action_local = ui::chat::ChatUiAction::None;
        let mut pause_action_local = ui::pause::PauseAction::None;
        let mut pause_settings_changed = false;
        let full_output = egui_ctx.run_ui(raw_input, |ui| {
            let ctx = ui.ctx();
            // 1) HUD（左上角统计面板受 hud.show_stats 开关控制；准星 / hotbar / 提示栏照常）
            if let Some(hud) = hud_data.as_ref() {
                draw_hud(ctx, hud.clone());
            }
            // 1b) 游戏内通知浮窗（信令错误等，5 秒自动消失）
            if !active_notifications.is_empty() {
                draw_toast_notifications(ctx, &active_notifications);
            }
            // 2) 玩家列表
            ui::players::draw_player_list(ctx, &player_list_entries);
            // 3) 远端玩家名牌
            ui::players::draw_nameplates(ctx, &nameplate_entries, view_proj_for_np);
            // 4) 聊天最近消息浮窗（不论叠加层都显示）
            if let Some(g) = game.as_ref() {
                ui::chat::draw_recent_overlay(ctx, &g.chat, now_local);
            }
            // 5) 聊天输入框（仅 chat_open）
            if next_chat_open && let Some(g) = game.as_mut() {
                chat_action_local = ui::chat::draw_chat_window(ctx, &mut g.chat);
            }
            // 6) 暂停菜单（仅 paused）
            if next_paused && let Some(g) = game.as_mut() {
                // 复制一份 settings 比较，避免 egui closure 内反复 mut/immutable 借用
                let working_before = g.settings.clone();
                pause_action_local = ui::pause::draw_pause_menu(ctx, &mut g.settings);
                if g.settings != working_before {
                    pause_settings_changed = true;
                }
            }
        });

        // 处理聊天动作
        match chat_action_local {
            ui::chat::ChatUiAction::Submit(content) => {
                chat_submission = Some(content);
                next_chat_open = false;
                request_pointer_lock_after = true;
            }
            ui::chat::ChatUiAction::Cancel => {
                next_chat_open = false;
                request_pointer_lock_after = true;
            }
            ui::chat::ChatUiAction::None => {}
        }
        // 处理暂停菜单：设置变更 → 应用；按钮 → 设置返回 flag
        if pause_settings_changed && let Some(g) = a.game.as_mut() {
            g.apply_settings();
        }
        match pause_action_local {
            ui::pause::PauseAction::Resume => {
                if let Some(g) = a.game.as_ref() {
                    settings_storage::save(&g.settings);
                }
                next_paused = false;
                request_pointer_lock_after = true;
            }
            ui::pause::PauseAction::ExitToLobby => {
                if let Some(g) = a.game.as_ref() {
                    settings_storage::save(&g.settings);
                }
                pause_exit_to_lobby = true;
            }
            ui::pause::PauseAction::SaveNow => {
                if let Some(g) = a.game.as_mut() {
                    g.save_now_requested = true;
                    g.last_persist_ms = 0.0;
                }
            }
            ui::pause::PauseAction::None => {}
        }

        let ppp = full_output.pixels_per_point;
        let jobs = a.egui_ctx.tessellate(full_output.shapes, ppp);
        (jobs, ppp, full_output.textures_delta)
    };

    // —— 7b. 聊天发送 / 暂停菜单 ExitToLobby 副作用 ——
    if let Some(content) = chat_submission {
        let mut a = app.borrow_mut();
        if let Some(g) = a.game.as_mut() {
            send_chat(g, content);
        }
    }
    if pause_exit_to_lobby {
        flush_dirty_best_effort(app, "return-to-lobby");
        let mut a = app.borrow_mut();
        return_to_lobby(&mut a);
        return Ok(());
    }

    // 把 paused / chat_open 写回 AppState
    {
        let mut a = app.borrow_mut();
        a.state = AppState::InGame {
            paused: next_paused,
            chat_open: next_chat_open,
        };
        if request_exit_pointer_lock && let Some(doc) = web_sys::window().and_then(|w| w.document())
        {
            doc.exit_pointer_lock();
        }
        if request_pointer_lock_after {
            a.request_pointer_lock_next = true;
        }
    }

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

        // 天空 + 可选 Depth Pre-Pass + 世界 Pass
        a.renderer
            .render_skybox(&mut encoder, &view, view_proj, (now_ms() / 1000.0) as f32);
        let depth_start = now_ms();
        let depth_enabled = a
            .game
            .as_ref()
            .map(|g| g.settings.depth_prepass_enabled)
            .unwrap_or(false);
        if depth_enabled {
            a.renderer.render_depth_prepass(&mut encoder, view_proj);
        }
        let depth_pass_ms = (now_ms() - depth_start) as f32;

        let world_start = now_ms();
        let world_stats =
            a.renderer
                .render_world(&mut encoder, &view, view_proj, [0.55, 0.78, 0.93, 1.0]);
        let world_pass_ms = (now_ms() - world_start) as f32;

        // Phase 5 玩家实体 Pass：从插值器拿远端位置 → instance buffer → 渲染
        let player_start = now_ms();
        {
            let now = now_ms();
            let mut instances: Vec<voxweb_render::passes::player::PlayerInstance> = Vec::new();
            if let Some(ref mut game) = a.game {
                let render_server_time = now + game.server_clock_offset_ms - game.interp.delay_ms;
                let eids: Vec<voxweb_core::protocol::EntityId> = game.interp.ids().collect();
                for eid in eids {
                    if let Some((pos, _yaw, _pitch)) = game.interp.advance(eid, render_server_time)
                        && let Some(rp) = game.remote_players.get(&eid)
                    {
                        instances.push(voxweb_render::passes::player::PlayerInstance {
                            position: pos.to_array(),
                            _pad0: 0.0,
                            color: rp.color_rgb,
                            _pad1: 0.0,
                        });
                    }
                }
            }
            a.renderer.upload_player_instances(&instances);
            a.renderer.render_players(&mut encoder, &view, view_proj);
        }
        let player_pass_ms = (now_ms() - player_start) as f32;

        let transparent_start = now_ms();
        a.renderer
            .render_transparent(&mut encoder, &view, view_proj, camera_pos);
        let transparent_pass_ms = (now_ms() - transparent_start) as f32;

        // 选中方块线框（命中时）
        let selection_start = now_ms();
        a.renderer
            .render_selection(&mut encoder, &view, view_proj, current_hit_pos);
        let selection_pass_ms = (now_ms() - selection_start) as f32;

        // egui Pass
        let egui_start = now_ms();
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
        let egui_pass_ms = (now_ms() - egui_start) as f32;

        a.perf.world_pass_ms = world_pass_ms;
        a.perf.depth_pass_ms = depth_pass_ms;
        a.perf.transparent_pass_ms = transparent_pass_ms;
        a.perf.player_pass_ms = player_pass_ms;
        a.perf.selection_pass_ms = selection_pass_ms;
        a.perf.egui_pass_ms = egui_pass_ms;
        a.perf.visible_chunks = world_stats.visible_chunks;
        a.perf.culled_chunks = world_stats.culled_chunks;
        a.perf.drawn_vertices = world_stats.drawn_vertices;
        a.perf.drawn_indices = world_stats.drawn_indices;

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

fn pump_persistence(app: &Rc<RefCell<App>>) {
    let now = now_ms();
    let maybe_job = {
        let mut a = app.borrow_mut();
        let session_id = a.world_session_id;
        let Some(g) = a.game.as_mut() else {
            return;
        };
        let save_now = g.save_now_requested;
        if matches!(g.mode, GameMode::Remote) {
            if save_now {
                g.save_now_requested = false;
                push_notification(&mut a, "Remote worlds are saved by the host");
            }
            return;
        }
        if !save_now && now - g.last_persist_ms < AUTO_SAVE_INTERVAL_MS {
            return;
        }
        if let Some(q) = g.quota {
            g.server
                .borrow_mut()
                .world
                .persistence
                .set_quota_pause_dirty(q.usage_ratio() > 0.95);
        }
        let Some(storage) = g.storage.clone() else {
            if save_now {
                g.save_now_requested = false;
                push_notification(&mut a, "Save unavailable");
            }
            return;
        };
        let tick = g.server.borrow().world.tick_count;
        let in_flight = g.server.borrow().world.persistence.in_flight_len();
        if in_flight > 0 {
            return;
        }
        if !save_now && !g.server.borrow().world.persistence.should_flush(tick) {
            return;
        }
        let batch_limit = if save_now {
            SAVE_NOW_BATCH_CHUNKS
        } else {
            AUTO_SAVE_BATCH_CHUNKS
        };
        let positions = g
            .server
            .borrow_mut()
            .world
            .snapshot_dirty(batch_limit, tick);
        if positions.is_empty() {
            if save_now {
                let dirty = g.server.borrow().world.persistence.dirty_len();
                let in_flight = g.server.borrow().world.persistence.in_flight_len();
                if dirty == 0 && in_flight == 0 {
                    g.save_now_requested = false;
                    push_notification(&mut a, "Save complete");
                } else if in_flight > 0 {
                    // 正在写上一批，完成回调会继续推进 / 提示。
                } else {
                    g.save_now_requested = false;
                    push_notification(&mut a, "Save will retry soon");
                }
            }
            return;
        }
        let server = g.server.clone();
        let mut encoded = Vec::new();
        let mut encoded_sizes = Vec::new();
        {
            let server_ref = server.borrow();
            for pos in &positions {
                if let Some(chunk) = server_ref.world.chunks.get(pos) {
                    let bytes = voxweb_core::chunk::encode(chunk);
                    encoded_sizes.push((*pos, bytes.len() as u64));
                    encoded.push((*pos, bytes));
                }
            }
        }
        let encoded_positions: Vec<_> = encoded.iter().map(|(pos, _)| *pos).collect();
        let encoded_position_set: HashSet<_> = encoded_positions.iter().copied().collect();
        let missing_positions: Vec<_> = positions
            .iter()
            .copied()
            .filter(|pos| !encoded_position_set.contains(pos))
            .collect();
        if !missing_positions.is_empty() {
            log::warn!(
                "[storage] {} dirty chunks were missing from memory",
                missing_positions.len()
            );
            server.borrow_mut().world.commit_flushed(&missing_positions);
        }
        if encoded.is_empty() {
            return;
        }
        g.last_persist_ms = now;
        Some((
            storage,
            server,
            encoded_positions,
            encoded,
            encoded_sizes,
            tick,
            save_now,
            session_id,
        ))
    };

    let Some((storage, server, positions, encoded, encoded_sizes, tick, save_now, session_id)) =
        maybe_job
    else {
        return;
    };
    let app_ref = app.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let result = storage.save_chunks(encoded).await;
        let refreshed_quota = if result.is_ok() {
            storage.quota().await
        } else {
            None
        };
        let mut s = server.borrow_mut();
        match result {
            Ok(()) => {
                s.world.commit_flushed(&positions);
                let save_now_done = save_now
                    && s.world.persistence.dirty_len() == 0
                    && s.world.persistence.in_flight_len() == 0;
                drop(s);
                let mut a = app_ref.borrow_mut();
                if a.world_session_id == session_id
                    && let Some(g) = a.game.as_mut()
                {
                    apply_persisted_accounting(g, &encoded_sizes, refreshed_quota);
                    if save_now_done {
                        g.save_now_requested = false;
                    }
                }
                if save_now_done && matches!(a.state, AppState::InGame { .. }) {
                    push_notification(&mut a, "Save complete");
                }
            }
            Err(e) => {
                log::warn!("[storage] save failed: {e:?}");
                s.world.record_flush_failure(&positions, tick);
                drop(s);
                if save_now {
                    let mut a = app_ref.borrow_mut();
                    if a.world_session_id == session_id
                        && let Some(g) = a.game.as_mut()
                    {
                        g.save_now_requested = false;
                    }
                    if matches!(a.state, AppState::InGame { .. }) {
                        push_notification(&mut a, format!("Save failed: {e:?}"));
                    }
                }
            }
        }
    });
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
        let input_tick = game.local_input_tick;
        let player_position = game.physics.feet_position;
        // Remote 的 server.world 只是本地世界视图，可以安全乐观修改；
        // Local/Host 与权威 server 共享同一份 world，仍等 BlockUpdate，避免提前改世界干扰校验。
        if game.mode == GameMode::Remote {
            game.server
                .borrow_mut()
                .world
                .set_block_untracked(pos, BlockID::AIR);
            for cp in affected_chunks(pos) {
                game.mesh_jobs.enqueue(cp, MeshPriority::High);
            }
        }
        let request_id = game.pending.next_request_id();
        game.pending.insert(
            request_id,
            PendingAction {
                kind: PendingKind::Break,
                pos,
                backup,
            },
        );
        game.net.send_client_message(ClientMessage::Break {
            pos,
            request_id,
            input_tick,
            player_position,
        });
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
        let input_tick = game.local_input_tick;
        let player_position = game.physics.feet_position;
        game.pending.insert(
            request_id,
            PendingAction {
                kind: PendingKind::Place(block),
                pos: neighbor,
                backup,
            },
        );
        if game.mode == GameMode::Remote {
            game.server
                .borrow_mut()
                .world
                .set_block_untracked(neighbor, block);
            for cp in affected_chunks(neighbor) {
                game.mesh_jobs.enqueue(cp, MeshPriority::High);
            }
        }
        game.net.send_client_message(ClientMessage::Place {
            pos: neighbor,
            block,
            request_id,
            input_tick,
            player_position,
        });
    }
}

/// Phase 6：发送一条聊天消息。
///
/// 走和其它客户端→服务端消息（Break / Place / Ping）一致的 [`NetEndpoint::send_client_message`] 路径：
/// - **Local-Only**：消息经内部 mpsc 进入 server_inbox，下一帧的 `handle_message` 把内容回灌成
///   `ServerMessage::Chat` 广播到自己（也会进入 `apply_server_message` 推到 `game.chat`）。
/// - **Host**：本地 server 直接处理 + 广播（自身 mpsc + DC），表现一致。
/// - **Remote**：通过 reliable DC 发到 Host；Host 广播回包含自己。
///
/// 不在本地直接 push 消息，避免和服务端广播的回灌重复出现。
fn send_chat(game: &mut Game, content: String) {
    game.net
        .send_client_message(ClientMessage::Chat { content });
}

/// 摄入一条 Host 时钟偏移样本。
///
/// 高延迟网络下，直接用最新 `server_time_ms - now` 覆盖偏移会让远端玩家插值 target
/// 来回抖动。这里第一条样本直接采用，后续做指数平滑；Pong 样本权重较高，
/// PlayerTick 样本只做低权重微调。
fn ingest_server_clock_sample(game: &mut Game, estimated_offset_ms: f64, alpha: f64) {
    if !game.server_clock_synced {
        game.server_clock_offset_ms = estimated_offset_ms;
        game.server_clock_synced = true;
        return;
    }
    let a = alpha.clamp(0.0, 1.0);
    game.server_clock_offset_ms = game.server_clock_offset_ms * (1.0 - a) + estimated_offset_ms * a;
}

fn apply_host_render_distance(game: &mut Game, host_render_distance: u32) {
    let capped = host_render_distance.max(1);
    let before = game.chunk_loader.render_distance;
    game.host_render_distance = capped;
    game.apply_settings();
    let after = game.chunk_loader.render_distance;
    if game.mode == GameMode::Remote && before != after {
        log::info!(
            "[remote] effective render distance capped by host: requested={} host={} effective={}",
            game.settings.render_distance,
            capped,
            after
        );
    }
}

fn apply_server_message(game: &mut Game, msg: ServerMessage) {
    match msg {
        ServerMessage::Welcome {
            entity_id,
            world_seed,
            host_entity_id,
            host_render_distance,
            players,
            ..
        } => {
            game.entity_id = entity_id;
            game.host_entity_id = host_entity_id;
            apply_host_render_distance(game, host_render_distance);
            log::info!(
                "Welcome v3: entity_id={entity_id}, seed={world_seed}, host={host_entity_id}, host_rd={host_render_distance}, roster_size={}",
                players.len()
            );
            // 写入 roster：除自己以外的玩家进入 remote_players
            for PlayerEntry {
                entity_id: ex_eid,
                display_name,
            } in players
            {
                if ex_eid == entity_id {
                    continue;
                }
                game.remote_players
                    .entry(ex_eid)
                    .or_insert_with(|| RemotePlayerState::new(display_name.clone(), ex_eid));
            }
            // Remote：清掉本地占位生成的 chunks（Phase 4 用 seed=0 生成了一个空世界），
            // 后续 ChunkSnapshot 逐个填充 Host 的真实世界。
            if game.mode == GameMode::Remote {
                game.server.borrow_mut().world.chunks.clear();
            }
        }
        ServerMessage::HostSettings { render_distance } => {
            apply_host_render_distance(game, render_distance);
        }
        ServerMessage::ChunkSnapshot {
            pos,
            frag_index,
            frag_total,
            payload,
        } => {
            if let Some(full) = game
                .chunk_assembler
                .ingest(pos, frag_index, frag_total, payload)
            {
                match voxweb_core::chunk::decode_chunk(&full) {
                    Ok(blocks) => {
                        let chunk = voxweb_core::chunk::Chunk { blocks };
                        game.server.borrow_mut().world.chunks.insert(pos, chunk);
                        game.chunk_loader.mark_loaded(pos);
                        // 自己 + 相邻 8 个 chunk 都重 mesh
                        for dz in -1..=1i32 {
                            for dx in -1..=1i32 {
                                game.mesh_jobs.enqueue(
                                    voxweb_core::chunk::ChunkPos::new(pos.x + dx, pos.z + dz),
                                    MeshPriority::High,
                                );
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!("[client] ChunkSnapshot {pos:?} decode failed: {e}");
                    }
                }
            }
        }
        ServerMessage::BlockUpdate { pos, block } => {
            // Remote：先写 world，再做 remesh（因为 Remote 的 server 不做本地 handle_message）
            if game.mode == GameMode::Remote {
                game.server
                    .borrow_mut()
                    .world
                    .set_block_untracked(pos, block);
            }
            for cp in affected_chunks(pos) {
                game.mesh_jobs.enqueue(cp, MeshPriority::High);
            }
        }
        ServerMessage::ActionAck {
            request_id,
            accepted,
            reason,
        } => {
            if let Some(rolled) = game.pending.resolve(request_id, accepted) {
                log::warn!(
                    "ActionAck rejected: id={request_id} reason={reason:?} pos={:?}",
                    rolled.pos
                );
                if game.mode == GameMode::Remote {
                    game.server
                        .borrow_mut()
                        .world
                        .set_block_untracked(rolled.pos, rolled.backup);
                    for cp in affected_chunks(rolled.pos) {
                        game.mesh_jobs.enqueue(cp, MeshPriority::High);
                    }
                }
            }
        }
        ServerMessage::PlayerTick {
            tick: server_tick,
            players,
            server_time_ms,
        } => {
            let now = now_ms();
            if game.mode != GameMode::Local {
                let one_way_ms = game.rtt_ms.map(|rtt| rtt as f64 * 0.5).unwrap_or(0.0);
                let estimated_offset = server_time_ms as f64 + one_way_ms - now;
                ingest_server_clock_sample(game, estimated_offset, CLOCK_TICK_ALPHA);
            }

            for snap in &players {
                if snap.entity_id == game.entity_id {
                    // 自己的权威位置 → reconcile
                    let result = reconcile_self(
                        snap.position,
                        snap.last_input_tick,
                        &mut game.physics,
                        &mut game.input_history,
                    );
                    match result {
                        ReconcileResult::SoftCorrection(delta) => {
                            game.pending_position_correction += delta;
                        }
                        ReconcileResult::HardCorrection(_) => {
                            game.pending_position_correction = glam::Vec3::ZERO;
                        }
                        ReconcileResult::Ok | ReconcileResult::MissingHistory => {}
                    }
                } else {
                    // 远端玩家 → 喂入插值缓冲
                    game.interp.ingest_tick(
                        snap.entity_id,
                        server_time_ms,
                        snap.position,
                        snap.yaw,
                        snap.pitch,
                    );
                    if let Some(rp) = game.remote_players.get_mut(&snap.entity_id) {
                        rp.last_seen_tick = server_tick;
                    }
                }
            }
        }
        ServerMessage::PeerJoined {
            entity_id,
            display_name,
        } => {
            // 排查重复加入（信令层理论上不应重复，但防御不 panic）
            game.remote_players
                .entry(entity_id)
                .or_insert_with(|| RemotePlayerState::new(display_name.clone(), entity_id));
            game.chat
                .push_system(format!("{display_name} joined the room"), now_ms());
        }
        ServerMessage::PeerLeft { entity_id } => {
            // 在 remove 之前先取一下名字，PeerLeft 系统消息才能拿到原名
            let name = game
                .remote_players
                .get(&entity_id)
                .map(|r| r.display_name.clone())
                .unwrap_or_else(|| format!("Player {entity_id}"));
            game.chat
                .push_system(format!("{name} left the room"), now_ms());
            game.remote_players.remove(&entity_id);
            game.interp.remove(entity_id);
        }
        ServerMessage::Chat { from, content } => {
            let name = if from == game.entity_id {
                game.display_name.clone()
            } else {
                game.remote_players
                    .get(&from)
                    .map(|r| r.display_name.clone())
                    .unwrap_or_else(|| format!("Player {from}"))
            };
            game.chat.push_user(from, name, content, now_ms());
        }
        ServerMessage::Pong {
            client_time_ms,
            server_time_ms,
        } => {
            if let Some(sent_ms) = game.pending_pings.remove(&client_time_ms) {
                let now = now_ms();
                let rtt = (now - sent_ms) as f32;
                game.rtt_ms = Some(match game.rtt_ms {
                    Some(prev) => prev * 0.8 + rtt * 0.2,
                    None => rtt,
                });
                let estimated_offset = server_time_ms as f64 + (rtt as f64 * 0.5) - now;
                ingest_server_clock_sample(game, estimated_offset, CLOCK_PONG_ALPHA);
            }
        }
    }
}

// ============================================================
// HUD（egui）
// ============================================================

#[derive(Clone)]
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
    /// Phase 4：当前网络模式 + 房间号 + RTT。
    game_mode: GameMode,
    rtt_ms: Option<f32>,
    room_id: String,
    /// 当前走信令 Worker 中继的 peer 数。> 0 时 HUD 显示「RELAY n」徽标。
    relayed_peer_count: usize,
    /// Phase 6：[`AppSettings::show_stats`] 透传。false 时跳过左上角统计面板（保留准星 / hotbar）。
    show_stats: bool,
    depth_prepass_enabled: bool,
    quota: Option<crate::storage::QuotaInfo>,
    current_world_bytes: u64,
    other_worlds_bytes: u64,
    storage_error: Option<String>,
    /// Phase 7：上一帧渲染 / 网格化统计。
    perf: FramePerfStats,
}

fn draw_hud(ctx: &egui::Context, data: HudData) {
    // 左上角 stat（show_stats 关闭时跳过；准星 / hotbar / 提示栏照常显示）
    if data.show_stats {
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
                        ui.colored_label(
                            egui::Color32::from_rgb(160, 175, 190),
                            format!(
                                "DEPTH_PRE {}",
                                if data.depth_prepass_enabled { "ON" } else { "OFF" }
                            ),
                        );
                        if let Some(q) = data.quota {
                            let available_for_world =
                                q.quota.saturating_sub(data.other_worlds_bytes);
                            let ratio = if available_for_world == 0 {
                                1.0
                            } else {
                                data.current_world_bytes as f32 / available_for_world as f32
                            };
                            let color = if ratio > 0.95 {
                                egui::Color32::from_rgb(240, 80, 80)
                            } else if ratio > 0.80 {
                                egui::Color32::from_rgb(230, 190, 80)
                            } else {
                                egui::Color32::from_rgb(160, 200, 170)
                            };
                            ui.colored_label(
                                color,
                                format!(
                                    "SAVE {} / {}",
                                    crate::storage::format_storage_bytes(data.current_world_bytes),
                                    crate::storage::format_storage_bytes(available_for_world)
                                ),
                            );
                        }
                        if let Some(err) = data.storage_error.as_deref() {
                            ui.colored_label(
                                egui::Color32::from_rgb(240, 120, 120),
                                format!("SAVE ERR {err}"),
                            );
                        }
                        ui.colored_label(
                            egui::Color32::from_rgb(160, 175, 190),
                            format!(
                                "VISIBLE {}  CULLED {}  DRAW_V/I {}/{}",
                                data.perf.visible_chunks,
                                data.perf.culled_chunks,
                                data.perf.drawn_vertices,
                                data.perf.drawn_indices
                            ),
                        );
                        let reduction = data
                            .perf
                            .mesh_reduction_percent()
                            .map(|v| format!("{v:>5.1}%"))
                            .unwrap_or_else(|| "  -- ".to_string());
                        ui.colored_label(
                            egui::Color32::from_rgb(170, 200, 210),
                            format!(
                                "MESH {:>4.1}ms  jobs {}  v {}→{}  i {}  -{}",
                                data.perf.mesh_ms,
                                data.perf.mesh_jobs,
                                data.perf.mesh_phase2_vertices,
                                data.perf.mesh_vertices,
                                data.perf.mesh_indices,
                                reduction
                            ),
                        );
                        ui.colored_label(
                            egui::Color32::from_rgb(170, 190, 220),
                            format!(
                                "PASS depth {:>4.1}  world {:>4.1}  player {:>4.1}  trans {:>4.1}  sel {:>4.1}  ui {:>4.1} ms",
                                data.perf.depth_pass_ms,
                                data.perf.world_pass_ms,
                                data.perf.player_pass_ms,
                                data.perf.transparent_pass_ms,
                                data.perf.selection_pass_ms,
                                data.perf.egui_pass_ms
                            ),
                        );
                        // Phase 4：网络模式 + RTT + 房间号
                        let mode_str = match data.game_mode {
                            GameMode::Local => "LOCAL",
                            GameMode::Host => "HOST",
                            GameMode::Remote => "REMOTE",
                        };
                        let rtt_str = match data.rtt_ms {
                            Some(rtt) => format!("{rtt:>5.1} ms"),
                            None => "  --  ".to_string(),
                        };
                        let room_str = if data.room_id.is_empty() {
                            String::new()
                        } else {
                            format!("  ROOM {}", data.room_id)
                        };
                        ui.colored_label(
                            egui::Color32::from_rgb(200, 200, 160),
                            format!("NET {mode_str}  RTT {rtt_str}{room_str}"),
                        );
                        if data.relayed_peer_count > 0 {
                            // 醒目橙色：表示当前有 peer 走信令 Worker 中继
                            ui.colored_label(
                                egui::Color32::from_rgb(240, 165, 80),
                                format!("RELAY {} peer(s) (relaying)", data.relayed_peer_count),
                            );
                        }
                    });
            });
    }

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

/// 在屏幕顶部居中绘制通知浮窗（信令错误等），5 秒自动消失。
/// 多条通知从上到下堆叠，半透明深色背景 + 橙红色文字。
fn draw_toast_notifications(ctx: &egui::Context, messages: &[String]) {
    egui::Area::new(egui::Id::new("toast_notifications"))
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 60.0))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                for msg in messages {
                    egui::Frame::default()
                        .fill(egui::Color32::from_rgba_unmultiplied(40, 20, 20, 200))
                        .corner_radius(egui::CornerRadius::same(6))
                        .inner_margin(egui::Margin::symmetric(16, 8))
                        .show(ui, |ui| {
                            ui.set_max_width(420.0);
                            ui.label(
                                egui::RichText::new(msg)
                                    .color(egui::Color32::from_rgb(240, 140, 120))
                                    .size(14.0),
                            );
                        });
                    ui.add_space(4.0);
                }
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
