# `client` 模块设计

> **何时阅读**：改启动流程；改主循环节奏；改 AppState 状态切换；改输入/相机/UI 集成
> **关联文档**：[`README.md`](../../README.md) · [`architecture.md`](../architecture.md) · [`features/physics.md`](../features/physics.md) · [`features/ui.md`](../features/ui.md) · [`networking/prediction.md`](../networking/prediction.md) · [`features/persistence.md`](../features/persistence.md)

---

## 一、职责

`client` 是 **orchestrator**：把 `core` / `render` / `server` / `net` 粘合成一个 WASM 应用。
- 浏览器入口（`#[wasm_bindgen(start)]`）
- AppState 状态机（Lobby / Connecting / InGame / EscMenu / ChatOpen）
- 主循环（RAF + 固定 60Hz 逻辑帧累加器）
- 输入（键盘/鼠标）、相机控制、本地物理预测、DDA 射线
- UI（egui）— 大厅、HUD、暂停、聊天、玩家列表、名牌
- OPFS 持久化的具体实现（`web_sys::FileSystemDirectoryHandle` + palette/RLE 压缩）
- 网格化任务调度（mesh job queue）

`client` crate 的 cargo crate-type 设为 `["cdylib", "rlib"]`，便于 wasm-bindgen 输出。

---

### 阶段实装范围

| 阶段 | 包含 |
|---|---|
| **Phase 2 ✅** | `AppState::{Lobby, InGame}`；`Game` 子结构持 `server / net / camera / mesh_jobs / chunk_loader`；`ui::lobby` 实装；`mesh_jobs.rs` / `chunk_loader.rs` 新模块；Fly 模式相机 |
| **Phase 3 ✅** | Walk 模式 + `physics.rs` + `raycast.rs`；挖放 hotbar；PendingActions rollback；server validate_break/place 闭环；选中方块线框 SelectionPass；PlayerInput 上报 |
| Phase 4 ✅ | `AppState::Connecting / Disconnected`；`NetEndpoint::Host / Remote`（Ping-Pong 闭环） |
| Phase 5 ✅ | `prediction.rs`（InputHistory + reconcile_self）/ `interp.rs`（PlayerInterp 100ms 延迟）/ `chunk_assembler.rs`（ChunkSnapshot 分片组装）；远端玩家身体渲染（PlayerPass）；outbox+Recipient 路由；OPFS 持久化整体延后至 Phase 8 |
| Phase 8 | 补 `storage.rs` OPFS 完整实现（`WorldStorage` trait + `OpfsStorage` + prime + 按需 load + 1s flush + pagehide） |

下面 §4 起的 `App` 完整结构是**Phase 5+ 终态**。Phase 2 实际仅需子集，标注见 §4。

---

## 二、目录结构

```
crates/client/src/
├── lib.rs              wasm 入口 + 主循环 + HUD 绘制 + 事件监听
├── app.rs              AppState 状态机 + App / Game 主结构
├── camera.rs           第一人称相机
├── input.rs            键盘/鼠标输入管理
├── mesh_jobs.rs        [Phase 2] 网格化任务队列 + 分帧调度
├── chunk_loader.rs     [Phase 2] 区块滚动加载 / 卸载 + affected_chunks 工具
├── physics.rs          [Phase 3 ✅] 玩家本地物理预测（Walk/Fly 双模式）
├── raycast.rs          [Phase 3 ✅] DDA 射线
├── hotbar.rs           [Phase 3 ✅] 9 格快捷栏 + 方块标签
├── prediction.rs       [Phase 3 ✅] 挖放 PendingActions（rollback）；[Phase 5] 完整输入历史+协调
├── interp.rs           [Phase 5 ✅] 远端玩家位置插值（PlayerInterp, 100ms 延迟）
├── chunk_assembler.rs  [Phase 5 ✅] ChunkSnapshot 分片接收与组装
├── storage.rs          [Phase 8] OPFS 异步包装（`WorldStorage` trait + `OpfsStorage`；当前为 stub）
└── ui/
    ├── mod.rs          UI 总入口（按 AppState 路由）
    ├── lobby.rs        [Phase 2] 大厅 + Connecting UI（单机/Host/Remote 入口 + 连接进度）
    ├── hud.rs          [Phase 1+] HUD 绘制函数（供 lib.rs 调用）
    ├── pause.rs        [Phase 6] 暂停菜单（当前 stub）
    ├── chat.rs         [Phase 6] 聊天框 + 消息历史（当前 stub）
    ├── players.rs      [Phase 6] 玩家名牌（3D billboard）+ 玩家列表 widget（当前 stub）
    └── ui_state.rs     UI 状态哈希（防止重复渲染判断）
```

---

## 三、`lib.rs` — wasm 入口

Phase 2 实装：直接通过浏览器原生 API（`add_event_listener_with_callback`）注册事件，不引入 `winit` 事件循环。`#[wasm_bindgen(start)]` 异步函数完成 Renderer + egui 初始化后挂上 RAF 闭包链。

```rust
#[wasm_bindgen(start)]
pub async fn start() -> Result<(), JsValue> {
    console_error_panic_hook::set_once();
    tracing_wasm::set_as_global_default();

    let canvas: HtmlCanvasElement = /* document.get_element_by_id("game") */;
    let renderer = Renderer::new(&canvas).await?;
    let egui_ctx = egui::Context::default();
    let egui_renderer = egui_wgpu::Renderer::new(/* ... */);
    let input = Rc::new(RefCell::new(InputState::default()));
    let egui_events = Rc::new(RefCell::new(Vec::<egui::Event>::new()));

    let app = Rc::new(RefCell::new(App {
        canvas, renderer, egui_ctx, egui_renderer,
        input: input.clone(),
        egui_events: egui_events.clone(),
        state: AppState::Lobby,
        lobby_state: LobbyState::default(),
        game: None,
        last_time_ms, fps_frames: 0, fps_accum: 0.0, fps_display: 0.0,
        request_pointer_lock_next: false,
    }));

    install_event_listeners(&canvas, &document, input, egui_events, app.clone())?;
    spawn_raf_loop(app);
    Ok(())
}
```

**事件路由**（`install_event_listeners`）：
- `click` on canvas → 仅在 InGame 时 `canvas.request_pointer_lock()`
- `pointerlockchange` on document → 写回 `input.pointer_locked`
- `keydown` / `keyup` on document → InGame 时映射到 `InputState`；Lobby 时不消费（让 egui 处理文本输入）
- `mousemove` on document：指针锁时累积相机 dx/dy；否则上报 `egui::Event::PointerMoved`（让 Lobby 按钮能接收 hover）
- `mousedown` / `mouseup`：InGame 时写 InputState，Lobby 时转 `egui::Event::PointerButton`

> 关键修复（commit edae0e6）：早期版本只在 InGame 才向 egui 喂事件，导致大厅按钮无法点击。Phase 2 起 mouse 事件在所有 state 下都会推到 `egui_events` 累加器，每帧 drain 入 `RawInput.events`。

---

## 四、`app.rs` — AppState 与全局 App

### AppState

```rust
pub enum AppState {
    Loading,                                      // [Phase 0+] 初始占位，实际未使用
    Lobby,                                        // [Phase 2+] 大厅
    Connecting,                                   // [Phase 4+] 网络协商 + 区块预载
    InGame { paused: bool, chat_open: bool },     // [Phase 6+] 游戏中（可叠加暂停 / 聊天）
    Disconnected,                                 // [Phase 6+] 连接断开页面
}
impl AppState {
    pub fn is_in_game(&self) -> bool;             // 匹配任意 InGame 子状态
    pub fn ingame_default() -> Self;              // InGame { paused: false, chat_open: false }
}
```

Phase 6 把原 `EscMenu` / `ChatOpen` unit variant 合入 `InGame` 结构变体，让 HUD 与名牌在暂停 / 聊天期间仍可绘制，且两个叠加层可并存。

Connecting 期间不携带子状态结构体——进度信息由 `RoomSession::loading_steps()`（网络步骤）和 `App::preload_state`（区块预载步骤）联合提供。

### 区块预载

`PreloadState` 追踪出生点周围 chunk 的加载进度（Phase 5+）：

```rust
pub struct PreloadState {
    pub total: usize,     // 期望加载的 chunk 总数 = (2*render_distance+1)^2
    pub received: usize,  // 已存在于 world.chunks 中的 chunk 数
    pub meshed: usize,    // 已有 GPU mesh 的 chunk 数
    pub active: bool,     // 预载进行中
}
```

预载完成条件：`received >= total && mesh_jobs.is_empty()`。
用 `received` 而非 `meshed` 判完成是因为全空气 chunk mesh 后顶点为空，不产生 GPU 记录。`mesh_jobs.is_empty()` 确保所有已入队 chunk 都被处理过。

### App / Game 主结构

Phase 2 起，`App` 容器只持有跨状态资源（renderer / egui / input / 大厅 UI state）。游戏内运行时持有于 `Game` 子结构，仅 `InGame` 时存在：

```rust
struct App {
    canvas: HtmlCanvasElement,
    renderer: Renderer,
    egui_ctx: egui::Context,
    egui_renderer: egui_wgpu::Renderer,

    input: Rc<RefCell<InputState>>,
    egui_events: Rc<RefCell<Vec<egui::Event>>>,

    state: AppState,
    lobby_state: LobbyState,

    // —— Connecting 阶段 ——
    connecting_mode: GameMode,          // Host / Remote / Local
    connecting_room_id: String,
    connecting_error: Option<String>,
    preload_state: Option<PreloadState>, // [Phase 5+] 区块预载进度

    disconnect_reason: Option<String>,

    game: Option<Game>,

    last_time_ms: f64,
    fps_frames: u32, fps_accum: f32, fps_display: f32,
    request_pointer_lock_next: bool,
}

pub struct Game {
    pub server: Rc<RefCell<Server>>,   // [Phase 2] Local-Only 持有完整 Server
    pub server_inbox: ServerInbox,     // [Phase 2] mpsc 服务端侧
    pub net: NetEndpoint,              // [Phase 2] ::Local；Phase 4 → Host/Remote
    pub camera: Camera,                // [Phase 1+] 朝向+矩阵；位置由 physics 每帧同步
    pub physics: LocalPhysics,         // [Phase 3 ✅] Walk/Fly + 分轴碰撞 + 重力
    pub hotbar: Hotbar,                // [Phase 3 ✅] 9 格快捷栏
    pub pending: PendingActions,       // [Phase 3 ✅] 挖放 rollback 队列
    pub current_hit: Option<RaycastHit>, // [Phase 3 ✅] DDA 命中缓存
    pub last_break_at_ms: f64,         // [Phase 3 ✅] 连续挖掘冷却
    pub mesh_jobs: MeshJobQueue,       // [Phase 2]
    pub chunk_loader: ChunkLoader,     // [Phase 2]
    pub frame_clock: FrameClock,       // [Phase 2+]
    pub settings: AppSettings,             // [Phase 6+] 用户设置（曾用名 GameSettings）
    pub entity_id: u32,                    // [Phase 2] 由 Welcome 填充
    pub display_name: String,              // [Phase 6] 由 lobby 注入
    pub host_entity_id: EntityId,          // [Phase 6] Host=自己；Remote=Welcome 填

    // —— 以下为后续 Phase 引入 ——
    pub interp: PlayerInterp,          // [Phase 5 ✅]
    pub chunk_assembler: ChunkAssembler, // [Phase 5 ✅]
    pub remote_players: HashMap<EntityId, RemotePlayerState>, // [Phase 5 ✅]
    pub input_history: InputHistory,   // [Phase 5 ✅]
    pub server_clock_offset_ms: i64,   // [Phase 5 ✅]
    pub chat: ChatHistory,             // [Phase 6 ✅] 聊天历史（含本地合成的系统消息）
}

/// 用户设置（[Phase 6 ✅] 取代 [`GameSettings`]；serde JSON 持久化到 localStorage）。
///
/// 开发/调优字段（fly_speed / mesh_budget_ms / min_action_interval_ms）标 `#[serde(skip)]`，
/// 不污染存储，仅在运行时可用。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppSettings {
    pub fov_degrees: f32,          // 默认 70.0，范围 30..=110
    pub mouse_sensitivity: f32,    // 默认 1.0（乘以 BASE_SENSITIVITY_RAD_PER_PIXEL = 0.0025）
    pub render_distance: u32,      // 默认 6，选项 2/4/6/8/10
    pub interp_delay_ms: f32,      // 默认 100.0，选项 50 / 100 / 150
    pub show_stats: bool,          // 默认 true，控制左上角统计面板
    #[serde(skip)]
    pub fly_speed: f32,
    #[serde(skip)]
    pub mesh_budget_ms: f32,
    #[serde(skip)]
    pub min_action_interval_ms: f64,
}

/// 设置 localStorage 持久化。
/// 键 "voxweb.settings.v1"，schema 头字段版本 = 1；schema 不匹配视为不可读、回退默认。
pub mod settings_storage {
    pub fn load() -> Option<AppSettings>;
    pub fn save(settings: &AppSettings);
}

/// [Phase 2+] 固定步长（1/60）累加器：渲染帧的 dt 累加到 `accumulator`，
/// 每次 `consume_logic_step` 扣除一步返回 true；累加上限 0.25s 防止后台 Tab 回前台时风暴。
pub struct FrameClock { /* accumulator: f32, step: f32 */ }
```

> Phase 2 不引入 `world_view`：Local 模式下 mesh 回调直接借 `server.world.get_block_world`。Phase 5 加入 Remote 模式时才有 `WorldView` 副本（由 ChunkSnapshot 喂数据）。

---

## 五、主循环

### Phase 2 主循环（Lobby / InGame 二态）

`render_frame` 按 `app.state` 分流：

```rust
fn render_frame(app: &Rc<RefCell<App>>) -> Result<(), String> {
    let dt = update_clock(app);
    let (cw, ch) = sync_canvas_size(...);

    match app.borrow().state.clone() {
        AppState::InGame => render_game_frame(app, dt, cw, ch),
        AppState::Connecting => render_connecting_frame(app, cw, ch),  // [Phase 4+]
        _ => render_lobby_frame(app, cw, ch),
    }
}
```

#### `render_connecting_frame`（Phase 4+）

专用于 `Connecting` 状态：
1. `poll_net(app)` — 推进网络状态机（信令+WebRTC 协商）
2. **drain Server→Client inbox** — Remote 端收 Welcome / ChunkSnapshot / BlockUpdate 等消息（[Phase 5+]）
3. **区块预载**（[Phase 5+]）：若 `preload_state.active`：
   - Host/Local：调 `chunk_loader.update(DEFAULT_SPAWN, ...)` 生成出生点周围 chunk 并入队网格化
   - Remote：仅运行 `mesh_jobs.run_until_budget(16ms)`（chunk 由上一步的 inbox drain 喂入）
   - 统计 `received`（存在于 `world.chunks`）和 `meshed`（有 GPU mesh）
   - 完成条件 `received >= total && mesh_jobs.is_empty()` → `state = InGame`
4. 构建步骤列表：`RoomSession::loading_steps()`（网络步骤）+ 区块预载步骤
5. 渲染 UI：`draw_connecting(steps)` + 暗色清屏 + egui pass

#### `render_lobby_frame`

1. 取 `egui_events` 累加器 drain 出鼠标事件 → 塞入 `RawInput.events`
2. `egui_ctx.run_ui(...)` 跑大厅 UI（`draw_lobby` 返回 `Option<LobbyAction>`）
3. 若返回 `LobbyAction::StartSinglePlayer { seed }` → `start_single_player(app, seed)`：
   - 用 `getrandom` 抓 8 字节 → u64 seed（输入为空时）
   - `Game::new_local(seed, settings)` 创建 server + mpsc + Camera
   - 发 `ClientMessage::Hello` 入队
   - 置 `state = InGame`、`request_pointer_lock_next = true`
4. 编码 lobby Pass：先 `Clear` 暗蓝色背景，再画 egui

#### `render_game_frame`（9 步，Phase 3）

```rust
fn render_game_frame(app: &Rc<RefCell<App>>, dt: f32, cw: u32, ch: u32) -> Result<(), String> {
    // 1. drain Client→Server → Server.handle_message → 推回 outbox
    // 2. drain Server→Client → apply_server_message
    //    （Welcome / ActionAck / BlockUpdate）

    // 3. 输入 → 物理 + 动作
    //    - 鼠标 apply_mouse
    //    - hotbar_request 消费
    //    - fly_toggle_pending 切换 Walk/Fly
    //    - physics.step(get_block, &camera, &input, dt) → camera.position 同步
    //    - 60Hz 逻辑帧：frame_clock.accumulate + consume → server.tick + PlayerInput 发送
    //    - DDA raycast → current_hit
    //    - dispatch_actions（挖放边沿+冷却+乐观记录+发消息）
    //    - input.reset_delta()

    // 4. ChunkLoader 滚动（&mut Server / &mut MeshJobQueue / &mut Renderer）
    // 5. mesh_jobs.run_until_budget
    // 6. egui HUD：FPS / POS / MODE / CHUNKS / 准星 / Hotbar 9 格

    // 7. wgpu 渲染 + present
    //    - OpaquePass（render_world）
    //    - SelectionPass（render_selection，current_hit 时才画）
    //    - egui Pass
    Ok(())
}
```

> **借用顺序**（避免 RefCell 二次借用 / borrow_mut 冲突）：
> 1. 先 `app.borrow_mut()` drain mpsc inbox
> 2. 后续 ChunkLoader.update 需 `&mut Server` + `&mut Renderer`，用结构体解构借出
> 3. mesh_jobs.run_until_budget 用 `server.borrow()`（immutable）+ `&mut Renderer`，与上一步不重叠

### Phase 5+ 完整主循环（前瞻）

```rust
fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
    let dt = self.frame_clock.tick();

    self.process_inbound();              // 网络入站
    self.update_camera(dt);              // 即时响应

    self.frame_clock.accumulate(dt);
    while self.frame_clock.consume_logic_step() {
        self.update_logic(LOGIC_DT);     // 含 prediction.reconcile
    }

    self.interp.advance(dt);             // 远端玩家插值
    self.mesh_jobs.run_until_budget(MESH_BUDGET_MS, &mut self.renderer);
    let egui_output = self.build_ui();
    self.renderer.render_frame(&self.frame_data(), egui_output);
    self.maybe_flush_persistence();      // 每 1 秒触发一次 OPFS 写入（snapshot dirty + 异步 save）
}
```

---

## 六、各子模块速览

### 6.1 `camera.rs`

```rust
pub struct Camera {
    pub position: glam::Vec3,    // [Phase 3] 由 LocalPhysics.eye_position() 每帧同步
    pub yaw: f32,                // 弧度
    pub pitch: f32,
    pub fov: f32,
    pub aspect: f32,
    pub near: f32,
    pub far: f32,
}

pub enum CameraMode {
    Walk,    // [Phase 3 ✅] 受重力、AABB 碰撞
    Fly,     // [Phase 1+] 无重力、自由飞行
}

impl Camera {
    pub fn forward(&self) -> Vec3;              // 单位向量（含 pitch）
    pub fn forward_horizontal(&self) -> Vec3;   // [Phase 3] XZ 面单位向量，丢弃 pitch
    pub fn right(&self) -> Vec3;                // 水平面右向
    pub fn view_matrix(&self) -> Mat4;
    pub fn projection_matrix(&self) -> Mat4;
    pub fn vp_matrix(&self) -> Mat4;
    pub fn apply_mouse(&mut self, dx: f32, dy: f32, sensitivity: f32);
}
```

`Camera` **不**持有移动输入逻辑：位置由 `LocalPhysics` 每帧 `eye_position()` 同步覆盖；Fly 移动也归入 physics（Phase 3 删除了 `apply_fly_input`，统一在 `LocalPhysics::step_fly` 中实现）。

### 6.2 `input.rs`

Phase 3 实装是**扁平 InputState**：每帧累加按键 / 鼠标事件，按"持续"vs"边沿"两类语义存储；帧末 `reset_delta()` 清掉所有边沿，保留持续按下状态。WASM 侧浏览器事件经 `lib.rs::map_key`（`KeyboardEvent.code` → `winit::keyboard::KeyCode`）路由到 `on_key_down/up`。

```rust
pub struct InputState {
    // —— WASD 持续按下 ——
    pub forward: bool, pub backward: bool, pub left: bool, pub right: bool,
    // —— 跳跃 / 蹲伏 ——
    pub jump_held: bool,         // 持续：Fly 上升 / Walk 见 jump_just_pressed
    pub jump_just_pressed: bool, // 边沿：Walk 起跳触发
    pub sneak: bool,             // 持续：Fly 下降 / 后续 Walk 蹲伏
    // —— 鼠标按键 ——
    pub break_held: bool, pub break_just_pressed: bool,    // 0 = 左
    pub place_held: bool, pub place_just_pressed: bool,    // 2 = 右；1 中键忽略
    // —— 边沿事件 ——
    pub hotbar_request: Option<u8>,  // Digit1..=Digit9 → Some(0..=8)
    pub fly_toggle_pending: bool,    // 双击空格 250ms 内触发
    pub chat_open: bool, pub esc_menu: bool,
    // —— 鼠标移动 ——
    pub mouse_dx: f32, pub mouse_dy: f32,
    pub pointer_locked: bool,
    // —— 内部：双击空格判定时间戳（performance.now() ms）——
    last_space_press_at_ms: Option<f64>,
}

impl InputState {
    pub fn on_key_down(&mut self, key: KeyCode, now_ms: f64);
    pub fn on_key_up(&mut self, key: KeyCode);
    pub fn on_mouse_down(&mut self, button: u16);  // 0/1/2 = 左/中/右
    pub fn on_mouse_up(&mut self, button: u16);
    pub fn on_mouse_move(&mut self, dx: f32, dy: f32);
    /// 帧末调用：清边沿（mouse_dx/dy / *_just_pressed / hotbar_request / fly_toggle_pending / chat_open / esc_menu），
    /// 保留持续状态（forward..right / *_held / sneak / pointer_locked）
    pub fn reset_delta(&mut self);
}
```

**指针锁**：进入 InGame 时调用 `canvas.request_pointer_lock()`（必须由用户手势触发，故"开始游戏"按钮的点击事件中发起）。`pointerlockchange` 监听器写回 `pointer_locked`，主循环只在 `pointer_locked=true` 时消费 `mouse_dx/dy` 和 WASD（避免后台移动）。ESC 释放。

### 6.7 `mesh_jobs.rs` · `chunk_loader.rs`（[Phase 2]）

**MeshJobQueue**：

```rust
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MeshPriority {
    Critical = 0,   // 玩家正站立的 chunk
    High = 1,       // 玩家附近 1 chunk 范围
    Medium = 2,     // 渲染距离内其它
    Low = 3,        // 邻居加载触发的重网格化 / 边界 chunk
}

pub struct MeshJobQueue {
    queues: [VecDeque<ChunkPos>; 4],   // 按 MeshPriority 索引
    pending: HashSet<ChunkPos>,        // 防重 / cancel
}

impl MeshJobQueue {
    pub fn enqueue(&mut self, pos: ChunkPos, priority: MeshPriority);
    pub fn cancel(&mut self, pos: ChunkPos);
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    /// `now_ms` 注入便于测试与平台抽象，运行期传 `&now_ms`（封装 performance.now()）
    pub fn run_until_budget(
        &mut self,
        budget_ms: f32,
        server: &Server,
        renderer: &mut Renderer,
        now_ms: &dyn Fn() -> f64,
    );
}
```

`run_until_budget` 每次取最高优先级队列 head，从 `server.world.chunks.get(&pos)` 取出 chunk（若已被卸载就跳过），调 `chunk_mesh::generate_with_neighbors(chunk, pos, &|wx,wy,wz| server.world.get_block_world(...))`，结果通过 `renderer.upload_chunk_mesh` 上传。`(now_ms() - start) as f32 >= budget_ms` 时退出。

> **防重语义**：同 chunk 二次 `enqueue` 被忽略，保留最早的优先级（不"升级"）。如需调整优先级，先 `cancel` 再 `enqueue`。

**ChunkLoader**：

```rust
pub struct ChunkLoader {
    pub render_distance: i32,          // 默认 6
    pub unload_buffer: i32,            // 常数 3（实际卸载半径 = render_distance + buffer）
    pub loaded: HashSet<ChunkPos>,
    last_center: Option<ChunkPos>,
}

impl ChunkLoader {
    pub fn new(render_distance: u32) -> Self;
    pub fn invalidate(&mut self);     // 下一次 update 强制重算
    /// 返回是否发生了变更（用于调试 / 性能 stat）
    pub fn update(
        &mut self,
        camera_pos: Vec3,
        server: &mut Server,
        mesh_jobs: &mut MeshJobQueue,
        renderer: &mut Renderer,
    ) -> bool;
}

// 工具函数
pub fn chunk_pos_of(world_pos: Vec3) -> ChunkPos;       // div_euclid 处理负坐标
pub fn chebyshev_distance(a: ChunkPos, b: ChunkPos) -> i32;
pub fn priority_for_distance(pos: ChunkPos, center: ChunkPos) -> MeshPriority;
//   d == 0 → Critical, d == 1 → High, d >= 2 → Medium
```

`update` 行为：
1. 算出当前 chunk 中心；若与 `last_center` 相同则直接返回 false（chunk 内移动不触发）
2. 计算期望集合（`render_distance` 半径切比雪夫方形）
3. 新增 chunk：`desired - loaded`，逐个 `server.world.ensure_chunk_generated(pos)` → `mesh_jobs.enqueue(pos, prio)`
4. 对**这一批新 chunk** 的 4 个水平邻居（已在 `loaded` 中且 `renderer.has_chunk_mesh(neighbor) == true`）以 `MeshPriority::Low` 重新入队，使跨区块剔除生效
5. 卸载：`loaded` 中切比雪夫距离 `> render_distance + unload_buffer` 的 chunk → `server.unload_chunk` + `mesh_jobs.cancel` + `renderer.drop_chunk_mesh`

> **借用顺序**：ChunkLoader.update 需要 `&mut Server`，mesh_jobs.run_until_budget 需要 `&Server`。两段按序执行，不重叠。Phase 2 主循环（[`crates/client/src/lib.rs`](../../crates/client/src/lib.rs)）严格遵守此顺序。

### 6.8 `physics.rs`（[Phase 3 ✅]，详见 [`features/physics.md`](../features/physics.md)）
`LocalPhysics { feet_position, velocity, on_ground, mode }`—— Walk 模式：重力（−32 m/s²）、跳跃（8.4 m/s）、lerp 水平加速（HORIZ_ACC=12）、Y/X/Z 分轴碰撞、5cm 地面探测。Fly 模式：直接 `position += dir * FLY_SPEED * dt`。位置驱动 `camera.position = physics.eye_position()`。

### 6.9 `raycast.rs`（[Phase 3 ✅]，详见 [`features/physics.md`](../features/physics.md)）
Amanatides & Woo DDA 网格步进，最大射程 6 格。`RaycastHit { pos, normal: IVec3, face: Face, distance }`——`Face` 复用 `render::vertex::Face`。命中条件 `block != AIR && properties(block).solid`。

### 6.10 `hotbar.rs`（[Phase 3 ✅]）
`Hotbar { items: [BlockID; 9], selected: usize }`，1-9 键切换（`InputState::hotbar_request` 边沿）。`block_label(id) -> &'static str` 供 HUD 显示。

### 6.11 `prediction.rs`（[Phase 3 ✅]/[Phase 5 完整]，详见 [`networking/prediction.md`](../networking/prediction.md)）
Phase 3 已实装 `PendingActions`：挖放操作乐观记录 + ActionAck 协调（commit/rollback）。Phase 5 补输入历史缓冲 + PlayerTick 协调（位置预测）。

### 6.11 `interp.rs`（[Phase 5 ✅]）

远端玩家位置插值缓冲区：

```rust
pub struct PlayerInterp {
    buffers: HashMap<EntityId, RemotePlayerBuffer>,
    interp_delay_ms: f32,    // 默认 100ms，平衡平滑与延迟
}

pub struct RemotePlayerBuffer {
    snapshots: VecDeque<TimedSnapshot>,    // 按 server_time_ms 排序
    interpolated_pos: Vec3,
    interpolated_yaw: f32,
    interpolated_pitch: f32,
}

impl PlayerInterp {
    pub fn ingest_tick(&mut self, players: &[PlayerSnapshot], server_time_ms: u64);
    pub fn advance(&mut self, dt: f32);
    pub fn current(&self, entity: EntityId) -> Option<(Vec3, f32, f32)>;
}
```

### 6.12 `storage.rs`（[Phase 8]，Phase 5 仅保留 stub 签名）

```rust
#[async_trait::async_trait(?Send)]
pub trait WorldStorage {
    async fn open(room_id: &str, seed: u64) -> Result<Self, StorageError> where Self: Sized;
    async fn list_chunks(&self) -> Result<Vec<ChunkPos>, StorageError>;
    async fn load_chunk(&self, pos: ChunkPos) -> Result<Option<Vec<u8>>, StorageError>;
    async fn save_chunks(&self, items: Vec<(ChunkPos, Vec<u8>)>) -> Result<(), StorageError>;
    async fn delete_world(&self) -> Result<(), StorageError>;
    async fn quota(&self) -> Option<QuotaInfo>;
}

pub struct OpfsStorage {
    root: web_sys::FileSystemDirectoryHandle,         // opfs:/voxweb/<world_key>/
    chunks_dir: web_sys::FileSystemDirectoryHandle,
    world_key: String,
}
```

**调用模式**：所有方法返回 future。client 在合适时机用 `wasm_bindgen_futures::spawn_local(async move { ... })` 启动，完成后通过 `futures-channel::oneshot` 把结果投递回主循环。chunk 字节流的 encode/decode 由 [`crates/core/src/chunk.rs`](../../crates/core/src/chunk.rs) 的 `encode` / `decode`（palette + RLE）负责，storage 层只处理原始字节。

详见 [`features/persistence.md`](../features/persistence.md)。

---

## 七、UI 子模块概览

详见 [`features/ui.md`](../features/ui.md)。模块划分：
- `ui::lobby` — 大厅
- `ui::hud` — HUD（坐标、玩家列表、聊天叠层、准星）
- `ui::pause` — ESC 菜单
- `ui::chat` — 聊天框（输入与历史）
- `ui::players` — 远端玩家名牌（特殊：在 3D 空间渲染，需要 `egui::Painter` + 投影计算）

`ui::mod::draw(app)` 按 `AppState` 路由：

```rust
pub fn draw(app: &mut App, ctx: &egui::Context) {
    match app.state {
        AppState::Lobby => lobby::draw(app, ctx),
        AppState::Connecting { .. } => connecting::draw(app, ctx),
        AppState::InGame { paused, chat_open } => {
            hud::draw(app, ctx);
            players::draw_nameplates(app, ctx);
            if chat_open { chat::draw(app, ctx); }
            if paused { pause::draw(app, ctx); }
        }
        AppState::Disconnected { reason } => disconnected::draw(app, ctx, reason),
    }
}
```

---

## 八、状态切换流程

### Lobby → Connecting → InGame
1. 用户在大厅点击"单人模式"/"创建房间"/"加入房间"
2. 创建 `Game` 实例（`new_local` / `new_host` / `new_remote`）
3. `app.state = Connecting`，开启 `render_connecting_frame` 循环
4. **网络协商**（仅 Host/Remote）：信令 → 注册 → offer/answer → DataChannel open
   - Host：信令注册成功后即视为网络 Connected
   - Remote：等 DataChannel 打开后才 Connected
   - Local：跳过此步，直接进入区块预载
5. **区块预载**（[Phase 5+] 全部三种模式）：网络 Connected 后启动，加载出生点周围 `(2*render_distance+1)^2` 个 chunk：
   - Host/Local：本地 `chunk_loader.update(DEFAULT_SPAWN, ...)` 生成 + 入队，每帧 `mesh_jobs.run_until_budget(16ms)` 网格化
   - Remote：等 Host 发来的 `ChunkSnapshot` 消息到达（inbox drain 在每帧 `poll_net` 之后），入队网格化
   - 完成条件：`received >= total && mesh_jobs.is_empty()` → `state = InGame`
6. 进入 InGame 后请求指针锁

### InGame → EscMenu
- ESC 键按下 → `paused = true`，释放指针锁
- 再按 ESC 或点击"返回游戏" → `paused = false`，请求指针锁

### InGame → ChatOpen
- T 键按下 → `chat_open = true`，输入路由到 chat input buffer
- Enter 提交 → 发 `ClientMessage::Chat`，`chat_open = false`
- ESC 取消 → `chat_open = false`

### Any → Disconnected
- 收到 `RoomEvent::Disconnected`，渲染断线提示页

---

## 九、配置与设置

```rust
pub struct AppSettings {
    pub display_name: String,
    pub mouse_sensitivity: f32,         // 0.1..=5.0
    pub fov_degrees: f32,               // 30..=110
    pub render_distance_chunks: u32,    // 2..=10
    pub vsync: bool,                    // 浏览器侧 RAF 自动 vsync，此项仅作占位
    pub depth_prepass: bool,
    pub interp_delay_ms: f32,
    pub show_stats: bool,
}
```

存放：localStorage（轻量配置）；不进 OPFS（OPFS 留给世界数据）。

---

## 十、错误与日志

- `console_error_panic_hook` 把 Rust panic 输出到浏览器 console
- `tracing-wasm` 桥接 `tracing` 宏到 `console.log`/`console.error`
- 所有 `Err` 路径必须 `tracing::error!` 一次再决定是否传给 UI（避免静默失败）
- UI 错误显示：`AppState::Disconnected { reason }` + 大厅角落的 toast（v2）

---

## 十一、性能与体积

| 指标 | 目标 |
|---|---|
| WASM 包体积（gz） | < 6MB |
| 启动到大厅可交互 | < 2s（中等网络） |
| 60fps 维持率（中等设备 + 渲染距离 6） | > 95% |
| 内存峰值 | < 512MB（Tab 级别） |

降体积手段：
- `[profile.release] lto = "fat"`、`codegen-units = 1`、`opt-level = "z"`
- 构建后 `wasm-opt -Oz`
- 不引入大依赖（如 `winit` 已包含很多 web 后端代码，不可避免）
- 字符串本地化只内嵌中文（v2 抽英文）

---

## 十二、不在范围

- 多窗口（浏览器 Tab 即窗口）
- 全屏 API（v2，按 F11 触发 `requestFullscreen`）
- 控制器/手柄输入（gamepad API；v2）
- 帧率限制 throttle（依赖浏览器 RAF；提供 `vsync` 占位但不实装）
- 不同语言切换（仅中文）
- 启动画面（splash） — 直接显示大厅
