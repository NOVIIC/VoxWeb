/// <reference types="@cloudflare/workers-types" />

// 入口：把 /room/:id 升级请求转发到对应房间的 Durable Object，
// 健康检查 / 其它路径直接走 fetch handler。
// 协议详见 docs/networking/signaling.md。

export { Room } from "./room";

interface Env {
  ROOM: DurableObjectNamespace;
}

// roomId 校验：4-12 个字符，[a-z0-9_-]
const ROOM_ID_RE = /^[a-z0-9_-]{4,12}$/;

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);

    // 健康检查（监控用）
    if (url.pathname === "/health") {
      return new Response("ok", { status: 200 });
    }

    // CORS 预检（信令本身在 WebSocket 内不走 CORS，但同源页面在调试时可能发普通 GET）
    if (request.method === "OPTIONS") {
      return new Response(null, {
        headers: {
          "Access-Control-Allow-Origin": "*",
          "Access-Control-Allow-Methods": "GET, OPTIONS",
          "Access-Control-Allow-Headers": "Content-Type",
        },
      });
    }

    // 房间 WebSocket：/room/:id
    const match = url.pathname.match(/^\/room\/([^/]+)$/);
    if (match) {
      // 统一小写后路由，避免 abc 和 ABC 被分配到两个 DO
      const roomId = match[1].toLowerCase();
      if (!ROOM_ID_RE.test(roomId)) {
        return new Response("invalid room id", { status: 400 });
      }
      if (request.headers.get("Upgrade") !== "websocket") {
        return new Response("expected websocket", { status: 400 });
      }
      const id = env.ROOM.idFromName(roomId);
      const stub = env.ROOM.get(id);
      return stub.fetch(request);
    }

    return new Response("VoxWeb Signaling v0.2", { status: 200 });
  },
};
