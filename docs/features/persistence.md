# 持久化（IndexedDB）

> **何时阅读**：改存档逻辑；增删存储字段；调写入频率；处理配额异常
> **关联文档**：[`README.md`](../../README.md) · [`modules/server.md`](../modules/server.md) · [`modules/client.md`](../modules/client.md) · [`reference.md`](../reference.md)

---

## 一、范围

**仅 Host 与 Local-Only 写入存档**。Remote Client 不写：
- Remote 不持有权威世界
- 重连时从 Host 重新拉取快照即可

存档生命周期绑定到**房间号 + 世界种子**。同一房间号 + 同一种子 → 同一份存档；不同种子 → 不同存档（同房间号下的"另开一个世界"）。

---

## 二、IndexedDB Schema

数据库名：`voxweb_world`
版本：`1`

### Object Stores

#### `worlds`

记录每个世界的元信息。

| 字段 | 类型 | 说明 |
|---|---|---|
| `key`（primary） | `string` | `<room_id>__<seed>` |
| `room_id` | `string` | 房间号 |
| `seed` | `u64`（存为 string，IDB 不支持 BigInt 全部） | 世界种子 |
| `display_name` | `string` | 玩家给世界命名（可选） |
| `created_at_ms` | `number` | UNIX 毫秒 |
| `updated_at_ms` | `number` | UNIX 毫秒 |
| `protocol_version` | `number` | 写入时的协议版本，加载时校验 |

#### `chunks`

存放每个被玩家修改过的 chunk。**只存 dirty chunk**，未修改的 chunk 永远靠 terrain 重新生成。

| 字段 | 类型 | 说明 |
|---|---|---|
| `key`（primary） | `string` | `<room_id>__<seed>__<cx>__<cz>` |
| `world_key` | `string` | 上面的 world key（建索引用于 cascade delete） |
| `cx` | `number` | ChunkPos.x |
| `cz` | `number` | ChunkPos.z |
| `data` | `Uint8Array` | bincode 序列化的 `Chunk`（含 RLE 优化） |
| `updated_at_ms` | `number` | |

**Indexes**：
- `chunks` 上建 `by_world` 索引（`world_key`），用于一次删除整个世界的全部 chunks

---

## 三、Rust 包装（`crates/client/src/storage.rs`）

使用 `idb` crate（`web-sys` IndexedDB 异步包装）。

```rust
use idb::*;

pub struct IndexedDbStorage {
    db: Database,
    world_key: String,
}

impl IndexedDbStorage {
    pub async fn open(room_id: &str, seed: u64) -> Result<Self, idb::Error> {
        let factory = Factory::new()?;
        let mut request = factory.open("voxweb_world", Some(1))?;
        request.on_upgrade_needed(|event| {
            let db = event.database()?;

            if !db.store_names().contains(&"worlds".to_string()) {
                db.create_object_store("worlds", ObjectStoreParams::new()
                    .key_path(Some(KeyPath::new_single("key"))))?;
            }
            if !db.store_names().contains(&"chunks".to_string()) {
                let store = db.create_object_store("chunks", ObjectStoreParams::new()
                    .key_path(Some(KeyPath::new_single("key"))))?;
                store.create_index("by_world", KeyPath::new_single("world_key"), None)?;
            }
            Ok(())
        });
        let db = request.await?;
        let world_key = format!("{}__{}", room_id, seed);
        Ok(Self { db, world_key })
    }

    pub async fn ensure_world_record(&self, room_id: &str, seed: u64) -> Result<(), idb::Error> {
        let now = performance_now_ms();
        let tx = self.db.transaction(&["worlds"], TransactionMode::ReadWrite)?;
        let store = tx.object_store("worlds")?;

        // get-or-create
        let existing = store.get(JsValue::from_str(&self.world_key))?.await?;
        if existing.is_undefined() {
            let record = serde_wasm_bindgen::to_value(&WorldRecord {
                key: self.world_key.clone(),
                room_id: room_id.into(),
                seed: seed.to_string(),
                display_name: room_id.into(),
                created_at_ms: now,
                updated_at_ms: now,
                protocol_version: PROTOCOL_VERSION,
            })?;
            store.put(&record, None)?;
        } else {
            let mut record: WorldRecord = serde_wasm_bindgen::from_value(existing)?;
            record.updated_at_ms = now;
            let v = serde_wasm_bindgen::to_value(&record)?;
            store.put(&v, None)?;
        }
        tx.await?;
        Ok(())
    }

    pub async fn save_chunks(&self, dirty: Vec<(ChunkPos, Chunk)>) -> Result<(), idb::Error> {
        if dirty.is_empty() { return Ok(()); }
        let tx = self.db.transaction(&["chunks"], TransactionMode::ReadWrite)?;
        let store = tx.object_store("chunks")?;
        let now = performance_now_ms();
        for (pos, chunk) in dirty {
            let data = core::encode(&chunk).map_err(idb_internal)?;
            let record = ChunkRecord {
                key: format!("{}__{}__{}", self.world_key, pos.x, pos.z),
                world_key: self.world_key.clone(),
                cx: pos.x,
                cz: pos.z,
                data,
                updated_at_ms: now,
            };
            let v = serde_wasm_bindgen::to_value(&record)?;
            store.put(&v, None)?;
        }
        tx.await?;
        Ok(())
    }

    pub async fn load_chunk(&self, pos: ChunkPos) -> Result<Option<Chunk>, idb::Error> {
        let key = format!("{}__{}__{}", self.world_key, pos.x, pos.z);
        let tx = self.db.transaction(&["chunks"], TransactionMode::ReadOnly)?;
        let store = tx.object_store("chunks")?;
        let value = store.get(JsValue::from_str(&key))?.await?;
        if value.is_undefined() { return Ok(None); }
        let record: ChunkRecord = serde_wasm_bindgen::from_value(value)?;
        let chunk: Chunk = core::decode(&record.data).map_err(idb_internal)?;
        Ok(Some(chunk))
    }

    pub async fn load_all_chunks(&self) -> Result<Vec<(ChunkPos, Chunk)>, idb::Error> {
        let tx = self.db.transaction(&["chunks"], TransactionMode::ReadOnly)?;
        let store = tx.object_store("chunks")?;
        let index = store.index("by_world")?;
        let cursor = index.open_cursor(Some(&JsValue::from_str(&self.world_key)), None)?;
        let mut out = Vec::new();
        // ... cursor 遍历，详见 idb crate 文档
        Ok(out)
    }

    pub async fn delete_world(&self) -> Result<(), idb::Error> {
        let tx = self.db.transaction(&["worlds", "chunks"], TransactionMode::ReadWrite)?;
        // 删 chunks
        let chunks = tx.object_store("chunks")?;
        let index = chunks.index("by_world")?;
        let cursor = index.open_cursor(Some(&JsValue::from_str(&self.world_key)), None)?;
        // 遍历 cursor 一一删除（IDB 不支持 batch delete by index）
        // 删 world record
        tx.object_store("worlds")?.delete(JsValue::from_str(&self.world_key))?;
        tx.await?;
        Ok(())
    }
}
```

---

## 四、读写时机

### 启动加载

Host 模式 / Local-Only 模式启动时：

```rust
async fn start_host(app: &mut App, room_id: String, seed: u64) {
    app.state = AppState::Connecting { stage: ConnectingStage::SignalingHandshake.into() };

    let storage = IndexedDbStorage::open(&room_id, seed).await.expect("idb open");
    storage.ensure_world_record(&room_id, seed).await.expect("idb ensure");

    let mut server = Server::new(seed, default_config());
    let chunks = storage.load_all_chunks().await.expect("idb load");
    for (pos, chunk) in chunks {
        server.load_chunk_from_storage(pos, chunk);
    }

    app.server = Some(server);
    app.storage = Some(storage);

    // 继续：连接信令、等 Remote 加入
    ...
}
```

### 周期性 flush

主循环每 30 秒触发一次：

```rust
fn maybe_flush_persistence(&mut self) {
    if !self.frame_clock.persistence_due(30_000.0) { return; }
    let Some(server) = &mut self.server else { return; };
    let Some(storage) = self.storage.clone() else { return; };

    let dirty = server.take_dirty_chunks();
    if dirty.is_empty() { return; }

    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) = storage.save_chunks(dirty).await {
            tracing::error!("persist failed: {e:?}");
            // 失败时把 dirty 还回 server？ 简化：丢弃，下个 30 秒再尝试新 dirty
        }
    });
}
```

### 退出前 flush

在浏览器 `beforeunload` 事件中：

```rust
let cb = Closure::wrap(Box::new(move |_: web_sys::BeforeUnloadEvent| {
    // 注意：beforeunload 内只能做同步工作
    // 异步保存可能不会完成 → 需要在 logic tick 末尾保持 dirty 集合较小
    // 或：采取主动策略（每 5 秒 flush 而非 30 秒），减少退出风险
}) as Box<dyn FnMut(_)>);
window.add_event_listener_with_callback("beforeunload", cb.as_ref().unchecked_ref())?;
```

> **限制**：浏览器 `beforeunload` 不允许阻塞异步操作完成；最稳妥是把 flush 频率提高到 5 秒。本期默认 5 秒 flush。

### 玩家手动保存

暂停菜单提供"立即保存"按钮：
```rust
if ui.button("立即保存").clicked() {
    let dirty = app.server.as_mut().unwrap().take_dirty_chunks();
    let storage = app.storage.clone().unwrap();
    spawn_local(async move {
        let _ = storage.save_chunks(dirty).await;
    });
}
```

---

## 五、加载策略

`server.world.get_or_generate(pos)` 改为：

```rust
fn get_or_generate(&mut self, pos: ChunkPos) -> &Chunk {
    self.chunks.entry(pos).or_insert_with(|| {
        terrain::generate(self.seed, pos)
    })
}
```

注意：`server` 不能直接异步读 IDB（核心原则：server 平台无关）。所以加载流程改为：

1. **启动时全量加载**：`load_all_chunks` 把 IDB 中所有 dirty chunk 一次性塞入 server（如上 `start_host`）
2. **运行时按需加载**：玩家走远到新区域 → `server.get_or_generate` 直接调地形生成器（IDB 中没有"未修改"的 chunk）

这样 server 永远走同步路径，IDB 只在启动时介入一次。

> 副作用：玩家走过的 chunk 一旦 dirty 了就常驻内存。运行长时间后内存可能膨胀；若需要释放，v2 加 LRU 卸载策略（卸载前必须 flush 该 chunk 到 IDB）。

---

## 六、配额管理

浏览器对 IndexedDB 有总配额限制（通常 ≥ 1 GB，通过 `navigator.storage.estimate()` 查询）。

```rust
pub async fn check_quota() -> Option<QuotaInfo> {
    let storage = web_sys::window()?.navigator().storage();
    let est = JsFuture::from(storage.estimate().ok()?).await.ok()?;
    let quota = js_sys::Reflect::get(&est, &"quota".into()).ok()?.as_f64()?;
    let usage = js_sys::Reflect::get(&est, &"usage".into()).ok()?.as_f64()?;
    Some(QuotaInfo { quota: quota as u64, usage: usage as u64 })
}
```

UI 在暂停菜单中显示当前使用量：
```
存档使用：12.3 MB / 1.0 GB（1.2%）
[导出存档]  [删除存档]
```

接近上限（> 80%）时弹提示。

### 持久化授权

可选：调用 `navigator.storage.persist()` 申请"持久存储"权限，避免浏览器自动清理。
```rust
let _ = JsFuture::from(navigator.storage().persist().ok()?).await;
```

---

## 七、世界导出 / 导入（v2）

格式：`.l3w` 文件（zip 压缩 bincode）
- 内含 `world.bin`（WorldRecord）+ 若干 `chunks/x_z.bin`
- 暂停菜单"导出存档"→ 浏览器下载
- 大厅"导入存档"→ 文件选择器 → 解压写入 IDB

---

## 八、错误处理

| 错误 | 行为 |
|---|---|
| IDB 打开失败 | 弹大厅提示；玩家可"无存档继续"（仅本会话有效，关闭丢失） |
| IDB 写入失败 | 日志 error；保留 dirty 标记，下次重试；UI 不打扰 |
| IDB 读取失败 | 视为 chunk 不存在 → 走 terrain 生成 |
| 配额满 | 弹提示 → 玩家手动清理或导出 |
| 数据损坏 | bincode decode 失败 → 视为 chunk 不存在；记录损坏 key 供调试 |

---

## 九、协议升级

`protocol_version` 写在 `WorldRecord` 中。加载时校验：

```rust
if record.protocol_version != PROTOCOL_VERSION {
    // 大厅弹提示：存档版本不兼容，是否删除并重建？
    return Err(StorageError::IncompatibleVersion);
}
```

简化：本期一旦版本不兼容就要求用户删存档重建（v2 实现 migration）。

---

## 十、性能

| 操作 | 预期耗时 |
|---|---|
| `open` | 50-200 ms |
| `load_all_chunks`（200 个 dirty chunk） | 500-1000 ms |
| `save_chunks`（10 个 dirty） | 50-150 ms |
| 单 chunk 序列化（带 RLE） | 1-5 ms |

启动加载是阻塞用户体验的关键路径 → 在大厅按"创建房间"后，UI 显示"加载存档..."进度。

---

## 十一、调试工具

浏览器 DevTools → Application → IndexedDB → `voxweb_world` 可直接查看 / 删除记录。

开发模式下提供 console 命令：
```js
window.voxwebDebug.exportWorld(roomId, seed)  // 触发导出
window.voxwebDebug.clearAllWorlds()            // 一键清空
```

通过 `wasm-bindgen` 暴露这些函数到 `window.voxwebDebug` namespace。

---

## 十二、不在范围

- 多版本备份 / 时光回溯
- 自动云同步（与 GitHub/Drive 集成）
- 加密存档（涉及密钥管理；浏览器内做意义不大）
- chunk 增量 diff（每次只存 delta） — 实现复杂收益有限
- 跨域共享存档（不同站点的 IDB 是隔离的，无法直接共享）
