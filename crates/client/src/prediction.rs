//! 客户端预测：挖放操作的待确认队列 + 位置预测的 input history。
//!
//! - **挖放预测**（Phase 3 ✅）：玩家点击挖/放时记录 backup；ActionAck 返回后 commit 或 rollback。
//! - **位置预测**（Phase 5）：客户端每逻辑步本地立即移动，记下 InputRecord；
//!   收到 Host PlayerTick 后对比服务端权威位置与本地预测位置，
//!   误差过大则 Snap 回服务端位置。

use std::collections::{HashMap, VecDeque};

use glam::Vec3;

use voxweb_core::block::BlockID;
use voxweb_core::chunk::Position;

use crate::physics::LocalPhysics;

/// 单条待确认的操作（用于 rollback 时还原世界状态）。
#[derive(Copy, Clone, Debug)]
pub struct PendingAction {
    pub kind: PendingKind,
    pub pos: Position,
    /// 操作发生前该坐标的方块，server 拒绝时写回这一格。
    pub backup: BlockID,
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
                backup: BlockID::STONE,
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
            backup: BlockID::AIR,
        };
        p.insert(id, action);
        let rolled = p.resolve(id, false).expect("rejected 应返回 backup action");
        assert_eq!(rolled.pos, Position::new(2, 64, 2));
        assert_eq!(rolled.backup, BlockID::AIR);
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

    /// 丢弃所有 tick ≤ server_tick 的遗留记录（服务端已追上）。
    pub fn drop_until(&mut self, server_tick: u32) {
        while self.records.front().is_some_and(|r| r.tick <= server_tick) {
            self.records.pop_front();
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
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ReconcileResult {
    /// 误差可接受，不做修正。
    Ok,
    /// 误差超过 HARD_THRESHOLD，已把 physics 瞬移回服务端位置。
    Snap,
}

/// 对比服务端权威位置与本地 physics，决定是接受还是 Snap。
///
/// * `server_position` — PlayerTick 中与自己 entity_id 对应的权威位置（脚底）
/// * `server_tick` — 该 PlayerTick 的 server tick（用于清历史）
/// * `physics` — 本地物理状态（feet_position 在此被修正）
/// * `history` — 输入记录历史（用于清掉 server 已处理过的步数）
pub fn reconcile_self(
    server_position: Vec3,
    server_tick: u32,
    physics: &mut LocalPhysics,
    history: &mut InputHistory,
) -> ReconcileResult {
    history.drop_until(server_tick);

    let error = (physics.feet_position - server_position).length();

    if error >= HARD_THRESHOLD_M {
        physics.feet_position = server_position;
        ReconcileResult::Snap
    } else if error < SOFT_THRESHOLD_M {
        ReconcileResult::Ok
    } else {
        // 中等误差：Phase 5 不做软插值（Phase 7 加 blend）
        ReconcileResult::Ok
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
        history.push(1, Vec3::new(10.0, 64.0, 10.0));
        // 误差 0.05 < SOFT
        let r = reconcile_self(Vec3::new(10.03, 64.0, 10.04), 2, &mut physics, &mut history);
        assert_eq!(r, ReconcileResult::Ok);
        assert!((physics.feet_position - Vec3::new(10.0, 64.0, 10.0)).length() < 0.001);
    }

    #[test]
    fn reconcile_snaps_for_large_error() {
        let mut physics = LocalPhysics::new(Vec3::new(10.0, 64.0, 10.0));
        let mut history = InputHistory::new(10);
        history.push(1, Vec3::new(10.0, 64.0, 10.0));
        // 误差 10m > HARD → Snap
        let r = reconcile_self(Vec3::new(20.0, 64.0, 10.0), 2, &mut physics, &mut history);
        assert_eq!(r, ReconcileResult::Snap);
        assert!((physics.feet_position - Vec3::new(20.0, 64.0, 10.0)).length() < 0.001);
    }
}
