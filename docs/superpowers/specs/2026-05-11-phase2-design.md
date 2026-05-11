# Phase 2 · 体素单人 · 设计文档

> 完成目标：单机模式可玩 —— 大厅入口 → 进入游戏 → 看到连绵地形 → 飞行时持续流式加载。
> 关联：[`docs/roadmap.md`](../../roadmap.md) Phase 2 任务清单
> 起草日期：2026-05-11

---

## 一、范围

### 范围内
- 大厅 UI（仅"单机模式"按钮）→ InGame 状态切换
- 地形流式生成（Perlin 高度图 + 分层填充，复用已有 `TerrainGenerator`）
- Chunk 动态加载 / 卸载（玩家移动时，按渲染距离半径滚动）
- 跨区块面剔除（`generate_with_neighbors` + 世界坐标回调）
- 网格化任务调度（优先级队列 + 分帧 budget，4ms/帧）
- `NetEndpoint::Local` 双向 mpsc 通道（为 Phase 4 留接口）
- 文档体系全面更新（modules/server.md, modules/client.md, features/meshing.md, roadmap.md 同步 Phase 2 实现）

### 范围外（明确推后）
- 玩家物理 / 碰撞 / 跳跃 / 挖放（Phase 3）
- WebRTC / 信令 / 远端玩家（Phase 4-5）
- 玩家位置仲裁 / 预测协调（Phase 5）
- IndexedDB 持久化（Phase 5+）
- 贪婪网格化（Phase 7）
- 视锥剔除（Phase 7；本期渲染距离 6 时不做仍可 60fps）
- ChunkSnapshot 分片传输协议（Phase 5 才需，Local 模式下不需要）

---

## 二、关键决策

| 维度 | 决策 | 替代方案与拒绝理由 |
|---|---|---|
| Local 模式通信 | mpsc 双向通道（`futures::channel::mpsc`） | 拒绝同步直接调用：Phase 4/5 会强制走消息驱动，前期一致更划算 |
| Server↔Client Chunk 数据路径 | **共享 `Rc<RefCell<Server>>`，mesh 回调直接读 `server.world`** | 拒绝 ChunkLoaded 内部事件：Local 模式同进程，复制 Chunk 数据浪费内存；分片只在 P2P 字节流才有意义。Phase 5 引入 WorldView 副本时再单独走 ChunkSnapshot 路径 |
| Chunk 卸载策略 | 超出"渲染距离 × 1.5 缓冲"全部卸载（内存 + GPU） | 拒绝 LRU：Phase 2 无方块修改，重生成 = 原状态；保留 dirty 标记接口给 Phase 3 |
| 大厅 UI | 仅"单机模式"按钮 + 可选种子输入框 | 拒绝预占位 Host/Join 按钮：Phase 4 再画 |
| MeshJob 队列 | `MeshPriority::{Critical,High,Medium,Low}` 枚举 + 4 个 deque | 拒绝单 deque：接口一次到位避免 Phase 7 改 API |
| 渲染距离默认 | 6 chunks（设置范围 2..=10 留给后续阶段 UI） | 与 roadmap 验收标准一致 |
| 视锥剔除 | Phase 2 暂不做 | 渲染距离 6 时 chunk 数 ≈ 169，naïve 网格化下顶点量可控；Phase 7 与贪婪一起做 |

---

## 三、架构

### 3.1 App 容器演进

Phase 1 是扁平 `Runtime`（直接 own renderer + camera + demo chunk）。Phase 2 重构为 AppState 状态机 + 可选 `Game`：

```
App                                   // 替代 Phase 1 的 Runtime
├── canvas: HtmlCanvasElement
├── renderer: Renderer
├── egui_ctx / egui_renderer
├── input: Rc<RefCell<InputState>>
├── state: AppState                   // Lobby | InGame
└── game: Option<Game>                // InGame 时存在

Game
├── server: Rc<RefCell<Server>>       // Local-Only 模式
├── net: NetEndpoint                  // ::Local { tx_client, rx_server }
├── camera: Camera
├── mesh_jobs: MeshJobQueue
├── chunk_loader: ChunkLoader
├── settings: GameSettings
└── stats: FrameStats                 // 给 HUD 显示
```

> Phase 2 的 AppState 只用 `Lobby` 与 `InGame` 两态。`Connecting / EscMenu / ChatOpen / Disconnected` 状态保留枚举，但本期不进入。

### 3.2 Local 模式通道

```
Client                          Server
  │                               │
  │  ClientMessage  (mpsc tx)     │
  │ ────────────────────────────► │
  │                               │  Server::handle_message
  │                               │  Server::tick (logic accumulator)
  │ ◄──────────────────────────── │
  │  ServerMessage  (mpsc tx)     │
  │                               │
```

- `NetEndpoint::new_local_pair() -> (NetEndpoint, ServerInbox)`：返回 `client side` 与 `server side` 两端。
- 主循环每帧：先 `drain ServerInbox → 调 server.handle_message → 把产出的 ServerMessage 推回 client side`，再 `client try_recv ServerMessage → 应用到 Game 状态`。
- Phase 2 客户端唯一发送的 `ClientMessage` 是 `Hello`（启动时）。其它消息保留接口。

> **不变量**：所有 `Server::handle_message` 调用必须通过这条通道，不允许 client 直接拿 `Rc<RefCell<Server>>` 调用 `handle_message`。但**读取 `server.world`** 是允许的（用于网格化回调），因为只读且 Local 模式同进程合理。

### 3.3 主循环

```rust
fn render_frame(app: &mut App) {
    let dt = compute_dt();

    match &mut app.state {
        AppState::Lobby => {
            render_lobby_frame(app, dt);  // 仅 egui
        }
        AppState::InGame => {
            let game = app.game.as_mut().unwrap();

            // 1. drain Local 通道：Client→Server 入站消息
            game.net.drain_inbox_to(&mut game.server.borrow_mut());

            // 2. drain Local 通道：Server→Client 出站消息 → 应用
            while let Some(msg) = game.net.try_recv_server_message() {
                game.apply_server_message(msg);
            }

            // 3. 输入 → 相机（Fly 模式，无物理；Phase 3 改）
            game.update_input_and_camera(dt);

            // 4. 逻辑帧累加器（60Hz）
            game.frame_clock.accumulate(dt);
            while game.frame_clock.consume_logic_step() {
                game.server.borrow_mut().tick();
            }

            // 5. ChunkLoader 滚动加载
            game.chunk_loader.update(
                game.camera.position,
                &mut game.server.borrow_mut(),
                &mut game.mesh_jobs,
                &mut app.renderer,
            );

            // 6. mesh_jobs 跑 budget
            game.mesh_jobs.run_until_budget(
                MESH_BUDGET_MS,
                &game.server.borrow(),
                &mut app.renderer,
            );

            // 7. egui HUD（沿用 Phase 1）
            // 8. wgpu 渲染 + present（沿用 Phase 1）
        }
    }
}
```

### 3.4 ChunkLoader 行为

```rust
pub struct ChunkLoader {
    pub render_distance: u32,
    pub unload_buffer: u32,          // 1.5× → 实际半径 = render_distance + 3
    pub loaded: HashSet<ChunkPos>,   // 已生成 + mesh 已上传 / 入队的 chunk
    last_center: Option<ChunkPos>,
}

impl ChunkLoader {
    pub fn update(
        &mut self,
        camera_pos: Vec3,
        server: &mut Server,
        mesh_jobs: &mut MeshJobQueue,
        renderer: &mut Renderer,
    ) {
        let center = chunk_pos_of(camera_pos);
        if Some(center) == self.last_center { return; }  // 同 chunk 内移动不触发
        self.last_center = Some(center);

        let load_radius = self.render_distance as i32;
        let unload_radius = load_radius + self.unload_buffer as i32;

        // 期望集合
        let desired: HashSet<ChunkPos> = (-load_radius..=load_radius)
            .flat_map(|dx| (-load_radius..=load_radius).map(move |dz| ChunkPos::new(center.x+dx, center.z+dz)))
            .collect();

        // 新增：生成 + 入队
        for pos in desired.difference(&self.loaded).copied().collect::<Vec<_>>() {
            server.world.ensure_chunk_generated(pos);
            let priority = priority_for_distance(pos, center);
            mesh_jobs.enqueue(pos, priority);
            self.loaded.insert(pos);
        }

        // 卸载：超出 unload_radius 的释放
        let to_unload: Vec<_> = self.loaded.iter()
            .filter(|p| chebyshev_distance(**p, center) > unload_radius)
            .copied().collect();
        for pos in to_unload {
            server.world.unload_chunk(pos);
            mesh_jobs.cancel(pos);
            renderer.drop_chunk_mesh(pos);
            self.loaded.remove(&pos);
        }
    }
}
```

### 3.5 邻居加载顺序

跨区块剔除要求**网格化时其邻居 chunk 已生成**（否则边界面会被错误地视作空气暴露）。两个保证：

1. `ChunkLoader.update` 中**先批量调 `ensure_chunk_generated`**（对所有新增 chunk），再统一入队 mesh_jobs。这样 mesh_jobs 出队执行时，4 个水平邻居都已存在于 `server.world.chunks`（前提是邻居也在期望集合内）。
2. mesh_jobs 出队执行时，回调 `|wx,wy,wz| server.world.get_block_world(wx,wy,wz)` 读到的就是已存在数据；超出已加载范围的位置（如 unload_radius 外）返回 AIR。

**邻居首次加载 → 已生成 mesh 需重网格化**（仅边界 chunk 受影响）：

ChunkLoader.update 在批量生成本帧的新 chunk 后，遍历**这些新 chunk**，对每个新 chunk 的 4 个水平邻居：若邻居在 `loaded` 集合中且已经有 GPU mesh（`renderer.has_chunk_mesh`），则把该邻居以 `MeshPriority::Low` 重新入队。

> 一次普通的"玩家向前飞行"产生的新 chunk 数量很小（一条 1×(2*RD+1) 的"列"），重网格化的邻居数也小，不会形成风暴。

### 3.6 MeshJobQueue

```rust
pub enum MeshPriority {
    Critical,   // 玩家正站立的 chunk
    High,       // 半径 1 范围
    Medium,     // 渲染距离内其它
    Low,        // 重新网格化（邻居加载触发）
}

pub struct MeshJobQueue {
    queues: [VecDeque<ChunkPos>; 4],     // 按 MeshPriority 索引
    pending: HashSet<ChunkPos>,           // 防重 / cancel 用
}

impl MeshJobQueue {
    pub fn enqueue(&mut self, pos: ChunkPos, priority: MeshPriority);
    pub fn cancel(&mut self, pos: ChunkPos);
    pub fn run_until_budget(&mut self, budget_ms: f32, server: &Server, renderer: &mut Renderer);
}
```

`run_until_budget` 每次取最高优先级队列的 head，调 `chunk_mesh::generate_with_neighbors`，回调闭包通过 `server.world.get_block_world` 读取相邻区块，结果上传到 renderer。`performance.now()` 监控耗时超过 budget 时退出。

---

## 四、模块改动清单

### 4.1 `core` crate
- 无改动。

### 4.2 `server` crate

**`world.rs`**：
- 删除 / 重命名当前 `ensure_chunk`（返回空 chunk 易产生混淆）
- 新增 `World::ensure_chunk_generated(pos: ChunkPos)`：若已存在则跳过；否则调 `TerrainGenerator::generate_chunk(pos)` 插入。
- 新增 `World::get_block_world(wx: i32, wy: i32, wz: i32) -> BlockID`：世界坐标查询；chunk 未加载或 y 超界一律返回 AIR。
- 新增 `World::unload_chunk(pos: ChunkPos)`：从 `chunks` 移除。**不删 `dirty_chunks`**（Phase 5 持久化要求）；Phase 2 dirty 集合不使用。
- `World::set_block`：保留 Phase 1 行为；Phase 3 引入挖放时再扩展 dirty 标记。

**`lib.rs`**：
- `Server::new(seed)`：`World` 内持有 `TerrainGenerator`（terrain 是 world 的属性）。`ensure_chunk_generated` 通过 `self.terrain.generate_chunk(pos)` 实现。
- `Server::handle_message`：保留现状（Phase 3 完善）；Phase 2 仅在收到 `Hello` 时回 `Welcome { entity_id, server_tick, world_seed }`，`entity_id` 硬编码为 1。新增 Hello 分支。

> **本期不引入** `World::dirty_chunks` 字段、`PlayerEntity` 表、`Server::add_player/remove_player`、`Server::take_dirty_chunks`：这些都是 Phase 3 / Phase 5 引入点。 `docs/modules/server.md` 现有描述需在 §5 文档更新中标注阶段。

### 4.3 `render` crate

**`chunk_mesh.rs`**：
- 新增 `generate_with_neighbors(chunk: &Chunk, pos: ChunkPos, get_block_world: &dyn Fn(i32,i32,i32) -> BlockID) -> ChunkMeshCpu`
  - 与 `generate_opaque_mesh` 行为相同，但**所有**面可见性查询走 `get_block_world(wx, wy, wz)`（同 chunk 内也走，统一接口）
  - chunk 边界外 → 回调返回真实邻居方块或 AIR（取决于邻居是否已加载）
- 保留原 `generate_opaque_mesh` 作单元测试 fallback。
- 单元测试：构造两个相邻 chunk，A 的 lx=15 与 B 的 lx=0 都是 STONE → A 的 PosX 面应被剔除。

**`lib.rs`** (Renderer)：
- 新增 `Renderer::drop_chunk_mesh(pos: ChunkPos)`：从 `chunk_meshes` 移除并释放 GPU buffer。
- 新增 `Renderer::has_chunk_mesh(pos: ChunkPos) -> bool`。
- `upload_chunk_mesh`：保留现有行为，**覆盖**同 pos 旧 mesh。

### 4.4 `net` crate

**`lib.rs`**：
- `NetEndpoint::Local` 改为带字段的变体（持有 mpsc sender/receiver）。
- 新增 `pub struct ServerInbox { tx_server: ..., rx_client: ... }`
- 新增 `NetEndpoint::new_local_pair() -> (NetEndpoint, ServerInbox)`：基于 `futures::channel::mpsc::unbounded()`。
- 新增 `NetEndpoint::send_client_message(msg)`：Local 分支 push 到 tx_client。
- 新增 `NetEndpoint::try_recv_server_message() -> Option<ServerMessage>`：非阻塞拉取。
- 新增 `ServerInbox::try_recv_client_message() -> Option<ClientMessage>`。
- 新增 `ServerInbox::send_server_message(msg)`。

> Phase 4 引入 `NetEndpoint::Host { ... }` 与 `::Remote { ... }` 时 mpsc 仍是骨干（与 WebRTC 适配器组合），接口不变。

### 4.5 `client` crate

**新文件 `mesh_jobs.rs`**：
- `MeshPriority` 枚举 + `MeshJobQueue`（见 §3.6）
- 单元测试：插入混合优先级 → pop 顺序按 Critical < High < Medium < Low

**新文件 `chunk_loader.rs`**：
- `ChunkLoader` struct + `update` 方法（见 §3.4）
- `chunk_pos_of(world_pos: Vec3) -> ChunkPos`
- `chebyshev_distance(a: ChunkPos, b: ChunkPos) -> i32`
- `priority_for_distance(pos: ChunkPos, center: ChunkPos) -> MeshPriority`

**`app.rs`**：
- 把 `AppState` 中的 `Loading` 改为入口默认值，但 Phase 2 实质上一进来就 `Lobby`。
- 新增 `pub struct Game { ... }`（见 §3.1）

**`ui/lobby.rs`**：
- 实装 `draw_lobby(ctx, &mut LobbyAction) -> ()`，其中 `LobbyAction` 是状态变更意图（如 `StartSinglePlayer { seed }`）。
- 内容：
  - 中央卡片：标题"VoxWeb"
  - "单机模式" 按钮 → 触发 `LobbyAction::StartSinglePlayer`
  - 折叠区："高级 / 种子"：u64 文本输入框 + "随机"按钮（默认空 = 随机生成）
  - 底部小字：版本号 + Phase 标记
- ASCII 字符串（沿用 Phase 1 字体限制）

**`lib.rs`**：
- 把 `Runtime` 重命名为 `App`，加 `state: AppState` 与 `game: Option<Game>`。
- 启动流程：Phase 1 直接构 demo chunk → Phase 2 启动后 `state = Lobby`。
- 拆 `render_frame` 为 `render_lobby_frame` / `render_game_frame`。
- 键盘事件路由：Lobby 时不消费 WASD（egui 自己处理文本输入）；InGame 时沿用 Phase 1。
- 指针锁：只在进入 InGame 后用户首次点击时请求。

---

## 五、文档体系更新清单

CLAUDE.md 要求"文档先行 + 代码改动同步更新文档"。Phase 2 实施中**必须**同步以下文档：

| 文档 | 更新内容 |
|---|---|
| [`docs/roadmap.md`](../../roadmap.md) | Phase 2 标题加 ✅；实际完成项与 stretch 区分 |
| [`docs/modules/server.md`](../../modules/server.md) | `World` 结构对齐实际字段（terrain 持有位置、`dirty_chunks` 是否本期建表）；`ensure_chunk_generated` 接口落地；删除/保留的 `add_player` 等 stub 项标注阶段 |
| [`docs/modules/client.md`](../../modules/client.md) | App 结构图对齐 Phase 2 实际形态；`Game` 子结构补充；主循环代码块同步；`mesh_jobs.rs` / `chunk_loader.rs` 加入目录树 |
| [`docs/modules/net.md`](../../modules/net.md) | `NetEndpoint::Local` 通道接口落地（mpsc 双向） |
| [`docs/features/meshing.md`](../../features/meshing.md) | `generate_with_neighbors` 接口签名同步；Phase 2 朴素逐面 + 跨区块剔除 vs Phase 7 贪婪的关系澄清 |
| [`docs/architecture.md`](../../architecture.md) | 如有"Phase 2 已完成"相关角色 / 帧调度细节差异（如 mesh budget 默认值），同步过来 |
| 仓库根 `PHASE_2_DONE.md` | 新增：完成项 + 实测验证表 + 已知问题 |
| `README.md` 决策表 | 新增"当前 Phase" 行（如已有则更新） |

> 这些更新作为 Phase 2 实施的最后一道任务，在所有代码 PR 完成、`cargo check / clippy / test` 全绿、浏览器人工验证通过后一次性提交。

---

## 六、单元测试

### 6.1 server (`cargo test -p voxweb-server`)
- `terrain::generate_chunk(seed=42, pos=(0,0))` 顶点高度基线 hash 稳定
- `World::ensure_chunk_generated` 同 pos 二次调用不重生成（hash 不变 + 计数不增）
- `World::get_block_world` 表驱动：chunk 已加载 / 未加载 / y 越界

### 6.2 render (`cargo test -p voxweb-render --lib`)
- `generate_with_neighbors`：
  - 空回调（视区块外为 AIR） → 等价 `generate_opaque_mesh`
  - 单方块在 lx=15，邻居 chunk 同 ly 同 lz 是 STONE → 该 PosX 面不应发射（与朴素版相比顶点 -6）

### 6.3 client（rlib target）
- `MeshJobQueue` 优先级出队顺序正确
- `MeshJobQueue::cancel(pos)` 后 `enqueue(pos, ...)` 仍工作
- `ChunkLoader.update` 触发的 diff：`(camera 移动跨 chunk 边界)` 产出正确的加载 / 卸载集合

### 6.4 浏览器人工验收（执行人确认）
| 项 | 标准 |
|---|---|
| 大厅入口 | 看到 VoxWeb 标题 + "单机模式" 按钮，无报错 |
| 进入游戏 | 点击按钮 < 1s 切到 InGame，看到地形 |
| 地形外观 | 草/泥/石分层正确；高度连贯无断崖伪影 |
| 飞行加载 | 持续飞行 30s 无卡顿；HUD FPS ≥ 55 |
| 跨区块剔除 | 飞低于 y=0 抬头看，chunk 底面无可见漏面或洞 |
| 回头 | 走出 6 chunks 再回头，地形完全等同（同 seed） |
| 渲染距离 6 | 60 fps（中等设备） |

---

## 七、风险与缓解

| 风险 | 缓解 |
|---|---|
| `Rc<RefCell<Server>>` 借用冲突（mesh_jobs 借 immutable 读，但 ChunkLoader 同帧借 mutable 写） | 严格按主循环顺序：先 ChunkLoader.update（mut borrow），结束后再 mesh_jobs.run_until_budget（immut borrow）。两段不重叠。 |
| 渲染距离 6 时 chunk 数 169，naïve 网格化顶点量爆炸 | 朴素逐面只对暴露面发射；典型地形暴露率约 10-30%。预估 169 chunk × 30K 顶点 = 5M 顶点，u32 压缩后 20MB，可接受。如帧时间不达标，临时降渲染距离到 4。 |
| 邻居 chunk 首次加载触发"已有 chunk 重网格化" 风暴 | 已生成 mesh 的邻居以 `Low` 优先级重新入队，不抢占当前帧的 critical / high；玩家附近优先。 |
| Phase 1 起 wasm-opt 未装导致 release 偏大 | 沿用 Phase 1 处理；不在 Phase 2 范围。 |

---

## 八、不在范围（重申，避免误判）

- 玩家碰撞 / 重力（Phase 3）
- 玩家挖放（Phase 3）
- WebRTC P2P（Phase 4）
- 协议层：`Hello/Welcome` 在 Phase 2 只占位实现（client 启动发 Hello，server 回 Welcome 携 seed/entity_id；其它消息流转留 Phase 5）
- 视锥剔除（Phase 7）
- 贪婪网格化（Phase 7）
- AO（Phase 7）

---

## 九、实施顺序提示

> 详细实施步骤由后续 writing-plans 产出。此处仅提示可独立验证的里程碑顺序：

1. `server::world::ensure_chunk_generated` + `get_block_world` + `unload_chunk`
2. `render::chunk_mesh::generate_with_neighbors` + 单元测试
3. `Renderer::drop_chunk_mesh` / `has_chunk_mesh`
4. `NetEndpoint::Local` mpsc 通道
5. `client::mesh_jobs.rs` + 单元测试
6. `client::chunk_loader.rs`
7. `client::ui::lobby` 实装
8. `client::lib.rs` App 重构 + Lobby/InGame 切换
9. 浏览器联调
10. 文档体系更新 + `PHASE_2_DONE.md`
