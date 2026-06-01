//! 区块数据结构、世界坐标、区块坐标。
//!
//! ## Chunk 网络压缩格式（palette + RLE）
//!
//! ChunkSnapshot 发送前用 [`encode_chunk`] 压缩，接收端用 [`decode_chunk`] 解压。
//! 格式：`{ palette: Vec<BlockID>, runs: Vec<(palette_index: u16, run_length: u32)> }`
//! 遍历 blocks 按顺序扫描连续相同 BlockID 的 run，记录其 palette 下标和长度。
//! 典型地形（草/泥/石/空气 4 种方块）从 131KB 压缩到 2-5KB。

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
/// OPFS 存盘格式版本。网络 ChunkSnapshot 继续使用协议版本，不受此常量影响。
pub const STORAGE_VERSION: u8 = 1;

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

// —— Chunk 网络压缩（palette + RLE）——

/// palette+RLE 压缩格式。bincode 序列化后作为 ChunkSnapshot 的 payload。
#[derive(Clone, Debug, Serialize, Deserialize)]
struct CompressedChunk {
    /// 本 chunk 中出现的所有不同 BlockID（按首次出现顺序排列）
    palette: Vec<BlockID>,
    /// 按顺序排列的 run：(palette_index, run_length)
    runs: Vec<(u16, u32)>,
}

/// OPFS 存盘 chunk 包装。外层带版本号，避免未来格式升级时误读旧数据。
#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredChunk {
    version: u8,
    compressed: CompressedChunk,
}

/// 存盘 chunk 解码错误。所有错误都返回给调用方处理，不 panic。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Corrupted(String),
    VersionMismatch { found: u8, expected: u8 },
    InvalidPaletteIndex(u16),
    LengthMismatch(usize),
}

/// 将 Chunk 的 blocks 数组编码为 palette+RLE 压缩字节。
///
/// 算法：遍历 65536 个方块，将连续相同 BlockID 合并为一个 run；
/// 每个 BlockID 查找/插入 palette，记录 (palette_index, run_length)。
pub fn encode_chunk(blocks: &[BlockID]) -> Result<Vec<u8>, String> {
    let compressed = compress_blocks(blocks)?;
    crate::protocol::encode(&compressed).map_err(|e| format!("encode_chunk bincode: {e}"))
}

/// 将 `encode_chunk` 的压缩字节还原为 `Vec<BlockID>`（长度恒为 CHUNK_SIZE）。
pub fn decode_chunk(bytes: &[u8]) -> Result<Vec<BlockID>, String> {
    let compressed: CompressedChunk =
        crate::protocol::decode(bytes).map_err(|e| format!("decode_chunk bincode: {e}"))?;
    expand_compressed(&compressed).map_err(|e| format!("{e:?}"))
}

/// OPFS 存盘用编码：`StoredChunk { version, compressed }`。
pub fn encode(chunk: &Chunk) -> Vec<u8> {
    let compressed = compress_blocks(&chunk.blocks).expect("Chunk blocks length is always valid");
    let stored = StoredChunk {
        version: STORAGE_VERSION,
        compressed,
    };
    crate::protocol::encode(&stored).expect("StoredChunk serialization should not fail")
}

/// OPFS 存盘用解码。版本不匹配时返回结构化错误，调用方可显示“需升级”。
pub fn decode(bytes: &[u8]) -> Result<Chunk, DecodeError> {
    let stored: StoredChunk =
        crate::protocol::decode(bytes).map_err(|e| DecodeError::Corrupted(e.to_string()))?;
    if stored.version != STORAGE_VERSION {
        return Err(DecodeError::VersionMismatch {
            found: stored.version,
            expected: STORAGE_VERSION,
        });
    }
    let blocks = expand_compressed(&stored.compressed)?;
    Ok(Chunk { blocks })
}

fn compress_blocks(blocks: &[BlockID]) -> Result<CompressedChunk, String> {
    if blocks.len() != CHUNK_SIZE {
        return Err(format!(
            "compress_blocks: expected {CHUNK_SIZE} blocks, got {}",
            blocks.len()
        ));
    }

    let mut palette: Vec<BlockID> = Vec::new();
    let mut runs: Vec<(u16, u32)> = Vec::new();

    let mut cursor = 0usize;
    while cursor < CHUNK_SIZE {
        let current = blocks[cursor];
        let mut run_len: u32 = 1;
        while cursor + (run_len as usize) < CHUNK_SIZE
            && blocks[cursor + (run_len as usize)] == current
        {
            run_len += 1;
        }
        let pi = match palette.iter().position(|b| *b == current) {
            Some(i) => i as u16,
            None => {
                let i = palette.len() as u16;
                palette.push(current);
                i
            }
        };
        runs.push((pi, run_len));
        cursor += run_len as usize;
    }

    Ok(CompressedChunk { palette, runs })
}

fn expand_compressed(compressed: &CompressedChunk) -> Result<Vec<BlockID>, DecodeError> {
    let mut blocks: Vec<BlockID> = Vec::with_capacity(CHUNK_SIZE);
    for (pi, run_len) in &compressed.runs {
        let block = compressed
            .palette
            .get(*pi as usize)
            .ok_or(DecodeError::InvalidPaletteIndex(*pi))?;
        for _ in 0..*run_len {
            blocks.push(*block);
        }
    }

    if blocks.len() != CHUNK_SIZE {
        return Err(DecodeError::LengthMismatch(blocks.len()));
    }
    Ok(blocks)
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

    // —— palette+RLE 压缩 roundtrip ——

    #[test]
    fn compress_all_air_roundtrip() {
        let blocks = vec![BlockID::AIR; CHUNK_SIZE];
        let bytes = encode_chunk(&blocks).expect("encode");
        // 全 AIR 压缩后应极小（1 palette + 1 run）
        assert!(
            bytes.len() < 30,
            "all-air should be <30B, got {}",
            bytes.len()
        );
        let decoded = decode_chunk(&bytes).expect("decode");
        assert_eq!(decoded, blocks);
    }

    #[test]
    fn compress_all_stone_roundtrip() {
        let blocks = vec![BlockID::STONE; CHUNK_SIZE];
        let bytes = encode_chunk(&blocks).expect("encode");
        assert!(
            bytes.len() < 30,
            "all-stone should be <30B, got {}",
            bytes.len()
        );
        let decoded = decode_chunk(&bytes).expect("decode");
        assert_eq!(decoded, blocks);
    }

    #[test]
    fn compress_layered_terrain_roundtrip() {
        // 模拟典型地形：底部石头、中层泥土、顶部草、其余空气
        let mut blocks = vec![BlockID::AIR; CHUNK_SIZE];
        for y in 0..64 {
            for x in 0..16 {
                for z in 0..16 {
                    let idx = index(x, y, z);
                    blocks[idx] = BlockID::STONE;
                }
            }
        }
        for y in 64..70 {
            for x in 0..16 {
                for z in 0..16 {
                    let idx = index(x, y, z);
                    blocks[idx] = BlockID::DIRT;
                }
            }
        }
        for x in 0..16 {
            for z in 0..16 {
                let idx = index(x, 70, z);
                blocks[idx] = BlockID::GRASS;
            }
        }
        let bytes = encode_chunk(&blocks).expect("encode");
        // 典型地形压缩后应 < 2KB
        assert!(
            bytes.len() < 2048,
            "layered terrain should be <2KB, got {}",
            bytes.len()
        );
        let decoded = decode_chunk(&bytes).expect("decode");
        assert_eq!(decoded, blocks);
    }

    #[test]
    fn compress_interleaved_blocks_roundtrip() {
        // 交替方块：偶数索引用 STONE，奇数索引用 DIRT（最差情况，无法合并 run）
        let mut blocks = vec![BlockID::AIR; CHUNK_SIZE];
        for (i, block) in blocks.iter_mut().enumerate() {
            *block = if i % 2 == 0 {
                BlockID::STONE
            } else {
                BlockID::DIRT
            };
        }
        let bytes = encode_chunk(&blocks).expect("encode");
        // 交替时压缩率很差，但不应超过原始 131KB 太多
        assert!(bytes.len() < 200_000);
        let decoded = decode_chunk(&bytes).expect("decode");
        assert_eq!(decoded, blocks);
    }

    #[test]
    fn encode_wrong_block_count() {
        let short = vec![BlockID::AIR; 100];
        assert!(encode_chunk(&short).is_err());
    }

    #[test]
    fn decode_corrupt_bytes() {
        assert!(decode_chunk(b"garbage data not valid").is_err());
    }

    #[test]
    fn stored_chunk_roundtrip() {
        let mut chunk = Chunk::empty();
        chunk.set(1, 2, 3, BlockID::GLASS);
        chunk.set(4, 5, 6, BlockID::WATER);
        let bytes = encode(&chunk);
        let decoded = decode(&bytes).expect("decode stored chunk");
        assert_eq!(decoded.blocks, chunk.blocks);
    }

    #[test]
    fn stored_chunk_rejects_future_version() {
        let stored = StoredChunk {
            version: STORAGE_VERSION + 1,
            compressed: compress_blocks(&vec![BlockID::AIR; CHUNK_SIZE]).unwrap(),
        };
        let bytes = crate::protocol::encode(&stored).unwrap();
        assert_eq!(
            decode(&bytes),
            Err(DecodeError::VersionMismatch {
                found: STORAGE_VERSION + 1,
                expected: STORAGE_VERSION,
            })
        );
    }
}
