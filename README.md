# VoxWeb README.md

> 文档体系总入口。**任何 AI agent 默认从这里读起**，再按本页阅读路径跳转到子文档。
> VoxWeb 是一款浏览器内运行的 WASM 体素沙盒游戏。

---

## 一、项目概述

VoxWeb 是一款基于 **Rust + WebAssembly** 的浏览器内体素沙盒游戏。

- **零后端静态托管**：游戏本体打包为 `.wasm` + `.html` + `.js`，部署在 Caddy 静态站点，访问网址即玩
- **P2P 多人联机**：WebRTC `DataChannel` 直连传输世界数据；信令服务独立部署在 Cloudflare Workers
- **主机权威架构**：首位玩家成为 Host 运行权威逻辑（地形/物理/挖放），其他玩家本地做渲染、输入预测和协调；离线时为单人 Local-Only
- **当前能力**：地形动态加载、玩家物理、挖放交互、P2P 同步、多人 UI、聊天、名牌、多 Pass 渲染、透明方块、OPFS 存档和 Worker 字节中继兜底

---

## 二、关键决策表

| 维度 | 决策 | 理由 |
|---|---|---|
| 信令方案 | Cloudflare Workers + Durable Objects | 静态站点零后端；CF Workers 全球边缘 + DO 维护房间状态简单 |
| 多人架构 | Host-Authoritative | 复用单人模式 server 代码；冲突解决最简单 |
| 渲染后端 | 仅 WebGPU | 主流浏览器已支持；不实现 WebGL2 兜底以减少代码复杂度 |
| 项目结构 | 多 Crate workspace | 模块边界清晰，便于单独测试 |
| 线程模型 | 单线程 async（`wasm-bindgen-futures`） | 避开 SharedArrayBuffer / Web Worker 调试复杂度，重 CPU 任务用分帧调度兜底 |
| 存档 | OPFS（Host / Local-Only 写入） | 浏览器内多 GB 容量；FieldChunk column store；LRU 控制内存；详见 [`docs/features/persistence.md`](docs/features/persistence.md) |
| 序列化 | `bincode`（little-endian、定长配置） | 与 DataChannel 二进制传输契合，体积比 JSON 小一个数量级 |
| 构建工具 | `trunk`（首选）或 `wasm-pack` | trunk 集成 HTML 模板与资源管线，开箱即用 |
| P2P 兜底 | CF Worker 应用层字节中继 | ICE 失败 / 协商超时自动切换；无需部署 TURN；详见 [`docs/networking/signaling.md`](docs/networking/signaling.md) |
| 文档原则 | 当前态优先 | 专题文档记录现在系统如何工作；历史流水不再作为入口维护 |

---

## 三、当前状态

已落地：

- 浏览器能力前置检测：WebAssembly / WebGPU / OPFS / WebRTC / WebSocket / 指针锁；触屏设备默认拦截
- 单机与 Host 共用 `server` 权威逻辑；Remote 通过 FieldSnapshot、FieldDelta、FreeObjectProject、PlayerTick 同步
- `core::field` 的 `FieldChunk` 已用于 OPFS 存档和网络快照；`core::chunk` 仍作为当前渲染/碰撞适配格式
- `core::block` 已有 MaterialID/MaterialProperties 过渡层；`core::field` 已有 FieldChunk/Column/Span 原型和 Chunk 双向转换，`server::World` 会同步维护 `field_chunks`
- 石砖进入第 9 格 hotbar，世界最低层生成不可破坏基岩
- `ImmediateRelaxation` 软材质已有局部松弛原型：沙/土/草在挖放后由 Host / Local-Only 立即下落或滑落，并通过多条 FieldDelta 同步
- `FloatingOnly` 硬材质已有第一版稳定性：完全浮空的小连通块会提取为 FreeObject、整体下落并投影回静态场
- 渲染主路径为 Skybox → Depth Pre-Pass（可关）→ Opaque → Player → Transparent → Selection → UI
- 网格化使用跨区块面剔除、硬材质贪婪合并、SmoothGranular 高度场平滑提面、AO、index buffer、视锥剔除和分帧任务队列
- OPFS Variant A：主线程 async 存取、周期 flush、手动保存、删档、配额 UI 和严格版本校验

仍需关注：

- OPFS 关闭 Tab 时是尽力保存；若出现可观察丢数据，再升级到 Dedicated Worker + sync handle
- TransparentPass 按 chunk 排序，不做逐面透明排序
- Pass 耗时统计是 CPU 编码耗时，不是 GPU timestamp query
- TURN 凭据下发、触屏操作、声音、主机迁移和更复杂地形属于可选增强

---

## 四、阅读路径

| 场景 | 先读 | 再读 |
|---|---|---|
| 新人了解项目 | [`docs/architecture.md`](docs/architecture.md) | [`docs/roadmap.md`](docs/roadmap.md)、[`docs/reference.md`](docs/reference.md) |
| 改 workspace / 模块边界 | [`docs/architecture.md`](docs/architecture.md) | 对应 [`docs/modules/*.md`](docs/modules/) |
| 改协议 / 联机同步 | [`docs/networking/protocol.md`](docs/networking/protocol.md) | [`docs/networking/signaling.md`](docs/networking/signaling.md)、[`docs/networking/prediction.md`](docs/networking/prediction.md)、[`docs/modules/net.md`](docs/modules/net.md) |
| 改渲染 / 网格化 | [`docs/modules/render.md`](docs/modules/render.md) | [`docs/features/meshing.md`](docs/features/meshing.md)、[`docs/features/ui.md`](docs/features/ui.md) |
| 改物理 / 挖放 | [`docs/features/physics.md`](docs/features/physics.md) | [`docs/modules/server.md`](docs/modules/server.md)、[`docs/modules/client.md`](docs/modules/client.md) |
| 改存档 / 世界加载 | [`docs/features/persistence.md`](docs/features/persistence.md) | [`docs/modules/server.md`](docs/modules/server.md)、[`docs/modules/client.md`](docs/modules/client.md) |
| 改部署 / 本地运行 | [`docs/deployment.md`](docs/deployment.md) | [`signaling/`](signaling/)、[`Caddyfile`](Caddyfile)、[`trunk.toml`](trunk.toml) |

文档目录：

```
docs/
├── architecture.md             系统架构总览（拓扑、角色、帧调度、依赖图）
├── modules/                    每个 crate 的当前内部设计
├── networking/                 协议、信令、预测与插值
├── features/                   网格化、物理、UI、持久化等端到端特性
├── deployment.md               Caddy 静态站 + 信令 Workers 部署 + 构建工具链
├── reference.md                技术栈版本表 + 浏览器 API 约束 + 已知坑
└── roadmap.md                  当前能力、风险和可选增强方向
```

---

## 五、术语速查

| 术语 | 含义 |
|---|---|
| **BlockID** | `u16` 方块标识，0 = 空气 |
| **Chunk / ChunkPos** | 16×256×16 = 65536 个方块的存储单元；`(x, z)` 区块坐标支持负数 |
| **Host / Remote / Local-Only** | 房主（跑权威 Server）/ 非房主玩家 / 单人模式（无网络） |
| **OPFS** | Origin Private File System，浏览器内置“源专属”虚拟文件系统，本项目存档底层 |
| **FieldChunk** | 统一体素存档/网络快照单元，内部为 16×16 column store，可在 span 与 dense cell 列之间切换 |
| **DataChannel** | WebRTC 字节流通道。本项目用两条：`reliable`（FieldSnapshot/FieldDelta/FreeObjectProject/Chat/Join/Leave）与 `unreliable`（60Hz PlayerTick） |
| **Tick / Snapshot** | 服务端 60Hz 逻辑步长 / 新玩家加入时的世界全量快照（分片传输） |
| **Render Graph / Pass** | 多 Pass 渲染调度框架；Pass 即一次 GPU 渲染编码（Depth Pre / Opaque / Skybox / Transparent / UI） |
| **AABB / DDA / AO** | 玩家碰撞体（0.6×1.8）/ 体素射线检测算法 / 顶点级 4 等级环境光遮蔽 |
| **Greedy Meshing** | 贪婪网格化，将相邻同材质方块面合并为大矩形 |
| **AppState** | 客户端状态机：Lobby / Connecting / InGame / Disconnected，并在 InGame 内叠加暂停和聊天状态 |
| **Signaling / STUN / TURN** | SDP/ICE 握手通道 / NAT 公网地址发现 / WebRTC 中继；本项目当前优先使用 Worker 字节中继兜底 |
| **Durable Object** | Cloudflare Workers 的有状态对象，维护房间成员列表和中继对 |
| **trunk** | Rust WASM 项目构建工具，整合 HTML 模板与资源管线 |

---

## 六、给后续 agent 的工作纪律

1. **先读入口，再按范围读专题文档**：修改源代码前必须读本 README；跨多个 crate 时读 [`docs/architecture.md`](docs/architecture.md)，再读对应 `docs/modules/*.md` 和相关 `docs/features/*` / `docs/networking/*`
2. **文档同步**：代码行为、协议、存档 schema、部署方式或用户可见流程变化时，同步更新对应专题文档；新增文档放进 `docs/` 合适子目录并在本 README 登记
3. **文档冲突**：以本 README 决策表为准；其余冲突时 `docs/architecture.md` > `docs/modules/*` > `docs/features/*` / `docs/networking/*` > 其它
4. **代码注释**：复杂图形学、网络和持久化逻辑使用中文注释帮助学习；架构说明放文档，不塞进注释
5. **API 变化**：WGPU、web-sys、Cloudflare Workers 等 API 可能变化，涉及新版 API 时查阅最新官方文档
6. **代码检查**：完成源代码编辑后必须通过 `cargo fmt` 和 `cargo clippy --target wasm32-unknown-unknown --all-targets`；仅改 Markdown 时运行文档引用检查即可
