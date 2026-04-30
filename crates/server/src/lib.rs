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
