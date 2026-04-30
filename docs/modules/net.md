# `net` 模块设计

> **何时阅读**：改 WebRTC 连接流程；改信令客户端；改通道分配；调 NAT 穿透
> **关联文档**：[`README.md`](../../README.md) · [`architecture.md`](../architecture.md) · [`networking/protocol.md`](../networking/protocol.md) · [`networking/signaling.md`](../networking/signaling.md) · [`networking/prediction.md`](../networking/prediction.md)

---

## 一、职责

`net` crate 把 P2P 网络细节封装成统一接口供 `client` 调用：
- 与 Cloudflare Workers 信令服务握手
- 管理 WebRTC `RtcPeerConnection`（一个 Host 对多个 Remote）
- 管理两条 `DataChannel`（reliable + unreliable）
- 房间会话状态机
- 提供统一的 `NetEndpoint` 抽象（隐藏 Local/Host/RemoteClient 三种角色差异）

---

## 二、目录结构

```
crates/net/src/
├── lib.rs              NetEndpoint + 公开 API
├── signaling.rs        WebSocket 信令客户端
├── peer.rs             RtcPeerConnection 包装 + DataChannel 双通道
├── room.rs             房间会话状态机
└── transport.rs        通道选择策略 + 消息序列化辅助
```

---

## 三、`lib.rs` — 顶层抽象

### `NetEndpoint`

统一不同角色的网络端点，对 `client` 暴露同一套 send/recv API：

```rust
pub enum NetEndpoint {
    /// 单机模式：直接持有 server 的内存通道
    Local {
        to_server: VecDeque<ClientMessage>,    // client→server
        to_client: VecDeque<ServerMessage>,    // server→client
    },
    /// 房主：自身 Server + 多个 Remote Peer
    Host {
        local: Box<NetEndpoint>,               // 自身的 Local 通道（仍走内存）
        peers: HashMap<PeerId, PeerConnection>,
        signaling: SignalingClient,
        room: RoomSession,
    },
    /// 远程客户端：单条到 Host 的 PeerConnection
    Remote {
        host: PeerConnection,
        signaling: SignalingClient,
        room: RoomSession,
    },
}

pub type PeerId = u32;

impl NetEndpoint {
    /// 创建单机端点
    pub fn local() -> Self;

    /// 创建房主，并连接信令服务
    pub async fn host(signaling_url: &str, room_id: &str) -> Result<Self, NetError>;

    /// 加入房间
    pub async fn join(signaling_url: &str, room_id: &str) -> Result<Self, NetError>;

    /// 发送一条客户端消息（Local→Server / Remote→Host）
    pub fn send_to_server(&mut self, msg: ClientMessage);

    /// 发送一条服务端消息（Host 角色用，会自动选择通道与接收方）
    pub fn broadcast(&mut self, recipient: Recipient, msg: ServerMessage);

    /// 拉取本端入站消息
    pub fn poll_inbound(&mut self) -> Vec<InboundEnvelope>;

    /// 房间生命周期事件（连接建立/断开/peer 加入/离开）
    pub fn poll_events(&mut self) -> Vec<RoomEvent>;
}

pub struct InboundEnvelope {
    pub sender: PeerId,            // Host 视角下来源 peer；Remote 视角下固定为 0
    pub message: NetMessage,
}

pub enum NetMessage {
    FromClient(ClientMessage),     // Host 收到 Remote 输入
    FromServer(ServerMessage),     // Remote 收到 Host 状态
}

pub enum NetError {
    SignalingUnreachable,
    PeerConnectionFailed,
    DataChannelClosed,
    InvalidRoomId,
    Timeout,
}
```

> **设计意图**：`client` 不需要 `match endpoint { Local => ..., Host => ..., Remote => ... }`。把所有差异封装在 `net` 内，`client` 只调 `send_to_server` / `broadcast` / `poll_inbound` / `poll_events`。

---

## 四、`signaling.rs` — WebSocket 信令客户端

### 协议
完整协议见 [`networking/signaling.md`](../networking/signaling.md)。简要：

| 客户端发 | 服务端发 |
|---|---|
| `Register { room: String, role: "host" \| "join" }` | `Registered { peer_id: u32 }` |
| `Offer { to: u32, sdp: String }` | `Offer { from: u32, sdp: String }` |
| `Answer { to: u32, sdp: String }` | `Answer { from: u32, sdp: String }` |
| `Ice { to: u32, candidate: String }` | `Ice { from: u32, candidate: String }` |
| `Leave` | `PeerJoined { peer_id: u32 }` / `PeerLeft { peer_id: u32 }` / `RoomClosed` |

消息格式：JSON（信令通量很小，简单胜过紧凑）。

### 实现

```rust
pub struct SignalingClient {
    socket: web_sys::WebSocket,
    peer_id: Option<PeerId>,
    inbox: Rc<RefCell<VecDeque<SignalingEvent>>>,
}

pub enum SignalingEvent {
    Registered(PeerId),
    PeerJoined(PeerId),
    PeerLeft(PeerId),
    OfferReceived { from: PeerId, sdp: String },
    AnswerReceived { from: PeerId, sdp: String },
    IceReceived { from: PeerId, candidate: String },
    RoomClosed,
    Error(String),
}

impl SignalingClient {
    pub async fn connect(url: &str, room: &str, role: Role) -> Result<Self, NetError>;
    pub fn send_offer(&self, to: PeerId, sdp: &str);
    pub fn send_answer(&self, to: PeerId, sdp: &str);
    pub fn send_ice(&self, to: PeerId, candidate: &str);
    pub fn poll(&self) -> Vec<SignalingEvent>;
    pub fn close(self);
}
```

**注意**：`web_sys::WebSocket` 的事件回调通过 `Closure<dyn FnMut(...)>` 注册，回调内只能 `inbox.borrow_mut().push_back(event)`，不能直接做长时间工作（保持回调短）。

---

## 五、`peer.rs` — RtcPeerConnection 包装

### 设计

```rust
pub struct PeerConnection {
    pub peer_id: PeerId,
    rtc: web_sys::RtcPeerConnection,
    pub state: PeerState,
    pub reliable: DataChannel,
    pub unreliable: DataChannel,
    pub inbox: Rc<RefCell<VecDeque<DataChannelMessage>>>,
}

pub enum PeerState {
    New,
    Connecting,
    Connected,
    Disconnected,
    Failed,
}

pub struct DataChannel {
    inner: web_sys::RtcDataChannel,
    pub label: &'static str,    // "reliable" / "unreliable"
    pub open: bool,
}

pub struct DataChannelMessage {
    pub channel: ChannelKind,
    pub bytes: Vec<u8>,
}

pub enum ChannelKind { Reliable, Unreliable }
```

### 通道配置

| 通道 | 配置 | 用途 |
|---|---|---|
| `reliable` | `ordered: true, maxRetransmits: null, maxPacketLifeTime: null` | ChunkSnapshot, BlockUpdate, Chat, Hello/Welcome, ActionAck, Join/Leave |
| `unreliable` | `ordered: false, maxRetransmits: 0, maxPacketLifeTime: null` | PlayerInput, PlayerTick, Ping/Pong |

`maxRetransmits: 0` 实现"发出去就完事"，丢失不重传，适合频繁的位置广播。

### Offer/Answer 流程

**Host 侧**（每个新 Remote 加入）：
1. 收到 `SignalingEvent::PeerJoined(remote_id)`
2. 创建 `RtcPeerConnection` + 两条 `DataChannel`（Host 主动 `createDataChannel`）
3. `createOffer()` → `setLocalDescription` → 发 `Offer { to: remote_id }`
4. 等 `AnswerReceived` → `setRemoteDescription`
5. 在 `onicecandidate` 中通过信令发送本地 candidate

**Remote 侧**：
1. `connect` 后 `Register { role: "join" }` → `Registered`
2. 等 `OfferReceived { from: host_id }`
3. 创建 `RtcPeerConnection`，**等待** Host 创建的 DataChannel 通过 `ondatachannel` 到达
4. `setRemoteDescription(offer)` → `createAnswer()` → `setLocalDescription` → 发 `Answer`
5. 双方互发 ICE candidate，直到连接 `connected`

### ICE 配置

```rust
let mut config = web_sys::RtcConfiguration::new();
let ice_servers = js_sys::Array::new();

// Google 公共 STUN
let stun = js_sys::Object::new();
js_sys::Reflect::set(&stun, &"urls".into(), &"stun:stun.l.google.com:19302".into()).unwrap();
ice_servers.push(&stun);

// TURN（v2，由信令服务下发）
// ice_servers.push(&turn_config);

config.ice_servers(&ice_servers);
let rtc = web_sys::RtcPeerConnection::new_with_configuration(&config)?;
```

**TURN 凭据下发**：v2 阶段，信令服务 `Registered` 消息中携带短期 TURN 凭据（防泄漏），客户端动态注入到 `iceServers`。本期固定使用公共 STUN，TURN 槽位预留。

### `send` / `recv`

```rust
impl PeerConnection {
    pub fn send(&self, channel: ChannelKind, bytes: &[u8]) -> Result<(), NetError> {
        let dc = match channel {
            ChannelKind::Reliable => &self.reliable,
            ChannelKind::Unreliable => &self.unreliable,
        };
        if !dc.open { return Err(NetError::DataChannelClosed); }
        dc.inner.send_with_u8_array(bytes).map_err(|_| NetError::DataChannelClosed)?;
        Ok(())
    }

    pub fn drain_inbox(&self) -> Vec<DataChannelMessage> {
        std::mem::take(&mut *self.inbox.borrow_mut()).into_iter().collect()
    }
}
```

---

## 六、`room.rs` — 房间会话状态机

```rust
pub enum RoomSession {
    Idle,
    JoiningSignaling,
    Negotiating { progress: NegotiationProgress },
    Connected,
    Disconnected { reason: String },
}

pub struct NegotiationProgress {
    pub signaling_ok: bool,
    pub offer_sent: bool,
    pub answer_received: bool,
    pub ice_complete: bool,
}
```

状态转换：

```
Idle ──connect()──▶ JoiningSignaling ──Registered──▶ Negotiating
                                                         │
   ┌───────────────────────── ICE complete + DataChannel open ──┐
   ▼                                                            ▼
Connected ◀────────────────────────────────────  Negotiating
   │
   │ DataChannel close / signaling close / 显式 leave
   ▼
Disconnected
```

---

## 七、`transport.rs` — 通道选择与路由

### 通道选择规则

| 消息 | 通道 | 原因 |
|---|---|---|
| `ClientMessage::Hello` | reliable | 不能丢，需 ack |
| `ClientMessage::PlayerInput` | unreliable | 60Hz 高频，丢一两帧不影响 |
| `ClientMessage::Break/Place` | reliable | 一次性操作必须送达 |
| `ClientMessage::Chat` | reliable | 同上 |
| `ClientMessage::Ping` | unreliable | 时延探测，丢失忽略 |
| `ServerMessage::Welcome` | reliable | 必须送达 |
| `ServerMessage::ChunkSnapshot` | reliable | 不能丢、按序 |
| `ServerMessage::BlockUpdate` | reliable | 状态不可丢 |
| `ServerMessage::ActionAck` | reliable | 必须送达 |
| `ServerMessage::PlayerTick` | unreliable | 高频广播，最新即可 |
| `ServerMessage::PeerJoined/Left` | reliable | 状态变化必须送达 |
| `ServerMessage::Chat` | reliable | 同上 |
| `ServerMessage::Pong` | unreliable | 同 Ping |

### 路由

`NetEndpoint::send_to_server` 内部：
- Local：push 到 `to_server` VecDeque
- Remote：序列化 → `host.send(channel, bytes)`

`NetEndpoint::broadcast(Recipient, ServerMessage)`：
- Local：仅自己一条 to_client（Recipient 当作单元素）
- Host：
  - 把消息发给本地 client（自己的 NetEndpoint::Local 子端点）
  - 遍历 `peers`，按 Recipient 过滤后逐个 send

---

## 八、断线与重连策略

| 触发 | 行为 |
|---|---|
| 信令 WebSocket 断开 | 已建立的 PeerConnection 不受影响（已脱离信令）；新 peer 无法加入 |
| Remote 的 Host PeerConnection 断开 | client 弹出"主机断开"提示，回到大厅 |
| Host 的某个 Remote PeerConnection 断开 | Host 调 `server.remove_player(entity)`，广播 `PeerLeft` |
| `RoomClosed`（信令通知） | 双方都断开 |

**重连**：本期不实现自动重连（避免协议状态复杂）；用户手动从大厅再次加入。

---

## 九、对外公开 API 总结

`client` 看到的 net crate 接口仅限：

```rust
pub use lib::{NetEndpoint, NetError, InboundEnvelope, NetMessage, PeerId};
pub use room::RoomSession;
pub use transport::{ChannelKind};

// 不公开：SignalingClient / PeerConnection / DataChannel 等内部细节
```

---

## 十、单元测试与集成测试

### 单元测试（原生 target）
- `transport::pick_channel(message_kind) -> ChannelKind` 表驱动测试
- `room` 状态机转换合法性

### 集成测试
- WebRTC 必须在浏览器跑：用 Playwright 起两个 headless 浏览器互联，验证 Hello/Welcome 闭环
- 本期暂不上 CI（v2 加上）

---

## 十一、不在范围

- 端到端加密（DataChannel 默认 DTLS 已加密；本期不在应用层再加）
- 多 Host 故障转移
- 自动重连
- 拥塞控制 / 限流（依赖浏览器 SCTP 实现）
- 消息优先级队列
- Web Worker 卸载网络处理（线程模型决策为单线程）
