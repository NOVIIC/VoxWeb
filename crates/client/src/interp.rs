//! 远端玩家位置插值。
//!
//! 维护每个远端玩家的位置快照缓冲区，
//! 在当前渲染时间减去固定延迟的过去时刻插值，产生平滑的远端移动。

use glam::Vec3;

/// 插值缓冲管理。
pub struct InterpolationBuffer {
    /// 插值延迟（秒），典型值 0.05-0.1
    pub delay: f32,
}

impl Default for InterpolationBuffer {
    fn default() -> Self {
        Self { delay: 0.1 }
    }
}

impl InterpolationBuffer {
    /// 接收一条新的远端位置快照。
    pub fn push_snapshot(
        &mut self,
        _entity_id: u32,
        _tick: u32,
        _position: Vec3,
        _yaw: f32,
        _pitch: f32,
    ) {
        // Phase 5 实现
    }

    /// 在给定时间点获取插值后的远端玩家位置。
    pub fn get_interpolated(&self, _entity_id: u32, _render_time: f64) -> Option<(Vec3, f32, f32)> {
        // Phase 5 实现
        None
    }
}
