# Phase 2 · 体素单人 · 完成报告

> 完成日期：2026-05-12
> 关联：[`docs/roadmap.md`](docs/roadmap.md) Phase 2

---

## 实际完成项

- ✅ **World 地形与生命周期** [crates/server/src/world.rs](crates/server/src/world.rs)
  - `World` 持有 `TerrainGenerator` + `chunks` + `tick_count`
  - `ensure_chunk_generated(pos)`：未生成则调地形生成器；已存在跳过（幂等）
  - `get_block_world(wx, wy, wz)`：世界坐标查询，chunk 未加载或 y 越界一律返回 AIR
  - `unload_chunk(pos)`：从 chunks 表移除
  - `set_block(pos, block)`：Phase 2 仅供测试与未来挖放使用
- ✅ **Perlin 地形生成器** [crates/server/src/terrain.rs](crates/server/src/terrain.rs)
  - 单层 Perlin 高度图（频率 0.01），分层填充：基岩 / STONE / DIRT / GRASS / AIR
  - 高度范围 ≈ 0..102（CHUNK_Y * 0.4）
- ✅ **跨区块面剔除** [crates/render/src/chunk_mesh.rs](crates/render/src/chunk_mesh.rs)
  - `generate_with_neighbors(chunk, pos, get_block_world)`：所有面查询走回调
  - 单元测试覆盖：邻居为 STONE 时 PosX 面剔除、空回调等价于朴素版、y 边界处理
- ✅ **Renderer chunk 资源生命周期** [crates/render/src/lib.rs](crates/render/src/lib.rs)
  - `drop_chunk_mesh(pos)` / `has_chunk_mesh(pos)` / `loaded_chunk_count()`
  - 多 chunk 渲染：每个 `ChunkMeshGpu` 自带 `globals_buffer + bind_group`（规避 `queue.write_buffer` 合并写入问题）
- ✅ **NetEndpoint::Local mpsc 双向通道** [crates/net/src/lib.rs](crates/net/src/lib.rs)
  - `new_local_pair() -> (NetEndpoint, ServerInbox)`：基于 `futures_channel::mpsc::unbounded`
  - `send_client_message` / `try_recv_server_message`（client 侧）
  - `ServerInbox::try_recv_client_message` / `send_server_message`（server 侧）
  - 单元测试 ping-pong 双向消息
- ✅ **MeshJobQueue** [crates/client/src/mesh_jobs.rs](crates/client/src/mesh_jobs.rs)
  - 4 档优先级（Critical / High / Medium / Low）× 4 个 `VecDeque` + `pending: HashSet` 防重
  - `run_until_budget(budget_ms, &Server, &mut Renderer, &now_ms)`：注入 `now_ms` 便于测试
  - 单元测试：优先级 pop 顺序、enqueue 去重、cancel、is_empty
- ✅ **ChunkLoader** [crates/client/src/chunk_loader.rs](crates/client/src/chunk_loader.rs)
  - `render_distance` 默认 6，`unload_buffer` 常数 3
  - 同 chunk 内移动不触发；跨边界时按 desired/loaded 集合 diff
  - 新 chunk 的水平邻居（已有 GPU mesh 的）以 `MeshPriority::Low` 重新入队 → 跨区块剔除生效
  - 工具函数 `chunk_pos_of` / `chebyshev_distance` / `priority_for_distance` 单元测试
- ✅ **Server Hello → Welcome** [crates/server/src/lib.rs](crates/server/src/lib.rs)
  - Phase 2 固定 `entity_id = 1`；`world_seed` + `server_tick` 一并回复
  - Break / Place 仍走 Phase 1 无校验 set_block 路径
- ✅ **App + Game 状态机** [crates/client/src/app.rs](crates/client/src/app.rs)
  - `AppState::{Loading, Lobby, Connecting, InGame, EscMenu, ChatOpen, Disconnected}`（Phase 2 仅用 Lobby + InGame）
  - `Game::new_local(seed, settings)`：构造 Server + 配对 NetEndpoint + 初始相机
  - `GameSettings`：render_distance / mouse_sensitivity / fly_speed / mesh_budget_ms
  - `FrameClock`：60Hz 累加器，上限 0.25s 防后台 Tab 回前台风暴
- ✅ **Lobby UI** [crates/client/src/ui/lobby.rs](crates/client/src/ui/lobby.rs)
  - "Single Player" 主按钮 + 可折叠 "Advanced / Seed" 输入框
  - `LobbyAction::StartSinglePlayer { seed: Option<u64> }`
  - 空字符串 → `getrandom` 抓 8 字节生成随机 u64
- ✅ **主循环 Lobby/InGame 分流** [crates/client/src/lib.rs](crates/client/src/lib.rs)
  - `render_lobby_frame`：暗蓝清屏 + egui Pass
  - `render_game_frame`：8 步（drain mpsc 双向 → 输入相机 → 逻辑帧 → ChunkLoader → mesh_jobs budget → HUD → world Pass → egui Pass）
  - 一次性 `request_pointer_lock_next` 在 "Single Player" 点击后的下一 RAF 兑现
- ✅ **浏览器鼠标事件喂给 egui**（[commit edae0e6](https://example/edae0e6) 修复）
  - 早期版本只在 InGame 才路由鼠标事件 → 大厅按钮无法点击
  - 改为：mousemove / mousedown / mouseup 在所有 state 都推入 `egui_events: Rc<RefCell<Vec<egui::Event>>>` 累加器，每帧 drain 入 `RawInput.events`
  - InGame 指针锁时 mousemove 走 `InputState`（相机增量），不喂 egui

---

## 关键文件改动

| 文件 | 改动 |
|---|---|
| [crates/server/src/world.rs](crates/server/src/world.rs) | 新增 `ensure_chunk_generated` / `get_block_world` / `unload_chunk` + 内嵌 5 个单元测试；World 持有 TerrainGenerator |
| [crates/server/src/lib.rs](crates/server/src/lib.rs) | `handle_message` 新增 Hello → Welcome 分支 + 内嵌测试 |
| [crates/render/src/chunk_mesh.rs](crates/render/src/chunk_mesh.rs) | 新增 `generate_with_neighbors` + 3 个跨区块剔除单元测试 |
| [crates/render/src/lib.rs](crates/render/src/lib.rs) | `Renderer::drop_chunk_mesh` / `has_chunk_mesh` / `loaded_chunk_count` |
| [crates/net/src/lib.rs](crates/net/src/lib.rs) | `NetEndpoint::Local` 改持 mpsc + `new_local_pair` + `ServerInbox` + 2 个测试 |
| [crates/client/src/mesh_jobs.rs](crates/client/src/mesh_jobs.rs) | 新建：MeshPriority + MeshJobQueue + budget runner |
| [crates/client/src/chunk_loader.rs](crates/client/src/chunk_loader.rs) | 新建：ChunkLoader + 工具函数 |
| [crates/client/src/app.rs](crates/client/src/app.rs) | AppState / GameSettings / FrameClock / Game::new_local |
| [crates/client/src/ui/lobby.rs](crates/client/src/ui/lobby.rs) | LobbyState + draw_lobby + parse_seed |
| [crates/client/src/lib.rs](crates/client/src/lib.rs) | App 替代 Phase 1 的 Runtime；事件路由 + egui_events 累加器；Lobby/InGame 分流主循环 |

---

## 验证

| 项 | 标准 | 实测 |
|---|---|---|
| `cargo fmt --all -- --check` | 无 diff | ✅ |
| `cargo clippy --workspace --target wasm32-unknown-unknown -- -D warnings` | 无错误 | ✅ |
| `cargo test --workspace --lib` | 全通过 | ✅（含 server world/handle_message、render chunk_mesh、net mpsc、client mesh_jobs/chunk_loader/lobby/app） |
| 大厅 → 单机模式进入 | < 1s | 待人工验证（修复 commit edae0e6 后按钮可点） |
| 飞行 30s 60fps | FPS ≥ 55 | 待人工验证 |
| 跨区块边界无漏面 | 视觉确认 | 待人工验证 |
| 走远再回头地形一致 | 同 seed 重生成等价 | 待人工验证 |

---

## 关键设计决策

1. **Local 模式共享 `Rc<RefCell<Server>>`，mesh 回调直接读 `server.world`**
   拒绝复制 chunk 数据走 ChunkLoaded 事件：Phase 5 引入 Remote 模式时再单独走 `WorldView` + ChunkSnapshot 路径。
2. **mpsc 双向通道**（即使本地）
   拒绝同步直接调用 `Server::handle_message`：Phase 4/5 强制走消息驱动，前期一致更划算。Phase 2 客户端仅发 Hello，但接口已落地。
3. **ChunkLoader 卸载半径 = render_distance + 3**
   常数缓冲（不是原设计的 1.5×）—— 简单够用；玩家附近往返时不会反复重生成。
4. **同 chunk 内移动不触发 update**
   `ChunkLoader.last_center` 缓存，跨 chunk 边界才计算 diff。
5. **新 chunk 的已生成 mesh 邻居以 `Low` 优先级重新入队**
   跨区块剔除生效条件：邻居必须存在。先生成所有新 chunk → 再入队，保证 mesh 出队时 4 个水平邻居已存在；已生成 mesh 的边界邻居须重做（用 Low 优先级避免抢占）。
6. **Phase 2 不做视锥剔除 / 贪婪网格化**
   渲染距离 6 时 chunk 数 ≈ 169，朴素逐面 + 跨区块剔除下顶点量可控（暴露率 10-30%）。Phase 7 再上贪婪 + 视锥。
7. **不引入 winit 事件循环**
   wasm 直接 `add_event_listener_with_callback` 更简单；仅借用 `winit::keyboard::KeyCode` 作为统一键码枚举（沿用 Phase 1）。
8. **`now_ms` 注入 `MeshJobQueue::run_until_budget`**
   生产环境传 `web_sys::Performance::now()` 薄包装；单元测试可注入受控时钟。

---

## 已知问题 / 后续

1. **未跑浏览器人工验收** — 渲染层涉及 WebGPU，必须在浏览器跑 `trunk serve`。 commit edae0e6 修复了大厅按钮失效问题；视觉细节（地形分层、跨区块边界、远距离卸载）尚需人工确认。
2. **`Server::handle_message` 的 `Break/Place` 无校验** — 沿用 Phase 1 行为：直改 `world.set_block`。Phase 3 起加入 `physics::validate_break/place` 仲裁 + `dirty_chunks` 标记。
3. **挖放更新不触发重网格化** — Phase 2 没有挖放 UX，所以未实装；Phase 3 完整闭环时一并补：Server 处理 → BlockUpdate 广播 → client 收到 → 受影响 chunk（含 6 个邻居方向中边界情况）入队。
4. **`unload_chunk` 不 flush dirty** — Phase 2 dirty 集合不存在；Phase 5 引入持久化时这里要先 flush 再 remove。
5. **`CentralPanel::show` deprecated 警告** — egui 0.34 起对根面板使用 `CentralPanel::show` 给出 deprecation；目前无更合适替代，加 `#[allow(deprecated)]` 抑制。

---

## 下一步：Phase 3 · 物理与交互

入口文档：[docs/features/physics.md](docs/features/physics.md)

要点（参考 [docs/roadmap.md](docs/roadmap.md) Phase 3 任务清单）：
- `client::physics` AABB + 重力 + 跳跃 + 分轴碰撞
- Walk / Fly 模式切换（双击空格）
- `client::raycast` DDA 算法 + HUD 选中方块线框
- 鼠标左键挖 / 右键放 → BlockUpdate 闭环
- `server::physics::validate_break/place` 仲裁
- 1-9 hotbar
- ActionAck rollback 路径
