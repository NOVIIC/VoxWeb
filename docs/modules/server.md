# `server` 模块设计

> **何时阅读**：改服务端权威逻辑；改地形生成；改物理仲裁；改持久化触发
> **关联文档**：[`README.md`](../../README.md) · [`architecture.md`](../architecture.md) · [`modules/core.md`](core.md) · [`networking/protocol.md`](../networking/protocol.md) · [`features/physics.md`](../features/physics.md) · [`features/persistence.md`](../features/persistence.md)

---

## 一、职责

`server` 是**世界权威**：所有可信状态在这里维护与变更。
- 区块管理：按需生成 / 加载 / 卸载
- 玩家状态：入会、位置、离会
- 物理仲裁：拒绝非法位置（穿墙、瞬移）
- 方块挖放仲裁：射程、合法性、广播变更
- 持久化触发：维护 dirty 集合，让 client 异步任务执行 IndexedDB 写入

**部署形态**：
- **Local-Only**：内嵌于 client，输入输出走内存通道
- **Host**：内嵌于 client，但消息源/汇是 P2P DataChannel + 自身本地通道（混合）
- **不存在独立进程形态**（浏览器内无独立 server bin）

---

### 阶段实装范围

本文档涵盖 server crate 的**长期完整设计**。各阶段实装范围：

| 阶段 | 包含 |
|---|---|
| **Phase 2 ✅** | `World::ensure_chunk_generated` / `get_block_world` / `unload_chunk`；`TerrainGenerator`（已在 Phase 1 stub）；`Server::handle_message` 仅 `Hello → Welcome` 占位 + `Break/Place` 无校验直改 |
| Phase 3 | `physics::validate_break/place` 仲裁；`World::dirty_chunks` 字段；Break/Place 完整闭环 |
| Phase 5 | `PlayerEntity` 表 + `add_player/remove_player`；`PlayerInput` 限速校验；`broadcast_tick`（PlayerTick 广播）；`send_initial_snapshot`；`take_dirty_chunks` / `ChunkStorage` trait |

下面 §3 起描述的是**完整设计**；每节遇到 Phase 3+ 才引入的字段会用 `> Phase N` 注明。

---

## 二、目录结构

```
crates/server/src/
├── lib.rs              World + Server 主结构 + 公开 API
├── world.rs            ChunkStore + EntityTable + Tick
├── terrain.rs          Perlin 地形生成
├── physics.rs          物理仲裁（位置合法性、挖放校验）
└── persistence.rs      Persistence trait（具体实现在 client::storage）
```

---

## 三、核心数据结构

### `World`

```rust
pub struct World {
    pub seed: u64,
    pub chunks: HashMap<ChunkPos, Chunk>,
    pub terrain: TerrainGenerator,           // [Phase 2] Perlin 高度图
    pub tick_count: u64,                     // [Phase 2] tick 累加器；Phase 5 起驱动玩家广播
    pub dirty_chunks: HashSet<ChunkPos>,     // [Phase 3] 需要持久化的 chunk
    pub players: HashMap<EntityId, PlayerEntity>,  // [Phase 5]
}

impl World {
    /// [Phase 2] 若 chunk 未生成则调 terrain 生成并插入；已存在则跳过。
    pub fn ensure_chunk_generated(&mut self, pos: ChunkPos);

    /// [Phase 2] 世界坐标查询；chunk 未加载或 y 越界一律返回 AIR。
    /// 用于 chunk_mesh::generate_with_neighbors 的回调。
    pub fn get_block_world(&self, wx: i32, wy: i32, wz: i32) -> BlockID;

    /// [Phase 2] 卸载 chunk（移除 chunks 表）。dirty_chunks 不在 Phase 2 使用。
    pub fn unload_chunk(&mut self, pos: ChunkPos);

    /// [Phase 1/2] 直接读写方块（Phase 3 起会触发 dirty）。
    /// Phase 2：`get_block` 等价于 `get_block_world(pos.x, pos.y, pos.z)`；
    /// `set_block` 在 chunk 未加载或 local_index 越界时静默忽略。
    pub fn get_block(&self, pos: Position) -> BlockID;
    pub fn set_block(&mut self, pos: Position, block: BlockID);

    /// [Phase 1/2] 推进 tick 计数（Phase 5 起会驱动玩家广播）。
    pub fn tick(&mut self);
}

pub type EntityId = u32;     // [Phase 5]

/// [Phase 5] 玩家实体
pub struct PlayerEntity {
    pub entity_id: EntityId,
    pub display_name: String,
    pub position: glam::Vec3,
    pub yaw: f32,
    pub pitch: f32,
    pub last_input_tick: u32,                // 用于丢弃过期输入
    pub joined_at_tick: u32,
}
```

### `Server`

```rust
pub struct Server {
    pub world: World,
    pub tick: u32,                           // [Phase 1+]
    pub seed: u64,                           // [Phase 1+]
    pub config: ServerConfig,                // [Phase 3+]
    pub outbox: VecDeque<OutboundMessage>,   // [Phase 5] 待广播的消息
}

/// [Phase 3+]
pub struct ServerConfig {
    pub render_distance_chunks: u32,
    pub max_break_range: f32,                // 6.0 默认
    pub max_move_per_tick: f32,              // 防穿墙：上限速度 × dt
}

/// [Phase 5]
pub struct OutboundMessage {
    pub recipient: Recipient,
    pub message: ServerMessage,
}

pub enum Recipient {
    All,                                     // 广播
    Except(EntityId),                        // 排除某玩家（通常是来源）
    One(EntityId),
}

impl Server {
    /// [Phase 1+]
    pub fn new(seed: u64) -> Self;

    /// [Phase 1+] 处理来自客户端的消息（Local 或 Remote）。
    /// Phase 2：仅 Hello→Welcome 占位 + Break/Place 直改（无校验）。
    /// Phase 3：加入 validate_break/place + dirty 标记。
    /// Phase 5：完整 dispatch + outbox。
    pub fn handle_message(&mut self, sender: u32, msg: ClientMessage) -> Vec<ServerMessage>;

    /// [Phase 1+] 推进一个逻辑帧（60Hz 调用）。Phase 2 仅累加 tick 计数。
    pub fn tick(&mut self);

    // —— 以下为 Phase 5 引入 ——
    pub fn add_player(&mut self, display_name: String) -> EntityId;
    pub fn remove_player(&mut self, entity_id: EntityId);
    pub fn drain_outbox(&mut self) -> Vec<OutboundMessage>;
    pub fn take_dirty_chunks(&mut self) -> Vec<(ChunkPos, Chunk)>;
    pub fn load_chunk_from_storage(&mut self, pos: ChunkPos, chunk: Chunk);
}
```

---

## 四、`world.rs` — 区块与玩家

### Chunk 生命周期

```
[Phase 2 simplified]
  ChunkLoader 滚动 ──▶ World::ensure_chunk_generated(pos)
                          ├─ 已加载？跳过
                          └─ 未加载？terrain.generate_chunk(pos) → 插入

  ChunkLoader 滚动 ──▶ World::unload_chunk(pos)
                          └─ 直接 remove

[Phase 5 完整版]
  首次访问 ──▶ get_or_generate(pos)
                ├─ 已加载？返回引用
                ├─ 持久化中有？load（异步，先返回 Empty 占位 + 标记 loading）
                └─ 都没有？terrain::generate(seed, pos) → 插入
```

**注意**：`server` 自身**不直接读 IndexedDB**（核心原则：server 无浏览器依赖）。持久化由 `client::storage` 异步任务完成，加载完成后调 `server.load_chunk_from_storage`（Phase 5）。

### 玩家位置更新

> Phase 5 引入。Phase 2 单人 Fly 模式不走该路径。

```rust
fn handle_player_input(&mut self, entity: EntityId, msg: PlayerInput) {
    let player = self.world.players.get_mut(&entity)?;

    // 拒绝过期输入
    if msg.tick <= player.last_input_tick { return; }

    // 限速校验
    let delta = msg.position - player.position;
    let max = self.config.max_move_per_tick * (msg.tick - player.last_input_tick) as f32;
    let new_pos = if delta.length() > max {
        // 超速：拒绝接受，强制回到原位（下次广播让客户端协调）
        player.position
    } else {
        msg.position
    };

    player.position = new_pos;
    player.yaw = msg.yaw;
    player.pitch = msg.pitch;
    player.last_input_tick = msg.tick;
}
```

### 玩家广播

每个 `tick()` 末尾，把所有玩家的当前位置打包成 `PlayerTick` 广播：

```rust
fn broadcast_tick(&mut self) {
    let players: Vec<PlayerSnapshot> = self.world.players.values()
        .map(|p| PlayerSnapshot {
            entity_id: p.entity_id,
            position: p.position,
            yaw: p.yaw,
            pitch: p.pitch,
        }).collect();

    self.outbox.push_back(OutboundMessage {
        recipient: Recipient::All,
        message: ServerMessage::PlayerTick {
            tick: self.world.current_tick,
            players,
            server_time_ms: now_ms(),
        },
    });
}
```

通过 unreliable channel 发送（详见 [`networking/protocol.md`](../networking/protocol.md)）。

---

## 五、`terrain.rs` — 地形生成

> Phase 2 ✅ 已实装基础形态（在 Phase 1 stub 基础上接入 `World::ensure_chunk_generated`）。

### 算法（Phase 2 实装版）

1. `TerrainGenerator::new(seed)`：用 `noise::Perlin::new(seed as u32)` 构造一个噪声源
2. 对 chunk 内每个 `(lx, lz)` 列：
   - 世界坐标 `(world_x, world_z) = (pos.x * 16 + lx, pos.z * 16 + lz)`
   - 采样 `perlin.get([world_x * 0.01, world_z * 0.01])` → 值域 `[-1, 1]`
   - 映射到高度 `height = ((noise + 1) * 0.5 * CHUNK_Y * 0.4) as usize`（最高 ≈ 102）
3. 分层填充每个 `(lx, ly, lz)`：
   - `ly == 0` → 强制 STONE（基岩兜底，避免下溢）
   - `ly + 3 < height` → STONE
   - `ly < height` → DIRT
   - `ly == height` → GRASS
   - `ly > height` → AIR

> 见 [`crates/server/src/terrain.rs`](../../crates/server/src/terrain.rs)。
>
> 注意：Phase 2 仅使用单一 Perlin 通道；多倍频叠加 / 山脉 / 平原差异化留给 v2。

### v2 扩展点
- 生物群系（草原 / 沙漠 / 雪地）
- 树木 / 矿物随机分布
- 水面（海平面以下填 WATER）
- 自定义地形 trait 让模块化（不在本期范围）

---

## 六、`physics.rs` — 物理仲裁

> **Phase 3** 引入。Phase 2 不实装。

> 玩家本地物理预测在 `client::physics`；这里只做**仲裁**（防作弊最低限度）。

### 位置合法性
- 移动距离上限：见 `world.rs` 中已实现
- 穿墙检测：`server.physics::check_position_inside_solid(world, pos)` 不强制；本期客户端预测已经做碰撞，仲裁只在挖放时确保操作位置合理

### 挖方块
```rust
pub fn validate_break(world: &World, entity: EntityId, target: Position) -> Result<(), AckReason> {
    let player = world.players.get(&entity).ok_or(AckReason::Cooldown)?;
    let dist = (player.position - target.as_vec3()).length();
    if dist > MAX_BREAK_RANGE { return Err(AckReason::OutOfRange); }

    let block = world.get_block(target).ok_or(AckReason::BlockNotEmpty)?;
    if block == BlockID::AIR { return Err(AckReason::BlockNotEmpty); }
    Ok(())
}
```

### 放方块
```rust
pub fn validate_place(world: &World, entity: EntityId, pos: Position, block: BlockID) -> Result<(), AckReason> {
    let player = world.players.get(&entity).ok_or(AckReason::Cooldown)?;
    let dist = (player.position - pos.as_vec3()).length();
    if dist > MAX_PLACE_RANGE { return Err(AckReason::OutOfRange); }

    let existing = world.get_block(pos).ok_or(AckReason::BlockNotEmpty)?;
    if existing != BlockID::AIR { return Err(AckReason::BlockNotEmpty); }

    // 检查是否与玩家 AABB 重叠
    let aabb = aabb_for_block(pos);
    let player_aabb = player_aabb_at(player.position);
    if aabb.intersects(&player_aabb) { return Err(AckReason::Overlap); }

    Ok(())
}
```

---

## 七、`tick()` 流程

```rust
// [Phase 2] 仅累加 tick 计数 + 推进 world tick；无玩家广播、无 dirty 处理
pub fn tick(&mut self) {
    self.tick = self.tick.wrapping_add(1);
    self.world.tick();
}

// [Phase 5+] 完整版
pub fn tick(&mut self) {
    self.world.current_tick += 1;

    // 当前实现：仅广播玩家位置；未来扩展：实体物理、方块更新（流体）等
    self.broadcast_tick();

    // 周期性持久化触发：每 30 秒（即每 1800 tick）
    if self.world.current_tick % 1800 == 0 {
        // 不直接写盘，仅把 dirty chunks 暴露出来；client 层 take 后异步写入
        // (此处无需操作，client 通过 take_dirty_chunks 拉取)
    }
}
```

---

## 八、消息分发逻辑

> **Phase 2 范围**：仅 `Hello`→`Welcome { entity_id: 1, server_tick, world_seed }` 与 `Break/Place` 无校验直改。`PlayerInput / Chat / Ping` 一律忽略（返回空）。
>
> **Phase 3** 加入 `validate_break/place` + dirty 标记。
>
> **Phase 5** 完整 dispatch（如下）+ outbox + Recipient。

`Server::handle_client_message` 的核心 dispatch（Phase 5 完整版）：

```rust
match msg {
    ClientMessage::Hello { display_name, version } => {
        if version != PROTOCOL_VERSION { /* 拒绝 */ }
        // entity_id 由调用方在 add_player 时生成
        self.outbox.push_back(welcome(...));
        self.send_initial_snapshot(sender);
    }
    ClientMessage::PlayerInput { tick, position, yaw, pitch } => {
        self.handle_player_input(sender, tick, position, yaw, pitch);
    }
    ClientMessage::Break { pos, request_id } => {
        match physics::validate_break(&self.world, sender, pos) {
            Ok(()) => {
                self.world.set_block(pos, BlockID::AIR);
                self.world.dirty_chunks.insert(pos.to_chunk_pos());
                self.outbox.push_back(broadcast(BlockUpdate { pos, block: BlockID::AIR }));
                self.outbox.push_back(reply_to(sender, ActionAck { request_id, accepted: true, reason: Ok }));
            }
            Err(reason) => {
                self.outbox.push_back(reply_to(sender, ActionAck { request_id, accepted: false, reason }));
            }
        }
    }
    ClientMessage::Place { pos, block, request_id } => { /* 同上 */ }
    ClientMessage::Chat { content } => {
        self.outbox.push_back(broadcast(Chat { from: sender, content }));
    }
    ClientMessage::Ping { client_time_ms } => {
        self.outbox.push_back(reply_to(sender, Pong { client_time_ms, server_time_ms: now_ms() }));
    }
}
```

---

## 九、初始快照同步

> **Phase 5** 引入（与 ChunkSnapshot 分片协议一起）。Phase 2 单人模式不走该路径：客户端通过 `Rc<RefCell<Server>>` 共享读 `server.world`，由 `ChunkLoader` 自身触发 `ensure_chunk_generated`。

新玩家 `Hello` → `Welcome` 之后，Server 把当前所有已加载 chunk 通过 `ChunkSnapshot` 分片发给该玩家（仅该玩家，`Recipient::One`）。

伪代码：
```rust
fn send_initial_snapshot(&mut self, recipient: EntityId) {
    let player = &self.world.players[&recipient];
    let center = player.position.to_chunk_pos();
    for dx in -RD..=RD {
        for dz in -RD..=RD {
            let pos = ChunkPos { x: center.x + dx, z: center.z + dz };
            let chunk = self.world.get_or_generate(pos);
            let payload = encode_chunk_payload(chunk);
            // 切片为 ≤ 14KB（留出 header 余量）
            for (i, frag) in payload.chunks(MAX_FRAG).enumerate() {
                self.outbox.push_back(reply_to(recipient, ChunkSnapshot {
                    pos, frag_index: i as u16, frag_total: total as u16, payload: frag.to_vec(),
                }));
            }
        }
    }
}
```

> 流量控制：分片之间通过 reliable channel 发送，浏览器自动 backpressure；如果担心拥塞可在 client 网络层做发送窗口（v2）。

---

## 十、与持久化的交互

> **Phase 5** 引入。Phase 2 / 3 / 4 不持久化（每次进入世界重生成；同 seed 确定性保证一致）。

`server` 通过 trait 抽象不感知具体存储：

```rust
pub trait ChunkStorage {
    fn save_chunks(&self, dirty: Vec<(ChunkPos, Chunk)>);
    fn load_chunk(&self, pos: ChunkPos) -> Option<Chunk>;
}
```

但**实际实现不在 server crate 内**（避免引入 `idb` / `web-sys` 依赖污染 server 的平台无关性）。
- Client 端实现 `IndexedDbStorage: ChunkStorage`
- Client 主循环每 N 秒调 `let dirty = server.take_dirty_chunks();` 然后 `spawn_local(async { storage.save(dirty).await })`
- 加载请求由 client 触发：`storage.load(pos)` → `await` → `server.load_chunk_from_storage(pos, chunk)`

详见 [`features/persistence.md`](../features/persistence.md)。

---

## 十一、单元测试要求

可在原生 target 直接 `cargo test -p voxweb-server` 运行：

**Phase 2**：
- `terrain::generate_chunk(seed=固定, pos=(0,0))` 输出稳定（基线 hash 比对）
- `World::ensure_chunk_generated` 幂等（二次调用不重生成）
- `World::get_block_world` chunk 内 / 未加载 / y 越界三种情况

**Phase 3+**：
- `physics::validate_break` 各拒绝路径覆盖
- `Server::handle_client_message` 状态机：Hello → Welcome → 多次 Break → 一次非法 Place → ActionAck 全部正确
- `tick()` 不会随时间无限增长内存（dirty 集合需被外部 drain）

---

## 十二、不在范围

- 怪物 / NPC / 战斗
- 流体扩散（水会自动流动）
- 红石 / 自动机械
- 时间循环（昼夜变化的服务端建模 — 客户端做即可）
- 区域权限 / 玩家管理（v2）
- 玩家死亡 / 掉血
- 外部数据库（Postgres 等） — 本期仅 IndexedDB
