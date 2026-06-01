//! OPFS（Origin Private File System）异步读写包装。
//!
//! Phase 8 默认走 Variant A：主线程 async OPFS。这里不保存 `Chunk` 本体，
//! 只保存 `voxweb_core::chunk::encode` 产出的字节，调用方负责 encode/decode。

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
}

#[allow(async_fn_in_trait)]
pub trait WorldStorage {
    async fn open(room_id: &str, seed: u64) -> Result<Self, StorageError>
    where
        Self: Sized;
    async fn list_chunks(&self) -> Result<Vec<ChunkPos>, StorageError>;
    async fn load_chunk(&self, pos: ChunkPos) -> Result<Option<Vec<u8>>, StorageError>;
    async fn save_chunks(&self, items: Vec<(ChunkPos, Vec<u8>)>) -> Result<(), StorageError>;
    async fn delete_world(&self) -> Result<(), StorageError>;
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

    async fn open_impl(room_id: &str, seed: u64) -> Result<Self, StorageError> {
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
        let world_key = make_world_key(room_id, seed);
        let root = get_dir(&worlds_root, &world_key, &create_dir).await?;
        let chunks_dir = get_dir(&root, "chunks", &create_dir).await?;

        let now = now_ms() as u64;
        let record = match load_world_record(&root).await? {
            Some(record) if record.storage_version > STORAGE_VERSION => {
                return Err(StorageError::NeedsUpgrade {
                    found: record.storage_version,
                    supported: STORAGE_VERSION,
                });
            }
            Some(mut record) => {
                record.updated_at_ms = now;
                record
            }
            None => WorldRecord {
                key: world_key.clone(),
                room_id: room_id.to_string(),
                seed: seed.to_string(),
                display_name: room_id.to_string(),
                created_at_ms: now,
                updated_at_ms: now,
                storage_version: STORAGE_VERSION,
                protocol_version: PROTOCOL_VERSION,
            },
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
            world_key,
        })
    }

    pub async fn request_persistence() -> Option<bool> {
        let storage = web_sys::window()?.navigator().storage();
        let promise = storage.persist().ok()?;
        JsFuture::from(promise).await.ok()?.as_bool()
    }
}

impl WorldStorage for OpfsStorage {
    async fn open(room_id: &str, seed: u64) -> Result<Self, StorageError> {
        Self::open_impl(room_id, seed).await
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
        for (pos, bytes) in items {
            write_bytes_file(&self.chunks_dir, &chunk_filename(pos), &bytes).await?;
        }
        Ok(())
    }

    async fn delete_world(&self) -> Result<(), StorageError> {
        let opts = FileSystemRemoveOptions::new();
        opts.set_recursive(true);
        JsFuture::from(
            self.worlds_root
                .remove_entry_with_options(&self.world_key, &opts),
        )
        .await
        .map_err(js_error)?;
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

pub fn make_world_key(room_id: &str, seed: u64) -> String {
    format!("{}__{seed}", sanitize_key(room_id))
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

fn sanitize_key(s: &str) -> String {
    let trimmed = s.trim();
    let base = if trimmed.is_empty() { "local" } else { trimmed };
    base.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
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
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0)
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
    fn local_world_key_uses_local_prefix() {
        assert_eq!(make_world_key("", 7), "local__7");
        assert_eq!(make_world_key("room/a", 7), "room_a__7");
    }
}
