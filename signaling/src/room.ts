/// <reference types="@cloudflare/workers-types" />

// Durable Object: 每个房间一个实例。
// 维护成员列表（peer_id -> WebSocket），并按 docs/networking/signaling.md §三 协议转发 SDP/ICE。
// 设计要点：
//   - 第一条消息必须是 register{role}，否则 close
//   - 一房一 host；host 离开 → 整个房间销毁（其它 peer 收 room_closed）
//   - offer/answer/ice 按 to 字段路由；目标不存在静默丢弃
//   - 上限 16 个 peer，超出拒绝

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

// Workers 运行时全局 WebSocket 暴露的常量（@cloudflare/workers-types 已声明 READY_STATE_OPEN 等）。
const MAX_PEERS = 16;

export class Room {
  private peers = new Map<number, PeerInfo>();
  private hostId: number | null = null;
  private nextPeerId = 1;

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
      // 消息体期望是 JSON 字符串
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

  private handleLeave(peerId: number): void {
    const peer = this.peers.get(peerId);
    if (!peer) return;
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
