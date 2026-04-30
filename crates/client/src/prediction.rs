//! 客户端预测 + 协调（Reconciliation）。
//!
//! Remote Client 发送 PlayerInput 后不等 Host 确认就立即移动（预测），
//! 收到 Host 的 PlayerTick 后根据 server 位置进行回滚或平滑修正。

use glam::Vec3;

/// 未确认的输入历史（用于协调）。
pub struct InputHistory {
    entries: Vec<InputEntry>,
}

struct InputEntry {
    tick: u32,
    position: Vec3,
    yaw: f32,
    pitch: f32,
}

impl InputHistory {
    pub fn new() -> Self {
        Self { entries: Vec::new() }
    }

    /// 记录一条输入（发送前调用）。
    pub fn record(&mut self, _tick: u32, _position: Vec3, _yaw: f32, _pitch: f32) {
        // Phase 5 实现
    }

    /// Host 确认了某个 tick，移除该 tick 之前的记录并回滚不匹配的。
    pub fn reconcile(&mut self, _server_tick: u32, _server_position: Vec3) {
        // Phase 5 实现
    }
}

impl Default for InputHistory {
    fn default() -> Self {
        Self::new()
    }
}
