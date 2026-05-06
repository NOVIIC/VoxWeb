//! 网格生成：把 Chunk 的方块阵列转换为顶点列表。
//!
//! Phase 1：朴素逐面网格化 —— 遍历每个方块，对每个暴露的面（邻居为空气）
//! 生成 2 个三角形（6 个顶点）。区块边界外暂时一律视作"空气"。
//!
//! Phase 2 会引入"邻居 chunk 引用"做跨区块面剔除，
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
}
