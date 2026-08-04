//! 网格生成：把 Chunk 的方块阵列转换为 GPU 友好的压缩网格。
//!
//! Phase 1/2 采用朴素逐面网格化：每个可见方块面直接发射 6 个顶点。
//! Phase 7 起替换为贪婪网格化：同材质、同 AO 的相邻面合并为一个大四边形，
//! CPU 侧输出 4 个压缩顶点 + 6 个索引，并记录本 chunk 的可见 bounds 供视锥剔除。

use glam::Vec3;
use voxweb_core::{
    Aabb, BlockID, CHUNK_X, CHUNK_Y, CHUNK_Z, Chunk, ChunkPos, SmoothColumnSurface,
    column_hard_ceiling, column_has_hard_over_smooth, find_smooth_column_surface,
    is_smooth_granular, normal_from_corners, properties, smooth_corner_height, smooth_stack_bottom,
    solid_column_top_y,
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

/// SmoothGranular：连续列顶高度场 + 硬/空交界 skirt。
///
/// 不再按每个软材质 cell 发射立方体面。每个 (x,z) 只取露出表面，角点在邻域
/// 列顶之间共享插值，高低列自动连成斜坡；只有贴着非软材质或虚空时才补侧面裙边。
fn emit_smooth_granular(ctx: &MeshContext<'_>, mesh: &mut ChunkMeshCpu) {
    // 边框需覆盖角点加权半径，否则 chunk 边界坡面会突然变尖。
    const PAD: i32 = 2;
    let min_lx = -PAD;
    let max_lx = CHUNK_X as i32 + PAD;
    let min_lz = -PAD;
    let max_lz = CHUNK_Z as i32 + PAD;
    let stride_x = (max_lx - min_lx) as usize;
    let stride_z = (max_lz - min_lz) as usize;

    // 本 chunk 内部以 `chunk` 为准，邻居才走 `get_block_world`。避免测试/边界
    // 回调把本 chunk 列误判成空气，导致软表面整片丢失。
    let get_column_block = |wx: i32, wy: i32, wz: i32| -> BlockID {
        let lx = wx - ctx.origin_x;
        let lz = wz - ctx.origin_z;
        if (0..CHUNK_X as i32).contains(&lx)
            && (0..CHUNK_Z as i32).contains(&lz)
            && (0..CHUNK_Y as i32).contains(&wy)
        {
            ctx.chunk.get(lx as usize, wy as usize, lz as usize)
        } else {
            (ctx.get_block_world)(wx, wy, wz)
        }
    };

    let mut columns: Vec<Option<SmoothColumnSurface>> = vec![None; stride_x * stride_z];
    for lz in min_lz..max_lz {
        for lx in min_lx..max_lx {
            let wx = ctx.origin_x + lx;
            let wz = ctx.origin_z + lz;
            let idx = ((lz - min_lz) as usize) * stride_x + (lx - min_lx) as usize;
            columns[idx] = find_smooth_column_surface(&get_column_block, wx, wz);
        }
    }

    let col = |columns: &[Option<SmoothColumnSurface>],
               lx: i32,
               lz: i32|
     -> Option<SmoothColumnSurface> {
        if lx < min_lx || lx >= max_lx || lz < min_lz || lz >= max_lz {
            return None;
        }
        columns[((lz - min_lz) as usize) * stride_x + (lx - min_lx) as usize]
    };

    // 角点缓存：chunk 内 0..=16。高度直接从列缓存加权，避免每个角点重新扫世界。
    let corner_min = 0i32;
    let corner_max = CHUNK_X as i32; // inclusive
    let corner_stride = (corner_max - corner_min + 1) as usize;
    let mut corners = vec![0.0f32; corner_stride * corner_stride];
    const CORNER_RADIUS: i32 = 2;
    const MIXED_BIAS: f32 = -0.03;
    for cz in corner_min..=corner_max {
        for cx in corner_min..=corner_max {
            let prefer = col(&columns, cx - 1, cz - 1)
                .or_else(|| col(&columns, cx, cz - 1))
                .or_else(|| col(&columns, cx - 1, cz))
                .or_else(|| col(&columns, cx, cz))
                .map(|s| s.block);
            let mut total = 0.0f32;
            let mut weight_sum = 0.0f32;
            for sz in (cz - CORNER_RADIUS)..(cz + CORNER_RADIUS) {
                for sx in (cx - CORNER_RADIUS)..(cx + CORNER_RADIUS) {
                    let Some(surface) = col(&columns, sx, sz) else {
                        continue;
                    };
                    let dx = (sx as f32 + 0.5) - cx as f32;
                    let dz = (sz as f32 + 0.5) - cz as f32;
                    let dist = (dx * dx + dz * dz).sqrt();
                    let w = 1.0 / (1.0 + dist);
                    let bias = match prefer {
                        Some(block) if surface.block != block => MIXED_BIAS,
                        _ => 0.0,
                    };
                    total += (surface.top_y + bias) * w;
                    weight_sum += w;
                }
            }
            let h = if weight_sum > 0.0 {
                total / weight_sum
            } else {
                smooth_corner_height(
                    &get_column_block,
                    ctx.origin_x + cx,
                    ctx.origin_z + cz,
                    prefer,
                )
            };
            // 角点不得穿进触碰列的硬方块底面，否则软坡会钻进石头里造成透视/破面。
            let mut clamped = h;
            let probe_y = ((h as i32) - 2).max(0);
            for (dx, dz) in [(0, 0), (-1, 0), (0, -1), (-1, -1)] {
                let sx = cx + dx;
                let sz = cz + dz;
                let wx = ctx.origin_x + sx;
                let wz = ctx.origin_z + sz;
                if let Some(ceil) = column_hard_ceiling(&get_column_block, wx, wz, probe_y) {
                    clamped = clamped.min(ceil);
                }
            }
            corners[((cz - corner_min) as usize) * corner_stride + (cx - corner_min) as usize] =
                clamped;
        }
    }

    let corner_h = |corners: &[f32], cx: i32, cz: i32| -> f32 {
        corners[((cz - corner_min) as usize) * corner_stride + (cx - corner_min) as usize]
    };

    for lz in 0..CHUNK_Z as i32 {
        for lx in 0..CHUNK_X as i32 {
            let Some(surface) = col(&columns, lx, lz) else {
                continue;
            };
            let h00 = corner_h(&corners, lx, lz);
            let h10 = corner_h(&corners, lx + 1, lz);
            let h01 = corner_h(&corners, lx, lz + 1);
            let h11 = corner_h(&corners, lx + 1, lz + 1);

            let top_corners = [
                Vec3::new(lx as f32, h00, lz as f32),
                Vec3::new(lx as f32 + 1.0, h10, lz as f32),
                Vec3::new(lx as f32 + 1.0, h11, lz as f32 + 1.0),
                Vec3::new(lx as f32, h01, lz as f32 + 1.0),
            ];
            // CCW from above: (0,1,2,3) with outward +Y → use (0,3,2,1) wait
            // Local XZ: (lx,lz)=(0), (lx+1,lz)=(1), (lx+1,lz+1)=(2), (lx,lz+1)=(3)
            // From above (+Y), CCW is 0 -> 3 -> 2 -> 1
            let top_ccw = [
                top_corners[0],
                top_corners[3],
                top_corners[2],
                top_corners[1],
            ];
            let n = normal_from_corners(h00, h10, h01, h11);
            push_smooth_quad_with_normal(
                mesh,
                top_ccw,
                n,
                properties(surface.block).texture_index,
                2,
            );

            // 四条边：邻居没有软表面时补 skirt，接硬材质顶或本列堆底。
            let edges = [
                // -Z edge: corners (lx,lz)-(lx+1,lz), neighbor (lx, lz-1)
                (0i32, -1i32, top_corners[0], top_corners[1]),
                // +Z edge: corners (lx+1,lz+1)-(lx,lz+1), neighbor (lx, lz+1)
                (0, 1, top_corners[2], top_corners[3]),
                // -X edge: corners (lx,lz+1)-(lx,lz), neighbor (lx-1, lz)
                (-1, 0, top_corners[3], top_corners[0]),
                // +X edge: corners (lx+1,lz)-(lx+1,lz+1), neighbor (lx+1, lz)
                (1, 0, top_corners[1], top_corners[2]),
            ];
            let wx = ctx.origin_x + lx;
            let wz = ctx.origin_z + lz;
            let stack_bottom =
                smooth_stack_bottom(&get_column_block, wx, surface.cell_y, wz) as f32;
            for (dx, dz, a, b) in edges {
                let n_lx = lx + dx;
                let n_lz = lz + dz;
                if col(&columns, n_lx, n_lz).is_some() {
                    // 邻列也是软表面：共享角点已形成连续坡，无需裙边。
                    continue;
                }
                let n_wx = ctx.origin_x + n_lx;
                let n_wz = ctx.origin_z + n_lz;
                // 邻列若是「硬压软」，被压住的软材质不建顶面，必须把本列侧面补实，
                // 否则从坡面边缘能透视进硬块下方空洞。
                let floor = if column_has_hard_over_smooth(&get_column_block, n_wx, n_wz) {
                    stack_bottom
                } else {
                    solid_column_top_y(&get_column_block, n_wx, n_wz, surface.cell_y + 2)
                        .unwrap_or(stack_bottom)
                        .min(a.y.min(b.y))
                };
                if a.y <= floor + 0.02 && b.y <= floor + 0.02 {
                    continue;
                }
                let floor_a = Vec3::new(a.x, floor, a.z);
                let floor_b = Vec3::new(b.x, floor, b.z);
                // 外侧观察：从 edge a->b，裙边四边形 a, b, floor_b, floor_a
                let skirt = [a, b, floor_b, floor_a];
                let skirt_tex = skirt_texture(ctx, &get_column_block, wx, wz, surface, floor);
                let sn = quad_normal(skirt);
                push_smooth_quad_with_normal(mesh, skirt, sn, skirt_tex, skirt_uv_axis(dx, dz));
            }
        }
    }
}

fn skirt_texture(
    _ctx: &MeshContext<'_>,
    get_block: &dyn Fn(i32, i32, i32) -> BlockID,
    wx: i32,
    wz: i32,
    surface: SmoothColumnSurface,
    floor_y: f32,
) -> u8 {
    let sample_y = ((floor_y + surface.top_y) * 0.5).floor() as i32;
    let sample_y = sample_y.clamp(0, CHUNK_Y as i32 - 1);
    let block = get_block(wx, sample_y, wz);
    if is_smooth_granular(block) {
        properties(block).texture_index
    } else {
        properties(surface.block).texture_index
    }
}

fn skirt_uv_axis(dx: i32, dz: i32) -> usize {
    if dx != 0 {
        0 // 用 ZY
    } else if dz != 0 {
        4 // 用 XY
    } else {
        2
    }
}

fn push_smooth_quad_with_normal(
    mesh: &mut ChunkMeshCpu,
    corners: [Vec3; 4],
    normal: Vec3,
    tex: u8,
    uv_face: usize,
) {
    let base = mesh.smooth_vertices.len() as u32;
    for p in corners {
        mesh.include_smooth_vertex(p);
        mesh.smooth_vertices.push(SmoothVertex::new(
            [p.x, p.y, p.z],
            [normal.x, normal.y, normal.z],
            smooth_raw_uv(uv_face, p),
            tex,
        ));
    }
    mesh.smooth_indices
        .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
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
        // 连续高度场：1 个顶面 + 4 条 skirt，不再发射 6 个立方体面。
        assert_eq!(mesh.smooth_vertices.len(), 20);
        assert_eq!(mesh.smooth_indices.len(), 30);
    }

    #[test]
    fn smooth_granular_blends_adjacent_column_heights() {
        let mut chunk = Chunk::empty();
        chunk.set(5, 64, 5, BlockID::SAND);
        chunk.set(6, 62, 5, BlockID::SAND);
        let mesh = generate_opaque_mesh(&chunk);
        let shared_edge_y: Vec<f32> = mesh
            .smooth_vertices
            .iter()
            .filter(|v| (v.position[0] - 6.0).abs() < 1e-3)
            .map(|v| v.position[1])
            .collect();
        assert!(
            !shared_edge_y.is_empty(),
            "expected vertices on the shared x=6 edge"
        );
        // 列顶 65 与 63 应在共享边融合，而不是各自锁在整数格顶。
        assert!(
            shared_edge_y.iter().any(|y| *y > 63.2 && *y < 64.8),
            "expected blended edge heights, got {shared_edge_y:?}"
        );
        // 邻列都是软表面时不应再在共享边补直立裙边（连续顶面已覆盖）。
        let shared_skirt = mesh.smooth_indices.chunks(3).any(|tri| {
            let verts: Vec<&SmoothVertex> = tri
                .iter()
                .map(|&i| &mesh.smooth_vertices[i as usize])
                .collect();
            let on_edge = verts.iter().all(|v| (v.position[0] - 6.0).abs() < 1e-3);
            if !on_edge {
                return false;
            }
            let span = verts
                .iter()
                .map(|v| v.position[1])
                .fold(f32::NEG_INFINITY, f32::max)
                - verts
                    .iter()
                    .map(|v| v.position[1])
                    .fold(f32::INFINITY, f32::min);
            span > 1.5
        });
        assert!(
            !shared_skirt,
            "shared smooth edge should not emit a tall vertical skirt"
        );
    }

    #[test]
    fn smooth_field_stays_continuous_across_many_columns() {
        let mut chunk = Chunk::empty();
        for x in 0..8 {
            chunk.set(x, 60 + (x / 2), 4, BlockID::DIRT);
        }
        let mesh = generate_opaque_mesh(&chunk);
        assert!(mesh.smooth_vertices.len() >= 8 * 4);
        let max_y = mesh
            .smooth_vertices
            .iter()
            .map(|v| v.position[1])
            .fold(f32::NEG_INFINITY, f32::max);
        let min_y = mesh
            .smooth_vertices
            .iter()
            .map(|v| v.position[1])
            .fold(f32::INFINITY, f32::min);
        assert!(max_y - min_y > 2.0, "expected a sloping surface");
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
    fn hard_on_sand_gets_sealing_skirt_from_neighbor() {
        let mut chunk = Chunk::empty();
        // 露出的沙 + 邻列硬压沙：邻列不建软顶面，必须有 skirt 封住侧面空洞。
        chunk.set(5, 64, 5, BlockID::SAND);
        chunk.set(6, 64, 5, BlockID::SAND);
        chunk.set(6, 65, 5, BlockID::STONE);
        let mesh = generate_opaque_mesh(&chunk);
        assert!(!mesh.smooth_vertices.is_empty());
        // skirt 顶点会落到堆底附近（y≈64），而不仅是顶面 ~65。
        let has_low_skirt = mesh
            .smooth_vertices
            .iter()
            .any(|v| v.position[1] < 64.15 && (v.position[0] - 6.0).abs() < 1e-3);
        assert!(
            has_low_skirt,
            "expected a sealing skirt against hard-over-smooth neighbor"
        );
        // 角点不得高于石头底面 65。
        let shared_edge_max = mesh
            .smooth_vertices
            .iter()
            .filter(|v| (v.position[0] - 6.0).abs() < 1e-3)
            .map(|v| v.position[1])
            .fold(f32::NEG_INFINITY, f32::max);
        assert!(
            shared_edge_max <= 65.0 + 1e-3,
            "smooth surface must not pierce hard ceiling, max={shared_edge_max}"
        );
    }

    #[test]
    fn neighbor_callback_air_equivalent_to_naive() {
        let mut chunk = Chunk::empty();
        chunk.set(0, 64, 0, BlockID::STONE);
        chunk.set(15, 64, 15, BlockID::STONE_BRICKS);

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
