# Phase 4 · P2P 通道 — 完成报告

> 完成日期：2026-05-13
> 设计文档：[`docs/modules/net.md`](docs/modules/net.md) · [`docs/networking/signaling.md`](docs/networking/signaling.md) · [`docs/networking/protocol.md`](docs/networking/protocol.md)

---

## 目标回顾

让两个浏览器 Tab 通过 WebRTC DataChannel 互发字节，验证：
1. Cloudflare Workers 信令服务（独立部署）；
2. Rust 端信令客户端 + WebRTC PeerConnection + 双 DataChannel（reliable / unreliable）；
3. NetEndpoint::Host / ::Remote 完整集成；
4. 大厅 Create/Join Room UI + Connecting 视图 + RTT HUD。

---

## 实装清单

### 1. 信令服务（TypeScript / Cloudflare Workers）

- [`signaling/src/worker.ts`](signaling/src/worker.ts)：路由 `/room/:id` → DO，`/health` 健康检查，roomId 校验 `[a-z0-9_-]{4,12}` + `toLowerCase()`。
- [`signaling/src/room.ts`](signaling/src/room.ts)：Durable Object 完整实现 register/role/路由/leave 协议；一房一 host；host 离开 → 房间销毁；最多 16 个 peer。
- TypeScript 类型检查：`npm run typecheck` 通过。
- 协议帧 100% 对齐 [`docs/networking/signaling.md`](docs/networking/signaling.md) §三。

### 2. Rust 信令客户端

- [`crates/net/src/signaling.rs`](crates/net/src/signaling.rs)：
  - `SignalingClient::connect(url, room, role, display_name)` 立即返回；事件 push 到 `Rc<RefCell<VecDeque>>` 由主循环 `poll()` drain；
  - `SignalingEvent`：Open / Registered / PeerJoined / PeerLeft / Offer / Answer / Ice / RoomClosed / ServerError / SocketError / Closed；
  - `send_offer / send_answer / send_ice / send_leave` 同步写 WebSocket；
  - JSON 协议解析；4 个解析单元测试。

### 3. PeerConnection 包装

- [`crates/net/src/peer.rs`](crates/net/src/peer.rs)：
  - `create_offerer` (Host)：主动 `createDataChannel("reliable" / "unreliable")` + `start_offer()` 异步触发 `createOffer + setLocalDescription`；
  - `create_answerer` (Remote)：仅挂 `ondatachannel`；`accept_offer(sdp)` 异步 `setRemoteDescription → createAnswer → setLocalDescription`；
  - 异步 ops（`spawn_local`）完成后通过 `PeerEvent::OfferReady / AnswerReady / RemoteDescApplied / IceApplied / NegotiationError` 投递回主线程；
  - `onicecandidate` 收集本地 candidate 序列化 JSON → `PeerEvent::LocalIce`，主循环转发到信令；
  - `oniceconnectionstatechange` + DC `onopen / onclose` 合流维护 `PeerState`，状态切到 Connected 时 push `StateChanged(Connected)`；
  - DataChannel 配置：`reliable` ordered；`unreliable` `ordered:false, maxRetransmits:0`；
  - DC `binaryType = RtcDataChannelType::Arraybuffer`；`send_with_u8_array` 同步写。

### 4. 房间状态机

- [`crates/net/src/room.rs`](crates/net/src/room.rs)：
  - `RoomSession`：Idle / SignalingConnect / AwaitRegistered / Negotiating(progress) / Connected / Disconnected{reason}；
  - `NegotiationProgress`：5 个子步骤（signaling_ok / registered / offer_exchanged / answer_exchanged / data_channel_opened）；
  - `progress_label()` 给 Connecting UI 展示。

### 5. Transport

- [`crates/net/src/transport.rs`](crates/net/src/transport.rs)：
  - `ChannelKind` (Reliable / Unreliable) + `channel_for_client_message` / `channel_for_server_message`；
  - `encode/decode_client_message` / `encode/decode_server_message` 包装 bincode。

### 6. NetEndpoint 集成

- [`crates/net/src/lib.rs`](crates/net/src/lib.rs)：
  - `NetEndpoint::Host`：本地 mpsc + 信令 + `HashMap<PeerId, PeerConnection>` + RoomSession；
  - `NetEndpoint::Remote`：单 PeerConnection + 待 open 时的 outbox 暂存 + 反序列化后的 ServerMessage inbox；
  - `poll(server_handle)` 推进所有事件：信令 ↔ PC，PC 收 ClientMessage 调 `server.handle_message`，回包按消息类型走 reliable / unreliable DC；
  - `send_client_message`：Local/Host 走 mpsc；Remote 走 DC（未 open 时暂存 outbox）；
  - `try_recv_server_message`：Local/Host 拉 mpsc；Remote 拉 inbox。

### 7. Server: Ping handler

- [`crates/server/src/lib.rs`](crates/server/src/lib.rs)：
  - 新增 `current_time_ms: u64` 字段 + `set_clock(ms)` 方法（Host 主循环每帧调用）；
  - `ClientMessage::Ping { client_time_ms }` → `vec![ServerMessage::Pong { client_time_ms, server_time_ms: self.current_time_ms }]`。

### 8. Lobby UI 扩展

- [`crates/client/src/ui/lobby.rs`](crates/client/src/ui/lobby.rs)：
  - 新动作 `LobbyAction::CreateRoom { room_id, seed }` / `JoinRoom { room_id }` / `ConnectingAction::Cancel`；
  - Room ID 输入框 + Create / Join 按钮；
  - `validate_room_id` 校验 4-12 字符 `[a-z0-9_-]`；
  - `generate_room_id()` 用 `getrandom` 生成 6 位 `[a-z0-9]`；空 room_id 创建时自动回填到输入框并显示给用户分享；
  - `draw_connecting(mode, room_id, progress_label, error)` 居中显示进度 + Cancel 按钮。

### 9. Client 主循环

- [`crates/client/src/app.rs`](crates/client/src/app.rs)：
  - `GameMode { Local, Host, Remote }`；
  - `Game::new_host` / `new_remote` 构造器；
  - RTT 相关字段：`rtt_ms`、`last_ping_sent_ms`、`pending_pings: HashMap<client_time_ms, send_at_ms>`、`room_id`。
- [`crates/client/src/lib.rs`](crates/client/src/lib.rs)：
  - `AppState::Connecting` 渲染走 `render_connecting_frame`；
  - 每帧 `poll_net(app)`：注入 server.handle_message 闭包给 Host；Pong 由 `apply_server_message` 计算 RTT；
  - 每 5s 发 `ClientMessage::Ping { client_time_ms: now }` 入 `pending_pings`；
  - HUD 增加一行 `NET {LOCAL/HOST/REMOTE}  RTT {xx.x ms / --}  ROOM {id}`；
  - 信令 URL：`<meta name="signaling-url">`（默认 `wss://signal.voxweb.example.com`；本地改 `ws://localhost:8787`）。

### 10. Cargo.toml

- 根 [`Cargo.toml`](Cargo.toml)：新增 web-sys feature `RtcDataChannelType / RtcDataChannelEvent / RtcDataChannelState / RtcPeerConnectionIceEvent / RtcPeerConnectionState / RtcIceConnectionState / RtcIceGatheringState / CloseEvent / HtmlMetaElement`；新增 `serde_json` workspace 依赖。
- [`crates/net/Cargo.toml`](crates/net/Cargo.toml)：加 `serde_json`（运行时）+ `glam` （dev-dep，测试用）。

---

## 验证结果

### 编译
- `cargo check --workspace --target wasm32-unknown-unknown`：✅
- `cargo fmt --check`：✅
- `cargo test --workspace --lib`：✅
  - voxweb-core 51 / voxweb-render 7 / voxweb-server 19 / voxweb-net 12 / voxweb-client 17 — 共 106 测试通过
- `signaling && npm run typecheck`：✅

### 单元测试覆盖
- `transport::tests`：通道选择 + 编解码 roundtrip（3 个）
- `signaling::tests`：协议帧解析（5 个）
- `room::tests`：状态机转换 + label（2 个）
- `tests`：Local mpsc roundtrip + 空 try_recv（2 个）
- `server::ping_returns_pong_with_server_clock`：Server 端 Ping → Pong 路径

### 端到端验证（待手测）
- `cd signaling && npm run dev`（监听 :8787）；
- `trunk serve`（:8080）；
- 浏览器 A `Create Room` → 自动生成 6 位房间号回填输入框；
- 浏览器 B `Join Room` 输入相同房间号；
- 双方 1-2 秒内从 Connecting 切到 InGame；
- HUD 显示 `NET HOST/REMOTE  RTT xx.x ms  ROOM abcdef`；
- chrome://webrtc-internals 看到两条 DC `state: open`；
- 关掉 wrangler dev，已连接的两 Tab 仍能持续刷新 RTT；
- A 关闭 Tab → B 收到 RoomClosed/PeerLeft → 回 Lobby。

---

## 设计取舍

| 决策 | 选择 | 理由 |
|---|---|---|
| Remote 是否运行本地 Server | **是** | Phase 4 让 Remote 看到本地（独立）世界；diff 最小；Phase 5 改为 Host 推送 |
| Ping/Pong 路径 | **走 server.handle_message** | 一次到位，避免 Phase 5 回头改 Host 的消息处理 |
| 信令 URL 来源 | **仅读 `meta[name="signaling-url"]`** | 配置集中；本地开发由开发者改 meta 内容 |
| 空 room_id 创建 | **自动生成 6 位 [a-z0-9]** | 用户便利；自动回填到输入框 + UI 上显示生成的房间号 |
| 异步 WebRTC 操作 | **`spawn_local` + Rc<RefCell> 事件队列** | 主循环保持同步；不持有 .await 跨多 RefCell |
| Phase 4 entity_id 分配 | **`1000 + peer_id` 临时占位** | Phase 5 由 `server.add_player` 正式分配 |

---

## 已知限制 / 留给 Phase 5

- Remote 端世界与 Host 不同步（两 Tab 各自挖放只影响自己）；
- 没有 ChunkSnapshot 全量同步；
- 没有 PlayerTick 60Hz 广播 / 远端玩家身体渲染；
- 没有 BlockUpdate 跨网络广播；
- 没有 PeerJoined / PeerLeft 在 HUD 反映；
- Host 同时连接 ≥ 2 个 Remote 时未做过压力测试；
- 信令断开后自动重连：未实装（用户手动退回大厅重新加入）。

---

## 文件改动概要

| 文件 | 性质 |
|---|---|
| [`signaling/src/worker.ts`](signaling/src/worker.ts) | 改写：完整路由 + 健康检查 + roomId 校验 |
| [`signaling/src/room.ts`](signaling/src/room.ts) | 改写：register/role/路由/leave 完整协议 |
| [`crates/net/src/signaling.rs`](crates/net/src/signaling.rs) | 改写：SignalingClient + 事件队列 |
| [`crates/net/src/peer.rs`](crates/net/src/peer.rs) | 改写：PeerConnection + 双 DC + async 协商 |
| [`crates/net/src/room.rs`](crates/net/src/room.rs) | 改写：RoomSession 状态枚举 |
| [`crates/net/src/transport.rs`](crates/net/src/transport.rs) | 改写：ChannelKind + encode/decode 辅助 |
| [`crates/net/src/lib.rs`](crates/net/src/lib.rs) | 改写：NetEndpoint::Host/Remote + poll() |
| [`crates/net/Cargo.toml`](crates/net/Cargo.toml) | 加 serde_json dependency + glam dev |
| [`Cargo.toml`](Cargo.toml) | web-sys features + serde_json |
| [`crates/server/src/lib.rs`](crates/server/src/lib.rs) | 加 current_time_ms + set_clock + Ping 分支 |
| [`crates/client/src/app.rs`](crates/client/src/app.rs) | GameMode + new_host/new_remote + RTT 字段 |
| [`crates/client/src/ui/lobby.rs`](crates/client/src/ui/lobby.rs) | Create/Join 控件 + Connecting 视图 |
| [`crates/client/src/lib.rs`](crates/client/src/lib.rs) | Connecting 帧 + poll_net + 5s Ping + RTT HUD |

---

## 下一阶段

进入 Phase 5：主机权威同步（[`docs/roadmap.md`](docs/roadmap.md) §Phase 5）。要做的事：
- ChunkSnapshot 分片传输 + 接收端组装；
- 真正的 server.add_player（替换 Phase 4 临时 entity_id）；
- PlayerInput / PlayerTick 60Hz 收发；
- 客户端预测 + 协调；
- BlockUpdate 跨网络广播；
- 远端玩家身体渲染；
- 基础 OPFS 持久化最小可用版。
