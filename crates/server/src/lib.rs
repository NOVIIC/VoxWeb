//! VoxWeb 世界逻辑层（lib only）。
//! 负责地形生成、物理仲裁、方块操作校验、持久化触发。
//!
//! ## 角色与调用方
//!
//! - **Local-Only**：Client 直接持 `Rc<RefCell<Server>>`，每帧 `tick()` + 通过 mpsc 投递 ClientMessage。
//! - **Host**：与 Local 相同，额外接收来自远端 Peer 的 ClientMessage（由 [`voxweb_net::NetEndpoint`] 解码后调
//!   `handle_message`）。`broadcast_tick` 把 PlayerTick 加进 outbox 由 net 层路由。
//! - **Remote**：客户端仍持有一个 `Server` 实例**但仅作为方块数据宿主** — Phase 5 的取舍是不引入独立的
//!   `WorldView`，而是让 FieldSnapshot 直接写入 `server.world`。
//!   Remote 模式下 `tick()` / `handle_message()` 不会被主循环驱动；任何对这两个方法的调用都意味着调用方搞错了。

pub mod persistence;
pub mod physics;
pub mod terrain;
pub mod world;

use std::collections::{HashMap, HashSet, VecDeque};

use glam::Vec3;

use voxweb_core::block::MaterialCell;
use voxweb_core::chunk::{CHUNK_X, CHUNK_Z, ChunkPos};
use voxweb_core::protocol::{
    ClientMessage, EntityId, FIELD_SNAPSHOT_PAYLOAD_MAX, OutboundMessage, PlayerEntry,
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
            last_input_tick: self.last_input_tick,
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
    /// Host 当前允许 Remote 使用的最大视距（单位：chunk）。
    host_render_distance: u32,
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

/// 初始 FieldSnapshot 的半径（chunk 数）。Phase 5 固定值；后续随渲染距离动态调。
pub const INITIAL_SNAPSHOT_RADIUS: i32 = 6;
/// Host 默认视距（与 AppSettings 默认值保持一致）。
pub const DEFAULT_HOST_RENDER_DISTANCE: u32 = INITIAL_SNAPSHOT_RADIUS as u32;
/// Remote 单次区块请求最多处理多少个 chunk。
/// 当前设置 UI 最大渲染距离为 10，一次完整视距请求为 21×21=441 个 chunk；
/// 512 能覆盖完整首包，同时避免恶意客户端单包塞入无限列表。
const CHUNK_REQUEST_MAX_BATCH: usize = 512;
/// Remote 请求中心最多允许领先服务端记录位置多少个 chunk。
/// `FieldRequest` 与 `PlayerInput` 分属 reliable/unreliable 通道，可能先后倒置；
/// 留 2 个 chunk 的余量避免刚跨边界时误拒合法请求。
const CHUNK_REQUEST_CENTER_GRACE: i32 = 2;
/// 绝对上限兜底，避免异常设置或恶意包请求过大半径。
const CHUNK_REQUEST_MAX_DISTANCE: i32 = 16;

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
            host_render_distance: DEFAULT_HOST_RENDER_DISTANCE,
            outbox: VecDeque::new(),
            chat_window: HashMap::new(),
        }
    }

    /// 由 Host 主循环每帧更新（performance.now() 毫秒）；Pong/PlayerTick 中带回。
    pub fn set_clock(&mut self, ms: u64) {
        self.current_time_ms = ms;
    }

    /// 设置 Host 允许 Remote 使用的最大视距。
    ///
    /// Host 暂停菜单修改渲染距离后会调用这里。若房间已经有玩家，
    /// 变化会广播 `HostSettings`，让 Remote 立即收紧自己的有效视距。
    pub fn set_host_render_distance(&mut self, render_distance: u32) {
        let render_distance = render_distance.max(1);
        if self.host_render_distance == render_distance {
            return;
        }
        self.host_render_distance = render_distance;
        if self.host_entity_id.is_some() {
            self.enqueue(
                Recipient::All,
                ServerMessage::HostSettings { render_distance },
            );
        }
    }

    /// 每帧 tick（60Hz）：推进 world tick + 把当前所有玩家位置打包成 PlayerTick 广播。
    /// **Remote 模式不调用**（无权威逻辑可推进）。
    pub fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        let free_object_events = physics::tick_free_objects(&mut self.world);
        self.enqueue_free_object_states(free_object_events.states);
        self.enqueue_free_object_projections(free_object_events.projections);
        self.world.tick();
        self.broadcast_tick();
    }

    /// 加入一个玩家：分配 entity_id、入 players 表，并 enqueue 三类消息：
    /// - Welcome → `Recipient::One(eid)`
    /// - PeerJoined → `Recipient::Except(eid)`（其他在场玩家收到通告）
    /// - FieldSnapshot 分片 → `Recipient::One(eid)`（出生点周围 INITIAL_SNAPSHOT_RADIUS 内的所有 chunk）
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
                host_render_distance: self.host_render_distance,
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
        let snapshot_radius = self
            .host_render_distance
            .min(INITIAL_SNAPSHOT_RADIUS as u32) as i32;
        self.send_initial_snapshot(eid, spawn_chunk, snapshot_radius);
        self.send_active_free_objects(eid);

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

    pub fn load_field_chunk_from_storage(&mut self, pos: ChunkPos, field: voxweb_core::FieldChunk) {
        self.world.load_field_chunk_from_storage(pos, field);
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
            ClientMessage::FieldRequest {
                center,
                render_distance,
                chunks,
            } => {
                self.handle_field_request(entity_id, center, render_distance, chunks);
            }
            ClientMessage::Break {
                pos,
                request_id,
                input_tick,
                player_position,
            } => {
                let player_feet =
                    self.action_player_position(entity_id, input_tick, player_position);
                let reason = physics::validate_break(&self.world, pos, player_feet);
                if reason == voxweb_core::protocol::AckReason::Ok {
                    self.world.set_cell(pos, MaterialCell::EMPTY);
                    let relaxed = physics::relax_after_edit(&mut self.world, pos);
                    let spawned = physics::resolve_floating_after_edit(&mut self.world, pos);
                    self.enqueue(
                        Recipient::One(entity_id),
                        ServerMessage::ActionAck {
                            request_id,
                            accepted: true,
                            reason,
                        },
                    );
                    self.enqueue_field_delta(pos, MaterialCell::EMPTY);
                    self.enqueue_field_deltas(relaxed);
                    self.enqueue_free_object_spawns(spawned);
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
                input_tick,
                player_position,
            } => {
                let player_feet =
                    self.action_player_position(entity_id, input_tick, player_position);
                let reason = physics::validate_place(&self.world, pos, block, player_feet);
                if reason == voxweb_core::protocol::AckReason::Ok {
                    self.world.set_cell(pos, MaterialCell::from_block_id(block));
                    let relaxed = physics::relax_after_edit(&mut self.world, pos);
                    let spawned = physics::resolve_floating_after_edit(&mut self.world, pos);
                    self.enqueue(
                        Recipient::One(entity_id),
                        ServerMessage::ActionAck {
                            request_id,
                            accepted: true,
                            reason,
                        },
                    );
                    self.enqueue_field_delta(pos, MaterialCell::from_block_id(block));
                    self.enqueue_field_deltas(relaxed);
                    self.enqueue_free_object_spawns(spawned);
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

    /// 取玩家点击某个可靠操作时的脚底位置。
    ///
    /// `Break` / `Place` 走 reliable 通道，常会先于最新的 unreliable `PlayerInput`
    /// 到达 Host。此时如果继续使用服务端缓存的旧位置做范围校验，高 RTT 下玩家会被误判
    /// `OutOfRange`。因此操作消息携带点击时的预测位置；若它对应的 input tick 更新，
    /// 顺手推进服务端玩家位置与 last_input_tick，让下一帧 PlayerTick 的回执也对齐。
    fn action_player_position(
        &mut self,
        entity_id: EntityId,
        input_tick: u32,
        player_position: Vec3,
    ) -> Vec3 {
        let Some(player) = self.players.get_mut(&entity_id) else {
            return DEFAULT_SPAWN;
        };
        if input_tick > player.last_input_tick {
            player.position = player_position;
            player.last_input_tick = input_tick;
        }
        player_position
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

    /// 把出生点附近的 FieldChunk 切片成 FieldSnapshot 分片塞进 outbox（Recipient::One(eid)）。
    /// 半径 = chunk 数，覆盖 `(2*radius+1)^2` 个 chunk。未加载的 chunk 会先 `ensure_chunk_generated`。
    pub fn send_initial_snapshot(&mut self, recipient: EntityId, center: ChunkPos, radius: i32) {
        for dx in -radius..=radius {
            for dz in -radius..=radius {
                let pos = ChunkPos::new(center.x + dx, center.z + dz);
                self.send_field_snapshot(recipient, pos);
            }
        }
    }

    fn send_active_free_objects(&mut self, recipient: EntityId) {
        let active = self
            .world
            .free_objects
            .values()
            .filter(|object| object.state == voxweb_core::FreeObjectState::Dynamic)
            .map(|object| {
                (
                    object.id,
                    object.cells_at_current_position(),
                    object.transform.position,
                    object.velocity,
                )
            })
            .collect::<Vec<_>>();
        for (object_id, cells, position, velocity) in active {
            self.enqueue(
                Recipient::One(recipient),
                ServerMessage::FreeObjectSpawn { object_id, cells },
            );
            self.enqueue(
                Recipient::One(recipient),
                ServerMessage::FreeObjectState {
                    object_id,
                    position,
                    velocity,
                },
            );
        }
    }

    /// 处理 Remote 的按需区块请求。
    ///
    /// 这里做两个轻量防护：单包处理数量上限，以及只能请求服务端记录位置附近的 chunk。
    /// 真正的可靠性仍由 DataChannel reliable 保证；客户端侧会记录 in-flight，避免每帧重复请求。
    fn handle_field_request(
        &mut self,
        entity_id: EntityId,
        center: ChunkPos,
        render_distance: u32,
        chunks: Vec<ChunkPos>,
    ) {
        let Some(player_center) = self
            .players
            .get(&entity_id)
            .map(|p| chunk_pos_of_world(p.position))
        else {
            return;
        };
        if chunk_distance(center, player_center) > CHUNK_REQUEST_CENTER_GRACE {
            log::debug!(
                "[server] ignore chunk request with far center: eid={entity_id} center={center:?} player_center={player_center:?}"
            );
            return;
        }

        let allowed_radius = render_distance
            .min(self.host_render_distance)
            .min(CHUNK_REQUEST_MAX_DISTANCE as u32) as i32;
        let mut seen = HashSet::new();
        let mut processed = 0usize;
        let mut sent = 0usize;
        for pos in chunks {
            if processed >= CHUNK_REQUEST_MAX_BATCH {
                break;
            }
            if !seen.insert(pos) {
                continue;
            }
            processed += 1;
            if chunk_distance(pos, center) > allowed_radius {
                log::debug!(
                    "[server] ignore far chunk request: eid={entity_id} pos={pos:?} center={center:?} allowed_radius={allowed_radius}"
                );
                continue;
            }
            self.send_field_snapshot(entity_id, pos);
            sent += 1;
        }
        log::debug!("[server] chunk request: eid={entity_id} processed={processed} sent={sent}");
    }

    /// 把单个 chunk 编码并分片发给指定玩家。未生成时会先按世界种子生成。
    fn send_field_snapshot(&mut self, recipient: EntityId, pos: ChunkPos) {
        self.world.ensure_chunk_generated(pos);
        let Some(field) = self.world.field_chunks.get(&pos) else {
            return;
        };
        let bytes = voxweb_core::field::encode(field);
        let total_len = bytes.len();
        let frag_count = total_len.div_ceil(FIELD_SNAPSHOT_PAYLOAD_MAX).max(1);
        debug_assert!(frag_count <= u16::MAX as usize, "field chunk too big");
        for (i, chunk_bytes) in bytes.chunks(FIELD_SNAPSHOT_PAYLOAD_MAX).enumerate() {
            self.enqueue(
                Recipient::One(recipient),
                ServerMessage::FieldSnapshot {
                    pos,
                    frag_index: i as u16,
                    frag_total: frag_count as u16,
                    payload: chunk_bytes.to_vec(),
                },
            );
        }
    }

    /// 内部：把一条消息压入 outbox。
    fn enqueue(&mut self, recipient: Recipient, message: ServerMessage) {
        self.outbox
            .push_back(OutboundMessage { recipient, message });
    }

    fn enqueue_field_delta(&mut self, pos: voxweb_core::Position, cell: MaterialCell) {
        self.enqueue(Recipient::All, ServerMessage::FieldDelta { pos, cell });
    }

    fn enqueue_field_deltas(&mut self, updates: Vec<(voxweb_core::Position, MaterialCell)>) {
        for (pos, cell) in updates {
            self.enqueue_field_delta(pos, cell);
        }
    }

    fn enqueue_free_object_spawns(&mut self, spawns: Vec<physics::FreeObjectSpawn>) {
        for spawn in spawns {
            for (pos, _) in &spawn.cells {
                self.enqueue_field_delta(*pos, MaterialCell::EMPTY);
            }
            self.enqueue(
                Recipient::All,
                ServerMessage::FreeObjectSpawn {
                    object_id: spawn.object_id,
                    cells: spawn.cells,
                },
            );
        }
    }

    fn enqueue_free_object_states(&mut self, states: Vec<physics::FreeObjectStateUpdate>) {
        for state in states {
            self.enqueue(
                Recipient::All,
                ServerMessage::FreeObjectState {
                    object_id: state.object_id,
                    position: state.position,
                    velocity: state.velocity,
                },
            );
        }
    }

    fn enqueue_free_object_projections(&mut self, projections: Vec<physics::FreeObjectProjection>) {
        for projection in projections {
            self.enqueue(
                Recipient::All,
                ServerMessage::FreeObjectProject {
                    object_id: projection.object_id,
                    deltas: projection.deltas,
                },
            );
        }
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

fn chunk_pos_of_world(world_pos: Vec3) -> ChunkPos {
    ChunkPos::new(
        (world_pos.x as i32).div_euclid(CHUNK_X as i32),
        (world_pos.z as i32).div_euclid(CHUNK_Z as i32),
    )
}

fn chunk_distance(a: ChunkPos, b: ChunkPos) -> i32 {
    (a.x - b.x).abs().max((a.z - b.z).abs())
}

#[cfg(test)]
mod handle_message_tests;
