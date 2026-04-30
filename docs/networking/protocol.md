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
pub const PROTOCOL_VERSION: u32 = 1;
```

每次破坏性修改必须递增。客户端 `Hello.version != PROTOCOL_VERSION` 时 Host 立即关闭连接（不发 Welcome）。

---

## 三、消息总表

### Client → Server

| 消息 | 通道 | 频率 | 字段 | 说明 |
|---|---|---|---|---|
| `Hello` | reliable | 一次（连接建立后） | `display_name: String, version: u32` | 加入握手 |
| `PlayerInput` | unreliable | 60Hz | `tick: u32, position: Vec3, yaw: f32, pitch: f32` | 玩家移动同步 |
| `Break` | reliable | 按需 | `pos: Position, request_id: u32` | 挖方块 |
| `Place` | reliable | 按需 | `pos: Position, block: BlockID, request_id: u32` | 放方块 |
| `Chat` | reliable | 按需 | `content: String` | 文字聊天（≤ 256 字符） |
| `Ping` | unreliable | 5s | `client_time_ms: u64` | 时延探测，可选 |
| `Goodbye` | reliable | 一次（断开前） | 无 | 优雅关闭，可选 |

### Server → Client

| 消息 | 通道 | 频率 | 字段 | 接收对象 | 说明 |
|---|---|---|---|---|---|
| `Welcome` | reliable | 一次 | `entity_id: u32, server_tick: u32, world_seed: u64` | 单一 | 加入握手响应 |
| `ChunkSnapshot` | reliable | 一次（按 chunk） | `pos: ChunkPos, frag_index: u16, frag_total: u16, payload: Vec<u8>` | 单一 | 全量 chunk 数据，分片 |
| `BlockUpdate` | reliable | 按需 | `pos: Position, block: BlockID` | 广播 | 单方块变更 |
| `ActionAck` | reliable | 应答 | `request_id: u32, accepted: bool, reason: AckReason` | 单一 | 挖放应答 |
| `PlayerTick` | unreliable | 60Hz | `tick: u32, players: Vec<PlayerSnapshot>, server_time_ms: u64` | 广播 | 全员位置广播 |
| `PeerJoined` | reliable | 按需 | `entity_id: u32, display_name: String` | 广播（除新加入者） | 新玩家加入通告 |
| `PeerLeft` | reliable | 按需 | `entity_id: u32` | 广播 | 玩家离开通告 |
| `Chat` | reliable | 按需 | `from: u32, content: String` | 广播 | 聊天广播 |
| `Pong` | unreliable | 应答 | `client_time_ms: u64, server_time_ms: u64` | 单一 | Ping 应答 |
| `Kick` | reliable | 按需 | `reason: String` | 单一 | 主机踢人（v2） |

### 字段类型说明

| 类型 | 含义 |
|---|---|
| `BlockID` | `u16` |
| `Position` | `{ x: i32, y: i32, z: i32 }` |
| `ChunkPos` | `{ x: i32, z: i32 }` |
| `Vec3` | `[f32; 3]`（glam::Vec3 的 serde 表示） |
| `String` | UTF-8，bincode 默认带 varint 长度前缀 |
| `u32 request_id` | 客户端单调递增，用于 ActionAck 配对 |
| `u32 tick` | 服务端 60Hz 累计 tick |

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
   │◀── (reliable) Welcome{entity_id, server_tick, world_seed}
   │
   │   ┌── (reliable) ChunkSnapshot pos=(0,0) frag 0/3 ─▶ │
   │◀──┤── (reliable) ChunkSnapshot pos=(0,0) frag 1/3 ─▶ │
   │   └── (reliable) ChunkSnapshot pos=(0,0) frag 2/3 ─▶ │
   │   ... 渲染距离内全部 chunk ...
   │
   │◀── (reliable) PeerJoined{entity_id=X, name="Alice"}（其它老玩家收到）
   │
   InGame state，开始 60Hz PlayerInput / PlayerTick 流
```

**Welcome 之前不发 PlayerInput**：客户端等到 Welcome 收到才进入 InGame；其间显示"连接中"。

---

## 五、Chunk 快照同步

加入新成员时 Host 把渲染距离内所有 chunks 发给该成员。

### 分片规则

```
单个 chunk payload（bincode 序列化后）≈ 50KB（典型，65536 块大量 AIR 经 RLE 优化前）
DataChannel SCTP 用户消息上限 ≈ 16KB（不同浏览器实现略异，保守 14KB）

→ payload 切片为 ≤ 14336 字节
→ 每片附 (frag_index, frag_total) header
→ 接收端按 ChunkPos 维度组装，齐了就 deserialize
```

> **优化建议**：发送前可对 chunk.blocks 做简单 RLE（连续相同 BlockID 压缩）。本期可选；不做时单 chunk 130KB（65536×2 字节），分 10 片左右。

### 接收端组装

```rust
pub struct ChunkAssembler {
    fragments: HashMap<ChunkPos, Vec<Option<Vec<u8>>>>,
}

impl ChunkAssembler {
    pub fn ingest(&mut self, msg: ChunkSnapshot) -> Option<(ChunkPos, Chunk)> {
        let entry = self.fragments.entry(msg.pos)
            .or_insert_with(|| vec![None; msg.frag_total as usize]);
        entry[msg.frag_index as usize] = Some(msg.payload);
        if entry.iter().all(|x| x.is_some()) {
            let bytes: Vec<u8> = entry.drain(..).flatten().flatten().collect();
            self.fragments.remove(&msg.pos);
            let chunk: Chunk = core::decode(&bytes).ok()?;
            Some((msg.pos, chunk))
        } else { None }
    }
}
```

### 流量控制

- 浏览器 SCTP 自带流控（`bufferedAmount`）
- Host 在 `bufferedAmount > THRESHOLD` 时暂停发送下一片，等 `bufferedamountlow` 事件触发后继续
- 阈值建议 1MB

---

## 六、运行时双向流

### 6.1 玩家移动（unreliable）

```
Remote                          Host
──────                          ────
逻辑帧（60Hz）：
   PlayerInput{tick=N, pos, yaw, pitch}  ────▶
                                          server.handle_player_input
                                          （限速校验 → 接受/截断）
                                          server.tick() 末尾：
   PlayerTick{tick=N, players=[...], time_ms}  ◀────（广播给所有 peer，包括来源）
   ↓
   client.prediction.reconcile_self(snapshot)   ← 与本地预测对比
   client.interp.ingest_tick(snapshot)         ← 远端玩家入插值缓冲
```

详见 [`prediction.md`](prediction.md)。

### 6.2 方块挖放（reliable + ack）

```
Remote                          Host
──────                          ────
鼠标左键命中 (10,64,5)：
   prediction.optimistic_break((10,64,5))     ← 本地立即半透明预览
   Break{pos=(10,64,5), request_id=42}  ────▶
                                          physics::validate_break →
                                          OK：
                                            world.set_block AIR
                                            outbox: ActionAck{42, accepted=true}
                                            outbox: BlockUpdate{(10,64,5), AIR}（广播）
                                          NG：
                                            outbox: ActionAck{42, accepted=false, reason}
   ◀── (reliable) ActionAck{42, accepted=true}
   prediction.commit(42)
   ◀── (reliable) BlockUpdate{(10,64,5), AIR}
   client.world_view.set_block AIR
   mesh_jobs.enqueue(chunk_pos)
```

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
- `Chat.content.len() > 256` → 静默丢弃（不应答错误，避免被穷举）

**不做**：
- 速率限制（依赖 SCTP 自有限制）
- 操作签名 / 加密令牌（DataChannel 已 DTLS 加密）

---

## 九、消息大小预算

| 消息 | 典型大小（含 bincode 开销） | 备注 |
|---|---|---|
| `Hello` | 30-60 字节 | 名字最多 32 字符 |
| `Welcome` | 16 字节 | |
| `PlayerInput` | 24 字节 | varint 后 |
| `PlayerTick`（4 玩家） | ~120 字节 | 每玩家 ~24 字节 + header |
| `PlayerTick`（8 玩家） | ~220 字节 | 同上 |
| `Break/Place` | 16-20 字节 | |
| `BlockUpdate` | 16 字节 | |
| `ChunkSnapshot` 单片 | ≤ 14KB | 分片上限 |
| `Chat`（短消息） | 30-100 字节 | |
| `ActionAck` | 12 字节 | |

60Hz × 8 玩家广播带宽 ≈ 220 × 60 = 13KB/s out（Host 单向）。家用上行（5Mbps = 625KB/s）足够支撑 30+ 玩家。

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

## 十一、Future Work（v2/v3）

- 操作日志同步（让 Remote 重放 BlockUpdate 历史，避免重复全量同步）
- Delta 玩家广播（仅广播位置变化的玩家）
- 区块按需请求（Remote 走远后请求新 chunk，而非加入时一次性发全部）
- 端到端加密（双重保险）
- 更复杂的 Welcome 流程（双向能力协商，如纹理包版本）
