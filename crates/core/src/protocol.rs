//! 网络消息定义：Client/Server 间通信的三种顶层枚举。
//! 序列化使用 bincode 2.x，little-endian + varint。

use glam::Vec3;
use serde::{Deserialize, Serialize};

use crate::block::{BlockID, MaterialCell};
use crate::chunk::{ChunkPos, Position};
use crate::object::ObjectID;

// —— 协议常量 ——

/// 协议版本号。Hello.version 与之不一致时 Host 拒绝接入。
/// 任何破坏性消息字段变更必须递增此版本。
///
/// v10：新增 `FreeObjectSpawnBatch/StateBatch/ProjectBatch`；软材质颗粒（沙/土/草）
///      改为批量 active FreeObject 下落动画。
/// v9：FreeObject 升级为 Spawn/State/Project 动态生命周期。
pub const PROTOCOL_VERSION: u32 = 10;

/// FieldSnapshot 单片 payload 上限（字节）。
/// 浏览器 SCTP 用户消息上限约 16 KB；保守留 14 KB，剩余给 frag_index/frag_total/bincode header。
pub const FIELD_SNAPSHOT_PAYLOAD_MAX: usize = 14 * 1024;

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
    let (msg, len) = bincode::serde::decode_from_slice(bytes, bincode_config())?;
    if len != bytes.len() {
        return Err(bincode::error::DecodeError::OtherString(format!(
            "trailing bytes after message: consumed {len}, total {}",
            bytes.len()
        )));
    }
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
    /// 请求 Host 发送指定 FieldChunk 快照。
    ///
    /// Remote 端只保存自己视距范围内的区块；当玩家进入新 chunk 或渲染距离变化时，
    /// 客户端计算缺失区块列表并通过 reliable 通道请求。Host 收到后按需生成，
    /// 再用 [`ServerMessage::FieldSnapshot`] 分片回传。
    FieldRequest {
        /// Remote 当前用于计算 desired 集合的中心 chunk。
        center: ChunkPos,
        /// Remote 已经按 Host 上限裁剪后的有效视距。
        render_distance: u32,
        /// 本次请求的缺失 chunk 列表。
        chunks: Vec<ChunkPos>,
    },
    /// 挖掘方块请求
    Break {
        pos: Position,
        request_id: u32,
        /// 玩家点击时的本地输入序号，用于让服务端把操作与预测历史对齐。
        input_tick: u32,
        /// 玩家点击时的脚底位置。可靠操作包可能先于最新 PlayerInput 到达，
        /// 服务端用它做范围校验，避免高 RTT 下误判 OutOfRange。
        player_position: Vec3,
    },
    /// 放置方块请求
    Place {
        pos: Position,
        block: BlockID,
        request_id: u32,
        /// 玩家点击时的本地输入序号。
        input_tick: u32,
        /// 玩家点击时的脚底位置，用于范围与玩家 AABB 重叠校验。
        player_position: Vec3,
    },
    /// 文本聊天
    Chat { content: String },
    /// 心跳（防止 NAT 映射老化）
    Ping { client_time_ms: u64 },
}

/// Server → Client 的消息。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ServerMessage {
    /// 加入握手响应（分配 entity_id + 服务器当前 tick + 世界种子 + 房间内现存玩家名单）
    ///
    /// Phase 6（v2）扩展：
    /// - `host_entity_id`：房间主机的 eid（用于 UI 标 "（主机）" 标签）
    /// - `players`：包括本人在内的全员名单，避免 Remote 错过早于自己加入的玩家信息
    Welcome {
        entity_id: u32,
        server_tick: u32,
        world_seed: u64,
        host_entity_id: u32,
        /// Host 当前允许 Remote 使用的最大视距（单位：chunk）。
        host_render_distance: u32,
        players: Vec<PlayerEntry>,
    },
    /// 全量 FieldChunk 快照（分片传输，接收端按 frag_index 组装）
    FieldSnapshot {
        pos: ChunkPos,
        frag_index: u16,
        frag_total: u16,
        payload: Vec<u8>,
    },
    /// 单 cell 更新（挖放结果广播）
    FieldDelta { pos: Position, cell: MaterialCell },
    /// 从静态场提取出的动态材质团。Host 已经把这些 cell 从 MaterialField 移除。
    FreeObjectSpawn {
        object_id: ObjectID,
        cells: Vec<(Position, MaterialCell)>,
    },
    /// 动态 FreeObject 的权威状态。Remote 只应用 Host 状态，不决定最终落点。
    FreeObjectState {
        object_id: ObjectID,
        position: Vec3,
        velocity: Vec3,
    },
    /// FreeObject 静止后投影回静态场。
    FreeObjectProject {
        object_id: ObjectID,
        deltas: Vec<(Position, MaterialCell)>,
    },
    /// 批量提取软材质颗粒（sand/dirt/grass）。一帧内可能同时提取多个 grain，
    /// 合并成一条 reliable 消息避免消息风暴。语义等价于多条 `FreeObjectSpawn`。
    FreeObjectSpawnBatch {
        spawns: Vec<(ObjectID, Vec<(Position, MaterialCell)>)>,
    },
    /// 批量动态 FreeObject 权威状态。每 tick 把所有活跃 grain 的状态合并成一条
    /// unreliable 消息。语义等价于多条 `FreeObjectState`。
    FreeObjectStateBatch { states: Vec<(ObjectID, Vec3, Vec3)> },
    /// 批量投影：多个 grain 在同一帧落定时合并成一条 reliable 消息。
    /// 语义等价于多条 `FreeObjectProject`。
    FreeObjectProjectBatch {
        projections: Vec<(ObjectID, Vec<(Position, MaterialCell)>)>,
    },
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
    /// Host 视距变化。Remote 收到后更新自身有效视距上限。
    HostSettings { render_distance: u32 },
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
    /// 服务端已接受到该玩家的最新 PlayerInput.tick。
    /// 客户端收到自己的快照时用它查本地 InputHistory，而不是用 server tick 对齐。
    pub last_input_tick: u32,
    pub position: Vec3,
    pub yaw: f32,
    pub pitch: f32,
}

/// Welcome 中携带的单个玩家身份（Phase 6 起）。
///
/// 不包含位置 / 朝向：那些信息会在下一帧 PlayerTick 广播中带到；
/// Welcome 只承担 "新加入者一次性拿到当前 roster" 的职责。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlayerEntry {
    pub entity_id: EntityId,
    pub display_name: String,
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
    /// 仅发给单个 eid。常用于 Welcome / ActionAck / FieldSnapshot 等单点回执。
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
            input_tick: 9,
            player_position: Vec3::new(8.0, 65.0, 8.0),
        });
    }

    #[test]
    fn roundtrip_place() {
        roundtrip(&ClientMessage::Place {
            pos: Position::new(10, 65, -5),
            block: BlockID::STONE,
            request_id: 43,
            input_tick: 10,
            player_position: Vec3::new(8.0, 65.0, 8.0),
        });
    }

    #[test]
    fn roundtrip_server_welcome() {
        roundtrip(&ServerMessage::Welcome {
            entity_id: 1,
            server_tick: 0,
            world_seed: 123456789,
            host_entity_id: 1,
            host_render_distance: 6,
            players: Vec::new(),
        });
    }

    #[test]
    fn roundtrip_welcome_with_roster() {
        // Phase 6：Welcome 携带 host_entity_id + 完整 roster（含本人）。
        roundtrip(&ServerMessage::Welcome {
            entity_id: 3,
            server_tick: 600,
            world_seed: 0xDEADBEEF,
            host_entity_id: 1,
            host_render_distance: 6,
            players: vec![
                PlayerEntry {
                    entity_id: 1,
                    display_name: "Alice".into(),
                },
                PlayerEntry {
                    entity_id: 2,
                    display_name: "Bob".into(),
                },
                PlayerEntry {
                    entity_id: 3,
                    display_name: "Carol".into(),
                },
            ],
        });
    }

    #[test]
    fn roundtrip_field_request() {
        roundtrip(&ClientMessage::FieldRequest {
            center: ChunkPos::new(0, 0),
            render_distance: 6,
            chunks: vec![ChunkPos::new(0, 0), ChunkPos::new(-3, 5)],
        });
    }

    #[test]
    fn protocol_version_is_ten() {
        // 软材质颗粒批量下落动画网络语义为 v10；破坏性变更应同步更新此测试与版本号。
        assert_eq!(PROTOCOL_VERSION, 10);
    }

    #[test]
    fn decode_rejects_trailing_bytes() {
        let msg = ClientMessage::Ping { client_time_ms: 1 };
        let mut bytes = encode(&msg).expect("encode failed");
        bytes.extend_from_slice(&[0xAA, 0xBB]);
        assert!(decode::<ClientMessage>(&bytes).is_err());
    }

    #[test]
    fn roundtrip_player_tick() {
        roundtrip(&ServerMessage::PlayerTick {
            tick: 60,
            players: vec![PlayerSnapshot {
                entity_id: 1,
                last_input_tick: 9,
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
    fn roundtrip_free_object_project() {
        roundtrip(&ServerMessage::FreeObjectProject {
            object_id: 7,
            deltas: vec![(
                Position::new(1, 2, 3),
                MaterialCell::from_block_id(BlockID::STONE_BRICKS),
            )],
        });
    }

    #[test]
    fn roundtrip_free_object_spawn_and_state() {
        roundtrip(&ServerMessage::FreeObjectSpawn {
            object_id: 7,
            cells: vec![(
                Position::new(1, 2, 3),
                MaterialCell::from_block_id(BlockID::STONE_BRICKS),
            )],
        });
        roundtrip(&ServerMessage::FreeObjectState {
            object_id: 7,
            position: Vec3::new(1.0, 2.0, 3.0),
            velocity: Vec3::new(0.0, -3.0, 0.0),
        });
    }

    #[test]
    fn roundtrip_free_object_batches() {
        roundtrip(&ServerMessage::FreeObjectSpawnBatch {
            spawns: vec![
                (
                    11,
                    vec![(
                        Position::new(4, 65, 4),
                        MaterialCell::from_block_id(BlockID::SAND),
                    )],
                ),
                (
                    12,
                    vec![(
                        Position::new(5, 66, 4),
                        MaterialCell::from_block_id(BlockID::DIRT),
                    )],
                ),
            ],
        });
        roundtrip(&ServerMessage::FreeObjectStateBatch {
            states: vec![
                (11, Vec3::new(4.0, 63.5, 4.0), Vec3::new(0.0, -6.0, 0.0)),
                (12, Vec3::new(5.0, 64.2, 4.0), Vec3::new(3.0, -4.0, 0.0)),
            ],
        });
        roundtrip(&ServerMessage::FreeObjectProjectBatch {
            projections: vec![(
                11,
                vec![(
                    Position::new(4, 61, 4),
                    MaterialCell::from_block_id(BlockID::SAND),
                )],
            )],
        });
    }

    #[test]
    fn roundtrip_host_settings() {
        roundtrip(&ServerMessage::HostSettings { render_distance: 4 });
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
