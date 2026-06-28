//! 区块数据结构、世界坐标、区块坐标。

use serde::{Deserialize, Serialize};

use crate::block::BlockID;

// —— 常量 ——

/// Chunk X 方向方块数
pub const CHUNK_X: usize = 16;
/// Chunk Y 方向方块数（一柱到顶，Y 不分块）
pub const CHUNK_Y: usize = 256;
/// Chunk Z 方向方块数
pub const CHUNK_Z: usize = 16;
/// 单个 Chunk 的方块总数
pub const CHUNK_SIZE: usize = CHUNK_X * CHUNK_Y * CHUNK_Z; // 65536
/// OPFS/FieldChunk 存盘格式版本。
pub const STORAGE_VERSION: u8 = 2;

// —— 世界坐标 ——

/// 世界中的绝对方块坐标。
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct Position {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

impl Position {
    pub fn new(x: i32, y: i32, z: i32) -> Self {
        Self { x, y, z }
    }

    /// 获取该坐标所在的 ChunkPos。
    /// 向下取整除法（Rust 负值向零取整，需特殊处理）。
    pub fn to_chunk_pos(self) -> ChunkPos {
        ChunkPos {
            x: div_floor(self.x, CHUNK_X as i32),
            z: div_floor(self.z, CHUNK_Z as i32),
        }
    }

    /// 获取该坐标在 Chunk 内的局部索引 (0..CHUNK_SIZE)。
    /// 索引规则：(y << 8) | (z << 4) | x 。
    /// 越界返回 None。
    pub fn local_index(self) -> Option<usize> {
        let cp = self.to_chunk_pos();
        let lx = self.x - cp.x * CHUNK_X as i32;
        let lz = self.z - cp.z * CHUNK_Z as i32;
        if lx < 0 || lx >= CHUNK_X as i32 || lz < 0 || lz >= CHUNK_Z as i32 {
            return None;
        }
        if self.y < 0 || self.y >= CHUNK_Y as i32 {
            return None;
        }
        Some(index(lx as usize, self.y as usize, lz as usize))
    }
}

/// 除法的数学取整（向负无穷方向取整）。
fn div_floor(a: i32, b: i32) -> i32 {
    let d = a / b;
    let r = a % b;
    if (r > 0 && b < 0) || (r < 0 && b > 0) {
        d - 1
    } else {
        d
    }
}

// —— 区块坐标 ——

/// 区块在世界中的二维坐标（xz 平面）。Y 方向不分块，一柱 256 格到顶。
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ChunkPos {
    pub x: i32,
    pub z: i32,
}

impl ChunkPos {
    pub fn new(x: i32, z: i32) -> Self {
        Self { x, z }
    }
}

// —— Chunk ——

/// 一个 16×256×16 的方块列柱。
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Chunk {
    /// 长度恒为 CHUNK_SIZE 的方块数组
    pub blocks: Vec<BlockID>,
}

impl Chunk {
    /// 创建一个全 AIR 的 Chunk。
    pub fn empty() -> Self {
        Self {
            blocks: vec![BlockID::AIR; CHUNK_SIZE],
        }
    }

    /// 按局部坐标读取方块。不做边界检查，调用者保证参数合法。
    pub fn get(&self, lx: usize, ly: usize, lz: usize) -> BlockID {
        self.blocks[index(lx, ly, lz)]
    }

    /// 按局部坐标写入方块。不做边界检查，调用者保证参数合法。
    pub fn set(&mut self, lx: usize, ly: usize, lz: usize, id: BlockID) {
        let i = index(lx, ly, lz);
        self.blocks[i] = id;
    }
}

/// Chunk 内一维索引：(y << 8) | (z << 4) | x 。
/// Y 放在高位，使得同层方块在内存上连续，利于水平方向遍历时的缓存命中。
#[inline]
pub fn index(lx: usize, ly: usize, lz: usize) -> usize {
    (ly << 8) | (lz << 4) | lx
}

/// FieldChunk 解码错误。所有错误都返回给调用方处理，不 panic。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Corrupted(String),
    VersionMismatch { found: u8, expected: u8 },
    LengthMismatch(usize),
}

// —— 测试 ——

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_bounds() {
        // 最小值
        assert_eq!(index(0, 0, 0), 0);
        // 最大值
        assert_eq!(index(15, 255, 15), CHUNK_SIZE - 1);
    }

    #[test]
    fn position_to_chunk_pos() {
        // 正坐标
        assert_eq!(Position::new(0, 64, 0).to_chunk_pos(), ChunkPos::new(0, 0));
        assert_eq!(
            Position::new(15, 64, 15).to_chunk_pos(),
            ChunkPos::new(0, 0)
        );
        assert_eq!(Position::new(16, 64, 0).to_chunk_pos(), ChunkPos::new(1, 0));
        // 负坐标：-1 应落到 chunk_x=-1
        assert_eq!(
            Position::new(-1, 64, 0).to_chunk_pos(),
            ChunkPos::new(-1, 0)
        );
        assert_eq!(
            Position::new(-16, 64, 0).to_chunk_pos(),
            ChunkPos::new(-1, 0)
        );
        assert_eq!(
            Position::new(-17, 64, 0).to_chunk_pos(),
            ChunkPos::new(-2, 0)
        );
    }

    #[test]
    fn local_index_roundtrip() {
        let pos = Position::new(5, 128, 7);
        let idx = pos.local_index().unwrap();
        assert_eq!(idx, index(5, 128, 7));
    }

    #[test]
    fn chunk_empty_all_air() {
        let chunk = Chunk::empty();
        for i in 0..CHUNK_SIZE {
            assert_eq!(chunk.blocks[i], BlockID::AIR);
        }
    }

    #[test]
    fn chunk_get_set() {
        let mut chunk = Chunk::empty();
        chunk.set(3, 100, 5, BlockID::STONE);
        assert_eq!(chunk.get(3, 100, 5), BlockID::STONE);
        // 未改位置仍是 AIR
        assert_eq!(chunk.get(3, 100, 6), BlockID::AIR);
    }
}
