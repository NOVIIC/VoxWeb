# 持久化（OPFS）

> **何时阅读**：改存档逻辑；增删存储字段；调写入频率；处理配额异常；评估迁移到 Worker 同步路径
> **关联文档**：[`README.md`](../../README.md) · [`modules/server.md`](../modules/server.md) · [`modules/client.md`](../modules/client.md) · [`reference.md`](../reference.md) · [`roadmap.md`](../roadmap.md)
>
> **Phase 8 状态**：Variant A 已落地。`chunk::encode/decode`、`OpfsStorage`、`WorldStorage` trait、
> `PersistenceManager` snapshot/commit/failure、启动 prime、1s 周期 flush、LRU/pinned、配额 UI、
> `storage_version` 高版本拒绝与删档入口已实现。Worker sync handle 仍按本文 §十二保留为后续升级路径。

---

## 一、范围

**仅 Host 与 Local-Only 写入存档**。Remote Client 不写：
- Remote 不持有权威世界
- 重连时从 Host 重新拉取快照即可

存档生命周期绑定到 **房间号 + 世界种子**。同一房间号 + 同一种子 → 同一份存档；不同种子 → 不同存档（同房间号下的"另开一个世界"）。

> 历史：本项目曾计划用 IndexedDB（[git log](../) 可查 Phase 5 早期草案），但在评估"多人长期房间"（约 10000 dirty chunk）压力时发现 IDB 在配额、内存常驻、启动加载、退出 flush 四方面会同时撞墙，故改为 OPFS。详见下节对比。

---

## 二、为什么选 OPFS（而非 IndexedDB / File System Access API）

| 维度 | IndexedDB | **OPFS** | File System Access API |
|---|---|---|---|
| 容量上限 | ~1 GB（默认配额） | **~60% 磁盘**（配额，可 `persist()` 提升） | 用户磁盘自由 |
| 浏览器支持 | 全部 | Chrome/Edge 102+、Firefox 111+、Safari 17+ | 仅 Chromium 系（write） |
| 主线程 API | 仅 async | async + **Worker 内 sync handle** | 仅 async |
| 用户可见文件 | 否 | 否 | ✅ 是 |
| 每次会话需授权 | 否 | 否 | ✅ 是 |
| `pagehide` 同步落盘 | 不可能 | ✅ Worker + sync handle 可 | 不可能 |
| 选用为主存储 | ✗ | ✅ | ✗（作为 Phase 9 stretch 可选导出） |

OPFS 是当前唯一同时满足"全浏览器、多 GB 容量、可同步落盘"的方案。File System Access API 留作 Phase 9 「导出存档到本地文件夹」可选导出渠道。

---

## 三、文件布局

OPFS 根目录的内部结构：

```
opfs:/voxweb/
├── _meta.json                       # 全局元信息（所有世界的索引，大厅"我的存档"用）
└── <room_id>__<seed>/               # 一个世界一个目录
    ├── world.json                   # WorldRecord（JSON，便于 DevTools 调试）
    └── chunks/
        ├── <cx>_<cz>.bin            # encode() 输出：palette + RLE + bincode
        ├── 0_0.bin
        ├── 0_1.bin
        └── ...
```

### `world.json` schema

```jsonc
{
  "key": "abc123__1234567890",
  "room_id": "abc123",
  "seed": "1234567890",                 // u64 存为 string 避免 JSON 精度丢失
  "display_name": "abc123",
  "created_at_ms": 1746000000000,
  "updated_at_ms": 1746000060000,
  "storage_version": 1,                 // 见第十节 migration
  "protocol_version": 1                 // 写入时的 ClientMessage 协议版本
}
```

### `_meta.json` schema

```jsonc
{
  "worlds": [
    { "key": "abc123__1234567890", "display_name": "abc123", "updated_at_ms": ... },
    ...
  ]
}
```

### 单 chunk 文件 `<cx>_<cz>.bin`

二进制，结构由 [`crates/core/src/chunk.rs`](../../crates/core/src/chunk.rs) 的 `encode` / `decode` 决定（见第四节）。命名约定：负坐标使用 `n` 前缀，例如 `n1_2.bin` 表示 `(-1, 2)`；这样 OPFS 文件名跨平台安全（避免某些实现不接受连字符）。

---

## 四、Chunk 压缩格式（palette + RLE）

> 此格式同时用于 **OPFS 磁盘存储**（本节）和 **ChunkSnapshot 网络传输**（[`networking/protocol.md` §五](../networking/protocol.md#五chunk-快照同步)）。网络侧已实装 `encode_chunk` / `decode_chunk`（`crates/core/src/chunk.rs`）；磁盘侧延后至 Phase 8。

未压缩的 `Chunk` 在内存中是 `Vec<BlockID>`，长度 65536，每个 `BlockID` 2 字节 = **128 KB/chunk**。万级 chunk 直接 dump 会撑爆 OPFS 配额。

实际格式（bincode 2.x，little-endian + varint）：

```rust
#[derive(Serialize, Deserialize)]
struct EncodedChunk {
    version: u8,             // 1（与 STORAGE_VERSION 同步）
    palette: Vec<BlockID>,   // 出现过的所有 BlockID，无重复，典型 < 32 项
    runs: Vec<(u16, u32)>,   // (palette_index, run_length)，runs 总长 == CHUNK_SIZE
}
```

### 压缩比

| Chunk 内容 | 未压缩 | 压缩后 |
|---|---|---|
| 全 AIR | 128 KB | < 20 B |
| 全 STONE | 128 KB | < 20 B |
| 典型地形（草+泥+石分层） | 128 KB | 2-5 KB |
| 玩家精雕建筑（高方块多样性） | 128 KB | 8-20 KB |

### 公开接口

```rust
// crates/core/src/chunk.rs
pub const STORAGE_VERSION: u8 = 1;

pub fn encode(chunk: &Chunk) -> Vec<u8>;

#[derive(Debug)]
pub enum DecodeError {
    Corrupted(bincode::error::DecodeError),
    VersionMismatch { found: u8, expected: u8 },
    InvalidPaletteIndex(u16),
    LengthMismatch(usize),
}

pub fn decode(bytes: &[u8]) -> Result<Chunk, DecodeError>;
```

> 注意：当前 `core::lib.rs` 已 re-export `protocol::encode`（消息编码）。chunk 的 encode/decode 仅以 `voxweb_core::chunk::encode` 形式引用，**不**在 lib.rs 顶层 re-export，避免命名冲突。

### 单测要求

- 全空 / 全单方块 / 典型地形 / 高熵随机三种 chunk 的 roundtrip 全等
- 解码越界 palette index → 返回 `InvalidPaletteIndex`，不 panic
- 解码截断字节 → 返回 `Corrupted`
- 编码确定性：同输入两次编码字节相同（便于做 hash diff）

---

## 五、Rust 接口

存储层抽象为 trait，便于未来插入 Variant B（Worker）实现或 File System Access API 导出。

### 5.1 trait 定义（`crates/client/src/storage.rs`）

```rust
use voxweb_core::chunk::ChunkPos;

#[derive(Debug)]
pub enum StorageError {
    NotSupported,            // 浏览器无 OPFS
    NotFound,                // 文件/目录不存在
    QuotaExceeded,
    Io(String),              // 包装底层 DOMException
    Decode(chunk::DecodeError),
}

pub struct QuotaInfo {
    pub quota: u64,
    pub usage: u64,
}

/// 单 World 的存储句柄。一个 OpfsStorage 实例对应一个 (room_id, seed)。
#[async_trait::async_trait(?Send)]
pub trait WorldStorage {
    async fn open(room_id: &str, seed: u64) -> Result<Self, StorageError>
        where Self: Sized;

    /// 启动时先调一次：拉所有 chunk 的文件名清单（仅文件名，不读内容）。
    async fn list_chunks(&self) -> Result<Vec<ChunkPos>, StorageError>;

    /// 异步读单个 chunk 的原始字节（未 decode）。
    async fn load_chunk(&self, pos: ChunkPos) -> Result<Option<Vec<u8>>, StorageError>;

    /// 批量写入 encoded chunks。失败时调用者负责把失败的 pos 标回 dirty。
    async fn save_chunks(&self, items: Vec<(ChunkPos, Vec<u8>)>) -> Result<(), StorageError>;

    async fn delete_world(&self) -> Result<(), StorageError>;

    /// 配额信息，UI 显示用；不可用时返回 None。
    async fn quota(&self) -> Option<QuotaInfo>;
}
```

### 5.2 OPFS 实现

```rust
pub struct OpfsStorage {
    root: web_sys::FileSystemDirectoryHandle,         // opfs:/voxweb/<world_key>/
    chunks_dir: web_sys::FileSystemDirectoryHandle,   // .../chunks/
    world_key: String,
}
```

实现要点：

- 用 `navigator.storage.getDirectory()` 拿 OPFS root
- `getDirectoryHandle("voxweb", { create: true })` → world dir
- 写：`getFileHandle(name, { create: true }).createWritable()` → `write(Vec<u8>)` → `close()`
- 读：`getFileHandle().getFile().arrayBuffer()`
- 列：异步迭代 `chunks_dir.entries()`，对每个文件名解析回 `ChunkPos`
- 删：`removeEntry(name, { recursive: true })`（Chromium / 较新 Firefox 支持；旧 Safari 需逐文件 `removeEntry`）

不直接抛 `JsValue`，所有 DOMException 包装为 `StorageError`。

---

## 六、读写时机

### 6.1 启动 prime 阶段（同步预加载）

**目的**：让玩家出生即看到正确地形，避免落地后才异步覆盖造成视觉跳变。

```rust
async fn start_host(app: &mut App, room_id: String, seed: u64) {
    app.state = AppState::Connecting { stage: ConnectingStage::LoadingStorage };

    let storage = OpfsStorage::open(&room_id, seed).await?;
    let _ = web_sys::window().unwrap().navigator()
        .storage().persist().ok(); // 异步申请持久存储，不阻塞

    // 拉文件名清单（仅字符串列表，万级 chunk 也只需 100-300 ms）
    let known: HashSet<ChunkPos> = storage.list_chunks().await?.into_iter().collect();

    // prime：玩家出生点周围 4 chunk 半径（约 81 chunk）同步预加载
    let spawn = ChunkPos::new(0, 0);
    let mut server = Server::new(seed, default_config());
    for pos in chunks_within(spawn, 4) {
        if known.contains(&pos) {
            if let Some(bytes) = storage.load_chunk(pos).await? {
                let chunk = voxweb_core::chunk::decode(&bytes)?;
                server.load_chunk_from_storage(pos, chunk);
            }
        }
    }

    app.server = Some(server);
    app.storage = Some(storage);
    app.known_persisted = known;
    // 继续：信令握手 / 等 Remote 加入 ...
}
```

### 6.2 运行时按需加载

[`crates/server/src/world.rs`](../../crates/server/src/world.rs) 的 `get_or_generate(pos)` 改为三态：

1. **内存命中** → 返回
2. **`known_persisted` 不含此 pos** → 走 terrain 生成（最常见快路径）
3. **`known_persisted` 含此 pos 但内存无** → 发起异步 load，先返回 terrain 生成结果占位；load 完成后用 OPFS 数据**覆盖** server 内对应 chunk 并触发受影响 chunk 重网格化

注意：server crate 本身保持平台无关（无 `web-sys` 依赖）。异步 load 由 client 端协程发起，通过 `futures-channel` mpsc 把 `(pos, decoded_chunk)` 推给 server tick 消费。

副作用：在极短时间内（异步 load 完成前）玩家可能看到 terrain 生成的"原始版本"。缓解：prime 阶段已覆盖出生点；玩家不可能瞬间走出 prime 半径。

### 6.3 周期 flush

主循环每 **1 秒** 触发一次（频率较 IDB 旧方案的 30s/5s 更激进，因 OPFS 写入更廉价；目的是缩小关 Tab 丢失窗口到 1 秒以内）：

```rust
fn maybe_flush_persistence(&mut self) {
    if !self.frame_clock.persistence_due(1_000.0) { return; }
    let Some(persistence) = self.server.as_mut().map(|s| &mut s.persistence) else { return; };
    let Some(storage) = self.storage.clone() else { return; };

    // snapshot：不删 dirty，仅取出待写
    let to_flush = persistence.snapshot_dirty(64);
    if to_flush.is_empty() { return; }

    // 主线程 encode（每帧最多 4 chunk → 单帧 < 4 ms）
    let encoded: Vec<(ChunkPos, Vec<u8>)> = to_flush.iter()
        .take(4)
        .filter_map(|pos| {
            let chunk = persistence.world.peek_chunk(*pos)?;
            Some((*pos, voxweb_core::chunk::encode(chunk)))
        })
        .collect();
    let positions: Vec<ChunkPos> = encoded.iter().map(|(p, _)| *p).collect();

    let server_handle = self.server_handle();   // 用于 commit / failure 回写
    wasm_bindgen_futures::spawn_local(async move {
        match storage.save_chunks(encoded).await {
            Ok(()) => server_handle.commit_flushed(&positions),
            Err(e) => {
                tracing::error!("flush failed: {e:?}");
                server_handle.record_flush_failure(&positions);
            }
        }
    });
}
```

### 6.4 退出 flush（`pagehide`）

监听 `pagehide`（移动端 Safari 比 `beforeunload` 可靠）：

```rust
let cb = Closure::wrap(Box::new(move |_: web_sys::Event| {
    // 尽力 flush；BFCache 命中时实际会 await 完成
    let storage = ...;
    let persistence = ...;
    wasm_bindgen_futures::spawn_local(async move {
        let all_dirty = persistence.snapshot_dirty(usize::MAX);
        let encoded = all_dirty.iter()
            .filter_map(|pos| persistence.world.peek_chunk(*pos).map(|c| (*pos, encode(c))))
            .collect();
        let _ = storage.save_chunks(encoded).await;
    });
}) as Box<dyn FnMut(_)>);
window.add_event_listener_with_callback("pagehide", cb.as_ref().unchecked_ref())?;
```

> **风险**：纯 async 路径无法在 page unload 前保证完成。配合 1 秒周期 flush，预计丢失窗口 < 1 秒编辑。Variant B（Worker + sync handle）可消除此风险，见第十二节。

### 6.5 玩家手动保存

暂停菜单"立即保存"按钮：
```rust
if ui.button("立即保存").clicked() {
    let all = persistence.snapshot_dirty(usize::MAX);
    // ... encode + save_chunks（同步反馈进度条）
}
```

---

## 七、LRU 卸载策略

为防止 dirty chunk 常驻 + WASM 线性内存撑爆，[`crates/server/src/world.rs`](../../crates/server/src/world.rs) 用 LRU + pinned 集合：

```rust
struct World {
    chunks: LruCache<ChunkPos, Chunk>,        // 默认 capacity 4096
    pinned: HashSet<ChunkPos>,                // 玩家渲染距离内的 chunk
    persistence: PersistenceManager,
}
```

**驱逐规则**：
- 从 LRU tail 取候选；若 `pinned`，跳过到下一个
- 若 chunk 是 dirty：不能立即驱逐 → 触发紧急 flush（同步序列化 + 异步写），完成后再移除
- 触发节奏：插入新 chunk 导致超 capacity 时驱逐一个

**容量选择**：4096 chunk 约 1 km² 实加载区，覆盖渲染距离 16 仍有余量。容量做 runtime 可调（设置菜单 + 配置上限 1 万），以兼容设置渲染距离极高的桌面用户。

> **注意**：未压缩 chunk in-memory 仍是 128 KB；4096 chunk = 512 MB，在 wasm32 限额内但偏紧。Phase 8 可考虑 in-memory 也用 palette 压缩（懒解码），但初期不做。

---

## 八、PersistenceManager：snapshot / commit / retry

[`crates/server/src/persistence.rs`](../../crates/server/src/persistence.rs) 把"取出 dirty"和"确认成功"解耦：

```rust
pub struct PersistenceManager {
    dirty: HashSet<ChunkPos>,
    in_flight: HashSet<ChunkPos>,             // 已 snapshot 但未确认结果
    failure_backoff_until: HashMap<ChunkPos, f64>,  // 失败后退避的 deadline
    next_flush_tick: u64,
}

impl PersistenceManager {
    pub fn mark_dirty(&mut self, pos: ChunkPos);
    pub fn snapshot_dirty(&mut self, limit: usize) -> Vec<ChunkPos>;   // dirty → in_flight
    pub fn commit_flushed(&mut self, positions: &[ChunkPos]);           // in_flight 移除
    pub fn record_flush_failure(&mut self, positions: &[ChunkPos]);     // in_flight → dirty + backoff
    pub fn should_flush(&self, current_tick: u64) -> bool;
}
```

退避策略：失败一次 → 下次 +1s；连续 3 次 → +5s；连续 10 次 → +30s 并 `tracing::error` 告警。

---

## 九、配额管理

```rust
pub async fn check_quota() -> Option<QuotaInfo> {
    let storage = web_sys::window()?.navigator().storage();
    let est = JsFuture::from(storage.estimate().ok()?).await.ok()?;
    let quota = js_sys::Reflect::get(&est, &"quota".into()).ok()?.as_f64()?;
    let usage = js_sys::Reflect::get(&est, &"usage".into()).ok()?.as_f64()?;
    Some(QuotaInfo { quota: quota as u64, usage: usage as u64 })
}
```

UI（暂停菜单）：

```
存档使用：12.3 MB / 8.0 GB（0.15%）
[导出存档（FSA）]  [删除存档]
```

- 用量 > 80%：黄色警告
- 用量 > 95%：红色 + 暂停新挖块的持久化（仍允许在内存内编辑，但拒新 dirty 入 flush 队列）

### 持久化授权

启动时调一次 `navigator.storage.persist()`（Promise 异步），避免浏览器在低空间时自动清理 OPFS。返回值不阻塞主流程；用户拒绝时也继续运行。

---

## 十、协议 migration（替代强制删档）

`world.json.storage_version` 写入 `STORAGE_VERSION`。加载时三分支：

```rust
match world.storage_version.cmp(&STORAGE_VERSION) {
    Ordering::Equal => { /* 直接用 */ }
    Ordering::Less => migrate(&mut world, &storage)?,     // 跑 migrations[old..new]
    Ordering::Greater => return Err(StorageError::NeedsUpgrade),  // 用户需升级 VoxWeb
}
```

`migrations` 是有序数组，每项 `fn(&mut World, &OpfsStorage) -> Result<(), MigrationError>`。Phase 8 仅含 identity（v1→v1）；v2 协议升级时新增 v1→v2 步骤。

不再"版本不同就删档"——大存档用户的累积损失不可接受。

---

## 十一、错误处理

| 错误 | 行为 |
|---|---|
| OPFS 不可用（浏览器不支持） | **不应发生**：[第十五节](#十五浏览器能力前置检测)的能力检测会在 wasm 加载前拦截 |
| OPFS 打开失败 | 弹大厅提示；玩家可"无存档继续"（仅本会话有效） |
| OPFS 写入失败（配额满 / IO 异常） | 日志 error；`record_flush_failure` 退避重试；UI 不打扰 |
| OPFS 读取失败 | 视为 chunk 不存在 → 走 terrain 生成；记录损坏 key 供调试 |
| 配额满 | UI 弹红色提示 → 玩家手动清理或导出 |
| Chunk 解码失败 | 视为不存在；调用 `tracing::warn!`，文件保留供事后分析 |
| `storage_version` 高于本版本 | 大厅显示"需升级 VoxWeb 才能打开此存档"，提供升级链接 |

---

## 十二、Variant A vs Variant B（Worker）

两种实现路径，Phase 8 默认走 A，视真实丢数据投诉决定是否升级 B。

### Variant A：无 Worker，单线程 async（**Phase 8 默认**）

- 序列化在主线程；OPFS 写入走 `createWritable / write / close` 异步 API
- `pagehide` 内 `spawn_local`，尽力 await；BFCache 路径下浏览器会等待完成
- 配合 1 秒周期 flush，预计丢失窗口 < 1 秒编辑
- 代码量：≈ 600 行 Rust，0 行 JS
- 风险：极端情况下关 Tab 仍可能丢 < 1 秒编辑；万级 chunk 一次性 encode 可能造成单帧卡顿（缓解：每帧最多 4 chunk encode）

### Variant B：Dedicated Worker + sync handle（**Phase 8 升级路径**）

- 新增 `crates/client/src/storage_worker.rs`，独立 wasm 包
- Worker 内独占 `FileSystemSyncAccessHandle`，可同步 `write()` 落盘
- 主线程通过 `postMessage(Transferable)` 把 encoded `Vec<u8>` 零拷贝传 Worker
- 退出时发 `"flush-now"` 消息，Worker 在事件回调内同步完成所有 pending 写入
- 需要：Trunk 双 entry 构建配置、消息协议、`SharedArrayBuffer`（可选，否则用 transfer）
- 代码量：+600 行 Rust + Trunk/JS glue
- 代价：构建产物体积 +~300 KB；调试链路双倍复杂

**接口层（`WorldStorage` trait + `PersistenceManager` 拆分）已为 Variant B 平滑替换预留**——切换时仅替换 `OpfsStorage` 为 `WorkerStorage`，server / app 代码无需改。

---

## 十三、性能预期

| 操作 | Variant A 预期耗时 |
|---|---|
| `OpfsStorage::open` + `list_chunks`（1000 chunk） | 50-150 ms |
| `OpfsStorage::open` + `list_chunks`（10000 chunk） | 100-300 ms |
| Prime 加载 81 chunk（出生点） | 150-400 ms |
| `save_chunks`（4 chunk，每帧批） | 5-15 ms（其中 encode 1-4 ms） |
| 单 chunk encode（典型地形） | 0.5-2 ms |
| 单 chunk decode | 0.3-1 ms |

启动加载是用户体验关键路径 → 大厅显示"加载存档..."进度条遮蔽。

---

## 十四、调试工具

### DevTools

- **Chrome / Edge**：DevTools → Application → Storage → 选 origin → 「Open Origin Private File System」可直接浏览/删除
- **Firefox**：DevTools → Storage → IndexedDB 同级有 OPFS 节点（111+）
- **Safari**：尚未在 Web Inspector 暴露 OPFS UI；用控制台命令兜底

### 控制台命令（开发模式）

通过 `wasm-bindgen` 暴露到 `window.voxwebDebug`：

```js
window.voxwebDebug.listWorlds()              // 列所有存档 key
window.voxwebDebug.exportWorld(roomId, seed) // 触发 .l3w 下载（Phase 9）
window.voxwebDebug.clearAllWorlds()          // 一键清空 OPFS
window.voxwebDebug.fillDirty(n)              // 注入 n 个伪 dirty chunk（stress 测试）
window.voxwebDebug.quota()                   // 打印 quota / usage
```

---

## 十五、浏览器能力前置检测

由于 OPFS 是本方案的硬依赖，必须在用户访问站点 → 加载 `.wasm` **之前** 就告知不兼容用户，避免下载几 MB wasm 后才在运行时报错。

实现位置：[start.html](../../start.html) / landing `index.html`。在 `<script type="module">` 引入 wasm 之前插入内联检测脚本（< 2 KB gz，纯 JS，无依赖）。

检测清单：
- `WebAssembly`
- `navigator.gpu`（WebGPU）
- `navigator.storage.getDirectory`（**OPFS**）
- `RTCPeerConnection` + `createDataChannel`（WebRTC）
- `WebSocket`
- `requestPointerLock`

特殊处理：若仅 WebRTC 缺失而其他都满足 → 提供"仅单机模式"按钮，client 启动后强制走 Local-Only 路径，大厅禁用"创建/加入房间"按钮。其他必备项缺失 → 拒绝加载 wasm，显示降级页面（纯 HTML/CSS）。

详细检测清单、最低浏览器版本、文案表 → 见 [`reference.md` §3.4 OPFS](../reference.md#34-opfs)。

---

## 十六、世界导出 / 导入（Phase 9 stretch）

格式：`.l3w` 文件（zip 压缩 bincode）
- 内含 `world.json` + `_meta_local.json` + 若干 `chunks/<cx>_<cz>.bin`
- 暂停菜单"导出存档"按钮：
  - Chromium 系：`showDirectoryPicker` → 写到用户选择的真实目录
  - Firefox / Safari：触发浏览器单文件下载
- 大厅"导入存档" → `<input type="file">` → 解压写入 OPFS

不在 Phase 8 范围。

---

## 十七、不在范围

- IndexedDB fallback（OPFS 在所有目标浏览器都可用，能力检测兜底，无须再写一份 IDB 实现）
- 多版本备份 / 时光回溯
- 自动云同步（与 GitHub/Drive 集成）
- 加密存档（涉及密钥管理；浏览器内 ROI 低）
- Chunk 增量 diff（每次只存 delta） — 实现复杂收益有限，已用 palette+RLE 摊薄
- 跨域共享存档（不同站点的 OPFS 互相隔离，无法直接共享）
