# Phase 2 · 体素单人 · 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Phase 2 体素单人模式：大厅入口 → 单机模式 → 看到 Perlin 地形持续流式加载，60fps 稳定，跨区块边界无漏面。

**Architecture:** 客户端 `App` 状态机（Lobby / InGame）持有 `Rc<RefCell<Server>>` + `NetEndpoint::Local`（futures mpsc 双向通道）。`ChunkLoader` 滚动加载玩家附近区块；`MeshJobQueue`（4 档优先级 + 4 deque + 4ms 分帧 budget）执行 `generate_with_neighbors` 跨区块剔除网格化；mesh 回调通过共享 `server.world.get_block_world` 直接读邻居数据，不复制 chunk。

**Tech Stack:** Rust + wasm32-unknown-unknown · wgpu 29 · egui 0.34 · noise 0.9（Perlin）· futures-channel 0.3（mpsc）· trunk 构建

**Reference spec:** [`docs/superpowers/specs/2026-05-11-phase2-design.md`](../specs/2026-05-11-phase2-design.md)

---

## 文件结构

**新建**：
- `crates/client/src/mesh_jobs.rs` — `MeshPriority` 枚举 + `MeshJobQueue`（4 deque + pending HashSet + budget runner）
- `crates/client/src/chunk_loader.rs` — `ChunkLoader`（滚动加载 / 卸载 + 邻居重网格化触发）

**修改**：
- `crates/server/src/world.rs` — World 持有 `TerrainGenerator`；新增 `ensure_chunk_generated` / `get_block_world` / `unload_chunk`
- `crates/server/src/lib.rs` — `Server::handle_message` 新增 Hello→Welcome 分支
- `crates/render/src/chunk_mesh.rs` — 新增 `generate_with_neighbors(chunk, pos, get_block_world)`
- `crates/render/src/lib.rs` — 新增 `Renderer::drop_chunk_mesh` / `has_chunk_mesh`
- `crates/net/src/lib.rs` — `NetEndpoint::Local` 改持 mpsc senders/receivers；新增 `new_local_pair`、`send_client_message`、`try_recv_server_message`、`ServerInbox`
- `crates/client/src/app.rs` — 引入 `Game` 子结构 + `GameSettings` + `FrameClock`
- `crates/client/src/ui/lobby.rs` — 实装大厅 UI + `LobbyAction`
- `crates/client/src/lib.rs` — `App` 替代 `Runtime`；`render_frame` 按 `state` 分流；启动直入 Lobby
- `crates/client/Cargo.toml` — 加 `voxweb-net` 直接依赖（已有，确认）
- `crates/server/src/persistence.rs` — 无改动（Phase 5 实装）

---

## Task 1：server `World` 地形与生命周期接口

**Files:**
- Modify: `crates/server/src/world.rs`
- Modify: `crates/server/src/lib.rs:23-29`（`Server::new`）
- Test: `crates/server/src/world.rs`（内嵌 `#[cfg(test)] mod tests`）

- [ ] **Step 1: 写失败测试（先在 world.rs 末尾追加 test 模块）**

在 `crates/server/src/world.rs` 末尾追加：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use voxweb_core::block::BlockID;
    use voxweb_core::chunk::Position;

    #[test]
    fn ensure_chunk_generated_is_idempotent_and_uses_terrain() {
        let mut world = World::new(12345);
        let pos = ChunkPos::new(0, 0);

        // 首次生成
        world.ensure_chunk_generated(pos);
        assert!(world.chunks.contains_key(&pos));
        let snapshot: Vec<BlockID> = world.chunks[&pos].blocks.clone();
        // Perlin 地形：至少应有非 AIR 方块（基岩 + 一层地形）
        assert!(snapshot.iter().any(|b| *b != BlockID::AIR),
            "生成的 chunk 应至少有一个非空方块");

        // 第二次调用：不应覆盖（同 blocks）
        world.ensure_chunk_generated(pos);
        assert_eq!(world.chunks[&pos].blocks, snapshot);
    }

    #[test]
    fn get_block_world_returns_air_for_unloaded_or_out_of_bounds() {
        let world = World::new(42);
        // 未加载 chunk
        assert_eq!(world.get_block_world(0, 64, 0), BlockID::AIR);
        // y 越界
        assert_eq!(world.get_block_world(0, -1, 0), BlockID::AIR);
        assert_eq!(world.get_block_world(0, 256, 0), BlockID::AIR);
    }

    #[test]
    fn get_block_world_reads_loaded_chunk() {
        let mut world = World::new(7);
        world.ensure_chunk_generated(ChunkPos::new(0, 0));
        // 与 chunk.get 等价
        let direct = world.chunks[&ChunkPos::new(0, 0)].get(3, 0, 5);
        let via_world = world.get_block_world(3, 0, 5);
        assert_eq!(direct, via_world);
    }

    #[test]
    fn unload_chunk_removes_chunk_entry() {
        let mut world = World::new(1);
        let pos = ChunkPos::new(2, -3);
        world.ensure_chunk_generated(pos);
        assert!(world.chunks.contains_key(&pos));
        world.unload_chunk(pos);
        assert!(!world.chunks.contains_key(&pos));
    }

    #[test]
    fn set_block_uses_position_local_index() {
        let mut world = World::new(0);
        world.ensure_chunk_generated(ChunkPos::new(0, 0));
        world.set_block(Position::new(5, 100, 7), BlockID::STONE);
        assert_eq!(world.get_block(Position::new(5, 100, 7)), BlockID::STONE);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p voxweb-server --lib`
Expected: 编译失败 — `ensure_chunk_generated` / `get_block_world` / `unload_chunk` 未定义。

- [ ] **Step 3: 重写 `world.rs` 主体**

把 `crates/server/src/world.rs` 整个文件替换为：

```rust
//! 世界状态管理：Chunk 表、地形生成器、方块读写。
//!
//! Phase 2：World 持有 TerrainGenerator；区块按需生成 + 卸载；
//! 提供世界坐标查询接口供网格化跨区块剔除使用。

use std::collections::HashMap;

use voxweb_core::block::BlockID;
use voxweb_core::chunk::{CHUNK_X, CHUNK_Y, CHUNK_Z, Chunk, ChunkPos, Position};

use crate::terrain::TerrainGenerator;

/// 世界状态。Phase 2 仅含 chunk 表 + 地形生成器；玩家表与 dirty 集合留后续 Phase。
pub struct World {
    pub seed: u64,
    pub chunks: HashMap<ChunkPos, Chunk>,
    pub terrain: TerrainGenerator,
    /// 自创建以来的总 tick 数（Phase 2 仅累加，不驱动逻辑）
    pub tick_count: u64,
}

impl World {
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            chunks: HashMap::new(),
            terrain: TerrainGenerator::new(seed),
            tick_count: 0,
        }
    }

    /// 若 chunk 未生成则调地形生成器生成并插入。已存在则跳过。
    /// Phase 2 的 chunk 入口点（由 client::chunk_loader 调用）。
    pub fn ensure_chunk_generated(&mut self, pos: ChunkPos) {
        if self.chunks.contains_key(&pos) {
            return;
        }
        let chunk = self.terrain.generate_chunk(pos);
        self.chunks.insert(pos, chunk);
    }

    /// 卸载（移除）一个 chunk。Phase 5 引入持久化后会先把 dirty 数据 flush 再移除。
    pub fn unload_chunk(&mut self, pos: ChunkPos) {
        self.chunks.remove(&pos);
    }

    /// 世界坐标方块查询；chunk 未加载或 y 越界一律返回 AIR。
    /// 供 chunk_mesh::generate_with_neighbors 的回调使用。
    pub fn get_block_world(&self, wx: i32, wy: i32, wz: i32) -> BlockID {
        if wy < 0 || wy >= CHUNK_Y as i32 {
            return BlockID::AIR;
        }
        let cp = Position::new(wx, wy, wz).to_chunk_pos();
        let Some(chunk) = self.chunks.get(&cp) else {
            return BlockID::AIR;
        };
        // local 坐标计算（rem_euclid 保证负坐标正确折算）
        let lx = wx.rem_euclid(CHUNK_X as i32) as usize;
        let lz = wz.rem_euclid(CHUNK_Z as i32) as usize;
        let ly = wy as usize;
        chunk.get(lx, ly, lz)
    }

    /// 在世界坐标处放置一个方块（若 chunk 未加载则忽略）。
    /// Phase 2 仅供测试与未来挖放使用；本期主循环不调用。
    pub fn set_block(&mut self, pos: Position, block: BlockID) {
        let cp = pos.to_chunk_pos();
        let Some(chunk) = self.chunks.get_mut(&cp) else {
            return;
        };
        if let Some(idx) = pos.local_index() {
            chunk.blocks[idx] = block;
        }
    }

    /// 读取世界坐标处的方块。chunk 未加载返回 AIR（与 get_block_world 等价的 Position 接口）。
    pub fn get_block(&self, pos: Position) -> BlockID {
        self.get_block_world(pos.x, pos.y, pos.z)
    }

    /// 推进 tick 计数（Phase 5 起会驱动玩家广播等）。
    pub fn tick(&mut self) {
        self.tick_count += 1;
    }
}
```

- [ ] **Step 4: 同步 `Server::new` 调用点**

`crates/server/src/lib.rs:23-29` 当前：
```rust
pub fn new(seed: u64) -> Self {
    Self {
        world: world::World::new(seed),
        tick: 0,
        seed,
    }
}
```
**不需要改**（`World::new` 签名未变）。仅确认编译通过。

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test -p voxweb-server --lib`
Expected: 5 个新测试 + 既有测试全部 PASS。

- [ ] **Step 6: clippy + fmt**

Run: `cargo clippy -p voxweb-server --target wasm32-unknown-unknown -- -D warnings`
Expected: 无错误。

Run: `cargo fmt --all -- --check`
Expected: 无 diff。

- [ ] **Step 7: 提交**

```bash
git add crates/server/src/world.rs crates/server/src/lib.rs
git commit -m "feat(server): World ensure_chunk_generated/get_block_world/unload_chunk for Phase 2"
```

---

## Task 2：render `generate_with_neighbors` 跨区块剔除

**Files:**
- Modify: `crates/render/src/chunk_mesh.rs`

- [ ] **Step 1: 在 chunk_mesh.rs 末尾追加失败测试**

在 `crates/render/src/chunk_mesh.rs` 的 `#[cfg(test)] mod tests` 内追加：

```rust
    #[test]
    fn neighbor_callback_skips_face_when_neighbor_is_solid() {
        // 单方块在 lx=15, ly=64, lz=5；邻居 chunk (1,0) 的 lx=0, ly=64, lz=5 是 STONE
        // → PosX 面应被跨区块剔除（与朴素版相比顶点 -6）
        let mut chunk = Chunk::empty();
        chunk.set(15, 64, 5, BlockID::STONE);

        // 朴素版（区块外视空气）：6 面 × 6 顶点 = 36
        let naive = generate_opaque_mesh(&chunk);
        assert_eq!(naive.vertex_count(), 36);

        // with_neighbors：邻居在 (16, 64, 5) 是 STONE
        let neighbor_x = 16;
        let with_n = generate_with_neighbors(
            &chunk,
            voxweb_core::ChunkPos::new(0, 0),
            &|wx, _wy, _wz| {
                if wx == neighbor_x {
                    BlockID::STONE
                } else {
                    BlockID::AIR
                }
            },
        );
        // 5 面（PosX 被剔除）× 6 顶点 = 30
        assert_eq!(with_n.vertex_count(), 30);
    }

    #[test]
    fn neighbor_callback_air_equivalent_to_naive() {
        // 全 AIR 回调（区块外一律空气）应等同于 generate_opaque_mesh
        let mut chunk = Chunk::empty();
        chunk.set(0, 64, 0, BlockID::STONE);
        chunk.set(15, 64, 15, BlockID::DIRT);

        let naive = generate_opaque_mesh(&chunk);
        let with_n = generate_with_neighbors(
            &chunk,
            voxweb_core::ChunkPos::new(0, 0),
            &|_, _, _| BlockID::AIR,
        );
        assert_eq!(naive.vertex_count(), with_n.vertex_count());
    }

    #[test]
    fn neighbor_callback_handles_y_boundary() {
        // 顶层方块（ly=255），不存在更高层；回调对 y=256 返回 AIR → PosY 面应发射
        let mut chunk = Chunk::empty();
        chunk.set(5, 255, 5, BlockID::STONE);
        let with_n = generate_with_neighbors(
            &chunk,
            voxweb_core::ChunkPos::new(0, 0),
            &|_, _, _| BlockID::AIR,
        );
        // 单方块 6 面（无邻居遮挡）
        assert_eq!(with_n.vertex_count(), 36);
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p voxweb-render --lib`
Expected: 编译失败 — `generate_with_neighbors` 未定义。

- [ ] **Step 3: 在 chunk_mesh.rs 添加 `generate_with_neighbors`**

在 `crates/render/src/chunk_mesh.rs` 的 `generate_opaque_mesh` 函数之后插入：

```rust
/// 跨区块面剔除版网格化。
///
/// 与 `generate_opaque_mesh` 行为相同，但所有面可见性查询通过 `get_block_world`
/// 回调进行。同 chunk 内的查询也走回调（统一接口）；区块外由调用方决定（一般返回邻居
/// chunk 已加载的真实方块，或 AIR）。
///
/// 这是 Phase 2 视觉正确性的核心：避免 chunk 边界处误把邻居 chunk 的实心方块视为空气
/// 而多绘制一层"墙皮"。
pub fn generate_with_neighbors(
    chunk: &Chunk,
    chunk_pos: voxweb_core::ChunkPos,
    get_block_world: &dyn Fn(i32, i32, i32) -> BlockID,
) -> ChunkMeshCpu {
    let mut vertices: Vec<PackedVertex> = Vec::with_capacity(4096);

    let origin_x = chunk_pos.x * CHUNK_X as i32;
    let origin_z = chunk_pos.z * CHUNK_Z as i32;

    for ly in 0..CHUNK_Y {
        for lz in 0..CHUNK_Z {
            for lx in 0..CHUNK_X {
                let block = chunk.get(lx, ly, lz);
                if block == BlockID::AIR {
                    continue;
                }
                let props = properties(block);
                if props.transparent {
                    continue;
                }
                let tex = props.texture_index;

                for fi in 0..6 {
                    let (dx, dy, dz) = FACE_NEIGHBORS[fi];
                    let wx = origin_x + lx as i32 + dx;
                    let wy = ly as i32 + dy;
                    let wz = origin_z + lz as i32 + dz;
                    let neighbor = get_block_world(wx, wy, wz);
                    let visible = neighbor == BlockID::AIR || properties(neighbor).transparent;
                    if visible {
                        emit_face(&mut vertices, lx as u8, ly as u16, lz as u8, fi, tex);
                    }
                }
            }
        }
    }

    ChunkMeshCpu { vertices }
}
```

同时确认文件顶部 `use voxweb_core::...` 已导入 `CHUNK_X / CHUNK_Y / CHUNK_Z`（Phase 1 已导入）。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p voxweb-render --lib`
Expected: 3 个新测试 + 既有测试全部 PASS。

- [ ] **Step 5: clippy + fmt**

Run: `cargo clippy -p voxweb-render --target wasm32-unknown-unknown -- -D warnings`
Run: `cargo fmt --all -- --check`
Expected: 无错误 / 无 diff。

- [ ] **Step 6: 提交**

```bash
git add crates/render/src/chunk_mesh.rs
git commit -m "feat(render): generate_with_neighbors for cross-chunk face culling"
```

---

## Task 3：render `Renderer::drop_chunk_mesh` / `has_chunk_mesh`

**Files:**
- Modify: `crates/render/src/lib.rs`

- [ ] **Step 1: 读取 Renderer 当前 chunk_mesh 持有字段**

Run: `cargo check -p voxweb-render --target wasm32-unknown-unknown`（确认基线编译通过）

打开 `crates/render/src/lib.rs`，定位 `Renderer` 结构体里持有 `HashMap<ChunkPos, ChunkMeshGpu>` 的字段（Phase 1 已存在；多半叫 `chunk_meshes` 或 `meshes`）。记下确切字段名以备 Step 3 使用。

- [ ] **Step 2: 在 lib.rs 末尾追加单元测试（仅在 non-wasm target 验证 API 存在性，逻辑测试 wasm 才有效）**

跳过单元测试——render crate 的 wgpu 资源只能在浏览器跑，本任务仅做 API 增量。Step 3 直接写实现，Step 4 用编译检查替代。

- [ ] **Step 3: 在 `Renderer` impl 块中追加方法**

在 `crates/render/src/lib.rs` 的 `impl Renderer { ... }` 块内追加（假设字段名为 `chunk_meshes`，按 Step 1 实际名替换）：

```rust
    /// 删除指定 chunk 的 GPU 资源（buffer 通过 Drop 释放）。
    /// 用于 Phase 2 ChunkLoader 卸载远离玩家的 chunk。
    pub fn drop_chunk_mesh(&mut self, pos: voxweb_core::ChunkPos) {
        self.chunk_meshes.remove(&pos);
    }

    /// 查询某个 chunk 是否已有 GPU mesh。
    /// 用于 Phase 2 ChunkLoader 决定是否需要为邻居重网格化。
    pub fn has_chunk_mesh(&self, pos: voxweb_core::ChunkPos) -> bool {
        self.chunk_meshes.contains_key(&pos)
    }
```

> 若 Step 1 确认的字段名不是 `chunk_meshes`，把上述两处 `self.chunk_meshes` 改为实际名。

- [ ] **Step 4: 编译确认**

Run: `cargo check -p voxweb-render --target wasm32-unknown-unknown`
Expected: 编译通过。

Run: `cargo clippy -p voxweb-render --target wasm32-unknown-unknown -- -D warnings`
Run: `cargo fmt --all -- --check`
Expected: 无错误 / 无 diff。

- [ ] **Step 5: 提交**

```bash
git add crates/render/src/lib.rs
git commit -m "feat(render): Renderer drop_chunk_mesh + has_chunk_mesh for Phase 2 dynamic loading"
```

---

## Task 4：net `NetEndpoint::Local` mpsc 双向通道

**Files:**
- Modify: `crates/net/src/lib.rs`

- [ ] **Step 1: 整体重写 `crates/net/src/lib.rs`**

完整替换：

```rust
//! VoxWeb P2P 网络层。
//!
//! Phase 2：仅 `NetEndpoint::Local` 落地（基于 futures mpsc 双向通道）。
//! Phase 4 起补 Host / Remote 分支（WebRTC）。

pub mod peer;
pub mod room;
pub mod signaling;
pub mod transport;

use futures_channel::mpsc::{self, UnboundedReceiver, UnboundedSender};

use voxweb_core::protocol::{ClientMessage, ServerMessage};

/// 网络端点。Phase 2 仅实装 Local 分支。
pub enum NetEndpoint {
    /// 单机模式：通过 mpsc 与同进程的 Server 通信。
    Local {
        tx_client: UnboundedSender<ClientMessage>,
        rx_server: UnboundedReceiver<ServerMessage>,
    },
    /// 房主：Phase 4 引入。
    Host,
    /// 远端客户端：Phase 4 引入。
    Remote,
}

/// Server 侧持有的对偶端，与 `NetEndpoint::Local` 配对。
pub struct ServerInbox {
    pub rx_client: UnboundedReceiver<ClientMessage>,
    pub tx_server: UnboundedSender<ServerMessage>,
}

impl NetEndpoint {
    /// 创建 Local 端点 + 对偶 ServerInbox。
    /// Client 持 NetEndpoint，Server driver 持 ServerInbox。
    pub fn new_local_pair() -> (Self, ServerInbox) {
        let (tx_client, rx_client) = mpsc::unbounded::<ClientMessage>();
        let (tx_server, rx_server) = mpsc::unbounded::<ServerMessage>();
        let endpoint = NetEndpoint::Local {
            tx_client,
            rx_server,
        };
        let inbox = ServerInbox {
            rx_client,
            tx_server,
        };
        (endpoint, inbox)
    }

    /// 发送一条 ClientMessage 给服务端。
    /// Local：push 到 mpsc。Phase 4 Host/Remote：序列化走 DataChannel。
    pub fn send_client_message(&self, msg: ClientMessage) {
        match self {
            NetEndpoint::Local { tx_client, .. } => {
                // mpsc unbounded：发送几乎不会失败（除非 receiver drop）
                let _ = tx_client.unbounded_send(msg);
            }
            NetEndpoint::Host | NetEndpoint::Remote => {
                // Phase 4+ 实装
            }
        }
    }

    /// 非阻塞拉取一条 ServerMessage。
    pub fn try_recv_server_message(&mut self) -> Option<ServerMessage> {
        match self {
            NetEndpoint::Local { rx_server, .. } => {
                match rx_server.try_next() {
                    Ok(Some(msg)) => Some(msg),
                    Ok(None) => None,  // channel closed
                    Err(_) => None,    // 暂无消息
                }
            }
            NetEndpoint::Host | NetEndpoint::Remote => None,
        }
    }
}

impl ServerInbox {
    /// 非阻塞拉取一条 ClientMessage。
    pub fn try_recv_client_message(&mut self) -> Option<ClientMessage> {
        match self.rx_client.try_next() {
            Ok(Some(msg)) => Some(msg),
            Ok(None) => None,
            Err(_) => None,
        }
    }

    /// 推一条 ServerMessage 给客户端。
    pub fn send_server_message(&self, msg: ServerMessage) {
        let _ = self.tx_server.unbounded_send(msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use voxweb_core::protocol::{ClientMessage, ServerMessage};

    #[test]
    fn local_pair_roundtrip() {
        let (mut endpoint, mut inbox) = NetEndpoint::new_local_pair();

        // Client → Server
        endpoint.send_client_message(ClientMessage::Ping { client_time_ms: 42 });
        let received = inbox.try_recv_client_message();
        assert!(matches!(received, Some(ClientMessage::Ping { client_time_ms: 42 })));

        // Server → Client
        inbox.send_server_message(ServerMessage::Pong {
            client_time_ms: 42,
            server_time_ms: 100,
        });
        let received = endpoint.try_recv_server_message();
        assert!(matches!(
            received,
            Some(ServerMessage::Pong {
                client_time_ms: 42,
                server_time_ms: 100
            })
        ));
    }

    #[test]
    fn try_recv_returns_none_when_empty() {
        let (mut endpoint, mut inbox) = NetEndpoint::new_local_pair();
        assert!(endpoint.try_recv_server_message().is_none());
        assert!(inbox.try_recv_client_message().is_none());
    }
}
```

- [ ] **Step 2: 编译 + 测试**

Run: `cargo test -p voxweb-net --lib`
Expected: 2 个测试 PASS。

Run: `cargo check -p voxweb-net --target wasm32-unknown-unknown`
Expected: 编译通过。

Run: `cargo clippy -p voxweb-net --target wasm32-unknown-unknown -- -D warnings`
Run: `cargo fmt --all -- --check`
Expected: 无错误 / 无 diff。

- [ ] **Step 3: 提交**

```bash
git add crates/net/src/lib.rs
git commit -m "feat(net): NetEndpoint::Local mpsc bidirectional channel + ServerInbox"
```

---

## Task 5：client `mesh_jobs.rs`

**Files:**
- Create: `crates/client/src/mesh_jobs.rs`
- Modify: `crates/client/src/lib.rs`（加 `pub mod mesh_jobs;`）

- [ ] **Step 1: 在 lib.rs 模块声明区追加**

打开 `crates/client/src/lib.rs`，在 Phase 1 已有的 `pub mod` 列表里追加：

```rust
pub mod chunk_loader;
pub mod mesh_jobs;
```

（chunk_loader 提前声明，Task 6 实装。）

- [ ] **Step 2: 创建 `crates/client/src/mesh_jobs.rs` 含失败测试**

```rust
//! 网格化任务调度：优先级队列 + 分帧 budget。
//!
//! Phase 2：4 档优先级（Critical / High / Medium / Low）+ 4 个 VecDeque + pending HashSet。
//! 每帧 `run_until_budget` 从最高优先级开始 pop，调用 `chunk_mesh::generate_with_neighbors`
//! 跑跨区块剔除，结果上传 Renderer。`performance.now()` 监控耗时超 budget 退出。

use std::collections::{HashSet, VecDeque};

use voxweb_core::ChunkPos;
use voxweb_render::Renderer;
use voxweb_render::chunk_mesh;
use voxweb_server::Server;

/// 网格化任务优先级（越靠前越先跑）。
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MeshPriority {
    /// 玩家正站立的 chunk
    Critical = 0,
    /// 玩家附近 1 chunk 范围
    High = 1,
    /// 渲染距离内其它
    Medium = 2,
    /// 邻居加载触发的重网格化 / 边界 chunk
    Low = 3,
}

impl MeshPriority {
    const COUNT: usize = 4;
}

/// 4 优先级队列 + pending 集合防重。
pub struct MeshJobQueue {
    queues: [VecDeque<ChunkPos>; MeshPriority::COUNT],
    pending: HashSet<ChunkPos>,
}

impl Default for MeshJobQueue {
    fn default() -> Self {
        Self {
            queues: [const { VecDeque::new() }; MeshPriority::COUNT],
            pending: HashSet::new(),
        }
    }
}

impl MeshJobQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// 把 pos 加入指定优先级队列。若已在队列里，则忽略（防重）。
    pub fn enqueue(&mut self, pos: ChunkPos, priority: MeshPriority) {
        if self.pending.insert(pos) {
            self.queues[priority as usize].push_back(pos);
        }
    }

    /// 从队列中移除 pos（卸载 chunk 时调用）。
    pub fn cancel(&mut self, pos: ChunkPos) {
        if self.pending.remove(&pos) {
            for q in &mut self.queues {
                q.retain(|p| *p != pos);
            }
        }
    }

    /// 从最高优先级队列 pop 一个；若全空返回 None。
    fn pop_highest(&mut self) -> Option<ChunkPos> {
        for q in self.queues.iter_mut() {
            if let Some(pos) = q.pop_front() {
                self.pending.remove(&pos);
                return Some(pos);
            }
        }
        None
    }

    /// 当前队列总长度（所有优先级）。
    pub fn len(&self) -> usize {
        self.queues.iter().map(|q| q.len()).sum()
    }

    /// 是否所有队列都为空。
    pub fn is_empty(&self) -> bool {
        self.queues.iter().all(|q| q.is_empty())
    }

    /// 在给定时间预算内执行尽量多的网格化任务。
    /// `now_ms` 是返回当前 performance.now() 毫秒值的闭包（便于测试）。
    pub fn run_until_budget(
        &mut self,
        budget_ms: f32,
        server: &Server,
        renderer: &mut Renderer,
        now_ms: &dyn Fn() -> f64,
    ) {
        let start = now_ms();
        loop {
            if (now_ms() - start) as f32 >= budget_ms {
                break;
            }
            let Some(pos) = self.pop_highest() else { break; };
            let Some(chunk) = server.world.chunks.get(&pos) else {
                // chunk 已被卸载，跳过
                continue;
            };
            let mesh = chunk_mesh::generate_with_neighbors(chunk, pos, &|wx, wy, wz| {
                server.world.get_block_world(wx, wy, wz)
            });
            renderer.upload_chunk_mesh(pos, &mesh);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_pop_order() {
        let mut q = MeshJobQueue::new();
        q.enqueue(ChunkPos::new(0, 0), MeshPriority::Low);
        q.enqueue(ChunkPos::new(1, 0), MeshPriority::Medium);
        q.enqueue(ChunkPos::new(2, 0), MeshPriority::Critical);
        q.enqueue(ChunkPos::new(3, 0), MeshPriority::High);

        assert_eq!(q.pop_highest(), Some(ChunkPos::new(2, 0))); // Critical
        assert_eq!(q.pop_highest(), Some(ChunkPos::new(3, 0))); // High
        assert_eq!(q.pop_highest(), Some(ChunkPos::new(1, 0))); // Medium
        assert_eq!(q.pop_highest(), Some(ChunkPos::new(0, 0))); // Low
        assert_eq!(q.pop_highest(), None);
    }

    #[test]
    fn enqueue_dedupe() {
        let mut q = MeshJobQueue::new();
        q.enqueue(ChunkPos::new(0, 0), MeshPriority::Medium);
        q.enqueue(ChunkPos::new(0, 0), MeshPriority::Critical); // 重复，应忽略
        assert_eq!(q.len(), 1);
        // 第一次入队的优先级保留（Medium）
        assert_eq!(q.pop_highest(), Some(ChunkPos::new(0, 0)));
    }

    #[test]
    fn cancel_removes_from_queues_and_pending() {
        let mut q = MeshJobQueue::new();
        q.enqueue(ChunkPos::new(0, 0), MeshPriority::Medium);
        q.enqueue(ChunkPos::new(1, 0), MeshPriority::High);
        q.cancel(ChunkPos::new(0, 0));
        assert_eq!(q.len(), 1);
        // 取消后可重新入队
        q.enqueue(ChunkPos::new(0, 0), MeshPriority::Low);
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn is_empty_initial_and_after_drain() {
        let mut q = MeshJobQueue::new();
        assert!(q.is_empty());
        q.enqueue(ChunkPos::new(0, 0), MeshPriority::Medium);
        assert!(!q.is_empty());
        q.pop_highest();
        assert!(q.is_empty());
    }
}
```

- [ ] **Step 3: 运行测试**

Run: `cargo test -p voxweb-client --lib`
Expected: 4 个测试 PASS。

> 若 `cargo test` 在 wasm32 target 失败，本任务的逻辑测试只需在 native target 通过：`cargo test -p voxweb-client --lib --target x86_64-pc-windows-msvc`（按本机替换 target）。client crate 是 `["cdylib", "rlib"]`，rlib 部分可在 native 编译。如某些 wasm-only 依赖阻挡 native 编译，加 `#[cfg(target_arch = "wasm32")]` 隔离即可。

- [ ] **Step 4: clippy + fmt**

Run: `cargo clippy -p voxweb-client --target wasm32-unknown-unknown -- -D warnings`
Run: `cargo fmt --all -- --check`
Expected: 无错误 / 无 diff。

- [ ] **Step 5: 提交**

```bash
git add crates/client/src/mesh_jobs.rs crates/client/src/lib.rs
git commit -m "feat(client): MeshJobQueue with 4-priority deque + budget runner"
```

---

## Task 6：client `chunk_loader.rs`

**Files:**
- Create: `crates/client/src/chunk_loader.rs`

- [ ] **Step 1: 写文件含工具函数 + 主结构 + 测试**

创建 `crates/client/src/chunk_loader.rs`：

```rust
//! 区块滚动加载：根据玩家相机位置维护"应加载"chunk 集合，
//! diff 出新增与移除，触发 Server 生成 / Renderer 卸载 / MeshJobQueue 入队。
//!
//! Phase 2：每次 update 在玩家跨 chunk 边界时执行；
//! 边界 chunk 通过 MeshPriority::Low 重新入队，触发跨区块剔除生效。

use std::collections::HashSet;

use glam::Vec3;
use voxweb_core::{CHUNK_X, CHUNK_Z, ChunkPos};
use voxweb_render::Renderer;
use voxweb_server::Server;

use crate::mesh_jobs::{MeshJobQueue, MeshPriority};

pub struct ChunkLoader {
    pub render_distance: i32,
    pub unload_buffer: i32,
    pub loaded: HashSet<ChunkPos>,
    last_center: Option<ChunkPos>,
}

impl ChunkLoader {
    pub fn new(render_distance: u32) -> Self {
        Self {
            render_distance: render_distance as i32,
            unload_buffer: 3,
            loaded: HashSet::new(),
            last_center: None,
        }
    }

    /// 强制下一次 update 重新计算（用于初始化或渲染距离变更）。
    pub fn invalidate(&mut self) {
        self.last_center = None;
    }

    /// 根据相机位置同步加载集合。返回是否有变更（供调试 / 性能 stat）。
    pub fn update(
        &mut self,
        camera_pos: Vec3,
        server: &mut Server,
        mesh_jobs: &mut MeshJobQueue,
        renderer: &mut Renderer,
    ) -> bool {
        let center = chunk_pos_of(camera_pos);
        if Some(center) == self.last_center {
            return false;
        }
        self.last_center = Some(center);

        // —— 1. 期望集合 ——
        let r = self.render_distance;
        let desired: HashSet<ChunkPos> = (-r..=r)
            .flat_map(|dx| (-r..=r).map(move |dz| ChunkPos::new(center.x + dx, center.z + dz)))
            .collect();

        // —— 2. 新增：生成 + 入队 ——
        let new_chunks: Vec<ChunkPos> = desired.difference(&self.loaded).copied().collect();
        for pos in &new_chunks {
            server.world.ensure_chunk_generated(*pos);
            let priority = priority_for_distance(*pos, center);
            mesh_jobs.enqueue(*pos, priority);
            self.loaded.insert(*pos);
        }

        // —— 3. 邻居重网格化：新 chunk 的水平邻居中已有 mesh 的需重做（跨区块剔除生效）——
        for pos in &new_chunks {
            for (dx, dz) in &[(1, 0), (-1, 0), (0, 1), (0, -1)] {
                let neighbor = ChunkPos::new(pos.x + dx, pos.z + dz);
                if self.loaded.contains(&neighbor) && renderer.has_chunk_mesh(neighbor) {
                    mesh_jobs.enqueue(neighbor, MeshPriority::Low);
                }
            }
        }

        // —— 4. 卸载：超出 render_distance + unload_buffer ——
        let unload_r = self.render_distance + self.unload_buffer;
        let to_unload: Vec<ChunkPos> = self
            .loaded
            .iter()
            .copied()
            .filter(|p| chebyshev_distance(*p, center) > unload_r)
            .collect();
        for pos in to_unload {
            server.world.unload_chunk(pos);
            mesh_jobs.cancel(pos);
            renderer.drop_chunk_mesh(pos);
            self.loaded.remove(&pos);
        }

        true
    }
}

/// 世界坐标 → 所在 chunk 的 ChunkPos。
pub fn chunk_pos_of(world_pos: Vec3) -> ChunkPos {
    let x = (world_pos.x as i32).div_euclid(CHUNK_X as i32);
    let z = (world_pos.z as i32).div_euclid(CHUNK_Z as i32);
    ChunkPos::new(x, z)
}

/// 切比雪夫距离（最大轴差）—— 适合方形 render distance。
pub fn chebyshev_distance(a: ChunkPos, b: ChunkPos) -> i32 {
    (a.x - b.x).abs().max((a.z - b.z).abs())
}

/// 根据距离决定网格化优先级。
pub fn priority_for_distance(pos: ChunkPos, center: ChunkPos) -> MeshPriority {
    let d = chebyshev_distance(pos, center);
    match d {
        0 => MeshPriority::Critical,
        1 => MeshPriority::High,
        _ => MeshPriority::Medium,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_pos_of_negative_coords() {
        // 负坐标向负无穷取整
        assert_eq!(chunk_pos_of(Vec3::new(-1.0, 0.0, -1.0)), ChunkPos::new(-1, -1));
        assert_eq!(chunk_pos_of(Vec3::new(0.0, 0.0, 0.0)), ChunkPos::new(0, 0));
        assert_eq!(chunk_pos_of(Vec3::new(16.0, 0.0, 0.0)), ChunkPos::new(1, 0));
        assert_eq!(chunk_pos_of(Vec3::new(15.9, 0.0, 0.0)), ChunkPos::new(0, 0));
    }

    #[test]
    fn chebyshev_basics() {
        assert_eq!(chebyshev_distance(ChunkPos::new(0, 0), ChunkPos::new(0, 0)), 0);
        assert_eq!(chebyshev_distance(ChunkPos::new(3, 4), ChunkPos::new(0, 0)), 4);
        assert_eq!(chebyshev_distance(ChunkPos::new(-2, 5), ChunkPos::new(1, 1)), 4);
    }

    #[test]
    fn priority_classification() {
        let c = ChunkPos::new(0, 0);
        assert_eq!(priority_for_distance(ChunkPos::new(0, 0), c), MeshPriority::Critical);
        assert_eq!(priority_for_distance(ChunkPos::new(1, 0), c), MeshPriority::High);
        assert_eq!(priority_for_distance(ChunkPos::new(0, -1), c), MeshPriority::High);
        assert_eq!(priority_for_distance(ChunkPos::new(2, 2), c), MeshPriority::Medium);
        assert_eq!(priority_for_distance(ChunkPos::new(5, 3), c), MeshPriority::Medium);
    }
}
```

- [ ] **Step 2: 运行测试**

Run: `cargo test -p voxweb-client --lib`
Expected: 既有 mesh_jobs 测试 + 3 个新 chunk_loader 测试全部 PASS。

- [ ] **Step 3: clippy + fmt**

Run: `cargo clippy -p voxweb-client --target wasm32-unknown-unknown -- -D warnings`
Run: `cargo fmt --all -- --check`
Expected: 无错误 / 无 diff。

- [ ] **Step 4: 提交**

```bash
git add crates/client/src/chunk_loader.rs
git commit -m "feat(client): ChunkLoader scrolling load/unload + neighbor remesh trigger"
```

---

## Task 7：server `handle_message` Hello→Welcome

**Files:**
- Modify: `crates/server/src/lib.rs:37-71`

- [ ] **Step 1: 在 server/lib.rs 末尾追加测试**

```rust
#[cfg(test)]
mod handle_message_tests {
    use super::*;
    use voxweb_core::protocol::{ClientMessage, ServerMessage};

    #[test]
    fn hello_returns_welcome_with_seed() {
        let mut server = Server::new(42);
        let replies = server.handle_message(
            1,
            ClientMessage::Hello {
                display_name: "Tester".into(),
                version: 1,
            },
        );
        assert_eq!(replies.len(), 1);
        match &replies[0] {
            ServerMessage::Welcome {
                entity_id,
                world_seed,
                ..
            } => {
                assert_eq!(*entity_id, 1);
                assert_eq!(*world_seed, 42);
            }
            other => panic!("expected Welcome, got {other:?}"),
        }
    }

    #[test]
    fn unknown_message_returns_empty_vec() {
        let mut server = Server::new(0);
        let replies = server.handle_message(1, ClientMessage::Ping { client_time_ms: 0 });
        assert!(replies.is_empty(), "Phase 2 Ping handler 未实装，应返回空");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p voxweb-server --lib handle_message_tests`
Expected: `hello_returns_welcome_with_seed` 失败（当前 `_ => vec![]` 把 Hello 吞了）。

- [ ] **Step 3: 修改 `handle_message`**

把 `crates/server/src/lib.rs` 中 `handle_message` 的 match 表添加 Hello 分支（在 Break / Place 之后、`_ => vec![]` 之前）：

```rust
            ClientMessage::Hello { .. } => {
                vec![ServerMessage::Welcome {
                    entity_id: 1,
                    server_tick: self.tick,
                    world_seed: self.seed,
                }]
            }
```

- [ ] **Step 4: 测试通过**

Run: `cargo test -p voxweb-server --lib`
Expected: 全部 PASS（含既有 World 测试 + 新 handle_message 测试）。

- [ ] **Step 5: clippy + fmt**

Run: `cargo clippy -p voxweb-server --target wasm32-unknown-unknown -- -D warnings`
Run: `cargo fmt --all -- --check`
Expected: 无错误 / 无 diff。

- [ ] **Step 6: 提交**

```bash
git add crates/server/src/lib.rs
git commit -m "feat(server): handle Hello with Welcome { entity_id, server_tick, world_seed }"
```

---

## Task 8：client `app.rs` 引入 Game 子结构

**Files:**
- Modify: `crates/client/src/app.rs`

- [ ] **Step 1: 替换 app.rs 全文**

```rust
//! 客户端全局状态机 + Game 子结构定义。
//!
//! Phase 2：AppState 仅使用 Lobby / InGame；其余态留给后续 Phase。
//! Game 子结构持有 InGame 状态下的所有运行时（Server / NetEndpoint / Camera / 调度器等）。

use std::cell::RefCell;
use std::rc::Rc;

use voxweb_net::{NetEndpoint, ServerInbox};
use voxweb_server::Server;

use crate::camera::Camera;
use crate::chunk_loader::ChunkLoader;
use crate::mesh_jobs::MeshJobQueue;

/// 应用全局状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppState {
    /// 初始加载阶段（等待 wasm + 资源初始化）
    Loading,
    /// 大厅：选择单机 / 创建 / 加入
    Lobby,
    /// 正在连接信令服务 — Phase 4 使用
    Connecting,
    /// 游戏进行中
    InGame,
    /// ESC 暂停菜单 — Phase 6 使用
    EscMenu,
    /// 聊天输入框打开 — Phase 6 使用
    ChatOpen,
    /// 连接断开提示 — Phase 4+ 使用
    Disconnected,
}

impl Default for AppState {
    fn default() -> Self {
        AppState::Loading
    }
}

/// Phase 2 游戏运行时设置。Phase 6 起会扩展为 AppSettings 全集。
#[derive(Clone, Debug)]
pub struct GameSettings {
    /// 渲染距离（单位：chunk）。默认 6，UI 可调 2..=10（Phase 6 落地）。
    pub render_distance: u32,
    /// 鼠标灵敏度。
    pub mouse_sensitivity: f32,
    /// 飞行速度（方块/秒）。
    pub fly_speed: f32,
    /// 每帧网格化预算（毫秒）。
    pub mesh_budget_ms: f32,
}

impl Default for GameSettings {
    fn default() -> Self {
        Self {
            render_distance: 6,
            mouse_sensitivity: 0.0025,
            fly_speed: 12.0,
            mesh_budget_ms: 4.0,
        }
    }
}

/// 60Hz 逻辑帧累加器。
pub struct FrameClock {
    accumulator: f32,
    step: f32,
}

impl FrameClock {
    pub fn new() -> Self {
        Self {
            accumulator: 0.0,
            step: 1.0 / 60.0,
        }
    }

    /// 累加本次 RAF 的 dt（秒）。
    pub fn accumulate(&mut self, dt: f32) {
        self.accumulator += dt;
        // 防止极端帧导致循环过长（如 tab 切到后台再回来）
        if self.accumulator > 0.25 {
            self.accumulator = 0.25;
        }
    }

    /// 若累加器 ≥ step，扣除一次返回 true。
    pub fn consume_logic_step(&mut self) -> bool {
        if self.accumulator >= self.step {
            self.accumulator -= self.step;
            true
        } else {
            false
        }
    }
}

impl Default for FrameClock {
    fn default() -> Self {
        Self::new()
    }
}

/// InGame 状态下的所有运行时资源。
pub struct Game {
    pub server: Rc<RefCell<Server>>,
    pub server_inbox: ServerInbox,
    pub net: NetEndpoint,
    pub camera: Camera,
    pub mesh_jobs: MeshJobQueue,
    pub chunk_loader: ChunkLoader,
    pub frame_clock: FrameClock,
    pub settings: GameSettings,
    /// 自己的 entity_id（由 Welcome 提供）。
    pub entity_id: u32,
}

impl Game {
    /// 启动一个单机游戏：创建 Server + 配对 NetEndpoint + 初始相机。
    /// 调用方负责后续：发 Hello、初始 chunk_loader.update。
    pub fn new_local(seed: u64, settings: GameSettings) -> Self {
        let server = Rc::new(RefCell::new(Server::new(seed)));
        let (net, server_inbox) = NetEndpoint::new_local_pair();
        let camera = Camera::default();
        let render_distance = settings.render_distance;
        Self {
            server,
            server_inbox,
            net,
            camera,
            mesh_jobs: MeshJobQueue::new(),
            chunk_loader: ChunkLoader::new(render_distance),
            frame_clock: FrameClock::new(),
            settings,
            entity_id: 0, // 待 Welcome 填充
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_clock_consume_60hz() {
        let mut fc = FrameClock::new();
        fc.accumulate(1.0 / 60.0 + 0.0001);
        assert!(fc.consume_logic_step());
        assert!(!fc.consume_logic_step());
    }

    #[test]
    fn frame_clock_caps_huge_dt() {
        let mut fc = FrameClock::new();
        fc.accumulate(10.0); // tab 切到后台
        // 累加器被限到 0.25，最多 15 个 step
        let mut steps = 0;
        while fc.consume_logic_step() {
            steps += 1;
        }
        assert!(steps <= 16, "got {steps}");
    }
}
```

- [ ] **Step 2: 编译 + 测试**

Run: `cargo test -p voxweb-client --lib`
Expected: 全部 PASS（含 mesh_jobs + chunk_loader + 新 app 测试）。

Run: `cargo check -p voxweb-client --target wasm32-unknown-unknown`
Expected: 编译通过（lib.rs 尚未消费 Game，本任务先到此）。

- [ ] **Step 3: clippy + fmt**

Run: `cargo clippy -p voxweb-client --target wasm32-unknown-unknown -- -D warnings`
Run: `cargo fmt --all -- --check`
Expected: 无错误 / 无 diff。

- [ ] **Step 4: 提交**

```bash
git add crates/client/src/app.rs
git commit -m "feat(client): Game struct + GameSettings + FrameClock"
```

---

## Task 9：client `ui/lobby.rs` 实装

**Files:**
- Modify: `crates/client/src/ui/lobby.rs`

- [ ] **Step 1: 重写 lobby.rs 全文**

```rust
//! 大厅 UI：Phase 2 仅"单机模式"按钮 + 可选种子输入。
//! Phase 4 起补"创建房间 / 加入房间"按钮。

/// 大厅按钮触发的动作。lib.rs 主循环消费。
#[derive(Clone, Debug)]
pub enum LobbyAction {
    /// 用户点了"单机模式"。seed 为 None 则随机生成。
    StartSinglePlayer { seed: Option<u64> },
}

/// 大厅 UI 持久状态（输入框文本等）。
#[derive(Default)]
pub struct LobbyState {
    pub seed_input: String,
}

/// 绘制大厅 UI。返回触发的动作（点击按钮时）。
pub fn draw_lobby(ctx: &egui::Context, state: &mut LobbyState) -> Option<LobbyAction> {
    let mut action = None;

    egui::CentralPanel::default().show(ctx, |ui| {
        ui.add_space(80.0);
        ui.vertical_centered(|ui| {
            ui.heading(
                egui::RichText::new("VoxWeb")
                    .size(48.0)
                    .color(egui::Color32::from_rgb(230, 240, 245)),
            );
            ui.add_space(8.0);
            ui.colored_label(
                egui::Color32::from_rgb(160, 170, 180),
                "Browser Voxel Sandbox (Phase 2)",
            );

            ui.add_space(48.0);

            // —— 单机模式按钮 ——
            let btn = egui::Button::new(
                egui::RichText::new("Single Player")
                    .size(20.0)
                    .color(egui::Color32::from_rgb(230, 240, 245)),
            )
            .min_size(egui::vec2(240.0, 48.0))
            .fill(egui::Color32::from_rgb(60, 90, 120));
            if ui.add(btn).clicked() {
                let seed = parse_seed(&state.seed_input);
                action = Some(LobbyAction::StartSinglePlayer { seed });
            }

            ui.add_space(16.0);

            // —— 种子输入（折叠区）——
            egui::CollapsingHeader::new("Advanced / Seed")
                .default_open(false)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Seed (u64, blank = random):");
                        ui.add(
                            egui::TextEdit::singleline(&mut state.seed_input)
                                .desired_width(180.0)
                                .hint_text("e.g. 1234567"),
                        );
                    });
                });

            ui.add_space(80.0);
            ui.colored_label(
                egui::Color32::from_rgb(120, 130, 140),
                "Phase 4: Create/Join room (coming soon)",
            );
        });

        // 底部版本提示
        egui::Area::new(egui::Id::new("lobby_version"))
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -12.0))
            .show(ctx, |ui| {
                ui.colored_label(
                    egui::Color32::from_rgb(100, 110, 120),
                    "VoxWeb 0.1.0 · Phase 2",
                );
            });
    });

    action
}

/// 把输入框文本解析为 Option<u64>。空字符串 → None（随机）。
fn parse_seed(input: &str) -> Option<u64> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_seed_empty_is_none() {
        assert_eq!(parse_seed(""), None);
        assert_eq!(parse_seed("   "), None);
    }

    #[test]
    fn parse_seed_valid_u64() {
        assert_eq!(parse_seed("42"), Some(42));
        assert_eq!(parse_seed("18446744073709551615"), Some(u64::MAX));
    }

    #[test]
    fn parse_seed_invalid_is_none() {
        assert_eq!(parse_seed("not_a_number"), None);
        assert_eq!(parse_seed("-1"), None); // 负数不是合法 u64
    }
}
```

- [ ] **Step 2: 测试 + 编译**

Run: `cargo test -p voxweb-client --lib`
Expected: 含 3 个新 lobby 测试在内全部 PASS。

Run: `cargo check -p voxweb-client --target wasm32-unknown-unknown`
Expected: 编译通过。

- [ ] **Step 3: clippy + fmt**

Run: `cargo clippy -p voxweb-client --target wasm32-unknown-unknown -- -D warnings`
Run: `cargo fmt --all -- --check`
Expected: 无错误 / 无 diff。

- [ ] **Step 4: 提交**

```bash
git add crates/client/src/ui/lobby.rs
git commit -m "feat(client): lobby UI with Single Player button + seed input"
```

---

## Task 10：client `lib.rs` Lobby/InGame 主循环

> 这是 Phase 2 最大一步：把 Phase 1 的扁平 `Runtime` 替换成 `App` 容器 + 两态主循环 + 事件路由。

**Files:**
- Modify: `crates/client/src/lib.rs`

- [ ] **Step 1: 完整重写 lib.rs**

把 `crates/client/src/lib.rs` 完整替换为：

```rust
//! VoxWeb 客户端入口（cdylib）。
//!
//! Phase 2：
//! - Lobby UI：单机模式按钮 + 种子输入
//! - InGame：Local 模式 Server + NetEndpoint::Local + ChunkLoader 滚动 + MeshJobQueue
//! - 主循环按 AppState 分流：Lobby（仅 egui） / InGame（完整 server tick + 网格化 + 渲染）

pub mod app;
pub mod camera;
pub mod chunk_loader;
pub mod input;
pub mod interp;
pub mod mesh_jobs;
pub mod physics;
pub mod prediction;
pub mod raycast;
pub mod storage;
pub mod ui;

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

use voxweb_core::protocol::{ClientMessage, ServerMessage};
use voxweb_render::Renderer;

use crate::app::{AppState, FrameClock, Game, GameSettings};
use crate::input::InputState;
use crate::ui::lobby::{LobbyAction, LobbyState, draw_lobby};

/// 全局 App：跨 state 持有 renderer / egui / input；InGame 时持有 Game。
struct App {
    canvas: HtmlCanvasElement,
    renderer: Renderer,
    egui_ctx: egui::Context,
    egui_renderer: egui_wgpu::Renderer,

    input: Rc<RefCell<InputState>>,

    state: AppState,
    lobby_state: LobbyState,
    game: Option<Game>,

    /// 上一帧 performance.now()（毫秒）
    last_time_ms: f64,
    /// FPS 滑动平均
    fps_frames: u32,
    fps_accum: f32,
    fps_display: f32,

    /// 标志：下次 InGame 渲染前请求一次指针锁（点击 Lobby 按钮触发）
    request_pointer_lock_next: bool,
}

#[wasm_bindgen(start)]
pub async fn start() -> Result<(), JsValue> {
    console_error_panic_hook::set_once();
    tracing_wasm::set_as_global_default();

    log::info!("VoxWeb 启动（Phase 2：体素单人）");

    let window = web_sys::window().ok_or_else(|| JsValue::from_str("无 window"))?;
    let document = window
        .document()
        .ok_or_else(|| JsValue::from_str("无 document"))?;
    let canvas: HtmlCanvasElement = document
        .get_element_by_id("game")
        .ok_or_else(|| JsValue::from_str("未找到 #game canvas"))?
        .dyn_into()
        .map_err(|_| JsValue::from_str("#game 不是 <canvas>"))?;
    sync_canvas_size(&canvas);

    let mut renderer = Renderer::new(&canvas)
        .await
        .map_err(|e| JsValue::from_str(&format!("Renderer init: {e}")))?;
    // resize 一次保证 surface 配置
    let (cw, ch) = sync_canvas_size(&canvas);
    renderer.resize(cw, ch);

    let egui_ctx = egui::Context::default();
    let egui_renderer = egui_wgpu::Renderer::new(
        &renderer.device,
        renderer.surface_format,
        egui_wgpu::RendererOptions::default(),
    );

    let input = Rc::new(RefCell::new(InputState::default()));

    let app = Rc::new(RefCell::new(App {
        canvas: canvas.clone(),
        renderer,
        egui_ctx,
        egui_renderer,
        input: input.clone(),
        state: AppState::Lobby,
        lobby_state: LobbyState::default(),
        game: None,
        last_time_ms: now_ms(),
        fps_frames: 0,
        fps_accum: 0.0,
        fps_display: 0.0,
        request_pointer_lock_next: false,
    }));

    install_event_listeners(&canvas, &document, input.clone(), app.clone())?;
    spawn_raf_loop(app);

    Ok(())
}

// ============================================================
// 主循环 & 事件
// ============================================================

fn spawn_raf_loop(app: Rc<RefCell<App>>) {
    let cell: Rc<RefCell<Option<Closure<dyn FnMut()>>>> = Rc::new(RefCell::new(None));
    let cell_outer = cell.clone();

    *cell.borrow_mut() = Some(Closure::wrap(Box::new(move || {
        if let Err(e) = render_frame(&app) {
            log::warn!("帧渲染失败: {e:?}");
        }
        if let Some(c) = cell_outer.borrow().as_ref() {
            request_animation_frame(c);
        }
    }) as Box<dyn FnMut()>));

    if let Some(c) = cell.borrow().as_ref() {
        request_animation_frame(c);
    }
}

fn request_animation_frame(closure: &Closure<dyn FnMut()>) {
    let _ = web_sys::window()
        .expect("no window")
        .request_animation_frame(closure.as_ref().unchecked_ref());
}

fn install_event_listeners(
    canvas: &HtmlCanvasElement,
    document: &web_sys::Document,
    input: Rc<RefCell<InputState>>,
    app: Rc<RefCell<App>>,
) -> Result<(), JsValue> {
    // —— 点击 canvas → 请求指针锁（仅在 InGame 时）——
    {
        let canvas_clone = canvas.clone();
        let app_clone = app.clone();
        let on_click = Closure::<dyn FnMut(_)>::new(move |_e: web_sys::MouseEvent| {
            if app_clone.borrow().state == AppState::InGame {
                canvas_clone.request_pointer_lock();
            }
        });
        canvas.add_event_listener_with_callback("click", on_click.as_ref().unchecked_ref())?;
        on_click.forget();
    }

    // —— pointerlockchange ——
    {
        let input_clone = input.clone();
        let document_clone = document.clone();
        let canvas_id = canvas.clone();
        let on_lock_change = Closure::<dyn FnMut()>::new(move || {
            let locked = document_clone
                .pointer_lock_element()
                .map(|el| el == *canvas_id.as_ref())
                .unwrap_or(false);
            input_clone.borrow_mut().pointer_locked = locked;
        });
        document.add_event_listener_with_callback(
            "pointerlockchange",
            on_lock_change.as_ref().unchecked_ref(),
        )?;
        on_lock_change.forget();
    }

    // —— 键盘 ——
    {
        let input_clone = input.clone();
        let app_clone = app.clone();
        let on_keydown = Closure::<dyn FnMut(_)>::new(move |e: web_sys::KeyboardEvent| {
            // Lobby 时让 egui 接管文本输入（不消费）
            if app_clone.borrow().state != AppState::InGame {
                return;
            }
            if let Some(key) = map_key(&e.code()) {
                input_clone.borrow_mut().on_key_down(key);
            }
            if input_clone.borrow().pointer_locked {
                e.prevent_default();
            }
        });
        document
            .add_event_listener_with_callback("keydown", on_keydown.as_ref().unchecked_ref())?;
        on_keydown.forget();
    }
    {
        let input_clone = input.clone();
        let app_clone = app.clone();
        let on_keyup = Closure::<dyn FnMut(_)>::new(move |e: web_sys::KeyboardEvent| {
            if app_clone.borrow().state != AppState::InGame {
                return;
            }
            if let Some(key) = map_key(&e.code()) {
                input_clone.borrow_mut().on_key_up(key);
            }
        });
        document.add_event_listener_with_callback("keyup", on_keyup.as_ref().unchecked_ref())?;
        on_keyup.forget();
    }

    // —— 鼠标移动 ——
    {
        let input_clone = input.clone();
        let on_mousemove = Closure::<dyn FnMut(_)>::new(move |e: web_sys::MouseEvent| {
            let mut s = input_clone.borrow_mut();
            if s.pointer_locked {
                s.on_mouse_move(e.movement_x() as f32, e.movement_y() as f32);
            }
        });
        document
            .add_event_listener_with_callback("mousemove", on_mousemove.as_ref().unchecked_ref())?;
        on_mousemove.forget();
    }

    // —— 鼠标按下（Phase 3 才接挖放）——
    {
        let input_clone = input.clone();
        let on_mousedown = Closure::<dyn FnMut(_)>::new(move |e: web_sys::MouseEvent| {
            input_clone.borrow_mut().on_mouse_down(e.button() as u16);
        });
        canvas
            .add_event_listener_with_callback("mousedown", on_mousedown.as_ref().unchecked_ref())?;
        on_mousedown.forget();
    }

    Ok(())
}

fn map_key(code: &str) -> Option<winit::keyboard::KeyCode> {
    use winit::keyboard::KeyCode;
    Some(match code {
        "KeyW" => KeyCode::KeyW,
        "KeyA" => KeyCode::KeyA,
        "KeyS" => KeyCode::KeyS,
        "KeyD" => KeyCode::KeyD,
        "KeyT" => KeyCode::KeyT,
        "Space" => KeyCode::Space,
        "ShiftLeft" => KeyCode::ShiftLeft,
        "ShiftRight" => KeyCode::ShiftRight,
        "Escape" => KeyCode::Escape,
        _ => return None,
    })
}

// ============================================================
// 帧分发
// ============================================================

fn render_frame(app: &Rc<RefCell<App>>) -> Result<(), String> {
    // 计算 dt + FPS
    let (dt, _fps) = update_clock(app);

    // 同步 canvas 尺寸
    let (cw, ch) = {
        let app_borrow = app.borrow();
        sync_canvas_size(&app_borrow.canvas)
    };
    {
        let mut a = app.borrow_mut();
        a.renderer.resize(cw, ch);
    }

    // 按 state 分流
    let state = app.borrow().state.clone();
    match state {
        AppState::Loading | AppState::Lobby => render_lobby_frame(app, cw, ch),
        AppState::InGame => render_game_frame(app, dt, cw, ch),
        _ => render_lobby_frame(app, cw, ch), // 其它态由后续 Phase 接入
    }
}

fn update_clock(app: &Rc<RefCell<App>>) -> (f32, f32) {
    let mut a = app.borrow_mut();
    let now = now_ms();
    let dt_ms = (now - a.last_time_ms).max(0.0);
    a.last_time_ms = now;
    let dt = (dt_ms / 1000.0) as f32;
    a.fps_frames += 1;
    a.fps_accum += dt;
    if a.fps_accum >= 0.5 {
        a.fps_display = a.fps_frames as f32 / a.fps_accum;
        a.fps_frames = 0;
        a.fps_accum = 0.0;
    }
    (dt, a.fps_display)
}

// ============================================================
// Lobby 帧
// ============================================================

fn render_lobby_frame(app: &Rc<RefCell<App>>, cw: u32, ch: u32) -> Result<(), String> {
    // 跑 egui Lobby UI
    let (action, paint_jobs, pixels_per_point, textures_delta) = {
        let mut a = app.borrow_mut();
        let raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(cw as f32, ch as f32),
            )),
            ..Default::default()
        };
        let App {
            ref egui_ctx,
            ref mut lobby_state,
            ..
        } = *a;
        let mut act: Option<LobbyAction> = None;
        let full_output = egui_ctx.run_ui(raw_input, |ui| {
            act = draw_lobby(ui.ctx(), lobby_state);
        });
        let ppp = full_output.pixels_per_point;
        let jobs = egui_ctx.tessellate(full_output.shapes, ppp);
        (act, jobs, ppp, full_output.textures_delta)
    };

    // 处理动作（开始游戏）
    if let Some(LobbyAction::StartSinglePlayer { seed }) = action {
        start_single_player(app, seed);
        // 进入 InGame 后下一帧才走 game 路径；本帧仍渲染 lobby（避免 game 未初始化的纹理上传）
    }

    // 上传 egui 纹理 + 渲染
    {
        let mut a = app.borrow_mut();
        let device = a.renderer.device.clone();
        let queue = a.renderer.queue.clone();
        for (id, image_delta) in &textures_delta.set {
            a.egui_renderer
                .update_texture(&device, &queue, *id, image_delta);
        }
        for id in &textures_delta.free {
            a.egui_renderer.free_texture(id);
        }

        let Some(surface_texture) = a.renderer.acquire_frame() else {
            return Ok(());
        };
        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("lobby_frame"),
        });

        // 清屏（暗蓝色背景），不用 OpaquePass
        {
            let _clear_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("lobby_clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.05,
                            g: 0.07,
                            b: 0.10,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }

        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [cw, ch],
            pixels_per_point,
        };
        let extra_cmds = a.egui_renderer.update_buffers(
            &device,
            &queue,
            &mut encoder,
            &paint_jobs,
            &screen_descriptor,
        );
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("lobby_egui"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let mut pass = pass.forget_lifetime();
            a.egui_renderer
                .render(&mut pass, &paint_jobs, &screen_descriptor);
        }

        queue.submit(
            extra_cmds
                .into_iter()
                .chain(std::iter::once(encoder.finish())),
        );
        surface_texture.present();
    }
    Ok(())
}

fn start_single_player(app: &Rc<RefCell<App>>, seed: Option<u64>) {
    use getrandom::getrandom;

    let seed = seed.unwrap_or_else(|| {
        let mut buf = [0u8; 8];
        let _ = getrandom(&mut buf);
        u64::from_le_bytes(buf)
    });
    log::info!("启动单机游戏，seed = {seed}");

    let settings = GameSettings::default();
    let mut game = Game::new_local(seed, settings);

    // 发 Hello，driver 下一帧消费
    game.net.send_client_message(ClientMessage::Hello {
        display_name: "Player".into(),
        version: 1,
    });

    // 把 spawn 位置塞进相机（先看一眼地形）
    game.camera.position = glam::Vec3::new(8.0, 100.0, 8.0);
    game.camera.pitch = -0.4;

    let mut a = app.borrow_mut();
    a.game = Some(game);
    a.state = AppState::InGame;
    a.request_pointer_lock_next = true;
}

// ============================================================
// InGame 帧
// ============================================================

fn render_game_frame(app: &Rc<RefCell<App>>, dt: f32, cw: u32, ch: u32) -> Result<(), String> {
    // —— 1. drain Local 通道（Client→Server）→ Server::handle_message → 推回 Server→Client ——
    {
        let mut a = app.borrow_mut();
        let Some(game) = a.game.as_mut() else {
            return Ok(());
        };
        // 拉客户端消息
        let mut pending = Vec::new();
        while let Some(msg) = game.server_inbox.try_recv_client_message() {
            pending.push(msg);
        }
        let entity_id = if game.entity_id == 0 { 1 } else { game.entity_id };
        for msg in pending {
            let replies = game.server.borrow_mut().handle_message(entity_id, msg);
            for reply in replies {
                game.server_inbox.send_server_message(reply);
            }
        }
    }

    // —— 2. drain Server→Client → 应用 ——
    {
        let mut a = app.borrow_mut();
        let Some(game) = a.game.as_mut() else {
            return Ok(());
        };
        while let Some(msg) = game.net.try_recv_server_message() {
            apply_server_message(game, msg);
        }
    }

    // —— 3. 输入 → 相机 + 4. 逻辑帧 ——
    let (camera_pos, view_proj, fps_display, mesh_budget) = {
        let mut a = app.borrow_mut();
        let fps_display = a.fps_display;
        let Some(game) = a.game.as_mut() else {
            return Ok(());
        };
        // 更新 camera aspect
        game.camera.aspect = cw as f32 / ch.max(1) as f32;

        // 输入 → 相机（Fly 模式）
        let input_rc = a.input.clone();
        let mut input = input_rc.borrow_mut();
        if input.pointer_locked && (input.mouse_dx != 0.0 || input.mouse_dy != 0.0) {
            game.camera
                .apply_mouse(input.mouse_dx, input.mouse_dy, game.settings.mouse_sensitivity);
        }
        game.camera
            .apply_fly_input(&input, game.settings.fly_speed, dt);
        input.reset_delta();
        drop(input);

        // 逻辑帧（仅推进 tick）
        game.frame_clock.accumulate(dt);
        while game.frame_clock.consume_logic_step() {
            game.server.borrow_mut().tick();
        }

        (
            game.camera.position,
            game.camera.vp_matrix(),
            fps_display,
            game.settings.mesh_budget_ms,
        )
    };

    // —— 5. ChunkLoader 滚动 ——
    {
        let mut a = app.borrow_mut();
        let App {
            ref mut renderer,
            ref mut game,
            ..
        } = *a;
        let Some(game) = game.as_mut() else {
            return Ok(());
        };
        let mut server_mut = game.server.borrow_mut();
        game.chunk_loader.update(
            camera_pos,
            &mut server_mut,
            &mut game.mesh_jobs,
            renderer,
        );
    }

    // —— 6. mesh_jobs run_until_budget ——
    {
        let mut a = app.borrow_mut();
        let App {
            ref mut renderer,
            ref mut game,
            ..
        } = *a;
        let Some(game) = game.as_mut() else {
            return Ok(());
        };
        let server_borrow = game.server.borrow();
        game.mesh_jobs
            .run_until_budget(mesh_budget, &server_borrow, renderer, &now_ms);
    }

    // —— 7. egui HUD ——
    let raw_input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(cw as f32, ch as f32),
        )),
        ..Default::default()
    };
    let pointer_locked = app.borrow().input.borrow().pointer_locked;
    let (paint_jobs, pixels_per_point, textures_delta) = {
        let a = app.borrow();
        let yaw_deg = a.game.as_ref().map(|g| g.camera.yaw.to_degrees()).unwrap_or(0.0);
        let pitch_deg = a.game.as_ref().map(|g| g.camera.pitch.to_degrees()).unwrap_or(0.0);
        let pos = a.game.as_ref().map(|g| g.camera.position).unwrap_or_default();
        let loaded_chunks = a.game.as_ref().map(|g| g.chunk_loader.loaded.len()).unwrap_or(0);
        let mesh_pending = a.game.as_ref().map(|g| g.mesh_jobs.len()).unwrap_or(0);
        let full_output = a.egui_ctx.run_ui(raw_input, |ui| {
            draw_hud(
                ui.ctx(),
                fps_display,
                (pos.x, pos.y, pos.z),
                yaw_deg,
                pitch_deg,
                pointer_locked,
                loaded_chunks,
                mesh_pending,
            );
        });
        let ppp = full_output.pixels_per_point;
        let jobs = a.egui_ctx.tessellate(full_output.shapes, ppp);
        (jobs, ppp, full_output.textures_delta)
    };

    // —— 8. 渲染 + present ——
    {
        let mut a = app.borrow_mut();
        let device = a.renderer.device.clone();
        let queue = a.renderer.queue.clone();
        for (id, image_delta) in &textures_delta.set {
            a.egui_renderer
                .update_texture(&device, &queue, *id, image_delta);
        }
        for id in &textures_delta.free {
            a.egui_renderer.free_texture(id);
        }

        let Some(surface_texture) = a.renderer.acquire_frame() else {
            return Ok(());
        };
        let view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("game_frame"),
        });

        // 世界 Pass
        a.renderer
            .render_world(&mut encoder, &view, view_proj, [0.55, 0.78, 0.93, 1.0]);

        // egui Pass
        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [cw, ch],
            pixels_per_point,
        };
        let extra_cmds = a.egui_renderer.update_buffers(
            &device,
            &queue,
            &mut encoder,
            &paint_jobs,
            &screen_descriptor,
        );
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("game_egui"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let mut pass = pass.forget_lifetime();
            a.egui_renderer
                .render(&mut pass, &paint_jobs, &screen_descriptor);
        }

        queue.submit(
            extra_cmds
                .into_iter()
                .chain(std::iter::once(encoder.finish())),
        );
        surface_texture.present();

        // 进入 InGame 后请求指针锁（必须在用户手势后；首次的"开始游戏"按钮点击算手势）
        if a.request_pointer_lock_next {
            a.canvas.request_pointer_lock();
            a.request_pointer_lock_next = false;
        }
    }

    Ok(())
}

fn apply_server_message(game: &mut Game, msg: ServerMessage) {
    match msg {
        ServerMessage::Welcome {
            entity_id,
            world_seed,
            ..
        } => {
            game.entity_id = entity_id;
            log::info!("Welcome: entity_id={entity_id}, seed={world_seed}");
        }
        _ => {
            // Phase 3+ 才处理 BlockUpdate / PlayerTick 等
        }
    }
}

// ============================================================
// HUD（egui）
// ============================================================

#[allow(clippy::too_many_arguments)]
fn draw_hud(
    ctx: &egui::Context,
    fps: f32,
    pos: (f32, f32, f32),
    yaw_deg: f32,
    pitch_deg: f32,
    pointer_locked: bool,
    loaded_chunks: usize,
    mesh_pending: usize,
) {
    egui::Area::new(egui::Id::new("hud_topleft"))
        .anchor(egui::Align2::LEFT_TOP, egui::vec2(12.0, 12.0))
        .show(ctx, |ui| {
            egui::Frame::default()
                .fill(egui::Color32::from_rgba_unmultiplied(0, 0, 0, 140))
                .inner_margin(egui::Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 230, 235),
                        format!("FPS  {:>5.1}", fps),
                    );
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 230, 235),
                        format!("POS  x {:+8.2}  y {:+8.2}  z {:+8.2}", pos.0, pos.1, pos.2),
                    );
                    ui.colored_label(
                        egui::Color32::from_rgb(180, 190, 200),
                        format!("YAW {:+6.1}°  PITCH {:+5.1}°", yaw_deg, pitch_deg),
                    );
                    ui.colored_label(
                        egui::Color32::from_rgb(160, 175, 190),
                        format!("CHUNKS {loaded_chunks}  MESH_Q {mesh_pending}"),
                    );
                });
        });

    egui::Area::new(egui::Id::new("hud_crosshair"))
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.label(
                egui::RichText::new("+")
                    .color(egui::Color32::from_rgba_unmultiplied(255, 255, 255, 220))
                    .size(22.0)
                    .strong(),
            );
        });

    egui::Area::new(egui::Id::new("hud_bottom"))
        .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -16.0))
        .show(ctx, |ui| {
            let msg = if pointer_locked {
                "WASD move | Space up | Shift down | Mouse look | ESC release"
            } else {
                "Click to enter camera control"
            };
            egui::Frame::default()
                .fill(egui::Color32::from_rgba_unmultiplied(0, 0, 0, 110))
                .inner_margin(egui::Margin::symmetric(14, 8))
                .show(ui, |ui| {
                    ui.colored_label(egui::Color32::from_rgb(230, 235, 240), msg);
                });
        });
}

// ============================================================
// 工具
// ============================================================

fn sync_canvas_size(canvas: &HtmlCanvasElement) -> (u32, u32) {
    let w = (canvas.client_width().max(1)) as u32;
    let h = (canvas.client_height().max(1)) as u32;
    if canvas.width() != w {
        canvas.set_width(w);
    }
    if canvas.height() != h {
        canvas.set_height(h);
    }
    (w, h)
}

fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0)
}
```

> **注意 Step 1 中的"稳健版" 重复**：上面 `render_lobby_frame` 已是清理后的单版本（无 unsafe）。

- [ ] **Step 2: 编译**

Run: `cargo check -p voxweb-client --target wasm32-unknown-unknown`
Expected: 编译通过。

> 若有借用错误，针对 `render_game_frame` 内的 `&mut a` 块拆分，确保 `a.renderer` 与 `a.game` 不同时可变借出 — 已用 `let App { ref mut renderer, ref mut game, ..} = *a;` 拆解。

- [ ] **Step 3: clippy + fmt + test**

Run: `cargo clippy --workspace --target wasm32-unknown-unknown -- -D warnings`
Run: `cargo fmt --all -- --check`
Run: `cargo test --workspace --lib`
Expected: 全部 PASS / 无 warning / 无 diff。

- [ ] **Step 4: 提交**

```bash
git add crates/client/src/lib.rs
git commit -m "feat(client): App state machine Lobby/InGame + main loop refactor for Phase 2"
```

---

## Task 11：浏览器人工验收

**Files:**（无源码改动）

- [ ] **Step 1: 启动 trunk serve**

Run: `trunk serve --port 8080`
Expected: 终端无错误，浏览器 `http://localhost:8080/start` 可访问。

- [ ] **Step 2: Lobby 检查**

- 访问 `http://localhost:8080/start`
- 看到 "VoxWeb" 标题 + "Single Player" 大按钮 + 折叠的 "Advanced / Seed"
- 控制台无错误

- [ ] **Step 3: 进入游戏检查**

- 点 "Single Player"
- 1 秒内进入游戏画面，看到连绵地形（草 / 泥 / 石分层）
- HUD 显示 FPS / POS / YAW PITCH / CHUNKS / MESH_Q
- 按 ESC 释放指针锁；点画面再次锁定

- [ ] **Step 4: 飞行加载检查**

- 用 WASD + 空格 + Shift 飞行 30 秒
- 持续看到新地形流入
- FPS 始终 ≥ 55（M2 / 中端 PC）
- CHUNKS 显示约 (2×6+1)² = 169 上下浮动
- MESH_Q 在飞行后稳定回到 0

- [ ] **Step 5: 跨区块剔除检查**

- 飞到地形下方（y < 0）抬头看 chunk 底面
- 边界处**无可见漏面或墙皮**（重点验收）
- 若看到边界裂缝，回到 Task 6 确认邻居重网格化逻辑触发正确

- [ ] **Step 6: 卸载 / 回头检查**

- 选定固定方向飞 15 个 chunk（≈240 米），观察 HUD CHUNKS 维持稳定
- 反向飞回起点
- 起点附近地形外观与离开时完全相同（同 seed 重生成等价）

- [ ] **Step 7: 失败回滚原则**

任何上述项不通过：
- 不要提交"修复"，先回 docs/superpowers/specs/2026-05-11-phase2-design.md 找对应章节理清意图
- 必要时回上一个绿色提交：`git log --oneline` + `git reset --hard <hash>` 后逐 Task 排查

- [ ] **Step 8: 不提交本任务（人工验收无代码改动）**

---

## Task 12：文档与里程碑

**Files:**
- Create: `PHASE_2_DONE.md`
- Modify: `docs/roadmap.md`（Phase 2 标题加 ✅ + 移除"设计已批准"占位）

- [ ] **Step 1: 写 `PHASE_2_DONE.md`**

参考 `PHASE_1_DONE.md` 结构。模板：

```markdown
# Phase 2 · 体素单人 · 完成报告

> 完成日期：YYYY-MM-DD（按实际填写）
> 关联：[`docs/roadmap.md`](docs/roadmap.md) Phase 2
> 设计：[`docs/superpowers/specs/2026-05-11-phase2-design.md`](docs/superpowers/specs/2026-05-11-phase2-design.md)

---

## 实际完成项

- ✅ **World 地形与生命周期** [crates/server/src/world.rs](crates/server/src/world.rs)
  - `ensure_chunk_generated` / `get_block_world` / `unload_chunk`
  - World 持有 `TerrainGenerator`
- ✅ **跨区块面剔除** [crates/render/src/chunk_mesh.rs](crates/render/src/chunk_mesh.rs)
  - `generate_with_neighbors(chunk, pos, get_block_world)`
- ✅ **Renderer chunk 资源生命周期** [crates/render/src/lib.rs](crates/render/src/lib.rs)
  - `drop_chunk_mesh` / `has_chunk_mesh`
- ✅ **NetEndpoint::Local mpsc 双向通道** [crates/net/src/lib.rs](crates/net/src/lib.rs)
  - `new_local_pair` + `ServerInbox`
- ✅ **MeshJobQueue** [crates/client/src/mesh_jobs.rs](crates/client/src/mesh_jobs.rs)
  - 4 优先级 + budget runner
- ✅ **ChunkLoader** [crates/client/src/chunk_loader.rs](crates/client/src/chunk_loader.rs)
  - 滚动加载 / 卸载 + 邻居重网格化
- ✅ **Server Hello→Welcome** [crates/server/src/lib.rs](crates/server/src/lib.rs)
- ✅ **App + Game 状态机** [crates/client/src/app.rs](crates/client/src/app.rs)
  - `GameSettings` + `FrameClock` + `Game::new_local`
- ✅ **Lobby UI** [crates/client/src/ui/lobby.rs](crates/client/src/ui/lobby.rs)
  - 单机模式按钮 + 种子输入
- ✅ **主循环 Lobby/InGame 分流** [crates/client/src/lib.rs](crates/client/src/lib.rs)

---

## 验证

| 项 | 标准 | 实测 |
|---|---|---|
| `cargo fmt --all -- --check` | 无 diff | ✅ |
| `cargo clippy --workspace --target wasm32-unknown-unknown -- -D warnings` | 无错误 | ✅ |
| `cargo test --workspace --lib` | 全通过 | ✅ N/N |
| 大厅 → 单机模式进入 | < 1s | （填写）|
| 飞行 30s 60fps | FPS ≥ 55 | （填写）|
| 跨区块边界无漏面 | 视觉确认 | （填写）|
| 走远再回头地形一致 | 视觉确认 | （填写）|

---

## 已知问题 / 后续

（按实际填写：边角情况、Phase 3 需要补的接口、性能 hot spot 等）

---

## 下一步：Phase 3 · 物理与交互

入口文档：[docs/features/physics.md](docs/features/physics.md)
要点（参考 [docs/roadmap.md](docs/roadmap.md) Phase 3 任务清单）：
- Walk 模式 + AABB 物理 + 重力 + 跳跃
- DDA 射线 + 鼠标左键挖 / 右键放
- BlockUpdate 闭环 + 受影响 chunk 重网格化
- 1-9 hotbar
```

- [ ] **Step 2: 更新 `docs/roadmap.md`**

把 `## Phase 2 · 体素单人` 改为 `## Phase 2 · 体素单人 ✅`。
把"> 设计已批准"那行替换为 "> 完成日期：YYYY-MM-DD · 详见 [`PHASE_2_DONE.md`](../PHASE_2_DONE.md)"。
把任务清单里 `- [ ]` 全部改为 `- [x]`。

- [ ] **Step 3: 提交**

```bash
git add PHASE_2_DONE.md docs/roadmap.md
git commit -m "docs: Phase 2 done - 体素单人模式完整实装"
```

- [ ] **Step 4: （可选）打 tag**

```bash
git tag phase-2
```

---

## 完成标志

12 个 Task 全部 `- [x]`，且：
- `cargo test --workspace --lib` 全绿
- `cargo clippy --workspace --target wasm32-unknown-unknown -- -D warnings` 无 warning
- 浏览器人工验收 Task 11 全部 OK
- `PHASE_2_DONE.md` 写完，roadmap.md Phase 2 ✅
