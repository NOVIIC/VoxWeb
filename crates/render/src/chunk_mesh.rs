//! 网格生成：把 Chunk 的方块阵列转换为顶点列表。
//!
//! Phase 1：朴素逐面网格化 —— 遍历每个方块，对每个暴露的面（邻居为空气）
//! 生成 2 个三角形（6 个顶点）。区块边界外暂时一律视作"空气"。
//!
//! Phase 2：`generate_with_neighbors` 通过回调查询世界坐标方块，
//! 实现跨区块面剔除（避免边界处误绘制邻居 chunk 的"墙皮"）。
//!
//! Phase 7 会替换为贪婪算法。

use voxweb_core::{BlockID, CHUNK_X, CHUNK_Y, CHUNK_Z, Chunk, properties};

use crate::vertex::{Face, PackedVertex};

/// 一个 Chunk 的不透明网格 CPU 数据。
pub struct ChunkMeshCpu {
    pub vertices: Vec<PackedVertex>,
}

impl ChunkMeshCpu {
    /// 顶点数（== 三角形数 * 3）。
    pub fn vertex_count(&self) -> u32 {
        self.vertices.len() as u32
    }
}

/// 6 个面对应的 4 个角点局部偏移（相对方块最小角）。
/// 顺序为 CCW（从面"外侧"观察），三角形剖分为 (0,1,2) + (0,2,3)。
/// Phase 1 的 OpaquePass 暂关 face culling，winding 错也能渲染出来。
const FACE_CORNERS: [[(u8, u8, u8); 4]; 6] = [
    // 0 PosX  (x = max, normal = +X)
    [(1, 0, 1), (1, 0, 0), (1, 1, 0), (1, 1, 1)],
    // 1 NegX  (x = min, normal = -X)
    [(0, 0, 0), (0, 0, 1), (0, 1, 1), (0, 1, 0)],
    // 2 PosY  (y = max, normal = +Y)
    [(0, 1, 1), (1, 1, 1), (1, 1, 0), (0, 1, 0)],
    // 3 NegY  (y = min, normal = -Y)
    [(0, 0, 0), (1, 0, 0), (1, 0, 1), (0, 0, 1)],
    // 4 PosZ  (z = max, normal = +Z)
    [(0, 0, 1), (1, 0, 1), (1, 1, 1), (0, 1, 1)],
    // 5 NegZ  (z = min, normal = -Z)
    [(1, 0, 0), (0, 0, 0), (0, 1, 0), (1, 1, 0)],
];

/// 6 个面对应的"邻居方块"相对偏移。
const FACE_NEIGHBORS: [(i32, i32, i32); 6] = [
    (1, 0, 0),  // PosX
    (-1, 0, 0), // NegX
    (0, 1, 0),  // PosY
    (0, -1, 0), // NegY
    (0, 0, 1),  // PosZ
    (0, 0, -1), // NegZ
];

const FACES: [Face; 6] = [
    Face::PosX,
    Face::NegX,
    Face::PosY,
    Face::NegY,
    Face::PosZ,
    Face::NegZ,
];

/// 生成一个 Chunk 的不透明网格顶点。
///
/// 仅渲染非 AIR、且 `BlockProperties::transparent == false` 的方块。
/// 每个方块的 6 个面：若该方向邻居为 AIR / 区块外 / 透明方块，则发射该面。
pub fn generate_opaque_mesh(chunk: &Chunk) -> ChunkMeshCpu {
    // 预估容量：典型 chunk 暴露率约 10-30%，每方块 6 面 × 6 顶点
    let mut vertices: Vec<PackedVertex> = Vec::with_capacity(4096);

    for ly in 0..CHUNK_Y {
        for lz in 0..CHUNK_Z {
            for lx in 0..CHUNK_X {
                let block = chunk.get(lx, ly, lz);
                if block == BlockID::AIR {
                    continue;
                }
                let props = properties(block);
                if props.transparent {
                    // 透明方块走 transparent pass（Phase 8）
                    continue;
                }
                let tex = props.texture_index;

                for fi in 0..6 {
                    if face_is_visible(chunk, lx as i32, ly as i32, lz as i32, fi) {
                        emit_face(&mut vertices, lx as u8, ly as u16, lz as u8, fi, tex);
                    }
                }
            }
        }
    }

    ChunkMeshCpu { vertices }
}

/// 判断一个面是否需要渲染：邻居在区块内且非透明则被遮挡。
fn face_is_visible(chunk: &Chunk, lx: i32, ly: i32, lz: i32, face_idx: usize) -> bool {
    let (dx, dy, dz) = FACE_NEIGHBORS[face_idx];
    let nx = lx + dx;
    let ny = ly + dy;
    let nz = lz + dz;

    // 越出当前 chunk → Phase 1 视作空气，发射面
    if nx < 0 || nx >= CHUNK_X as i32 || nz < 0 || nz >= CHUNK_Z as i32 {
        return true;
    }
    if ny < 0 || ny >= CHUNK_Y as i32 {
        return true;
    }

    let neighbor = chunk.get(nx as usize, ny as usize, nz as usize);
    if neighbor == BlockID::AIR {
        return true;
    }
    // 邻居是透明方块也要渲染（这样能透过玻璃看到后面的石头）
    properties(neighbor).transparent
}

/// 跨区块面剔除版网格化。
///
/// 与 `generate_opaque_mesh` 行为相同，但所有面可见性查询通过 `get_block_world`
/// 回调进行。同 chunk 内的查询也走回调（统一接口）；区块外由调用方决定（一般返回邻居
/// chunk 已加载的真实方块，或 AIR）。
///
/// 这是 Phase 2 视觉正确性的核心：避免 chunk 边界处误把邻居 chunk 的实心方块视为空气
/// 而多绘制一层"墙皮"。
pub fn generate_with_neighbors(
    chunk: &Chunk,
    chunk_pos: voxweb_core::ChunkPos,
    get_block_world: &dyn Fn(i32, i32, i32) -> BlockID,
) -> ChunkMeshCpu {
    let mut vertices: Vec<PackedVertex> = Vec::with_capacity(4096);

    let origin_x = chunk_pos.x * CHUNK_X as i32;
    let origin_z = chunk_pos.z * CHUNK_Z as i32;

    for ly in 0..CHUNK_Y {
        for lz in 0..CHUNK_Z {
            for lx in 0..CHUNK_X {
                let block = chunk.get(lx, ly, lz);
                if block == BlockID::AIR {
                    continue;
                }
                let props = properties(block);
                if props.transparent {
                    continue;
                }
                let tex = props.texture_index;

                for (fi, &(dx, dy, dz)) in FACE_NEIGHBORS.iter().enumerate() {
                    let wx = origin_x + lx as i32 + dx;
                    let wy = ly as i32 + dy;
                    let wz = origin_z + lz as i32 + dz;
                    let neighbor = get_block_world(wx, wy, wz);
                    let visible = neighbor == BlockID::AIR || properties(neighbor).transparent;
                    if visible {
                        emit_face(&mut vertices, lx as u8, ly as u16, lz as u8, fi, tex);
                    }
                }
            }
        }
    }

    ChunkMeshCpu { vertices }
}

/// 把一个面（2 三角形 = 6 顶点）追加到顶点缓冲。
fn emit_face(out: &mut Vec<PackedVertex>, lx: u8, ly: u16, lz: u8, face_idx: usize, tex: u8) {
    let face = FACES[face_idx];
    let corners = &FACE_CORNERS[face_idx];

    // 6 个顶点的索引顺序：(0,1,2) + (0,2,3)
    const TRI_INDICES: [usize; 6] = [0, 1, 2, 0, 2, 3];

    for &i in &TRI_INDICES {
        let (cx, cy, cz) = corners[i];
        let vx = lx + cx;
        let vy = ly + cy as u16;
        let vz = lz + cz;
        // Phase 1：AO 一律 3（最亮，无遮蔽）
        out.push(PackedVertex::new(vx, vy, vz, face, tex, 3));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_chunk_has_no_vertices() {
        let chunk = Chunk::empty();
        let mesh = generate_opaque_mesh(&chunk);
        assert_eq!(mesh.vertex_count(), 0);
    }

    #[test]
    fn single_block_emits_six_faces() {
        let mut chunk = Chunk::empty();
        chunk.set(5, 64, 5, BlockID::STONE);
        let mesh = generate_opaque_mesh(&chunk);
        // 6 面 × 6 顶点（2 三角形）= 36
        assert_eq!(mesh.vertex_count(), 36);
    }

    #[test]
    fn touching_blocks_share_faces() {
        // 两个相邻的实心方块，中间那对面应被剔除。
        // 总暴露面 = 2 * 6 - 2 = 10 个面 → 60 个顶点
        let mut chunk = Chunk::empty();
        chunk.set(5, 64, 5, BlockID::STONE);
        chunk.set(6, 64, 5, BlockID::STONE);
        let mesh = generate_opaque_mesh(&chunk);
        assert_eq!(mesh.vertex_count(), 60);
    }

    #[test]
    fn neighbor_callback_skips_face_when_neighbor_is_solid() {
        // 单方块在 lx=15, ly=64, lz=5；邻居 chunk (1,0) 的 lx=0, ly=64, lz=5 是 STONE
        // → PosX 面应被跨区块剔除（与朴素版相比顶点 -6）
        let mut chunk = Chunk::empty();
        chunk.set(15, 64, 5, BlockID::STONE);

        // 朴素版（区块外视空气）：6 面 × 6 顶点 = 36
        let naive = generate_opaque_mesh(&chunk);
        assert_eq!(naive.vertex_count(), 36);

        // with_neighbors：邻居在 (16, 64, 5) 是 STONE
        let neighbor_x = 16;
        let with_n =
            generate_with_neighbors(&chunk, voxweb_core::ChunkPos::new(0, 0), &|wx, _wy, _wz| {
                if wx == neighbor_x {
                    BlockID::STONE
                } else {
                    BlockID::AIR
                }
            });
        // 5 面（PosX 被剔除）× 6 顶点 = 30
        assert_eq!(with_n.vertex_count(), 30);
    }

    #[test]
    fn neighbor_callback_air_equivalent_to_naive() {
        // 全 AIR 回调（区块外一律空气）应等同于 generate_opaque_mesh
        let mut chunk = Chunk::empty();
        chunk.set(0, 64, 0, BlockID::STONE);
        chunk.set(15, 64, 15, BlockID::DIRT);

        let naive = generate_opaque_mesh(&chunk);
        let with_n =
            generate_with_neighbors(&chunk, voxweb_core::ChunkPos::new(0, 0), &|_, _, _| {
                BlockID::AIR
            });
        assert_eq!(naive.vertex_count(), with_n.vertex_count());
    }

    #[test]
    fn neighbor_callback_handles_y_boundary() {
        // 顶层方块（ly=255），不存在更高层；回调对 y=256 返回 AIR → PosY 面应发射
        let mut chunk = Chunk::empty();
        chunk.set(5, 255, 5, BlockID::STONE);
        let with_n =
            generate_with_neighbors(&chunk, voxweb_core::ChunkPos::new(0, 0), &|_, _, _| {
                BlockID::AIR
            });
        // 单方块 6 面（无邻居遮挡）
        assert_eq!(with_n.vertex_count(), 36);
    }
}
