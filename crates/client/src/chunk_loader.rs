//! 区块滚动加载：根据玩家相机位置维护"应加载"chunk 集合，
//! diff 出新增与移除，触发 Server 生成 / Renderer 卸载 / MeshJobQueue 入队。
//!
//! Phase 2：每次 update 在玩家跨 chunk 边界时执行；
//! 边界 chunk 通过 MeshPriority::Low 重新入队，触发跨区块剔除生效。

use std::collections::HashSet;

use glam::Vec3;
use voxweb_core::chunk::Position;
use voxweb_core::{CHUNK_X, CHUNK_Z, ChunkPos};
use voxweb_render::Renderer;
use voxweb_server::Server;

use crate::mesh_jobs::{MeshJobQueue, MeshPriority};

pub struct ChunkLoader {
    pub render_distance: i32,
    pub unload_buffer: i32,
    pub loaded: HashSet<ChunkPos>,
    requested: HashSet<ChunkPos>,
    last_center: Option<ChunkPos>,
}

impl ChunkLoader {
    pub fn new(render_distance: u32) -> Self {
        Self {
            render_distance: render_distance as i32,
            unload_buffer: 3,
            loaded: HashSet::new(),
            requested: HashSet::new(),
            last_center: None,
        }
    }

    /// 强制下一次 update 重新计算（用于初始化或渲染距离变更）。
    pub fn invalidate(&mut self) {
        self.last_center = None;
    }

    /// Phase 6：暂停菜单设置渲染距离后同步生效。
    /// 调用此方法后 [`ChunkLoader::update`] 会在下一帧重算 desired 集合。
    pub fn set_render_distance(&mut self, render_distance: u32) {
        let new_r = render_distance as i32;
        if self.render_distance != new_r {
            self.render_distance = new_r;
            self.invalidate();
        }
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

    /// Remote 模式的区块滚动加载。
    ///
    /// Remote 没有权威地形生成权，只维护本地缓存集合：缺失 chunk 通过
    /// `ClientMessage::FieldRequest` 向 Host 请求；Host 回传 `FieldSnapshot` 后，
    /// 调用 [`ChunkLoader::mark_loaded`] 把它从 in-flight 集合移到 loaded 集合。
    pub fn update_remote(
        &mut self,
        camera_pos: Vec3,
        server: &mut Server,
        mesh_jobs: &mut MeshJobQueue,
        renderer: &mut Renderer,
    ) -> Vec<ChunkPos> {
        let center = chunk_pos_of(camera_pos);
        let center_changed = Some(center) != self.last_center;
        if center_changed {
            self.last_center = Some(center);
        }

        let mut requests = Vec::new();
        if center_changed {
            let r = self.render_distance;
            let desired: HashSet<ChunkPos> = (-r..=r)
                .flat_map(|dx| (-r..=r).map(move |dz| ChunkPos::new(center.x + dx, center.z + dz)))
                .collect();

            for pos in desired.difference(&self.loaded).copied() {
                if self.requested.insert(pos) {
                    requests.push(pos);
                }
            }
        }

        // Remote 也要卸载远离视距的本地 world/mesh，否则长途移动会无限增长。
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

        // 已经飞出缓冲范围但尚未返回的请求直接忘掉；若快照稍后到达，
        // 下一次 update_remote 会按当前位置把它清理出缓存。
        let stale_requests: Vec<ChunkPos> = self
            .requested
            .iter()
            .copied()
            .filter(|p| chebyshev_distance(*p, center) > unload_r)
            .collect();
        for pos in stale_requests {
            self.requested.remove(&pos);
        }

        requests
    }

    /// 标记一个 Remote chunk 已收到完整快照。
    pub fn mark_loaded(&mut self, pos: ChunkPos) {
        self.loaded.insert(pos);
        self.requested.remove(&pos);
    }

    /// 标记一个正方形范围的 chunk 已经有快照在路上。
    ///
    /// Host 当前仍会在玩家加入时主动推送出生点附近的 bootstrap 快照。
    /// Remote 端把这批 chunk 记为 in-flight，后续按视距请求时只补缺口，
    /// 避免初始阶段重复传同一批地图数据。
    pub fn mark_requested_square(&mut self, center: ChunkPos, radius: i32) {
        let r = radius.max(0);
        for dx in -r..=r {
            for dz in -r..=r {
                let pos = ChunkPos::new(center.x + dx, center.z + dz);
                if !self.loaded.contains(&pos) {
                    self.requested.insert(pos);
                }
            }
        }
    }
}

/// 世界坐标 → 所在 chunk 的 ChunkPos。
pub fn chunk_pos_of(world_pos: Vec3) -> ChunkPos {
    let x = (world_pos.x as i32).div_euclid(CHUNK_X as i32);
    let z = (world_pos.z as i32).div_euclid(CHUNK_Z as i32);
    ChunkPos::new(x, z)
}

/// 一次方块变更影响的 chunk 集合：自身所在 chunk + 若位于 x/z 边界则对应横向邻居。
/// Y 不分 chunk，所以不考虑上下邻居。最多返回 4 个（同时在 x 与 z 双边界的角点）。
pub fn affected_chunks(pos: Position) -> Vec<ChunkPos> {
    let cp = ChunkPos::new(
        pos.x.div_euclid(CHUNK_X as i32),
        pos.z.div_euclid(CHUNK_Z as i32),
    );
    let local_x = pos.x.rem_euclid(CHUNK_X as i32);
    let local_z = pos.z.rem_euclid(CHUNK_Z as i32);
    let mut out = Vec::with_capacity(4);
    out.push(cp);
    // x 方向边界：local==0 → 影响 -x 邻居；local==15 → 影响 +x 邻居
    if local_x == 0 {
        out.push(ChunkPos::new(cp.x - 1, cp.z));
    } else if local_x == CHUNK_X as i32 - 1 {
        out.push(ChunkPos::new(cp.x + 1, cp.z));
    }
    if local_z == 0 {
        out.push(ChunkPos::new(cp.x, cp.z - 1));
    } else if local_z == CHUNK_Z as i32 - 1 {
        out.push(ChunkPos::new(cp.x, cp.z + 1));
    }
    // 角点情况（同时跨 x 和 z 边界）也补上斜对角 chunk
    if out.len() == 3 {
        let dx = out[1].x - cp.x;
        let dz = out[2].z - cp.z;
        out.push(ChunkPos::new(cp.x + dx, cp.z + dz));
    }
    out
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
        assert_eq!(
            chunk_pos_of(Vec3::new(-1.0, 0.0, -1.0)),
            ChunkPos::new(-1, -1)
        );
        assert_eq!(chunk_pos_of(Vec3::new(0.0, 0.0, 0.0)), ChunkPos::new(0, 0));
        assert_eq!(chunk_pos_of(Vec3::new(16.0, 0.0, 0.0)), ChunkPos::new(1, 0));
        assert_eq!(chunk_pos_of(Vec3::new(15.9, 0.0, 0.0)), ChunkPos::new(0, 0));
    }

    #[test]
    fn chebyshev_basics() {
        assert_eq!(
            chebyshev_distance(ChunkPos::new(0, 0), ChunkPos::new(0, 0)),
            0
        );
        assert_eq!(
            chebyshev_distance(ChunkPos::new(3, 4), ChunkPos::new(0, 0)),
            4
        );
        assert_eq!(
            chebyshev_distance(ChunkPos::new(-2, 5), ChunkPos::new(1, 1)),
            4
        );
    }

    #[test]
    fn priority_classification() {
        let c = ChunkPos::new(0, 0);
        assert_eq!(
            priority_for_distance(ChunkPos::new(0, 0), c),
            MeshPriority::Critical
        );
        assert_eq!(
            priority_for_distance(ChunkPos::new(1, 0), c),
            MeshPriority::High
        );
        assert_eq!(
            priority_for_distance(ChunkPos::new(0, -1), c),
            MeshPriority::High
        );
        assert_eq!(
            priority_for_distance(ChunkPos::new(2, 2), c),
            MeshPriority::Medium
        );
        assert_eq!(
            priority_for_distance(ChunkPos::new(5, 3), c),
            MeshPriority::Medium
        );
    }

    #[test]
    fn affected_chunks_interior_returns_one() {
        let pos = Position::new(5, 64, 7);
        let v = affected_chunks(pos);
        assert_eq!(v, vec![ChunkPos::new(0, 0)]);
    }

    #[test]
    fn affected_chunks_x_boundary_returns_two() {
        // local_x = 0 → 还影响 (-1, 0)
        let v = affected_chunks(Position::new(0, 64, 5));
        assert_eq!(v.len(), 2);
        assert!(v.contains(&ChunkPos::new(0, 0)));
        assert!(v.contains(&ChunkPos::new(-1, 0)));

        // local_x = 15 → 还影响 (1, 0)
        let v = affected_chunks(Position::new(15, 64, 5));
        assert_eq!(v.len(), 2);
        assert!(v.contains(&ChunkPos::new(0, 0)));
        assert!(v.contains(&ChunkPos::new(1, 0)));
    }

    #[test]
    fn affected_chunks_corner_returns_four() {
        // 角点 (0, _, 0) → 自身 + (-1, 0) + (0, -1) + (-1, -1)
        let v = affected_chunks(Position::new(0, 64, 0));
        assert_eq!(v.len(), 4);
        assert!(v.contains(&ChunkPos::new(0, 0)));
        assert!(v.contains(&ChunkPos::new(-1, 0)));
        assert!(v.contains(&ChunkPos::new(0, -1)));
        assert!(v.contains(&ChunkPos::new(-1, -1)));
    }

    #[test]
    fn affected_chunks_handles_negative_coords() {
        // pos.x = -1 → chunk_x = -1, local_x = 15 → 还影响 (0, *)
        let v = affected_chunks(Position::new(-1, 64, 5));
        assert_eq!(v.len(), 2);
        assert!(v.contains(&ChunkPos::new(-1, 0)));
        assert!(v.contains(&ChunkPos::new(0, 0)));
    }
}
