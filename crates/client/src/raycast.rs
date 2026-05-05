//! DDA 射线检测：沿视线方向查找目标方块。
//!
//! 使用 Amanatides & Woo 算法进行体素遍历。

use glam::Vec3;

use voxweb_core::chunk::Position;

/// DDA 射线检测结果。
pub struct RaycastHit {
    /// 命中方块的坐标
    pub pos: Position,
    /// 命中面的法线（用于计算放置位置）
    pub normal: glam::IVec3,
    /// 从射线起点到命中点的距离
    pub distance: f32,
}

/// 从 origin 沿 direction 发射射线，在 max_distance 内查找第一个非空气方块。
/// 返回命中信息。Phase 3 实现完整 DDA。
pub fn raycast(_origin: Vec3, _direction: Vec3, _max_distance: f32) -> Option<RaycastHit> {
    // Phase 3: DDA 迭代实现
    None
}
