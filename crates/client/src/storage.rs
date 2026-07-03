//! OPFS（Origin Private File System）异步读写包装。
//!
//! Phase 8 默认走 Variant A：主线程 async OPFS。这里不保存运行时对象本体，
//! 只保存 `voxweb_core::field::encode` 产出的 FieldChunk 字节，调用方负责 encode/decode。

use std::collections::HashMap;

use js_sys::{Array, Reflect, Uint8Array};
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    Blob, FileSystemDirectoryHandle, FileSystemFileHandle, FileSystemGetDirectoryOptions,
    FileSystemGetFileOptions, FileSystemRemoveOptions, FileSystemWritableFileStream,
};

use voxweb_core::chunk::{self, ChunkPos, STORAGE_VERSION};
use voxweb_core::object::FreeObject;
use voxweb_core::protocol::PROTOCOL_VERSION;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageError {
    NotSupported,
    NotFound,
    QuotaExceeded,
    NeedsUpgrade { found: u8, supported: u8 },
    Decode(chunk::DecodeError),
    Io(String),
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct QuotaInfo {
    pub quota: u64,
    pub usage: u64,
}

impl QuotaInfo {
    pub fn usage_ratio(self) -> f32 {
        if self.quota == 0 {
            0.0
        } else {
            self.usage as f32 / self.quota as f32
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorldRecord {
    pub key: String,
    pub room_id: String,
    pub seed: String,
    pub display_name: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub storage_version: u8,
    pub protocol_version: u32,
    #[serde(default)]
    pub active_free_objects: Vec<FreeObject>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct MetaRecord {
    pub worlds: Vec<WorldSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorldSummary {
    pub key: String,
    pub display_name: String,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub used_bytes: u64,
}

#[derive(Clone, Debug, Default)]
pub struct StorageOverview {
    pub worlds: Vec<WorldSummary>,
    pub quota: Option<QuotaInfo>,
}

#[allow(async_fn_in_trait)]
pub trait WorldStorage {
    async fn open(room_id: &str, seed: u64) -> Result<Self, StorageError>
    where
        Self: Sized;
    async fn list_chunks(&self) -> Result<Vec<ChunkPos>, StorageError>;
    async fn load_chunk(&self, pos: ChunkPos) -> Result<Option<Vec<u8>>, StorageError>;
    async fn save_chunks(&self, items: Vec<(ChunkPos, Vec<u8>)>) -> Result<(), StorageError>;
    async fn quota(&self) -> Option<QuotaInfo>;
}

#[derive(Clone)]
pub struct OpfsStorage {
    worlds_root: FileSystemDirectoryHandle,
    chunks_dir: FileSystemDirectoryHandle,
    world_key: String,
}

impl OpfsStorage {
    pub fn world_key(&self) -> &str {
        &self.world_key
    }

    /// 内部方法：通过 key 打开世界目录
    async fn open_impl_by_key(key: &str) -> Result<Self, StorageError> {
        let storage = web_sys::window()
            .ok_or(StorageError::NotSupported)?
            .navigator()
            .storage();
        let root_value = JsFuture::from(storage.get_directory())
            .await
            .map_err(js_error)?;
        let opfs_root: FileSystemDirectoryHandle = root_value.dyn_into().map_err(js_error)?;

        let create_dir = {
            let opts = FileSystemGetDirectoryOptions::new();
            opts.set_create(true);
            opts
        };
        let worlds_root = get_dir(&opfs_root, "voxweb", &create_dir).await?;
        let root = get_dir(&worlds_root, key, &create_dir).await?;
        let chunks_dir = get_dir(&root, "chunks", &create_dir).await?;

        let now = now_ms() as u64;
        let record = match load_world_record(&root).await? {
            Some(record) if record.storage_version != STORAGE_VERSION => {
                return Err(StorageError::NeedsUpgrade {
                    found: record.storage_version,
                    supported: STORAGE_VERSION,
                });
            }
            Some(mut record) => {
                record.updated_at_ms = now;
                record
            }
            None => {
                // 解析 key 获取 seed
                let seed = seed_from_key(key).unwrap_or(0);
                WorldRecord {
                    key: key.to_string(),
                    room_id: "local".to_string(),
                    seed: seed.to_string(),
                    display_name: "Unknown".to_string(),
                    created_at_ms: parse_world_key(key).map(|(ts, _)| ts * 1000).unwrap_or(now),
                    updated_at_ms: now,
                    storage_version: STORAGE_VERSION,
                    protocol_version: PROTOCOL_VERSION,
                    active_free_objects: Vec::new(),
                }
            }
        };
        write_text_file(
            &root,
            "world.json",
            &serde_json::to_string_pretty(&record).unwrap(),
        )
        .await?;
        update_meta(&worlds_root, &record).await?;

        Ok(Self {
            worlds_root,
            chunks_dir,
            world_key: key.to_string(),
        })
    }

    pub async fn request_persistence() -> Option<bool> {
        let storage = web_sys::window()?.navigator().storage();
        let promise = storage.persist().ok()?;
        JsFuture::from(promise).await.ok()?.as_bool()
    }

    /// 通过 key 直接打开已有存档（加载存档时用）
    pub async fn open_by_key(key: &str) -> Result<Self, StorageError> {
        Self::open_impl_by_key(key).await
    }

    /// 创建新存档（用当前时间戳 + seed 生成 key）
    pub async fn create_new(seed: u64) -> Result<Self, StorageError> {
        let created_at_s = (now_ms() / 1000.0) as u64;
        let key = make_world_key(created_at_s, seed);
        Self::open_impl_by_key(&key).await
    }

    /// 获取当前世界的 WorldRecord
    pub async fn world_record(&self) -> Option<WorldRecord> {
        let root = get_dir(&self.worlds_root, &self.world_key, &{
            let opts = FileSystemGetDirectoryOptions::new();
            opts.set_create(false);
            opts
        })
        .await
        .ok()?;
        load_world_record(&root).await.ok().flatten()
    }

    pub async fn load_active_free_objects(&self) -> Result<Vec<FreeObject>, StorageError> {
        let root = get_dir(&self.worlds_root, &self.world_key, &{
            let opts = FileSystemGetDirectoryOptions::new();
            opts.set_create(false);
            opts
        })
        .await?;
        Ok(load_world_record(&root)
            .await?
            .map(|record| record.active_free_objects)
            .unwrap_or_default())
    }

    pub async fn save_world_state(
        &self,
        chunks: Vec<(ChunkPos, Vec<u8>)>,
        active_free_objects: Vec<FreeObject>,
    ) -> Result<(), StorageError> {
        for (pos, bytes) in chunks {
            write_bytes_file(&self.chunks_dir, &chunk_filename(pos), &bytes).await?;
        }
        self.save_active_free_objects(&active_free_objects).await
    }

    pub async fn save_active_free_objects(
        &self,
        active_free_objects: &[FreeObject],
    ) -> Result<(), StorageError> {
        let root = get_dir(&self.worlds_root, &self.world_key, &{
            let opts = FileSystemGetDirectoryOptions::new();
            opts.set_create(false);
            opts
        })
        .await?;
        let now = now_ms() as u64;
        let mut record = load_world_record(&root).await?.unwrap_or_else(|| {
            let seed = seed_from_key(&self.world_key).unwrap_or(0);
            WorldRecord {
                key: self.world_key.clone(),
                room_id: "local".to_string(),
                seed: seed.to_string(),
                display_name: "Unknown".to_string(),
                created_at_ms: parse_world_key(&self.world_key)
                    .map(|(ts, _)| ts * 1000)
                    .unwrap_or(now),
                updated_at_ms: now,
                storage_version: STORAGE_VERSION,
                protocol_version: PROTOCOL_VERSION,
                active_free_objects: Vec::new(),
            }
        });
        record.updated_at_ms = now;
        record.active_free_objects = active_free_objects.to_vec();
        write_text_file(
            &root,
            "world.json",
            &serde_json::to_string_pretty(&record).unwrap(),
        )
        .await?;
        update_meta(&self.worlds_root, &record).await
    }

    /// 获取当前世界每个 chunk 文件的持久化大小，供 HUD 增量维护世界占用。
    pub async fn chunk_file_sizes(&self) -> Result<HashMap<ChunkPos, u64>, StorageError> {
        chunk_file_sizes(&self.chunks_dir).await
    }

    /// 获取当前世界 `world.json` 的大小。文件不存在时按 0 处理。
    pub async fn world_record_size(&self) -> Result<u64, StorageError> {
        let root = get_dir(&self.worlds_root, &self.world_key, &{
            let opts = FileSystemGetDirectoryOptions::new();
            opts.set_create(false);
            opts
        })
        .await?;
        match file_size(&root, "world.json").await {
            Ok(size) => Ok(size),
            Err(StorageError::NotFound) => Ok(0),
            Err(e) => Err(e),
        }
    }

    /// 将当前世界的更新时间写回 `world.json` 和 `_meta.json`。
    pub async fn touch_updated_at(&self) -> Result<(), StorageError> {
        let root = get_dir(&self.worlds_root, &self.world_key, &{
            let opts = FileSystemGetDirectoryOptions::new();
            opts.set_create(false);
            opts
        })
        .await?;
        let now = now_ms() as u64;
        let mut record = load_world_record(&root).await?.unwrap_or_else(|| {
            let seed = seed_from_key(&self.world_key).unwrap_or(0);
            WorldRecord {
                key: self.world_key.clone(),
                room_id: "local".to_string(),
                seed: seed.to_string(),
                display_name: "Unknown".to_string(),
                created_at_ms: parse_world_key(&self.world_key)
                    .map(|(ts, _)| ts * 1000)
                    .unwrap_or(now),
                updated_at_ms: now,
                storage_version: STORAGE_VERSION,
                protocol_version: PROTOCOL_VERSION,
                active_free_objects: Vec::new(),
            }
        });
        record.updated_at_ms = now;
        write_text_file(
            &root,
            "world.json",
            &serde_json::to_string_pretty(&record).unwrap(),
        )
        .await?;
        update_meta(&self.worlds_root, &record).await
    }
}

impl WorldStorage for OpfsStorage {
    async fn open(_room_id: &str, seed: u64) -> Result<Self, StorageError> {
        // 创建新存档（使用当前时间戳 + seed 生成 key）
        Self::create_new(seed).await
    }

    async fn list_chunks(&self) -> Result<Vec<ChunkPos>, StorageError> {
        let mut out = Vec::new();
        let iter = self.chunks_dir.keys();
        loop {
            let next = JsFuture::from(iter.next().map_err(js_error)?)
                .await
                .map_err(js_error)?;
            let done = Reflect::get(&next, &JsValue::from_str("done"))
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if done {
                break;
            }
            let Some(name) = Reflect::get(&next, &JsValue::from_str("value"))
                .ok()
                .and_then(|v| v.as_string())
            else {
                continue;
            };
            if let Some(pos) = parse_chunk_filename(&name) {
                out.push(pos);
            }
        }
        Ok(out)
    }

    async fn load_chunk(&self, pos: ChunkPos) -> Result<Option<Vec<u8>>, StorageError> {
        let name = chunk_filename(pos);
        let handle = match get_file(&self.chunks_dir, &name, false).await {
            Ok(handle) => handle,
            Err(StorageError::NotFound) => return Ok(None),
            Err(e) => return Err(e),
        };
        let file: Blob = JsFuture::from(handle.get_file())
            .await
            .map_err(js_error)?
            .dyn_into()
            .map_err(js_error)?;
        let buffer = JsFuture::from(file.array_buffer())
            .await
            .map_err(js_error)?;
        let bytes = Uint8Array::new(&buffer).to_vec();
        Ok(Some(bytes))
    }

    async fn save_chunks(&self, items: Vec<(ChunkPos, Vec<u8>)>) -> Result<(), StorageError> {
        let touched = !items.is_empty();
        for (pos, bytes) in items {
            write_bytes_file(&self.chunks_dir, &chunk_filename(pos), &bytes).await?;
        }
        if touched {
            self.touch_updated_at().await?;
        }
        Ok(())
    }

    async fn quota(&self) -> Option<QuotaInfo> {
        quota().await
    }
}

pub async fn quota() -> Option<QuotaInfo> {
    let storage = web_sys::window()?.navigator().storage();
    let estimate = JsFuture::from(storage.estimate().ok()?).await.ok()?;
    let quota = Reflect::get(&estimate, &JsValue::from_str("quota"))
        .ok()?
        .as_f64()? as u64;
    let usage = Reflect::get(&estimate, &JsValue::from_str("usage"))
        .ok()?
        .as_f64()? as u64;
    Some(QuotaInfo { quota, usage })
}

/// 列出所有已保存的世界（大厅用）
pub async fn list_saved_worlds() -> Result<Vec<WorldSummary>, StorageError> {
    let storage = web_sys::window()
        .ok_or(StorageError::NotSupported)?
        .navigator()
        .storage();
    let root_value = JsFuture::from(storage.get_directory())
        .await
        .map_err(js_error)?;
    let opfs_root: FileSystemDirectoryHandle = root_value.dyn_into().map_err(js_error)?;

    let create_dir = {
        let opts = FileSystemGetDirectoryOptions::new();
        opts.set_create(true);
        opts
    };
    let worlds_root = get_dir(&opfs_root, "voxweb", &create_dir).await?;

    let meta = load_meta(&worlds_root).await.unwrap_or_default();
    let mut worlds = meta.worlds;
    for world in &mut worlds {
        world.used_bytes = world_size_bytes(&worlds_root, &world.key)
            .await
            .unwrap_or(0);
    }
    worlds.sort_by_key(|w| std::cmp::Reverse(w.updated_at_ms));
    Ok(worlds)
}

/// 大厅一次性需要的存储概览：世界列表（含各自大小）+ 浏览器配额。
pub async fn storage_overview() -> Result<StorageOverview, StorageError> {
    let worlds = list_saved_worlds().await?;
    let quota = quota().await;
    Ok(StorageOverview { worlds, quota })
}

/// 删除指定世界（通过 key）
pub async fn delete_world_by_key(key: &str) -> Result<(), StorageError> {
    let storage = web_sys::window()
        .ok_or(StorageError::NotSupported)?
        .navigator()
        .storage();
    let root_value = JsFuture::from(storage.get_directory())
        .await
        .map_err(js_error)?;
    let opfs_root: FileSystemDirectoryHandle = root_value.dyn_into().map_err(js_error)?;

    let worlds_root = get_dir(&opfs_root, "voxweb", &{
        let opts = FileSystemGetDirectoryOptions::new();
        opts.set_create(true);
        opts
    })
    .await?;

    // 删除世界目录
    let opts = FileSystemRemoveOptions::new();
    opts.set_recursive(true);
    JsFuture::from(worlds_root.remove_entry_with_options(key, &opts))
        .await
        .map_err(js_error)?;

    // 更新 _meta.json
    let mut meta = load_meta(&worlds_root).await.unwrap_or_default();
    meta.worlds.retain(|w| w.key != key);
    write_text_file(
        &worlds_root,
        "_meta.json",
        &serde_json::to_string_pretty(&meta).unwrap(),
    )
    .await?;

    Ok(())
}

/// 生成世界目录 key：`<created_at_s>__<seed>`（秒级时间戳 + seed）
pub fn make_world_key(created_at_s: u64, seed: u64) -> String {
    format!("{created_at_s}__{seed}")
}

/// 解析 world_key 为 (created_at_s, seed)
/// 格式: `<created_at_s>__<seed>`，如 `1746000000__1234567890`
pub fn parse_world_key(key: &str) -> Option<(u64, u64)> {
    let (ts_str, seed_str) = key.split_once("__")?;
    let ts = ts_str.parse::<u64>().ok()?;
    let seed = seed_str.parse::<u64>().ok()?;
    Some((ts, seed))
}

/// 从 key 提取 seed（加载存档时用）
pub fn seed_from_key(key: &str) -> Option<u64> {
    parse_world_key(key).map(|(_, seed)| seed)
}

/// 从 key 提取创建时间并格式化为本地时间字符串
/// 输入: "1746000000__1234567890"
/// 输出: "2026-06-01 12:00:00"
pub fn format_creation_time(key: &str) -> String {
    let (ts_s, _) = parse_world_key(key).unwrap_or((0, 0));
    // 使用 js_sys::Date 将秒级时间戳转换为日期字符串
    let date = js_sys::Date::new(&JsValue::from_f64((ts_s as f64) * 1000.0));
    format!(
        "{}-{:02}-{:02} {:02}:{:02}:{:02}",
        date.get_full_year(),
        date.get_month() + 1,
        date.get_date(),
        date.get_hours(),
        date.get_minutes(),
        date.get_seconds()
    )
}

pub fn chunk_filename(pos: ChunkPos) -> String {
    format!("{}_{}.bin", coord_part(pos.x), coord_part(pos.z))
}

pub fn parse_chunk_filename(name: &str) -> Option<ChunkPos> {
    let stem = name.strip_suffix(".bin")?;
    let (x, z) = stem.split_once('_')?;
    Some(ChunkPos::new(parse_coord_part(x)?, parse_coord_part(z)?))
}

fn coord_part(v: i32) -> String {
    if v < 0 {
        format!("n{}", v.saturating_abs())
    } else {
        v.to_string()
    }
}

fn parse_coord_part(s: &str) -> Option<i32> {
    if let Some(rest) = s.strip_prefix('n') {
        rest.parse::<i32>().ok().map(|v| -v)
    } else {
        s.parse().ok()
    }
}

async fn get_dir(
    parent: &FileSystemDirectoryHandle,
    name: &str,
    opts: &FileSystemGetDirectoryOptions,
) -> Result<FileSystemDirectoryHandle, StorageError> {
    JsFuture::from(parent.get_directory_handle_with_options(name, opts))
        .await
        .map_err(js_error)?
        .dyn_into()
        .map_err(js_error)
}

async fn get_file(
    parent: &FileSystemDirectoryHandle,
    name: &str,
    create: bool,
) -> Result<FileSystemFileHandle, StorageError> {
    let opts = FileSystemGetFileOptions::new();
    opts.set_create(create);
    JsFuture::from(parent.get_file_handle_with_options(name, &opts))
        .await
        .map_err(js_error)?
        .dyn_into()
        .map_err(js_error)
}

async fn load_world_record(
    root: &FileSystemDirectoryHandle,
) -> Result<Option<WorldRecord>, StorageError> {
    let handle = match get_file(root, "world.json", false).await {
        Ok(handle) => handle,
        Err(StorageError::NotFound) => return Ok(None),
        Err(e) => return Err(e),
    };
    let file: Blob = JsFuture::from(handle.get_file())
        .await
        .map_err(js_error)?
        .dyn_into()
        .map_err(js_error)?;
    let buffer = JsFuture::from(file.array_buffer())
        .await
        .map_err(js_error)?;
    let bytes = Uint8Array::new(&buffer).to_vec();
    let text = String::from_utf8(bytes).map_err(|e| StorageError::Io(e.to_string()))?;
    serde_json::from_str(&text).map_err(|e| StorageError::Io(e.to_string()))
}

async fn update_meta(
    worlds_root: &FileSystemDirectoryHandle,
    record: &WorldRecord,
) -> Result<(), StorageError> {
    let mut meta = load_meta(worlds_root).await.unwrap_or_default();
    meta.worlds.retain(|w| w.key != record.key);
    meta.worlds.push(WorldSummary {
        key: record.key.clone(),
        display_name: record.display_name.clone(),
        updated_at_ms: record.updated_at_ms,
        used_bytes: 0,
    });
    meta.worlds
        .sort_by_key(|w| std::cmp::Reverse(w.updated_at_ms));
    write_text_file(
        worlds_root,
        "_meta.json",
        &serde_json::to_string_pretty(&meta).unwrap(),
    )
    .await
}

async fn load_meta(root: &FileSystemDirectoryHandle) -> Result<MetaRecord, StorageError> {
    let handle = get_file(root, "_meta.json", false).await?;
    let file: Blob = JsFuture::from(handle.get_file())
        .await
        .map_err(js_error)?
        .dyn_into()
        .map_err(js_error)?;
    let buffer = JsFuture::from(file.array_buffer())
        .await
        .map_err(js_error)?;
    let text = String::from_utf8(Uint8Array::new(&buffer).to_vec())
        .map_err(|e| StorageError::Io(e.to_string()))?;
    serde_json::from_str(&text).map_err(|e| StorageError::Io(e.to_string()))
}

async fn world_size_bytes(
    worlds_root: &FileSystemDirectoryHandle,
    key: &str,
) -> Result<u64, StorageError> {
    let root = get_dir(worlds_root, key, &{
        let opts = FileSystemGetDirectoryOptions::new();
        opts.set_create(false);
        opts
    })
    .await?;

    let mut total = match file_size(&root, "world.json").await {
        Ok(size) => size,
        Err(StorageError::NotFound) => 0,
        Err(e) => return Err(e),
    };
    let chunks_dir = match get_dir(&root, "chunks", &{
        let opts = FileSystemGetDirectoryOptions::new();
        opts.set_create(false);
        opts
    })
    .await
    {
        Ok(dir) => dir,
        Err(StorageError::NotFound) => return Ok(total),
        Err(e) => return Err(e),
    };
    total = total.saturating_add(chunk_file_sizes(&chunks_dir).await?.values().sum::<u64>());
    Ok(total)
}

async fn chunk_file_sizes(
    chunks_dir: &FileSystemDirectoryHandle,
) -> Result<HashMap<ChunkPos, u64>, StorageError> {
    let mut out = HashMap::new();
    let iter = chunks_dir.keys();
    loop {
        let next = JsFuture::from(iter.next().map_err(js_error)?)
            .await
            .map_err(js_error)?;
        let done = Reflect::get(&next, &JsValue::from_str("done"))
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if done {
            break;
        }
        let Some(name) = Reflect::get(&next, &JsValue::from_str("value"))
            .ok()
            .and_then(|v| v.as_string())
        else {
            continue;
        };
        let Some(pos) = parse_chunk_filename(&name) else {
            continue;
        };
        match file_size(chunks_dir, &name).await {
            Ok(size) => {
                out.insert(pos, size);
            }
            Err(StorageError::NotFound) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

async fn file_size(dir: &FileSystemDirectoryHandle, name: &str) -> Result<u64, StorageError> {
    let handle = get_file(dir, name, false).await?;
    let file: Blob = JsFuture::from(handle.get_file())
        .await
        .map_err(js_error)?
        .dyn_into()
        .map_err(js_error)?;
    Ok(file.size().max(0.0) as u64)
}

pub fn format_storage_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while unit + 1 < UNITS.len() && value >= 100.0 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

async fn write_text_file(
    dir: &FileSystemDirectoryHandle,
    name: &str,
    text: &str,
) -> Result<(), StorageError> {
    write_bytes_file(dir, name, text.as_bytes()).await
}

async fn write_bytes_file(
    dir: &FileSystemDirectoryHandle,
    name: &str,
    bytes: &[u8],
) -> Result<(), StorageError> {
    let handle = get_file(dir, name, true).await?;
    let stream: FileSystemWritableFileStream = JsFuture::from(handle.create_writable())
        .await
        .map_err(js_error)?
        .dyn_into()
        .map_err(js_error)?;
    JsFuture::from(stream.write_with_u8_array(bytes).map_err(js_error)?)
        .await
        .map_err(js_error)?;
    let writable: web_sys::WritableStream = stream.unchecked_into();
    JsFuture::from(writable.close()).await.map_err(js_error)?;
    Ok(())
}

fn js_error(value: JsValue) -> StorageError {
    let name = Reflect::get(&value, &JsValue::from_str("name"))
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_default();
    match name.as_str() {
        "NotFoundError" => StorageError::NotFound,
        "QuotaExceededError" => StorageError::QuotaExceeded,
        _ => StorageError::Io(
            value
                .as_string()
                .or_else(|| js_sys::JSON::stringify(&value).ok().map(String::from))
                .unwrap_or_else(|| format!("{value:?}")),
        ),
    }
}

fn now_ms() -> f64 {
    js_sys::Date::now()
}

#[wasm_bindgen::prelude::wasm_bindgen]
pub async fn voxweb_debug_quota() -> JsValue {
    match quota().await {
        Some(q) => {
            let obj = js_sys::Object::new();
            let _ = Reflect::set(&obj, &"quota".into(), &JsValue::from_f64(q.quota as f64));
            let _ = Reflect::set(&obj, &"usage".into(), &JsValue::from_f64(q.usage as f64));
            obj.into()
        }
        None => JsValue::NULL,
    }
}

#[allow(dead_code)]
fn _array_from_bytes(bytes: &[u8]) -> Array {
    let a = Array::new();
    a.push(&Uint8Array::from(bytes).into());
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_filename_roundtrip() {
        for pos in [
            ChunkPos::new(0, 0),
            ChunkPos::new(-1, 2),
            ChunkPos::new(15, -23),
        ] {
            assert_eq!(parse_chunk_filename(&chunk_filename(pos)), Some(pos));
        }
    }

    #[test]
    fn invalid_chunk_filename_rejected() {
        assert_eq!(parse_chunk_filename("1.bin"), None);
        assert_eq!(parse_chunk_filename("x_1.bin"), None);
        assert_eq!(parse_chunk_filename("1_2.txt"), None);
    }

    #[test]
    fn world_key_format() {
        // 新格式: <timestamp_s>__<seed>
        assert_eq!(make_world_key(1000, 7), "1000__7");
        assert_eq!(
            make_world_key(1746000000, 1234567890),
            "1746000000__1234567890"
        );
    }

    #[test]
    fn parse_world_key_roundtrip() {
        let key = make_world_key(1746000000, 1234567890);
        let (ts, seed) = parse_world_key(&key).unwrap();
        assert_eq!(ts, 1746000000);
        assert_eq!(seed, 1234567890);
    }

    #[test]
    fn parse_world_key_invalid() {
        assert_eq!(parse_world_key("invalid"), None);
        assert_eq!(parse_world_key("abc__def"), None);
    }

    #[test]
    fn format_storage_bytes_keeps_one_decimal() {
        assert_eq!(format_storage_bytes(0), "0.0 B");
        assert_eq!(format_storage_bytes(42), "42.0 B");
        assert_eq!(format_storage_bytes(100), "0.1 KB");
        assert_eq!(format_storage_bytes(12 * 1024), "12.0 KB");
        assert_eq!(format_storage_bytes(100 * 1024), "0.1 MB");
        assert_eq!(format_storage_bytes(12 * 1024 * 1024), "12.0 MB");
    }
}
