# 网络协议

> **何时阅读**：增删/改消息字段；改通道可靠性；调快照同步流程；排查协议兼容
> **关联文档**：[`README.md`](../../README.md) · [`modules/core.md`](../modules/core.md) · [`modules/net.md`](../modules/net.md) · [`networking/signaling.md`](signaling.md) · [`networking/prediction.md`](prediction.md)

---

## 一、协议总览

`VoxWeb` 在 P2P DataChannel 上跑两类消息：
- `ClientMessage`：客户端 → 服务端（Local 或 Host 都接收）
- `ServerMessage`：服务端 → 客户端（Local 自己 / Host 给 Remote）

序列化使用 `bincode`（little-endian + varint），定义在 [`crates/core/src/protocol.rs`](../modules/core.md#五protocolrs--网络消息)。

每条消息都通过两条 `DataChannel` 之一发送：
- **`reliable`**：ordered + reliable（默认 SCTP 行为）
- **`unreliable`**：unordered + 0 retransmits（适合频繁的状态更新）

---

## 二、协议版本

```rust
pub const PROTOCOL_VERSION: u32 = 9; // FreeObject 动态生命周期
```

每次破坏性修改必须递增。客户端 `Hello.version != PROTOCOL_VERSION` 时 Host 立即关闭连接（不发 Welcome）。

| 版本 | 变化 |
|---|---|
| v10 | 新增 `FreeObjectSpawnBatch` / `FreeObjectStateBatch` / `FreeObjectProjectBatch`；软材质颗粒（沙/土/草）改为批量 active FreeObject 下落动画，tick 内多个 grain 的 spawn/state/project 各合并成一条消息；硬材质 tick 状态/投影也走 batch |
| v9 | `FreeObjectProject` 不再承担下落动画；新增 `FreeObjectSpawn` 和 `FreeObjectState`，active FreeObject 由 Host 按 tick 推进，Remote 跟随权威状态，静止后再 `FreeObjectProject` |
| v8 | 新增 `FreeObjectProject { object_id, deltas }`；`FloatingOnly` 硬材质小连通块坍落以 FreeObject 投影事件同步 |
| v7 | `ChunkRequest` 改为 `FieldRequest`；`ChunkSnapshot` 改为 `FieldSnapshot`，payload 使用 `field::encode(FieldChunk)`；`BlockUpdate` 改为 `FieldDelta { pos, cell: MaterialCell }` |
| v6 | `Welcome` 增加 `host_render_distance: u32`；`ChunkRequest` 增加 `center` / `render_distance`；新增 `HostSettings` 用于 Host 视距变化通知 |
| v5 | 新增 `ClientMessage::ChunkRequest { chunks: Vec<ChunkPos> }`；Remote 移动或渲染距离变化时向 Host 请求视距内缺失区块 |
| v4 | `Break` / `Place` 增加 `input_tick: u32` 与 `player_position: Vec3`；Host 用点击时玩家脚底位置校验挖放范围，避免高 RTT / unreliable 丢包时用旧位置误拒 |
| v3 | `PlayerSnapshot` 增加 `last_input_tick: u32`；客户端用它对齐预测历史，避免高延迟下把旧回声当成当前位置校正 |
| v2 | Welcome 增加 `host_entity_id: u32` + `players: Vec<PlayerEntry>`；新增 `PlayerEntry { entity_id, display_name }` |

---

## 三、消息总表

### Client → Server

| 消息 | 通道 | 频率 | 字段 | 说明 |
|---|---|---|---|---|
| `Hello` | reliable | 一次（连接建立后） | `display_name: String, version: u32` | 加入握手 |
| `PlayerInput` | unreliable | 60Hz | `tick: u32, position: Vec3, yaw: f32, pitch: f32` | 玩家移动同步；`tick` 是该客户端本地单调递增的输入序号 |
| `FieldRequest` | reliable | 按需 | `center: ChunkPos, render_distance: u32, chunks: Vec<ChunkPos>` | Remote 请求 Host 发送自己有效视距内缺失的 FieldChunk |
| `Break` | reliable | 按需 | `pos: Position, request_id: u32, input_tick: u32, player_position: Vec3` | 挖方块；携带点击时脚底位置用于高延迟范围校验 |
| `Place` | reliable | 按需 | `pos: Position, block: BlockID, request_id: u32, input_tick: u32, player_position: Vec3` | 放方块；携带点击时脚底位置用于高延迟范围/重叠校验 |
| `Chat` | reliable | 按需 | `content: String` | 文字聊天（≤ 256 字符） |
| `Ping` | unreliable | 5s | `client_time_ms: u64` | 时延探测，可选 |
| `Goodbye` | reliable | 一次（断开前） | 无 | 优雅关闭（v2） |

### Server → Client

| 消息 | 通道 | 频率 | 字段 | 接收对象 | 说明 |
|---|---|---|---|---|---|
| `Welcome` | reliable | 一次 | `entity_id: u32, server_tick: u32, world_seed: u64, host_entity_id: u32, host_render_distance: u32, players: Vec<PlayerEntry>` | 单一 | 加入握手响应（v2 起含全员名单；v6 起含 Host 视距上限） |
| `FieldSnapshot` | reliable | 一次（按 chunk） | `pos: ChunkPos, frag_index: u16, frag_total: u16, payload: Vec<u8>` | 单一 | 全量 FieldChunk 数据，分片 |
| `FieldDelta` | reliable | 按需 | `pos: Position, cell: MaterialCell` | 广播 | 单 cell 变更 |
| `FreeObjectSpawn` | reliable | 按需 | `object_id: ObjectID, cells: Vec<(Position, MaterialCell)>` | 广播 | 从静态场提取 active FreeObject；对应 cell 已从 Field 移除（硬材质挖放即时提取用） |
| `FreeObjectState` | unreliable | 最高 60Hz | `object_id: ObjectID, position: Vec3, velocity: Vec3` | 广播 | active FreeObject 的权威动态状态；丢旧包不影响最终结果（join 快照单发用） |
| `FreeObjectProject` | reliable | 按需 | `object_id: ObjectID, deltas: Vec<(Position, MaterialCell)>` | 广播 | FreeObject 静止后投影回静态场，并结束 active 生命周期 |
| `FreeObjectSpawnBatch` | reliable | 按需 | `spawns: Vec<(ObjectID, Vec<(Position, MaterialCell)>)>` | 广播 | 批量提取（软材质颗粒级联）；一帧多个 grain 合一条，语义等价多条 `FreeObjectSpawn` |
| `FreeObjectStateBatch` | unreliable | 最高 60Hz | `states: Vec<(ObjectID, Vec3, Vec3)>` | 广播 | 每 tick 把所有活跃对象（含 grain）状态合一条；语义等价多条 `FreeObjectState` |
| `FreeObjectProjectBatch` | reliable | 按需 | `projections: Vec<(ObjectID, Vec<(Position, MaterialCell)>)>` | 广播 | 同帧落定的多个对象合一条；语义等价多条 `FreeObjectProject` |
| `ActionAck` | reliable | 应答 | `request_id: u32, accepted: bool, reason: AckReason` | 单一 | 挖放应答 |
| `PlayerTick` | unreliable | 60Hz | `tick: u32, players: Vec<PlayerSnapshot>, server_time_ms: u64` | 广播 | 全员位置广播 |
| `PeerJoined` | reliable | 按需 | `entity_id: u32, display_name: String` | 广播（除新加入者） | 新玩家加入通告 |
| `PeerLeft` | reliable | 按需 | `entity_id: u32` | 广播 | 玩家离开通告 |
| `Chat` | reliable | 按需 | `from: u32, content: String` | 广播 | 聊天广播 |
| `Pong` | unreliable | 应答 | `client_time_ms: u64, server_time_ms: u64` | 单一 | Ping 应答 |
| `HostSettings` | reliable | 按需 | `render_distance: u32` | 广播 | Host 视距变化；Remote 更新有效视距上限 |
| `Kick` | reliable | 按需 | `reason: String` | 单一 | 主机踢人（v2） |

### 字段类型说明

| 类型 | 含义 |
|---|---|
| `BlockID` | `u16` |
| `MaterialCell` | `{ occupancy: u8, primary: MaterialID, secondary: Option<MixSlot>, flags: CellFlags }` |
| `ObjectID` | `u64`，由 Host / Local-Only 权威分配 |
| `Position` | `{ x: i32, y: i32, z: i32 }` |
| `ChunkPos` | `{ x: i32, z: i32 }` |
| `Vec3` | `[f32; 3]`（glam::Vec3 的 serde 表示） |
| `String` | UTF-8，bincode 默认带 varint 长度前缀 |
| `u32 request_id` | 客户端单调递增，用于 ActionAck 配对 |
| `PlayerInput.tick` | 客户端本地 60Hz 输入序号，用于服务端丢弃乱序输入，也用于客户端协调 |
| `FieldRequest.render_distance` | Remote 已按 Host 上限裁剪后的有效视距；Host 再取 `min(request, host_render_distance)` 做最终校验 |
| `Break/Place.input_tick` | 玩家点击时已生成的最新本地输入序号；用于日志/调试，并允许 Host 判断该操作来自哪个预测时刻 |
| `Break/Place.player_position` | 玩家点击时的脚底位置；Host 用它做挖放距离和放置重叠校验，避免可靠操作包先于最新 `PlayerInput` 到达时被旧位置误拒 |
| `PlayerTick.tick` | 服务端 60Hz 累计 tick，用于远端插值、调试和 UI |
| `PlayerSnapshot.last_input_tick` | 服务端已接受到该玩家的最新输入序号；本玩家收到自身快照时用它查找同一输入时刻的预测记录 |

> **中继兜底**：当 Host 与某 Remote 的 P2P 直连失败，该对 peer 会自动切换为通过信令 Worker 的 WebSocket 中继 bincode 字节（详见 [`signaling.md`](signaling.md) §九）。
> 此时 `reliable` / `unreliable` 两个通道在该对方向上**统一退化为 reliable+ordered**（WS 自带语义），原 unreliable 的「丢了无所谓」不再保留。
> 接收侧按消息 `kind` 而非 channel 分发，正确性不变；现象上只会偶发额外延迟，不会出现错乱或丢消息。

---

## 四、连接握手序列

```
Remote                                 Host
──────                                 ────
DataChannel 双通道 OPEN
   │
   │── (reliable) Hello{name, version} ─▶
   │                                       验证 version
   │                                       server.add_player → entity_id
   │◀── (reliable) Welcome{entity_id, server_tick, world_seed, host_render_distance}
   │
   │◀── (reliable) bootstrap FieldSnapshot（出生点默认半径）
   │── (reliable) FieldRequest{chunks=[视距内缺失区块]} ─▶
   │◀── (reliable) FieldSnapshot 分片（补齐 Remote 视距）
   │
   │◀── (reliable) PeerJoined{entity_id=X, name="Alice"}（其它老玩家收到）
   │
   InGame state，开始 60Hz PlayerInput / PlayerTick 流
```

**Welcome 之前不发 PlayerInput**：客户端等到 Welcome 收到才进入 InGame；其间显示"连接中"。

---

## 五、Field 快照同步

Remote 客户端应始终维护自己视距范围内的 chunk 集合。

- 加入房间时 Host 先推送出生点附近的 bootstrap 快照（当前半径 6），让默认设置能尽快预载。
- Remote 收到 `Welcome` 后计算有效视距 `effective_render_distance = min(local_render_distance, host_render_distance)`，按该半径请求缺失区块；渲染距离大于 bootstrap 半径时会立刻请求外圈。
- InGame 后 Remote 每次跨 chunk 边界或渲染距离变化，都会重新计算 desired 集合，只请求尚未加载且不在 in-flight 的缺失区块。
- Host 收到请求后先确认请求中心离服务端记录的玩家位置足够近，再按 `min(request.render_distance, host_render_distance)` 校验每个 chunk，合法才 `ensure_chunk_generated` 并用 `FieldSnapshot` 分片回传。
- Host 修改自身视距时广播 `HostSettings`；Remote 收到后立即更新有效视距并在下一帧卸载超出范围的本地缓存。

### FieldChunk 编码

FieldSnapshot 发送前通过 `voxweb_core::field::encode(field)` 编码 `FieldChunk`：

```rust
struct StoredFieldChunk {
    version: u8,
    field: FieldChunk,
}
```

编码后整体做 bincode 序列化，作为 `FieldSnapshot.payload`。`FieldChunk` 内部使用 16×16 column store：自然地形列为 `Column::Spans(Vec<Span>)`，高熵编辑列可退化为 `Column::Dense(Box<[MaterialCell]>)`。

| FieldChunk 内容 | dense 等价大小 | FieldChunk 编码后 |
|---|---|---|
| 全 AIR | ~128 KB | < 20 B |
| 全 STONE | ~128 KB | < 20 B |
| 典型地形（草/泥/石/空气） | ~128 KB | 2-5 KB |
| 高多样性建筑 | ~128 KB | 8-20 KB |

典型地形压缩比约 **30x-60x**，初始快照（169 chunk）从 ~22 MB dense 等价降至约 ~0.5 MB。

### 分片规则

```
单个 FieldChunk 编码后 payload 通常远小于 dense 等价大小
DataChannel SCTP 用户消息上限 ≈ 16KB（不同浏览器实现略异，保守 14KB）

→ payload 切片为 ≤ 14336 字节
→ 每片附 (frag_index, frag_total) header
→ 接收端按 ChunkPos 维度组装，齐了就 field::decode 解码
```

典型地形 FieldChunk 编码后仅 2-5 KB，**大多数 chunk 不需分片**（frag_total=1）。

### 接收端组装

ChunkAssembler 只负责拼字节，不关心编码格式。组装完成后调用 `field::decode()` 解码为 `FieldChunk`，再写入 `server.world.load_field_chunk_from_storage` 同步 FieldChunk 与 dense Chunk 视图。

```rust
if let Some(full) = assembler.ingest(pos, frag_index, frag_total, payload) {
    let field = voxweb_core::field::decode(&full)?;
    server.load_field_chunk_from_storage(pos, field);
}
```

### 流量控制

- 浏览器 SCTP 自带流控（`bufferedAmount`）
- Reliable DataChannel 设置 `bufferedAmountLowThreshold = 256 KB`
- `PeerConnection::send()` 发送后检查 `bufferedAmount`，超过 **1 MB 高水位**时设置暂停标志
- 暂停后该 peer 的 reliable 消息被推迟到下帧发送，等待 `onbufferedamountlow` 事件清除暂停标志
- `host_route_outbox()` 返回流控阻塞的未发送消息，由 `flush_server_outbox()` 重新入队 server.outbox
- 中继模式额外使用 Worker 下发的 `max_rate` 做客户端本地令牌桶节流（默认按 80% 留余量）；高视距产生的大量 `FieldSnapshot` 会分帧发送，避免触发 `relay_closed{reason:"rate_limit"}`

---

## 六、运行时双向流

### 6.1 玩家移动（unreliable，delta 广播）

```
Remote                          Host
──────                          ────
逻辑帧（60Hz）：
   PlayerInput{tick=input_seq, pos, yaw, pitch}  ────▶
                                          server.handle_player_input
                                          （限速校验 → 接受/截断）
                                          server.tick() 末尾：
   PlayerTick{tick=N, players=[...], time_ms}  ◀────（广播给所有 peer，包括来源）
   ↓
   client.prediction.reconcile_self(snapshot)   ← 用 snapshot.last_input_tick 与本地预测历史对齐
   client.interp.ingest_tick(snapshot)         ← 远端玩家入插值缓冲
```

**Delta 广播规则**（`Server::broadcast_tick`）：

- 位置变化 < 0.01m（约 0.1m 位移）且朝向变化 < 0.5° 的玩家**不包含**在本 tick 的 `players` 列表中
- 每 30 tick（0.5s @ 60Hz）强制全量广播一次，防止丢包导致远端玩家冻结
- 新加入玩家（首次广播）始终包含
- 即使所有玩家都静止，仍发送空的 `PlayerTick` 以维持 `server_time_ms` 时钟同步

**效果**：静止或缓慢移动的玩家消耗零带宽；多数场景节省 50-80% PlayerTick 带宽。

详见 [`prediction.md`](prediction.md)。

### 6.2 方块挖放（reliable + ack）

```
Remote                          Host
──────                          ────
鼠标左键命中 (10,64,5)：
   prediction.optimistic_break((10,64,5))     ← 本地立即半透明预览
   Break{pos=(10,64,5), request_id=42,
         input_tick=810, player_position=feet_at_click}  ────▶
                                          physics::validate_break →
                                          OK：
                                            world.set_cell (10,64,5) EMPTY
                                            mark_edit_unstable(pos)  ← 软材质邻域入队，颗粒下落推迟到 tick
                                            resolve_floating_after_edit(pos)  ← 硬材质浮空即时提取
                                            outbox: ActionAck{42, accepted=true}
                                            outbox: FieldDelta{(10,64,5), AIR cell}（广播）
                                            outbox: optional FreeObjectSpawn...（广播）
                                          server.tick():
                                            step_soft_grains → FreeObjectSpawnBatch / 超预算 try_relax_one FieldDelta
                                            outbox: FreeObjectState...（广播，unreliable）
                                            settled → FreeObjectProject...（广播，reliable）
                                          NG：
                                            outbox: ActionAck{42, accepted=false, reason}
   ◀── (reliable) ActionAck{42, accepted=true}
   prediction.commit(42)
   ◀── (reliable) FieldDelta{(10,64,5), AIR cell}
   client.world_view.set_cell AIR
   mesh_jobs.enqueue(chunk_pos)
```

若被挖放的材质或其邻近材质触发 `ImmediateRelaxation`，Host 会在同一可靠通道继续广播一串 `FieldDelta`。Remote 不独立决定最终滑落位置，只把这些更新按顺序写入本地世界并重网格化受影响 chunk。

若硬建筑材质触发 `FloatingOnly` 坍落，Host 会先生成 `FreeObjectSpawn`：其中包含本次提取对象的 `object_id` 和被移出静态场的 cell。随后 Host / Local-Only 在 60Hz 逻辑 tick 中推进 active FreeObject 的 AABB 动态体，并通过 unreliable `FreeObjectState` 广播当前位置和速度。Remote 只应用这些权威状态，不决定落点。对象静止后 Host 发送 reliable `FreeObjectProject`，客户端删除 active 对象、写入最终静态 cell，并重网格化受影响 chunk。

当前 FreeObject 动态体的第一版约束：

- 只有平移 AABB，没有旋转、反弹或碎裂。
- 玩家碰撞、raycast 和放置校验会把 active FreeObject AABB 纳入查询。
- `FreeObjectProject` 是生命周期结束事件，不再用于播放投影后的假下落动画。

```
若 ActionAck.accepted=false：
   prediction.rollback(42)        ← 撤销本地预览
   UI 提示（toast / 红框闪烁）
```

### 6.3 文本聊天（reliable）

```
Remote → Chat{content="hello"} → Host
Host → Chat{from=remote_id, content="hello"} → 广播（包括来源，让发送者也看到自己消息）
```

---

## 七、心跳与时延探测

可选：每 5 秒 Remote 发 `Ping{client_time_ms}` → Host 立即 `Pong{client_time_ms, server_time_ms}` → Remote 计算 RTT。

用于：
- HUD 显示当前延迟
- 时钟同步（PlayerTick 携带 server_time_ms，Remote 用 RTT 估算偏移）

详见 [`prediction.md` 时钟同步章节](prediction.md#四时钟同步)。

---

## 八、防作弊（最低限度）

完整方案见 [`modules/server.md`](../modules/server.md)。本协议层强制：
- `PlayerInput.position` 与上次相比 distance > `max_move_per_tick * dt` → 截断为合法距离，不接受
- `Break.pos` 与玩家位置 distance > 6.0 → ActionAck rejected with `OutOfRange`
- `Place.pos` 同上 + 不能与玩家 AABB 重叠 → ActionAck rejected with `Overlap`
- `Chat.content.chars().count() > 256` → 静默丢弃（不应答错误，避免被穷举）
- 速率限制：每玩家 5 条 / 3s（180 tick 滑窗），超出静默丢弃（Protocol v1 没有）
- 操作签名 / 加密令牌（DataChannel 已 DTLS 加密）

---

## 九、消息大小预算

| 消息 | 典型大小（含 bincode 开销） | 备注 |
|---|---|---|
| `Hello` | 30-60 字节 | 名字最多 32 字符 |
| `Welcome` | 16 字节 | |
| `PlayerInput` | 24 字节 | varint 后 |
| `PlayerTick`（4 玩家，delta） | ~30-120 字节 | delta 模式下多数 tick 只含 0-2 个玩家 |
| `PlayerTick`（8 玩家，全量） | ~220 字节 | 每 0.5s 强制全量 |
| `FieldRequest`（一次移动外圈） | 100-400 字节 | 请求中心 + 有效视距 + 缺失 `ChunkPos` 列表；完整视距首包最多约 441 个 chunk |
| `Break/Place` | 16-20 字节 | |
| `FieldDelta` | 20-40 字节 | 取决于 `MaterialCell` secondary slot |
| `FreeObjectSpawn`（小硬块提取） | 40B-数 KB | 取决于 sample/cell 数量 |
| `FreeObjectState` | ~30 字节 | unreliable，旧状态可丢弃 |
| `FreeObjectProject`（小硬块静止） | 40B-数 KB | 取决于投影 delta 数量 |
| `FieldSnapshot`（典型地形 chunk） | 2-5 KB | FieldChunk column store 编码后；通常不需分片 |
| `FieldSnapshot`（高多样性 chunk） | 8-20 KB | 可能需要 1-2 片 |
| `Chat`（短消息） | 30-100 字节 | |
| `ActionAck` | 12 字节 | |

bootstrap 初始快照（169 chunk）：典型地形 ~0.5 MB（dense 等价 ~22 MB）。渲染距离更大时，Remote 会通过 `FieldRequest` 只补请求外圈。

---

## 十、协议演进规则

### 允许的修改
- 在 `ClientMessage` / `ServerMessage` 末尾追加新变体（旧客户端解码失败 → 视作不兼容版本，断开）
- 增加新的 reliable 消息（无副作用）

### 禁止的修改
- 修改已有变体的字段名/类型
- 删除已有变体
- 修改字段顺序（bincode 顺序敏感）

任何破坏性修改：递增 `PROTOCOL_VERSION` + 同步更新 [`modules/core.md`](../modules/core.md) + 本文档。

---

## 十一、可选演进

- 操作日志同步（让 Remote 重放 FieldDelta 历史，避免重复全量同步）
- 端到端加密（双重保险）
- 更复杂的 Welcome 流程（双向能力协商，如纹理包版本）
