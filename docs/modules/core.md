# `core` 模块设计

> **何时阅读**：改动数据结构（BlockID/Chunk/Position）；新增网络消息类型；调 bincode 配置时
> **关联文档**：[`README.md`](../../README.md) · [`architecture.md`](../architecture.md) · [`networking/protocol.md`](../networking/protocol.md) · [`features/meshing.md`](../features/meshing.md)

---

## 一、职责与原则

`core` 是项目的**协议与数据层**，所有 crate 都依赖它，但它不依赖任何 crate。

**核心原则**：
1. **零浏览器依赖**：禁止引用 `web-sys` / `js-sys` / `wasm-bindgen`
2. **零图形依赖**：禁止引用 `wgpu` / `winit`
3. **可序列化优先**：所有公开数据类型都实现 `Serialize + Deserialize`
4. **极致紧凑**：网络传输频繁，结构体内存布局尽量小

**允许的依赖**：`serde`、`bincode`、`glam`（仅用于位置/向量类型）、`bitflags`（如有需要）

---

## 二、目录结构

```
crates/core/
├── Cargo.toml
└── src/
    ├── lib.rs          模块声明 + 公开 re-export
    ├── block.rs        BlockID + 硬编码方块属性表
    ├── chunk.rs        Chunk + Position + ChunkPos
    └── protocol.rs     ClientMessage / ServerMessage / RoomEvent
```

---

## 三、`block.rs` — 方块定义

### BlockID

```rust
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct BlockID(pub u16);

impl BlockID {
    pub const AIR: BlockID = BlockID(0);
    pub const STONE: BlockID = BlockID(1);
    pub const GRASS: BlockID = BlockID(2);
    pub const DIRT: BlockID = BlockID(3);
    pub const WATER: BlockID = BlockID(4);     // 半透明
    pub const GLASS: BlockID = BlockID(5);     // 半透明
    pub const SAND: BlockID = BlockID(6);
    pub const WOOD: BlockID = BlockID(7);
    pub const LEAVES: BlockID = BlockID(8);
    // ... 后续按需扩展
}
```

### BlockProperties

每种 BlockID 关联静态属性表，用于查询：

```rust
pub struct BlockProperties {
    pub solid: bool,           // 是否参与碰撞（用于物理）
    pub transparent: bool,     // 是否走 Transparent Pass
    pub emits_light: bool,     // （v2）光源
    pub texture_index: u8,     // 纹理图集索引（顶点压缩用）
    pub display_name: &'static str,
}

pub fn properties(id: BlockID) -> &'static BlockProperties { ... }
```

实现方式：编译期 `const` 数组按 `BlockID.0` 索引。新增方块时在数组追加一行。

> **不做**：JSON 数据驱动方块定义（见 `README.md` Out-of-Scope）。当前直接 Rust 硬编码够用，且性能/体积更好。

---

## 四、`chunk.rs` — 区块与坐标

### 常量

```rust
pub const CHUNK_X: usize = 16;
pub const CHUNK_Y: usize = 256;
pub const CHUNK_Z: usize = 16;
pub const CHUNK_SIZE: usize = CHUNK_X * CHUNK_Y * CHUNK_Z; // 65536
```

### Position

世界坐标（绝对方块坐标）：

```rust
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct Position { pub x: i32, pub y: i32, pub z: i32 }

impl Position {
    pub fn to_chunk_pos(self) -> ChunkPos { ... }    // 向下取整除法
    pub fn local_index(self) -> Option<usize> { ... } // 区块内 0..65536，越界返 None
}
```

### ChunkPos

区块在世界中的二维坐标（Y 不分块，Chunk 高度 256 一柱到顶）：

```rust
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ChunkPos { pub x: i32, pub z: i32 }
```

### Chunk

```rust
#[derive(Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub blocks: Vec<BlockID>, // 长度恒为 CHUNK_SIZE
    // 可能扩展：dirty 标记、最后修改 tick 等元数据（不放此结构，放 server::World）
}

impl Chunk {
    pub fn empty() -> Self { ... }                           // 全 AIR
    pub fn get(&self, lx: usize, ly: usize, lz: usize) -> BlockID { ... }
    pub fn set(&mut self, lx: usize, ly: usize, lz: usize, id: BlockID) { ... }

    // 索引规则：(y << 8) | (z << 4) | x 
    #[inline]
    pub fn index(lx: usize, ly: usize, lz: usize) -> usize {
        (ly << 8) | (lz << 4) | lx
    }
}
```

**索引选择理由**：`y` 在最高位 → 同层方块在内存上连续，利于水平遍历（贪婪网格化按层扫描时缓存命中）。

### Chunk 网络压缩（palette + RLE）

`encode_chunk` / `decode_chunk` 用于 ChunkSnapshot 网络传输（详见 [`networking/protocol.md` §五](../networking/protocol.md#五chunk-快照同步)）。

```rust
/// 将 blocks 数组编码为 palette+RLE 压缩字节。
/// 算法：扫描连续相同 BlockID 合并为 run；记录 (palette_index, run_length)。
pub fn encode_chunk(blocks: &[BlockID]) -> Result<Vec<u8>, String>;

/// 将 encode_chunk 的压缩字节还原为 Vec<BlockID>（长度恒为 CHUNK_SIZE）。
pub fn decode_chunk(bytes: &[u8]) -> Result<Vec<BlockID>, String>;
```

内部格式：`CompressedChunk { palette: Vec<BlockID>, runs: Vec<(u16, u32)> }` → bincode 序列化。

典型地形（草/泥/石/空气 4 种方块）从 ~131 KB 压缩到 2-5 KB（**30-60x**）。

### 边界检查

`get`/`set` 必须做断言或 `Result` 返回，禁止越界。建议：内部函数用断言（debug 期捕捉），公开 API 用 `Option`/`Result`。

---

## 五、`protocol.rs` — 网络消息

> 完整消息字段表见 [`networking/protocol.md`](../networking/protocol.md)。本文只列结构骨架。

### 三个顶层枚举

```rust
/// Client → Server 的消息（远程客户端 → Host；或本地 Client → 内嵌 Server）
#[derive(Clone, Serialize, Deserialize)]
pub enum ClientMessage {
    /// 加入房间初始握手
    Hello { display_name: String, version: u32 },
    /// 玩家移动同步（高频，走 unreliable 通道）
    PlayerInput { tick: u32, position: glam::Vec3, yaw: f32, pitch: f32 },
    /// 方块挖掘
    Break { pos: Position, request_id: u32 },
    /// 方块放置
    Place { pos: Position, block: BlockID, request_id: u32 },
    /// 文本聊天
    Chat { content: String },
    /// 心跳（防止 NAT 老化；可选）
    Ping { client_time_ms: u64 },
}

/// Server → Client 的消息
#[derive(Clone, Serialize, Deserialize)]
pub enum ServerMessage {
    /// 加入握手响应（含玩家 entity_id 与 server tick）
    Welcome { entity_id: u32, server_tick: u32, world_seed: u64 },
    /// 全量 Chunk 数据（分片）
    ChunkSnapshot { pos: ChunkPos, frag_index: u16, frag_total: u16, payload: Vec<u8> },
    /// 单方块更新
    BlockUpdate { pos: Position, block: BlockID },
    /// 挖放请求结果（携 request_id 用于客户端协调）
    ActionAck { request_id: u32, accepted: bool, reason: AckReason },
    /// 远端玩家位置广播（高频，unreliable）
    PlayerTick { tick: u32, players: Vec<PlayerSnapshot>, server_time_ms: u64 },
    /// 玩家加入/离开
    PeerJoined { entity_id: u32, display_name: String },
    PeerLeft { entity_id: u32 },
    /// 文本聊天广播
    Chat { from: u32, content: String },
    /// 心跳响应
    Pong { client_time_ms: u64, server_time_ms: u64 },
}

/// 房间会话事件（信令层产生，给 client 用）
#[derive(Clone, Serialize, Deserialize)]
pub enum RoomEvent {
    Connected,
    Disconnected { reason: String },
    PeerCount(u32),
    SignalingError(String),
    /// Host 端：某 Remote 离开（见 docs/networking/signaling.md 房间生命周期）
    RemoteLeft { peer_id: u32 },
    /// 该 peer 已升级为信令 Worker 字节中继（见 docs/networking/signaling.md §九）
    /// 仅 UI 提示用，不改变协议路径
    PeerRelayed { peer_id: u32 },
}

/// 玩家快照（PlayerTick 携带）
#[derive(Clone, Serialize, Deserialize)]
pub struct PlayerSnapshot {
    pub entity_id: u32,
    pub position: glam::Vec3,
    pub yaw: f32,
    pub pitch: f32,
}

#[derive(Clone, Serialize, Deserialize)]
pub enum AckReason { Ok, OutOfRange, BlockNotEmpty, Overlap, Cooldown }
```

### 序列化配置

```rust
pub fn bincode_config() -> bincode::config::Configuration {
    bincode::config::standard()
        .with_little_endian()      // 跨平台一致
        .with_variable_int_encoding()
}

pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, bincode::error::EncodeError> {
    bincode::serde::encode_to_vec(msg, bincode_config())
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, bincode::error::DecodeError> {
    let (msg, _len) = bincode::serde::decode_from_slice(bytes, bincode_config())?;
    Ok(msg)
}
```

**字节序**：固定 little-endian，避免不同浏览器字节序差异（理论上 WASM 运行环境都是 LE，但显式声明更安全）。

**varint**：`PlayerInput.tick` / `entity_id` 等小整数走 varint，节省 60Hz 广播带宽。

### 消息分片

`ChunkSnapshot` 单条可能大于 16KB（DataChannel MTU），通过 `frag_index` / `frag_total` 显式分片，由接收端组装。详细流程见 [`networking/protocol.md`](../networking/protocol.md#chunk-快照同步)。

---

## 六、`lib.rs` — 公开 API

```rust
pub mod block;
pub mod chunk;
pub mod geometry;
pub mod protocol;

pub use block::{BlockID, BlockProperties, properties};
pub use chunk::{CHUNK_SIZE, CHUNK_X, CHUNK_Y, CHUNK_Z, Chunk, ChunkPos, Position};
pub use geometry::{Aabb, PLAYER_EYE_OFFSET, PLAYER_HEIGHT, PLAYER_WIDTH, player_aabb};
pub use protocol::{AckReason, ClientMessage, PlayerSnapshot, RoomEvent, ServerMessage, encode};
```

---

## 七、版本化与兼容性

- **协议版本号**：`Hello.version: u32` 在加入时携带；Host 拒绝不兼容版本（直接断开）
- **新增字段**：`bincode` 不像 protobuf 自带兼容性，**禁止给已有结构体加字段**；改协议必须递增版本号并双方升级
- **新增消息变体**：枚举追加变体兼容（旧客户端反序列化失败时记录错误并断开）；推荐每次大改协议时增 `version`
- 文档同步：每次改 `protocol.rs` 必须同步更新 [`networking/protocol.md`](../networking/protocol.md) 的消息表

---

## 八、测试要求

- `chunk::index` 必须有单元测试覆盖边界（lx/ly/lz 的 0 与最大值）
- `Position::to_chunk_pos` 必须测试负坐标向下取整（如 `x=-1 → chunk_x=-1`）
- `encode`/`decode` 往返测试每个 `ClientMessage` / `ServerMessage` 变体
- `core` 单元测试可在原生 target 跑：`cargo test -p voxweb-core`，无需 wasm 环境

---

## 九、不在范围

- 玩家库存 / 物品系统（v2）
- 实体（怪物/动物） — 当前只有 Player 一种实体
- 命令（`/teleport` 等） — 仅文本聊天，无指令解析
- 地图传送门 / 多维世界
