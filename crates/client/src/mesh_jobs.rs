//! 网格化任务调度：优先级队列 + 分帧 budget。
//!
//! Phase 2：4 档优先级（Critical / High / Medium / Low）+ 4 个 VecDeque + pending HashSet。
//! 每帧 `run_until_budget` 从最高优先级开始 pop，调用 `chunk_mesh::generate_with_neighbors`
//! 跑跨区块剔除，结果上传 Renderer。`performance.now()` 监控耗时超 budget 退出。

use std::collections::{HashMap, VecDeque};

use voxweb_core::ChunkPos;
use voxweb_render::Renderer;
use voxweb_render::chunk_mesh;
use voxweb_server::Server;

/// 网格化任务优先级（数值越小越先跑）。
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

/// 单次 `run_until_budget` 的 CPU 统计。HUD 显示的是上一帧跑出的这一批数据。
#[derive(Copy, Clone, Debug, Default)]
pub struct MeshRunStats {
    pub elapsed_ms: f32,
    pub jobs_processed: u32,
    pub vertices_uploaded: u32,
    pub indices_uploaded: u32,
    pub phase2_vertices: u32,
}

impl MeshRunStats {
    pub fn greedy_reduction_percent(&self) -> Option<f32> {
        if self.phase2_vertices == 0 {
            return None;
        }
        let after = self.vertices_uploaded as f32;
        let before = self.phase2_vertices as f32;
        Some(((1.0 - after / before) * 100.0).max(0.0))
    }
}

/// 4 优先级队列 + pending map 防重。
pub struct MeshJobQueue {
    queues: [VecDeque<ChunkPos>; MeshPriority::COUNT],
    pending: HashMap<ChunkPos, MeshPriority>,
}

impl Default for MeshJobQueue {
    fn default() -> Self {
        Self {
            queues: [const { VecDeque::new() }; MeshPriority::COUNT],
            pending: HashMap::new(),
        }
    }
}

impl MeshJobQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// 把 pos 加入指定优先级队列。若已在队列里，允许升级到更高优先级。
    pub fn enqueue(&mut self, pos: ChunkPos, priority: MeshPriority) {
        match self.pending.get(&pos).copied() {
            None => {
                self.pending.insert(pos, priority);
                self.queues[priority as usize].push_back(pos);
            }
            Some(old) if (priority as usize) < (old as usize) => {
                self.queues[old as usize].retain(|p| *p != pos);
                self.pending.insert(pos, priority);
                self.queues[priority as usize].push_back(pos);
            }
            Some(_) => {}
        }
    }

    /// 从队列中移除 pos（卸载 chunk 时调用）。
    pub fn cancel(&mut self, pos: ChunkPos) {
        if self.pending.remove(&pos).is_some() {
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
        self.pending.len()
    }

    /// 是否所有队列都为空。
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// 在给定时间预算内执行尽量多的网格化任务。
    /// `now_ms` 是返回当前 performance.now() 毫秒值的闭包（便于测试与平台抽象）。
    pub fn run_until_budget(
        &mut self,
        budget_ms: f32,
        server: &Server,
        renderer: &mut Renderer,
        now_ms: &dyn Fn() -> f64,
    ) -> MeshRunStats {
        let start = now_ms();
        let mut stats = MeshRunStats::default();
        loop {
            if (now_ms() - start) as f32 >= budget_ms {
                break;
            }
            let Some(pos) = self.pop_highest() else {
                break;
            };
            let Some(chunk) = server.world.chunks.get(&pos) else {
                // chunk 已被卸载，跳过
                continue;
            };
            let mesh = chunk_mesh::generate_with_neighbors(chunk, pos, &|wx, wy, wz| {
                server.world.get_block_world(wx, wy, wz)
            });
            stats.jobs_processed = stats.jobs_processed.saturating_add(1);
            stats.vertices_uploaded = stats.vertices_uploaded.saturating_add(mesh.vertex_count());
            stats.indices_uploaded = stats.indices_uploaded.saturating_add(mesh.index_count());
            stats.phase2_vertices = stats
                .phase2_vertices
                .saturating_add(mesh.phase2_vertex_count());
            renderer.upload_chunk_mesh(pos, &mesh);
        }
        stats.elapsed_ms = (now_ms() - start) as f32;
        stats
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
        q.enqueue(ChunkPos::new(0, 0), MeshPriority::Critical); // 重复，但应升级
        assert_eq!(q.len(), 1);
        // Phase 7：重复入队时允许升级优先级，玩家附近 chunk 不会被旧 Low/Medium 卡住。
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
