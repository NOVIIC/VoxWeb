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

## 二、目录结构

```
crates/client/src/
├── lib.rs              wasm 入口 + 主循环
├── app.rs              AppState 状态机 + 全局 App 持有所有子模块
├── camera.rs           第一人称相机
├── input.rs            键盘/鼠标输入管理
├── physics.rs          玩家本地物理预测（详见 features/physics.md）
├── raycast.rs          DDA 射线
├── prediction.rs       客户端预测协调（详见 networking/prediction.md）
├── interp.rs           远端玩家位置插值
├── mesh_jobs.rs        网格化任务队列 + 分帧调度
├── storage.rs          IndexedDB 异步包装（实现 server::ChunkStorage trait）
└── ui/
    ├── mod.rs          UI 总入口（按 AppState 路由）
    ├── lobby.rs        大厅：昵称 + 房间号 + Host/Join
    ├── hud.rs          HUD：FPS / 坐标 / 玩家列表 / 准星
    ├── pause.rs        暂停菜单：FOV / 灵敏度 / 渲染距离 / 退出
    ├── chat.rs         聊天框 + 消息历史
    └── players.rs      玩家名牌（3D billboard）+ 玩家列表 widget
```

---

## 三、`lib.rs` — wasm 入口

```rust
use wasm_bindgen::prelude::*;

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
    tracing_wasm::set_as_global_default();

    wasm_bindgen_futures::spawn_local(async {
        if let Err(e) = run().await {
            web_sys::console::error_1(&format!("VoxWeb fatal: {e:?}").into());
        }
    });
}

async fn run() -> Result<(), AppError> {
    let canvas = grab_canvas("game")?;     // <canvas id="game">
    let renderer = Renderer::new(canvas.clone()).await?;
    let mut app = App::new(renderer, canvas).await?;

    // 由 winit 的 web 后端把 RAF 接管 → ApplicationHandler::about_to_wait 中触发渲染
    app.run_event_loop()?;
    Ok(())
}
```

---

## 四、`app.rs` — AppState 与全局 App

### AppState

```rust
pub enum AppState {
    Lobby,                                // 大厅，未联网
    Connecting { progress: ConnectingProgress },  // 信令 + ICE 阶段
    InGame { paused: bool, chat_open: bool },
    Disconnected { reason: String },      // 显示原因 + "返回大厅"按钮
}

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

### App 主结构

```rust
pub struct App {
    pub state: AppState,
    pub renderer: Renderer,
    pub egui_ctx: egui::Context,
    pub egui_state: egui_winit::State,
    pub input: InputManager,
    pub camera: Camera,
    pub server: Option<Server>,           // Local-Only / Host 角色才持有
    pub net: NetEndpoint,
    pub world_view: WorldView,            // 客户端持有的世界视图（远端发来的状态）
    pub physics: ClientPhysics,
    pub prediction: Prediction,
    pub interp: PlayerInterp,
    pub mesh_jobs: MeshJobQueue,
    pub storage: IndexedDbStorage,        // 仅 Host/Local-Only 使用
    pub settings: AppSettings,
    pub config: ServerConfig,
    pub frame_clock: FrameClock,
    pub chat: ChatHistory,
    pub canvas: web_sys::HtmlCanvasElement,
}
```

`WorldView` 是客户端持有的"已知世界"（包括 Local-Only 自身完整世界，或 Remote 端从 Host 收到的部分）。结构与 `server::World` 相似但不持有玩家权威表。

---

## 五、主循环

主循环运行在 winit 的 `ApplicationHandler::about_to_wait`（每个 RAF 触发一次）：

```rust
fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
    let dt = self.frame_clock.tick();   // 真实经过时间，单位秒

    // 1. 处理累积的网络入站消息
    self.process_inbound();

    // 2. 输入 → 相机（即时响应，不等待逻辑帧）
    self.update_camera(dt);

    // 3. 逻辑帧累加器：固定 60Hz 步长
    self.frame_clock.accumulate(dt);
    while self.frame_clock.consume_logic_step() {
        self.update_logic(LOGIC_DT);
    }

    // 4. 远端玩家插值（用本地时间，非 logic step）
    self.interp.advance(dt);

    // 5. 网格化任务（frame budget）
    self.mesh_jobs.run_until_budget(MESH_BUDGET_MS, &mut self.renderer);

    // 6. UI 重建
    let egui_output = self.build_ui();

    // 7. 渲染
    self.renderer.render_frame(&self.frame_data(), egui_output);

    // 8. 持久化触发（每 30 秒）
    self.maybe_flush_persistence();

    // 通知浏览器我们要再来一帧
    self.canvas.request_animation_frame();   // winit 已自动管理
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

### 6.3 `physics.rs`（详见 [`features/physics.md`](../features/physics.md)）
本地玩家 AABB 物理预测（重力、跳跃、分轴碰撞）。仅作"乐观更新"，Host 仲裁后通过 prediction 模块协调。

### 6.4 `raycast.rs`（详见 [`features/physics.md`](../features/physics.md)）
DDA 算法，最大射程 6 格。返回命中的方块位置 + 命中面（用于放方块时计算邻居位置）。

### 6.5 `prediction.rs`（详见 [`networking/prediction.md`](../networking/prediction.md)）
- 玩家位置：本地立即更新 + 收到 PlayerTick 后做软协调（误差小则插补，超过阈值则瞬移）
- 方块挖放：先发请求 + 本地半透明预览 → 收到 ActionAck 决定 commit 或 rollback

### 6.6 `interp.rs`

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
    pub fn advance(&mut self, dt: f32);    // 推进所有 buffer 的插值
    pub fn current(&self, entity: EntityId) -> Option<(Vec3, f32, f32)>;
}
```

### 6.7 `mesh_jobs.rs`

```rust
pub struct MeshJobQueue {
    pending: PriorityQueue<ChunkPos, MeshPriority>,
    in_flight: HashSet<ChunkPos>,
}

impl MeshJobQueue {
    pub fn enqueue(&mut self, pos: ChunkPos, priority: MeshPriority);
    pub fn run_until_budget(&mut self, budget_ms: f32, renderer: &mut Renderer);
}
```

每次 budget：
1. 取最高优先级的 ChunkPos（玩家附近优先）
2. 从 `world_view` 取 chunk + 6 邻居引用
3. 调 `chunk_mesh::generate_with_neighbors(...)` 得到 CPU 数据
4. `renderer.upload_chunk_mesh(pos, mesh)` 上传 GPU
5. 累计耗时超过 budget 退出循环（剩余下一帧再处理）

详见 [`features/meshing.md`](../features/meshing.md)。

### 6.8 `storage.rs`

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
