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
mod relay;
pub mod room;
pub mod signaling;
pub mod transport;

use std::collections::{HashMap, HashSet, VecDeque};

use futures_channel::mpsc::{self, UnboundedReceiver, UnboundedSender};

use voxweb_core::protocol::{ClientMessage, EntityId, OutboundMessage, RoomEvent, ServerMessage};

pub use peer::{PeerConnection, PeerEvent, PeerState};
use relay::RelayRateLimiter;
pub use relay::{RoutingPlan, plan_route};
pub use room::{LoadingStep, NegotiationProgress, RoomSession, StepStatus};
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
        /// 已升级为中继的 peer 集合。这些 peer 的 RTCPeerConnection 已被关闭，
        /// 后续 server→peer 字节走信令 WS 二进制帧。
        relayed_peers: HashSet<u32>,
        /// 每个中继 peer 的本地发送限速器。
        /// Worker 仍是最终裁决者；这里提前节流，避免高视距 FieldSnapshot
        /// 在一帧内打爆 DO 的 `rate_limit`。
        relay_rate_limiters: HashMap<u32, RelayRateLimiter>,
        /// 已经发出 relay_request 但还在等服务端 RelayActive 的 peer。
        /// 用于避免对同一 peer 重复请求升级。
        relay_requested: HashSet<u32>,
        /// 每个 peer 协商起始时间（performance.now() ms）。用于 15s 超时升级。
        negotiation_started_ms: HashMap<u32, f64>,
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
        /// 若已切到中继模式，记录 Host 的 peer_id；此时 `host` 已被关闭并置 None。
        relayed_host: Option<u32>,
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
            relayed_peers: HashSet::new(),
            relay_rate_limiters: HashMap::new(),
            relay_requested: HashSet::new(),
            negotiation_started_ms: HashMap::new(),
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
            relayed_host: None,
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
            NetEndpoint::Remote {
                host,
                outbox,
                signaling,
                relayed_host,
                ..
            } => {
                // 中继模式：直接经由信令 WS 二进制帧发往 Host。
                if let Some(host_pid) = *relayed_host {
                    match transport::encode_client_message(&msg) {
                        Ok(bytes) => signaling.send_relay_binary(host_pid, &bytes),
                        Err(e) => log::warn!("[net] encode client message (relay): {e}"),
                    }
                    return;
                }
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
                    // 还没开连接 → 暂存；DC open 或 RelayActive 时 flush
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
    /// 返回因流控（reliable DC `bufferedAmount` 过高）未能发送的消息，
    /// 调用方应将其重新入队到下帧发送。
    ///
    /// 仅 Host 模式生效；Local / Remote 调用是 no-op。
    pub fn host_route_outbox(
        &mut self,
        msgs: Vec<OutboundMessage>,
        local_inbox: &ServerInbox,
    ) -> Vec<OutboundMessage> {
        let NetEndpoint::Host {
            peers,
            peer_to_entity,
            host_self_entity_id,
            relayed_peers,
            relay_rate_limiters,
            signaling,
            ..
        } = self
        else {
            return Vec::new();
        };

        let mut unsent: Vec<OutboundMessage> = Vec::new();

        let mut iter = msgs.into_iter();
        while let Some(msg) = iter.next() {
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

            // 流控检查：若任一目标 peer 的 reliable DC 因 bufferedAmount 暂停，整条消息推迟。
            // 中继 peer 走下面的本地令牌桶；两类流控都在发送前检查，避免半条消息已发再回滚。
            if let Some(_b) = &bytes
                && channel == crate::transport::ChannelKind::Reliable
            {
                let any_paused = plan.peers_to_send.iter().any(|pid| {
                    !relayed_peers.contains(pid)
                        && peers.get(pid).is_some_and(|pc| pc.is_reliable_paused())
                });
                if any_paused {
                    unsent.push(msg);
                    unsent.extend(iter);
                    break;
                }
            }

            if bytes.is_some() {
                let relayed_targets: Vec<u32> = plan
                    .peers_to_send
                    .iter()
                    .copied()
                    .filter(|pid| relayed_peers.contains(pid))
                    .collect();
                let now = now_ms();
                for pid in &relayed_targets {
                    relay_rate_limiters
                        .entry(*pid)
                        .or_insert_with(|| RelayRateLimiter::default(now));
                }
                let relay_budget_exhausted = relayed_targets.iter().any(|pid| {
                    relay_rate_limiters
                        .get_mut(pid)
                        .is_some_and(|limiter| !limiter.has_token(now))
                });
                if relay_budget_exhausted {
                    unsent.push(msg);
                    unsent.extend(iter);
                    break;
                }
                for pid in &relayed_targets {
                    if let Some(limiter) = relay_rate_limiters.get_mut(pid) {
                        limiter.consume_token(now);
                    }
                }
            }

            if let Some(b) = bytes {
                for pid in &plan.peers_to_send {
                    if relayed_peers.contains(pid) {
                        // 中继路径：信令 WS 二进制帧
                        signaling.send_relay_binary(*pid, &b);
                        continue;
                    }
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

        unsent
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
                relayed_peers,
                relay_rate_limiters,
                relay_requested,
                negotiation_started_ms,
                ..
            } => {
                poll_host(
                    signaling,
                    peers,
                    pending,
                    ice_servers,
                    session,
                    relayed_peers,
                    relay_rate_limiters,
                    relay_requested,
                    negotiation_started_ms,
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
                relayed_host,
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
                    relayed_host,
                    &mut out,
                );
            }
        }
        out
    }
}

const CONNECTED_SESSION: RoomSession = RoomSession::Connected;

/// 协商超时阈值：从 PeerJoined 起 15s 仍未 Connected → 触发中继升级。
const NEGOTIATION_TIMEOUT_MS: f64 = 15_000.0;

/// 取当前 performance.now() 毫秒；浏览器外（如 Node 测试）返回 0。
fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0)
}

// ──────────────────────────────────────────────────────────────
// Host poll
// ──────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn poll_host(
    signaling: &mut SignalingClient,
    peers: &mut HashMap<u32, PeerConnection>,
    pending: &mut HashMap<u32, PendingNegotiation>,
    ice_servers: &mut Vec<IceServerConfig>,
    session: &mut RoomSession,
    relayed_peers: &mut HashSet<u32>,
    relay_rate_limiters: &mut HashMap<u32, RelayRateLimiter>,
    relay_requested: &mut HashSet<u32>,
    negotiation_started_ms: &mut HashMap<u32, f64>,
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
                        negotiation_started_ms.insert(peer_id, now_ms());
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
                negotiation_started_ms.remove(&peer_id);
                relayed_peers.remove(&peer_id);
                relay_rate_limiters.remove(&peer_id);
                relay_requested.remove(&peer_id);
                // Phase 5：通知 client 端做 host_unregister_peer + server.remove_player
                out.push(RoomEvent::RemoteLeft { peer_id });
                out.push(RoomEvent::PeerCount(
                    peers.len() as u32 + relayed_peers.len() as u32,
                ));
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
            SignalingEvent::RelayActive {
                peer_id, max_rate, ..
            } => {
                // 服务端确认中继对建立。关闭对应 PeerConnection（释放资源），把 peer 移入 relayed_peers。
                relay_requested.remove(&peer_id);
                if let Some(pc) = peers.remove(&peer_id) {
                    pc.close();
                }
                pending.remove(&peer_id);
                negotiation_started_ms.remove(&peer_id);
                if relayed_peers.insert(peer_id) {
                    relay_rate_limiters.insert(peer_id, RelayRateLimiter::new(max_rate, now_ms()));
                    log::info!("[net/host] peer {peer_id} upgraded to relay");
                    out.push(RoomEvent::PeerRelayed { peer_id });
                    // 中继视为已连通，PeerCount 也包含 relayed
                    session.mark_dc_open();
                    out.push(RoomEvent::PeerCount(
                        peers.len() as u32 + relayed_peers.len() as u32,
                    ));
                }
            }
            SignalingEvent::RelayClosed { peer_id, reason } => {
                log::info!("[net/host] relay closed peer {peer_id}: {reason}");
                relayed_peers.remove(&peer_id);
                relay_rate_limiters.remove(&peer_id);
                relay_requested.remove(&peer_id);
                // 直连 PC 之前已关闭；若不在 relayed_peers 也不在 peers，就把 peer 当作离线
                out.push(RoomEvent::RemoteLeft { peer_id });
                out.push(RoomEvent::PeerCount(
                    peers.len() as u32 + relayed_peers.len() as u32,
                ));
            }
            SignalingEvent::RelayBinary {
                sender_peer_id,
                payload,
            } => {
                if !relayed_peers.contains(&sender_peer_id) {
                    // 未在中继名单内的 sender → 安全起见丢弃
                    log::warn!(
                        "[net/host] relay binary from non-relayed peer {sender_peer_id}, dropped"
                    );
                    continue;
                }
                match transport::decode_client_message(&payload) {
                    Ok(msg) => {
                        if let Some(handle) = peer_msg_handler.as_mut() {
                            (**handle)(sender_peer_id, msg);
                        }
                    }
                    Err(e) => log::warn!("[net/host] decode relay msg from {sender_peer_id}: {e}"),
                }
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
                        negotiation_started_ms.remove(&peer_id);
                        out.push(RoomEvent::PeerCount(
                            peers.len() as u32 + relayed_peers.len() as u32,
                        ));
                    } else if matches!(state, PeerState::Disconnected | PeerState::Failed) {
                        // 直连失败 → 不直接 RemoteLeft；尝试升级为中继。
                        if !relay_requested.contains(&peer_id) && !relayed_peers.contains(&peer_id)
                        {
                            log::info!("[net/host] peer {peer_id} state={state:?} → request_relay");
                            signaling.request_relay(peer_id);
                            relay_requested.insert(peer_id);
                            // 注意：不立即 remove peer 或 emit RemoteLeft —
                            // 等 RelayActive 把 peer 迁到 relayed_peers，或服务端拒绝时再清理。
                        }
                    }
                }
                PeerEvent::NegotiationError(msg) => {
                    log::warn!("[net/host] peer {peer_id} negotiation error: {msg}");
                }
                PeerEvent::BufferFreed => {
                    // reliable DC 缓冲区降到阈值以下；下帧 flush_server_outbox 会重试发送
                }
            }
        }
    }

    // 3) 协商超时：未连通且未请求中继的 peer，超过 NEGOTIATION_TIMEOUT_MS 就触发升级
    let now = now_ms();
    let timeout_ids: Vec<u32> = negotiation_started_ms
        .iter()
        .filter(|(pid, start)| {
            !relay_requested.contains(*pid)
                && !relayed_peers.contains(*pid)
                && peers
                    .get(*pid)
                    .is_some_and(|pc| pc.state() != PeerState::Connected)
                && now - **start > NEGOTIATION_TIMEOUT_MS
        })
        .map(|(pid, _)| *pid)
        .collect();
    for peer_id in timeout_ids {
        log::info!(
            "[net/host] peer {peer_id} negotiation timeout {NEGOTIATION_TIMEOUT_MS}ms → request_relay"
        );
        signaling.request_relay(peer_id);
        relay_requested.insert(peer_id);
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
    relayed_host: &mut Option<u32>,
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
            SignalingEvent::RelayActive { peer_id, .. } => {
                // 仅接受来自 host 的中继升级通知
                if Some(peer_id) != *host_peer_id {
                    log::warn!("[net/remote] relay_active for non-host peer {peer_id}, ignored");
                    continue;
                }
                if relayed_host.is_some() {
                    continue; // 幂等
                }
                // 关闭已建立的（或失败中的）PeerConnection，转入中继模式
                if let Some(pc) = host.take() {
                    pc.close();
                }
                *relayed_host = Some(peer_id);
                log::info!("[net/remote] upgraded to relay via host {peer_id}");
                // 视为已连通：flush outbox
                session.mark_dc_open();
                *session = RoomSession::Connected;
                out.push(RoomEvent::Connected);
                out.push(RoomEvent::PeerRelayed { peer_id });
                let drained: Vec<_> = outbox.drain(..).collect();
                for msg in drained {
                    match transport::encode_client_message(&msg) {
                        Ok(bytes) => signaling.send_relay_binary(peer_id, &bytes),
                        Err(e) => log::warn!("[net/remote] encode (relay flush): {e}"),
                    }
                }
            }
            SignalingEvent::RelayClosed { peer_id, reason } => {
                if Some(peer_id) != *host_peer_id {
                    continue;
                }
                log::info!("[net/remote] relay closed by server: {reason}");
                *relayed_host = None;
                *session = RoomSession::Disconnected {
                    reason: reason.clone(),
                };
                out.push(RoomEvent::Disconnected { reason });
            }
            SignalingEvent::RelayBinary {
                sender_peer_id,
                payload,
            } => {
                // 仅接受来自 host 且已升级为中继的二进制帧
                if Some(sender_peer_id) != *host_peer_id || relayed_host.is_none() {
                    log::warn!(
                        "[net/remote] unexpected relay binary from {sender_peer_id}, dropped"
                    );
                    continue;
                }
                match transport::decode_server_message(&payload) {
                    Ok(msg) => inbox.push_back(msg),
                    Err(e) => log::warn!("[net/remote] decode relay server msg: {e}"),
                }
            }
        }
    }

    // 2) Host PeerConnection 事件（仅在未升级到中继时）
    if relayed_host.is_some() {
        return;
    }
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
                        // 直连失败 → 不立刻断开，等 Host 端发起的 RelayActive。
                        // 若服务端最终拒绝升级（RelayClosed 也未来），UI 仍卡在 Negotiating；
                        // 现阶段保守处理：保留 PC（已 closed）等服务端通知，不抢先 emit Disconnected。
                        log::info!("[net/remote] host PC state={state:?}; awaiting relay decision");
                    }
                }
                PeerEvent::NegotiationError(msg) => {
                    log::warn!("[net/remote] negotiation error: {msg}");
                }
                PeerEvent::BufferFreed => {
                    // Remote 不发送大块数据，忽略
                }
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────
// 单元测试（Local 端点；WebRTC 部分需浏览器集成测试 v2 引入）
// ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
}
