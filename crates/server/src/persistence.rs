//! 持久化触发逻辑。
//!
//! `server` crate 不直接碰 OPFS；它只维护 dirty / in-flight / retry 状态，
//! 让 `client::storage` 在浏览器侧异步完成真实读写。这样 server 仍保持平台无关，
//! 原生单元测试也能覆盖持久化调度行为。

use std::collections::{HashMap, HashSet};

use voxweb_core::chunk::ChunkPos;

/// Phase 8 默认每 3 秒 flush 一次（60Hz 逻辑 tick）。
pub const DEFAULT_FLUSH_INTERVAL_TICKS: u64 = 180;

/// dirty chunk 的 snapshot/commit/retry 管理器。
#[derive(Clone, Debug)]
pub struct PersistenceManager {
    dirty: HashSet<ChunkPos>,
    in_flight: HashSet<ChunkPos>,
    failure_count: HashMap<ChunkPos, u32>,
    backoff_until_tick: HashMap<ChunkPos, u64>,
    next_flush_tick: u64,
    flush_interval_ticks: u64,
    quota_pause_dirty: bool,
}

impl PersistenceManager {
    pub fn new() -> Self {
        Self {
            dirty: HashSet::new(),
            in_flight: HashSet::new(),
            failure_count: HashMap::new(),
            backoff_until_tick: HashMap::new(),
            next_flush_tick: DEFAULT_FLUSH_INTERVAL_TICKS,
            flush_interval_ticks: DEFAULT_FLUSH_INTERVAL_TICKS,
            quota_pause_dirty: false,
        }
    }

    /// 标记一个 chunk 需要落盘。配额到达红线时可暂停接受新 dirty。
    pub fn mark_dirty(&mut self, pos: ChunkPos) {
        if !self.quota_pause_dirty {
            self.dirty.insert(pos);
        }
    }

    pub fn set_quota_pause_dirty(&mut self, paused: bool) {
        self.quota_pause_dirty = paused;
    }

    pub fn quota_paused(&self) -> bool {
        self.quota_pause_dirty
    }

    pub fn dirty_len(&self) -> usize {
        self.dirty.len()
    }

    pub fn in_flight_len(&self) -> usize {
        self.in_flight.len()
    }

    pub fn is_dirty_or_in_flight(&self, pos: ChunkPos) -> bool {
        self.dirty.contains(&pos) || self.in_flight.contains(&pos)
    }

    /// 到达周期且仍有可 flush 项时返回 true。
    pub fn should_flush(&self, current_tick: u64) -> bool {
        !self.dirty.is_empty() && current_tick >= self.next_flush_tick
    }

    /// 将 dirty 移入 in-flight，但不视为成功。失败时调用 `record_flush_failure` 放回 dirty。
    pub fn snapshot_dirty(&mut self, limit: usize, current_tick: u64) -> Vec<ChunkPos> {
        if limit == 0 || self.dirty.is_empty() {
            return Vec::new();
        }

        let mut out = Vec::new();
        let candidates: Vec<ChunkPos> = self.dirty.iter().copied().collect();
        for pos in candidates {
            if out.len() >= limit {
                break;
            }
            if self
                .backoff_until_tick
                .get(&pos)
                .is_some_and(|deadline| current_tick < *deadline)
            {
                continue;
            }
            self.dirty.remove(&pos);
            self.in_flight.insert(pos);
            out.push(pos);
        }
        self.next_flush_tick = current_tick.saturating_add(self.flush_interval_ticks);
        out
    }

    /// 写入成功，清理 in-flight 与失败计数。
    pub fn commit_flushed(&mut self, positions: &[ChunkPos]) {
        for pos in positions {
            self.in_flight.remove(pos);
            self.failure_count.remove(pos);
            self.backoff_until_tick.remove(pos);
        }
    }

    /// 写入失败，把 chunk 放回 dirty，并根据失败次数做退避。
    pub fn record_flush_failure(&mut self, positions: &[ChunkPos], current_tick: u64) {
        for pos in positions {
            self.in_flight.remove(pos);
            self.dirty.insert(*pos);
            let count = self.failure_count.entry(*pos).or_insert(0);
            *count = count.saturating_add(1);
            let delay = match *count {
                0..=2 => 60,
                3..=9 => 300,
                _ => 1800,
            };
            self.backoff_until_tick
                .insert(*pos, current_tick.saturating_add(delay));
        }
    }

    /// 一次性取走所有 dirty。主路径优先用 snapshot/commit。
    pub fn take_dirty(&mut self) -> Vec<ChunkPos> {
        self.dirty.drain().collect()
    }
}

impl Default for PersistenceManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_commit_clears_in_flight() {
        let mut p = PersistenceManager::new();
        let a = ChunkPos::new(1, 2);
        p.mark_dirty(a);
        let snap = p.snapshot_dirty(8, 60);
        assert_eq!(snap, vec![a]);
        assert_eq!(p.dirty_len(), 0);
        assert_eq!(p.in_flight_len(), 1);
        p.commit_flushed(&snap);
        assert_eq!(p.in_flight_len(), 0);
    }

    #[test]
    fn failure_returns_to_dirty_with_backoff() {
        let mut p = PersistenceManager::new();
        let a = ChunkPos::new(0, 0);
        p.mark_dirty(a);
        let snap = p.snapshot_dirty(8, 60);
        p.record_flush_failure(&snap, 60);
        assert_eq!(p.dirty_len(), 1);
        assert!(p.snapshot_dirty(8, 61).is_empty());
        assert_eq!(p.snapshot_dirty(8, 121), vec![a]);
    }

    #[test]
    fn quota_pause_blocks_new_dirty() {
        let mut p = PersistenceManager::new();
        p.set_quota_pause_dirty(true);
        p.mark_dirty(ChunkPos::new(5, 5));
        assert_eq!(p.dirty_len(), 0);
    }

    #[test]
    fn default_flush_interval_is_three_seconds_at_60hz() {
        let mut p = PersistenceManager::new();
        p.mark_dirty(ChunkPos::new(0, 0));
        assert!(!p.should_flush(DEFAULT_FLUSH_INTERVAL_TICKS - 1));
        assert!(p.should_flush(DEFAULT_FLUSH_INTERVAL_TICKS));
    }
}
