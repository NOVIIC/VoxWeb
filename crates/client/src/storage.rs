//! OPFS（Origin Private File System）异步读写包装。
//!
//! Host 与 Local-Only 角色使用 OPFS 存储世界数据：
//! - 启动时拉文件名清单 + prime 出生点周围 chunk
//! - 运行时按需 load 玩家走到的区域
//! - 周期性 flush dirty chunk
//! - `pagehide` 退出时尽力 flush
//!
//! 完整设计见 `docs/features/persistence.md`。Phase 2 仅占位以保证模块名稳定；
//! Phase 5 由 `WorldStorage` trait + OPFS 实现取代。

use voxweb_core::chunk::ChunkPos;

/// 持久化层错误（Phase 5 实装时按 OPFS / 浏览器异常补全分支）。
#[derive(Debug)]
pub enum StorageError {
    /// 浏览器不支持 OPFS（理论上 wasm 加载前的能力检测已拦截）
    NotSupported,
    /// 文件/目录不存在
    NotFound,
    /// 配额耗尽
    QuotaExceeded,
    /// 包装底层 DOMException
    Io(String),
}

/// `navigator.storage.estimate()` 返回的配额信息（UI 显示用）。
#[derive(Copy, Clone, Debug)]
pub struct QuotaInfo {
    pub quota: u64,
    pub usage: u64,
}

/// OPFS 存储句柄。一个实例对应一个 (room_id, seed) 世界。
///
/// Phase 5 实装后字段将包含：
/// - `root: web_sys::FileSystemDirectoryHandle`           opfs:/voxweb/<world_key>/
/// - `chunks_dir: web_sys::FileSystemDirectoryHandle`     .../chunks/
/// - `world_key: String`
pub struct OpfsStorage {
    // Phase 5: 真实字段
}

impl OpfsStorage {
    /// 打开（或创建）某个世界的 OPFS 目录。
    pub async fn open(_room_id: &str, _seed: u64) -> Result<Self, StorageError> {
        // Phase 5: navigator.storage.getDirectory() → getDirectoryHandle 递归到 world dir
        Ok(Self {})
    }

    /// 拉取已存档 chunk 的文件名清单（不读内容）。供启动时构建 known_persisted 集合。
    pub async fn list_chunks(&self) -> Result<Vec<ChunkPos>, StorageError> {
        // Phase 5: 异步迭代 chunks_dir.entries()，解析 "<cx>_<cz>.bin"
        Ok(Vec::new())
    }

    /// 按 ChunkPos 读取单个 chunk 的 encoded 字节（未 decode）。
    /// 调用方（client）拿到字节后用 voxweb_core::chunk::decode 还原。
    pub async fn load_chunk(&self, _pos: ChunkPos) -> Result<Option<Vec<u8>>, StorageError> {
        // Phase 5: getFileHandle(name).getFile().arrayBuffer()
        Ok(None)
    }

    /// 批量写入若干 encoded chunk。失败时调用方负责把失败的 ChunkPos 还回 dirty 集合。
    pub async fn save_chunks(
        &self,
        _items: Vec<(ChunkPos, Vec<u8>)>,
    ) -> Result<(), StorageError> {
        // Phase 5: 对每个 item 走 createWritable / write / close
        Ok(())
    }

    /// 删除整个世界（chunks 目录 + world.json）。
    pub async fn delete_world(&self) -> Result<(), StorageError> {
        // Phase 5: removeEntry({recursive: true})；旧 Safari 走逐文件 fallback
        Ok(())
    }

    /// 当前 origin 的存储配额；浏览器不支持时返回 None。
    pub async fn quota(&self) -> Option<QuotaInfo> {
        // Phase 5: navigator.storage.estimate()
        None
    }
}
