/// <reference types="@cloudflare/workers-types" />

// Durable Object: 每个房间一个实例。
// 维护成员列表（peer_id -> WebSocket），并按 docs/networking/signaling.md §三 协议转发 SDP/ICE。
// 设计要点：
//   - 第一条消息必须是 register{role}，否则 close
//   - 一房一 host；host 离开 → 整个房间销毁（其它 peer 收 room_closed）
//   - offer/answer/ice 按 to 字段路由；目标不存在静默丢弃
//   - 上限 16 个 peer，超出拒绝
//
// 数据中继 fallback（详见 docs/networking/signaling.md §X）：
//   - 当 Host 与某 Remote ICE 协商失败时，Host 发 relay_request 升级该对为中继模式；
//   - 升级后，该对的 bincode 字节流通过同一条信令 WS 的二进制帧转发：
//       Client→DO binary：[target_peer_id: u32 LE][payload...]
//       DO→Client binary：[sender_peer_id: u32 LE][payload...]
//   - 每 peer 200 msg/s、单 payload ≤ 64KB；违规则关闭该中继对。

interface PeerInfo {
  ws: WebSocket;
  role: "host" | "join";
  displayName?: string;
}

interface IceServer {
  urls: string[];
  username?: string;
  credential?: string;
}

// 中继限流状态（令牌桶）。容量 200，每 50ms 补 10。
interface RateState {
  tokens: number;
  lastRefillMs: number;
}

// Workers 运行时全局 WebSocket 暴露的常量（@cloudflare/workers-types 已声明 READY_STATE_OPEN 等）。
const MAX_PEERS = 16;
const RELAY_MAX_PAYLOAD = 64 * 1024; // 64KB（不含 4B header）
const RELAY_RATE_CAPACITY = 200; // 桶容量（消息条数）
const RELAY_RATE_REFILL_PER_MS = 200 / 1000; // 200/s

export class Room {
  private peers = new Map<number, PeerInfo>();
  private hostId: number | null = null;
  private nextPeerId = 1;
  // 已建立中继的双向 peer 集合。relayPairs.get(A) 含 B ⇔ relayPairs.get(B) 含 A。
  private relayPairs = new Map<number, Set<number>>();
  // 每 peer 一个令牌桶，控制其发出的中继帧速率。
  private rateState = new Map<number, RateState>();

  constructor(_state: DurableObjectState) {}

  async fetch(request: Request): Promise<Response> {
    if (request.headers.get("Upgrade") !== "websocket") {
      return new Response("expected websocket", { status: 400 });
    }
    const pair = new WebSocketPair();
    const [client, server] = Object.values(pair);

    server.accept();
    this.attach(server);

    return new Response(null, { status: 101, webSocket: client });
  }

  private attach(ws: WebSocket): void {
    // 每个 WS 连接的会话状态：未注册时 myPeerId === null。
    let myPeerId: number | null = null;
    let registered = false;

    ws.addEventListener("message", (event: MessageEvent) => {
      // 二进制帧：中继数据载荷。未注册一律拒绝。
      if (event.data instanceof ArrayBuffer) {
        if (!registered || myPeerId == null) {
          this.sendError(ws, "must_register_first");
          ws.close();
          return;
        }
        this.handleBinary(myPeerId, event.data);
        return;
      }

      // 文本帧：JSON 信令消息
      let raw: unknown;
      try {
        if (typeof event.data !== "string") {
          this.sendError(ws, "invalid_frame");
          ws.close();
          return;
        }
        raw = JSON.parse(event.data);
      } catch {
        this.sendError(ws, "invalid_json");
        ws.close();
        return;
      }
      const msg = raw as Record<string, unknown> | null;
      if (!msg || typeof msg.kind !== "string") {
        this.sendError(ws, "invalid_message");
        ws.close();
        return;
      }

      // 第一条必须是 register
      if (!registered) {
        if (msg.kind !== "register") {
          this.sendError(ws, "must_register_first");
          ws.close();
          return;
        }
        const assigned = this.handleRegister(ws, msg);
        if (assigned == null) {
          ws.close();
          return;
        }
        myPeerId = assigned;
        registered = true;
        return;
      }

      // 已注册：根据 kind 分发
      switch (msg.kind) {
        case "leave":
          this.handleLeave(myPeerId!);
          ws.close();
          return;
        case "offer":
        case "answer":
        case "ice":
          this.routeMessage(myPeerId!, msg as unknown as RoutedMessage);
          return;
        case "relay_request":
          this.handleRelayRequest(myPeerId!, msg);
          return;
        default:
          this.sendError(ws, "unknown_kind");
          return;
      }
    });

    ws.addEventListener("close", () => {
      if (myPeerId != null) this.handleLeave(myPeerId);
    });

    ws.addEventListener("error", () => {
      if (myPeerId != null) this.handleLeave(myPeerId);
    });
  }

  private handleRegister(
    ws: WebSocket,
    msg: Record<string, unknown>,
  ): number | null {
    const role = msg.role;
    const displayName =
      typeof msg.display_name === "string" ? msg.display_name : undefined;

    if (role !== "host" && role !== "join") {
      this.sendError(ws, "invalid_role");
      return null;
    }
    if (this.peers.size >= MAX_PEERS) {
      this.sendError(ws, "room_full");
      return null;
    }

    if (role === "host") {
      if (this.hostId !== null) {
        this.sendError(ws, "host_already_exists");
        return null;
      }
      const peerId = this.allocateId();
      this.hostId = peerId;
      this.peers.set(peerId, { ws, role: "host", displayName });
      this.sendJson(ws, {
        kind: "registered",
        peer_id: peerId,
        existing_peers: [],
        ice_servers: this.iceServers(),
      });
      console.log(
        `[room] CREATED host=peer${peerId}${displayName ? ` name=${displayName}` : ""}`,
      );
      return peerId;
    }

    // role === "join"
    if (this.hostId === null) {
      this.sendError(ws, "no_host");
      return null;
    }
    const peerId = this.allocateId();
    this.peers.set(peerId, { ws, role: "join", displayName });
    const existing = [...this.peers.keys()].filter((id) => id !== peerId);
    this.sendJson(ws, {
      kind: "registered",
      peer_id: peerId,
      existing_peers: existing,
      ice_servers: this.iceServers(),
    });
    console.log(
      `[room] JOIN peer${peerId}${displayName ? ` name=${displayName}` : ""} (host=peer${this.hostId})`,
    );
    // 通知 host 有新人加入；其它已在房间的 peer 也通知（便于未来 mesh 拓扑）
    for (const [id, peer] of this.peers) {
      if (id === peerId) continue;
      this.sendJson(peer.ws, {
        kind: "peer_joined",
        peer_id: peerId,
        display_name: displayName,
      });
    }
    return peerId;
  }

  private routeMessage(from: number, msg: RoutedMessage): void {
    const to = typeof msg.to === "number" ? msg.to : null;
    if (to == null) return;
    const target = this.peers.get(to);
    if (!target) return; // 静默丢弃；不向 sender 暴露成员存在与否
    // 复制并替换 to → from（接收端关心的是来源）
    const forwarded: Record<string, unknown> = { kind: msg.kind, from };
    if (msg.kind === "offer" || msg.kind === "answer") {
      forwarded.sdp = msg.sdp;
    } else if (msg.kind === "ice") {
      forwarded.candidate = msg.candidate;
    }
    this.sendJson(target.ws, forwarded);
  }

  // 处理 Host 发起的中继升级请求。
  // 仅 Host 可调用；peer_id 必须存在且非自身；幂等（已建立则 no-op）。
  private handleRelayRequest(from: number, msg: Record<string, unknown>): void {
    if (from !== this.hostId) {
      this.sendError(this.peers.get(from)!.ws, "relay_request_host_only");
      return;
    }
    const peerId =
      typeof msg.peer_id === "number" ? (msg.peer_id as number) : null;
    if (peerId == null || peerId === from) {
      this.sendError(this.peers.get(from)!.ws, "relay_request_invalid_peer");
      return;
    }
    const target = this.peers.get(peerId);
    if (!target) {
      this.sendError(this.peers.get(from)!.ws, "relay_request_no_peer");
      return;
    }
    // 已建立 → 幂等
    if (this.relayPairs.get(from)?.has(peerId)) return;

    // 双向写入
    this.linkRelay(from, peerId);

    const params = {
      max_msg_size: RELAY_MAX_PAYLOAD,
      max_rate: RELAY_RATE_CAPACITY,
    };
    this.sendJson(this.peers.get(from)!.ws, {
      kind: "relay_active",
      peer_id: peerId,
      ...params,
    });
    this.sendJson(target.ws, {
      kind: "relay_active",
      peer_id: from,
      ...params,
    });
    console.log(`[room] RELAY peer${from} <-> peer${peerId}`);
  }

  // 处理二进制中继帧。
  // 帧格式：[target_peer_id: u32 LE][bincode payload...]
  // 校验：长度合法 + 双方已建立 relay pair + 限流。
  private handleBinary(from: number, buf: ArrayBuffer): void {
    if (buf.byteLength < 4) {
      this.closeRelayPair(from, /* notifyPeer */ null, "invalid_frame");
      return;
    }
    if (buf.byteLength - 4 > RELAY_MAX_PAYLOAD) {
      this.closeRelayPair(from, /* notifyPeer */ null, "msg_too_large");
      return;
    }
    const view = new DataView(buf);
    const targetId = view.getUint32(0, /* littleEndian */ true);
    const partners = this.relayPairs.get(from);
    if (!partners || !partners.has(targetId)) {
      // 未建立中继的目标 → 静默丢弃（不暴露成员存在与否）
      return;
    }
    const target = this.peers.get(targetId);
    if (!target) {
      // 对端已不在房间（应已被 handleLeave 清理），保险起见再清一次
      this.unlinkRelay(from, targetId);
      return;
    }

    // 令牌桶限流
    if (!this.consumeToken(from)) {
      this.closeRelayPair(from, targetId, "rate_limit");
      return;
    }

    // 重写头部为 sender_peer_id，转发剩余字节
    const out = new Uint8Array(buf.byteLength);
    const outView = new DataView(out.buffer);
    outView.setUint32(0, from, true);
    out.set(new Uint8Array(buf, 4), 4);
    try {
      target.ws.send(out.buffer);
    } catch {
      // ignore；下次心跳或 close 事件会清理
    }
  }

  // 令牌桶：容量 RELAY_RATE_CAPACITY，按时间线性补充。
  private consumeToken(peerId: number): boolean {
    const now = Date.now();
    let state = this.rateState.get(peerId);
    if (!state) {
      state = { tokens: RELAY_RATE_CAPACITY, lastRefillMs: now };
      this.rateState.set(peerId, state);
    }
    const elapsed = now - state.lastRefillMs;
    if (elapsed > 0) {
      state.tokens = Math.min(
        RELAY_RATE_CAPACITY,
        state.tokens + elapsed * RELAY_RATE_REFILL_PER_MS,
      );
      state.lastRefillMs = now;
    }
    if (state.tokens < 1) return false;
    state.tokens -= 1;
    return true;
  }

  private linkRelay(a: number, b: number): void {
    let setA = this.relayPairs.get(a);
    if (!setA) {
      setA = new Set();
      this.relayPairs.set(a, setA);
    }
    setA.add(b);
    let setB = this.relayPairs.get(b);
    if (!setB) {
      setB = new Set();
      this.relayPairs.set(b, setB);
    }
    setB.add(a);
  }

  private unlinkRelay(a: number, b: number): void {
    this.relayPairs.get(a)?.delete(b);
    this.relayPairs.get(b)?.delete(a);
    if (this.relayPairs.get(a)?.size === 0) this.relayPairs.delete(a);
    if (this.relayPairs.get(b)?.size === 0) this.relayPairs.delete(b);
  }

  // 关闭 from 与 partner 的中继对（partner=null 表示关闭 from 与所有对端的中继）。
  // reason 通过 relay_closed 通知双方。
  private closeRelayPair(
    from: number,
    partner: number | null,
    reason: string,
  ): void {
    const partners = this.relayPairs.get(from);
    if (!partners) return;
    const targets = partner != null ? [partner] : Array.from(partners);
    for (const p of targets) {
      this.unlinkRelay(from, p);
      const fromWs = this.peers.get(from)?.ws;
      const pWs = this.peers.get(p)?.ws;
      if (fromWs) {
        this.sendJson(fromWs, { kind: "relay_closed", peer_id: p, reason });
      }
      if (pWs) {
        this.sendJson(pWs, { kind: "relay_closed", peer_id: from, reason });
      }
      console.log(
        `[room] RELAY_CLOSED peer${from} <-> peer${p} reason=${reason}`,
      );
    }
  }

  private handleLeave(peerId: number): void {
    const peer = this.peers.get(peerId);
    if (!peer) return;

    // 先关闭该 peer 涉及的所有中继对，通知对端
    if (this.relayPairs.has(peerId)) {
      this.closeRelayPair(peerId, null, "peer_left");
    }
    this.rateState.delete(peerId);

    this.peers.delete(peerId);

    if (peerId === this.hostId) {
      // 房主离开 → 整个房间销毁
      console.log(
        `[room] CLOSED reason=host_left host=peer${peerId} remaining=${this.peers.size}`,
      );
      this.hostId = null;
      for (const [, p] of this.peers) {
        try {
          this.sendJson(p.ws, {
            kind: "room_closed",
            reason: "host_left",
          });
          p.ws.close();
        } catch {
          // ignore
        }
      }
      this.peers.clear();
      this.relayPairs.clear();
      this.rateState.clear();
    } else {
      // 普通 peer 离开 → 通知 host（Phase 5+ 还会通知其它 remote）
      console.log(`[room] LEFT peer${peerId} (host=peer${this.hostId})`);
      const host = this.hostId != null ? this.peers.get(this.hostId) : null;
      if (host) {
        this.sendJson(host.ws, { kind: "peer_left", peer_id: peerId });
      }
    }
  }

  private allocateId(): number {
    return this.nextPeerId++;
  }

  private iceServers(): IceServer[] {
    // 公共 STUN；TURN 凭据下发是 v2 的事（见 docs/networking/signaling.md §九）
    return [{ urls: ["stun:stun.l.google.com:19302"] }];
  }

  private sendJson(ws: WebSocket, obj: unknown): void {
    try {
      ws.send(JSON.stringify(obj));
    } catch {
      // 连接已关闭等情况下静默
    }
  }

  private sendError(ws: WebSocket, message: string): void {
    this.sendJson(ws, { kind: "error", message });
  }
}

interface RoutedMessage {
  kind: "offer" | "answer" | "ice";
  to?: number;
  sdp?: string;
  candidate?: string;
}
