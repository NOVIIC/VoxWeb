//! 网络消息定义：Client/Server 间通信的三种顶层枚举。
//! 序列化使用 bincode 2.x，little-endian + varint。

use glam::Vec3;
use serde::{Deserialize, Serialize};

use crate::block::BlockID;
use crate::chunk::{ChunkPos, Position};

// —— 协议常量 ——

/// 协议版本号。Hello.version 与之不一致时 Host 拒绝接入。
/// 任何破坏性消息字段变更必须递增此版本。
pub const PROTOCOL_VERSION: u32 = 1;

/// ChunkSnapshot 单片 payload 上限（字节）。
/// 浏览器 SCTP 用户消息上限约 16 KB；保守留 14 KB，剩余给 frag_index/frag_total/bincode header。
pub const CHUNK_SNAPSHOT_PAYLOAD_MAX: usize = 14 * 1024;

/// 单个玩家实体的全局唯一 ID。Phase 5 起由 Server::add_player 分配（u32，1 起步）。
/// 0 表示"未分配"（Welcome 之前的 Remote 端使用）。
pub type EntityId = u32;

// —— 序列化工具 ——

/// 获取项目统一的 bincode 配置：little-endian + varint。
pub fn bincode_config() -> impl bincode::config::Config {
    bincode::config::standard()
        .with_little_endian()
        .with_variable_int_encoding()
}

/// 将实现了 Serialize 的消息编码为 Vec<u8>。
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, bincode::error::EncodeError> {
    bincode::serde::encode_to_vec(msg, bincode_config())
}

/// 从 &[u8] 解码出实现了 Deserialize 的消息。
pub fn decode<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
) -> Result<T, bincode::error::DecodeError> {
    let (msg, _len) = bincode::serde::decode_from_slice(bytes, bincode_config())?;
    Ok(msg)
}

// —— 消息枚举 ——

/// Client → Server 的消息（Remote Client → Host；或本地 Client → 内嵌 Server）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ClientMessage {
    /// 加入房间的初始握手
    Hello {
        display_name: String,
        /// 协议版本号，Host 可据此拒绝不兼容的客户端
        version: u32,
    },
    /// 玩家移动输入（高频，走 unreliable 通道）
    PlayerInput {
        tick: u32,
        position: Vec3,
        yaw: f32,
        pitch: f32,
    },
    /// 挖掘方块请求
    Break { pos: Position, request_id: u32 },
    /// 放置方块请求
    Place {
        pos: Position,
        block: BlockID,
        request_id: u32,
    },
    /// 文本聊天
    Chat { content: String },
    /// 心跳（防止 NAT 映射老化）
    Ping { client_time_ms: u64 },
}

/// Server → Client 的消息。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ServerMessage {
    /// 加入握手响应（分配 entity_id + 服务器当前 tick + 世界种子）
    Welcome {
        entity_id: u32,
        server_tick: u32,
        world_seed: u64,
    },
    /// 全量 Chunk 快照（分片传输，接收端按 frag_index 组装）
    ChunkSnapshot {
        pos: ChunkPos,
        frag_index: u16,
        frag_total: u16,
        payload: Vec<u8>,
    },
    /// 单方块更新（挖放结果广播）
    BlockUpdate { pos: Position, block: BlockID },
    /// 挖放请求的仲裁结果
    ActionAck {
        request_id: u32,
        accepted: bool,
        reason: AckReason,
    },
    /// 远端玩家位置广播（高频，unreliable）
    PlayerTick {
        tick: u32,
        players: Vec<PlayerSnapshot>,
        server_time_ms: u64,
    },
    /// 玩家加入房间
    PeerJoined {
        entity_id: u32,
        display_name: String,
    },
    /// 玩家离开房间
    PeerLeft { entity_id: u32 },
    /// 聊天消息广播
    Chat { from: u32, content: String },
    /// 心跳响应
    Pong {
        client_time_ms: u64,
        server_time_ms: u64,
    },
}

/// 信令层产生的事件（给 client 状态机消费）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum RoomEvent {
    Connected,
    Disconnected {
        reason: String,
    },
    PeerCount(u32),
    SignalingError(String),
    /// Phase 5：某个 Remote 离开（Host 端用，Remote 收不到）。
    /// Client 端收到后调 `host_unregister_peer(peer_id)` → eid → `server.remove_player(eid)`。
    RemoteLeft {
        peer_id: u32,
    },
    /// 该 peer 已从 P2P 切换为通过信令 Worker 中继（详见 docs/networking/signaling.md 数据中继章节）。
    /// Host 端：peer_id 为对应 Remote 的 id；Remote 端：peer_id 为 Host 的 id。
    /// 仅 UI 用于显示「中继中」徽标；不影响协议路径。
    PeerRelayed {
        peer_id: u32,
    },
}

/// PlayerTick 中携带的单个玩家快照。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlayerSnapshot {
    pub entity_id: u32,
    pub position: Vec3,
    pub yaw: f32,
    pub pitch: f32,
}

/// ActionAck 中的拒绝（或通过）原因。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AckReason {
    /// 请求通过
    Ok,
    /// 超出操作距离
    OutOfRange,
    /// 目标位置已有方块（放置时）
    BlockNotEmpty,
    /// 放置位置与玩家重叠
    Overlap,
    /// 操作冷却中
    Cooldown,
}

// —— Server 内部：出站消息 + 收信对象路由 ——

/// 一条 ServerMessage 的目标接收者。
/// 由 Server::handle_message 在 enqueue 时填入；Net 层 host_route_outbox 据此分发。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Recipient {
    /// 广播给所有玩家（含 Host 本人）。
    All,
    /// 广播给除 eid 之外的所有玩家。常用于"PeerJoined 通知其他人但不通知新加入者"。
    Except(EntityId),
    /// 仅发给单个 eid。常用于 Welcome / ActionAck / ChunkSnapshot 等单点回执。
    One(EntityId),
}

/// Server outbox 中的一项：携带 Recipient 标签的 ServerMessage。
#[derive(Clone, Debug, PartialEq)]
pub struct OutboundMessage {
    pub recipient: Recipient,
    pub message: ServerMessage,
}

// —— 测试 ——

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::BlockID;

    fn roundtrip<T: Serialize + serde::de::DeserializeOwned + std::fmt::Debug + PartialEq>(
        msg: &T,
    ) {
        let bytes = encode(msg).expect("encode failed");
        let decoded: T = decode(&bytes).expect("decode failed");
        assert_eq!(*msg, decoded);
    }

    #[test]
    fn roundtrip_client_hello() {
        roundtrip(&ClientMessage::Hello {
            display_name: "TestPlayer".into(),
            version: 1,
        });
    }

    #[test]
    fn roundtrip_break() {
        roundtrip(&ClientMessage::Break {
            pos: Position::new(10, 64, -5),
            request_id: 42,
        });
    }

    #[test]
    fn roundtrip_place() {
        roundtrip(&ClientMessage::Place {
            pos: Position::new(10, 65, -5),
            block: BlockID::STONE,
            request_id: 43,
        });
    }

    #[test]
    fn roundtrip_server_welcome() {
        roundtrip(&ServerMessage::Welcome {
            entity_id: 1,
            server_tick: 0,
            world_seed: 123456789,
        });
    }

    #[test]
    fn roundtrip_player_tick() {
        roundtrip(&ServerMessage::PlayerTick {
            tick: 60,
            players: vec![PlayerSnapshot {
                entity_id: 1,
                position: Vec3::new(1.0, 64.0, 2.0),
                yaw: 0.5,
                pitch: -0.2,
            }],
            server_time_ms: 1000,
        });
    }

    #[test]
    fn roundtrip_action_ack() {
        roundtrip(&ServerMessage::ActionAck {
            request_id: 42,
            accepted: true,
            reason: AckReason::Ok,
        });
        roundtrip(&ServerMessage::ActionAck {
            request_id: 43,
            accepted: false,
            reason: AckReason::OutOfRange,
        });
    }

    #[test]
    fn roundtrip_recipient_variants() {
        // Recipient 是 Serialize/Deserialize 派生的；走一遍确保 derive 配置正确，
        // 后续 Server outbox 序列化（如果做存档/录像）能复用。
        roundtrip(&Recipient::All);
        roundtrip(&Recipient::Except(7));
        roundtrip(&Recipient::One(42));
    }

    #[test]
    fn roundtrip_room_event_remote_left() {
        // 确保 Phase 5 新增的 RoomEvent::RemoteLeft 不破坏旧变体序列化兼容。
        roundtrip(&RoomEvent::RemoteLeft { peer_id: 1001 });
        roundtrip(&RoomEvent::Connected);
    }
}
