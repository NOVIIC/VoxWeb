//! 客户端预测：挖放操作的待确认队列 + 位置预测的 input history。
//!
//! - **挖放预测**（Phase 3 ✅）：玩家点击挖/放时记录 backup；ActionAck 返回后 commit 或 rollback。
//! - **位置预测**（Phase 5）：客户端每逻辑步本地立即移动，记下 InputRecord；
//!   收到 Host PlayerTick 后对比服务端权威位置与本地预测位置，
//!   误差过大则 Snap 回服务端位置。

use std::collections::{HashMap, VecDeque};

use glam::Vec3;

use voxweb_core::block::{BlockID, MaterialCell};
use voxweb_core::chunk::Position;

use crate::physics::LocalPhysics;

/// 单条待确认的操作（用于 rollback 时还原世界状态）。
#[derive(Copy, Clone, Debug)]
pub struct PendingAction {
    pub kind: PendingKind,
    pub pos: Position,
    /// 操作发生前该坐标的完整 cell，server 拒绝时写回这一格。
    pub backup: MaterialCell,
}

/// 操作类型 + 对应的新方块（Place 时记录玩家想放的方块）。
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PendingKind {
    Break,
    Place(BlockID),
}

/// 全部未应答的挖放操作。
pub struct PendingActions {
    map: HashMap<u32, PendingAction>,
    next_request_id: u32,
}

impl Default for PendingActions {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
            // 0 保留给"无效"，从 1 开始
            next_request_id: 1,
        }
    }
}

impl PendingActions {
    pub fn new() -> Self {
        Self::default()
    }

    /// 申请下一个 request_id（单调递增）。
    pub fn next_request_id(&mut self) -> u32 {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        id
    }

    /// 记一条 pending（发 Break / Place 消息后立即调用）。
    pub fn insert(&mut self, request_id: u32, action: PendingAction) {
        self.map.insert(request_id, action);
    }

    /// 处理 ActionAck。
    /// - `accepted=true`：移除条目并返回 None。
    /// - `accepted=false`：移除条目并返回 backup（调用方写回 world）。
    pub fn resolve(&mut self, request_id: u32, accepted: bool) -> Option<PendingAction> {
        let action = self.map.remove(&request_id)?;
        if accepted { None } else { Some(action) }
    }

    /// 当前未应答操作数（HUD / 调试用）。
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_id_monotonic() {
        let mut p = PendingActions::new();
        assert_eq!(p.next_request_id(), 1);
        assert_eq!(p.next_request_id(), 2);
        assert_eq!(p.next_request_id(), 3);
    }

    #[test]
    fn insert_and_resolve_accepted_drops_entry() {
        let mut p = PendingActions::new();
        let id = p.next_request_id();
        p.insert(
            id,
            PendingAction {
                kind: PendingKind::Break,
                pos: Position::new(1, 64, 1),
                backup: MaterialCell::from_block_id(BlockID::STONE),
            },
        );
        assert_eq!(p.len(), 1);
        let rolled = p.resolve(id, true);
        assert!(rolled.is_none(), "accepted=true 不返回 backup");
        assert!(p.is_empty());
    }

    #[test]
    fn insert_and_resolve_rejected_returns_backup() {
        let mut p = PendingActions::new();
        let id = p.next_request_id();
        let action = PendingAction {
            kind: PendingKind::Place(BlockID::DIRT),
            pos: Position::new(2, 64, 2),
            backup: MaterialCell::EMPTY,
        };
        p.insert(id, action);
        let rolled = p.resolve(id, false).expect("rejected 应返回 backup action");
        assert_eq!(rolled.pos, Position::new(2, 64, 2));
        assert_eq!(rolled.backup, MaterialCell::EMPTY);
        assert_eq!(rolled.kind, PendingKind::Place(BlockID::DIRT));
        assert!(p.is_empty(), "resolve 后条目应被移除");
    }

    #[test]
    fn resolve_unknown_id_is_noop() {
        let mut p = PendingActions::new();
        assert!(p.resolve(999, true).is_none());
        assert!(p.resolve(999, false).is_none());
    }
}

// ────────────────────────────────────────────────────────────────────
// Phase 5：位置预测 — InputHistory + reconcile_self
// ────────────────────────────────────────────────────────────────────

/// 客户端每逻辑步（60Hz）推入的一条本地预测记录。
/// Host PlayerTick 回播时用它找回对应 tick 的本地位置，计算误差。
#[derive(Copy, Clone, Debug)]
pub struct InputRecord {
    pub tick: u32,
    pub position: Vec3,
}

/// 客户端预测的 input history 环形缓冲区。最多保留最近 120 步（= 2 秒）。
pub struct InputHistory {
    records: VecDeque<InputRecord>,
    cap: usize,
}

impl InputHistory {
    pub fn new(cap: usize) -> Self {
        Self {
            records: VecDeque::new(),
            cap,
        }
    }

    /// 推入一条本地预测记录。超过 capacity 则踢出最旧。
    pub fn push(&mut self, tick: u32, position: Vec3) {
        self.records.push_back(InputRecord { tick, position });
        while self.records.len() > self.cap {
            self.records.pop_front();
        }
    }

    /// 丢弃所有 tick ≤ input_tick 的遗留记录（服务端已确认这些输入）。
    pub fn drop_until(&mut self, input_tick: u32) {
        while self.records.front().is_some_and(|r| r.tick <= input_tick) {
            self.records.pop_front();
        }
    }

    /// 查找某个本地输入序号对应的预测位置。
    pub fn position_at(&self, input_tick: u32) -> Option<Vec3> {
        self.records
            .iter()
            .find(|r| r.tick == input_tick)
            .map(|r| r.position)
    }

    /// 服务端确认某个输入后，若发现该输入时刻存在差值，后续未确认记录也要平移同样的差值。
    /// 否则下一次回执会继续拿“旧基准线”比较，造成重复校正。
    pub fn translate_remaining(&mut self, delta: Vec3) {
        for record in &mut self.records {
            record.position += delta;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// 位置误差低于此值视为正常，不做任何修正。
pub const SOFT_THRESHOLD_M: f32 = 0.1;

/// 位置误差高于此值视为异常（瞬移 / 卡地形回弹），直接 Snap 到服务端权威位置。
pub const HARD_THRESHOLD_M: f32 = 2.0;

/// reconcile_self 的结果——方便调用方区分状态并在 HUD 上显示不同颜色。
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum ReconcileResult {
    /// 误差可接受，不做修正。
    Ok,
    /// 本地已没有对应输入 tick 的历史，通常是旧包或历史过短；忽略以避免高延迟旧回声拉扯。
    MissingHistory,
    /// 误差中等，调用方应把该差值加入 pending correction，按帧软修正。
    SoftCorrection(Vec3),
    /// 误差过大，已把当前 physics 位置按同 tick 差值立即平移。
    HardCorrection(Vec3),
}

/// 对比服务端权威位置与同一输入 tick 的本地预测记录，决定是否校正。
///
/// * `server_position` — PlayerTick 中与自己 entity_id 对应的权威位置（脚底）
/// * `acked_input_tick` — 服务端已处理到的本地 PlayerInput.tick
/// * `physics` — 本地物理状态（feet_position 在此被修正）
/// * `history` — 输入记录历史（用于查找同 tick 位置并清掉已确认输入）
pub fn reconcile_self(
    server_position: Vec3,
    acked_input_tick: u32,
    physics: &mut LocalPhysics,
    history: &mut InputHistory,
) -> ReconcileResult {
    let Some(predicted_position) = history.position_at(acked_input_tick) else {
        history.drop_until(acked_input_tick);
        return ReconcileResult::MissingHistory;
    };

    let correction = server_position - predicted_position;
    let error = correction.length();

    history.drop_until(acked_input_tick);

    if error < SOFT_THRESHOLD_M {
        ReconcileResult::Ok
    } else if error >= HARD_THRESHOLD_M {
        physics.feet_position += correction;
        history.translate_remaining(correction);
        ReconcileResult::HardCorrection(correction)
    } else {
        history.translate_remaining(correction);
        ReconcileResult::SoftCorrection(correction)
    }
}

/// 每渲染帧应用一部分待修正位移，避免中等误差造成肉眼可见瞬移。
pub fn apply_pending_position_correction(
    physics: &mut LocalPhysics,
    pending_correction: &mut Vec3,
    dt: f32,
) {
    if pending_correction.length_squared() < 0.000001 {
        *pending_correction = Vec3::ZERO;
        return;
    }

    let blend = (dt * 5.0).clamp(0.0, 1.0);
    let step = *pending_correction * blend;
    physics.feet_position += step;
    *pending_correction -= step;

    if pending_correction.length_squared() < 0.000001 {
        *pending_correction = Vec3::ZERO;
    }
}

#[cfg(test)]
mod prediction_tests {
    use super::*;

    #[test]
    fn input_history_caps_at_capacity() {
        let mut h = InputHistory::new(3);
        h.push(1, Vec3::ZERO);
        h.push(2, Vec3::ZERO);
        h.push(3, Vec3::ZERO);
        h.push(4, Vec3::ZERO);
        assert_eq!(h.records.len(), 3);
        assert_eq!(h.records[0].tick, 2);
    }

    #[test]
    fn input_history_drop_until_clears_old() {
        let mut h = InputHistory::new(10);
        h.push(1, Vec3::ZERO);
        h.push(2, Vec3::ZERO);
        h.push(3, Vec3::ZERO);
        h.drop_until(2);
        assert_eq!(h.records.len(), 1);
        assert_eq!(h.records[0].tick, 3);
    }

    #[test]
    fn reconcile_returns_ok_for_small_error() {
        let mut physics = LocalPhysics::new(Vec3::new(10.0, 64.0, 10.0));
        let mut history = InputHistory::new(10);
        history.push(2, Vec3::new(10.0, 64.0, 10.0));
        // 误差 0.05 < SOFT
        let r = reconcile_self(Vec3::new(10.03, 64.0, 10.04), 2, &mut physics, &mut history);
        assert_eq!(r, ReconcileResult::Ok);
        assert!((physics.feet_position - Vec3::new(10.0, 64.0, 10.0)).length() < 0.001);
    }

    #[test]
    fn reconcile_shifts_current_by_same_tick_error_for_large_error() {
        let mut physics = LocalPhysics::new(Vec3::new(30.0, 64.0, 10.0));
        let mut history = InputHistory::new(10);
        history.push(2, Vec3::new(10.0, 64.0, 10.0));
        history.push(3, Vec3::new(12.0, 64.0, 10.0));
        // 误差 10m > HARD → 当前预测位置按差值平移，而不是退回旧快照位置。
        let r = reconcile_self(Vec3::new(20.0, 64.0, 10.0), 2, &mut physics, &mut history);
        assert_eq!(
            r,
            ReconcileResult::HardCorrection(Vec3::new(10.0, 0.0, 0.0))
        );
        assert!((physics.feet_position - Vec3::new(40.0, 64.0, 10.0)).length() < 0.001);
        assert_eq!(history.position_at(3), Some(Vec3::new(22.0, 64.0, 10.0)));
    }

    #[test]
    fn reconcile_returns_soft_correction_for_mid_error() {
        let mut physics = LocalPhysics::new(Vec3::new(10.5, 64.0, 10.0));
        let mut history = InputHistory::new(10);
        history.push(7, Vec3::new(10.0, 64.0, 10.0));

        let r = reconcile_self(Vec3::new(10.5, 64.0, 10.0), 7, &mut physics, &mut history);
        assert_eq!(r, ReconcileResult::SoftCorrection(Vec3::new(0.5, 0.0, 0.0)));
        assert!((physics.feet_position - Vec3::new(10.5, 64.0, 10.0)).length() < 0.001);
    }

    #[test]
    fn reconcile_missing_history_does_not_snap_to_stale_echo() {
        let mut physics = LocalPhysics::new(Vec3::new(30.0, 64.0, 10.0));
        let mut history = InputHistory::new(10);

        let r = reconcile_self(Vec3::new(10.0, 64.0, 10.0), 99, &mut physics, &mut history);
        assert_eq!(r, ReconcileResult::MissingHistory);
        assert!((physics.feet_position - Vec3::new(30.0, 64.0, 10.0)).length() < 0.001);
    }
}
