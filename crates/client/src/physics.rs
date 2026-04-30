//! 客户端物理：本地 AABB 碰撞 + 重力 + 预测移动。
//!
//! 注意：服务端权威物理在 `server::physics` 中。
//! 此处仅作客户端预测，Host 发回的 PlayerTick 会进行协调。

use glam::Vec3;

/// 客户端玩家物理体。
pub struct LocalPhysics {
    pub position: Vec3,
    pub velocity: Vec3,
    pub on_ground: bool,
    pub width: f32,
    pub height: f32,
}

impl Default for LocalPhysics {
    fn default() -> Self {
        Self {
            position: Vec3::new(0.0, 65.0, 0.0),
            velocity: Vec3::ZERO,
            on_ground: false,
            width: 0.6,
            height: 1.8,
        }
    }
}

impl LocalPhysics {
    /// 每帧物理步进（dt 秒）。Phase 3 实现完整碰撞。
    pub fn step(&mut self, _dt: f32) {
        // Phase 3: 重力、碰撞检测、跳跃
    }
}
