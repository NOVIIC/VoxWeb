//! 持久化触发逻辑。
//!
//! Host 角色在每 N 个 tick 或退出时将 dirty chunks 交给 client 异步写入 IndexedDB。
//! 具体 IndexedDB 读写在 client::storage 中执行。

use std::collections::HashSet;

use voxweb_core::chunk::ChunkPos;

/// 持久化管理器（Phase 5 实现）。
#[derive(Default)]
pub struct PersistenceManager {
    /// 自上次 flush 后被修改过的 ChunkPos
    pub dirty_chunks: HashSet<ChunkPos>,
    /// 下次 flush 的 tick 计数
    pub next_flush_tick: u64,
}

impl PersistenceManager {
    pub fn new() -> Self {
        Self {
            dirty_chunks: HashSet::new(),
            next_flush_tick: 300, // 5 秒 @ 60Hz
        }
    }

    /// 标记一个 Chunk 为 dirty（方块被修改后调用）。
    pub fn mark_dirty(&mut self, pos: ChunkPos) {
        self.dirty_chunks.insert(pos);
    }

    /// 检查是否到达 flush 时机。
    pub fn should_flush(&self, current_tick: u64) -> bool {
        !self.dirty_chunks.is_empty() && current_tick >= self.next_flush_tick
    }

    /// 取出所有 dirty chunks 并重置定时器。
    pub fn take_dirty(&mut self) -> Vec<ChunkPos> {
        let v: Vec<ChunkPos> = self.dirty_chunks.drain().collect();
        v
    }
}
