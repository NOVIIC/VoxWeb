# 路线图

> **何时阅读**：决定下一步做什么；估排期；评估某 Phase 是否完成
> **关联文档**：[`../README.md`](../README.md) · [`architecture.md`](architecture.md) · 各 `modules/*` · 各 `features/*`

---

## 总览

```
Phase 0 ─▶ Phase 1 ─▶ Phase 2 ─▶ Phase 3 ─▶ Phase 4 ─▶ Phase 5 ─▶ Phase 6 ─▶ Phase 7 ─▶ Phase 8 ─▶ Phase 9 (stretch)
脚手架      渲染骨架     体素单人     物理交互     P2P 通道     权威同步     多人 UI      渲染优化     多 Pass+存档   后处理/TURN/触屏
  ✅          ✅          ✅          ✅          ✅          ✅          ✅          ✅          ✅
```

每个 Phase **可独立验收**：完成时可以演示一个具体的、可观察的功能。后续 Phase 只增不破。

---

## Phase 0 · 脚手架 ✅

### 目标
创建项目骨架，让一个空 canvas + Hello World UI 在浏览器跑起来。

### 涉及文档
- [`architecture.md`](architecture.md)（Workspace 结构）
- [`deployment.md`](deployment.md)（trunk 配置、本地命令、Caddyfile）

### 任务清单
- [x] 创建 `Cargo.toml` workspace + 五个 crate 的空骨架
- [x] `start.html` + `trunk.toml` 跑通 `trunk serve`（landing 页 `index.html` 经 `data-trunk rel="copy-file"` 同步复制）
- [x] `client::start` 创建一个空 wgpu Surface 并清屏纯色
- [x] 集成 egui，绘制"Hello VoxWeb"在屏幕中央
- [x] 设置 `console_error_panic_hook` + `tracing-wasm`
- [x] CI：`cargo check --target wasm32-unknown-unknown` + `cargo fmt --check`
- [x] `signaling/` TS 项目初始化（空 worker，仅返回 200）
- [x] **浏览器能力前置检测（wasm 加载之前）**：在 `index.html` / `start.html` 内联检测脚本（< 2 KB gz），检查 WebAssembly / WebGPU / OPFS / WebRTC / WebSocket / 指针锁；缺失项展示降级页面并阻止 wasm 加载；仅 WebRTC 缺失时提供"仅单机模式"入口（`window.__VOXWEB_FORCE_LOCAL_ONLY = true`）。详见 [`reference.md` §浏览器能力前置检测](reference.md#浏览器能力前置检测wasm-加载之前) 与 [`features/persistence.md` §十五](features/persistence.md#十五浏览器能力前置检测)。回填日期：2026-05-12（见 [PHASE_0_DONE.md](../PHASE_0_DONE.md) §回填）

### 验证
- `trunk serve` → 浏览器打开 http://localhost:8080 → 看到纯色背景 + 居中文字
- 控制台无错误
- WASM gz 体积 < 1.5 MB
- **不支持浏览器（如 Firefox stable 缺 WebGPU）访问时看到友好提示，且 `.wasm` 不被下载（Network 面板可验证）**
- **加 `?force=1` query 可跳过检测进入游戏（开发模式用）**

### 可能踩的坑
- wgpu Surface 在 canvas 未挂载到 DOM 时初始化会失败 → 等 RAF 第一次回调再创建
- egui 中文字体未注入 → 临时只显示英文

---

## Phase 1 · 渲染骨架 ✅

### 目标
让一个 chunk（手工填充的几个方块）出现在屏幕上，相机能用 WASD + 鼠标控制视角。

### 涉及文档
- [`modules/render.md`](modules/render.md)
- [`modules/client.md`](modules/client.md)（camera / input / 主循环）
- [`modules/core.md`](modules/core.md)（BlockID / Chunk / Position）

### 任务清单
- [x] `core::block` + `core::chunk` 完整定义
- [x] `render::vertex` 实现 u32 压缩顶点（先实现编码 + WGSL 解码）
- [x] `render::passes::opaque` 单 Pass：纯色 + 顶点 packed
- [x] `client::camera` 第一人称相机（Fly 模式）
- [x] `client::input` 键盘 + 鼠标 movement
- [x] 指针锁集成（点击 canvas 进入；ESC 退出）
- [x] 手工填充一个 16×16×4 的方块阵列，渲染显示
- [x] HUD：FPS + 坐标显示

### 验证
- 在浏览器内可以用 WASD + 鼠标自由飞行
- 看到一片彩色方块（不同 BlockID 不同颜色）
- 60 fps 稳定
- 退出/进入指针锁正常

---

## Phase 2 · 体素单人 ✅

> 完成日期：2026-05-12 · 详见 [`../PHASE_2_DONE.md`](../PHASE_2_DONE.md)

### 目标
单机模式可玩：地形生成、动态加载、跨区块剔除。

### 涉及文档
- [`modules/server.md`](modules/server.md)
- [`features/meshing.md`](features/meshing.md)
- [`modules/client.md`](modules/client.md)（mesh_jobs）
- [`modules/net.md`](modules/net.md)（NetEndpoint::Local mpsc）

### 任务清单
- [x] `server::terrain` Perlin 高度图地形生成
- [x] `server::world` Chunk 表 + tick
- [x] `server::Server::handle_client_message` 基础 dispatch（无网络，仅本地 enum 转换）
- [x] `client::NetEndpoint::Local` 实现内存通道
- [x] `render::chunk_mesh` 朴素逐面网格化（先不上贪婪算法）
- [x] 跨区块面剔除（`generate_with_neighbors` + 世界坐标回调）
- [x] `client::mesh_jobs` 优先级队列 + 分帧 budget
- [x] 渲染距离动态加载/卸载（玩家走远了 chunk 释放）
- [x] 大厅 UI：单机模式按钮 → 进入游戏

### 验证
- 大厅点"单机模式"→ 进入游戏 → 看到连绵地形（草/泥/石分层）
- 飞过去能持续看到新地形加载
- 走远后回头，原地形仍在
- 渲染距离调到 6 时仍 60 fps（中等设备）
- 区块边界处无明显漏面或多余面

---

## Phase 3 · 物理与交互 ✅

> 完成日期：2026-05-12 · 详见 [`../PHASE_3_DONE.md`](../PHASE_3_DONE.md)

### 目标
玩家有物理身体，能跳、能挖、能放。

### 涉及文档
- [`features/physics.md`](features/physics.md)
- [`networking/protocol.md`](networking/protocol.md)（Break/Place 消息流）

### 任务清单
- [x] `client::physics` AABB + 重力 + 跳跃 + 分轴碰撞
- [x] Walk / Fly 模式切换（双击空格）
- [x] `client::raycast` DDA 算法
- [x] HUD 选中方块线框
- [x] 鼠标左键挖（本地直接修改 + send Break）
- [x] 鼠标右键放（计算邻居位置 + send Place）
- [x] `server::physics::validate_break/place` 仲裁
- [x] BlockUpdate 闭环：本地 server 处理 → 客户端收到 → 重网格化
- [x] 1-9 键 hotbar（hotbar UI HUD 简化版）
- [x] ActionAck rollback 路径（即使本地一定通过，也要走完逻辑）

### 验证
- 单机模式下走路、跳跃、攀爬上小山坡
- 不会穿墙或卡住
- 左键能挖任何方块
- 右键能放（不在身体里）
- 1-9 切换不同方块
- 挖完一个方块下层方块面立即正确显示

---

## Phase 4 · P2P 通道 ✅

> 完成日期：2026-05-13 · 详见 [`../PHASE_4_DONE.md`](../PHASE_4_DONE.md)

### 目标
让两个浏览器 Tab 之间能传字节。先不接入服务端逻辑，只验证 WebRTC + 信令 + DataChannel 工作正常。

### 涉及文档
- [`modules/net.md`](modules/net.md)
- [`networking/signaling.md`](networking/signaling.md)
- [`networking/protocol.md`](networking/protocol.md)（消息格式）

### 任务清单
- [x] `signaling/` Cloudflare Workers + Durable Object 完整实现
- [x] `wrangler dev --local` 本地跑通
- [x] `net::signaling` Rust 端 WebSocket 客户端 + Closure 事件回调
- [x] `net::peer` PeerConnection + DataChannel（双通道）
- [x] `net::room` 状态机
- [x] `net::NetEndpoint::Host` / `::Remote` 实装
- [x] 大厅"创建房间" / "加入房间" UI（Phase 1 实装的简化版扩展）
- [x] 调试：connecting UI 显示进度
- [x] 简单消息 ping-pong：Remote 发 `Ping{client_time_ms}`，Host 回 `Pong`，HUD 显示 RTT

### 验证
- 同一台机两个 Tab：A 创建房间 abc123，B 加入 abc123
- 双方 connecting UI 走完流程，进入 InGame（暂时只显示空地形）
- HUD 显示 RTT < 50ms（本地）
- chrome://webrtc-internals 中看到双通道开启
- 关掉 wrangler dev，已建连的两 Tab 仍能互发 Ping

---

## Phase 5 · 主机权威同步

### 目标
两个 Tab 看到同一个世界。Host 是权威，Remote 通过快照 + tick 同步。

### 涉及文档
- [`modules/server.md`](modules/server.md)（add_player, send_initial_snapshot）
- [`networking/protocol.md`](networking/protocol.md)
- [`networking/prediction.md`](networking/prediction.md)
- [`features/persistence.md`](features/persistence.md)

### 任务清单
- [x] `core::protocol` 完整消息定义 + bincode 配置（消息枚举 + PROTOCOL_VERSION + Recipient/OutboundMessage）
- [x] Hello/Welcome 握手（add_player 分配 eid，Welcome 含 seed + eid）
- [x] ChunkSnapshot 分片传输 + 接收端组装（ChunkAssembler）
- [x] PlayerInput / PlayerTick 60Hz 收发
- [x] `client::prediction` 自身位置预测协调（InputHistory + reconcile_self）
- [x] `client::interp` 远端玩家位置插值（PlayerInterp, delay=100ms）
- [x] BlockUpdate 广播闭环（Remote 操作 → Host 仲裁 → 全员看到）
- [x] Action Ack 协调
- [x] PeerJoined / PeerLeft 玩家进出广播
- [x] 简单玩家身体渲染（PlayerPass：实心 AABB cube，颜色 entity_id 派生）
- [ ] **基础 OPFS 持久化** — 整体延后到 Phase 8

### 验证
- A 创建房间 → B 加入
- B 看到 A 站立位置（box）
- A 走动，B 看到平滑移动（不抖）
- B 挖一个方块，A 看到方块消失
- A 关闭 Tab → B 收到"主机断开"，回到大厅
- A 重新进入相同 room_id + seed → 看到上次挖掉的方块仍是空气

---

## Phase 6 · 多人 UI/HUD ✅

> 完成日期：2026-05-21 · 详见 [`../PHASE_6_DONE.md`](../PHASE_6_DONE.md)

### 目标
让多人体验完整：知道谁在线、能聊天、看到玩家名字。

### 涉及文档
- [`features/ui.md`](features/ui.md)
- [`networking/protocol.md`](networking/protocol.md)（Chat 消息）

### 任务清单
- [x] HUD 玩家列表 widget
- [x] T 键打开聊天框
- [x] 聊天历史 + 系统消息（Join/Leave）
- [x] 平时显示最近 5 条聊天浮窗（5 秒淡出）
- [x] 远端玩家头顶名牌（egui Painter billboard）
- [x] 距离衰减（> 32m 不显示）
- [x] EscMenu 完整设置：FOV / 灵敏度 / 渲染距离 / 插值延迟 / 显示统计
- [x] 设置存到 localStorage
- [x] Disconnected 页面 + "返回大厅"按钮

### 验证
- 4 人同房间，HUD 右上角列表显示 4 行
- 玩家头顶看到名字，距离够远会自动消失
- T 打字、Enter 发送、其他玩家收到
- 系统消息 "Bob 加入了房间" 准确出现
- ESC 菜单调整 FOV，画面立即响应

---

## Phase 7 · 渲染优化 ✅

> 完成日期：2026-05-31 · 详见 [`../PHASE_7_DONE.md`](../PHASE_7_DONE.md)

### 目标
让渲染距离能开到 8-10 仍然 60 fps。

### 涉及文档
- [`features/meshing.md`](features/meshing.md)

### 任务清单
- [x] 贪婪网格化算法（替换 Phase 2 的朴素逐面）
- [x] 顶点压缩（Phase 1 已写位段编码，此处确认全链路用上）
- [x] AO 计算（顶点级 4 等级）
- [x] mesh_jobs 优先级队列优化（玩家附近 critical / high 优先）
- [x] 视锥剔除（每帧根据 camera frustum 过滤 chunk）
- [x] 网格化分帧 budget tuning（默认 4ms/帧）
- [x] 性能 stat HUD（每个 Pass 耗时）

### 验证
- 渲染距离 8、复杂地形、4 人房间、稳定 60 fps（M2 mac）
- 贪婪合并后顶点数比 Phase 2 减少 80%+（HUD stat 显示）
- 远处看不到 chunk 边界缝隙
- AO 在角落产生柔和阴影

---

## Phase 8 · 多 Pass + 存档完善 ✅

> 完成日期：2026-06-01 · 详见 [`../PHASE_8_DONE.md`](../PHASE_8_DONE.md)

### 目标
渲染管线优雅化（多 Pass）；存档体验完整。

### 涉及文档
- [`modules/render.md`](modules/render.md)（Render Graph）
- [`features/persistence.md`](features/persistence.md)

### 任务清单
- [x] `render::graph::RenderGraph` + `RenderPass` trait
- [x] Depth Pre-Pass（可关）
- [x] Skybox Pass（程序化天空 + 太阳方向）
- [x] Transparent Pass（水/玻璃 alpha blend）
- [x] UI Pass 重构为 trait 实现
- [x] 透明方块网格独立 buffer + 距离排序
- [x] **存档完善**：
  - `crates/server/src/world.rs` 引入 LRU + pinned 集合（capacity 4096，runtime 可调）
  - 暂停菜单"立即保存"按钮 + 配额 UI（使用量、> 80% 警告、> 95% 暂停 dirty）
  - `navigator.storage.persist()` 启动时申请
  - 协议 migration 框架（`storage_version` + `migrations[]` 数组，本期含 identity）
  - 加载失败错误处理 + 删档功能（不再因版本不匹配强制删档）
- [ ] （可选）Variant B Worker + sync handle 升级：若 Phase 8 Variant A 上线后出现可观察的"关 Tab 丢数据"投诉，新增 `crates/client/src/storage_worker.rs` 走 `FileSystemSyncAccessHandle`；详见 [`features/persistence.md` §十二](features/persistence.md#十二variant-a-vs-variant-bworker)

### 验证
- 看到清晰的天空（地平线渐变 + 太阳）
- 水透明，能看到下面的方块
- Depth Pre-Pass 开/关切换不影响画面，但 stat 显示帧时变化
- 存档使用量准确显示
- 手动注入 10000 dirty chunk（控制台 `voxwebDebug.fillDirty(10000)`）→ OPFS 占用 < 200 MB；启动重连 < 3s；运行内存 < 500 MB
- 强制删档后重进世界为初始地形
- 修改 `world.json.storage_version` 为 999 → 大厅显示"需升级 VoxWeb" 而非删档

---

## Phase 9 · Stretch（不在硬性目标内）

可选增强项，按时间和需求选做：

### 9.1 后处理 Pass
- 雾（distance fog，远处淡出）
- 色调映射（HDR → LDR）
- Bloom（v3）

### 9.2 TURN 中继 fallback

**应用层字节中继 ✅（2026-05-21 实装，详见 [`networking/signaling.md`](networking/signaling.md) §九）**
- ICE 失败 / 15s 协商超时 → Host 主动发 `relay_request`
- 信令 Worker DO 维护 `relayPairs`，在 WS 上做二进制帧字节级转发
- 客户端 HUD 显示「RELAY n」徽标
- Per-peer 粒度：仅升级失败的对，其它直连不受影响

**剩余项（仍为 stretch）**：
- 信令服务下发 TURN 凭据（让 WebRTC 走 TURN，保留 unreliable 语义）
- TURN/字节中继并存策略：先试 TURN，TURN 也失败再退到字节中继

### 9.3 触屏 / 移动端
- 屏幕左右虚拟摇杆（左：移动，右：视角）
- 简化 UI（按钮放大）
- 跳跃/挖/放按钮

### 9.4 声音
- 走路 / 挖方块音效（`web-sys::AudioContext`）
- BGM（可选）
- 远端玩家位置感

### 9.5 其它
- 可迁移主机（Host 退出时投票选新 Host + 同步状态）
- 地形群系（沙漠/雪地/海洋）
- 简易光照（顶级方块发光的 light propagation）
- 录制 / 回放（世界历史命令日志）

---

## 进度追踪建议

每个 Phase 完成后：
1. 在仓库根创建 `PHASE_N_DONE.md`，列出实际完成项 + 已知问题
2. 更新本文档对应 Phase 标题加 `✅`
3. 在 [`../README.md`](../README.md) 决策表新增"当前 Phase"行
4. （可选）git tag `phase-N`

---

## 不在路线图中

- Phase 100：完美主义级别的视觉效果（PBR 材质、屏幕空间反射、体积光）
- Phase X：Mod 系统
- Phase X：移动端原生 App（用 Tauri/Capacitor 包装）
- Phase X：Steam / 应用商店发布
- Phase X：服务端中央化（违背 P2P 设计）
