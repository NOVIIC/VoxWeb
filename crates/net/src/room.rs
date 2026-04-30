//! 房间会话状态机。
//! 管理从大厅进入房间 → 信令连接 → Peer 建连 → 进入游戏的完整流程。
//!
//! Phase 4 实现。

/// 房间状态机：管理连接生命周期的各阶段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoomState {
    /// 大厅：未开始连接
    Lobby,
    /// 正在连接信令服务
    Signaling,
    /// ICE 候选正在收集
    IceGathering,
    /// P2P 连接已建立，等待 Host 发送 Welcome
    Connecting,
    /// 游戏进行中
    InGame,
    /// 连接断开
    Disconnected,
}

/// 房间会话管理器。
pub struct RoomSession {
    pub state: RoomState,
    pub room_id: String,
}

impl RoomSession {
    pub fn new() -> Self {
        Self {
            state: RoomState::Lobby,
            room_id: String::new(),
        }
    }

    /// 开始创建房间流程（Host 端）。
    pub fn create_room(&mut self, _room_id: &str) {
        self.room_id = _room_id.to_string();
        self.state = RoomState::Signaling;
    }

    /// 开始加入房间流程（Remote 端）。
    pub fn join_room(&mut self, _room_id: &str) {
        self.room_id = _room_id.to_string();
        self.state = RoomState::Signaling;
    }
}

impl Default for RoomSession {
    fn default() -> Self {
        Self::new()
    }
}
