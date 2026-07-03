# 系统架构总览

> **何时阅读**：加入项目第一周；不确定模块归属时；评估架构级改动时
> **关联文档**：[`../README.md`](../README.md) · [`modules/*`](modules/) · [`networking/protocol.md`](networking/protocol.md) · [`roadmap.md`](roadmap.md)

---

## 一、进程拓扑

`VoxWeb` 的运行环境是浏览器 Tab。每个 Tab 是一个 WASM 实例，下图描述多 Tab（多玩家）协作时的全貌：

```
┌──────────────────────────────────────────────────────────────────────────┐
│                          Caddy 静态站点 (HTTPS)                            │
│   /        → dist/index.html       (Monet 风格 landing；纯 HTML/CSS)       │
│   /start   → dist/start.html       (canvas + wasm-bindgen 胶水)            │
│   /*.wasm  → dist/voxweb-client-<hash>_bg.wasm                            │
│   /*.js    → dist/voxweb-client-<hash>.js                                 │
└──────────────────────────────────────────────────────────────────────────┘
                            ↓ HTTP GET（首屏）
        ┌────────────────┐         ┌────────────────┐         ┌────────────────┐
        │ 浏览器 Tab A    │         │ 浏览器 Tab B    │         │ 浏览器 Tab C    │
        │  (Host)        │         │  (Remote 1)    │         │  (Remote 2)    │
        │  ┌──────────┐  │         │  ┌──────────┐  │         │  ┌──────────┐  │
        │  │ Server   │  │         │  │  ─       │  │         │  │  ─       │  │
        │  │ + Client │  │         │  │  Client  │  │         │  │  Client  │  │
        │  └──────────┘  │         │  └──────────┘  │         │  └──────────┘  │
        └────────────────┘         └────────────────┘         └────────────────┘
                ↑                          ↑                           ↑
                │   WebRTC DataChannel    │   WebRTC DataChannel      │
                ├──────── (P2P) ──────────┴──────── (P2P) ────────────┘
                │
                │  WebSocket (signaling, 仅在握手期使用)
                ↓
        ┌────────────────────────────────────────────────────────────────┐
        │   Cloudflare Workers + Durable Objects（独立部署，与静态站分离） │
        │   wss://signal.example.com/room/:id                              │
        └────────────────────────────────────────────────────────────────┘
                ↑
                │  STUN（公共 Google STUN）
                │  TURN 中继（v2，可选；当 P2P 直连失败时降级走中继）
                ↓
        ┌────────────────────────────────────────────────────────────────┐
        │   公共 STUN 服务器 / TURN 服务器（可选：cloudflare-turn 或自建）       │
        └────────────────────────────────────────────────────────────────┘
```

**关键观察**：
- Caddy 静态站只负责把游戏代码送进浏览器；游戏开始后浏览器与静态站之间无任何长连接。
- Cloudflare Workers 信令仅在 P2P 握手期短暂使用；连接建立后两个 Peer 直连，信令服务可下线不影响进行中的会话。
- 所有游戏世界数据流（FieldSnapshot、FieldDelta、FreeObjectSpawn/State/Project、PlayerTick）都走 P2P DataChannel，不经过任何中心服务器。
- **兜底**：若某对 (Host, Remote) 的 WebRTC 直连失败（ICE 失败 / 15s 协商超时），Host 自动请求信令 Worker 把该对升级为「字节中继」——后续 bincode 字节走 WS 二进制帧由 DO 转发，其它直连 peer 不受影响。详见 [`networking/signaling.md`](networking/signaling.md) §九。

---

## 二、角色矩阵

每个浏览器 Tab 在某一时刻只处于以下三种角色之一：

| 角色 | Server 实例 | 网络职责 | 触发条件 |
|---|---|---|---|
| **Local-Only** | 内嵌 | 不联网 | 玩家选择"单机模式"，跳过信令 |
| **Host** | 内嵌 + 权威 | 监听信令房间，与所有 Remote 双向 P2P | 玩家选择"创建房间" |
| **Remote Client** | 不持有 | 与 Host 单向 P2P | 玩家选择"加入房间" |

**角色切换路径**：
- Local-Only ↔ Host：当前仅在大厅决定一次，不支持游戏中直接开放联机
- Host ↔ Remote：不可切换；房间销毁后所有人回到大厅
- Remote 升 Host：可选增强（主机迁移）

> 设计含义：`server` crate 在 Local-Only 与 Host 模式下代码完全相同，只是消息源不同（Local-Only 走内存通道；Host 走 P2P + 内存通道复用）。这让单人模式与多人模式共享 90% 服务端代码。

---

## 三、三类帧（调度模型）

整个 WASM 实例运行在单线程上（按 `../README.md` 决策），所有任务通过 `wasm-bindgen-futures` 协作调度。任务分三类：

### 3.1 渲染帧（RAF）
- **频率**：可变，由浏览器 `requestAnimationFrame` 驱动（典型 60 fps，高刷屏 120/144 fps）
- **职责**：
  1. 采集本地输入（键盘/鼠标增量）
  2. 立即更新本地相机位置/朝向（客户端预测，零延迟）
  3. 推进 GPU 渲染：依次执行 Render Graph 中所有 Pass
  4. 远端玩家位置插值（基于本地时间 + interp 缓冲区）
  5. egui UI 重建与渲染
- **预算**：单帧总耗时 < 16.6ms（60fps 目标）；网格化任务从这里"借时间"，每帧最多 4ms
- **关键代码**：`crates/client/src/lib.rs` 主循环

### 3.2 逻辑帧（固定 60Hz）
- **频率**：60Hz 固定步长（dt = 1/60 s）
- **驱动方式**：渲染帧内累加 `dt`，超过 1/60 时触发一次或多次逻辑 tick（`accumulator -= step` 模式）
- **职责**：
  1. 物理模拟（重力、碰撞）
  2. 服务端权威判定（Host/Local-Only 角色才执行）
  3. 远端 Peer 位置广播（Host 角色：发 PlayerTick）
  4. 已修改区块的 dirty 标记（供持久化）
- **关键代码**：`crates/server/src/world.rs::tick()` + `crates/client/src/lib.rs::update_logic()`

### 3.3 异步任务（事件驱动）
- **频率**：事件触发，无固定节奏
- **典型任务**：
  - 信令 WebSocket 消息收发（连接建立 / 断开 / Offer/Answer / ICE）
  - WebRTC PeerConnection 状态机推进
  - DataChannel `onmessage` 回调（解码消息 → 推入 mpsc 通道）
  - OPFS 读写（Host：启动 prime / 按需 load / 周期性 flush，详见 [`features/persistence.md`](features/persistence.md)）
  - 远端 Chunk 解码 + 网格化任务入队
- **调度**：通过 `wasm_bindgen_futures::spawn_local` 启动；与渲染/逻辑帧通过 `futures-channel` mpsc 通信
- **重要约束**：异步任务**不得阻塞主循环**，所有 await 点必须是真异步（如 OPFS 文件读、WebSocket 消息），不要在 await 之间做长 CPU 工作

```
┌────────────────────────────────────────────────────────────────────┐
│                       单线程协作调度示意                              │
│                                                                    │
│  ┌────────────┐                                                    │
│  │ Browser    │  RAF 回调       事件回调（WebRTC/OPFS/WS）              │
│  │ Event Loop │ ────────┐       ────────┐                          │
│  └────────────┘         │              │                          │
│                         ↓              ↓                          │
│                ┌────────────────┐ ┌────────────────┐               │
│                │ render_frame() │ │ async tasks    │               │
│                │  - 输入        │ │  - signaling   │               │
│                │  - 相机        │ │  - peer state  │               │
│                │  - logic tick  │ │  - OPFS        │               │
│                │    (n 次)      │ │  - mesh job    │               │
│                │  - mesh budget │ │    (decode)    │               │
│                │  - egui        │ └────────────────┘               │
│                │  - GPU submit  │         │                        │
│                └────────────────┘         │                        │
│                         ↑                 ↓                        │
│                         │   futures-channel mpsc 双向                │
│                         └─────────────────┘                        │
└────────────────────────────────────────────────────────────────────┘
```

---

## 四、Workspace 结构

```
VoxWeb/
├── Cargo.toml                  # workspace 根
├── README.md                   # 项目主入口文档
├── CLAUDE.md                   # AI agent 工作纪律（沿用）
├── docs/                       # 本文档体系所在目录
├── index.html                  # landing 页（Monet 风格，纯 HTML/CSS/JS；trunk copy-file 复制）
├── start.html                  # 游戏页 = trunk 模板入口（canvas + wasm-bindgen 胶水）
├── Caddyfile                   # 静态站点配置（含 /start → /start.html 重写）
├── trunk.toml                  # 构建配置（target = "start.html"）
├── crates/
│   ├── core/                   # 数据结构 + 协议（无浏览器依赖）
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs          # 模块声明 + re-export
│   │       ├── block.rs        # BlockID/MaterialID + 硬编码材质表
│   │       ├── chunk.rs        # Chunk + Position + ChunkPos
│   │       ├── field.rs        # FieldChunk + Column Store 原型
│   │       ├── geometry.rs     # AABB + 玩家碰撞体工具
│   │       └── protocol.rs     # ClientMessage / ServerMessage / RoomEvent
│   ├── render/                 # WGPU 渲染（仅 WebGPU 后端）
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs          # Renderer + 公开 API
│   │       ├── device.rs       # Surface + Device 与 canvas 绑定
│   │       ├── frustum.rs      # 视锥体平面抽取 + AABB 裁剪
│   │       ├── passes/
│   │       │   ├── mod.rs
│   │       │   ├── opaque.rs   # 实体方块 Pass
│   │       │   ├── skybox.rs   # 天空盒 Pass
│   │       │   ├── transparent.rs # 半透明 Pass
│   │       │   └── selection.rs   # 选中方块线框 Pass
│   │       ├── chunk_mesh.rs   # 贪婪网格化 + 跨区块面剔除 + AO + bounds
│   │       ├── vertex.rs       # u32 压缩格式
│   │       ├── texture.rs      # 纹理图集
│   │       └── shaders/
│   │           ├── chunk.wgsl      # 实体方块着色器
│   │           └── selection.wgsl  # 选中线框着色器
│   ├── server/                 # 世界逻辑（lib only）
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── world.rs        # World + 玩家表 + tick
│   │       ├── terrain.rs      # Perlin 地形
│   │       ├── physics.rs      # 物理仲裁
│   │       ├── persistence.rs  # PersistenceManager + dirty snapshot/commit/retry
│   │       └── handle_message_tests.rs # Server 消息处理单元测试
│   ├── net/                    # P2P 网络层
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs          # NetEndpoint + 公开 API
│   │       ├── signaling.rs    # WebSocket 信令客户端
│   │       ├── peer.rs         # WebRTC PeerConnection 包装
│   │       ├── room.rs         # 房间会话状态机
│   │       ├── relay.rs        # outbox 路由计划 + Worker 中继限流
│   │       └── transport.rs    # 通道分配（reliable/unreliable）
│   └── client/                 # 入口（cdylib for wasm）
│       ├── Cargo.toml
│       └── src/
│           ├── lib.rs          # #[wasm_bindgen(start)] + 主循环编排
│           ├── browser.rs      # 浏览器时间、URL/query、canvas 尺寸
│           ├── events.rs       # DOM 事件监听 + 输入/egui 事件转发
│           ├── hud.rs          # 游戏内 HUD、hotbar、通知和性能统计
│           ├── app.rs          # AppState 状态机 + Game 主结构
│           ├── camera.rs       # 第一人称相机
│           ├── input.rs        # 键盘/鼠标输入
│           ├── physics.rs      # 玩家本地物理（Walk/Fly）
│           ├── raycast.rs      # DDA 射线
│           ├── prediction.rs   # 客户端预测 + PendingActions rollback
│           ├── interp.rs       # 远端玩家插值
│           ├── mesh_jobs.rs    # 网格化任务队列 + 分帧调度
│           ├── chunk_loader.rs # 区块滚动加载 / 卸载
│           ├── hotbar.rs       # 9 格快捷栏
│           ├── ui/
│           │   ├── mod.rs
│           │   ├── lobby.rs    # 大厅 + Connecting UI
│           │   ├── pause.rs    # 暂停菜单
│           │   ├── chat.rs     # 聊天
│           │   └── players.rs  # 玩家列表/名牌
│           └── storage.rs      # OPFS 异步包装
└── signaling/                  # 独立部署（TS 项目）
    ├── wrangler.toml
    ├── package.json
    └── src/
        ├── worker.ts           # Worker 入口
        └── room.ts             # Durable Object: Room
```

---

## 五、模块依赖图

```
                    ┌─────────────────────────┐
                    │        client (cdylib)  │
                    │  ┌──────────────────┐   │
                    │  │ AppState 主循环    │   │
                    │  └──────────────────┘   │
                    │   ↓        ↓        ↓   │
                    └───┼────────┼────────┼───┘
                        │        │        │
              ┌─────────┘        │        └─────────┐
              ↓                  ↓                  ↓
         ┌─────────┐       ┌──────────┐       ┌──────────┐
         │ render  │       │  server  │       │   net    │
         └─────────┘       └──────────┘       └──────────┘
              │                  │                  │
              └─────────┬────────┴──────────────────┘
                        ↓
                   ┌─────────┐
                   │  core   │ （无依赖；所有 crate 都依赖它）
                   └─────────┘
```

**依赖规则**：
- `core` 是叶子，不依赖其它 crate；不允许依赖 `wgpu`、`web-sys` 等平台库
- `render` 依赖 `core`，但**不知道** `server` / `net` 的存在
- `server` 依赖 `core`，但**不知道** `render` / `net` 的存在（持久化通过抽象 trait 实现）
- `net` 依赖 `core`（消息序列化）；不知道 `render` / `server`
- `client` 是 orchestrator，依赖全部其它 crate；负责把它们粘合起来
- 反向依赖一律禁止（`render` 不能依赖 `client`）

---

## 六、消息流（端到端示例：玩家挖方块）

```
[Tab A: Remote Client]                              [Tab B: Host]
─────────────────────                              ──────────────
1. 鼠标左键按下
2. 本地 raycast 命中 (10, 64, 5)
3. 本地半透明预览（待确认）
4. 发 ClientMessage::Break {pos:(10,64,5), input_tick, player_position}
   通过 reliable DataChannel              ────→    5. 收到消息，server.physics 校验
                                                      （射程内？方块非空？）
                                                  6. 通过 → world.set_cell(pos, MaterialCell::EMPTY)
                                                  7. 标记 chunk dirty
                                                  8. 广播 ServerMessage::FieldDelta
                                                     给所有 peer（含来源）
                                  ←────────       9. （reliable channel）

10. 收到 FieldDelta，commit cell
11. 重新生成受影响 chunk 网格
   （通过 mesh job 异步入队，下个 frame budget 跑）
12. 渲染下一帧呈现新世界
```

详见 `networking/protocol.md` 完整消息表，`features/physics.md` 挖放逻辑细节。

---

## 七、构建与运行视角

```
开发期：
   trunk serve --port 8080
       └→ 监视 crates/* 变化 → cargo build wasm32 → wasm-bindgen → 注入 start.html
   wrangler dev signaling/
       └→ 本地启动 Workers + Durable Objects 模拟器

发布期：
   trunk build --release
       └→ wasm-opt -Oz → dist/pkg/voxweb-client_bg.wasm（目标 < 6MB gz）
   wrangler deploy signaling/
       └→ 推送到 Cloudflare 边缘节点
   rsync dist/ caddy-server:/srv/voxweb/
       └→ Caddy 自动 reload
```

---

## 八、设计原则与不变量

1. **平台无关性下推**：`core` 必须无 `web-sys` / `js-sys` 依赖
2. **协议优先**：任何新功能先在 `core/protocol.rs` 定义消息，再实现客户端/服务端
3. **服务端权威**：客户端预测可"乐观更新"，但 Host 拒绝时必须能回滚
4. **单线程友好**：禁止任何 long-running 同步循环；超过 4ms 的 CPU 任务必须分帧
5. **网络容错**：所有协议消息都假设可能丢失/乱序（unreliable 通道）；可靠性由通道保证而非应用层
6. **文档同步**：架构改动 → 先改本文档与 `modules/*` → 再改代码
