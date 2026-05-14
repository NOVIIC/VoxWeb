//! 世界状态管理：Chunk 表、地形生成器、方块读写。
//!
//! Phase 2：World 持有 TerrainGenerator；区块按需生成 + 卸载；
//! 提供世界坐标查询接口供网格化跨区块剔除使用。
//! Phase 5：set_block 实装 dirty 标记；持久化层（Phase 8）会从 drain_dirty 取出待写。

use std::collections::{HashMap, HashSet};

use voxweb_core::block::BlockID;
use voxweb_core::chunk::{CHUNK_X, CHUNK_Y, CHUNK_Z, Chunk, ChunkPos, Position};

use crate::terrain::TerrainGenerator;

/// 世界状态。Phase 2 仅含 chunk 表 + 地形生成器；Phase 5 起加 dirty_chunks。
pub struct World {
    pub seed: u64,
    pub chunks: HashMap<ChunkPos, Chunk>,
    pub terrain: TerrainGenerator,
    /// 自创建以来的总 tick 数（Phase 2 仅累加，Phase 5 起驱动 Server::broadcast_tick）
    pub tick_count: u64,
    /// Phase 5：被 set_block 修改过的 ChunkPos 集合。
    /// 持久化层（Phase 8）通过 drain_dirty 取出待写；Phase 5 暂不消费。
    pub dirty_chunks: HashSet<ChunkPos>,
}

impl World {
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            chunks: HashMap::new(),
            terrain: TerrainGenerator::new(seed),
            tick_count: 0,
            dirty_chunks: HashSet::new(),
        }
    }

    /// 若 chunk 未生成则调地形生成器生成并插入。已存在则跳过。
    /// Phase 2 的 chunk 入口点（由 client::chunk_loader 调用）。
    pub fn ensure_chunk_generated(&mut self, pos: ChunkPos) {
        if self.chunks.contains_key(&pos) {
            return;
        }
        let chunk = self.terrain.generate_chunk(pos);
        self.chunks.insert(pos, chunk);
    }

    /// 卸载（移除）一个 chunk。Phase 5 引入持久化后会先把 dirty 数据 flush 再移除。
    pub fn unload_chunk(&mut self, pos: ChunkPos) {
        self.chunks.remove(&pos);
    }

    /// 世界坐标方块查询；chunk 未加载或 y 越界一律返回 AIR。
    /// 供 chunk_mesh::generate_with_neighbors 的回调使用。
    pub fn get_block_world(&self, wx: i32, wy: i32, wz: i32) -> BlockID {
        if wy < 0 || wy >= CHUNK_Y as i32 {
            return BlockID::AIR;
        }
        let cp = Position::new(wx, wy, wz).to_chunk_pos();
        let Some(chunk) = self.chunks.get(&cp) else {
            return BlockID::AIR;
        };
        // local 坐标计算（rem_euclid 保证负坐标正确折算）
        let lx = wx.rem_euclid(CHUNK_X as i32) as usize;
        let lz = wz.rem_euclid(CHUNK_Z as i32) as usize;
        let ly = wy as usize;
        chunk.get(lx, ly, lz)
    }

    /// 在世界坐标处放置一个方块（若 chunk 未加载则忽略）。
    /// Phase 5 起：成功写入会把该 chunk 标记为 dirty。
    pub fn set_block(&mut self, pos: Position, block: BlockID) {
        let cp = pos.to_chunk_pos();
        let Some(chunk) = self.chunks.get_mut(&cp) else {
            return;
        };
        if let Some(idx) = pos.local_index() {
            chunk.blocks[idx] = block;
            self.dirty_chunks.insert(cp);
        }
    }

    /// 读取世界坐标处的方块。chunk 未加载返回 AIR（与 get_block_world 等价的 Position 接口）。
    pub fn get_block(&self, pos: Position) -> BlockID {
        self.get_block_world(pos.x, pos.y, pos.z)
    }

    /// Phase 5：取出当前 dirty chunk 列表并清空集合。
    /// Phase 5 暂无调用方；Phase 8 持久化层每秒 flush 一次。
    pub fn drain_dirty(&mut self) -> Vec<ChunkPos> {
        self.dirty_chunks.drain().collect()
    }

    /// 推进 tick 计数（Phase 5 起会驱动玩家广播等）。
    pub fn tick(&mut self) {
        self.tick_count += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use voxweb_core::block::BlockID;
    use voxweb_core::chunk::Position;

    #[test]
    fn ensure_chunk_generated_is_idempotent_and_uses_terrain() {
        let mut world = World::new(12345);
        let pos = ChunkPos::new(0, 0);

        // 首次生成
        world.ensure_chunk_generated(pos);
        assert!(world.chunks.contains_key(&pos));
        let snapshot: Vec<BlockID> = world.chunks[&pos].blocks.clone();
        // Perlin 地形：至少应有非 AIR 方块（基岩 + 一层地形）
        assert!(
            snapshot.iter().any(|b| *b != BlockID::AIR),
            "生成的 chunk 应至少有一个非空方块"
        );

        // 第二次调用：不应覆盖（同 blocks）
        world.ensure_chunk_generated(pos);
        assert_eq!(world.chunks[&pos].blocks, snapshot);
    }

    #[test]
    fn get_block_world_returns_air_for_unloaded_or_out_of_bounds() {
        let world = World::new(42);
        // 未加载 chunk
        assert_eq!(world.get_block_world(0, 64, 0), BlockID::AIR);
        // y 越界
        assert_eq!(world.get_block_world(0, -1, 0), BlockID::AIR);
        assert_eq!(world.get_block_world(0, 256, 0), BlockID::AIR);
    }

    #[test]
    fn get_block_world_reads_loaded_chunk() {
        let mut world = World::new(7);
        world.ensure_chunk_generated(ChunkPos::new(0, 0));
        // 与 chunk.get 等价
        let direct = world.chunks[&ChunkPos::new(0, 0)].get(3, 0, 5);
        let via_world = world.get_block_world(3, 0, 5);
        assert_eq!(direct, via_world);
    }

    #[test]
    fn unload_chunk_removes_chunk_entry() {
        let mut world = World::new(1);
        let pos = ChunkPos::new(2, -3);
        world.ensure_chunk_generated(pos);
        assert!(world.chunks.contains_key(&pos));
        world.unload_chunk(pos);
        assert!(!world.chunks.contains_key(&pos));
    }

    #[test]
    fn set_block_uses_position_local_index() {
        let mut world = World::new(0);
        world.ensure_chunk_generated(ChunkPos::new(0, 0));
        world.set_block(Position::new(5, 100, 7), BlockID::STONE);
        assert_eq!(world.get_block(Position::new(5, 100, 7)), BlockID::STONE);
    }

    #[test]
    fn set_block_marks_world_dirty() {
        // Phase 5：set_block 成功写入应把该 chunk 加进 dirty_chunks。
        let mut world = World::new(0);
        let cp = ChunkPos::new(0, 0);
        world.ensure_chunk_generated(cp);
        assert!(world.dirty_chunks.is_empty());

        world.set_block(Position::new(5, 100, 7), BlockID::STONE);
        assert!(world.dirty_chunks.contains(&cp));

        // drain 后清空
        let drained = world.drain_dirty();
        assert_eq!(drained, vec![cp]);
        assert!(world.dirty_chunks.is_empty());
    }

    #[test]
    fn set_block_on_unloaded_chunk_does_not_mark_dirty() {
        // chunk 未加载时 set_block 不应静默写入 / 标 dirty。
        let mut world = World::new(0);
        world.set_block(Position::new(0, 64, 0), BlockID::STONE);
        assert!(world.dirty_chunks.is_empty());
    }
}
