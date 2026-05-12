//! VoxWeb 世界逻辑层（lib only）。
//! 负责地形生成、物理仲裁、方块操作校验、持久化触发。

pub mod persistence;
pub mod physics;
pub mod terrain;
pub mod world;

use std::collections::HashMap;

use glam::Vec3;

use voxweb_core::protocol::{ClientMessage, ServerMessage};

/// 服务端实例。在 Local-Only 模式下与 Client 共享内存；
/// 在 Host 模式下额外接收远程 Client 消息。
pub struct Server {
    pub world: world::World,
    /// 当前 server tick（60Hz 递增）
    pub tick: u32,
    /// 世界种子（地形与生物群落共用）
    pub seed: u64,
    /// 玩家位置表：entity_id → 脚底世界坐标。Phase 3 仅用于挖放范围/重叠仲裁。
    /// Phase 5 会扩展为完整 PlayerSnapshot（含 yaw/pitch）并参与 PlayerTick 广播。
    pub players: HashMap<u32, Vec3>,
}

/// Phase 3 单玩家固定 entity_id；Phase 5 由 add_player 动态分配。
const LOCAL_ENTITY_ID: u32 = 1;

/// Phase 3 出生点（与 client::start_single_player 一致）。
const DEFAULT_SPAWN: Vec3 = Vec3::new(8.0, 100.0, 8.0);

impl Server {
    /// 根据种子创建世界（自动生成初始 spawn 区域地形）。
    pub fn new(seed: u64) -> Self {
        Self {
            world: world::World::new(seed),
            tick: 0,
            seed,
            players: HashMap::new(),
        }
    }

    /// 每帧 tick（60Hz）：推进物理、标记 dirty chunks、持久化触发。
    pub fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        self.world.tick();
    }

    /// 处理一条来自 Client 的消息（本地或远程），返回需要广播的响应消息列表。
    pub fn handle_message(&mut self, _entity_id: u32, msg: ClientMessage) -> Vec<ServerMessage> {
        match msg {
            ClientMessage::Hello { .. } => {
                // Phase 2：固定 entity_id=1。Phase 5 引入玩家表后由 add_player 分配。
                self.players.insert(LOCAL_ENTITY_ID, DEFAULT_SPAWN);
                vec![ServerMessage::Welcome {
                    entity_id: LOCAL_ENTITY_ID,
                    server_tick: self.tick,
                    world_seed: self.seed,
                }]
            }
            ClientMessage::PlayerInput { position, .. } => {
                // 更新玩家位置（Phase 3 仅供挖放仲裁；Phase 5 同时驱动 PlayerTick 广播）。
                self.players.insert(LOCAL_ENTITY_ID, position);
                vec![]
            }
            ClientMessage::Break { pos, request_id } => {
                let player_feet = self
                    .players
                    .get(&LOCAL_ENTITY_ID)
                    .copied()
                    .unwrap_or(DEFAULT_SPAWN);
                let reason = physics::validate_break(&self.world, pos, player_feet);
                if reason == voxweb_core::protocol::AckReason::Ok {
                    self.world.set_block(pos, voxweb_core::BlockID::AIR);
                    vec![
                        ServerMessage::ActionAck {
                            request_id,
                            accepted: true,
                            reason,
                        },
                        ServerMessage::BlockUpdate {
                            pos,
                            block: voxweb_core::BlockID::AIR,
                        },
                    ]
                } else {
                    vec![ServerMessage::ActionAck {
                        request_id,
                        accepted: false,
                        reason,
                    }]
                }
            }
            ClientMessage::Place {
                pos,
                block,
                request_id,
            } => {
                let player_feet = self
                    .players
                    .get(&LOCAL_ENTITY_ID)
                    .copied()
                    .unwrap_or(DEFAULT_SPAWN);
                let reason = physics::validate_place(&self.world, pos, block, player_feet);
                if reason == voxweb_core::protocol::AckReason::Ok {
                    self.world.set_block(pos, block);
                    vec![
                        ServerMessage::ActionAck {
                            request_id,
                            accepted: true,
                            reason,
                        },
                        ServerMessage::BlockUpdate { pos, block },
                    ]
                } else {
                    vec![ServerMessage::ActionAck {
                        request_id,
                        accepted: false,
                        reason,
                    }]
                }
            }
            _ => vec![],
        }
    }
}

#[cfg(test)]
mod handle_message_tests {
    use super::*;
    use voxweb_core::chunk::Position;
    use voxweb_core::protocol::{AckReason, ClientMessage, ServerMessage};

    #[test]
    fn hello_returns_welcome_with_seed() {
        let mut server = Server::new(42);
        let replies = server.handle_message(
            1,
            ClientMessage::Hello {
                display_name: "Tester".into(),
                version: 1,
            },
        );
        assert_eq!(replies.len(), 1);
        match &replies[0] {
            ServerMessage::Welcome {
                entity_id,
                world_seed,
                ..
            } => {
                assert_eq!(*entity_id, 1);
                assert_eq!(*world_seed, 42);
            }
            other => panic!("expected Welcome, got {other:?}"),
        }
        // Hello 应同时把 entity_id=1 的初始位置塞进 players 表
        assert_eq!(server.players.get(&LOCAL_ENTITY_ID), Some(&DEFAULT_SPAWN));
    }

    #[test]
    fn player_input_updates_players_table() {
        let mut server = Server::new(0);
        server.players.insert(LOCAL_ENTITY_ID, DEFAULT_SPAWN);
        let replies = server.handle_message(
            1,
            ClientMessage::PlayerInput {
                tick: 5,
                position: Vec3::new(3.5, 70.0, 4.5),
                yaw: 0.0,
                pitch: 0.0,
            },
        );
        assert!(replies.is_empty());
        assert_eq!(
            server.players.get(&LOCAL_ENTITY_ID),
            Some(&Vec3::new(3.5, 70.0, 4.5))
        );
    }

    #[test]
    fn ping_message_returns_empty_vec() {
        let mut server = Server::new(0);
        let replies = server.handle_message(1, ClientMessage::Ping { client_time_ms: 0 });
        assert!(replies.is_empty(), "Phase 3 Ping handler 未实装，应返回空");
    }

    /// 把 chunk(0,0) 的一柱方块设置好，便于挖放测试。
    fn prepare_world() -> Server {
        let mut server = Server::new(0);
        server
            .world
            .ensure_chunk_generated(voxweb_core::ChunkPos::new(0, 0));
        for x in 0..16 {
            for z in 0..16 {
                server
                    .world
                    .set_block(Position::new(x, 64, z), voxweb_core::BlockID::STONE);
                server
                    .world
                    .set_block(Position::new(x, 65, z), voxweb_core::BlockID::AIR);
            }
        }
        // 玩家站在 (3.5, 65, 3.5)
        server
            .players
            .insert(LOCAL_ENTITY_ID, Vec3::new(3.5, 65.0, 3.5));
        server
    }

    #[test]
    fn break_in_range_succeeds_and_broadcasts() {
        let mut server = prepare_world();
        let replies = server.handle_message(
            LOCAL_ENTITY_ID,
            ClientMessage::Break {
                pos: Position::new(3, 64, 3),
                request_id: 42,
            },
        );
        assert_eq!(replies.len(), 2);
        assert!(matches!(
            replies[0],
            ServerMessage::ActionAck {
                accepted: true,
                request_id: 42,
                ..
            }
        ));
        assert!(matches!(
            replies[1],
            ServerMessage::BlockUpdate {
                block: voxweb_core::BlockID::AIR,
                ..
            }
        ));
        assert_eq!(
            server.world.get_block(Position::new(3, 64, 3)),
            voxweb_core::BlockID::AIR
        );
    }

    #[test]
    fn break_out_of_range_only_returns_ack() {
        let mut server = prepare_world();
        let replies = server.handle_message(
            LOCAL_ENTITY_ID,
            ClientMessage::Break {
                pos: Position::new(15, 64, 15), // 距玩家约 sqrt(11.5^2 + 0.6^2 + 11.5^2) ≈ 16m
                request_id: 7,
            },
        );
        assert_eq!(replies.len(), 1);
        assert!(matches!(
            replies[0],
            ServerMessage::ActionAck {
                accepted: false,
                reason: AckReason::OutOfRange,
                request_id: 7,
            }
        ));
        // 没有 BlockUpdate；world 不变
        assert_eq!(
            server.world.get_block(Position::new(15, 64, 15)),
            voxweb_core::BlockID::STONE
        );
    }

    #[test]
    fn place_overlapping_player_rejected() {
        let mut server = prepare_world();
        let replies = server.handle_message(
            LOCAL_ENTITY_ID,
            ClientMessage::Place {
                pos: Position::new(3, 65, 3),
                block: voxweb_core::BlockID::STONE,
                request_id: 9,
            },
        );
        assert_eq!(replies.len(), 1);
        assert!(matches!(
            replies[0],
            ServerMessage::ActionAck {
                accepted: false,
                reason: AckReason::Overlap,
                request_id: 9,
            }
        ));
    }
}
