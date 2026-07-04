//! 通道分配与消息序列化辅助。
//!
//! 两条 DataChannel：
//! - `reliable`   (ordered + reliable):   Hello/Welcome、HostSettings、FieldRequest/FieldSnapshot、FieldDelta、FreeObjectSpawn/Project、ActionAck、Chat、PeerJoined/Left
//! - `unreliable` (unordered + 0 retransmits): PlayerInput、PlayerTick、FreeObjectState、Ping/Pong
//!
//! 选择依据见 docs/networking/protocol.md §三 通道列。

use voxweb_core::protocol::{ClientMessage, ServerMessage, decode, encode};

/// DataChannel 的语义类别。net::peer 中根据 ChannelKind 路由到对应 RTCDataChannel 实例。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelKind {
    /// ordered + reliable。状态变更类消息（必达且按序）。
    Reliable,
    /// unordered + 0 retransmits。高频快照类消息（丢了无所谓，最新即可）。
    Unreliable,
}

/// 根据 ClientMessage 类型选择 DataChannel。
pub fn channel_for_client_message(msg: &ClientMessage) -> ChannelKind {
    match msg {
        ClientMessage::PlayerInput { .. } | ClientMessage::Ping { .. } => ChannelKind::Unreliable,
        ClientMessage::Hello { .. }
        | ClientMessage::FieldRequest { .. }
        | ClientMessage::Break { .. }
        | ClientMessage::Place { .. }
        | ClientMessage::Chat { .. } => ChannelKind::Reliable,
    }
}

/// 根据 ServerMessage 类型选择 DataChannel。
pub fn channel_for_server_message(msg: &ServerMessage) -> ChannelKind {
    match msg {
        ServerMessage::PlayerTick { .. }
        | ServerMessage::FreeObjectState { .. }
        | ServerMessage::FreeObjectStateBatch { .. }
        | ServerMessage::Pong { .. } => ChannelKind::Unreliable,
        ServerMessage::Welcome { .. }
        | ServerMessage::HostSettings { .. }
        | ServerMessage::FieldSnapshot { .. }
        | ServerMessage::FieldDelta { .. }
        | ServerMessage::FreeObjectSpawn { .. }
        | ServerMessage::FreeObjectProject { .. }
        | ServerMessage::FreeObjectSpawnBatch { .. }
        | ServerMessage::FreeObjectProjectBatch { .. }
        | ServerMessage::ActionAck { .. }
        | ServerMessage::PeerJoined { .. }
        | ServerMessage::PeerLeft { .. }
        | ServerMessage::Chat { .. } => ChannelKind::Reliable,
    }
}

/// 序列化 ClientMessage 为字节数组（bincode）。
pub fn encode_client_message(msg: &ClientMessage) -> Result<Vec<u8>, bincode::error::EncodeError> {
    encode(msg)
}

/// 反序列化 ClientMessage。
pub fn decode_client_message(bytes: &[u8]) -> Result<ClientMessage, bincode::error::DecodeError> {
    decode(bytes)
}

/// 序列化 ServerMessage 为字节数组（bincode）。
pub fn encode_server_message(msg: &ServerMessage) -> Result<Vec<u8>, bincode::error::EncodeError> {
    encode(msg)
}

/// 反序列化 ServerMessage。
pub fn decode_server_message(bytes: &[u8]) -> Result<ServerMessage, bincode::error::DecodeError> {
    decode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Vec3;
    use voxweb_core::BlockID;
    use voxweb_core::chunk::Position;
    use voxweb_core::protocol::{AckReason, PlayerSnapshot};

    #[test]
    fn client_channel_selection() {
        assert_eq!(
            channel_for_client_message(&ClientMessage::Hello {
                display_name: "x".into(),
                version: 1,
            }),
            ChannelKind::Reliable
        );
        assert_eq!(
            channel_for_client_message(&ClientMessage::PlayerInput {
                tick: 0,
                position: Vec3::ZERO,
                yaw: 0.0,
                pitch: 0.0,
            }),
            ChannelKind::Unreliable
        );
        assert_eq!(
            channel_for_client_message(&ClientMessage::Ping { client_time_ms: 0 }),
            ChannelKind::Unreliable
        );
        assert_eq!(
            channel_for_client_message(&ClientMessage::Break {
                pos: Position::new(0, 0, 0),
                request_id: 0,
                input_tick: 0,
                player_position: Vec3::ZERO,
            }),
            ChannelKind::Reliable
        );
        assert_eq!(
            channel_for_client_message(&ClientMessage::FieldRequest {
                center: voxweb_core::ChunkPos::new(0, 0),
                render_distance: 2,
                chunks: vec![voxweb_core::ChunkPos::new(1, -2)],
            }),
            ChannelKind::Reliable
        );
    }

    #[test]
    fn server_channel_selection() {
        assert_eq!(
            channel_for_server_message(&ServerMessage::Welcome {
                entity_id: 1,
                server_tick: 0,
                world_seed: 0,
                host_entity_id: 1,
                host_render_distance: 6,
                players: Vec::new(),
            }),
            ChannelKind::Reliable
        );
        assert_eq!(
            channel_for_server_message(&ServerMessage::HostSettings { render_distance: 6 }),
            ChannelKind::Reliable
        );
        assert_eq!(
            channel_for_server_message(&ServerMessage::PlayerTick {
                tick: 0,
                players: vec![PlayerSnapshot {
                    entity_id: 1,
                    last_input_tick: 0,
                    position: Vec3::ZERO,
                    yaw: 0.0,
                    pitch: 0.0,
                }],
                server_time_ms: 0,
            }),
            ChannelKind::Unreliable
        );
        assert_eq!(
            channel_for_server_message(&ServerMessage::Pong {
                client_time_ms: 0,
                server_time_ms: 0,
            }),
            ChannelKind::Unreliable
        );
        assert_eq!(
            channel_for_server_message(&ServerMessage::ActionAck {
                request_id: 0,
                accepted: true,
                reason: AckReason::Ok,
            }),
            ChannelKind::Reliable
        );
    }

    #[test]
    fn encode_decode_client_roundtrip() {
        let msg = ClientMessage::Place {
            pos: Position::new(1, 2, 3),
            block: BlockID::STONE,
            request_id: 7,
            input_tick: 8,
            player_position: Vec3::new(1.0, 64.0, 1.0),
        };
        let bytes = encode_client_message(&msg).unwrap();
        let back = decode_client_message(&bytes).unwrap();
        assert_eq!(msg, back);
    }
}
