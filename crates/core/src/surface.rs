//! 软颗粒材质的共享表面查询。
//!
//! 渲染、raycast 和玩家碰撞都用这里的高度规则，避免同一块沙/土在视觉与交互中
//! 出现明显不一致。
//!
//! 这一版把 SmoothGranular 当作**列顶高度场**：每个 (x,z) 只取最上方露出的软材质
//! 表面，角点在邻域列顶之间自由插值，不再把高度锁进单格 `[y, y+1]`。这样相邻
//! 高低列会连成连续斜坡，而不是一格一格的台阶立方体。

use glam::{Vec2, Vec3};

use crate::block::{BlockID, VisualClass, properties};
use crate::chunk::CHUNK_Y;

/// 角点邻域半径（列坐标）。3×3 邻域足以连接相邻列，避免把单个沙堆抹成大范围波浪。
pub const SMOOTH_CORNER_RADIUS: i32 = 1;
/// 表面相对列顶 cell 上沿的最小厚度，避免退化成零面积三角形。
pub const SMOOTH_MIN_FILL: f32 = 0.05;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SmoothCellRef {
    pub wx: i32,
    pub wy: i32,
    pub wz: i32,
    pub block: BlockID,
}

/// 一列软材质的露出表面。
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct SmoothColumnSurface {
    /// 表面列顶的世界 Y（满格时为 `cell_y + 1`）。
    pub top_y: f32,
    /// 最上方露出的软材质 cell Y。
    pub cell_y: i32,
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

/// 查找 `(wx, wz)` 列最上方露出的软材质表面。
pub fn find_smooth_column_surface(
    get_block: &dyn Fn(i32, i32, i32) -> BlockID,
    wx: i32,
    wz: i32,
) -> Option<SmoothColumnSurface> {
    for y in (0..CHUNK_Y as i32).rev() {
        let here = get_block(wx, y, wz);
        if !is_smooth_granular(here) {
            continue;
        }
        let above = get_block(wx, y + 1, wz);
        if !is_open_for_surface(above) {
            continue;
        }
        return Some(SmoothColumnSurface {
            top_y: y as f32 + 1.0,
            cell_y: y,
            block: here,
        });
    }
    None
}

/// 从表面 cell 向下找连续软材质堆的底部 Y（含）。
pub fn smooth_stack_bottom(
    get_block: &dyn Fn(i32, i32, i32) -> BlockID,
    wx: i32,
    surface_y: i32,
    wz: i32,
) -> i32 {
    let mut y = surface_y;
    while y > 0 {
        let below = get_block(wx, y - 1, wz);
        if !is_smooth_granular(below) {
            break;
        }
        y -= 1;
    }
    y
}

/// 邻列非软材质固体的顶面 Y；若整列无固体则 `None`。
pub fn solid_column_top_y(
    get_block: &dyn Fn(i32, i32, i32) -> BlockID,
    wx: i32,
    wz: i32,
    max_y: i32,
) -> Option<f32> {
    let hi = max_y.clamp(0, CHUNK_Y as i32 - 1);
    for y in (0..=hi).rev() {
        let here = get_block(wx, y, wz);
        if here == BlockID::AIR || properties(here).transparent || is_smooth_granular(here) {
            continue;
        }
        return Some(y as f32 + 1.0);
    }
    None
}

fn is_hard_opaque(block: BlockID) -> bool {
    block != BlockID::AIR && !properties(block).transparent && !is_smooth_granular(block)
}

/// 列中 `min_y` 及以上第一个硬不透明方块的底面 Y（即该方块最小角的世界 Y）。
/// 用于把软表面角点夹在硬块之下，避免高度场穿进固体。
pub fn column_hard_ceiling(
    get_block: &dyn Fn(i32, i32, i32) -> BlockID,
    wx: i32,
    wz: i32,
    min_y: i32,
) -> Option<f32> {
    let start = min_y.clamp(0, CHUNK_Y as i32 - 1);
    for y in start..CHUNK_Y as i32 {
        if is_hard_opaque(get_block(wx, y, wz)) {
            return Some(y as f32);
        }
    }
    None
}

/// 该列是否存在「硬方块压在软材质之上」——被压住的软列不建顶面，邻列必须补 skirt，
/// 否则从侧面能透视进硬块下方的空洞。
pub fn column_has_hard_over_smooth(
    get_block: &dyn Fn(i32, i32, i32) -> BlockID,
    wx: i32,
    wz: i32,
) -> bool {
    let mut seen_hard = false;
    for y in (0..CHUNK_Y as i32).rev() {
        let here = get_block(wx, y, wz);
        if is_hard_opaque(here) {
            seen_hard = true;
            continue;
        }
        if seen_hard && is_smooth_granular(here) {
            return true;
        }
        if !seen_hard && is_smooth_granular(here) {
            // 先碰到露出的软表面，不是“硬压软”
            return false;
        }
    }
    false
}

/// 兼容旧调用：某 cell 作为表面时的中心高度。
///
/// 非露出表面（上方仍是软/硬固体）时退回整格顶 `wy+1`，供地下体积碰撞使用。
pub fn smooth_cell_top_height(
    get_block: &dyn Fn(i32, i32, i32) -> BlockID,
    wx: i32,
    wy: i32,
    wz: i32,
    _block: BlockID,
) -> f32 {
    let above = get_block(wx, wy + 1, wz);
    if !is_open_for_surface(above) {
        return wy as f32 + 1.0;
    }
    let h00 = smooth_corner_height(get_block, wx, wz);
    let h10 = smooth_corner_height(get_block, wx + 1, wz);
    let h01 = smooth_corner_height(get_block, wx, wz + 1);
    let h11 = smooth_corner_height(get_block, wx + 1, wz + 1);
    let avg = (h00 + h10 + h01 + h11) * 0.25;
    avg.max(wy as f32 + SMOOTH_MIN_FILL)
}

/// 世界角点 `(corner_wx, corner_wz)` 的高度场采样。
///
/// 几何高度只由列顶决定，与草/土/沙的材质编号无关。渲染、碰撞和 raycast 必须走
/// 同一条规则，不能在交界处按各自命中的材质产生不同高度。
pub fn smooth_corner_height(
    get_block: &dyn Fn(i32, i32, i32) -> BlockID,
    corner_wx: i32,
    corner_wz: i32,
) -> f32 {
    smooth_corner_height_from_columns(
        get_block,
        &|wx, wz| find_smooth_column_surface(get_block, wx, wz),
        corner_wx,
        corner_wz,
    )
}

/// 从预取列缓存计算角点高度。渲染器用它在 chunk 邻域内复用列扫描结果；
/// 运行时查询则通过 [`smooth_corner_height`] 使用完全相同的公式。
pub fn smooth_corner_height_from_columns(
    get_block: &dyn Fn(i32, i32, i32) -> BlockID,
    get_surface: &dyn Fn(i32, i32) -> Option<SmoothColumnSurface>,
    corner_wx: i32,
    corner_wz: i32,
) -> f32 {
    let mut total = 0.0f32;
    let mut weight_sum = 0.0f32;
    for sz in (corner_wz - SMOOTH_CORNER_RADIUS)..(corner_wz + SMOOTH_CORNER_RADIUS) {
        for sx in (corner_wx - SMOOTH_CORNER_RADIUS)..(corner_wx + SMOOTH_CORNER_RADIUS) {
            let Some(surface) = get_surface(sx, sz) else {
                continue;
            };
            let dx = (sx as f32 + 0.5) - corner_wx as f32;
            let dz = (sz as f32 + 0.5) - corner_wz as f32;
            let weight = 1.0 / (1.0 + (dx * dx + dz * dz).sqrt());
            total += surface.top_y * weight;
            weight_sum += weight;
        }
    }
    if weight_sum == 0.0 {
        return 0.0;
    }

    let mut height = total / weight_sum;
    // 与渲染一致：角点不得高于触碰列的硬方块底面。
    let probe_y = ((height as i32) - 2).max(0);
    for (dx, dz) in [(0, 0), (-1, 0), (0, -1), (-1, -1)] {
        if let Some(ceil) = column_hard_ceiling(get_block, corner_wx + dx, corner_wz + dz, probe_y)
        {
            height = height.min(ceil);
        }
    }
    height
}

/// 由角点上下左右样本计算连续法线，供共享网格顶点使用。
pub fn normal_from_neighbor_heights(left: f32, right: f32, down: f32, up: f32) -> Vec3 {
    Vec3::new((left - right) * 0.5, 1.0, (down - up) * 0.5)
        .try_normalize()
        .unwrap_or(Vec3::Y)
}

pub fn smooth_height_at(
    get_block: &dyn Fn(i32, i32, i32) -> BlockID,
    wx: i32,
    _wy: i32,
    wz: i32,
    local_x: f32,
    local_z: f32,
    _block: BlockID,
) -> f32 {
    let h00 = smooth_corner_height(get_block, wx, wz);
    let h10 = smooth_corner_height(get_block, wx + 1, wz);
    let h01 = smooth_corner_height(get_block, wx, wz + 1);
    let h11 = smooth_corner_height(get_block, wx + 1, wz + 1);
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
    _block: BlockID,
) -> Vec3 {
    let center = Vec2::splat(0.5);
    let e = 0.2;
    let hx0 = smooth_height_at(get_block, wx, wy, wz, center.x - e, center.y, _block);
    let hx1 = smooth_height_at(get_block, wx, wy, wz, center.x + e, center.y, _block);
    let hz0 = smooth_height_at(get_block, wx, wy, wz, center.x, center.y - e, _block);
    let hz1 = smooth_height_at(get_block, wx, wy, wz, center.x, center.y + e, _block);
    Vec3::new(-(hx1 - hx0), e * 2.0, -(hz1 - hz0))
        .try_normalize()
        .unwrap_or(Vec3::Y)
}

/// 由四个角点高度直接做法线，避免重复邻域查询。
pub fn normal_from_corners(h00: f32, h10: f32, h01: f32, h11: f32) -> Vec3 {
    let dh_dx = ((h10 - h00) + (h11 - h01)) * 0.5;
    let dh_dz = ((h01 - h00) + (h11 - h10)) * 0.5;
    Vec3::new(-dh_dx, 1.0, -dh_dz)
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
    let above = get_block(cell.wx, cell.wy + 1, cell.wz);
    if !is_open_for_surface(above) {
        return None;
    }
    let h00 = smooth_corner_height(get_block, cell.wx, cell.wz);
    let h10 = smooth_corner_height(get_block, cell.wx + 1, cell.wz);
    let h01 = smooth_corner_height(get_block, cell.wx, cell.wz + 1);
    let h11 = smooth_corner_height(get_block, cell.wx + 1, cell.wz + 1);
    let top = h00
        .max(h10)
        .max(h01)
        .max(h11)
        .max(cell.wy as f32 + SMOOTH_MIN_FILL);
    let bottom = smooth_stack_bottom(get_block, cell.wx, cell.wy, cell.wz) as f32;
    let min_t = ray_aabb_entry_t(
        origin,
        dir,
        Vec3::new(cell.wx as f32, bottom, cell.wz as f32),
        Vec3::new(cell.wx as f32 + 1.0, top, cell.wz as f32 + 1.0),
    )?;
    let start = min_t.max(0.0);
    let step = 0.04;
    let mut t = start;
    while t <= max_distance {
        let p = origin + dir * t;
        if p.x >= cell.wx as f32
            && p.x <= cell.wx as f32 + 1.0
            && p.z >= cell.wz as f32
            && p.z <= cell.wz as f32 + 1.0
            && p.y >= bottom
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
                return Some((t, normal_from_corners(h00, h10, h01, h11)));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::BlockID;

    fn world(blocks: &[((i32, i32, i32), BlockID)]) -> impl Fn(i32, i32, i32) -> BlockID + '_ {
        move |x, y, z| {
            blocks
                .iter()
                .find(|((bx, by, bz), _)| *bx == x && *by == y && *bz == z)
                .map(|(_, b)| *b)
                .unwrap_or(BlockID::AIR)
        }
    }

    #[test]
    fn corner_height_blends_across_column_steps() {
        let get = world(&[((0, 10, 0), BlockID::SAND), ((1, 8, 0), BlockID::SAND)]);
        let h = smooth_corner_height(&get, 1, 0);
        // 列顶分别是 11 与 9，角点应落在中间而不是锁在某一格。
        assert!(h > 9.3 && h < 10.8, "h={h}");
    }

    #[test]
    fn buried_smooth_cell_keeps_full_top() {
        let get = world(&[((0, 10, 0), BlockID::DIRT), ((0, 11, 0), BlockID::GRASS)]);
        let top = smooth_cell_top_height(&get, 0, 10, 0, BlockID::DIRT);
        assert!((top - 11.0).abs() < 1e-4);
    }

    #[test]
    fn hard_over_smooth_is_detected() {
        let get = world(&[((0, 10, 0), BlockID::SAND), ((0, 11, 0), BlockID::STONE)]);
        assert!(column_has_hard_over_smooth(&get, 0, 0));
        assert!(column_hard_ceiling(&get, 0, 0, 9).is_some_and(|y| (y - 11.0).abs() < 1e-4));
    }

    #[test]
    fn corner_height_clamps_under_hard_ceiling() {
        let get = world(&[
            ((0, 10, 0), BlockID::SAND),
            ((0, 11, 0), BlockID::STONE),
            ((1, 12, 0), BlockID::SAND),
        ]);
        // 邻列更高沙会把角点往上拉，但 (0,0) 上有石头底面 y=11，必须夹住。
        let h = smooth_corner_height(&get, 1, 0);
        assert!(h <= 11.0 + 1e-3, "h={h}");
    }

    #[test]
    fn material_id_does_not_change_shared_geometry() {
        let sand = world(&[((0, 10, 0), BlockID::SAND), ((1, 10, 0), BlockID::SAND)]);
        let mixed = world(&[((0, 10, 0), BlockID::GRASS), ((1, 10, 0), BlockID::DIRT)]);
        for corner_x in 0..=2 {
            let a = smooth_corner_height(&sand, corner_x, 0);
            let b = smooth_corner_height(&mixed, corner_x, 0);
            assert!((a - b).abs() < 1e-5, "x={corner_x}: {a} != {b}");
        }
    }

    #[test]
    fn prefetched_columns_match_direct_world_query() {
        let get = world(&[
            ((0, 10, 0), BlockID::GRASS),
            ((1, 9, 0), BlockID::DIRT),
            ((0, 8, 1), BlockID::SAND),
        ]);
        let direct = smooth_corner_height(&get, 1, 1);
        let cached = smooth_corner_height_from_columns(
            &get,
            &|x, z| find_smooth_column_surface(&get, x, z),
            1,
            1,
        );
        assert!((direct - cached).abs() < 1e-5);
    }
}
