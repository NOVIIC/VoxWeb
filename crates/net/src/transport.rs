//! 通道分配：管理 reliable / unreliable DataChannel 的消息路由。
//!
//! 两条 DataChannel：
//! - reliable (ordered):   ChunkSnapshot / BlockUpdate / Chat / Join/Leave
//! - unreliable (unordered): PlayerTick (60Hz)
//!
//! Phase 4 实现。

use voxweb_core::protocol::{ClientMessage, ServerMessage};

/// 根据消息类型决定走哪条 DataChannel。
pub fn channel_for_client_message(msg: &ClientMessage) -> DataChannelKind {
    match msg {
        ClientMessage::PlayerInput { .. } => DataChannelKind::Unreliable,
        _ => DataChannelKind::Reliable,
    }
}

/// 根据消息类型决定走哪条 DataChannel。
pub fn channel_for_server_message(msg: &ServerMessage) -> DataChannelKind {
    match msg {
        ServerMessage::PlayerTick { .. } => DataChannelKind::Unreliable,
        _ => DataChannelKind::Reliable,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataChannelKind {
    Reliable,
    Unreliable,
}
