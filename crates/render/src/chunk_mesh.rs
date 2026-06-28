//! 网格生成：把 Chunk 的方块阵列转换为 GPU 友好的压缩网格。
//!
//! Phase 1/2 采用朴素逐面网格化：每个可见方块面直接发射 6 个顶点。
//! Phase 7 起替换为贪婪网格化：同材质、同 AO 的相邻面合并为一个大四边形，
//! CPU 侧输出 4 个压缩顶点 + 6 个索引，并记录本 chunk 的可见 bounds 供视锥剔除。

use glam::Vec3;
use voxweb_core::{
    Aabb, BlockID, CHUNK_X, CHUNK_Y, CHUNK_Z, Chunk, ChunkPos, is_open_for_surface,
    is_smooth_granular, properties, smooth_corner_height, smooth_height_normal,
};

use crate::vertex::{Face, PackedVertex, SmoothVertex};

/// 一个 Chunk 的不透明网格 CPU 数据。
pub struct ChunkMeshCpu {
    /// 贪婪合并后的压缩顶点。每个 quad 只保留 4 个角点。
    pub vertices: Vec<PackedVertex>,
    /// u32 index buffer。每个 quad 6 个索引。
    pub indices: Vec<u32>,
    /// 平滑颗粒材质的 float 顶点。当前用于 SmoothGranular 材质的斜坡表面。
    pub smooth_vertices: Vec<SmoothVertex>,
    pub smooth_indices: Vec<u32>,
    /// 半透明方块独立顶点。Phase 8 起由 TransparentPass 负责 alpha blend。
    pub transparent_vertices: Vec<PackedVertex>,
    pub transparent_indices: Vec<u32>,
    /// 网格的局部 AABB（相对 chunk 原点），用于上传时转换为世界 AABB。
    pub bounds: Aabb,
    /// 贪婪前的可见单位面数量，用于 HUD 展示 Phase 2 等价顶点数。
    pub visible_faces: u32,
}

impl ChunkMeshCpu {
    /// 顶点数（贪婪合并后）。
    pub fn vertex_count(&self) -> u32 {
        self.vertices
            .len()
            .saturating_add(self.smooth_vertices.len()) as u32
    }

    /// 索引数（实际 draw_indexed 数量）。
    pub fn index_count(&self) -> u32 {
        self.indices.len().saturating_add(self.smooth_indices.len()) as u32
    }

    /// 贪婪前 Phase 2 朴素路径的等价顶点数：每个可见单位面 6 个顶点。
    pub fn phase2_vertex_count(&self) -> u32 {
        self.visible_faces.saturating_mul(6)
    }
}

impl ChunkMeshCpu {
    fn include_smooth_vertex(&mut self, p: Vec3) {
        if self.bounds.min == Vec3::ZERO
            && self.bounds.max == Vec3::ZERO
            && self.vertex_count() == 0
        {
            self.bounds = Aabb::new(p, p);
        } else {
            self.bounds = Aabb::new(self.bounds.min.min(p), self.bounds.max.max(p));
        }
    }
}

/// 6 个面对应的 4 个角点局部偏移（相对方块最小角）。
/// 顺序为 CCW（从面外侧观察），三角形剖分为 (0,1,2) + (0,2,3)。
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MaskCell {
    tex: u8,
    ao: [u8; 4],
}

struct MeshBuilder {
    vertices: Vec<PackedVertex>,
    indices: Vec<u32>,
    visible_faces: u32,
    bounds_min: Vec3,
    bounds_max: Vec3,
    has_bounds: bool,
}

impl MeshBuilder {
    fn new() -> Self {
        Self {
            vertices: Vec::with_capacity(4096),
            indices: Vec::with_capacity(6144),
            visible_faces: 0,
            bounds_min: Vec3::ZERO,
            bounds_max: Vec3::ZERO,
            has_bounds: false,
        }
    }

    fn push_visible_face(&mut self) {
        self.visible_faces = self.visible_faces.saturating_add(1);
    }

    fn emit_quad(&mut self, corners: [(u8, u16, u8); 4], face: Face, tex: u8, ao: [u8; 4]) {
        let base = self.vertices.len() as u32;
        for (i, &(x, y, z)) in corners.iter().enumerate() {
            self.include_vertex(x, y, z);
            self.vertices
                .push(PackedVertex::new(x, y, z, face, tex, ao[i]));
        }
        self.indices
            .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }

    fn include_vertex(&mut self, x: u8, y: u16, z: u8) {
        let p = Vec3::new(x as f32, y as f32, z as f32);
        if self.has_bounds {
            self.bounds_min = self.bounds_min.min(p);
            self.bounds_max = self.bounds_max.max(p);
        } else {
            self.bounds_min = p;
            self.bounds_max = p;
            self.has_bounds = true;
        }
    }

    fn finish(self) -> ChunkMeshCpu {
        let bounds = if self.has_bounds {
            Aabb::new(self.bounds_min, self.bounds_max)
        } else {
            Aabb::new(Vec3::ZERO, Vec3::ZERO)
        };
        ChunkMeshCpu {
            vertices: self.vertices,
            indices: self.indices,
            smooth_vertices: Vec::new(),
            smooth_indices: Vec::new(),
            transparent_vertices: Vec::new(),
            transparent_indices: Vec::new(),
            bounds,
            visible_faces: self.visible_faces,
        }
    }
}

struct MeshContext<'a> {
    chunk: &'a Chunk,
    origin_x: i32,
    origin_z: i32,
    get_block_world: &'a dyn Fn(i32, i32, i32) -> BlockID,
}

#[derive(Copy, Clone)]
struct SmoothFaceInfo {
    face_idx: usize,
    lx: usize,
    ly: usize,
    lz: usize,
    block: BlockID,
}

/// 生成一个 Chunk 的不透明网格顶点。
///
/// 仅渲染非 AIR、且 `BlockProperties::transparent == false` 的方块。
/// 区块外一律视作 AIR，主要用于单元测试和 Phase 1 兼容路径。
pub fn generate_opaque_mesh(chunk: &Chunk) -> ChunkMeshCpu {
    generate_with_neighbors(chunk, ChunkPos::new(0, 0), &|wx, wy, wz| {
        if wy < 0 || wy >= CHUNK_Y as i32 {
            return BlockID::AIR;
        }
        if wx < 0 || wx >= CHUNK_X as i32 || wz < 0 || wz >= CHUNK_Z as i32 {
            return BlockID::AIR;
        }
        chunk.get(wx as usize, wy as usize, wz as usize)
    })
}

/// 跨区块面剔除 + 贪婪网格化版本。
///
/// 所有可见性和 AO 采样都通过 `get_block_world` 查询世界坐标。邻居 chunk
/// 未加载时由调用方返回 AIR，因此会保守绘制更多边界面；邻居加载完成后再重网格化即可消除。
pub fn generate_with_neighbors(
    chunk: &Chunk,
    chunk_pos: ChunkPos,
    get_block_world: &dyn Fn(i32, i32, i32) -> BlockID,
) -> ChunkMeshCpu {
    let mut out = MeshBuilder::new();
    let ctx = MeshContext {
        chunk,
        origin_x: chunk_pos.x * CHUNK_X as i32,
        origin_z: chunk_pos.z * CHUNK_Z as i32,
        get_block_world,
    };

    emit_pos_x(&ctx, &mut out);
    emit_neg_x(&ctx, &mut out);
    emit_pos_y(&ctx, &mut out);
    emit_neg_y(&ctx, &mut out);
    emit_pos_z(&ctx, &mut out);
    emit_neg_z(&ctx, &mut out);

    let mut mesh = out.finish();
    emit_smooth_granular(&ctx, &mut mesh);
    emit_transparent(&ctx, &mut mesh);
    mesh
}

fn emit_smooth_granular(ctx: &MeshContext<'_>, mesh: &mut ChunkMeshCpu) {
    for ly in 0..CHUNK_Y {
        for lz in 0..CHUNK_Z {
            for lx in 0..CHUNK_X {
                let block = ctx.chunk.get(lx, ly, lz);
                if !is_smooth_granular(block) {
                    continue;
                }
                let wx = ctx.origin_x + lx as i32;
                let wy = ly as i32;
                let wz = ctx.origin_z + lz as i32;
                for (face_idx, &(dx, dy, dz)) in FACE_NEIGHBORS.iter().enumerate() {
                    let neighbor = (ctx.get_block_world)(wx + dx, wy + dy, wz + dz);
                    if neighbor != BlockID::AIR && !properties(neighbor).transparent {
                        continue;
                    }
                    emit_smooth_face(ctx, mesh, lx, ly, lz, face_idx, block);
                }
            }
        }
    }
}

fn emit_smooth_face(
    ctx: &MeshContext<'_>,
    mesh: &mut ChunkMeshCpu,
    lx: usize,
    ly: usize,
    lz: usize,
    face_idx: usize,
    block: BlockID,
) {
    let tex = properties(block).texture_index;
    let corners = smooth_face_corners(ctx, lx, ly, lz, face_idx, block);
    push_smooth_quad(
        ctx,
        mesh,
        corners,
        tex,
        SmoothFaceInfo {
            face_idx,
            lx,
            ly,
            lz,
            block,
        },
    );
}

fn push_smooth_quad(
    ctx: &MeshContext<'_>,
    mesh: &mut ChunkMeshCpu,
    corners: [Vec3; 4],
    tex: u8,
    info: SmoothFaceInfo,
) {
    let base = mesh.smooth_vertices.len() as u32;
    let normal = if info.face_idx == Face::PosY as usize {
        let wx = ctx.origin_x + info.lx as i32;
        let wy = info.ly as i32;
        let wz = ctx.origin_z + info.lz as i32;
        smooth_height_normal(ctx.get_block_world, wx, wy, wz, info.block)
    } else {
        quad_normal(corners)
    };
    for p in corners {
        mesh.include_smooth_vertex(p);
        mesh.smooth_vertices.push(SmoothVertex::new(
            [p.x, p.y, p.z],
            [normal.x, normal.y, normal.z],
            smooth_raw_uv(info.face_idx, p),
            tex,
        ));
    }
    mesh.smooth_indices
        .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
}

fn smooth_face_corners(
    ctx: &MeshContext<'_>,
    lx: usize,
    ly: usize,
    lz: usize,
    face_idx: usize,
    block: BlockID,
) -> [Vec3; 4] {
    let wx = ctx.origin_x + lx as i32;
    let wy = ly as i32;
    let wz = ctx.origin_z + lz as i32;
    let top_is_open = {
        let above = (ctx.get_block_world)(wx, wy + 1, wz);
        is_open_for_surface(above)
    };

    FACE_CORNERS[face_idx].map(|(cx, cy, cz)| {
        let x = lx as f32 + cx as f32;
        let z = lz as f32 + cz as f32;
        let mut y = ly as f32 + cy as f32;
        if cy == 1 && top_is_open {
            y = smooth_corner_height(
                ctx.get_block_world,
                wx + cx as i32,
                wz + cz as i32,
                wy,
                block,
            );
        }
        Vec3::new(x, y, z)
    })
}

fn quad_normal(corners: [Vec3; 4]) -> Vec3 {
    let normal = (corners[1] - corners[0]).cross(corners[2] - corners[0])
        + (corners[2] - corners[0]).cross(corners[3] - corners[0]);
    normal.try_normalize().unwrap_or(Vec3::Y)
}

fn smooth_raw_uv(face_idx: usize, p: Vec3) -> [f32; 2] {
    match face_idx {
        0 | 1 => [p.z, p.y],
        2 | 3 => [p.x, p.z],
        _ => [p.x, p.y],
    }
}

fn emit_transparent(ctx: &MeshContext<'_>, mesh: &mut ChunkMeshCpu) {
    for ly in 0..CHUNK_Y {
        for lz in 0..CHUNK_Z {
            for lx in 0..CHUNK_X {
                let block = ctx.chunk.get(lx, ly, lz);
                if block == BlockID::AIR || !properties(block).transparent {
                    continue;
                }
                let wx = ctx.origin_x + lx as i32;
                let wy = ly as i32;
                let wz = ctx.origin_z + lz as i32;
                let tex = properties(block).texture_index;
                for face_idx in 0..6 {
                    let (dx, dy, dz) = FACE_NEIGHBORS[face_idx];
                    let neighbor = (ctx.get_block_world)(wx + dx, wy + dy, wz + dz);
                    // 同类透明方块内部面不画；透明贴着空气或不透明方块时仍画一层可见面。
                    if neighbor == block {
                        continue;
                    }
                    let base = mesh.transparent_vertices.len() as u32;
                    for &(cx, cy, cz) in &FACE_CORNERS[face_idx] {
                        mesh.transparent_vertices.push(PackedVertex::new(
                            (lx + cx as usize) as u8,
                            (ly + cy as usize) as u16,
                            (lz + cz as usize) as u8,
                            Face::from_index(face_idx as u8),
                            tex,
                            3,
                        ));
                    }
                    mesh.transparent_indices.extend_from_slice(&[
                        base,
                        base + 1,
                        base + 2,
                        base,
                        base + 2,
                        base + 3,
                    ]);
                }
            }
        }
    }
}

fn emit_pos_x(ctx: &MeshContext<'_>, out: &mut MeshBuilder) {
    for lx in 0..CHUNK_X {
        let mut mask = vec![None; CHUNK_Z * CHUNK_Y];
        for ly in 0..CHUNK_Y {
            for lz in 0..CHUNK_Z {
                if let Some(cell) = visible_cell(ctx, lx, ly, lz, 0) {
                    mask[ly * CHUNK_Z + lz] = Some(cell);
                    out.push_visible_face();
                }
            }
        }
        greedy_merge(&mut mask, CHUNK_Z, CHUNK_Y, |z, y, w, h, cell| {
            let x = (lx + 1) as u8;
            let y0 = y as u16;
            let y1 = (y + h) as u16;
            let z0 = z as u8;
            let z1 = (z + w) as u8;
            out.emit_quad(
                [(x, y0, z1), (x, y0, z0), (x, y1, z0), (x, y1, z1)],
                Face::PosX,
                cell.tex,
                cell.ao,
            );
        });
    }
}

fn emit_neg_x(ctx: &MeshContext<'_>, out: &mut MeshBuilder) {
    for lx in 0..CHUNK_X {
        let mut mask = vec![None; CHUNK_Z * CHUNK_Y];
        for ly in 0..CHUNK_Y {
            for lz in 0..CHUNK_Z {
                if let Some(cell) = visible_cell(ctx, lx, ly, lz, 1) {
                    mask[ly * CHUNK_Z + lz] = Some(cell);
                    out.push_visible_face();
                }
            }
        }
        greedy_merge(&mut mask, CHUNK_Z, CHUNK_Y, |z, y, w, h, cell| {
            let x = lx as u8;
            let y0 = y as u16;
            let y1 = (y + h) as u16;
            let z0 = z as u8;
            let z1 = (z + w) as u8;
            out.emit_quad(
                [(x, y0, z0), (x, y0, z1), (x, y1, z1), (x, y1, z0)],
                Face::NegX,
                cell.tex,
                cell.ao,
            );
        });
    }
}

fn emit_pos_y(ctx: &MeshContext<'_>, out: &mut MeshBuilder) {
    for ly in 0..CHUNK_Y {
        let mut mask = vec![None; CHUNK_X * CHUNK_Z];
        for lz in 0..CHUNK_Z {
            for lx in 0..CHUNK_X {
                if let Some(cell) = visible_cell(ctx, lx, ly, lz, 2) {
                    mask[lz * CHUNK_X + lx] = Some(cell);
                    out.push_visible_face();
                }
            }
        }
        greedy_merge(&mut mask, CHUNK_X, CHUNK_Z, |x, z, w, h, cell| {
            let y = (ly + 1) as u16;
            let x0 = x as u8;
            let x1 = (x + w) as u8;
            let z0 = z as u8;
            let z1 = (z + h) as u8;
            out.emit_quad(
                [(x0, y, z1), (x1, y, z1), (x1, y, z0), (x0, y, z0)],
                Face::PosY,
                cell.tex,
                cell.ao,
            );
        });
    }
}

fn emit_neg_y(ctx: &MeshContext<'_>, out: &mut MeshBuilder) {
    for ly in 0..CHUNK_Y {
        let mut mask = vec![None; CHUNK_X * CHUNK_Z];
        for lz in 0..CHUNK_Z {
            for lx in 0..CHUNK_X {
                if let Some(cell) = visible_cell(ctx, lx, ly, lz, 3) {
                    mask[lz * CHUNK_X + lx] = Some(cell);
                    out.push_visible_face();
                }
            }
        }
        greedy_merge(&mut mask, CHUNK_X, CHUNK_Z, |x, z, w, h, cell| {
            let y = ly as u16;
            let x0 = x as u8;
            let x1 = (x + w) as u8;
            let z0 = z as u8;
            let z1 = (z + h) as u8;
            out.emit_quad(
                [(x0, y, z0), (x1, y, z0), (x1, y, z1), (x0, y, z1)],
                Face::NegY,
                cell.tex,
                cell.ao,
            );
        });
    }
}

fn emit_pos_z(ctx: &MeshContext<'_>, out: &mut MeshBuilder) {
    for lz in 0..CHUNK_Z {
        let mut mask = vec![None; CHUNK_X * CHUNK_Y];
        for ly in 0..CHUNK_Y {
            for lx in 0..CHUNK_X {
                if let Some(cell) = visible_cell(ctx, lx, ly, lz, 4) {
                    mask[ly * CHUNK_X + lx] = Some(cell);
                    out.push_visible_face();
                }
            }
        }
        greedy_merge(&mut mask, CHUNK_X, CHUNK_Y, |x, y, w, h, cell| {
            let z = (lz + 1) as u8;
            let x0 = x as u8;
            let x1 = (x + w) as u8;
            let y0 = y as u16;
            let y1 = (y + h) as u16;
            out.emit_quad(
                [(x0, y0, z), (x1, y0, z), (x1, y1, z), (x0, y1, z)],
                Face::PosZ,
                cell.tex,
                cell.ao,
            );
        });
    }
}

fn emit_neg_z(ctx: &MeshContext<'_>, out: &mut MeshBuilder) {
    for lz in 0..CHUNK_Z {
        let mut mask = vec![None; CHUNK_X * CHUNK_Y];
        for ly in 0..CHUNK_Y {
            for lx in 0..CHUNK_X {
                if let Some(cell) = visible_cell(ctx, lx, ly, lz, 5) {
                    mask[ly * CHUNK_X + lx] = Some(cell);
                    out.push_visible_face();
                }
            }
        }
        greedy_merge(&mut mask, CHUNK_X, CHUNK_Y, |x, y, w, h, cell| {
            let z = lz as u8;
            let x0 = x as u8;
            let x1 = (x + w) as u8;
            let y0 = y as u16;
            let y1 = (y + h) as u16;
            out.emit_quad(
                [(x1, y0, z), (x0, y0, z), (x0, y1, z), (x1, y1, z)],
                Face::NegZ,
                cell.tex,
                cell.ao,
            );
        });
    }
}

fn visible_cell(
    ctx: &MeshContext<'_>,
    lx: usize,
    ly: usize,
    lz: usize,
    face_idx: usize,
) -> Option<MaskCell> {
    let block = ctx.chunk.get(lx, ly, lz);
    if !is_opaque_render_block(block) {
        return None;
    }

    let (dx, dy, dz) = FACE_NEIGHBORS[face_idx];
    let wx = ctx.origin_x + lx as i32;
    let wy = ly as i32;
    let wz = ctx.origin_z + lz as i32;
    let neighbor = (ctx.get_block_world)(wx + dx, wy + dy, wz + dz);
    if neighbor != BlockID::AIR
        && !properties(neighbor).transparent
        && !is_smooth_granular(neighbor)
    {
        return None;
    }

    let props = properties(block);
    Some(MaskCell {
        tex: props.texture_index,
        ao: face_ao(wx, wy, wz, face_idx, ctx.get_block_world),
    })
}

fn greedy_merge<F>(mask: &mut [Option<MaskCell>], width: usize, height: usize, mut emit: F)
where
    F: FnMut(usize, usize, usize, usize, MaskCell),
{
    for v in 0..height {
        for u in 0..width {
            let idx = v * width + u;
            let Some(cell) = mask[idx] else {
                continue;
            };

            let mut w = 1usize;
            while u + w < width && mask[v * width + u + w] == Some(cell) {
                w += 1;
            }

            let mut h = 1usize;
            'height: while v + h < height {
                for du in 0..w {
                    if mask[(v + h) * width + u + du] != Some(cell) {
                        break 'height;
                    }
                }
                h += 1;
            }

            emit(u, v, w, h, cell);

            for dv in 0..h {
                for du in 0..w {
                    mask[(v + dv) * width + u + du] = None;
                }
            }
        }
    }
}

fn face_ao(
    wx: i32,
    wy: i32,
    wz: i32,
    face_idx: usize,
    get_block_world: &dyn Fn(i32, i32, i32) -> BlockID,
) -> [u8; 4] {
    let mut ao = [3; 4];
    for (i, corner) in FACE_CORNERS[face_idx].iter().enumerate() {
        ao[i] = corner_ao(wx, wy, wz, face_idx, *corner, get_block_world);
    }
    ao
}

fn corner_ao(
    wx: i32,
    wy: i32,
    wz: i32,
    face_idx: usize,
    corner: (u8, u8, u8),
    get_block_world: &dyn Fn(i32, i32, i32) -> BlockID,
) -> u8 {
    let normal = FACE_NEIGHBORS[face_idx];
    let mut sides = [(0, 0, 0); 2];
    let mut count = 0usize;

    for axis in 0..3 {
        let normal_component = match axis {
            0 => normal.0,
            1 => normal.1,
            _ => normal.2,
        };
        if normal_component != 0 {
            continue;
        }
        let corner_component = match axis {
            0 => corner.0,
            1 => corner.1,
            _ => corner.2,
        };
        let sign = if corner_component == 0 { -1 } else { 1 };
        sides[count] = match axis {
            0 => (sign, 0, 0),
            1 => (0, sign, 0),
            _ => (0, 0, sign),
        };
        count += 1;
    }

    debug_assert_eq!(count, 2);
    let s1 = sides[0];
    let s2 = sides[1];
    let base = (wx + normal.0, wy + normal.1, wz + normal.2);
    let side1 = blocks_ao(get_block_world(base.0 + s1.0, base.1 + s1.1, base.2 + s1.2));
    let side2 = blocks_ao(get_block_world(base.0 + s2.0, base.1 + s2.1, base.2 + s2.2));
    let corner = blocks_ao(get_block_world(
        base.0 + s1.0 + s2.0,
        base.1 + s1.1 + s2.1,
        base.2 + s1.2 + s2.2,
    ));
    vertex_ao(side1, side2, corner)
}

fn vertex_ao(side1: bool, side2: bool, corner: bool) -> u8 {
    if side1 && side2 {
        return 0;
    }
    let count = side1 as u8 + side2 as u8 + corner as u8;
    3 - count.min(3)
}

fn is_opaque_render_block(block: BlockID) -> bool {
    block != BlockID::AIR && !properties(block).transparent && !is_smooth_granular(block)
}

fn blocks_ao(block: BlockID) -> bool {
    block != BlockID::AIR && !properties(block).transparent
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ao_values(mesh: &ChunkMeshCpu) -> Vec<u32> {
        mesh.vertices.iter().map(|v| (v.0 >> 30) & 0x3).collect()
    }

    #[test]
    fn empty_chunk_has_no_vertices() {
        let chunk = Chunk::empty();
        let mesh = generate_opaque_mesh(&chunk);
        assert_eq!(mesh.vertex_count(), 0);
        assert_eq!(mesh.index_count(), 0);
    }

    #[test]
    fn single_block_emits_six_greedy_quads() {
        let mut chunk = Chunk::empty();
        chunk.set(5, 64, 5, BlockID::STONE);
        let mesh = generate_opaque_mesh(&chunk);
        // 6 面 × 4 顶点；index buffer 仍是 6 面 × 6 索引。
        assert_eq!(mesh.vertex_count(), 24);
        assert_eq!(mesh.index_count(), 36);
        assert_eq!(mesh.phase2_vertex_count(), 36);
    }

    #[test]
    fn touching_blocks_merge_outer_faces() {
        let mut chunk = Chunk::empty();
        chunk.set(5, 64, 5, BlockID::STONE);
        chunk.set(6, 64, 5, BlockID::STONE);
        let mesh = generate_opaque_mesh(&chunk);
        // 两个同材质方块组成 2x1x1 长方体，外表面被合并为 6 个 quad。
        assert_eq!(mesh.vertex_count(), 24);
        assert_eq!(mesh.index_count(), 36);
        assert_eq!(mesh.phase2_vertex_count(), 60);
    }

    #[test]
    fn flat_layer_reduces_vertices_by_more_than_eighty_percent() {
        let mut chunk = Chunk::empty();
        for x in 0..CHUNK_X {
            for z in 0..CHUNK_Z {
                chunk.set(x, 64, z, BlockID::STONE);
            }
        }
        let mesh = generate_opaque_mesh(&chunk);
        assert!(
            mesh.vertex_count() * 5 < mesh.phase2_vertex_count(),
            "greedy={} phase2={}",
            mesh.vertex_count(),
            mesh.phase2_vertex_count()
        );
    }

    #[test]
    fn smooth_granular_uses_float_mesh_not_blocky_mesh() {
        let mut chunk = Chunk::empty();
        chunk.set(5, 64, 5, BlockID::SAND);
        let mesh = generate_opaque_mesh(&chunk);
        assert_eq!(mesh.vertices.len(), 0);
        assert_eq!(mesh.indices.len(), 0);
        assert_eq!(mesh.smooth_vertices.len(), 24);
        assert_eq!(mesh.smooth_indices.len(), 36);
    }

    #[test]
    fn smooth_granular_interpolates_top_heights() {
        let mut chunk = Chunk::empty();
        chunk.set(5, 64, 5, BlockID::SAND);
        chunk.set(6, 63, 5, BlockID::SAND);
        let mesh = generate_opaque_mesh(&chunk);
        assert!(
            mesh.smooth_vertices
                .iter()
                .any(|v| v.position[1] > 64.35 && v.position[1] < 65.0),
            "expected at least one interpolated corner height"
        );
    }

    #[test]
    fn neighbor_callback_skips_face_when_neighbor_is_solid() {
        let mut chunk = Chunk::empty();
        chunk.set(15, 64, 5, BlockID::STONE);

        let naive = generate_opaque_mesh(&chunk);
        assert_eq!(naive.index_count(), 36);

        let neighbor_x = 16;
        let with_n = generate_with_neighbors(&chunk, ChunkPos::new(0, 0), &|wx, _wy, _wz| {
            if wx == neighbor_x {
                BlockID::STONE
            } else {
                BlockID::AIR
            }
        });
        assert_eq!(with_n.vertex_count(), 20);
        assert_eq!(with_n.index_count(), 30);
    }

    #[test]
    fn hard_face_remains_visible_against_smooth_neighbor() {
        let mut chunk = Chunk::empty();
        chunk.set(5, 64, 5, BlockID::STONE);
        chunk.set(6, 64, 5, BlockID::SAND);
        let mesh = generate_opaque_mesh(&chunk);
        assert!(
            mesh.vertices.len() >= 24,
            "hard block should keep visible faces when touching smooth material"
        );
        assert!(!mesh.smooth_vertices.is_empty());
    }

    #[test]
    fn neighbor_callback_air_equivalent_to_naive() {
        let mut chunk = Chunk::empty();
        chunk.set(0, 64, 0, BlockID::STONE);
        chunk.set(15, 64, 15, BlockID::DIRT);

        let naive = generate_opaque_mesh(&chunk);
        let with_n = generate_with_neighbors(&chunk, ChunkPos::new(0, 0), &|_, _, _| BlockID::AIR);
        assert_eq!(naive.vertex_count(), with_n.vertex_count());
        assert_eq!(naive.index_count(), with_n.index_count());
    }

    #[test]
    fn neighbor_callback_handles_y_boundary() {
        let mut chunk = Chunk::empty();
        chunk.set(5, 255, 5, BlockID::STONE);
        let with_n = generate_with_neighbors(&chunk, ChunkPos::new(0, 0), &|_, _, _| BlockID::AIR);
        assert_eq!(with_n.vertex_count(), 24);
        assert_eq!(with_n.index_count(), 36);
        assert_eq!(with_n.bounds.max.y, 256.0);
    }

    #[test]
    fn ao_darkens_when_occluders_touch_corner() {
        let mut chunk = Chunk::empty();
        chunk.set(5, 64, 5, BlockID::STONE);
        // 给顶面西北角外侧放两个遮挡方块，让至少一个顶点 AO 低于 3。
        chunk.set(4, 65, 5, BlockID::STONE);
        chunk.set(5, 65, 4, BlockID::STONE);

        let mesh = generate_opaque_mesh(&chunk);
        assert!(ao_values(&mesh).iter().any(|ao| *ao < 3));
    }
}
