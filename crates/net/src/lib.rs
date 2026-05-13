//! VoxWeb P2P 网络层。
//!
//! 三种 NetEndpoint：
//! - `Local`：单机模式。client ↔ server 通过 futures mpsc 双向通道，无网络。
//! - `Host`：房主。在本地继续以 mpsc 跑自己的 client ↔ server；同时维护多个
//!   [`PeerConnection`] 接受 Remote。Phase 4 仅做 Ping/Pong 验证；Phase 5 起接入完整广播。
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

use voxweb_core::protocol::{ClientMessage, RoomEvent, ServerMessage};

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

    /// Host 端向所有已连接 Remote 广播一条 ServerMessage。
    /// Phase 4 只在 Ping 转发路径用到（server.handle_message 返回 Pong）。
    pub fn broadcast_to_remotes(&mut self, msg: &ServerMessage) {
        let NetEndpoint::Host { peers, .. } = self else {
            return;
        };
        let channel = transport::channel_for_server_message(msg);
        let bytes = match transport::encode_server_message(msg) {
            Ok(b) => b,
            Err(e) => {
                log::warn!("[net] encode server message: {e}");
                return;
            }
        };
        for pc in peers.values() {
            if pc.is_open(channel) {
                if let Err(e) = pc.send(channel, &bytes) {
                    log::warn!("[net] broadcast send to peer {} failed: {e:?}", pc.peer_id);
                }
            }
        }
    }

    /// 每帧推进网络状态机。
    ///
    /// `server_handle`：Host 模式下，从某个 peer 收到 ClientMessage 时调它处理并广播回 Pong 等。
    /// 它接受 `(peer_entity_id, ClientMessage)` 返回 `Vec<ServerMessage>`。
    /// Local 模式忽略这个参数。
    ///
    /// 返回房间生命周期事件，供 UI 推进 AppState。
    pub fn poll(
        &mut self,
        mut server_handle: Option<&mut dyn FnMut(u32, ClientMessage) -> Vec<ServerMessage>>,
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
                    &mut server_handle,
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
    server_handle: &mut Option<&mut dyn FnMut(u32, ClientMessage) -> Vec<ServerMessage>>,
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
    // 收集要广播的 ServerMessage（针对每个 peer 处理 Ping → 通过该 peer 的 DC 回 Pong）
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
                            if let Some(handle) = server_handle.as_mut() {
                                // 临时 entity_id：1000 + peer_id（Phase 5 由 server.add_player 分配）
                                let entity_id = 1000u32 + peer_id;
                                let replies = (**handle)(entity_id, msg);
                                // 把回复发回 *该* peer（Phase 5 才区分 broadcast vs 单播）
                                if let Some(pc) = peers.get(&peer_id) {
                                    for reply in replies {
                                        let ch = transport::channel_for_server_message(&reply);
                                        if let Ok(b) = transport::encode_server_message(&reply) {
                                            let _ = pc.send(ch, &b);
                                        }
                                    }
                                }
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
// 单元测试（仅 Local，WebRTC 部分需浏览器集成测试 v2 引入）
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
