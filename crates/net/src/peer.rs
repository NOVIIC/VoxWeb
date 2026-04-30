//! WebRTC PeerConnection 包装。
//! 管理与单个对等端的 RTCPeerConnection 和两条 DataChannel。
//!
//! Phase 4 实现。

// Phase 4: use voxweb_core::protocol::{ClientMessage, ServerMessage};

/// 与一个对等端的 P2P 连接。
pub struct PeerConnection {
    pub peer_id: u32,
    // Phase 4: RTCPeerConnection, reliable DC, unreliable DC
}

impl PeerConnection {
    /// 创建发起方（Host 端）的 PeerConnection + DataChannel。
    pub fn create_offerer(_peer_id: u32) -> Self {
        Self { peer_id: _peer_id }
    }

    /// 创建应答方（Remote 端）的 PeerConnection（DataChannel 由 ondatachannel 接收）。
    pub fn create_answerer(_peer_id: u32) -> Self {
        Self { peer_id: _peer_id }
    }
}
