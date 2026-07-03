//! DDA 射线检测：沿视线方向查找目标方块。
//!
//! 使用 Amanatides & Woo 网格步进算法。
//! 命中条件：方块非 AIR 且 `properties().solid == true`。

use glam::{IVec3, Vec3};

use voxweb_core::block::{BlockID, properties};
use voxweb_core::chunk::Position;
use voxweb_core::geometry::Aabb;
use voxweb_core::object::ObjectID;
use voxweb_core::{SmoothCellRef, is_smooth_granular, ray_intersect_smooth_cell};
use voxweb_render::vertex::Face;

/// DDA 射线检测结果。
#[derive(Copy, Clone, Debug)]
pub struct RaycastHit {
    /// 命中方块的世界坐标
    pub pos: Position,
    /// 命中面的法线（指向命中点所在的外侧方块），用于放置位置计算
    pub normal: IVec3,
    /// 命中面（PosX / NegY / ...）
    pub face: Face,
    /// 命中方块的 BlockID
    pub block: BlockID,
    /// 从射线起点到命中点的距离（米）
    pub distance: f32,
    /// 精确命中点。硬方块为进入体素面的点；软材质为高度场表面点。
    pub point: Vec3,
    /// 精确表面法线。硬方块为轴向法线；软材质可为斜坡法线。
    pub surface_normal: Vec3,
    /// 命中动态 FreeObject 时为 Some；静态 cell 命中时为 None。
    pub object_id: Option<ObjectID>,
    /// 动态对象当前 AABB。静态 cell 命中时为 None。
    pub object_aabb: Option<Aabb>,
}

#[derive(Copy, Clone, Debug)]
pub struct DynamicObjectRaycast {
    pub object_id: ObjectID,
    pub aabb: Aabb,
    pub block: BlockID,
}

/// 从 `origin` 沿 `direction` 发射射线，在 `max_distance` 内查找第一个 solid 方块。
///
/// `get_block` 是世界坐标 → BlockID 的查询闭包（未加载/越界返回 AIR 即可）。
/// 返回 None 表示射程内未命中任何 solid 方块。
pub fn raycast(
    origin: Vec3,
    direction: Vec3,
    max_distance: f32,
    get_block: &dyn Fn(i32, i32, i32) -> BlockID,
    dynamic_objects: &[DynamicObjectRaycast],
) -> Option<RaycastHit> {
    let static_hit = raycast_static(origin, direction, max_distance, get_block);
    let dynamic_hit = raycast_dynamic(origin, direction, max_distance, dynamic_objects);
    match (static_hit, dynamic_hit) {
        (Some(static_hit), Some(dynamic_hit)) => {
            if dynamic_hit.distance < static_hit.distance {
                Some(dynamic_hit)
            } else {
                Some(static_hit)
            }
        }
        (Some(hit), None) | (None, Some(hit)) => Some(hit),
        (None, None) => None,
    }
}

fn raycast_static(
    origin: Vec3,
    direction: Vec3,
    max_distance: f32,
    get_block: &dyn Fn(i32, i32, i32) -> BlockID,
) -> Option<RaycastHit> {
    let dir = direction.normalize_or_zero();
    if dir.length_squared() < 1e-8 {
        return None;
    }

    // 当前所在体素（按整数坐标）
    let mut current = IVec3::new(
        origin.x.floor() as i32,
        origin.y.floor() as i32,
        origin.z.floor() as i32,
    );

    // 步进方向：+1 / -1 / 0（dir 分量为零时该轴永不步进）
    let step = IVec3::new(
        if dir.x > 0.0 {
            1
        } else if dir.x < 0.0 {
            -1
        } else {
            0
        },
        if dir.y > 0.0 {
            1
        } else if dir.y < 0.0 {
            -1
        } else {
            0
        },
        if dir.z > 0.0 {
            1
        } else if dir.z < 0.0 {
            -1
        } else {
            0
        },
    );

    // 每跨越一个体素需要的 t 增量（射线长度参数）
    let t_delta = Vec3::new(
        if dir.x != 0.0 {
            1.0 / dir.x.abs()
        } else {
            f32::INFINITY
        },
        if dir.y != 0.0 {
            1.0 / dir.y.abs()
        } else {
            f32::INFINITY
        },
        if dir.z != 0.0 {
            1.0 / dir.z.abs()
        } else {
            f32::INFINITY
        },
    );

    // 到下一个网格平面的 t 值
    let mut t_max = Vec3::new(
        next_boundary_t(origin.x, dir.x),
        next_boundary_t(origin.y, dir.y),
        next_boundary_t(origin.z, dir.z),
    );

    // 起点格子若已命中（origin 恰好在某 solid 方块内），face 含糊；返回时按"最后一次步进"或默认 +Y。
    // 实际上玩家眼睛在 AIR 里，几乎不会出现这种情况；保留兜底。
    if let Some(block) = block_solid(get_block, current)
        && origin.x.floor() == current.x as f32
    {
        // 起点格命中：normal 为零向量、face 任取（这里给 PosY）
        return Some(RaycastHit {
            pos: Position::new(current.x, current.y, current.z),
            normal: IVec3::ZERO,
            face: Face::PosY,
            block,
            distance: 0.0,
            point: origin,
            surface_normal: Vec3::Y,
            object_id: None,
            object_aabb: None,
        });
    }

    // 上一次踏入新体素时使用的轴（用于命中后推算 face）
    let mut last_axis: Axis;
    let mut traveled: f32;

    loop {
        // 选 t_max 三者中最小的轴推进
        if t_max.x < t_max.y && t_max.x < t_max.z {
            traveled = t_max.x;
            current.x += step.x;
            t_max.x += t_delta.x;
            last_axis = Axis::X;
        } else if t_max.y < t_max.z {
            traveled = t_max.y;
            current.y += step.y;
            t_max.y += t_delta.y;
            last_axis = Axis::Y;
        } else {
            traveled = t_max.z;
            current.z += step.z;
            t_max.z += t_delta.z;
            last_axis = Axis::Z;
        }

        if traveled > max_distance {
            return None;
        }

        if let Some(block) = block_solid(get_block, current) {
            // 命中面 = 进入该体素时穿过的那个面
            // 例如沿 +X 步进进入新体素 → 命中的是新体素的 NegX 面，normal 指向 -X
            let (face, normal) = face_and_normal(last_axis, step);
            if is_smooth_granular(block) {
                if let Some((smooth_distance, surface_normal)) = ray_intersect_smooth_cell(
                    get_block,
                    origin,
                    dir,
                    max_distance,
                    SmoothCellRef {
                        wx: current.x,
                        wy: current.y,
                        wz: current.z,
                        block,
                    },
                ) {
                    return Some(RaycastHit {
                        pos: Position::new(current.x, current.y, current.z),
                        normal: IVec3::new(0, 1, 0),
                        face: Face::PosY,
                        block,
                        distance: smooth_distance,
                        point: origin + dir * smooth_distance,
                        surface_normal,
                        object_id: None,
                        object_aabb: None,
                    });
                }
                continue;
            }
            return Some(RaycastHit {
                pos: Position::new(current.x, current.y, current.z),
                normal,
                face,
                block,
                distance: traveled,
                point: origin + dir * traveled,
                surface_normal: normal.as_vec3(),
                object_id: None,
                object_aabb: None,
            });
        }
    }
}

fn raycast_dynamic(
    origin: Vec3,
    direction: Vec3,
    max_distance: f32,
    dynamic_objects: &[DynamicObjectRaycast],
) -> Option<RaycastHit> {
    let dir = direction.normalize_or_zero();
    if dir.length_squared() < 1e-8 {
        return None;
    }
    dynamic_objects
        .iter()
        .filter_map(|object| {
            let (distance, normal) = ray_aabb_hit(origin, dir, object.aabb)?;
            if distance > max_distance {
                return None;
            }
            let point = origin + dir * distance;
            let face = face_from_normal(normal);
            Some(RaycastHit {
                pos: Position::new(
                    object.aabb.min.x.floor() as i32,
                    object.aabb.min.y.floor() as i32,
                    object.aabb.min.z.floor() as i32,
                ),
                normal,
                face,
                block: object.block,
                distance,
                point,
                surface_normal: normal.as_vec3(),
                object_id: Some(object.object_id),
                object_aabb: Some(object.aabb),
            })
        })
        .min_by(|a, b| a.distance.total_cmp(&b.distance))
}

fn ray_aabb_hit(origin: Vec3, dir: Vec3, aabb: Aabb) -> Option<(f32, IVec3)> {
    let mut t_min = 0.0f32;
    let mut t_max = f32::INFINITY;
    let mut normal = IVec3::Y;

    for axis in 0..3 {
        let o = origin[axis];
        let d = dir[axis];
        let mn = aabb.min[axis];
        let mx = aabb.max[axis];
        if d.abs() < 1e-6 {
            if o < mn || o > mx {
                return None;
            }
            continue;
        }

        let inv = 1.0 / d;
        let mut t0 = (mn - o) * inv;
        let mut t1 = (mx - o) * inv;
        let mut axis_normal = match axis {
            0 if d > 0.0 => IVec3::new(-1, 0, 0),
            0 => IVec3::new(1, 0, 0),
            1 if d > 0.0 => IVec3::new(0, -1, 0),
            1 => IVec3::new(0, 1, 0),
            2 if d > 0.0 => IVec3::new(0, 0, -1),
            _ => IVec3::new(0, 0, 1),
        };
        if t0 > t1 {
            std::mem::swap(&mut t0, &mut t1);
            axis_normal = -axis_normal;
        }
        if t0 > t_min {
            t_min = t0;
            normal = axis_normal;
        }
        t_max = t_max.min(t1);
        if t_min > t_max {
            return None;
        }
    }

    Some((t_min.max(0.0), normal))
}

fn face_from_normal(normal: IVec3) -> Face {
    match (normal.x, normal.y, normal.z) {
        (1, 0, 0) => Face::PosX,
        (-1, 0, 0) => Face::NegX,
        (0, 1, 0) => Face::PosY,
        (0, -1, 0) => Face::NegY,
        (0, 0, 1) => Face::PosZ,
        _ => Face::NegZ,
    }
}

#[derive(Copy, Clone)]
enum Axis {
    X,
    Y,
    Z,
}

/// 给定起点 `s` 和方向分量 `d`，返回射线参数 t 使得 `s + t*d` 抵达下一个整数网格面。
fn next_boundary_t(s: f32, d: f32) -> f32 {
    if d > 0.0 {
        // 下一个 +X 整数面
        let next = s.floor() + 1.0;
        (next - s) / d
    } else if d < 0.0 {
        // 下一个 -X 整数面
        let next = s.floor();
        (next - s) / d
    } else {
        f32::INFINITY
    }
}

/// 该格子的方块若 solid（非 AIR 且 properties.solid=true），返回 Some(block)。
fn block_solid(get_block: &dyn Fn(i32, i32, i32) -> BlockID, pos: IVec3) -> Option<BlockID> {
    let block = get_block(pos.x, pos.y, pos.z);
    if block == BlockID::AIR {
        return None;
    }
    if !properties(block).solid {
        return None;
    }
    Some(block)
}

/// 根据进入方向轴 + 步进符号，推算命中面 + 法线（向外）。
fn face_and_normal(axis: Axis, step: IVec3) -> (Face, IVec3) {
    match axis {
        Axis::X => {
            if step.x > 0 {
                (Face::NegX, IVec3::new(-1, 0, 0))
            } else {
                (Face::PosX, IVec3::new(1, 0, 0))
            }
        }
        Axis::Y => {
            if step.y > 0 {
                (Face::NegY, IVec3::new(0, -1, 0))
            } else {
                (Face::PosY, IVec3::new(0, 1, 0))
            }
        }
        Axis::Z => {
            if step.z > 0 {
                (Face::NegZ, IVec3::new(0, 0, -1))
            } else {
                (Face::PosZ, IVec3::new(0, 0, 1))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在 x=3, y=64, z=0 处放一个 STONE 方块，其它 AIR。
    fn single_block_at(target: (i32, i32, i32)) -> impl Fn(i32, i32, i32) -> BlockID {
        move |x, y, z| {
            if (x, y, z) == target {
                BlockID::STONE
            } else {
                BlockID::AIR
            }
        }
    }

    #[test]
    fn raycast_positive_x_hits_negx_face() {
        let getter = single_block_at((3, 64, 0));
        let hit = raycast(
            Vec3::new(0.5, 64.5, 0.5),
            Vec3::new(1.0, 0.0, 0.0),
            10.0,
            &getter,
            &[],
        )
        .expect("应命中 (3,64,0)");
        assert_eq!(hit.pos, Position::new(3, 64, 0));
        assert_eq!(hit.face, Face::NegX);
        assert_eq!(hit.normal, IVec3::new(-1, 0, 0));
        // 从 x=0.5 沿 +X 到 x=3 共 2.5 米
        assert!(
            (hit.distance - 2.5).abs() < 1e-3,
            "distance={}",
            hit.distance
        );
    }

    #[test]
    fn raycast_negative_y_hits_posy_face() {
        let getter = single_block_at((0, 64, 0));
        let hit = raycast(
            Vec3::new(0.5, 70.5, 0.5),
            Vec3::new(0.0, -1.0, 0.0),
            20.0,
            &getter,
            &[],
        )
        .expect("应命中 (0,64,0)");
        assert_eq!(hit.pos, Position::new(0, 64, 0));
        assert_eq!(hit.face, Face::PosY);
        assert_eq!(hit.normal, IVec3::new(0, 1, 0));
        // 从 y=70.5 沿 -Y 到 y=65（方块顶面）= 5.5 米
        assert!(
            (hit.distance - 5.5).abs() < 1e-3,
            "distance={}",
            hit.distance
        );
    }

    #[test]
    fn raycast_out_of_range_returns_none() {
        let getter = single_block_at((100, 64, 0));
        let hit = raycast(
            Vec3::new(0.5, 64.5, 0.5),
            Vec3::new(1.0, 0.0, 0.0),
            6.0, // 远小于 100
            &getter,
            &[],
        );
        assert!(hit.is_none());
    }

    #[test]
    fn raycast_skips_non_solid_blocks() {
        // 把 (2, 64, 0) 设为 WATER（透明 + 非 solid），(3, 64, 0) 设为 STONE
        let getter = |x: i32, y: i32, z: i32| {
            if (x, y, z) == (2, 64, 0) {
                BlockID::WATER
            } else if (x, y, z) == (3, 64, 0) {
                BlockID::STONE
            } else {
                BlockID::AIR
            }
        };
        let hit = raycast(
            Vec3::new(0.5, 64.5, 0.5),
            Vec3::new(1.0, 0.0, 0.0),
            10.0,
            &getter,
            &[],
        )
        .expect("应穿过 WATER 命中 STONE");
        assert_eq!(hit.pos, Position::new(3, 64, 0));
    }

    #[test]
    fn raycast_zero_direction_returns_none() {
        let getter = single_block_at((0, 0, 0));
        let hit = raycast(Vec3::new(0.5, 64.5, 0.5), Vec3::ZERO, 10.0, &getter, &[]);
        assert!(hit.is_none());
    }

    #[test]
    fn raycast_smooth_granular_returns_surface_distance() {
        let getter = |x, y, z| {
            if (x, y, z) == (0, 64, 0) {
                BlockID::SAND
            } else {
                BlockID::AIR
            }
        };
        let hit = raycast(
            Vec3::new(0.5, 70.0, 0.5),
            Vec3::new(0.0, -1.0, 0.0),
            10.0,
            &getter,
            &[],
        )
        .expect("应命中沙面");
        assert_eq!(hit.pos, Position::new(0, 64, 0));
        assert_eq!(hit.face, Face::PosY);
        assert!((hit.point.y - 65.0).abs() < 0.06, "point={:?}", hit.point);
        assert!(hit.surface_normal.y > 0.8);
    }

    #[test]
    fn raycast_dynamic_object_wins_when_closer_than_static_block() {
        let getter = single_block_at((5, 64, 0));
        let dynamic = [DynamicObjectRaycast {
            object_id: 9,
            aabb: Aabb::new(Vec3::new(2.0, 64.0, 0.0), Vec3::new(3.0, 65.0, 1.0)),
            block: BlockID::STONE_BRICKS,
        }];
        let hit = raycast(
            Vec3::new(0.5, 64.5, 0.5),
            Vec3::new(1.0, 0.0, 0.0),
            10.0,
            &getter,
            &dynamic,
        )
        .expect("应命中更近的动态对象");
        assert_eq!(hit.object_id, Some(9));
        assert_eq!(hit.block, BlockID::STONE_BRICKS);
        assert!((hit.distance - 1.5).abs() < 1e-3);
    }
}
