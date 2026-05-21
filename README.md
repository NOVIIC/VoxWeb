# VoxWeb README.md

> 文档体系总入口。**任何 AI agent 默认从这里读起**，按需跳转到子文档。
> VoxWeb 是一款浏览器内运行的 WASM 体素沙盒游戏。

---

## 一、项目概述

VoxWeb 是一款基于 **Rust + WebAssembly** 的浏览器内体素沙盒游戏。

- **零后端静态托管**：游戏本体打包为 `.wasm` + `.html` + `.js`，部署在 Caddy 静态站点，访问网址即玩
- **P2P 多人联机**：WebRTC `DataChannel` 直连传输世界数据；信令服务独立部署在 Cloudflare Workers
- **主机权威架构**：首位玩家成为 Host 运行权威逻辑（地形/物理/挖放），其他玩家本地仅做渲染和输入预测；离线时为单人 Local-Only

---

## 二、关键决策表

| 维度 | 决策 | 理由 |
|---|---|---|
| 信令方案 | Cloudflare Workers + Durable Objects | 静态站点零后端；CF Workers 全球边缘 + DO 维护房间状态简单 |
| 多人架构 | Host-Authoritative | 复用单人模式 server 代码；冲突解决最简单 |
| 渲染后端 | 仅 WebGPU | 主流浏览器已支持；不实现 WebGL2 兜底以减少代码复杂度 |
| 项目结构 | 多 Crate workspace | 模块边界清晰，便于单独测试 |
| 线程模型 | 单线程 async（`wasm-bindgen-futures`） | 避开 SharedArrayBuffer / Web Worker 调试复杂度，重 CPU 任务用分帧调度兜底 |
| 存档 | OPFS（仅 Host 写入） | 多 GB 容量；`FileSystemSyncAccessHandle` 同步落盘；palette+RLE 压缩 + LRU 支持万级 dirty chunk（详见 [`docs/features/persistence.md`](docs/features/persistence.md) §二） |
| 序列化 | `bincode`（little-endian、定长配置） | 与 DataChannel 二进制传输契合，体积比 JSON 小一个数量级 |
| 构建工具 | `trunk`（首选）或 `wasm-pack` | trunk 集成 HTML 模板与资源管线，开箱即用 |
| P2P 兜底 | CF Worker 应用层字节中继 | ICE 失败 / 15s 协商超时自动切换；无需部署 TURN（详见 [`docs/networking/signaling.md`](docs/networking/signaling.md) §九） |
| 当前 Phase | Phase 6 ✅ 多人 UI/HUD（2026-05-21） | 见 [`PHASE_6_DONE.md`](PHASE_6_DONE.md) |

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
│   └── persistence.md          OPFS schema + 压缩 + 读写时机 + 房间-世界绑定
├── deployment.md               Caddy 静态站 + 信令 Workers 部署 + 构建工具链
├── reference.md                技术栈版本表 + 浏览器 API 约束 + 已知坑
└── roadmap.md                  Phase 0-9 路线图，每个 Phase 可独立验证
```

---

## 四、术语速查

| 术语 | 含义 |
|---|---|
| **BlockID** | `u16` 方块标识，0 = 空气 |
| **Chunk / ChunkPos** | 16×256×16 = 65536 个方块的存储单元；`(x, z)` 区块坐标支持负数 |
| **Host / Remote / Local-Only** | 房主（跑权威 Server）/ 非房主玩家 / 单人模式（无网络） |
| **OPFS** | Origin Private File System，浏览器内置"源专属"虚拟文件系统，本项目存档底层 |
| **palette + RLE** | Chunk 序列化方案：唯一 BlockID 形成 palette，再对 index 做 run-length 编码 |
| **DataChannel** | WebRTC 字节流通道。本项目用两条：`reliable`（ChunkSync/BlockUpdate/Chat/Join/Leave）与 `unreliable`（60Hz PlayerTick） |
| **Tick / Snapshot** | 服务端 60Hz 逻辑步长 / 新玩家加入时的世界全量快照（分片传输） |
| **Render Graph / Pass** | 多 Pass 渲染调度框架；Pass 即一次 GPU 渲染编码（Depth Pre / Opaque / Skybox / Transparent / UI） |
| **AABB / DDA / AO** | 玩家碰撞体（0.6×1.8）/ 体素射线检测算法 / 顶点级 4 等级环境光遮蔽 |
| **Greedy Meshing** | 贪婪网格化，将相邻同材质方块面合并为大矩形 |
| **AppState** | 客户端状态机：Lobby / Connecting / InGame / EscMenu / ChatOpen |
| **Signaling / STUN / TURN** | SDP/ICE 握手通道（走 CF Workers WebSocket）/ NAT 公网地址发现 / 中继（本项目用 Worker 中继替代） |
| **Durable Object** | Cloudflare Workers 的有状态对象，维护房间成员列表 |
| **trunk** | Rust WASM 项目构建工具，整合 HTML 模板与资源管线 |

---

## 五、不在范围（Out of Scope）

明确**不做**的内容（避免后续 agent 误判）：

- **Mod 系统 / JSON 方块定义** — 方块类型直接硬编码在 `core::blocks`
- **着色器热重载** — 浏览器无文件系统监听 API；开发期依赖 `trunk serve` 整体重载
- **独立 server 二进制** — `server` crate 仅作 lib
- **触屏 / 移动端控件** — 列入 `docs/roadmap.md` Phase 9 stretch goal
- **语音聊天** — 仅文字聊天（Phase 6 实装）
- **WebGL2 兜底** — Firefox 稳定版需提示切换 nightly 或换浏览器
- **加密 / 反作弊高级机制** — 仅做最低限度的输入合法性校验

---

## 六、给后续 agent 的工作纪律

1. **修改源代码前先读文档**：跨多个 crate 至少读 `docs/architecture.md` + 对应 `docs/modules/*.md`
2. **文档先行**：先在文档说清楚再写代码，每次改动同步更新文档；新增文档放到 `docs/` 合适子目录并在本文「文档索引」登记
3. **文档冲突**：以本 README 决策表为准；其余冲突时 `docs/architecture.md` > `docs/modules/*` > `docs/features/*` > 其它
4. **代码注释**：必须加上详细的中文注释以供没有图形学基础的人学习，但架构说明只放文档不写进注释
5. **API 变化**：WGPU 等库新版 API 可能有变化，注意查阅最新文档
