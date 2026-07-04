//! 地形生成：使用 Perlin 噪声生成高度图，按高度分层填充方块。

use noise::{NoiseFn, Perlin};

use voxweb_core::block::BlockID;
use voxweb_core::chunk::{CHUNK_X, CHUNK_Y, CHUNK_Z, Chunk, ChunkPos};

const BASE_HEIGHT: f64 = 58.0;
const HILL_AMPLITUDE: f64 = 24.0;
const DETAIL_AMPLITUDE: f64 = 4.0;
const HEIGHT_MIN: usize = 18;
const HEIGHT_MAX: usize = 112;

/// 地形生成器：封装 Perlin 噪声高度图（多倍频叠加 + 邻域平滑）。当前无 biome 分区。
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

                let height = self.height_at(world_x, world_z);

                for ly in 0..CHUNK_Y {
                    // ly == 0 强制 BEDROCK 兜底；height 极小时 height-3 会下溢，故用 ly + 3 < height 等价判断。
                    let block = if ly == 0 {
                        BlockID::BEDROCK
                    } else if ly + 3 < height {
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

    fn height_at(&self, world_x: i32, world_z: i32) -> usize {
        let mut weighted = 0.0;
        let mut total_weight = 0.0;
        for dx in -1..=1 {
            for dz in -1..=1 {
                let distance = ((dx * dx + dz * dz) as f64).sqrt();
                let weight = if distance == 0.0 { 2.0 } else { 1.0 / distance };
                weighted += self.raw_height(world_x + dx, world_z + dz) * weight;
                total_weight += weight;
            }
        }
        (weighted / total_weight)
            .round()
            .clamp(HEIGHT_MIN as f64, HEIGHT_MAX as f64) as usize
    }

    fn raw_height(&self, world_x: i32, world_z: i32) -> f64 {
        let x = world_x as f64;
        let z = world_z as f64;
        let continent = self.perlin.get([x * 0.0035, z * 0.0035]);
        let hills = self.perlin.get([x * 0.009, z * 0.009]);
        let detail = self.perlin.get([x * 0.035, z * 0.035]);
        BASE_HEIGHT + continent * 10.0 + hills * HILL_AMPLITUDE + detail * DETAIL_AMPLITUDE
    }
}
