//! AABB（轴对齐包围盒）几何工具。
//!
//! 玩家与方块都用 AABB 表示碰撞体积；Phase 3 客户端物理（分轴扫动碰撞）
//! 与服务端校验（放置位置不能压在玩家身上）共用同一套定义，避免两端漂移。

use glam::Vec3;
use serde::{Deserialize, Serialize};

use crate::chunk::Position;

/// 玩家碰撞体宽度（x 与 z 方向各占 ±WIDTH/2）。
pub const PLAYER_WIDTH: f32 = 0.6;
/// 玩家碰撞体高度（从脚底到头顶）。
pub const PLAYER_HEIGHT: f32 = 1.8;
/// 眼睛相对脚底的 Y 偏移（相机位置 = feet + (0, EYE_OFFSET, 0)）。
pub const PLAYER_EYE_OFFSET: f32 = 1.62;

/// 轴对齐包围盒。`min.x <= max.x` 等约束由调用者保证。
#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Aabb {
    pub min: Vec3,
    pub max: Vec3,
}

impl Aabb {
    /// 直接由 min/max 构造。
    pub fn new(min: Vec3, max: Vec3) -> Self {
        Self { min, max }
    }

    /// 单方块 AABB：以 (pos.x, pos.y, pos.z) 为最小角的 1×1×1 立方体。
    pub fn block_at(pos: Position) -> Self {
        let p = Vec3::new(pos.x as f32, pos.y as f32, pos.z as f32);
        Self {
            min: p,
            max: p + Vec3::ONE,
        }
    }

    /// 两 AABB 是否相交（开区间，即仅触面不算相交）。
    /// 触面不算相交：玩家紧贴墙面站立时，不会把"贴着的方块"判作冲突，
    /// 这与分轴扫动碰撞中"刚好吸附"的语义一致。
    pub fn intersects(&self, other: &Aabb) -> bool {
        self.min.x < other.max.x
            && self.max.x > other.min.x
            && self.min.y < other.max.y
            && self.max.y > other.min.y
            && self.min.z < other.max.z
            && self.max.z > other.min.z
    }
}

/// 玩家 AABB：以 `feet` 为脚底中心，X/Z 各 ±WIDTH/2，Y 向上延伸 HEIGHT。
pub fn player_aabb(feet: Vec3) -> Aabb {
    let half = PLAYER_WIDTH * 0.5;
    Aabb {
        min: Vec3::new(feet.x - half, feet.y, feet.z - half),
        max: Vec3::new(feet.x + half, feet.y + PLAYER_HEIGHT, feet.z + half),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_aabb_is_unit_cube() {
        let a = Aabb::block_at(Position::new(3, 64, -2));
        assert_eq!(a.min, Vec3::new(3.0, 64.0, -2.0));
        assert_eq!(a.max, Vec3::new(4.0, 65.0, -1.0));
    }

    #[test]
    fn player_aabb_centers_on_feet_xz() {
        let a = player_aabb(Vec3::new(8.0, 70.0, 8.0));
        // x/z 各 ±0.3
        assert!((a.min.x - 7.7).abs() < 1e-6);
        assert!((a.max.x - 8.3).abs() < 1e-6);
        assert!((a.min.z - 7.7).abs() < 1e-6);
        assert!((a.max.z - 8.3).abs() < 1e-6);
        // y 从脚底向上 1.8
        assert!((a.min.y - 70.0).abs() < 1e-6);
        assert!((a.max.y - 71.8).abs() < 1e-6);
    }

    #[test]
    fn overlap_detected_when_block_inside_player() {
        // 玩家脚在 (0, 0, 0)，方块在 (0, 0, 0) → 完全重叠
        let pa = player_aabb(Vec3::ZERO);
        let ba = Aabb::block_at(Position::new(0, 0, 0));
        assert!(pa.intersects(&ba));
        assert!(ba.intersects(&pa));
    }

    #[test]
    fn touching_face_is_not_intersection() {
        // 玩家脚 x=0.7（max.x=1.0），方块在 x=1（min.x=1.0）→ 触面不相交
        let pa = player_aabb(Vec3::new(0.7, 64.0, 0.0));
        let ba = Aabb::block_at(Position::new(1, 64, 0));
        assert!(!pa.intersects(&ba));
    }

    #[test]
    fn no_overlap_when_block_far_away() {
        let pa = player_aabb(Vec3::new(0.0, 64.0, 0.0));
        let ba = Aabb::block_at(Position::new(10, 64, 10));
        assert!(!pa.intersects(&ba));
    }

    #[test]
    fn block_above_player_overlap() {
        // 脚 y=64，AABB.max.y=65.8；方块 y=65（min.y=65, max.y=66）→ y 区间 [65, 65.8) ∩ [65, 66) 非空
        let pa = player_aabb(Vec3::new(0.0, 64.0, 0.0));
        let ba = Aabb::block_at(Position::new(0, 65, 0));
        assert!(pa.intersects(&ba));
    }
}
