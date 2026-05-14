//! VoxWeb P2P 网络层。
//!
//! 三种 NetEndpoint：
//! - `Local`：单机模式。client ↔ server 通过 futures mpsc 双向通道，无网络。
//! - `Host`：房主。在本地继续以 mpsc 跑自己的 client ↔ server；同时维护多个
//!   [`PeerConnection`] 接受 Remote。Phase 5 起完整接入 outbox 路由（broadcast/unicast）。
//! - `Remote`：远端客户端。一个 [`PeerConnection`] 连到 Host；client 发出的 ClientMessage
//!   按 [`transport::ChannelKind`] 通过 DataChannel 发出，收到的 ServerMessage 入 inbox。
//!
//! 主驱动在 [`NetEndpoint::poll`]：每帧 RAF 主循环调用，把信令事件 / Peer 事件推进状态机，
//! 并把 Remote 端的字节流反序列化后入 inbox。

pub mod peer;
pub mod room;
pub mod signaling;
pub mod transport;

use std::collections::{HashMap, VecDeque};

use futures_channel::mpsc::{self, UnboundedReceiver, UnboundedSender};

use voxweb_core::protocol::{
    ClientMessage, EntityId, OutboundMessage, Recipient, RoomEvent, ServerMessage,
};

pub use peer::{PeerConnection, PeerEvent, PeerState};
pub use room::{NegotiationProgress, RoomSession};
pub use signaling::{IceServerConfig, Role, SignalingClient, SignalingEvent};
pub use transport::ChannelKind;

/// 网络错误。本期只是粗粒度分类；Phase 5+ 视需要细化。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetError {
    SignalingUnreachable,
    PeerConnectionFailed,
    DataChannelClosed,
    InvalidRoomId,
    Timeout,
}

/// 接收端的 ServerInbox（Server 持有，对偶于 NetEndpoint::Local）。
pub struct ServerInbox {
    pub rx_client: UnboundedReceiver<ClientMessage>,
    pub tx_server: UnboundedSender<ServerMessage>,
}

impl ServerInbox {
    pub fn try_recv_client_message(&mut self) -> Option<ClientMessage> {
        self.rx_client.try_recv().ok()
    }

    pub fn send_server_message(&self, msg: ServerMessage) {
        let _ = self.tx_server.unbounded_send(msg);
    }
}

/// Host 端记录每个 Remote 的待办协商进度（用于 PeerConnection 建立中的诊断）。
#[derive(Default)]
pub struct PendingNegotiation {
    pub offer_sent: bool,
    pub answer_received: bool,
}

/// outbox 路由的执行计划：哪些 peer 走 DC，是否还要送给本地 Host。
/// 纯数据结构便于单元测试，避免触碰 PeerConnection / mpsc。
#[derive(Debug, PartialEq)]
pub struct RoutingPlan {
    pub peers_to_send: Vec<u32>,
    pub send_to_local: bool,
}

/// 给定一条 OutboundMessage 与当前的 peer→entity 映射 + Host 自身 eid，
/// 计算该消息应该发往哪些 peer 以及是否要回流到本地 Host。
///
/// 提取为独立纯函数以便单元测试；`host_route_outbox` 是它的 IO 包装。
pub fn plan_route(
    msg: &OutboundMessage,
    peer_to_entity: &HashMap<u32, EntityId>,
    host_self: Option<EntityId>,
) -> RoutingPlan {
    match msg.recipient {
        Recipient::All => RoutingPlan {
            peers_to_send: peer_to_entity.keys().copied().collect(),
            send_to_local: host_self.is_some(),
        },
        Recipient::Except(excluded) => {
            let peers_to_send = peer_to_entity
                .iter()
                .filter(|(_, eid)| **eid != excluded)
                .map(|(pid, _)| *pid)
                .collect();
            let send_to_local = match host_self {
                Some(self_eid) => self_eid != excluded,
                None => false,
            };
            RoutingPlan {
                peers_to_send,
                send_to_local,
            }
        }
        Recipient::One(target) => {
            if host_self == Some(target) {
                RoutingPlan {
                    peers_to_send: vec![],
                    send_to_local: true,
                }
            } else {
                let peer = peer_to_entity
                    .iter()
                    .find(|(_, eid)| **eid == target)
                    .map(|(pid, _)| *pid);
                RoutingPlan {
                    peers_to_send: peer.into_iter().collect(),
                    send_to_local: false,
                }
            }
        }
    }
}

pub enum NetEndpoint {
    /// 单机模式：mpsc 双向通道。
    Local {
        tx_client: UnboundedSender<ClientMessage>,
        rx_server: UnboundedReceiver<ServerMessage>,
    },
    /// 房主：本地 mpsc + 信令 + 多个 Peer。
    Host {
        /// 自身玩家走的 mpsc（同 Local），driver 直接调 server.handle_message。
        tx_client: UnboundedSender<ClientMessage>,
        rx_server: UnboundedReceiver<ServerMessage>,
        signaling: SignalingClient,
        peers: HashMap<u32, PeerConnection>,
        pending: HashMap<u32, PendingNegotiation>,
        ice_servers: Vec<IceServerConfig>,
        session: RoomSession,
        room_id: String,
        display_name: String,
        /// peer_id → entity_id 映射（Phase 5）。
        /// 由 client 端在收到 Hello 时调 `host_register_peer` 写入；
        /// PeerLeft 时 `host_unregister_peer` 取出后传给 server.remove_player。
        peer_to_entity: HashMap<u32, EntityId>,
        /// Host 本人的 entity_id（add_player 第一次后由 client 端 `host_set_self_entity` 设置）。
        host_self_entity_id: Option<EntityId>,
    },
    /// 远端客户端：单条到 Host 的 Peer。
    Remote {
        signaling: SignalingClient,
        host: Option<PeerConnection>,
        host_peer_id: Option<u32>,
        ice_servers: Vec<IceServerConfig>,
        session: RoomSession,
        room_id: String,
        display_name: String,
        /// DC open 前积压的 ClientMessage（Hello / 早期 PlayerInput）。
        outbox: VecDeque<ClientMessage>,
        /// 从 Host 收到的 ServerMessage 队列（供 client try_recv_server_message 拉取）。
        inbox: VecDeque<ServerMessage>,
    },
}

impl NetEndpoint {
    /// 创建 Local 端点 + 对偶 ServerInbox。
    pub fn new_local_pair() -> (Self, ServerInbox) {
        let (tx_client, rx_client) = mpsc::unbounded::<ClientMessage>();
        let (tx_server, rx_server) = mpsc::unbounded::<ServerMessage>();
        (
            NetEndpoint::Local {
                tx_client,
                rx_server,
            },
            ServerInbox {
                rx_client,
                tx_server,
            },
        )
    }

    /// Host 模式：本地 mpsc + 信令连接（角色 host）。
    pub fn new_host(
        signaling_url: &str,
        room_id: &str,
        display_name: &str,
    ) -> Result<(Self, ServerInbox), NetError> {
        let signaling = SignalingClient::connect(signaling_url, room_id, Role::Host, display_name)?;
        let (tx_client, rx_client) = mpsc::unbounded::<ClientMessage>();
        let (tx_server, rx_server) = mpsc::unbounded::<ServerMessage>();
        let endpoint = NetEndpoint::Host {
            tx_client,
            rx_server,
            signaling,
            peers: HashMap::new(),
            pending: HashMap::new(),
            ice_servers: Vec::new(),
            session: RoomSession::SignalingConnect,
            room_id: room_id.to_string(),
            display_name: display_name.to_string(),
            peer_to_entity: HashMap::new(),
            host_self_entity_id: None,
        };
        let inbox = ServerInbox {
            rx_client,
            tx_server,
        };
        Ok((endpoint, inbox))
    }

    /// Remote 模式：信令连接（角色 join）。不带本地 mpsc（Remote 不直接驱动 server）。
    pub fn new_remote(
        signaling_url: &str,
        room_id: &str,
        display_name: &str,
    ) -> Result<Self, NetError> {
        let signaling = SignalingClient::connect(signaling_url, room_id, Role::Join, display_name)?;
        Ok(NetEndpoint::Remote {
            signaling,
            host: None,
            host_peer_id: None,
            ice_servers: Vec::new(),
            session: RoomSession::SignalingConnect,
            room_id: room_id.to_string(),
            display_name: display_name.to_string(),
            outbox: VecDeque::new(),
            inbox: VecDeque::new(),
        })
    }

    pub fn session(&self) -> &RoomSession {
        match self {
            NetEndpoint::Local { .. } => &CONNECTED_SESSION,
            NetEndpoint::Host { session, .. } | NetEndpoint::Remote { session, .. } => session,
        }
    }

    /// 发送 ClientMessage。
    /// - Local / Host：推到本端 mpsc（同进程 server 处理）；
    /// - Remote：序列化 + 走 transport 选 DC 发；未 open 时存 outbox。
    pub fn send_client_message(&mut self, msg: ClientMessage) {
        match self {
            NetEndpoint::Local { tx_client, .. } => {
                let _ = tx_client.unbounded_send(msg);
            }
            NetEndpoint::Host { tx_client, .. } => {
                let _ = tx_client.unbounded_send(msg);
            }
            NetEndpoint::Remote { host, outbox, .. } => {
                let channel = transport::channel_for_client_message(&msg);
                let connected = host.as_ref().is_some_and(|pc| pc.is_open(channel));
                if connected {
                    let pc = host.as_ref().expect("host is Some(_)");
                    match transport::encode_client_message(&msg) {
                        Ok(bytes) => {
                            if let Err(e) = pc.send(channel, &bytes) {
                                log::warn!("[net] Remote send failed: {e:?}");
                            }
                        }
                        Err(e) => log::warn!("[net] encode client message: {e}"),
                    }
                } else {
                    // 还没开连接 → 暂存；DC open 时 flush
                    outbox.push_back(msg);
                }
            }
        }
    }

    /// 非阻塞拉取一条 ServerMessage（Local / Remote）。
    pub fn try_recv_server_message(&mut self) -> Option<ServerMessage> {
        match self {
            NetEndpoint::Local { rx_server, .. } | NetEndpoint::Host { rx_server, .. } => {
                rx_server.try_recv().ok()
            }
            NetEndpoint::Remote { inbox, .. } => inbox.pop_front(),
        }
    }

    /// Phase 5：注册 peer ↔ entity 映射（Host 模式）。
    /// 由 client 端在收到 Remote 的 Hello 时调 `server.add_player(...)` 得到 eid 后调用。
    pub fn host_register_peer(&mut self, peer_id: u32, entity_id: EntityId) {
        if let NetEndpoint::Host { peer_to_entity, .. } = self {
            peer_to_entity.insert(peer_id, entity_id);
        }
    }

    /// Phase 5：取消注册并返回该 peer 对应的 entity_id。
    /// 由 client 端在收到 `RoomEvent::RemoteLeft { peer_id }` 时调用，
    /// 取得 eid 后再调 `server.remove_player(eid)`。
    pub fn host_unregister_peer(&mut self, peer_id: u32) -> Option<EntityId> {
        if let NetEndpoint::Host { peer_to_entity, .. } = self {
            peer_to_entity.remove(&peer_id)
        } else {
            None
        }
    }

    /// Phase 5：查询某个 peer 对应的 entity_id（不删除）。
    pub fn host_peer_entity(&self, peer_id: u32) -> Option<EntityId> {
        if let NetEndpoint::Host { peer_to_entity, .. } = self {
            peer_to_entity.get(&peer_id).copied()
        } else {
            None
        }
    }

    /// Phase 5：克隆当前 peer_to_entity 表（用于 client 端在闭包中查 eid 时绕过 &mut self 借用冲突）。
    pub fn host_peer_to_entity_clone(&self) -> HashMap<u32, EntityId> {
        if let NetEndpoint::Host { peer_to_entity, .. } = self {
            peer_to_entity.clone()
        } else {
            HashMap::new()
        }
    }

    /// Phase 5：设置 Host 自身的 entity_id。Host 启动时 `server.add_player("Host")` 拿到 eid 后立即调一次。
    pub fn host_set_self_entity(&mut self, eid: EntityId) {
        if let NetEndpoint::Host {
            host_self_entity_id,
            ..
        } = self
        {
            *host_self_entity_id = Some(eid);
        }
    }

    /// Phase 5：把 server 的 outbox 按 Recipient 路由分发：
    /// - 走 peer DC 的部分调 [`PeerConnection::send`] 序列化发送；
    /// - 走本地 Host 的部分通过 `local_inbox.send_server_message` 喂回 client 主循环。
    ///
    /// 仅 Host 模式生效；Local / Remote 调用是 no-op。
    pub fn host_route_outbox(&mut self, msgs: Vec<OutboundMessage>, local_inbox: &ServerInbox) {
        let NetEndpoint::Host {
            peers,
            peer_to_entity,
            host_self_entity_id,
            ..
        } = self
        else {
            return;
        };

        for msg in msgs {
            let plan = plan_route(&msg, peer_to_entity, *host_self_entity_id);
            // 先准备字节（同一条消息发给多个 peer 时复用编码结果）
            let bytes = if plan.peers_to_send.is_empty() {
                None
            } else {
                match transport::encode_server_message(&msg.message) {
                    Ok(b) => Some(b),
                    Err(e) => {
                        log::warn!("[net/host] encode server message: {e}");
                        None
                    }
                }
            };
            let channel = transport::channel_for_server_message(&msg.message);

            if let Some(b) = bytes {
                for pid in &plan.peers_to_send {
                    if let Some(pc) = peers.get(pid)
                        && pc.is_open(channel)
                        && let Err(e) = pc.send(channel, &b)
                    {
                        log::warn!("[net/host] send to peer {pid} failed: {e:?}");
                    }
                }
            }

            if plan.send_to_local {
                local_inbox.send_server_message(msg.message);
            }
        }
    }

    /// 每帧推进网络状态机。
    ///
    /// `peer_msg_handler`：Host 模式下，从某个 peer 收到 ClientMessage 时调用。
    /// 它接受 `(peer_id, ClientMessage)`；闭包内部由 client 端负责：
    /// - 若是 Hello：校验 version → `server.add_player(...)` → 记录 peer_id ↔ entity_id（待 poll 返回后通过
    ///   [`NetEndpoint::host_register_peer`] 写入 endpoint）；
    /// - 否则：从 `peer_to_entity` 查 entity_id → `server.handle_message(eid, msg)`。
    ///
    /// 闭包不返回 — 所有响应通过 server.outbox 流出，由 client 调
    /// [`NetEndpoint::host_route_outbox`] 路由。
    ///
    /// Local / Remote 忽略此参数。
    ///
    /// 返回房间生命周期事件，供 UI 推进 AppState 与 host_unregister_peer 时机决策。
    pub fn poll(
        &mut self,
        mut peer_msg_handler: Option<&mut dyn FnMut(u32, ClientMessage)>,
    ) -> Vec<RoomEvent> {
        let mut out: Vec<RoomEvent> = Vec::new();
        match self {
            NetEndpoint::Local { .. } => {}
            NetEndpoint::Host {
                signaling,
                peers,
                pending,
                ice_servers,
                session,
                ..
            } => {
                poll_host(
                    signaling,
                    peers,
                    pending,
                    ice_servers,
                    session,
                    &mut peer_msg_handler,
                    &mut out,
                );
            }
            NetEndpoint::Remote {
                signaling,
                host,
                host_peer_id,
                ice_servers,
                session,
                outbox,
                inbox,
                ..
            } => {
                poll_remote(
                    signaling,
                    host,
                    host_peer_id,
                    ice_servers,
                    session,
                    outbox,
                    inbox,
                    &mut out,
                );
            }
        }
        out
    }
}

const CONNECTED_SESSION: RoomSession = RoomSession::Connected;

// ──────────────────────────────────────────────────────────────
// Host poll
// ──────────────────────────────────────────────────────────────

fn poll_host(
    signaling: &mut SignalingClient,
    peers: &mut HashMap<u32, PeerConnection>,
    pending: &mut HashMap<u32, PendingNegotiation>,
    ice_servers: &mut Vec<IceServerConfig>,
    session: &mut RoomSession,
    peer_msg_handler: &mut Option<&mut dyn FnMut(u32, ClientMessage)>,
    out: &mut Vec<RoomEvent>,
) {
    // 1) 信令事件
    for ev in signaling.poll() {
        match ev {
            SignalingEvent::Open => {
                *session = RoomSession::AwaitRegistered;
            }
            SignalingEvent::Registered {
                ice_servers: srvs, ..
            } => {
                *ice_servers = srvs;
                session.enter_negotiating();
                // Host 注册成功就可以视为 "Connected"（玩家可立刻进入空房）
                out.push(RoomEvent::Connected);
            }
            SignalingEvent::PeerJoined { peer_id } => {
                match PeerConnection::create_offerer(peer_id, ice_servers) {
                    Ok(pc) => {
                        // 立即发起 offer
                        pc.start_offer();
                        peers.insert(peer_id, pc);
                        pending.insert(peer_id, PendingNegotiation::default());
                        session.mark_offer_exchanged();
                    }
                    Err(e) => {
                        log::warn!("[net/host] create_offerer({peer_id}) failed: {e:?}");
                    }
                }
            }
            SignalingEvent::PeerLeft { peer_id } => {
                if let Some(pc) = peers.remove(&peer_id) {
                    pc.close();
                }
                pending.remove(&peer_id);
                // Phase 5：通知 client 端做 host_unregister_peer + server.remove_player
                out.push(RoomEvent::RemoteLeft { peer_id });
                out.push(RoomEvent::PeerCount(peers.len() as u32));
            }
            SignalingEvent::Answer { from, sdp } => {
                if let Some(pc) = peers.get(&from) {
                    pc.apply_answer(sdp);
                    if let Some(p) = pending.get_mut(&from) {
                        p.answer_received = true;
                    }
                    session.mark_answer_exchanged();
                }
            }
            SignalingEvent::Ice { from, candidate } => {
                if let Some(pc) = peers.get(&from) {
                    pc.add_remote_ice(candidate);
                }
            }
            SignalingEvent::Offer { .. } => {
                // Host 不接受 inbound offer（Remote → Host 是 join 角色，由 Host 主动发 offer）
            }
            SignalingEvent::RoomClosed { reason } => {
                *session = RoomSession::Disconnected {
                    reason: reason.clone(),
                };
                out.push(RoomEvent::Disconnected { reason });
            }
            SignalingEvent::ServerError { message } | SignalingEvent::SocketError { message } => {
                log::warn!("[net/host] signaling error: {message}");
                out.push(RoomEvent::SignalingError(message));
            }
            SignalingEvent::Closed => {
                // 信令关闭对已建立的 PC 没影响（设计如此）；不切 session
                log::info!("[net/host] signaling socket closed");
            }
        }
    }

    // 2) 每个 peer 的事件
    let peer_ids: Vec<u32> = peers.keys().copied().collect();
    for peer_id in peer_ids {
        let events = peers.get(&peer_id).map(|pc| pc.poll()).unwrap_or_default();
        for ev in events {
            match ev {
                PeerEvent::OfferReady(sdp) => {
                    signaling.send_offer(peer_id, &sdp);
                    if let Some(p) = pending.get_mut(&peer_id) {
                        p.offer_sent = true;
                    }
                }
                PeerEvent::AnswerReady(_) => {
                    // Host 不应该产 answer（它是 offerer）
                }
                PeerEvent::RemoteDescApplied | PeerEvent::IceApplied => {}
                PeerEvent::LocalIce(candidate_json) => {
                    signaling.send_ice(peer_id, &candidate_json);
                }
                PeerEvent::Message { channel: _, bytes } => {
                    match transport::decode_client_message(&bytes) {
                        Ok(msg) => {
                            if let Some(handle) = peer_msg_handler.as_mut() {
                                // Phase 5：闭包内部负责 Hello → add_player / 其他 → handle_message。
                                // 不再向闭包要回复 — 所有响应通过 server.outbox 流出，
                                // 由调用方 (client lib) 在 poll 后调 host_route_outbox 路由。
                                (**handle)(peer_id, msg);
                            }
                        }
                        Err(e) => log::warn!("[net/host] decode peer msg from {peer_id}: {e}"),
                    }
                }
                PeerEvent::StateChanged(state) => {
                    if state == PeerState::Connected {
                        session.mark_dc_open();
                        out.push(RoomEvent::PeerCount(peers.len() as u32));
                    } else if matches!(state, PeerState::Disconnected | PeerState::Failed) {
                        if let Some(pc) = peers.remove(&peer_id) {
                            pc.close();
                        }
                        pending.remove(&peer_id);
                        // Phase 5：让 client 端清理 server-side 玩家表
                        out.push(RoomEvent::RemoteLeft { peer_id });
                        out.push(RoomEvent::PeerCount(peers.len() as u32));
                    }
                }
                PeerEvent::NegotiationError(msg) => {
                    log::warn!("[net/host] peer {peer_id} negotiation error: {msg}");
                }
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────
// Remote poll
// ──────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn poll_remote(
    signaling: &mut SignalingClient,
    host: &mut Option<PeerConnection>,
    host_peer_id: &mut Option<u32>,
    ice_servers: &mut Vec<IceServerConfig>,
    session: &mut RoomSession,
    outbox: &mut VecDeque<ClientMessage>,
    inbox: &mut VecDeque<ServerMessage>,
    out: &mut Vec<RoomEvent>,
) {
    // 1) 信令事件
    for ev in signaling.poll() {
        match ev {
            SignalingEvent::Open => {
                *session = RoomSession::AwaitRegistered;
            }
            SignalingEvent::Registered {
                existing_peers,
                ice_servers: srvs,
                ..
            } => {
                *ice_servers = srvs;
                // Phase 4：第一个 existing peer 默认是 host
                if let Some(&id) = existing_peers.first() {
                    *host_peer_id = Some(id);
                    match PeerConnection::create_answerer(id, ice_servers) {
                        Ok(pc) => *host = Some(pc),
                        Err(e) => {
                            log::warn!("[net/remote] create_answerer failed: {e:?}");
                            out.push(RoomEvent::SignalingError(format!("{e:?}")));
                            *session = RoomSession::Disconnected {
                                reason: "create_answerer failed".into(),
                            };
                            return;
                        }
                    }
                }
                session.enter_negotiating();
            }
            SignalingEvent::Offer { from, sdp } => {
                // 只接受来自 host 的 offer
                if Some(from) == *host_peer_id
                    && let Some(pc) = host.as_ref()
                {
                    pc.accept_offer(sdp);
                    session.mark_offer_exchanged();
                }
            }
            SignalingEvent::Ice { from, candidate } => {
                if Some(from) == *host_peer_id
                    && let Some(pc) = host.as_ref()
                {
                    pc.add_remote_ice(candidate);
                }
            }
            SignalingEvent::PeerLeft { peer_id } => {
                if Some(peer_id) == *host_peer_id {
                    out.push(RoomEvent::Disconnected {
                        reason: "host_left".into(),
                    });
                    *session = RoomSession::Disconnected {
                        reason: "host_left".into(),
                    };
                }
            }
            SignalingEvent::PeerJoined { .. } | SignalingEvent::Answer { .. } => {
                // Remote 只关心 host 的 offer；Answer 是 Host 接收
            }
            SignalingEvent::RoomClosed { reason } => {
                *session = RoomSession::Disconnected {
                    reason: reason.clone(),
                };
                out.push(RoomEvent::Disconnected { reason });
            }
            SignalingEvent::ServerError { message } | SignalingEvent::SocketError { message } => {
                log::warn!("[net/remote] signaling error: {message}");
                out.push(RoomEvent::SignalingError(message));
            }
            SignalingEvent::Closed => {
                log::info!("[net/remote] signaling socket closed");
            }
        }
    }

    // 2) Host PeerConnection 事件
    if let Some(pc) = host.as_ref() {
        for ev in pc.poll() {
            match ev {
                PeerEvent::AnswerReady(sdp) => {
                    if let Some(host_id) = *host_peer_id {
                        signaling.send_answer(host_id, &sdp);
                        session.mark_answer_exchanged();
                    }
                }
                PeerEvent::OfferReady(_) => {
                    // Remote 不主动产 offer
                }
                PeerEvent::RemoteDescApplied | PeerEvent::IceApplied => {}
                PeerEvent::LocalIce(candidate_json) => {
                    if let Some(host_id) = *host_peer_id {
                        signaling.send_ice(host_id, &candidate_json);
                    }
                }
                PeerEvent::Message { channel: _, bytes } => {
                    match transport::decode_server_message(&bytes) {
                        Ok(msg) => inbox.push_back(msg),
                        Err(e) => log::warn!("[net/remote] decode server msg: {e}"),
                    }
                }
                PeerEvent::StateChanged(state) => {
                    if state == PeerState::Connected {
                        session.mark_dc_open();
                        *session = RoomSession::Connected;
                        out.push(RoomEvent::Connected);
                        // flush outbox
                        let drained: Vec<_> = outbox.drain(..).collect();
                        for msg in drained {
                            let channel = transport::channel_for_client_message(&msg);
                            if pc.is_open(channel) {
                                if let Ok(b) = transport::encode_client_message(&msg) {
                                    let _ = pc.send(channel, &b);
                                }
                            } else {
                                outbox.push_back(msg);
                            }
                        }
                    } else if matches!(state, PeerState::Disconnected | PeerState::Failed) {
                        *session = RoomSession::Disconnected {
                            reason: "peer_disconnected".into(),
                        };
                        out.push(RoomEvent::Disconnected {
                            reason: "peer_disconnected".into(),
                        });
                    }
                }
                PeerEvent::NegotiationError(msg) => {
                    log::warn!("[net/remote] negotiation error: {msg}");
                }
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────
// 单元测试（Local + 纯函数路由；WebRTC 部分需浏览器集成测试 v2 引入）
// ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use voxweb_core::protocol::AckReason;

    #[test]
    fn local_pair_roundtrip() {
        let (mut endpoint, mut inbox) = NetEndpoint::new_local_pair();

        endpoint.send_client_message(ClientMessage::Ping { client_time_ms: 42 });
        let received = inbox.try_recv_client_message();
        assert!(matches!(
            received,
            Some(ClientMessage::Ping { client_time_ms: 42 })
        ));

        inbox.send_server_message(ServerMessage::Pong {
            client_time_ms: 42,
            server_time_ms: 100,
        });
        let received = endpoint.try_recv_server_message();
        assert!(matches!(
            received,
            Some(ServerMessage::Pong {
                client_time_ms: 42,
                server_time_ms: 100
            })
        ));
    }

    #[test]
    fn try_recv_returns_none_when_empty() {
        let (mut endpoint, mut inbox) = NetEndpoint::new_local_pair();
        assert!(endpoint.try_recv_server_message().is_none());
        assert!(inbox.try_recv_client_message().is_none());
    }

    /// 构造一个 OutboundMessage（recipient 任意；payload 固定）。
    fn outbound(recipient: Recipient) -> OutboundMessage {
        OutboundMessage {
            recipient,
            message: ServerMessage::ActionAck {
                request_id: 0,
                accepted: true,
                reason: AckReason::Ok,
            },
        }
    }

    fn mapping(pairs: &[(u32, EntityId)]) -> HashMap<u32, EntityId> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn route_all_sends_to_all_peers_and_local() {
        let map = mapping(&[(101, 2), (102, 3)]);
        let plan = plan_route(&outbound(Recipient::All), &map, Some(1));
        assert!(plan.send_to_local);
        let mut got = plan.peers_to_send.clone();
        got.sort();
        assert_eq!(got, vec![101, 102]);
    }

    #[test]
    fn route_all_without_host_self_skips_local() {
        let map = mapping(&[(101, 2)]);
        let plan = plan_route(&outbound(Recipient::All), &map, None);
        assert!(!plan.send_to_local);
        assert_eq!(plan.peers_to_send, vec![101]);
    }

    #[test]
    fn route_except_skips_target_entity_on_peers_and_local() {
        let map = mapping(&[(101, 2), (102, 3)]);
        // 排除 eid=2 → 应该跳过 peer 101，但 host_self=1 仍要收
        let plan = plan_route(&outbound(Recipient::Except(2)), &map, Some(1));
        assert_eq!(plan.peers_to_send, vec![102]);
        assert!(plan.send_to_local);

        // 排除 host_self（eid=1）→ 应该跳过 local，但所有 peer 仍要收
        let plan2 = plan_route(&outbound(Recipient::Except(1)), &map, Some(1));
        let mut got = plan2.peers_to_send.clone();
        got.sort();
        assert_eq!(got, vec![101, 102]);
        assert!(!plan2.send_to_local);
    }

    #[test]
    fn route_one_to_host_self_only_goes_local() {
        let map = mapping(&[(101, 2), (102, 3)]);
        let plan = plan_route(&outbound(Recipient::One(1)), &map, Some(1));
        assert!(plan.send_to_local);
        assert!(plan.peers_to_send.is_empty());
    }

    #[test]
    fn route_one_to_remote_peer_goes_single_peer() {
        let map = mapping(&[(101, 2), (102, 3)]);
        let plan = plan_route(&outbound(Recipient::One(3)), &map, Some(1));
        assert!(!plan.send_to_local);
        assert_eq!(plan.peers_to_send, vec![102]);
    }

    #[test]
    fn route_one_to_unknown_entity_routes_nothing() {
        let map = mapping(&[(101, 2)]);
        let plan = plan_route(&outbound(Recipient::One(999)), &map, Some(1));
        assert!(!plan.send_to_local);
        assert!(plan.peers_to_send.is_empty());
    }
}
