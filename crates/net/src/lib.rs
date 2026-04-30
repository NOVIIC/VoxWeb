//! VoxWeb P2P 网络层。
//! 负责信令 WebSocket 通信、WebRTC PeerConnection 管理与 DataChannel 双通道传输。

pub mod peer;
pub mod room;
pub mod signaling;
pub mod transport;

use voxweb_core::protocol::{ClientMessage, ServerMessage};

/// 网络端点：对应三种角色。
pub enum NetEndpoint {
    /// 单人模式：不联网，消息走内存通道
    Local,
    /// 房主模式：托管 WebRTC 连接 + 接收所有 Remote Client 消息
    Host,
    /// 远端客户端模式：连接 Host 的单条 WebRTC 连接
    Remote,
}

impl NetEndpoint {
    /// 创建一个 Local-Only 端点。
    pub fn new_local() -> Self {
        NetEndpoint::Local
    }

    /// 发送一条 ClientMessage（本地通过内存；远程通过 DataChannel）。
    pub fn send_client_message(&self, _msg: ClientMessage) {
        // Phase 4+ 实现
    }

    /// 发送一条 ServerMessage（仅 Host 角色有效）。
    pub fn send_server_message(&self, _entity_id: u32, _msg: ServerMessage) {
        // Phase 4+ 实现
    }
}
