//! 客户端预测：挖放操作的待确认队列。
//!
//! 玩家点击挖/放时本地立即把方块改掉（乐观更新），同时把"操作 + 原方块"塞进队列，
//! 等 server 的 ActionAck 回来再决定是 commit 还是 rollback。
//!
//! Local 模式下 server 一定通过；但完整走一遍 pending/ack 协调路径，
//! 让 Phase 5 接入 Host/Remote 时不需要再回填业务逻辑。

use std::collections::HashMap;

use voxweb_core::block::BlockID;
use voxweb_core::chunk::Position;

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
