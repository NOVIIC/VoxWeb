# VoxWeb README.md

> 这是文档体系的总入口。**任何 AI agent 默认从这里读起**，按需跳转到子文档。
> VoxWeb 是一款浏览器内运行的 WASM 体素沙盒游戏。

---

## 一、项目概述

VoxWeb 是一款 **运行在浏览器内** 的体素沙盒游戏，采用 Rust 编译为 WebAssembly。其核心特性：

1. **零后端静态托管**：游戏本体是一个 `.wasm` + `.html` + `.js` 包，部署在 **Caddy 静态站点** 上，玩家访问网址即玩。
2. **P2P 多人联机**：玩家之间通过 WebRTC `DataChannel` 直连传输世界数据；信令服务独立部署在 **Cloudflare Workers**（与静态站分离），不与游戏本体耦合。
3. **主机权威架构（Host-Authoritative）**：第一个进入房间的玩家成为 Host，运行权威服务端逻辑（地形生成、物理仲裁、方块挖放校验）；其它玩家作为 Remote Client，本地仅做渲染和输入预测。
4. **功能一览**：第一人称、AABB 物理、跳跃、挖掘/放置、贪婪网格化、Egui UI、地形 Perlin 生成；P2P 联机、IndexedDB 存档、多 Pass 渲染（含天空盒）。
5. **典型用户场景**：朋友间打开网页，一人开房分享 6 位房间号，其它人输入房间号即可同房游戏；离线玩家也可单人模式（Local-Only 角色，跳过信令直接运行 Server）。

---

## 二、关键决策表

| 维度 | 决策 | 理由 |
|---|---|---|
| 信令方案 | Cloudflare Workers + Durable Objects（独立部署） | 静态站点要求零后端；CF Workers 全球边缘 + DO 维护房间内存状态简单 |
| 多人架构 | Host-Authoritative | 复用单人模式 server 代码；冲突解决最简单 |
| 渲染后端 | 仅 WebGPU | 主流浏览器（Chrome/Edge/Safari17+）已支持，Firefox 用户需 nightly；不实现 WebGL2 兜底以减少代码复杂度 |
| 项目结构 | 多 Crate workspace | 模块边界清晰，便于单独测试与未来代码共享 |
| 线程模型 | 单线程 async（`wasm-bindgen-futures`） | 避开 SharedArrayBuffer / Web Worker 调试复杂度，重 CPU 任务用分帧调度兜底 |
| 存档 | IndexedDB（仅 Host 写入） | 浏览器原生持久化，配额大；Remote Client 不持有权威世界 |
| 序列化 | `bincode`（little-endian、定长配置） | 与 DataChannel 二进制传输契合，体积比 JSON 小一个数量级 |
| 编译目标 | `wasm32-unknown-unknown` + `wasm-bindgen` | 主流路径，工具链成熟 |
| 构建工具 | `trunk`（首选）或 `wasm-pack` | trunk 集成 HTML 模板与资源管线，开箱即用 |
| 范围内功能 | 渲染优化核心 / 物理与挖放 / 多人 UI / 多 Pass / 天空盒 / 聊天 | 用户勾选 + agent 决定的扩展项 |
| 范围外功能 | Mod 系统 / 着色器热重载 / 移动端触屏 / 语音聊天 / 独立 server bin | 浏览器侧 ROI 偏低或非本期目标 |

---

## 三、文档索引

```
docs/
├── architecture.md             系统架构总览（拓扑、角色、帧调度、依赖图）
├── modules/                    每个 crate 的内部设计
│   ├── core.md                 数据结构 + 协议消息定义（无浏览器依赖）
│   ├── render.md               wgpu 封装 + Render Graph + 多 Pass + 着色器
│   ├── server.md               世界逻辑 + 物理仲裁 + 地形生成 + 持久化触发
│   ├── net.md                  信令客户端 + WebRTC peer + 房间状态机
│   └── client.md               入口 + AppState + 输入 + 相机 + 主循环
├── networking/                 跨进程协议与同步策略
│   ├── protocol.md             消息总表 + DataChannel 通道划分 + 快照同步流程
│   ├── signaling.md            CF Workers 信令服务 + Durable Objects 房间模型
│   └── prediction.md           客户端预测 + 协调（reconcile）+ 远端玩家插值
├── features/                   端到端的功能特性
│   ├── meshing.md              贪婪网格化 + 跨区块面剔除 + u32 顶点压缩 + 分帧调度
│   ├── physics.md              玩家 AABB + 重力 + 跳跃 + DDA 射线 + 挖放
│   ├── ui.md                   UI 状态机：大厅/HUD/暂停/聊天/玩家列表/名牌
│   └── persistence.md          IndexedDB schema + 读写时机 + 房间-世界绑定
├── deployment.md               Caddy 静态站 + 信令 Workers 部署 + 构建工具链
├── reference.md                技术栈版本表 + 浏览器 API 约束 + 已知坑
└── roadmap.md                  Phase 0-9 路线图，每个 Phase 可独立验证
```

### 文档职责清单（按需阅读）

| 路径 | 主题 | 何时阅读 |
|---|---|---|
| `docs/architecture.md` | 系统级架构图、三类帧调度、角色分裂 | 加入项目第一周 / 不确定模块归属时 |
| `docs/modules/core.md` | `BlockID`、`Chunk`、`Position`、协议消息枚举 | 改动数据结构、新增消息类型时 |
| `docs/modules/render.md` | 渲染器结构、Pass 列表、资源生命周期 | 加 Pass / 改顶点格式 / 改着色器接口时 |
| `docs/modules/server.md` | World 状态、玩家 entity、物理/挖放仲裁 | 改服务端权威逻辑时 |
| `docs/modules/net.md` | 信令客户端、PeerConnection、房间状态机 | 改 WebRTC 连接流程时 |
| `docs/modules/client.md` | 客户端入口、AppState、主循环 | 改启动流程、状态切换、主循环节奏时 |
| `docs/networking/protocol.md` | 完整消息表、通道划分、快照分片 | 增删消息、改字段、调通道可靠性时 |
| `docs/networking/signaling.md` | CF Workers 协议、Durable Objects 设计 | 改信令服务时（独立 repo） |
| `docs/networking/prediction.md` | 客户端预测、协调、远端插值 | 改延迟体验、调位置同步时 |
| `docs/features/meshing.md` | 贪婪网格化算法、压缩格式、AO | 改区块网格性能、加新方块属性时 |
| `docs/features/physics.md` | AABB、跳跃、DDA、挖放消息流 | 改物理手感、改方块交互时 |
| `docs/features/ui.md` | egui 各场景界面、指针锁、聊天 | 改 UI 任意页面时 |
| `docs/features/persistence.md` | IndexedDB schema、读写时机 | 改存档逻辑时 |
| `docs/deployment.md` | Caddy 配置、wrangler 部署、构建命令 | 部署 / 改 CI / 改构建管线时 |
| `docs/reference.md` | 依赖版本、浏览器约束、坑列表 | 升级依赖、排查浏览器兼容问题时 |
| `docs/roadmap.md` | Phase 切分、验证标准 | 决定下一步做什么、估排期时 |

---

## 四、典型阅读路径

按任务类型给出"先读哪份"的推荐链：

- **新增一种方块（含纹理、属性、放置规则）**
  → `docs/modules/core.md`（BlockID 注册）→ `docs/modules/render.md`（纹理图集）→ `docs/features/meshing.md`（顶点压缩里 texture 字段）→ `docs/modules/server.md`（地形生成/挖放校验）

- **优化网格化性能**
  → `docs/features/meshing.md`（核心）→ `docs/modules/render.md`（顶点缓冲调度）→ `docs/architecture.md`（分帧预算）

- **修网络协议（增加一条消息）**
  → `docs/networking/protocol.md`（先读）→ `docs/modules/core.md`（消息枚举定义）→ `docs/modules/net.md`（DataChannel 通道选择）→ `docs/modules/server.md` / `docs/modules/client.md`（收发处理）

- **改多人同步手感（位置抖动 / 卡顿）**
  → `docs/networking/prediction.md`（核心）→ `docs/networking/protocol.md`（PlayerTick 频率）→ `docs/modules/client.md`（插值缓冲区位置）

- **改 UI（某个页面或控件）**
  → `docs/features/ui.md`（核心）→ `docs/modules/client.md`（AppState 切换）

- **改物理（跳跃高度、走路速度、卡墙）**
  → `docs/features/physics.md`（核心）→ `docs/modules/server.md`（物理仲裁参数同步）

- **加一个渲染 Pass（如雾、阴影）**
  → `docs/modules/render.md`（Render Graph）→ `docs/architecture.md`（依赖关系）

- **部署 / 调 CI / 改构建产物体积**
  → `docs/deployment.md`（核心）→ `docs/reference.md`（已知坑）

- **从零理解项目**
  → 本文 → `docs/architecture.md` → `docs/roadmap.md` → 按当前 Phase 关心的领域读对应 `docs/modules/` / `docs/features/`

---

## 五、术语速查

| 术语 | 含义 |
|---|---|
| **BlockID** | `u16` 方块标识，0 = 空气；详见 `docs/modules/core.md` |
| **Chunk** | 16×256×16 = 65536 个方块的存储单元；按 `(y<<8)\|(z<<4)\|x` 索引 |
| **ChunkPos** | `(x, z)` 区块坐标，支持负数 |
| **Host** | 房主，运行权威 Server 实例的 peer |
| **Remote Client** | 非房主玩家，通过 DataChannel 接收 Host 状态 |
| **Local-Only** | 单人模式（无网络），Server 直接挂在 Client 内 |
| **Peer** | WebRTC 对等连接节点（一个浏览器 Tab） |
| **DataChannel** | WebRTC 字节流通道，本项目使用两条：`reliable` 和 `unreliable` |
| **Reliable Channel** | ordered+reliable，传 ChunkSync / BlockUpdate / Chat / Join/Leave |
| **Unreliable Channel** | unordered+unreliable，传 60Hz PlayerTick |
| **Tick** | 服务端固定频率（60Hz）逻辑步长 |
| **Snapshot** | Host 给新加入玩家的世界全量快照（分片传输） |
| **Render Graph** | 多 Pass 渲染调度框架；详见 `docs/modules/render.md` |
| **Pass** | 一次 GPU 渲染编码（Depth Pre / Opaque / Skybox / Transparent / UI） |
| **AABB** | 轴对齐包围盒，玩家碰撞体（0.6×1.8） |
| **DDA** | 数字微分分析器，用于沿视线方向做体素射线检测 |
| **AO** | Ambient Occlusion 环境光遮蔽，顶点级 4 等级 |
| **Greedy Meshing** | 贪婪网格化，将相邻同材质方块面合并为大矩形 |
| **AppState** | 客户端状态机：Lobby / Connecting / InGame / EscMenu / ChatOpen |
| **Signaling** | WebRTC 握手所需的 SDP/ICE 交换通道（本项目走 CF Workers WebSocket） |
| **STUN/TURN** | NAT 穿透辅助：STUN 用于发现公网地址；TURN 用于中继（v2 接入） |
| **Durable Object** | Cloudflare Workers 的有状态对象，用于维护房间成员列表 |
| **trunk** | Rust WASM 项目构建工具，整合 HTML 模板与资源管线 |

---

## 六、不在范围（Out of Scope）

明确**不做**的内容（避免后续 agent 误判）：

- **Mod 系统 / 数据驱动 JSON 方块定义** — 浏览器端运行时加载外部资源 ROI 偏低，方块类型直接硬编码在 `core::blocks`
- **着色器热重载** — 浏览器无文件系统监听 API；如需开发期调试，依赖 `trunk serve` 整体重载
- **独立 server 二进制** — `server` crate 仅作 lib
- **触屏 / 移动端控件** — 列入 `docs/roadmap.md` Phase 9 stretch goal
- **语音聊天** — 虽 WebRTC 支持媒体流，本期不做；文字聊天在 Phase 6 实装
- **WebGL2 兜底** — 用户运行 Firefox 稳定版需提示切换 nightly 或换浏览器
- **加密 / 反作弊高级机制** — 仅做最低限度的输入合法性校验（移动距离、射程上限），不做令牌/签名

---

## 七、给后续 agent 的工作纪律

1. **修改任何源代码前，先确认对应文档**：如果改动跨多个 crate，至少读一遍 `docs/architecture.md` + 对应 `docs/modules/*.md`
2. **每次改动需同步更新文档**：项目纪律是"文档先行"——先在文档说清楚再写代码
3. **新增文档**：放到 `docs/` 合适子目录，并在本文「文档索引」表里登记一行
4. **文档冲突**：以本 README 决策表为准；其它文档冲突时，`docs/architecture.md` > `docs/modules/*` > `docs/features/*` > 其它
5. **代码注释**：代码编写时必须加上详细的中文注释以供没有图形学基础的人进行学习，但不要把架构说明写进代码注释——架构说明只放文档
6. **注意API变化**:WGPU等库新版 API 可能有变化，注意查阅最新文档