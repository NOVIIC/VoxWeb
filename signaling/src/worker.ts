/// <reference types="@cloudflare/workers-types" />

export { Room } from "./room";

/// Worker 入口：根据 URL path 分发到不同 Durable Object。
export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);

    // CORS 预检
    if (request.method === "OPTIONS") {
      return new Response(null, {
        headers: {
          "Access-Control-Allow-Origin": "*",
          "Access-Control-Allow-Methods": "GET, POST, OPTIONS",
          "Access-Control-Allow-Headers": "Content-Type",
        },
      });
    }

    // WebSocket 升级：/room/:id
    const match = url.pathname.match(/^\/room\/([a-zA-Z0-9_-]+)$/);
    if (match) {
      const roomId = match[1];
      const id = env.ROOM.idFromName(roomId);
      const stub = env.ROOM.get(id);
      return stub.fetch(request);
    }

    return new Response("VoxWeb Signaling v0.1", { status: 200 });
  },
};

interface Env {
  ROOM: DurableObjectNamespace;
}
