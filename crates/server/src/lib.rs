//! VoxWeb 世界逻辑层（lib only）。
//! 负责地形生成、物理仲裁、方块操作校验、持久化触发。
//!
//! ## 角色与调用方
//!
//! - **Local-Only**：Client 直接持 `Rc<RefCell<Server>>`，每帧 `tick()` + 通过 mpsc 投递 ClientMessage。
//! - **Host**：与 Local 相同，额外接收来自远端 Peer 的 ClientMessage（由 [`voxweb_net::NetEndpoint`] 解码后调
//!   `handle_message`）。`broadcast_tick` 把 PlayerTick 加进 outbox 由 net 层路由。
//! - **Remote**：客户端仍持有一个 `Server` 实例**但仅作为方块数据宿主** — Phase 5 的取舍是不引入独立的
//!   `WorldView`，而是让 ChunkSnapshot 直接写入 `server.world.chunks`。
//!   Remote 模式下 `tick()` / `handle_message()` 不会被主循环驱动；任何对这两个方法的调用都意味着调用方搞错了。

pub mod persistence;
pub mod physics;
pub mod terrain;
pub mod world;

use std::collections::{HashMap, VecDeque};

use glam::Vec3;

use voxweb_core::chunk::{CHUNK_X, CHUNK_Z, ChunkPos};
use voxweb_core::protocol::{
    CHUNK_SNAPSHOT_PAYLOAD_MAX, ClientMessage, EntityId, OutboundMessage, PlayerEntry,
    PlayerSnapshot, Recipient, ServerMessage,
};

/// 服务端权威的玩家实体（Phase 5 起完整版）。
///
/// Local-Only 时只有一项（id=1 的 Host 本人）；Host 模式下 Remote 通过
/// [`Server::add_player`] 分配 id 并写入此表。
#[derive(Clone, Debug)]
pub struct PlayerEntity {
    pub display_name: String,
    /// 脚底世界坐标。挖放仲裁 / PlayerTick 广播都读这个值。
    pub position: Vec3,
    pub yaw: f32,
    pub pitch: f32,
    /// 已处理过的最大 PlayerInput.tick。过期输入会被丢弃。
    pub last_input_tick: u32,
    /// 加入时的 server tick 计数。
    pub joined_at_tick: u32,
    /// Delta 广播：上次广播时的位置（None 表示尚未广播过，需全量发送）
    pub last_broadcast_position: Option<Vec3>,
    /// Delta 广播：上次广播时的 yaw
    pub last_broadcast_yaw: Option<f32>,
    /// Delta 广播：上次广播时的 pitch
    pub last_broadcast_pitch: Option<f32>,
}

impl PlayerEntity {
    fn new(display_name: String, joined_at_tick: u32) -> Self {
        Self {
            display_name,
            position: DEFAULT_SPAWN,
            yaw: 0.0,
            pitch: 0.0,
            last_input_tick: 0,
            joined_at_tick,
            last_broadcast_position: None,
            last_broadcast_yaw: None,
            last_broadcast_pitch: None,
        }
    }

    fn to_snapshot(&self, entity_id: EntityId) -> PlayerSnapshot {
        PlayerSnapshot {
            entity_id,
            position: self.position,
            yaw: self.yaw,
            pitch: self.pitch,
        }
    }
}

/// 服务端实例。在 Local-Only 模式下与 Client 共享内存；
/// 在 Host 模式下额外接收远程 Client 消息；Remote 模式下仅作数据宿主（不 tick）。
pub struct Server {
    pub world: world::World,
    /// 当前 server tick（60Hz 递增）
    pub tick: u32,
    /// 世界种子（地形与生物群落共用）
    pub seed: u64,
    /// 玩家实体表。entity_id → 完整玩家状态。
    /// Phase 5 起替换了 Phase 3 的 `HashMap<u32, Vec3>` 单字段形式。
    pub players: HashMap<EntityId, PlayerEntity>,
    /// 当前实时时钟（毫秒，performance.now() 量级）。Host 每帧由主循环更新；
    /// `Pong` / `PlayerTick` 响应时携带，配合客户端的 RTT 与时钟偏移估算。
    pub current_time_ms: u64,
    /// 下一个待分配的 entity_id（从 [`FIRST_ENTITY_ID`] 起步）。
    next_entity_id: EntityId,
    /// 房间主机的 entity_id（首次 `add_player` 时设；用于 Welcome 标识 Host）。
    /// Local-Only 模式下也会被设为 FIRST_ENTITY_ID，行为一致。
    host_entity_id: Option<EntityId>,
    /// 出站消息队列（带 Recipient 标签）。
    /// `handle_message` / `add_player` / `broadcast_tick` 都向此队列追加，
    /// 由调用方（Local 直接消费 / Host 通过 net 层路由）每帧 `drain_outbox()` 取走。
    outbox: VecDeque<OutboundMessage>,
    /// 聊天速率限制窗口：entity_id → 最近的发送 tick 列表（滑窗）。
    /// Phase 6 起：每个 entity 在 [`CHAT_RATE_WINDOW_TICKS`] tick 内最多发
    /// [`CHAT_RATE_LIMIT`] 条；超出静默丢弃。
    chat_window: HashMap<EntityId, VecDeque<u32>>,
}

/// 第一个分配的 entity_id（Host 本人通常占这个值）。
pub const FIRST_ENTITY_ID: EntityId = 1;

/// Phase 3 出生点（与 client::start_single_player 一致）。
pub const DEFAULT_SPAWN: Vec3 = Vec3::new(8.0, 100.0, 8.0);

/// 初始 ChunkSnapshot 的半径（chunk 数）。Phase 5 固定值；后续随渲染距离动态调。
const INITIAL_SNAPSHOT_RADIUS: i32 = 6;

/// Delta 广播：位置变化距离平方阈值（0.01m² ≈ 0.1m 位移）。
const DELTA_POS_THRESHOLD_SQ: f32 = 0.0001;
/// Delta 广播：朝向变化角度阈值（弧度，约 0.5°）。
const DELTA_ANGLE_THRESHOLD: f32 = 0.0087;
/// Delta 广播：每 N tick 强制全量发送（0.5s @ 60Hz）。
const FULL_BROADCAST_INTERVAL: u32 = 30;

/// 聊天消息长度上限（Unicode scalar 数）。超过则静默丢弃（[`docs/networking/protocol.md`] §八）。
const CHAT_MAX_CHARS: usize = 256;
/// 聊天速率限制：滑窗 tick 数（60Hz × 3s ≈ 180 ticks）。
const CHAT_RATE_WINDOW_TICKS: u32 = 180;
/// 聊天速率限制：单玩家窗口内允许的最大消息数。
const CHAT_RATE_LIMIT: usize = 5;

impl Server {
    /// 根据种子创建世界。Phase 5 不再自动注册玩家：
    /// Client 启动时（Local/Host）显式调 [`Server::add_player`] 让 Host 本人入表。
    pub fn new(seed: u64) -> Self {
        Self {
            world: world::World::new(seed),
            tick: 0,
            seed,
            players: HashMap::new(),
            current_time_ms: 0,
            next_entity_id: FIRST_ENTITY_ID,
            host_entity_id: None,
            outbox: VecDeque::new(),
            chat_window: HashMap::new(),
        }
    }

    /// 由 Host 主循环每帧更新（performance.now() 毫秒）；Pong/PlayerTick 中带回。
    pub fn set_clock(&mut self, ms: u64) {
        self.current_time_ms = ms;
    }

    /// 每帧 tick（60Hz）：推进 world tick + 把当前所有玩家位置打包成 PlayerTick 广播。
    /// **Remote 模式不调用**（无权威逻辑可推进）。
    pub fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        self.world.tick();
        self.broadcast_tick();
    }

    /// 加入一个玩家：分配 entity_id、入 players 表，并 enqueue 三类消息：
    /// - Welcome → `Recipient::One(eid)`
    /// - PeerJoined → `Recipient::Except(eid)`（其他在场玩家收到通告）
    /// - ChunkSnapshot 分片 → `Recipient::One(eid)`（出生点周围 INITIAL_SNAPSHOT_RADIUS 内的所有 chunk）
    ///
    /// 返回新分配的 entity_id；Host 端 net 层用它建立 peer_id ↔ entity_id 映射。
    pub fn add_player(&mut self, display_name: String) -> EntityId {
        let eid = self.next_entity_id;
        self.next_entity_id = self.next_entity_id.wrapping_add(1);
        if self.next_entity_id == 0 {
            // u32 溢出兜底（理论上单房间内永远走不到）
            self.next_entity_id = FIRST_ENTITY_ID;
        }

        // 首位入表的玩家即为 Host（Local-Only 也走这个路径）。
        if self.host_entity_id.is_none() {
            self.host_entity_id = Some(eid);
        }

        let player = PlayerEntity::new(display_name.clone(), self.tick);
        self.players.insert(eid, player);

        // Welcome 携带完整 roster（含本人），让新加入者一次性建好玩家表。
        // Phase 6 协议 v2 起的扩展，替代了 v1 的"per-peer PeerJoined 到新加入者"逻辑。
        let roster: Vec<PlayerEntry> = self
            .players
            .iter()
            .map(|(id, p)| PlayerEntry {
                entity_id: *id,
                display_name: p.display_name.clone(),
            })
            .collect();
        let host_eid = self.host_entity_id.unwrap_or(eid);

        self.enqueue(
            Recipient::One(eid),
            ServerMessage::Welcome {
                entity_id: eid,
                server_tick: self.tick,
                world_seed: self.seed,
                host_entity_id: host_eid,
                players: roster,
            },
        );
        // 老玩家依旧通过 PeerJoined 得知新人加入。
        self.enqueue(
            Recipient::Except(eid),
            ServerMessage::PeerJoined {
                entity_id: eid,
                display_name,
            },
        );

        // 初始快照：以出生点 (chunk 0,0) 为中心扩 INITIAL_SNAPSHOT_RADIUS 圈。
        let spawn_chunk = ChunkPos::new(
            (DEFAULT_SPAWN.x as i32).div_euclid(CHUNK_X as i32),
            (DEFAULT_SPAWN.z as i32).div_euclid(CHUNK_Z as i32),
        );
        self.send_initial_snapshot(eid, spawn_chunk, INITIAL_SNAPSHOT_RADIUS);

        eid
    }

    /// 移除玩家：从 players 表删除，enqueue `PeerLeft` 到所有剩余玩家。
    pub fn remove_player(&mut self, eid: EntityId) {
        if self.players.remove(&eid).is_some() {
            self.chat_window.remove(&eid);
            self.enqueue(Recipient::All, ServerMessage::PeerLeft { entity_id: eid });
        }
    }

    /// 当前房间主机的 entity_id（首次 `add_player` 时设；尚无玩家时返回 None）。
    pub fn host_entity_id(&self) -> Option<EntityId> {
        self.host_entity_id
    }

    /// 取走 outbox 中所有 OutboundMessage。
    /// Local 调用方直接把每条 `message` 喂回 `ServerInbox`；
    /// Host 调用方走 `NetEndpoint::host_route_outbox`。
    pub fn drain_outbox(&mut self) -> Vec<OutboundMessage> {
        self.outbox.drain(..).collect()
    }

    /// 流控阻塞的消息重新入队（下帧重试发送）。
    pub fn reenqueue_outbox(&mut self, msgs: Vec<OutboundMessage>) {
        self.outbox.extend(msgs);
    }

    /// Phase 8 持久化层用的占位。Phase 5 不消费，留方法签名以免后续 API churn。
    pub fn load_chunk_from_storage(&mut self, pos: ChunkPos, chunk: voxweb_core::chunk::Chunk) {
        self.world.chunks.insert(pos, chunk);
    }

    /// 处理一条来自 Client 的消息（本地或远程）。
    /// 所有响应通过 [`Server::enqueue`] 进入 outbox；调用方负责 drain 后路由。
    ///
    /// **注意**：`Hello` 不在此处理 — 调用方收到 Hello 时应改调 [`Server::add_player`]
    /// （因为 entity_id 的分配是 Hello 的一部分，但分配又依赖 `&mut self`，
    /// 与 net 层 peer_to_entity 映射的写入顺序耦合太重 — 把它从 dispatch 中抽出来更干净）。
    pub fn handle_message(&mut self, entity_id: EntityId, msg: ClientMessage) {
        match msg {
            ClientMessage::Hello { .. } => {
                log::warn!(
                    "[server] Hello reached handle_message; caller should call add_player instead"
                );
            }
            ClientMessage::PlayerInput {
                tick,
                position,
                yaw,
                pitch,
            } => {
                let Some(player) = self.players.get_mut(&entity_id) else {
                    return;
                };
                // 拒绝过期 / 乱序 tick（防止丢包补传导致位置回退）
                if tick <= player.last_input_tick && player.last_input_tick != 0 {
                    return;
                }
                player.position = position;
                player.yaw = yaw;
                player.pitch = pitch;
                player.last_input_tick = tick;
            }
            ClientMessage::Break { pos, request_id } => {
                let player_feet = self
                    .players
                    .get(&entity_id)
                    .map(|p| p.position)
                    .unwrap_or(DEFAULT_SPAWN);
                let reason = physics::validate_break(&self.world, pos, player_feet);
                if reason == voxweb_core::protocol::AckReason::Ok {
                    self.world.set_block(pos, voxweb_core::BlockID::AIR);
                    self.enqueue(
                        Recipient::One(entity_id),
                        ServerMessage::ActionAck {
                            request_id,
                            accepted: true,
                            reason,
                        },
                    );
                    self.enqueue(
                        Recipient::All,
                        ServerMessage::BlockUpdate {
                            pos,
                            block: voxweb_core::BlockID::AIR,
                        },
                    );
                } else {
                    self.enqueue(
                        Recipient::One(entity_id),
                        ServerMessage::ActionAck {
                            request_id,
                            accepted: false,
                            reason,
                        },
                    );
                }
            }
            ClientMessage::Place {
                pos,
                block,
                request_id,
            } => {
                let player_feet = self
                    .players
                    .get(&entity_id)
                    .map(|p| p.position)
                    .unwrap_or(DEFAULT_SPAWN);
                let reason = physics::validate_place(&self.world, pos, block, player_feet);
                if reason == voxweb_core::protocol::AckReason::Ok {
                    self.world.set_block(pos, block);
                    self.enqueue(
                        Recipient::One(entity_id),
                        ServerMessage::ActionAck {
                            request_id,
                            accepted: true,
                            reason,
                        },
                    );
                    self.enqueue(Recipient::All, ServerMessage::BlockUpdate { pos, block });
                } else {
                    self.enqueue(
                        Recipient::One(entity_id),
                        ServerMessage::ActionAck {
                            request_id,
                            accepted: false,
                            reason,
                        },
                    );
                }
            }
            ClientMessage::Chat { content } => {
                // Phase 6：256 字符上限 + 5 条 / 3s 速率限制；超出静默丢弃（不回错以避免被穷举）。
                if content.chars().count() > CHAT_MAX_CHARS {
                    log::debug!(
                        "chat dropped (too long): eid={entity_id} chars={}",
                        content.chars().count()
                    );
                    return;
                }
                if !self.chat_rate_limit_allow(entity_id) {
                    log::debug!("chat dropped (rate limited): eid={entity_id}");
                    return;
                }
                self.enqueue(
                    Recipient::All,
                    ServerMessage::Chat {
                        from: entity_id,
                        content,
                    },
                );
            }
            ClientMessage::Ping { client_time_ms } => {
                self.enqueue(
                    Recipient::One(entity_id),
                    ServerMessage::Pong {
                        client_time_ms,
                        server_time_ms: self.current_time_ms,
                    },
                );
            }
        }
    }

    /// 每 tick 向所有玩家广播 PlayerTick（delta 模式）。
    ///
    /// **Delta 规则**：
    /// - 位置变化 < 0.01m 且朝向变化 < 0.5° 的玩家不包含在本 tick 的 players 列表中
    /// - 每 [`FULL_BROADCAST_INTERVAL`] tick 强制全量发送一次（防止丢包导致远端冻结）
    /// - 新玩家（`last_broadcast_position.is_none()`）始终包含
    ///
    /// 频率：60Hz（由 `tick()` 末尾调用）。
    pub fn broadcast_tick(&mut self) {
        if self.players.is_empty() {
            return;
        }

        let force_full = self.tick.is_multiple_of(FULL_BROADCAST_INTERVAL);

        let players: Vec<PlayerSnapshot> = self
            .players
            .iter_mut()
            .filter(|(_, p)| {
                if force_full {
                    return true;
                }
                let Some(last_pos) = p.last_broadcast_position else {
                    return true; // 首次广播
                };
                let moved = p.position.distance_squared(last_pos) > DELTA_POS_THRESHOLD_SQ;
                let turned = (p.yaw - p.last_broadcast_yaw.unwrap_or(0.0)).abs()
                    > DELTA_ANGLE_THRESHOLD
                    || (p.pitch - p.last_broadcast_pitch.unwrap_or(0.0)).abs()
                        > DELTA_ANGLE_THRESHOLD;
                moved || turned
            })
            .map(|(eid, p)| {
                p.last_broadcast_position = Some(p.position);
                p.last_broadcast_yaw = Some(p.yaw);
                p.last_broadcast_pitch = Some(p.pitch);
                p.to_snapshot(*eid)
            })
            .collect();

        // 如果 delta 过滤后没有玩家需要广播（极端情况：所有人完全静止超过 0.5s），
        // 仍发一个 PlayerTick 以维持 server_time_ms 时钟同步
        self.enqueue(
            Recipient::All,
            ServerMessage::PlayerTick {
                tick: self.tick,
                players,
                server_time_ms: self.current_time_ms,
            },
        );
    }

    /// 把出生点附近的 chunk 切片成 ChunkSnapshot 分片塞进 outbox（Recipient::One(eid)）。
    /// 半径 = chunk 数，覆盖 `(2*radius+1)^2` 个 chunk。未加载的 chunk 会先 `ensure_chunk_generated`。
    pub fn send_initial_snapshot(&mut self, recipient: EntityId, center: ChunkPos, radius: i32) {
        for dx in -radius..=radius {
            for dz in -radius..=radius {
                let pos = ChunkPos::new(center.x + dx, center.z + dz);
                self.world.ensure_chunk_generated(pos);
                let Some(chunk) = self.world.chunks.get(&pos) else {
                    continue;
                };
                let bytes = match voxweb_core::chunk::encode_chunk(&chunk.blocks) {
                    Ok(b) => b,
                    Err(e) => {
                        log::warn!("[server] encode chunk {pos:?} failed: {e}");
                        continue;
                    }
                };
                let total_len = bytes.len();
                let frag_count = total_len.div_ceil(CHUNK_SNAPSHOT_PAYLOAD_MAX).max(1);
                debug_assert!(frag_count <= u16::MAX as usize, "chunk too big");
                for (i, chunk_bytes) in bytes.chunks(CHUNK_SNAPSHOT_PAYLOAD_MAX).enumerate() {
                    self.enqueue(
                        Recipient::One(recipient),
                        ServerMessage::ChunkSnapshot {
                            pos,
                            frag_index: i as u16,
                            frag_total: frag_count as u16,
                            payload: chunk_bytes.to_vec(),
                        },
                    );
                }
            }
        }
    }

    /// 内部：把一条消息压入 outbox。
    fn enqueue(&mut self, recipient: Recipient, message: ServerMessage) {
        self.outbox
            .push_back(OutboundMessage { recipient, message });
    }

    /// 内部：聊天速率限制滑窗判定。
    ///
    /// 通过则把当前 tick 记入 `chat_window[eid]`，并清理窗口外的旧记录；
    /// 超出 [`CHAT_RATE_LIMIT`] 条 / [`CHAT_RATE_WINDOW_TICKS`] tick 时返回 false。
    ///
    /// 使用 `self.tick`（60Hz 单调递增）作为时钟，便于 Local-Only 模式下也可工作
    /// （`current_time_ms` 仅 Host 模式被驱动更新）。
    fn chat_rate_limit_allow(&mut self, eid: EntityId) -> bool {
        let now = self.tick;
        let window = self.chat_window.entry(eid).or_default();
        // 清理窗口外的旧记录（用 saturating_sub 防 u32 下溢，开服前几秒也安全）。
        let cutoff = now.saturating_sub(CHAT_RATE_WINDOW_TICKS);
        while let Some(&front) = window.front() {
            if front < cutoff {
                window.pop_front();
            } else {
                break;
            }
        }
        if window.len() >= CHAT_RATE_LIMIT {
            return false;
        }
        window.push_back(now);
        true
    }
}

#[cfg(test)]
mod handle_message_tests {
    use super::*;
    use voxweb_core::chunk::Position;
    use voxweb_core::protocol::{AckReason, ClientMessage, ServerMessage};

    /// 在 outbox 中找到第一条匹配 predicate 的消息，返回其 (Recipient, ServerMessage) 副本。
    fn find_outbox<F>(server: &Server, pred: F) -> Option<(Recipient, ServerMessage)>
    where
        F: Fn(&OutboundMessage) -> bool,
    {
        server
            .outbox
            .iter()
            .find(|m| pred(m))
            .map(|m| (m.recipient.clone(), m.message.clone()))
    }

    #[test]
    fn add_player_allocates_increasing_ids_and_enqueues_welcome_peerjoined_snapshot() {
        let mut server = Server::new(42);
        let eid1 = server.add_player("Alice".into());
        let eid2 = server.add_player("Bob".into());
        assert_eq!(eid1, 1);
        assert_eq!(eid2, 2);
        assert!(server.players.contains_key(&eid1));
        assert!(server.players.contains_key(&eid2));

        // Welcome 应给 eid1 和 eid2 各一条
        let welcome1 = find_outbox(&server, |m| {
            matches!(
                (&m.recipient, &m.message),
                (Recipient::One(e), ServerMessage::Welcome { entity_id, .. }) if *e == 1 && *entity_id == 1
            )
        });
        let welcome2 = find_outbox(&server, |m| {
            matches!(
                (&m.recipient, &m.message),
                (Recipient::One(e), ServerMessage::Welcome { entity_id, .. }) if *e == 2 && *entity_id == 2
            )
        });
        assert!(welcome1.is_some(), "missing Welcome to eid 1");
        assert!(welcome2.is_some(), "missing Welcome to eid 2");

        // PeerJoined：第一次 add_player 也会 enqueue（即使没有别人；Except 让 outbox 路由层自然过滤）
        let pj_for_bob = find_outbox(&server, |m| {
            matches!(
                (&m.recipient, &m.message),
                (Recipient::Except(e), ServerMessage::PeerJoined { entity_id, .. }) if *e == 2 && *entity_id == 2
            )
        });
        assert!(pj_for_bob.is_some(), "missing PeerJoined for eid 2");

        // ChunkSnapshot：至少有一片到 eid1
        let has_snapshot = server.outbox.iter().any(|m| {
            matches!(
                (&m.recipient, &m.message),
                (Recipient::One(e), ServerMessage::ChunkSnapshot { .. }) if *e == 1
            )
        });
        assert!(has_snapshot, "expected at least one ChunkSnapshot fragment");
    }

    #[test]
    fn remove_player_enqueues_peer_left_to_all() {
        let mut server = Server::new(0);
        let eid = server.add_player("Alice".into());
        server.drain_outbox(); // 清掉 add_player 产生的消息

        server.remove_player(eid);

        let pl = find_outbox(&server, |m| {
            matches!(
                (&m.recipient, &m.message),
                (Recipient::All, ServerMessage::PeerLeft { entity_id }) if *entity_id == eid
            )
        });
        assert!(pl.is_some(), "missing PeerLeft");
        assert!(!server.players.contains_key(&eid));
    }

    #[test]
    fn remove_unknown_player_is_no_op() {
        let mut server = Server::new(0);
        server.remove_player(999);
        assert!(server.outbox.is_empty());
    }

    #[test]
    fn drain_outbox_empties_queue() {
        let mut server = Server::new(0);
        server.add_player("A".into());
        let before = server.outbox.len();
        assert!(before > 0);
        let drained = server.drain_outbox();
        assert_eq!(drained.len(), before);
        assert!(server.outbox.is_empty());
    }

    #[test]
    fn handle_message_player_input_updates_player_entity_and_rejects_old_tick() {
        let mut server = Server::new(0);
        let eid = server.add_player("A".into());
        server.drain_outbox();

        // 推 tick=5
        server.handle_message(
            eid,
            ClientMessage::PlayerInput {
                tick: 5,
                position: Vec3::new(1.0, 70.0, 2.0),
                yaw: 0.1,
                pitch: -0.2,
            },
        );
        let p = &server.players[&eid];
        assert_eq!(p.position, Vec3::new(1.0, 70.0, 2.0));
        assert_eq!(p.last_input_tick, 5);

        // 过期 tick=3 应被拒绝
        server.handle_message(
            eid,
            ClientMessage::PlayerInput {
                tick: 3,
                position: Vec3::new(100.0, 70.0, 2.0),
                yaw: 0.0,
                pitch: 0.0,
            },
        );
        let p = &server.players[&eid];
        assert_eq!(
            p.position,
            Vec3::new(1.0, 70.0, 2.0),
            "expired tick must not override"
        );
        assert_eq!(p.last_input_tick, 5);
    }

    /// 把 chunk(0,0) 的一柱方块设置好，便于挖放测试。
    fn prepare_world() -> (Server, EntityId) {
        let mut server = Server::new(0);
        let eid = server.add_player("Tester".into());
        server.drain_outbox(); // 清掉初始消息

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
        server.players.get_mut(&eid).unwrap().position = Vec3::new(3.5, 65.0, 3.5);
        (server, eid)
    }

    #[test]
    fn handle_message_break_enqueues_ack_one_and_blockupdate_all() {
        let (mut server, eid) = prepare_world();
        server.handle_message(
            eid,
            ClientMessage::Break {
                pos: Position::new(3, 64, 3),
                request_id: 42,
            },
        );

        // ActionAck 应只发给 eid
        let ack = find_outbox(&server, |m| {
            matches!(
                (&m.recipient, &m.message),
                (Recipient::One(e), ServerMessage::ActionAck { request_id, accepted: true, .. })
                    if *e == eid && *request_id == 42
            )
        });
        assert!(ack.is_some(), "missing ActionAck One({eid})");

        // BlockUpdate 应是 All
        let bu = find_outbox(&server, |m| {
            matches!(
                (&m.recipient, &m.message),
                (Recipient::All, ServerMessage::BlockUpdate { block, .. })
                    if *block == voxweb_core::BlockID::AIR
            )
        });
        assert!(bu.is_some(), "missing BlockUpdate All");

        // World 应该真的更新了
        assert_eq!(
            server.world.get_block(Position::new(3, 64, 3)),
            voxweb_core::BlockID::AIR
        );
    }

    #[test]
    fn handle_message_break_out_of_range_only_ack_no_blockupdate() {
        let (mut server, eid) = prepare_world();
        server.handle_message(
            eid,
            ClientMessage::Break {
                pos: Position::new(15, 64, 15),
                request_id: 7,
            },
        );
        let ack = find_outbox(&server, |m| {
            matches!(
                (&m.recipient, &m.message),
                (
                    Recipient::One(_),
                    ServerMessage::ActionAck {
                        accepted: false,
                        reason: AckReason::OutOfRange,
                        request_id: 7
                    }
                )
            )
        });
        assert!(ack.is_some());
        // 不应出现 BlockUpdate
        let bu = find_outbox(&server, |m| {
            matches!(m.message, ServerMessage::BlockUpdate { .. })
        });
        assert!(
            bu.is_none(),
            "out-of-range break should not enqueue BlockUpdate"
        );
        assert_eq!(
            server.world.get_block(Position::new(15, 64, 15)),
            voxweb_core::BlockID::STONE
        );
    }

    #[test]
    fn handle_message_place_overlap_rejected_and_no_blockupdate() {
        let (mut server, eid) = prepare_world();
        server.handle_message(
            eid,
            ClientMessage::Place {
                pos: Position::new(3, 65, 3),
                block: voxweb_core::BlockID::STONE,
                request_id: 9,
            },
        );
        let ack = find_outbox(&server, |m| {
            matches!(
                (&m.recipient, &m.message),
                (
                    Recipient::One(_),
                    ServerMessage::ActionAck {
                        accepted: false,
                        reason: AckReason::Overlap,
                        request_id: 9
                    }
                )
            )
        });
        assert!(ack.is_some());
        let bu = find_outbox(&server, |m| {
            matches!(m.message, ServerMessage::BlockUpdate { .. })
        });
        assert!(bu.is_none());
    }

    #[test]
    fn handle_message_ping_returns_pong_one_with_server_clock() {
        let mut server = Server::new(0);
        let eid = server.add_player("A".into());
        server.set_clock(12345);
        server.drain_outbox();

        server.handle_message(eid, ClientMessage::Ping { client_time_ms: 7 });
        let pong = find_outbox(&server, |m| {
            matches!(
                (&m.recipient, &m.message),
                (Recipient::One(e), ServerMessage::Pong { client_time_ms: 7, server_time_ms: 12345 })
                    if *e == eid
            )
        });
        assert!(pong.is_some());
    }

    #[test]
    fn handle_message_chat_broadcasts_to_all() {
        let mut server = Server::new(0);
        let eid = server.add_player("A".into());
        server.drain_outbox();

        server.handle_message(
            eid,
            ClientMessage::Chat {
                content: "hi".into(),
            },
        );
        let chat = find_outbox(&server, |m| {
            matches!(
                (&m.recipient, &m.message),
                (Recipient::All, ServerMessage::Chat { from, content })
                    if *from == eid && content == "hi"
            )
        });
        assert!(chat.is_some());
    }

    // ── Phase 6 ──

    #[test]
    fn host_eid_set_on_first_add_player() {
        let mut server = Server::new(0);
        assert!(server.host_entity_id().is_none());

        let host = server.add_player("Alice".into());
        assert_eq!(server.host_entity_id(), Some(host));

        // 后续 add_player 不改 host_eid
        let _ = server.add_player("Bob".into());
        assert_eq!(server.host_entity_id(), Some(host));
    }

    #[test]
    fn welcome_carries_full_roster_and_host_eid() {
        let mut server = Server::new(0);
        let alice = server.add_player("Alice".into());
        server.drain_outbox(); // 清掉 Alice 自己的 Welcome

        // Bob 加入后，给 Bob 发的 Welcome 应当包含 Alice + Bob 名单与 Alice 的 host_eid
        let bob = server.add_player("Bob".into());
        let welcome = find_outbox(&server, |m| {
            matches!(
                (&m.recipient, &m.message),
                (Recipient::One(e), ServerMessage::Welcome { entity_id, .. })
                    if *e == bob && *entity_id == bob
            )
        });
        let (_, msg) = welcome.expect("missing Welcome to bob");
        match msg {
            ServerMessage::Welcome {
                host_entity_id,
                players,
                ..
            } => {
                assert_eq!(host_entity_id, alice, "host_eid should be Alice");
                let mut names: Vec<_> = players.iter().map(|p| p.display_name.as_str()).collect();
                names.sort_unstable();
                assert_eq!(names, vec!["Alice", "Bob"]);
                let ids: Vec<EntityId> = {
                    let mut v: Vec<_> = players.iter().map(|p| p.entity_id).collect();
                    v.sort_unstable();
                    v
                };
                assert_eq!(ids, vec![alice, bob]);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn chat_drops_messages_over_256_chars() {
        let mut server = Server::new(0);
        let eid = server.add_player("A".into());
        server.drain_outbox();

        // 257 个 ASCII 字符 → 静默丢弃
        let too_long: String = std::iter::repeat_n('x', 257).collect();
        server.handle_message(eid, ClientMessage::Chat { content: too_long });
        let chat = find_outbox(&server, |m| matches!(m.message, ServerMessage::Chat { .. }));
        assert!(
            chat.is_none(),
            "expected too-long chat to be silently dropped"
        );

        // 256 个 ASCII → 通过
        let ok_long: String = std::iter::repeat_n('y', 256).collect();
        server.handle_message(
            eid,
            ClientMessage::Chat {
                content: ok_long.clone(),
            },
        );
        let chat = find_outbox(
            &server,
            |m| matches!(&m.message, ServerMessage::Chat { content, .. } if content == &ok_long),
        );
        assert!(chat.is_some(), "256-char chat should pass");
    }

    #[test]
    fn chat_drop_counts_unicode_scalars_not_bytes() {
        let mut server = Server::new(0);
        let eid = server.add_player("A".into());
        server.drain_outbox();

        // 256 个中文字符（UTF-8 字节数远超 256，但 chars().count() == 256，应通过）
        let cn: String = std::iter::repeat_n('你', 256).collect();
        server.handle_message(eid, ClientMessage::Chat { content: cn });
        let chat = find_outbox(&server, |m| matches!(m.message, ServerMessage::Chat { .. }));
        assert!(chat.is_some(), "256-char unicode should pass");
    }

    #[test]
    fn chat_rate_limit_drops_after_5_per_3s() {
        let mut server = Server::new(0);
        let eid = server.add_player("A".into());
        server.drain_outbox();

        // 同一 tick 内连发 6 条：前 5 条通过，第 6 条被丢弃
        for i in 0..6 {
            server.handle_message(
                eid,
                ClientMessage::Chat {
                    content: format!("m{i}"),
                },
            );
        }
        let count = server
            .outbox
            .iter()
            .filter(|m| matches!(m.message, ServerMessage::Chat { .. }))
            .count();
        assert_eq!(count, 5, "expected 5 chats to pass, got {count}");

        // 推进时间窗外（>180 ticks），再发应该重新允许
        server.drain_outbox();
        server.tick = server.tick.saturating_add(CHAT_RATE_WINDOW_TICKS + 1);
        server.handle_message(
            eid,
            ClientMessage::Chat {
                content: "after-window".into(),
            },
        );
        let count_after = server
            .outbox
            .iter()
            .filter(|m| matches!(&m.message, ServerMessage::Chat { content, .. } if content == "after-window"))
            .count();
        assert_eq!(count_after, 1, "expected chat to pass after window expiry");
    }

    #[test]
    fn tick_enqueues_player_tick_with_all_players_to_all() {
        let mut server = Server::new(0);
        let eid1 = server.add_player("A".into());
        let eid2 = server.add_player("B".into());
        server.drain_outbox();

        server.set_clock(5000);
        server.tick();

        let pt = find_outbox(&server, |m| {
            matches!(m.message, ServerMessage::PlayerTick { .. })
        });
        assert!(pt.is_some());
        if let Some((
            rec,
            ServerMessage::PlayerTick {
                players,
                server_time_ms,
                ..
            },
        )) = pt
        {
            assert_eq!(rec, Recipient::All);
            assert_eq!(server_time_ms, 5000);
            assert_eq!(players.len(), 2);
            assert!(players.iter().any(|p| p.entity_id == eid1));
            assert!(players.iter().any(|p| p.entity_id == eid2));
        }
    }

    #[test]
    fn tick_without_players_does_not_enqueue_player_tick() {
        let mut server = Server::new(0);
        server.tick();
        let pt = find_outbox(&server, |m| {
            matches!(m.message, ServerMessage::PlayerTick { .. })
        });
        assert!(pt.is_none(), "empty world should not produce PlayerTick");
    }

    #[test]
    fn hello_in_handle_message_is_warning_no_op() {
        // 防御性：调用方应改调 add_player；handle_message 收到 Hello 也不该 panic 或副作用。
        let mut server = Server::new(0);
        server.handle_message(
            999,
            ClientMessage::Hello {
                display_name: "X".into(),
                version: 1,
            },
        );
        assert!(server.players.is_empty());
        assert!(server.outbox.is_empty());
    }
}
