/// <reference types="@cloudflare/workers-types" />

/// Durable Object: 维护一个房间的成员列表，中转 WebRTC 信令消息。
export class Room {
  /// 房间内的 WebSocket 连接列表
  private sessions: Map<string, WebSocket>;

  constructor(_state: DurableObjectState) {
    this.sessions = new Map();
  }

  async fetch(request: Request): Promise<Response> {
    const pair = new WebSocketPair();
    const [client, server] = Object.values(pair);

    const peerId = crypto.randomUUID();
    this.sessions.set(peerId, server);
    server.accept();

    server.addEventListener("message", (event) => {
      // 广播给房间内所有其他 peers
      for (const [id, ws] of this.sessions) {
        if (id !== peerId && ws.readyState === WebSocket.READY_STATE_OPEN) {
          ws.send(event.data as string);
        }
      }
    });

    server.addEventListener("close", () => {
      this.sessions.delete(peerId);
      // 通知其余 peer 有人离开
      for (const ws of this.sessions.values()) {
        if (ws.readyState === WebSocket.READY_STATE_OPEN) {
          ws.send(JSON.stringify({ type: "peer_left", peer_id: peerId }));
        }
      }
    });

    // 通知所有已有 peer 有人加入
    for (const [id, ws] of this.sessions) {
      if (id !== peerId && ws.readyState === WebSocket.READY_STATE_OPEN) {
        ws.send(JSON.stringify({ type: "peer_joined", peer_id: peerId }));
      }
    }

    return new Response(null, { status: 101, webSocket: client });
  }
}
