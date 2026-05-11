//! VoxWeb 世界逻辑层（lib only）。
//! 负责地形生成、物理仲裁、方块操作校验、持久化触发。

pub mod persistence;
pub mod physics;
pub mod terrain;
pub mod world;

use voxweb_core::protocol::{ClientMessage, ServerMessage};

/// 服务端实例。在 Local-Only 模式下与 Client 共享内存；
/// 在 Host 模式下额外接收远程 Client 消息。
pub struct Server {
    pub world: world::World,
    /// 当前 server tick（60Hz 递增）
    pub tick: u32,
    /// 世界种子（地形与生物群落共用）
    pub seed: u64,
}

impl Server {
    /// 根据种子创建世界（自动生成初始 spawn 区域地形）。
    pub fn new(seed: u64) -> Self {
        Self {
            world: world::World::new(seed),
            tick: 0,
            seed,
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
                vec![ServerMessage::Welcome {
                    entity_id: 1,
                    server_tick: self.tick,
                    world_seed: self.seed,
                }]
            }
            ClientMessage::Break { pos, request_id } => {
                self.world.set_block(pos, voxweb_core::BlockID::AIR);
                vec![
                    ServerMessage::ActionAck {
                        request_id,
                        accepted: true,
                        reason: voxweb_core::protocol::AckReason::Ok,
                    },
                    ServerMessage::BlockUpdate {
                        pos,
                        block: voxweb_core::BlockID::AIR,
                    },
                ]
            }
            ClientMessage::Place {
                pos,
                block,
                request_id,
            } => {
                self.world.set_block(pos, block);
                vec![
                    ServerMessage::ActionAck {
                        request_id,
                        accepted: true,
                        reason: voxweb_core::protocol::AckReason::Ok,
                    },
                    ServerMessage::BlockUpdate { pos, block },
                ]
            }
            _ => vec![],
        }
    }
}

#[cfg(test)]
mod handle_message_tests {
    use super::*;
    use voxweb_core::protocol::{ClientMessage, ServerMessage};

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
    }

    #[test]
    fn unknown_message_returns_empty_vec() {
        let mut server = Server::new(0);
        let replies = server.handle_message(1, ClientMessage::Ping { client_time_ms: 0 });
        assert!(replies.is_empty(), "Phase 2 Ping handler 未实装，应返回空");
    }
}
