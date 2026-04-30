//! IndexedDB 异步读写包装。
//!
//! Host 角色使用 IndexedDB 存储世界数据：
//! - 启动时从 IndexedDB 加载历史世界
//! - 周期性 flush dirty chunks
//! - 退出前保存

/// IndexedDB 存储管理器。
pub struct IndexedDbStorage {
    // Phase 5: 持有 idb::Database 句柄
}

impl IndexedDbStorage {
    /// 打开（或创建）IndexedDB 数据库。
    pub async fn open(_db_name: &str) -> Result<Self, String> {
        // Phase 5: 调用 idb::Factory::open
        Ok(Self {})
    }

    /// 按 ChunkPos 读取一个 chunk。
    pub async fn load_chunk(&self, _world_id: &str, _x: i32, _z: i32) -> Option<Vec<u8>> {
        // Phase 5 实现
        None
    }

    /// 写入一个 chunk 的序列化数据。
    pub async fn save_chunk(&self, _world_id: &str, _x: i32, _z: i32, _data: &[u8]) {
        // Phase 5 实现
    }
}
