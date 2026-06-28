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
- 持久化触发：维护 dirty 集合 + LRU 卸载策略，让 client 异步任务执行 OPFS 写入

**部署形态**：
- **Local-Only**：内嵌于 client，输入输出走内存通道
- **Host**：内嵌于 client，但消息源/汇是 P2P DataChannel + 自身本地通道（混合）
- **不存在独立进程形态**（浏览器内无独立 server bin）

---

### 当前组成

当前 server crate 覆盖完整权威世界循环：

- `World::ensure_chunk_generated` / `get_block_world` / `unload_chunk` 管理 chunk 生命周期
- `World::field_chunks` 同步维护 `FieldChunk` MaterialField 镜像；当前渲染/网络仍从 `chunks` 兼容读取
- `TerrainGenerator` 负责确定性 Perlin 高度图地形
- `physics::validate_break/place` 校验射程、AABB overlap 和方块合法性；`relax_after_edit` 对 `ImmediateRelaxation` 材质做局部下落/滑落
- `PlayerEntity` 表维护玩家位置、朝向、输入 tick 和显示名
- `Server` 通过 `handle_message` 消费 Local / Host / Remote 输入，向 outbox 写入点对点或广播消息
- `send_initial_snapshot` 与 `ChunkRequest` 负责 Remote 初始和运行时 chunk 同步
- `dirty_chunks`、LRU 和 pinned 集合为 OPFS 持久化提供最小变更集和内存上限

---

## 二、目录结构

```
crates/server/src/
├── lib.rs              World + Server 主结构 + 公开 API
├── world.rs            ChunkStore + EntityTable + Tick
├── terrain.rs          Perlin 地形生成
├── physics.rs          物理仲裁（位置合法性、挖放校验、局部颗粒松弛）
├── persistence.rs      PersistenceManager + LRU 卸载（具体存储读写在 client::storage）
└── handle_message_tests.rs  Server 消息处理单元测试
```

---

## 三、核心数据结构

### `World`

```rust
pub struct World {
    pub seed: u64,
    pub chunks: HashMap<ChunkPos, Chunk>,
    pub field_chunks: HashMap<ChunkPos, FieldChunk>, // MaterialField 兼容镜像
    pub terrain: TerrainGenerator,           // Perlin 高度图
    pub tick_count: u64,                     // tick 累加器；驱动玩家广播
    pub dirty_chunks: HashSet<ChunkPos>,     // 需要持久化的 chunk
    pub players: HashMap<EntityId, PlayerEntity>,
}

impl World {
    /// 若 chunk 未生成则调 terrain 生成并插入；已存在则跳过。
    pub fn ensure_chunk_generated(&mut self, pos: ChunkPos);

    /// 世界坐标查询；chunk 未加载或 y 越界一律返回 AIR。
    /// 用于 chunk_mesh::generate_with_neighbors 的回调。
    pub fn get_block_world(&self, wx: i32, wy: i32, wz: i32) -> BlockID;

    /// 卸载 chunk（移除 chunks 表）。调用方需要先处理 dirty flush / pinned 约束。
    pub fn unload_chunk(&mut self, pos: ChunkPos);

    /// 直接读写方块；`set_block` 会同步更新 chunks + field_chunks 并标记 dirty。
    /// chunk 未加载或 local_index 越界时静默忽略。
    pub fn get_block(&self, pos: Position) -> BlockID;
    pub fn set_block(&mut self, pos: Position, block: BlockID);

    /// 推进 tick 计数。
    pub fn tick(&mut self);
}

pub type EntityId = u32;

/// 玩家实体
pub struct PlayerEntity {
    pub entity_id: EntityId,
    pub display_name: String,
    pub position: glam::Vec3,
    pub yaw: f32,
    pub pitch: f32,
    pub last_input_tick: u32,                // 用于丢弃过期输入，并随 PlayerSnapshot 回执给客户端协调
    pub joined_at_tick: u32,
}
```

### `Server`

```rust
pub struct Server {
    pub world: World,
    pub tick: u32,
    pub seed: u64,
    pub players: HashMap<u32, Vec3>,         // entity_id → feet_position（挖放仲裁用）
    pub outbox: VecDeque<OutboundMessage>,   // 待广播的消息
}

// ServerConfig 和 physics 常量直接写在 physics.rs（MAX_REACH=6.0 等），
// 目前不引入 ServerConfig 结构体（Keep It Simple）。

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
    pub fn new(seed: u64) -> Self;

    /// 处理来自客户端的消息（Local 或 Remote）。
    /// Hello → 插入 players 表 + Welcome；PlayerInput → 更新 players 表；
    /// Break/Place → 调 validate → Ok 则 set_block + ActionAck + BlockUpdate；失败仅 ActionAck。
    pub fn handle_message(&mut self, sender: u32, msg: ClientMessage) -> Vec<ServerMessage>;

    /// 推进一个逻辑帧（60Hz 调用）。
    pub fn tick(&mut self);

    // —— 多人同步与 outbox ——
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
ChunkLoader / ChunkRequest ──▶ World::ensure_chunk_generated(pos)
                                  ├─ 已加载？跳过
                                  ├─ OPFS 有存档？由 client::storage 异步加载后回写
                                  └─ 未加载且无存档？terrain.generate_chunk(pos) → 插入

LRU / 视距卸载 ──▶ flush dirty（由 client 触发） ──▶ World::unload_chunk(pos)
```

**注意**：`server` 自身**不直接读 OPFS**（核心原则：server 无浏览器依赖）。持久化由 `client::storage` 异步任务完成，加载完成后调 `server.load_chunk_from_storage`。

### 玩家位置更新

Remote 加入房间后走玩家实体表；Local-Only 仍可通过同一套消息入口维护用于挖放仲裁的 feet position。

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

每个 `tick()` 末尾，收集玩家位置打包成 `PlayerTick` 广播。采用 **delta 模式**减少带宽：

```rust
fn broadcast_tick(&mut self) {
    // delta 过滤：仅包含位置变化 > 0.01m 或朝向变化 > 0.5° 的玩家
    // 每 30 tick（0.5s）强制全量广播一次，防止丢包导致远端冻结
    // 新玩家（last_broadcast_position.is_none()）始终包含
    let force_full = self.tick % 30 == 0;
    let players: Vec<PlayerSnapshot> = self.players.iter_mut()
        .filter(|(_, p)| force_full || p.has_changed_since_last_broadcast())
        .map(|(eid, p)| {
            p.record_broadcast();
            // PlayerSnapshot 携带 last_input_tick，让客户端用同一输入时刻做协调；
            // 高 RTT 下不能用 server tick 去对齐客户端预测历史。
            p.to_snapshot(*eid)
        })
        .collect();

    self.enqueue(Recipient::All, ServerMessage::PlayerTick {
        tick: self.tick,
        players,
        server_time_ms: self.current_time_ms,
    });
}
```

`PlayerEntity` 新增字段：`last_broadcast_position: Option<Vec3>`, `last_broadcast_yaw: Option<f32>`, `last_broadcast_pitch: Option<f32>`。

通过 unreliable channel 发送（详见 [`networking/protocol.md`](../networking/protocol.md) §六.1）。

---

## 五、`terrain.rs` — 地形生成

### 算法

1. `TerrainGenerator::new(seed)`：用 `noise::Perlin::new(seed as u32)` 构造一个噪声源
2. 对 chunk 内每个 `(lx, lz)` 列：
   - 世界坐标 `(world_x, world_z) = (pos.x * 16 + lx, pos.z * 16 + lz)`
   - 采样 `perlin.get([world_x * 0.01, world_z * 0.01])` → 值域 `[-1, 1]`
   - 映射到高度 `height = ((noise + 1) * 0.5 * CHUNK_Y * 0.4) as usize`（最高 ≈ 102）
3. 分层填充每个 `(lx, ly, lz)`：
   - `ly == 0` → 强制 BEDROCK（不可破坏世界边界，避免挖穿底部）
   - `ly + 3 < height` → STONE
   - `ly < height` → DIRT
   - `ly == height` → GRASS
   - `ly > height` → AIR

> 见 [`crates/server/src/terrain.rs`](../../crates/server/src/terrain.rs)。
>
> 注意：当前仅使用单一 Perlin 通道；多倍频叠加 / 山脉 / 平原差异化属于地形增强项。

### 扩展点
- 生物群系（草原 / 沙漠 / 雪地）
- 树木 / 矿物随机分布
- 水面（海平面以下填 WATER）
- 自定义地形 trait 让模块化

---

## 六、`physics.rs` — 物理仲裁

> 玩家本地物理预测在 `client::physics`；这里只做**仲裁**（防作弊最低限度）。

### 挖方块
```rust
pub const MAX_REACH: f32 = 6.0;    // 眼睛到方块中心的最大距离

pub fn validate_break(world: &World, pos: Position, player_feet: Vec3) -> AckReason {
    // 1) y 越界检查
    // 2) 眼-块中心距离 > MAX_REACH → OutOfRange
    // 3) y == 0 → BlockNotEmpty（世界底部边界，兼容旧存档 STONE 底层）
    // 4) 目标已是 AIR → BlockNotEmpty（语义复用）
    // 5) properties(block).breakable == false → BlockNotEmpty（如 BEDROCK）
    //    Ok
}
```

### 放方块
```rust
pub fn validate_place(world: &World, pos: Position, block: BlockID, player_feet: Vec3) -> AckReason {
    // 1) y 越界检查
    // 2) 距离 > MAX_REACH → OutOfRange
    // 3) y == 0 → BlockNotEmpty
    // 4) block == AIR 或 !properties(block).appears_in_hotbar → BlockNotEmpty
    // 5) 目标非空 → BlockNotEmpty
    // 6) Aabb::block_at(pos).intersects(&player_aabb(player_feet)) → Overlap
    //    Ok
}
```

共享 AABB 工具在 `voxweb_core::geometry`（`Aabb`、`player_aabb`、`Aabb::block_at`、`Aabb::intersects`），client 和 server 共用同一套定义。

玩家位置由 `Server::players: HashMap<u32, Vec3>` 提供（Hello 插入初始 spawn，PlayerInput 60Hz 更新）。`validate_break/place` 的签名接受显式 `player_feet` 参数（而非 entity_id），便于测试时直接注入位置。

### 局部颗粒松弛

```rust
pub fn relax_after_edit(world: &mut World, origin: Position) -> Vec<(Position, BlockID)> {
    // 1) 从 origin 及其上方/四邻域入队
    // 2) 只处理 properties(block).stability == ImmediateRelaxation 的材质
    // 3) 优先竖直下落；受阻后按确定性方向尝试斜下滑落
    // 4) 单次挖放受 RELAXATION_MOVE_BUDGET / RELAXATION_VISIT_BUDGET 限流
    // 5) 返回每个 cell 的权威 BlockUpdate 序列
}
```

当前这是 `BlockID` dense chunk 上的兼容原型，不做质量拆分和多材质混合；它保证 Host / Local-Only 立即给玩家软材质反馈，同时不新增网络消息类型。

---

## 七、`tick()` 流程

```rust
pub fn tick(&mut self) {
    self.world.current_tick += 1;

    // 当前实现：广播玩家位置；可扩展实体物理、方块更新（流体）等
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

`Server::handle_client_message` 的核心 dispatch：

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
    ClientMessage::ChunkRequest { center, render_distance, chunks } => {
        // 去重、限制单包数量，并确认请求中心没有明显远离该玩家服务端位置；
        // 每个 chunk 还要在 min(render_distance, host_render_distance) 内才会回传。
        self.handle_chunk_request(sender, center, render_distance, chunks);
    }
    ClientMessage::Break { pos, request_id, input_tick, player_position } => {
        let feet = self.action_player_position(sender, input_tick, player_position);
        match physics::validate_break(&self.world, pos, feet) {
            Ok(()) => {
                self.world.set_block(pos, BlockID::AIR);
                let relaxed = physics::relax_after_edit(&mut self.world, pos);
                self.outbox.push_back(broadcast(BlockUpdate { pos, block: BlockID::AIR }));
                for (pos, block) in relaxed {
                    self.outbox.push_back(broadcast(BlockUpdate { pos, block }));
                }
                self.outbox.push_back(reply_to(sender, ActionAck { request_id, accepted: true, reason: Ok }));
            }
            Err(reason) => {
                self.outbox.push_back(reply_to(sender, ActionAck { request_id, accepted: false, reason }));
            }
        }
    }
    ClientMessage::Place { pos, block, request_id, input_tick, player_position } => {
        let feet = self.action_player_position(sender, input_tick, player_position);
        /* 同上：按点击时 feet 校验范围与玩家 AABB 重叠；成功后 set_block + relax_after_edit + 多条 BlockUpdate */
    }
    ClientMessage::Chat { content } => {
        self.outbox.push_back(broadcast(Chat { from: sender, content }));
    }
    ClientMessage::Ping { client_time_ms } => {
        self.outbox.push_back(reply_to(sender, Pong { client_time_ms, server_time_ms: now_ms() }));
    }
}
```

---

## 九、初始与按需快照同步

Remote 通过 ChunkSnapshot 分片拿到初始世界；Local-Only / Host 的本地玩家则通过 `ChunkLoader` 直接触发 `server.world.ensure_chunk_generated`。

新玩家 `Hello` → `Welcome` 之后，Server 先把出生点附近 bootstrap 半径内的 chunk 通过 `ChunkSnapshot` 分片发给该玩家（仅该玩家，`Recipient::One`）。bootstrap 半径不会超过 Host 的视距。Remote 端随后按 `min(local_render_distance, host_render_distance)` 计算缺失区块，发送 `ChunkRequest` 补齐外圈；进入游戏后每次跨 chunk 边界继续按这个机制请求新区块。

伪代码：
```rust
fn send_initial_snapshot(&mut self, recipient: EntityId) {
    let center = DEFAULT_SPAWN.to_chunk_pos();
    for dx in -RD..=RD {
        for dz in -RD..=RD {
            let pos = ChunkPos { x: center.x + dx, z: center.z + dz };
            self.world.ensure_chunk_generated(pos);
            let chunk = self.world.chunks.get(&pos);
            // palette+RLE 压缩：典型地形 ~131KB → ~2-5KB
            let payload = voxweb_core::chunk::encode_chunk(&chunk.blocks)?;
            // 切片为 ≤ 14KB（大多数 chunk 压缩后不需分片，frag_total=1）
            for (i, frag) in payload.chunks(MAX_FRAG).enumerate() {
                self.enqueue(Recipient::One(recipient), ChunkSnapshot {
                    pos, frag_index: i as u16, frag_total: total as u16, payload: frag.to_vec(),
                }));
            }
        }
    }
}
```

按需请求伪代码：

```rust
fn handle_chunk_request(&mut self, recipient: EntityId, center: ChunkPos, render_distance: u32, chunks: Vec<ChunkPos>) {
    let player_center = chunk_pos_of_world(self.players[&recipient].position);
    if chebyshev_distance(center, player_center) > CHUNK_REQUEST_CENTER_GRACE {
        return;
    }
    let allowed_radius = min(render_distance, self.host_render_distance);
    for pos in unique(chunks).take(CHUNK_REQUEST_MAX_BATCH) {
        if chebyshev_distance(pos, center) > allowed_radius {
            continue;
        }
        self.send_chunk_snapshot(recipient, pos);
    }
}
```

> 流量控制：reliable DC 设 `bufferedAmountLowThreshold=256KB`；发送后检查 `bufferedAmount`，超过 1MB 暂停，`onbufferedamountlow` 恢复。阻塞的消息自动重入队下帧发送（`host_route_outbox` 返回 unsent → `reenqueue_outbox`）。

---

## 十、与持久化的交互

`server` 通过抽象接口不感知具体存储，仅维护"哪些 chunk 脏了"以及"何时把它们交给 client 写"。具体接口在 [`crates/server/src/persistence.rs`](../../crates/server/src/persistence.rs)：

```rust
pub struct PersistenceManager {
    dirty: HashSet<ChunkPos>,
    in_flight: HashSet<ChunkPos>,
    failure_backoff_until: HashMap<ChunkPos, f64>,
    next_flush_tick: u64,
}

impl PersistenceManager {
    pub fn mark_dirty(&mut self, pos: ChunkPos);
    pub fn snapshot_dirty(&mut self, limit: usize, current_tick: u64) -> Vec<ChunkPos>;
    pub fn commit_flushed(&mut self, positions: &[ChunkPos]);           // 写入成功后清理 in_flight
    pub fn record_flush_failure(&mut self, positions: &[ChunkPos], current_tick: u64);
    pub fn should_flush(&self, current_tick: u64) -> bool;
}
```

**实际存储实现不在 server crate 内**（避免引入 `web-sys` 依赖污染 server 的平台无关性）：
- Client 端 [`crates/client/src/storage.rs`](../../crates/client/src/storage.rs) 实现 `WorldStorage` trait（默认 `OpfsStorage`）
- Client 主循环每 3 秒：`let to_flush = persistence.snapshot_dirty(4, tick);` → 主线程 encode（每批最多 4 chunk）→ `spawn_local(async { storage.save_chunks(...).await })` → 成功 commit / 失败 record_failure
- 加载请求由 client 触发：`storage.load_chunk(pos)` → `decode` → `server.load_chunk_from_storage(pos, chunk)`

`World` 还配套 LRU + pinned 集合避免内存爆炸（capacity 4096，渲染距离内 chunk 不可驱逐；dirty chunk 驱逐前必须先 flush）。

详见 [`features/persistence.md`](../features/persistence.md)。

---

## 十一、单元测试要求

可在原生 target 直接 `cargo test -p voxweb-server` 运行：

**Chunk / terrain**：
- `terrain::generate_chunk(seed=固定, pos=(0,0))` 输出稳定（基线 hash 比对）
- `World::ensure_chunk_generated` 幂等（二次调用不重生成）
- `World::get_block_world` chunk 内 / 未加载 / y 越界三种情况

**Physics / message dispatch**：
- `physics::validate_break` 各拒绝路径：y 越界 / 射程 > 6m / 底层边界 / AIR / 不可破坏材质 → BlockNotEmpty
- `physics::validate_place`：y 越界 / 射程 / 底层边界 / 非热栏材质 / BlockNotEmpty / AABB Overlap
- `physics::relax_after_edit`：沙/土/草竖直下落、破坏支撑后补落、受阻后斜下滑落、单次预算限制
- `Server::handle_message` 集成：Hello 落表 / PlayerInput 更新 / Break 成功广播 / Place 重叠拒绝
- 全部 10 个单元测试通过 `cargo test -p voxweb-server --lib`

---

## 十二、不在范围

- 怪物 / NPC / 战斗
- 流体扩散（水会自动流动）
- 红石 / 自动机械
- 时间循环（昼夜变化的服务端建模 — 客户端做即可）
- 区域权限 / 玩家管理（v2）
- 玩家死亡 / 掉血
- 外部数据库（Postgres 等） — 当前仅使用浏览器 OPFS
