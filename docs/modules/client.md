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
- IndexedDB 持久化的具体实现（`idb` crate）
- 网格化任务调度（mesh job queue）

`client` crate 的 cargo crate-type 设为 `["cdylib", "rlib"]`，便于 wasm-bindgen 输出。

---

### 阶段实装范围

| 阶段 | 包含 |
|---|---|
| **Phase 2 ✅** | `AppState::{Lobby, InGame}`；`Game` 子结构持 `server / net / camera / mesh_jobs / chunk_loader`；`ui::lobby` 实装；`mesh_jobs.rs` / `chunk_loader.rs` 新模块；Fly 模式相机 |
| Phase 3 | Walk 模式 + `physics.rs` + `raycast.rs`；挖放 hotbar |
| Phase 4 | `AppState::Connecting / Disconnected`；`NetEndpoint::Host / Remote` |
| Phase 5 | `prediction.rs` / `interp.rs` 实装；`storage.rs` IndexedDB；远端玩家身体渲染 |
| Phase 6 | `ui::chat / players / pause` 完整；EscMenu / ChatOpen 状态 |

下面 §4 起的 `App` 完整结构是**Phase 5+ 终态**。Phase 2 实际仅需子集，标注见 §4。

---

## 二、目录结构

```
crates/client/src/
├── lib.rs              wasm 入口 + 主循环
├── app.rs              AppState 状态机 + App / Game 主结构
├── camera.rs           第一人称相机
├── input.rs            键盘/鼠标输入管理
├── mesh_jobs.rs        [Phase 2] 网格化任务队列 + 分帧调度
├── chunk_loader.rs     [Phase 2] 区块滚动加载 / 卸载
├── physics.rs          [Phase 3] 玩家本地物理预测
├── raycast.rs          [Phase 3] DDA 射线
├── prediction.rs       [Phase 5] 客户端预测协调
├── interp.rs           [Phase 5] 远端玩家位置插值
├── storage.rs          [Phase 5] IndexedDB 异步包装
└── ui/
    ├── mod.rs          UI 总入口（按 AppState 路由）
    ├── lobby.rs        [Phase 2] 大厅：单机模式按钮 + 种子（Phase 4 加 Host/Join）
    ├── hud.rs          [Phase 1+] HUD：FPS / 坐标 / 玩家列表 / 准星
    ├── pause.rs        [Phase 6] 暂停菜单
    ├── chat.rs         [Phase 6] 聊天框 + 消息历史
    └── players.rs      [Phase 6] 玩家名牌（3D billboard）+ 玩家列表 widget
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
    Loading,                              // [Phase 0+]
    Lobby,                                // [Phase 2+] 大厅，未联网
    Connecting { progress: ConnectingProgress },  // [Phase 4+] 信令 + ICE 阶段
    InGame,                               // [Phase 2+]（Phase 6 加 paused / chat_open 状态位）
    EscMenu,                              // [Phase 6]
    ChatOpen,                             // [Phase 6]
    Disconnected { reason: String },      // [Phase 4+]
}

// [Phase 4+]
pub struct ConnectingProgress {
    pub stage: ConnectingStage,
    pub error: Option<String>,
}

pub enum ConnectingStage {
    SignalingHandshake,
    PeerOfferAnswer,
    IceGathering,
    DataChannelOpening,
    SnapshotReceiving { received: u32, total: u32 },
}
```

### App / Game 主结构

Phase 2 起，`App` 容器只持有跨状态资源（renderer / egui / input / 大厅 UI state）。游戏内运行时持有于 `Game` 子结构，仅 `InGame` 时存在：

```rust
struct App {
    canvas: HtmlCanvasElement,
    renderer: Renderer,
    egui_ctx: egui::Context,
    egui_renderer: egui_wgpu::Renderer,

    input: Rc<RefCell<InputState>>,
    /// 浏览器 mouse 事件累加器：每帧 drain 到 egui RawInput.events
    egui_events: Rc<RefCell<Vec<egui::Event>>>,

    state: AppState,
    lobby_state: LobbyState,           // [Phase 2] 大厅输入框文本等
    game: Option<Game>,                // [Phase 2] InGame 时存在

    // 帧计时 / FPS / 一次性的 InGame 指针锁请求
    last_time_ms: f64,
    fps_frames: u32, fps_accum: f32, fps_display: f32,
    request_pointer_lock_next: bool,
}

pub struct Game {
    pub server: Rc<RefCell<Server>>,   // [Phase 2] Local-Only 持有完整 Server
    pub server_inbox: ServerInbox,     // [Phase 2] mpsc 服务端侧
    pub net: NetEndpoint,              // [Phase 2] ::Local；Phase 4 → Host/Remote
    pub camera: Camera,                // [Phase 1+]
    pub mesh_jobs: MeshJobQueue,       // [Phase 2]
    pub chunk_loader: ChunkLoader,     // [Phase 2]
    pub frame_clock: FrameClock,       // [Phase 2+]
    pub settings: GameSettings,        // [Phase 2+]
    pub entity_id: u32,                // [Phase 2] 由 Welcome 填充；Phase 2 固定为 1

    // —— 以下为后续 Phase 引入 ——
    pub physics: ClientPhysics,        // [Phase 3]
    pub prediction: Prediction,        // [Phase 5]
    pub interp: PlayerInterp,          // [Phase 5]
    pub world_view: WorldView,         // [Phase 5] Remote 模式用；Local 直接 borrow server.world
    pub storage: IndexedDbStorage,     // [Phase 5]
    pub chat: ChatHistory,             // [Phase 6]
}

/// [Phase 2+]
pub struct GameSettings {
    pub render_distance: u32,          // 默认 6
    pub mouse_sensitivity: f32,        // 默认 0.0025
    pub fly_speed: f32,                // 默认 12.0 方块/秒
    pub mesh_budget_ms: f32,           // 默认 4.0
}

/// [Phase 2+] 固定步长（1/60）累加器：渲染帧的 dt 累加到 `accumulator`，
/// 每次 `consume_logic_step` 扣除一步返回 true；累加上限 0.25s 防止后台 Tab 回前台时风暴。
pub struct FrameClock { /* accumulator: f32, step: f32 */ }
```

> Phase 2 不引入 `world_view`：Local 模式下 mesh 回调直接借 `server.world.get_block_world`。Phase 5 加入 Remote 模式时才有 `WorldView` 副本（由 ChunkSnapshot 喂数据）。

---

## 五、主循环

### Phase 2 主循环（Lobby / InGame 二态）

`render_frame` 按 `app.state` 分流到两个分支。Loading / 未启用态全部 fall back 到 Lobby。

```rust
fn render_frame(app: &Rc<RefCell<App>>) -> Result<(), String> {
    let dt = update_clock(app);            // 计算 dt + 滚动 FPS
    let (cw, ch) = sync_canvas_size(...);  // 同步 canvas client size + Renderer.resize

    match app.borrow().state.clone() {
        AppState::InGame => render_game_frame(app, dt, cw, ch),
        _ => render_lobby_frame(app, cw, ch),   // Loading / Lobby / 后续未启用态
    }
}
```

#### `render_lobby_frame`

1. 取 `egui_events` 累加器 drain 出鼠标事件 → 塞入 `RawInput.events`
2. `egui_ctx.run_ui(...)` 跑大厅 UI（`draw_lobby` 返回 `Option<LobbyAction>`）
3. 若返回 `LobbyAction::StartSinglePlayer { seed }` → `start_single_player(app, seed)`：
   - 用 `getrandom` 抓 8 字节 → u64 seed（输入为空时）
   - `Game::new_local(seed, settings)` 创建 server + mpsc + Camera
   - 发 `ClientMessage::Hello` 入队
   - 置 `state = InGame`、`request_pointer_lock_next = true`
4. 编码 lobby Pass：先 `Clear` 暗蓝色背景，再画 egui

#### `render_game_frame`（8 步）

```rust
fn render_game_frame(app: &Rc<RefCell<App>>, dt: f32, cw: u32, ch: u32) -> Result<(), String> {
    // 1. drain Client→Server（ServerInbox.try_recv_client_message）
    //    → Server.handle_message(entity_id, msg)
    //    → 把 replies 推回 ServerInbox.send_server_message
    // 2. drain Server→Client（net.try_recv_server_message）→ apply_server_message
    //    （Phase 2：仅 Welcome 把 entity_id 写回 game.entity_id）

    // 3. 输入 → 相机（Fly 模式）
    //    - 指针锁时 apply_mouse(dx, dy, sensitivity)
    //    - apply_fly_input(input, fly_speed, dt)
    //    - input.reset_delta()

    // 4. 60Hz 逻辑帧累加器
    //    frame_clock.accumulate(dt);
    //    while frame_clock.consume_logic_step() { server.tick(); }

    // 5. ChunkLoader 滚动（&mut Server / &mut MeshJobQueue / &mut Renderer）
    //    见 §6.7 — 同 chunk 内移动不触发；跨边界时算 diff
    // 6. mesh_jobs.run_until_budget(mesh_budget_ms, &Server, &mut Renderer, &now_ms)
    //    见 §6.7

    // 7. egui HUD：FPS / POS / YAW PITCH / CHUNKS / MESH_Q / 准星 / 底部提示
    // 8. wgpu 渲染 + present
    //    - Renderer::render_world（OpaquePass：清屏 + 多 chunk Pass）
    //    - egui Pass（Load）
    //    - 若 request_pointer_lock_next == true → canvas.request_pointer_lock()
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
    self.maybe_flush_persistence();      // 每 30 秒 IndexedDB 写入
}
```

---

## 六、各子模块速览

### 6.1 `camera.rs`

```rust
pub struct Camera {
    pub position: glam::Vec3,
    pub yaw: f32,        // 弧度
    pub pitch: f32,
    pub fov_radians: f32,
    pub aspect: f32,
    pub near: f32,
    pub far: f32,
    pub mode: CameraMode,
}

pub enum CameraMode {
    Walk,    // 受重力，本地物理生效
    Fly,     // 调试用，无重力
}

impl Camera {
    pub fn forward(&self) -> Vec3;     // 单位向量
    pub fn right(&self) -> Vec3;
    pub fn up(&self) -> Vec3;          // 通常恒为 (0,1,0) 不依赖 pitch
    pub fn view_matrix(&self) -> Mat4;
    pub fn proj_matrix(&self) -> Mat4;
    pub fn build_uniform(&self, time_seconds: f32) -> CameraUniform;
}
```

`Camera` **不**持有移动输入逻辑，仅是数据。控制由 `client::physics::apply_input(camera, input, dt)` 完成。

### 6.2 `input.rs`

```rust
pub struct InputManager {
    keys: HashSet<KeyCode>,
    mouse_buttons: [ButtonState; 3],     // Left, Right, Middle
    mouse_delta: Vec2,                   // 当前帧累积
    chat_text_buffer: String,            // 聊天输入时切换接收
}

#[derive(Copy, Clone)]
pub struct ButtonState { pub held: bool, pub just_pressed: bool, pub just_released: bool }

impl InputManager {
    pub fn handle_keyboard(&mut self, ev: &KeyEvent);
    pub fn handle_mouse_button(&mut self, button: MouseButton, state: ElementState);
    pub fn handle_mouse_motion(&mut self, dx: f32, dy: f32);
    pub fn end_frame(&mut self);         // 清掉 just_pressed/just_released 与 mouse_delta
    pub fn key_held(&self, k: KeyCode) -> bool;
    pub fn button(&self, b: MouseButton) -> ButtonState;
}
```

**指针锁**：进入 InGame 时调用 `canvas.request_pointer_lock()`（必须由用户手势触发，故"开始游戏"按钮的点击事件中发起）。ESC 释放。

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

### 6.8 `physics.rs`（[Phase 3]，详见 [`features/physics.md`](../features/physics.md)）
本地玩家 AABB 物理预测（重力、跳跃、分轴碰撞）。仅作"乐观更新"，Host 仲裁后通过 prediction 模块协调。

### 6.9 `raycast.rs`（[Phase 3]，详见 [`features/physics.md`](../features/physics.md)）
DDA 算法，最大射程 6 格。返回命中的方块位置 + 命中面（用于放方块时计算邻居位置）。

### 6.10 `prediction.rs`（[Phase 5]，详见 [`networking/prediction.md`](../networking/prediction.md)）
- 玩家位置：本地立即更新 + 收到 PlayerTick 后做软协调（误差小则插补，超过阈值则瞬移）
- 方块挖放：先发请求 + 本地半透明预览 → 收到 ActionAck 决定 commit 或 rollback

### 6.11 `interp.rs`（[Phase 5]）

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

### 6.12 `storage.rs`（[Phase 5]）

```rust
pub struct IndexedDbStorage {
    db: idb::Database,
}

impl IndexedDbStorage {
    pub async fn open(room_id: &str) -> Result<Self, idb::Error>;
    pub async fn save_chunks(&self, chunks: Vec<(ChunkPos, Chunk)>) -> Result<(), idb::Error>;
    pub async fn load_chunk(&self, pos: ChunkPos) -> Result<Option<Chunk>, idb::Error>;
    pub async fn delete_world(&self) -> Result<(), idb::Error>;
}
```

**调用模式**：所有方法返回 future。client 在合适时机用 `wasm_bindgen_futures::spawn_local(async move { ... })` 启动，完成后通过 `futures-channel::oneshot` 把结果投递回主循环。

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
1. 用户在大厅点击"创建房间"或"加入房间"
2. `app.state = Connecting { stage: SignalingHandshake }`
3. spawn `NetEndpoint::host(url, room).await` 或 `::join(...).await`
4. 完成后初始化 `server`（Host/Local-Only）或仅 `WorldView`（Remote）
5. Remote：等收到 `Welcome` + 全部 `ChunkSnapshot` → `app.state = InGame`
6. Host：收到 `peer_id` 后立即 `InGame`（自己的 chunks 由本地 server 生成）

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

存放：localStorage（轻量配置）；不进 IndexedDB（IndexedDB 留给世界数据）。

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
