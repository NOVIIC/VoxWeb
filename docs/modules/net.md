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

### 当前组成

- `NetEndpoint::Local` 使用 `futures::channel::mpsc` 双向通道，把单机路径也包装成消息驱动
- `NetEndpoint::Host` 持有本地 Local 通道、多个 Remote Peer、信令客户端和房间状态
- `NetEndpoint::Remote` 持有到 Host 的 PeerConnection、信令客户端和房间状态
- `signaling.rs` 管 WebSocket 信令；`peer.rs` 管 WebRTC PeerConnection；`room.rs` 管房间状态；`transport.rs` 管可靠/不可靠通道选择和序列化
- `relay.rs` 管 Host outbox 路由纯函数和 Worker 字节中继本地令牌桶限流

---

## 二、目录结构

```
crates/net/src/
├── lib.rs              NetEndpoint + 公开 API
├── signaling.rs        WebSocket 信令客户端
├── peer.rs             RtcPeerConnection 包装 + DataChannel 双通道
├── room.rs             房间会话状态机
├── relay.rs            outbox 路由计划 + 中继限流
└── transport.rs        通道选择策略 + 消息序列化辅助
```

---

## 三、`lib.rs` — 顶层抽象

### `NetEndpoint`

统一不同角色的网络端点，对 `client` 暴露同一套 send/recv API：

```rust
pub enum NetEndpoint {
    /// 单机模式：基于 futures mpsc 的内存通道
    Local {
        tx_client: mpsc::UnboundedSender<ClientMessage>,
        rx_server: mpsc::UnboundedReceiver<ServerMessage>,
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

/// Server 侧持有的 mpsc 端，与 NetEndpoint::Local 对偶。
pub struct ServerInbox {
    pub rx_client: mpsc::UnboundedReceiver<ClientMessage>,
    pub tx_server: mpsc::UnboundedSender<ServerMessage>,
}

pub type PeerId = u32;

impl NetEndpoint {
    /// 创建 Local 端点 + 对偶 ServerInbox。
    pub fn new_local_pair() -> (Self, ServerInbox);

    /// 创建房主，并连接信令服务
    pub async fn host(signaling_url: &str, room_id: &str) -> Result<Self, NetError>;

    /// 加入房间
    pub async fn join(signaling_url: &str, room_id: &str) -> Result<Self, NetError>;

    /// 发送一条客户端消息（Local 入 tx_client；Remote 序列化走 DataChannel）
    pub fn send_client_message(&self, msg: ClientMessage);

    /// 非阻塞拉取一条 ServerMessage
    pub fn try_recv_server_message(&mut self) -> Option<ServerMessage>;

    /// Host 广播 ServerMessage
    pub fn broadcast(&mut self, recipient: Recipient, msg: ServerMessage);

    /// 房间生命周期事件
    pub fn poll_events(&mut self) -> Vec<RoomEvent>;
}

impl ServerInbox {
    pub fn try_recv_client_message(&mut self) -> Option<ClientMessage>;
    pub fn send_server_message(&self, msg: ServerMessage);
}

pub enum NetError {
    SignalingUnreachable,
    PeerConnectionFailed,
    DataChannelClosed,
    InvalidRoomId,
    Timeout,
}
```

> **统一收发循环**：client 主循环每帧
> 1. 用 `server_inbox.try_recv_client_message()` 拉客户端消息，喂给 `server.handle_message(...)`；
> 2. 把 handle_message 返回的 `Vec<ServerMessage>` 通过 `server_inbox.send_server_message(msg)` 推回；
> 3. 在 client 端 `net.try_recv_server_message()` 消费并应用到 Game。

> **设计意图**：`client` 不需要 `match endpoint { Local => ..., Host => ..., Remote => ... }`。把所有差异封装在 `net` 内，`client` 只调 `send_client_message` / `try_recv_server_message` / `broadcast` / `poll_events`。

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
    // 数据中继 fallback（详见 docs/networking/signaling.md §九）
    RelayActive { peer_id: PeerId, max_msg_size: u32, max_rate: u32 },
    RelayClosed { peer_id: PeerId, reason: String },
    RelayBinary { sender_peer_id: PeerId, payload: Vec<u8> },
}

impl SignalingClient {
    pub async fn connect(url: &str, room: &str, role: Role) -> Result<Self, NetError>;
    pub fn send_offer(&self, to: PeerId, sdp: &str);
    pub fn send_answer(&self, to: PeerId, sdp: &str);
    pub fn send_ice(&self, to: PeerId, candidate: &str);
    pub fn poll(&self) -> Vec<SignalingEvent>;
    pub fn close(self);
    // 中继
    pub fn request_relay(&self, peer_id: PeerId);            // Host 主动升级
    pub fn send_relay_binary(&self, target: PeerId, payload: &[u8]);  // 已升级后发字节
}
```

`RelayActive.max_rate` 会被 Host 端记录为本地中继发送令牌桶的上限；`host_route_outbox()` 在真正调用 `send_relay_binary()` 前预扣 token，预算不足时把当前及后续 outbox 消息推迟到下一帧，避免高视距 FieldSnapshot 突发触发 Worker 的 `rate_limit`。

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
| `reliable` | `ordered: true, maxRetransmits: null, maxPacketLifeTime: null` | FieldRequest, FieldSnapshot, HostSettings, FieldDelta, FreeObjectProject, Chat, Hello/Welcome, ActionAck, Join/Leave |
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

**TURN 凭据下发**：可选增强中，信令服务 `Registered` 消息可携带短期 TURN 凭据（防泄漏），客户端动态注入到 `iceServers`。当前固定使用公共 STUN，TURN 槽位预留。

### IPv6 优先策略

国内运营商大量部署 CGNAT/对称 NAT，IPv4 srflx 打洞成功率显著低于 IPv6 host。浏览器默认对 IPv6 仅略微偏好（同类内 local-preference 高一点点），且 IPv4 host（注定连不通的内网地址）排序高于 IPv6 srflx，会浪费检查窗口。

`peer.rs` 在 `onicecandidate` 回调中通过纯函数 `bump_ipv6_priority` 拦截每条本地候选，把 IPv4 候选的 priority 字段减去 `IPV4_PRIORITY_PENALTY`（`0x4000_0000`，约 1.07B），饱和到 0。结果：

| 候选类型 | 默认 priority | 重写后 |
|---|---|---|
| IPv6 host | ~2.12 B | 不变 |
| IPv6 srflx | ~1.68 B | 不变 |
| IPv4 host | ~2.12 B | ~1.05 B |
| IPv4 srflx | ~1.68 B | ~0.61 B |

所有 IPv6 候选都排在所有 IPv4 候选之前，同族内部 host > srflx > relay 的相对顺序保留。双方都跑同一份代码 → 互相重写对方收到的 candidate priority → ICE pair 检查队列前段全部是 IPv6 pair，IPv4 仅作兜底（IPv6 全失败时浏览器自动降级到下一档）。

只重写出方向的 trickle candidate（即 `onicecandidate` 给出的）；SDP（`createOffer`/`createAnswer` 返回值）里可能内嵌的初始候选未处理。Chrome 默认 trickle 模式下初始 SDP 基本不含 candidate，所以单边重写够用；如果实测发现 IPv4 仍抢跑，再考虑在 `setLocalDescription` 前做 SDP munging。

非 IPv4 字面量（mDNS `.local` 主机名、IPv6 字面量）一律原样转发，不参与重写。

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
    SignalingConnect,
    AwaitRegistered,
    Negotiating(NegotiationProgress),
    Connected,
    Disconnected { reason: String },
}

pub struct NegotiationProgress {
    pub signaling_ok: bool,
    pub registered: bool,
    pub offer_exchanged: bool,
    pub answer_exchanged: bool,
    pub data_channel_opened: bool,
}
```

状态转换：

```
Idle ──connect()──▶ SignalingConnect ──WS open──▶ AwaitRegistered ──Registered──▶ Negotiating
                                                                                       │
                                      ICE complete + DataChannel open                   │
                                      ┌────────────────────────────────────────────────┘
                                      ▼
                                   Connected
                                      │
                                      │ DC close / signaling close / 显式 leave
                                      ▼
                                   Disconnected
```

### 加载步骤

为支持加载界面列表式显示，新增两个类型和 `loading_steps()` 方法：

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepStatus { Pending, InProgress, Done }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadingStep {
    pub label: String,
    pub status: StepStatus,
}
```

`RoomSession::loading_steps()` 返回网络协商的 5 个步骤及各自状态（信令 → 注册 → offer → answer → DC），按当前 `RoomSession` 状态推导每步是 Pending / InProgress / Done。

```rust
impl RoomSession {
    /// 返回网络连接相关的加载步骤列表。
    /// 区块预载步骤由客户端自行追加。
    pub fn loading_steps(&self) -> Vec<LoadingStep>;
}
```

旧 `progress_label()` 单行文本方法已移除。Local 模式的 `RoomSession` 始终为 `Idle`，`loading_steps()` 返回空列表——客户端只追加区块预载那一步。

---

## 七、`transport.rs` — 通道选择与路由

Local 通道不区分 reliable/unreliable，单 mpsc 即可；Host / Remote 走下表的 DataChannel 选择规则。

### 通道选择规则

| 消息 | 通道 | 原因 |
|---|---|---|
| `ClientMessage::Hello` | reliable | 不能丢，需 ack |
| `ClientMessage::PlayerInput` | unreliable | 60Hz 高频，丢一两帧不影响 |
| `ClientMessage::FieldRequest` | reliable | Remote 缺失区块必须送达 |
| `ClientMessage::Break/Place` | reliable | 一次性操作必须送达 |
| `ClientMessage::Chat` | reliable | 同上 |
| `ClientMessage::Ping` | unreliable | 时延探测，丢失忽略 |
| `ServerMessage::Welcome` | reliable | 必须送达 |
| `ServerMessage::HostSettings` | reliable | Host 视距上限变化，必须送达 |
| `ServerMessage::FieldSnapshot` | reliable | 不能丢、按序 |
| `ServerMessage::FieldDelta` | reliable | 状态不可丢 |
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

**重连**：当前不实现自动重连（避免协议状态复杂）；用户手动从大厅再次加入。

---

## 九、对外公开 API 总结

`client` 看到的 net crate 接口仅限：

```rust
pub use lib::{NetEndpoint, NetError, InboundEnvelope, NetMessage, PeerId};
pub use room::{LoadingStep, RoomSession, StepStatus};
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
- 当前暂不上 CI（可选增强）

---

## 十一、不在范围

- 端到端加密（DataChannel 默认 DTLS 已加密；当前不在应用层再加）
- 多 Host 故障转移
- 自动重连
- 拥塞控制 / 限流（依赖浏览器 SCTP 实现）
- 消息优先级队列
- Web Worker 卸载网络处理（线程模型决策为单线程）
