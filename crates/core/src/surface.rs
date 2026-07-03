//! 软颗粒材质的共享表面查询。
//!
//! 渲染、raycast 和玩家碰撞都用这里的高度规则，避免同一块沙/土在视觉与交互中
//! 出现明显不一致。当前是第一版高度场近似，不是完整 Surface Nets。

use glam::{Vec2, Vec3};

use crate::block::{BlockID, VisualClass, properties};
use crate::chunk::CHUNK_Y;

pub const SMOOTH_MIN_FILL: f32 = 0.20;
pub const SMOOTH_MAX_OVERFILL: f32 = 1.08;
const MIXED_MATERIAL_BIAS: f32 = -0.05;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SmoothCellRef {
    pub wx: i32,
    pub wy: i32,
    pub wz: i32,
    pub block: BlockID,
}

pub fn is_smooth_granular(block: BlockID) -> bool {
    block != BlockID::AIR
        && !properties(block).transparent
        && properties(block).visual_class == VisualClass::SmoothGranular
}

pub fn is_open_for_surface(block: BlockID) -> bool {
    block == BlockID::AIR || properties(block).transparent
}

pub fn smooth_cell_top_height(
    get_block: &dyn Fn(i32, i32, i32) -> BlockID,
    wx: i32,
    wy: i32,
    wz: i32,
    block: BlockID,
) -> f32 {
    let above = get_block(wx, wy + 1, wz);
    if !is_open_for_surface(above) {
        return wy as f32 + 1.0;
    }
    let h00 = smooth_corner_height(get_block, wx, wz, wy, block);
    let h10 = smooth_corner_height(get_block, wx + 1, wz, wy, block);
    let h01 = smooth_corner_height(get_block, wx, wz + 1, wy, block);
    let h11 = smooth_corner_height(get_block, wx + 1, wz + 1, wy, block);
    (h00 + h10 + h01 + h11) * 0.25
}

pub fn smooth_corner_height(
    get_block: &dyn Fn(i32, i32, i32) -> BlockID,
    corner_wx: i32,
    corner_wz: i32,
    base_y: i32,
    block: BlockID,
) -> f32 {
    let mut total = 0.0f32;
    let mut count = 0.0f32;
    for sx in (corner_wx - 1)..=(corner_wx + 1) {
        for sz in (corner_wz - 1)..=(corner_wz + 1) {
            if let Some(height) = nearby_smooth_column_height(get_block, sx, sz, base_y, block) {
                let dx = (sx - corner_wx).abs() as f32;
                let dz = (sz - corner_wz).abs() as f32;
                let weight = 1.0 / (1.0 + dx + dz);
                total += height * weight;
                count += weight;
            }
        }
    }
    if count == 0.0 {
        return base_y as f32 + 1.0;
    }
    (total / count).clamp(
        base_y as f32 + SMOOTH_MIN_FILL,
        base_y as f32 + SMOOTH_MAX_OVERFILL,
    )
}

pub fn nearby_smooth_column_height(
    get_block: &dyn Fn(i32, i32, i32) -> BlockID,
    wx: i32,
    wz: i32,
    base_y: i32,
    block: BlockID,
) -> Option<f32> {
    for y in ((base_y - 3).max(0)..=(base_y + 2).min(CHUNK_Y as i32 - 1)).rev() {
        let here = get_block(wx, y, wz);
        if !is_smooth_granular(here) {
            continue;
        }
        let above = get_block(wx, y + 1, wz);
        if !is_open_for_surface(above) {
            continue;
        }
        let material_bias = if here == block {
            0.0
        } else {
            MIXED_MATERIAL_BIAS
        };
        return Some(y as f32 + 1.0 + material_bias);
    }
    None
}

pub fn smooth_height_at(
    get_block: &dyn Fn(i32, i32, i32) -> BlockID,
    wx: i32,
    wy: i32,
    wz: i32,
    local_x: f32,
    local_z: f32,
    block: BlockID,
) -> f32 {
    let h00 = smooth_corner_height(get_block, wx, wz, wy, block);
    let h10 = smooth_corner_height(get_block, wx + 1, wz, wy, block);
    let h01 = smooth_corner_height(get_block, wx, wz + 1, wy, block);
    let h11 = smooth_corner_height(get_block, wx + 1, wz + 1, wy, block);
    let x = local_x.clamp(0.0, 1.0);
    let z = local_z.clamp(0.0, 1.0);
    let hx0 = h00 + (h10 - h00) * x;
    let hx1 = h01 + (h11 - h01) * x;
    hx0 + (hx1 - hx0) * z
}

pub fn smooth_height_normal(
    get_block: &dyn Fn(i32, i32, i32) -> BlockID,
    wx: i32,
    wy: i32,
    wz: i32,
    block: BlockID,
) -> Vec3 {
    let center = Vec2::splat(0.5);
    let e = 0.08;
    let hx0 = smooth_height_at(get_block, wx, wy, wz, center.x - e, center.y, block);
    let hx1 = smooth_height_at(get_block, wx, wy, wz, center.x + e, center.y, block);
    let hz0 = smooth_height_at(get_block, wx, wy, wz, center.x, center.y - e, block);
    let hz1 = smooth_height_at(get_block, wx, wy, wz, center.x, center.y + e, block);
    Vec3::new(-(hx1 - hx0), e * 2.0, -(hz1 - hz0))
        .try_normalize()
        .unwrap_or(Vec3::Y)
}

pub fn ray_intersect_smooth_cell(
    get_block: &dyn Fn(i32, i32, i32) -> BlockID,
    origin: Vec3,
    dir: Vec3,
    max_distance: f32,
    cell: SmoothCellRef,
) -> Option<(f32, Vec3)> {
    let top = smooth_cell_top_height(get_block, cell.wx, cell.wy, cell.wz, cell.block);
    let min_t = ray_aabb_entry_t(
        origin,
        dir,
        Vec3::new(cell.wx as f32, cell.wy as f32, cell.wz as f32),
        Vec3::new(
            cell.wx as f32 + 1.0,
            top.max(cell.wy as f32 + SMOOTH_MIN_FILL),
            cell.wz as f32 + 1.0,
        ),
    )?;
    let start = min_t.max(0.0);
    let step = 0.05;
    let mut t = start;
    while t <= max_distance {
        let p = origin + dir * t;
        if p.x >= cell.wx as f32
            && p.x <= cell.wx as f32 + 1.0
            && p.z >= cell.wz as f32
            && p.z <= cell.wz as f32 + 1.0
            && p.y >= cell.wy as f32
        {
            let h = smooth_height_at(
                get_block,
                cell.wx,
                cell.wy,
                cell.wz,
                p.x - cell.wx as f32,
                p.z - cell.wz as f32,
                cell.block,
            );
            if p.y <= h {
                return Some((
                    t,
                    smooth_height_normal(get_block, cell.wx, cell.wy, cell.wz, cell.block),
                ));
            }
        }
        t += step;
    }
    None
}

fn ray_aabb_entry_t(origin: Vec3, dir: Vec3, min: Vec3, max: Vec3) -> Option<f32> {
    let mut t_min: f32 = 0.0;
    let mut t_max = f32::INFINITY;
    for axis in 0..3 {
        let o = origin[axis];
        let d = dir[axis];
        let mn = min[axis];
        let mx = max[axis];
        if d.abs() < 1e-6 {
            if o < mn || o > mx {
                return None;
            }
            continue;
        }
        let inv = 1.0 / d;
        let mut t0 = (mn - o) * inv;
        let mut t1 = (mx - o) * inv;
        if t0 > t1 {
            std::mem::swap(&mut t0, &mut t1);
        }
        t_min = t_min.max(t0);
        t_max = t_max.min(t1);
        if t_min > t_max {
            return None;
        }
    }
    Some(t_min)
}
