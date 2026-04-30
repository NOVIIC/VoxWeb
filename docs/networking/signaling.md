# 信令服务（Cloudflare Workers）

> **何时阅读**：改 WebSocket 协议；改房间生命周期；改 Durable Objects；调 TURN 凭据下发
> **关联文档**：[`README.md`](../../README.md) · [`modules/net.md`](../modules/net.md) · [`networking/protocol.md`](protocol.md) · [`deployment.md`](../deployment.md)

---

## 一、定位与原则

信令服务的**唯一职责**：帮助两个浏览器找到彼此并交换 WebRTC SDP/ICE。一旦 P2P 直连成功，信令服务可下线不影响进行中的会话。

**严格独立部署**：
- 不依赖游戏静态站
- 不存储任何游戏数据
- 玩家选择的房间号即为信令通道地址
- v2 可扩展为"中继回退"，仍保持与游戏数据无耦合（仅做字节转发）

**部署形态**：Cloudflare Workers + Durable Objects
- Workers：处理 HTTP 升级到 WebSocket
- Durable Objects：每个房间一个实例，维护成员列表与转发逻辑

**目录布局**：

```
signaling/
├── package.json
├── tsconfig.json
├── wrangler.toml
└── src/
    ├── worker.ts       Worker 入口（路由 / 升级 WebSocket）
    └── room.ts         Durable Object: Room（房间状态）
```

---

## 二、HTTP 路由

| 路径 | 方法 | 功能 |
|---|---|---|
| `/health` | GET | 健康检查（返回 200） |
| `/room/:room_id` | GET (Upgrade: websocket) | WebSocket 升级；进入房间通信 |
| `/turn-credentials` | GET | （v2）下发短期 TURN 凭据 |

`room_id` 校验规则：
- 长度 4-12
- 仅包含 `[a-zA-Z0-9_-]`
- 不区分大小写（统一转 lowercase 后路由）

不合法直接返回 400。

---

## 三、WebSocket 协议

所有消息使用 **JSON 文本帧**（信令通量小，可读性优先于紧凑性）。

### 客户端 → 服务端

```ts
type ClientToServer =
  | { kind: "register"; role: "host" | "join"; display_name?: string }
  | { kind: "offer"; to: number; sdp: string }
  | { kind: "answer"; to: number; sdp: string }
  | { kind: "ice"; to: number; candidate: string }
  | { kind: "leave" };
```

### 服务端 → 客户端

```ts
type ServerToClient =
  | { kind: "registered"; peer_id: number; existing_peers: number[]; ice_servers: IceServer[] }
  | { kind: "peer_joined"; peer_id: number; display_name?: string }
  | { kind: "peer_left"; peer_id: number }
  | { kind: "offer"; from: number; sdp: string }
  | { kind: "answer"; from: number; sdp: string }
  | { kind: "ice"; from: number; candidate: string }
  | { kind: "room_closed"; reason: string }
  | { kind: "error"; message: string };

interface IceServer {
  urls: string[];
  username?: string;
  credential?: string;
}
```

### 协议规则

1. **第一条消息必须是 `register`**；其它消息在 register 之前到达 → `error` + 关闭
2. **`role: "host"`**：创建房间。如果房间已有其它 host → 拒绝（一房一主机）
3. **`role: "join"`**：加入现有房间。如果房间不存在或没有 host → 拒绝
4. **`peer_id`**：由服务端在 register 时分配（u32 单调递增；房间销毁后重置）
5. **转发逻辑**：`offer` / `answer` / `ice` 通过 `to` 字段路由到目标 peer；目标不存在则丢弃 + 不应答错误（避免泄漏房间成员）
6. **离开**：客户端主动发 `leave`；或 WebSocket 断开自动等价于 leave

---

## 四、房间生命周期

```
1. Host 客户端连接 wss://.../room/abc → Worker 升级 WS → 路由到 Room("abc") DO
2. Host 发 register{host} → DO 创建房间 → 分配 peer_id=1 → 返回 registered
3. Remote 1 连接 → register{join} → DO 检查房间存在 + 有 host
                                  → 分配 peer_id=2
                                  → 给 Host 发 peer_joined{2}
                                  → 给 Remote 1 发 registered{2, existing_peers=[1]}
4. Remote 1 自己 createOffer 后给 Host 发 offer{to=1, sdp}
                                  → DO 转发给 Host 的 WS
5. Host 处理 offer，createAnswer，发 answer{to=2, sdp}
                                  → DO 转发给 Remote 1
6. 双方互发 ice candidate via DO
7. WebRTC 直连成功后，DO 不再被使用（但仍保持房间状态以接受新 peer）
8. Host 离开（leave 或 WS 断开） → DO 给所有其它 peer 发 room_closed → 销毁房间
9. Remote 离开 → DO 给其它 peer 发 peer_left{该 peer_id}
```

---

## 五、Durable Object: `Room`

```ts
export class Room implements DurableObject {
  private peers = new Map<number, PeerInfo>();
  private nextPeerId = 1;
  private hostId: number | null = null;

  async fetch(req: Request): Promise<Response> {
    const url = new URL(req.url);
    if (req.headers.get("Upgrade") !== "websocket") {
      return new Response("expected websocket", { status: 400 });
    }
    const pair = new WebSocketPair();
    const [client, server] = Object.values(pair);

    server.accept();
    this.attach(server);

    return new Response(null, { status: 101, webSocket: client });
  }

  private attach(ws: WebSocket) {
    let myPeerId: number | null = null;
    let registered = false;

    ws.addEventListener("message", (ev) => {
      const msg = parseMessage(ev.data);
      if (!msg) { ws.send(err("invalid_json")); ws.close(); return; }

      if (!registered) {
        if (msg.kind !== "register") {
          ws.send(err("must_register_first")); ws.close(); return;
        }
        const peerId = this.handleRegister(ws, msg);
        if (peerId == null) { ws.close(); return; }
        myPeerId = peerId;
        registered = true;
        return;
      }

      if (msg.kind === "leave") {
        this.handleLeave(myPeerId!);
        ws.close();
        return;
      }

      // offer / answer / ice 路由
      this.routeMessage(myPeerId!, msg);
    });

    ws.addEventListener("close", () => {
      if (myPeerId != null) this.handleLeave(myPeerId);
    });
  }

  private handleRegister(ws: WebSocket, msg: RegisterMessage): number | null {
    if (msg.role === "host") {
      if (this.hostId !== null) {
        ws.send(err("host_already_exists"));
        return null;
      }
      const peerId = this.allocateId();
      this.hostId = peerId;
      this.peers.set(peerId, { ws, role: "host", displayName: msg.display_name });
      ws.send(JSON.stringify({
        kind: "registered",
        peer_id: peerId,
        existing_peers: [],
        ice_servers: this.iceServers(),
      }));
      return peerId;
    } else {
      if (this.hostId === null) {
        ws.send(err("no_host"));
        return null;
      }
      const peerId = this.allocateId();
      this.peers.set(peerId, { ws, role: "join", displayName: msg.display_name });
      ws.send(JSON.stringify({
        kind: "registered",
        peer_id: peerId,
        existing_peers: [...this.peers.keys()].filter(id => id !== peerId),
        ice_servers: this.iceServers(),
      }));
      // 通知 host
      this.peers.get(this.hostId)!.ws.send(JSON.stringify({
        kind: "peer_joined",
        peer_id: peerId,
        display_name: msg.display_name,
      }));
      return peerId;
    }
  }

  private routeMessage(from: number, msg: RoutedMessage) {
    const target = this.peers.get(msg.to);
    if (!target) return; // 静默丢弃
    target.ws.send(JSON.stringify({ ...msg, from }));
  }

  private handleLeave(peerId: number) {
    this.peers.delete(peerId);
    if (peerId === this.hostId) {
      // 房主离开 → 整个房间销毁
      for (const [, peer] of this.peers) {
        peer.ws.send(JSON.stringify({ kind: "room_closed", reason: "host_left" }));
        peer.ws.close();
      }
      this.peers.clear();
      this.hostId = null;
    } else {
      // 普通 peer 离开 → 通知 host
      const host = this.hostId != null ? this.peers.get(this.hostId) : null;
      host?.ws.send(JSON.stringify({ kind: "peer_left", peer_id: peerId }));
    }
  }

  private allocateId(): number { return this.nextPeerId++; }

  private iceServers(): IceServer[] {
    return [
      { urls: ["stun:stun.l.google.com:19302"] },
      // v2: 通过 env.TURN_SECRET 生成短期 TURN 凭据并附带在此处
    ];
  }
}
```

> 实际代码会比上述更紧凑，但状态机与角色分配逻辑必须严格一致。

---

## 六、`worker.ts` — Workers 入口

```ts
export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);

    if (url.pathname === "/health") {
      return new Response("ok", { status: 200 });
    }

    if (url.pathname.startsWith("/room/")) {
      const roomId = url.pathname.slice("/room/".length).toLowerCase();
      if (!validRoomId(roomId)) {
        return new Response("invalid room id", { status: 400 });
      }
      const id = env.ROOMS.idFromName(roomId);
      const stub = env.ROOMS.get(id);
      return stub.fetch(request);
    }

    return new Response("not found", { status: 404 });
  },
};

function validRoomId(s: string): boolean {
  return /^[a-z0-9_-]{4,12}$/.test(s);
}
```

---

## 七、`wrangler.toml`

```toml
name = "voxweb-signaling"
main = "src/worker.ts"
compatibility_date = "2026-04-01"

[[durable_objects.bindings]]
name = "ROOMS"
class_name = "Room"

[[migrations]]
tag = "v1"
new_classes = ["Room"]

[vars]
# 公共配置（非敏感）
ICE_SERVERS_PUBLIC = '[{"urls":["stun:stun.l.google.com:19302"]}]'

# 敏感配置通过 wrangler secret put 注入：
# TURN_SECRET = "..."          （v2 阶段使用）
```

部署：
```bash
cd signaling
npm install
wrangler deploy
```

绑定自定义域名（如 `wss://signal.voxweb.example.com/room/...`）通过 Cloudflare 仪表盘配置。

---

## 八、限流与安全

### 房间号防猜
房间号是唯一的"凭证"。建议：
- 客户端默认生成 6 位随机字符（约 36⁶ ≈ 22 亿组合）
- 仅在用户分享房间号给朋友的场景下使用，无法防御主动扫描
- v2 可加"房间密码"参数，DO 在 register 时校验

### Worker 限流
- Cloudflare 自带 DDoS 防护
- 单房间 peer 上限：硬编码 16（DO `handleRegister` 中校验 `peers.size < 16`）
- 单 IP 房间数限制：v2 实现（用 KV 存 IP→房间集合）

### CORS / Origin 校验
WebSocket 不走 CORS，但可在升级握手时校验 `Origin` 头：
```ts
const origin = req.headers.get("Origin") ?? "";
if (!ALLOWED_ORIGINS.includes(origin)) {
  return new Response("forbidden", { status: 403 });
}
```

`ALLOWED_ORIGINS` 通过 `env` 注入；本地开发与生产域名都加入白名单。

---

## 九、TURN 中继（v2）

当 P2P NAT 穿透失败（NAT 类型不友好），需要 TURN 中继。

**方案 A**：使用 Cloudflare Calls TURN（按流量计费）
- 在 `env` 中存 `TURN_KEY_ID` + `TURN_KEY_SECRET`
- 在 `registered` 消息中下发短期凭据（HMAC 生成，1 小时有效）

**方案 B**：自建 coturn
- 成本固定但需运维

**方案 C**（与 v2 联动）：信令服务自己当中继
- 客户端 WebRTC 失败后，重新连接信令服务，所有 DataChannel 消息走信令转发
- 性能差，仅作 fallback；本期完全不做

`registered` 消息的 `ice_servers` 字段就是为这个预留的扩展点。

---

## 十、本地开发

```bash
cd signaling
npm install
wrangler dev --local --port 8787
```

`wrangler dev` 模拟 Workers + Durable Objects 在本地运行。客户端连接 `ws://localhost:8787/room/test`。

游戏 client 中信令地址通过 query string 或环境变量切换：
- 开发：`ws://localhost:8787`
- 生产：`wss://signal.voxweb.example.com`

---

## 十一、监控

- Cloudflare Workers Dashboard：请求数、错误率、CPU 时间
- 自定义日志：`console.log` 在 Workers 中通过 `wrangler tail` 实时查看
- 关键事件埋点：`room_created` / `peer_joined` / `peer_left` / `host_left` / `signaling_error`
- v2 可发到 Cloudflare Analytics Engine

---

## 十二、不在范围

- 玩家账号系统 / 持久身份
- 房间发现 / 公共大厅列表（设计上要求"知道房间号才能加入"，避免被搜寻）
- 文字聊天历史持久化（聊天走 P2P，不经过信令）
- 反作弊机制
- 跨房间通信
- 录制/回放（v3 / 不规划）
