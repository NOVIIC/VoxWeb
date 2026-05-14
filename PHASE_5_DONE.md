# Phase 5 · 主机权威同步 — 完成报告

> 完成日期：2026-05-14
> 设计文档：[`docs/modules/server.md`](docs/modules/server.md) · [`docs/networking/protocol.md`](docs/networking/protocol.md) · [`docs/networking/prediction.md`](docs/networking/prediction.md)

---

## 目标回顾

两个浏览器 Tab 看到同一个世界。Host 是权威，Remote 通过 ChunkSnapshot 全量同步 + PlayerTick 60Hz 增量 + ActionAck/BlockUpdate 闭环维护一致性。

从 Phase 4 的"两个 Tab 能互发 Ping/Pong"升级到"两个 Tab 看到同一个世界，A 挖方块 B 看到消失"。

---

## 实装清单

### 1. 协议层（`voxweb-core`）

- [`crates/core/src/protocol.rs`](crates/core/src/protocol.rs)：
  - `PROTOCOL_VERSION: u32 = 1` — Hello 握手时的版本校验键
  - `CHUNK_SNAPSHOT_PAYLOAD_MAX: usize = 14 * 1024` — ChunkSnapshot 单片上限
  - `EntityId = u32` — 玩家实体全局唯一 ID 类型别名
  - `Recipient { All, Except(EntityId), One(EntityId) }` — Server outbox 路由标记
  - `OutboundMessage { recipient, message }` — 带路由标签的 ServerMessage 包装
  - `RoomEvent::RemoteLeft { peer_id }` — Host 端感知 Remote 离开的事件

### 2. Server 升级（`voxweb-server`）

- [`crates/server/src/lib.rs`](crates/server/src/lib.rs)：
  - `PlayerEntity { display_name, position, yaw, pitch, last_input_tick, joined_at_tick }` — 完整玩家实体
  - `Server.players: HashMap<u32, Vec3>` → `HashMap<EntityId, PlayerEntity>` — 升级位置表
  - `Server.outbox: VecDeque<OutboundMessage>` + `next_entity_id` — 出站消息队列 + ID 分配器
  - `add_player(display_name) -> EntityId` — 分配 eid、入表、enqueue Welcome/PeerJoined/ChunkSnapshot
  - `remove_player(eid)` — 移除表项、enqueue PeerLeft: All
  - `drain_outbox() -> Vec<OutboundMessage>` — 每帧取走出站消息
  - `broadcast_tick()` — 收集所有 PlayerSnapshot，enqueue PlayerTick: All
  - `send_initial_snapshot(eid, center, radius)` — 遍历 chunk × 切片为 ChunkSnapshot 分片
  - `handle_message(eid, msg)` 重构 — 不返回 Vec，全部 enqueue 到 outbox
  - `tick()` 加 broadcast_tick() 调用

- [`crates/server/src/world.rs`](crates/server/src/world.rs)：
  - `dirty_chunks: HashSet<ChunkPos>` — set_block 时自动标记
  - `drain_dirty() -> Vec<ChunkPos>` — Phase 8 持久化用

### 3. Net 层路由（`voxweb-net`）

- [`crates/net/src/lib.rs`](crates/net/src/lib.rs)：
  - `NetEndpoint::Host` 新字段：`peer_to_entity`, `host_self_entity_id`
  - `host_register_peer / host_unregister_peer / host_set_self_entity / host_peer_to_entity_clone`
  - `host_route_outbox(outbox, local_inbox)` — 按 Recipient 路由到 peers DC + 自身 mpsc
  - `plan_route(msg, peer_to_entity, host_self) -> RoutingPlan` — 纯函数路由（独立可单测）
  - `poll` 签名改：`FnMut(u32, ClientMessage) -> Vec<ServerMessage>` → `FnMut(u32, ClientMessage)`
  - `poll_host` 内部：PeerLeft/Disconnected 推 `RoomEvent::RemoteLeft { peer_id }`；Message handler 交给 client 闭包
  - 6 个路由单测（All / Except / One to self / One to peer / unknown entity / no host self）

### 4. Client 端 host driver（`voxweb-client`）

- [`crates/client/src/app.rs`](crates/client/src/app.rs)：
  - `Game::new_local` / `new_host` 启动时调 `server.add_player(display_name)`；丢弃初始 outbox
  - `RemotePlayerState { display_name, last_seen_tick, color_rgb }` + HSV 颜色派生
  - `Game` 新字段：`remote_players`, `interp`, `chunk_assembler`, `input_history`, `server_clock_offset_ms`

- [`crates/client/src/lib.rs`](crates/client/src/lib.rs)：
  - `poll_net` 重写：Host 闭包内 Hello → add_player + 记 `(peer_id, eid)`；其他 → 查 live_map → handle_message
  - `flush_server_outbox()` — Local 折叠到 mpsc、Host 走 `host_route_outbox`、Remote 忽略
  - `apply_room_event` 新增 `RemoteLeft` 分支：`host_unregister_peer` → `server.remove_player`
  - `render_game_frame`：Local/Host 路径 drain outbox 替代旧 `handle_message` 返回值循环

### 5. ChunkAssembler（`voxweb-client`）

- [`crates/client/src/chunk_assembler.rs`](crates/client/src/chunk_assembler.rs)：新增
  - `ChunkAssembler { partials: HashMap<ChunkPos, PartialAssemble> }`
  - `ingest(pos, frag_index, frag_total, payload) -> Option<Vec<u8>>`
  - 防御性：frag_total 改变重置、越界 fragment 丢弃、重复 fragment 幂等
  - 4 个单测

### 6. apply_server_message 扩展

- `Welcome`：Remote 模式清空本地占位 chunks
- `ChunkSnapshot`：assembler ingest → decode → 写入 server.world.chunks → remesh
- `BlockUpdate`：Remote 先 `server.world.set_block` 再 remesh
- `PlayerTick`：reconcile_self（自己的 snapshot）+ interp.ingest_tick（他人的）
- `PeerJoined / PeerLeft`：维护 remote_players 表 + interp buffer
- `Chat`：Phase 5 最小实现（`log::info!`）

### 7. Prediction · InputHistory + reconcile_self

- [`crates/client/src/prediction.rs`](crates/client/src/prediction.rs)：
  - `InputRecord { tick, position }` + `InputHistory { records: VecDeque, cap }`
  - `push / drop_until`
  - `reconcile_self(server_position, server_tick, physics, history) -> ReconcileResult`
  - SOFT_THRESHOLD = 0.1m, HARD_THRESHOLD = 2.0m
  - 4 个单测

### 8. PlayerInterp 完整实装

- [`crates/client/src/interp.rs`](crates/client/src/interp.rs)：
  - `PlayerInterp { buffers, delay_ms=100, max_per_entity=20 }`
  - `ingest_tick / advance(render_server_time_ms) / remove / ids()`
  - shortest-arc yaw lerp + 边界 clamp（早于最早 → 最早；晚于最新 → 最新）
  - 5 个单测（lerp between two, yaw wraparound, unknown entity, eviction, late clamp）

### 9. Main loop wire-up

- `server.tick()` gate 到 `Local | Host` 模式
- `input_history.push()` 每逻辑步
- `render_world` → `render_players`（PlayerPass instance buffer 上传 + draw）→ `render_selection` 顺序

### 10. PlayerPass 渲染（`voxweb-render`）

- [`crates/render/src/passes/player.rs`](crates/render/src/passes/player.rs)：新增
  - `PlayerInstance { position, color }`（32 字节 std140）
  - `PlayerPass`：36 顶点单位 cube + 动态 instance buffer + globals uniform
  - WGSL shader：infer_normal + Lambert 方向光
  - 2 个单测（32 byte layout, 36 vertex count）
- [`crates/render/src/lib.rs`](crates/render/src/lib.rs)：`upload_player_instances / render_players`

### 11. 端到端验证（待手测）

```bash
cd signaling && npm run dev    # 本地信令
trunk serve                     # localhost:8080
```

手测 checklist：参见 [roadmap.md §Phase 5 验证](docs/roadmap.md)。

---

## 测试覆盖

| 维度 | 位置 | 测试数 |
|---|---|---|
| core protocol | `crates/core/src/protocol.rs` | 21（+2 Phase 5: roundtrip Recipient + RemoteLeft） |
| server | `crates/server/src/lib.rs` | 21（重构 + 新增 add_player/remove_player/drain_outbox/broadcast_tick/chat/ping/break/place/player_input） |
| server world | `crates/server/src/world.rs` | 7（+2 dirty） |
| net | `crates/net/src/lib.rs` | 12（+6 routing） |
| client chunk_assembler | `crates/client/src/chunk_assembler.rs` | 4 |
| client prediction | `crates/client/src/prediction.rs` | 7（+4 input_history） |
| client interp | `crates/client/src/interp.rs` | 5 |
| render player | `crates/render/src/passes/player.rs` | 2 |
| **合计** | | **139** |

---

## 设计取舍

| 决策 | 选择 | 理由 |
|---|---|---|
| Remote 世界数据宿主 | 复用 `Server.world` 作纯数据宿主 | 不引入 `WorldView`；Remote 模式 tick/handle_message 不被驱动，Server.struct 加文档注释 |
| OPFS 持久化 | 整体延后到 Phase 8 | Phase 5 工作量大（11 步）；OPFS 独立可验证 |
| 位置误差中等阈值处理 | Phase 5 忽略中间误差（> SOFTERROR、< HARDERROR） | Phase 7 加 soft-blend 动画器 |
| ChunkSnapshot 编码 | 直接 bincode `Vec<BlockID>`（~65KB/chunk） | Phase 8 加 palette+RLE 压缩（~2-5KB/chunk） |
| 玩家身体渲染 | 实心立方体 PlayerPass，无 yaw 朝向 | 极简方案；视觉区分依靠 entity_id → 颜色 |
| Hello 处置 | 从 handle_message 中抽出，由 net 层调用 add_player | entity_id 分配需要 &mut Server + 同时建立 peer_id↔entity_id 映射，时序耦合在 net 闭包中更干净 |
| 并发玩家目标 | 4 人（1 Host + 3 Remote） | 可覆盖 Host→3 fanout 路径 |
| Server outbox 放 core | `OutboundMessage` / `Recipient` in `voxweb_core::protocol` | net 已依赖 core；避免 net→server 反向依赖 |
| poll 签名 | `FnMut(u32, ClientMessage)`（不返回） | 闭包副作用进 server.outbox；client 端 drain 后统一路由 |

---

## 已知限制 / 留给 Phase 6-8

- Remote 端 `server.world.chunks` 在 Welcome 时清空占位 chunks，但 `mesh_jobs.run_until_budget` 可能同时持有 `&Server`——ChunkSnapshot 写入 chunks 的时序与 mesh_jobs 无锁竞争（RefCell borrow 动态检查兜底）
- PlayerTick 的 `server_clock_offset_ms` 没有 EMA 滤波，网络抖动可能导致轻微视觉振动（Phase 7 加）
- ChunkSnapshot 分片没有流量控制；SCTP+RefCell backpressure 自然限速（监控手测）
- OPFS 持久化未覆盖：关 Tab 再开房间世界重生成（Phase 8）
- LRU + pinned chunk 驱逐未覆盖（Phase 8）
- 玩家身体不随 yaw 旋转（Phase 8 可扩展为 capsule/arm）

---

## 文件改动概要

| 文件 | 性质 |
|---|---|
| `crates/core/src/protocol.rs` | 改写：PROTOCOL_VERSION / CHUNK_SNAPSHOT_PAYLOAD_MAX / EntityId / Recipient / OutboundMessage / RoomEvent::RemoteLeft |
| `crates/core/src/lib.rs` | re-export 新符号 |
| `crates/server/src/lib.rs` | 改写：PlayerEntity / outbox / add_player / remove_player / broadcast_tick / send_initial_snapshot / handle_message 重构 / tick 改动；全部已有测试重写 |
| `crates/server/src/world.rs` | dirty_chunks + drain_dirty + 2 新测试 |
| `crates/net/src/lib.rs` | 改写：peer_to_entity 映射 / host_* 方法 / host_route_outbox / plan_route / poll 签名 / RemoteLeft 事件 |
| `crates/client/src/app.rs` | Game 新字段 + RemotePlayerState + entity_color + assemble 新默认值 |
| `crates/client/src/chunk_assembler.rs` | **新增**：ChunkAssembler + 4 测试 |
| `crates/client/src/lib.rs` | poll_net 重写 / flush_server_outbox / apply_room_event 扩展 / apply_server_message 扩展 / render_game_frame wire-up / start_* 无 Hello |
| `crates/client/src/prediction.rs` | +InputHistory + reconcile_self + 4 测试 |
| `crates/client/src/interp.rs` | 完整重写：PlayerInterp + 5 测试 |
| `crates/render/src/passes/player.rs` | **新增**：PlayerPass + PlayerInstance + WGSL shader |
| `crates/render/src/passes/mod.rs` | `pub mod player;` |
| `crates/render/src/lib.rs` | Renderer 加 player_pass / upload_player_instances / render_players |

---

## 下一阶段

进入 Phase 6：多人 UI/HUD（[`docs/roadmap.md`](docs/roadmap.md) §Phase 6）。要做的事：
- HUD 玩家列表 widget
- 聊天框（T 键打开）
- 远端玩家头顶名牌（egui billboard）
- EscMenu 设置（FOV / 灵敏度 / 渲染距离 / 插值延迟）
- 设置存 localStorage
- Disconnected 页面 + "返回大厅" 按钮
