//! 世界状态管理：Chunk 表、玩家列表、tick 逻辑。

use std::collections::HashMap;

use voxweb_core::block::BlockID;
use voxweb_core::chunk::{Chunk, ChunkPos, Position};

/// 世界状态：持有所需的 Chunk 表 + 实体列表。
pub struct World {
    pub chunks: HashMap<ChunkPos, Chunk>,
    pub seed: u64,
    /// 自创建以来的总 tick 数
    pub tick_count: u64,
}

impl World {
    pub fn new(seed: u64) -> Self {
        Self {
            chunks: HashMap::new(),
            seed,
            tick_count: 0,
        }
    }

    /// 获取或生成指定坐标的 Chunk（如果不存在则触发地形生成）。
    pub fn ensure_chunk(&mut self, pos: ChunkPos) -> &mut Chunk {
        self.chunks.entry(pos).or_insert_with(|| Chunk::empty())
    }

    /// 在世界坐标处放置一个方块（若 Chunk 不存在则自动创建）。
    pub fn set_block(&mut self, pos: Position, block: BlockID) {
        let cp = pos.to_chunk_pos();
        let chunk = self.ensure_chunk(cp);
        if let Some(idx) = pos.local_index() {
            chunk.blocks[idx] = block;
        }
    }

    /// 读取世界坐标处的方块。若 Chunk 未加载，返回 AIR。
    pub fn get_block(&self, pos: Position) -> BlockID {
        let cp = pos.to_chunk_pos();
        match self.chunks.get(&cp) {
            Some(chunk) => match pos.local_index() {
                Some(idx) => chunk.blocks[idx],
                None => BlockID::AIR,
            },
            None => BlockID::AIR,
        }
    }

    /// 每 tick 调用（当前为占位，Phase 2+ 加入物理）。
    pub fn tick(&mut self) {
        self.tick_count += 1;
    }
}
