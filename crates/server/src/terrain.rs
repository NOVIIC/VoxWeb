//! 地形生成：使用 Perlin 噪声生成高度图，按高度分层填充方块。

use noise::{NoiseFn, Perlin};

use voxweb_core::block::BlockID;
use voxweb_core::chunk::{CHUNK_X, CHUNK_Y, CHUNK_Z, Chunk, ChunkPos};

/// 地形生成器：封装 Perlin 噪声 + 生物群落参数。
pub struct TerrainGenerator {
    perlin: Perlin,
}

impl TerrainGenerator {
    /// 根据世界种子创建地形生成器。
    pub fn new(seed: u64) -> Self {
        Self {
            perlin: Perlin::new(seed as u32),
        }
    }

    /// 为一个 ChunkPos 生成完整 Chunk（含地形 + 分层填充）。
    pub fn generate_chunk(&self, pos: ChunkPos) -> Chunk {
        let mut chunk = Chunk::empty();
        for lx in 0..CHUNK_X {
            for lz in 0..CHUNK_Z {
                let world_x = pos.x * CHUNK_X as i32 + lx as i32;
                let world_z = pos.z * CHUNK_Z as i32 + lz as i32;

                // 采样 Perlin 噪声，映射到 0..CHUNK_Y
                let noise_val = self
                    .perlin
                    .get([world_x as f64 * 0.01, world_z as f64 * 0.01]);
                let height = ((noise_val + 1.0) * 0.5 * (CHUNK_Y as f64 * 0.4)) as usize;

                for ly in 0..CHUNK_Y {
                    // ly == 0 强制 STONE 兜底（height 极小时 height-3 会下溢，故用 ly + 3 < height 等价判断）
                    let block = if ly == 0 || ly + 3 < height {
                        BlockID::STONE
                    } else if ly < height {
                        BlockID::DIRT
                    } else if ly == height {
                        BlockID::GRASS
                    } else {
                        BlockID::AIR
                    };
                    chunk.set(lx, ly, lz, block);
                }
            }
        }
        chunk
    }
}
