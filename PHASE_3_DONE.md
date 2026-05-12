# Phase 3 · 物理与交互 · 完成报告

> 完成日期：2026-05-12
> 关联：[`docs/roadmap.md`](docs/roadmap.md) Phase 3

---

## 实际完成项

- ✅ **客户端物理（Walk/Fly 双模式）** [crates/client/src/physics.rs](crates/client/src/physics.rs)
  - Walk：重力（−32 m/s²）+ 跳跃初速度（8.4 m/s）+ 水平 lerp 平滑加速（HORIZ_ACC=12）+ Y/X/Z 分轴扫动碰撞 + 地面探测（脚底 5cm）
  - Fly：直接沿相机方向飞行（FLY_SPEED=12 m/s），速度清零，无重力
  - 碰撞检测 `collides_with_world`：遍历 AABB 覆盖的整数方块，用 `Aabb::intersects` 开区间判定
  - 14 单元测试：重力落地 / 跳跃 / 分轴擦墙滑动 / 地面碰撞 / eye offset
- ✅ **DDA 射线** [crates/client/src/raycast.rs](crates/client/src/raycast.rs)
  - Amanatides & Woo 网格步进算法
  - `RaycastHit { pos, normal: IVec3, face: Face, distance }` — face 复用 `render::vertex::Face`
  - 命中条件为 `block != AIR && properties(block).solid`（玻璃可被命中）
  - 5 单元测试：+X 命中 / −Y 命中 / 超射程 / 跳过非实体 / 零方向
- ✅ **挖放交互闭环** [crates/client/src/lib.rs:dispatch_actions](crates/client/src/lib.rs#L879)
  - 左键持续挖掘（held + 冷却 100ms）→ 发 `Break { pos, request_id }`
  - 右键一次性放置 → 本地 AABB overlap 预检 → 发 `Place { neighbor, block, request_id }`
  - Local 模式跳过乐观 world 写入（client/server 共享 World 会干扰 validate）
  - BlockUpdate 返回后统一触发受影响 chunk 重网格化
- ✅ **PendingActions（rollback 路径）** [crates/client/src/prediction.rs](crates/client/src/prediction.rs)
  - `HashMap<request_id, PendingAction>` + `next_request_id` 单调递增
  - `resolve(id, accepted)`：accepted → 移除；rejected → 返回 backup 供 world 还原
  - 4 单元测试：id 单调 / accepted 消除 / rejected 返还 backup / 未知 id noop
- ✅ **服务端物理仲裁** [crates/server/src/physics.rs](crates/server/src/physics.rs)
  - `validate_break(world, pos, player_feet) → AckReason`：y 越界 / 距离 > 6m / 目标 AIR → 拒绝
  - `validate_place(world, pos, block, player_feet) → AckReason`：y 越界 / 距离 > 6m / 非空 / 与玩家 AABB 重叠 → 拒绝
  - 玩家位置 tracker：`Server::players: HashMap<entity_id, feet_position>`（Hello 插入、PlayerInput 更新）
  - 10 单元测试：范围 / y 越界 / 重叠 / 空方块拒绝
- ✅ **Server handle_message 扩展** [crates/server/src/lib.rs](crates/server/src/lib.rs#L52)
  - `Hello` → 插入 `players[LOCAL_ENTITY_ID] = DEFAULT_SPAWN` + 返回 Welcome
  - `PlayerInput` → 更新 `players` 表，无 reply
  - `Break` / `Place` → 调 validate，Ok 才 set_block + 广播 BlockUpdate；否则仅 ActionAck(rejected)
  - 4 集成测试：Hello 落表 / PlayerInput 更新 / Break 成功 / Place 重叠拒绝
- ✅ **Hotbar** [crates/client/src/hotbar.rs](crates/client/src/hotbar.rs)
  - 9 格默认：`[STONE, DIRT, GRASS, SAND, WOOD, LEAVES, GLASS, WATER, STONE]`
  - 1-9 数字键切换选中格（`InputState::hotbar_request` 边沿）
  - HUD 9 格横排渲染，选中格金色高亮
  - 3 单元测试：默认选中 / 切换 / 越界忽略
- ✅ **选中方块线框** [crates/render/src/passes/selection.rs](crates/render/src/passes/selection.rs)
  - Line-list pipeline（12 边 × 2 端点 = 24 vertices）
  - 独立 shader `selection.wgsl`：顶点 + block_origin 变换，片元输出半透明黑（α=0.85）
  - 深度测试 `LessEqual`，`depth_write_enabled = false`；alpha blend `OneMinusSrcAlpha`
  - `Renderer::render_selection(&mut self, encoder, color_view, view_proj, block_pos: Option<Position>)`
- ✅ **输入系统增强** [crates/client/src/input.rs](crates/client/src/input.rs)
  - 修复右键 mouse button 映射：`0=左键` / `2=右键` / `1=中键忽略`
  - 引入 `_held` / `_just_pressed` 边沿语义（jump / break / place）
  - 双击空格检测：`last_space_press_at_ms: Option<f64>`，250ms 窗口内两次按下 → `fly_toggle_pending`
  - `Digit1..=Digit9` → `hotbar_request: Some(0..=8)`
  - 帧末 `reset_delta` 清所有边沿，保留持续按下状态
- ✅ **受影响 chunk 重网格化** [crates/client/src/chunk_loader.rs:affected_chunks](crates/client/src/chunk_loader.rs#L106)
  - 返回方块自身 chunk + 邻居（若在 x/z 边界）含对角 chunk（至多 4 个）
  - 3 单元测试：内部 1 个 / 边界 2 个 / 角点 4 个 / 负坐标
- ✅ **HUD 扩展** [crates/client/src/lib.rs:draw_hud](crates/client/src/lib.rs#L1019)
  - 左上角 stat 加 MODE（Walk/Fly）+ [ground] 标识
  - 底部双行：提示栏 + 9 格 Hotbar（选中格金色高亮）

---

## 共享基础设施

- ✅ **AABB 几何模块** [crates/core/src/geometry.rs](crates/core/src/geometry.rs)
  - `Aabb { min, max }` + `block_at(pos)` + `intersects`（开区间）
  - `player_aabb(feet)` — PLAYER_WIDTH=0.6 / PLAYER_HEIGHT=1.8 / EYE_OFFSET=1.62 常量
  - client physics 与 server physics 共用同一套定义，杜绝参数漂移
  - 5 单元测试：block / player / overlap / touch / neg

---

## 关键文件改动

| 文件 | 改动 |
|---|---|
| [crates/core/src/geometry.rs](crates/core/src/geometry.rs) | **新建**：Aabb + player_aabb + intersects |
| [crates/core/src/lib.rs](crates/core/src/lib.rs) | `pub mod geometry;` + re-export |
| [crates/client/src/physics.rs](crates/client/src/physics.rs) | **重写**：LocalPhysics + Walk/Fly + 分轴碰撞 |
| [crates/client/src/raycast.rs](crates/client/src/raycast.rs) | **重写**：DDA 算法实现 |
| [crates/client/src/input.rs](crates/client/src/input.rs) | 修鼠标映射 + 边沿语义 + 双击 + 1-9 |
| [crates/client/src/camera.rs](crates/client/src/camera.rs) | 删 apply_fly_input + 加 forward_horizontal |
| [crates/client/src/prediction.rs](crates/client/src/prediction.rs) | **重写**：PendingActions 替换 InputHistory |
| [crates/client/src/hotbar.rs](crates/client/src/hotbar.rs) | **新建**：Hotbar + block_label + 测试 |
| [crates/client/src/chunk_loader.rs](crates/client/src/chunk_loader.rs) | + affected_chunks 工具函数 + 3 测试 |
| [crates/client/src/app.rs](crates/client/src/app.rs) | Game + 6 字段；GameSettings + min_action_interval_ms |
| [crates/client/src/lib.rs](crates/client/src/lib.rs) | 主循环集成：物理/射线/动作分发/Ack/HUD/wireframe/PlayerInput |
| [crates/server/src/lib.rs](crates/server/src/lib.rs) | + players HashMap + PlayerInput 处理 + validate 接入 + 4 测试 |
| [crates/server/src/physics.rs](crates/server/src/physics.rs) | **重写**：validate_break/place + 10 测试 |
| [crates/render/src/passes/selection.rs](crates/render/src/passes/selection.rs) | **新建**：line-list pipeline + cube vbuf |
| [crates/render/src/shaders/selection.wgsl](crates/render/src/shaders/selection.wgsl) | **新建**：vs position 变换 + fs 半透明黑 |
| [crates/render/src/passes/mod.rs](crates/render/src/passes/mod.rs) | `pub mod selection;` |
| [crates/render/src/lib.rs](crates/render/src/lib.rs) | + selection_pass + render_selection |
| [crates/render/src/device.rs](crates/render/src/device.rs) | cfg gate desktop target（让单元测试可跑） |

---

## 验证

| 项 | 标准 | 实测 |
|---|---|---|
| `cargo fmt --check` | 无 diff | ✅ |
| `cargo clippy --workspace --lib -- -D warnings` | 无警告 | ✅ |
| `cargo test --workspace --lib` | 全通过（87 个） | ✅ |
| `cargo check --target wasm32-unknown-unknown --workspace` | 编译通过 | ✅ |
| `trunk serve` 浏览器 Walk 走动/跳跃/攀爬 | 不穿墙、不卡住 | 待人工验证 |
| 左键挖方块 → 消失 → 下层露面 | 重网格化生效 | 待人工验证 |
| 右键放方块 → chunk 边界邻居补面 | 跨区块重网格化 | 待人工验证 |
| 1-9 hotbar 切换 → 选中格高亮 | 视觉反馈 | 待人工验证 |
| 双击空格 Fly ↔ Walk 切换 | 手感变化 | 待人工验证 |
| 选中方块线框 | 半透明黑边浮在方块上 | 待人工验证 |
| 挖掘性能 60s | FPS ≥ 50 | 待人工验证 |

---

## 关键设计决策

1. **Local 模式不做乐观更新 — 等 BlockUpdate 再重 mesh**
   Local 下 client 和 server 共享同一份 `World`，乐观 `set_block` 会污染 server 的 `validate` 读取（server 看到 AIR 就误判 BlockNotEmpty 拒绝）。改为 client 拍快照 backup 入 `PendingActions`，发消息给 server；server 做出仲裁后由 `BlockUpdate` 触发重网格化。Phase 5 Remote 端引入独立 `WorldView` 时再加乐观路径。

2. **Server 射程 + overlap 校验 Phase 3 顺手实装**
   原按 roadmap 视为 Phase 5 任务，但代码量仅 ~80 行且为"顺手实装范围+overlap"的用户选择。新增 `Server::players: HashMap<u32, Vec3>` 最小 tracker（仅存 feet），Hello 插入初始位置，PlayerInput（60Hz）每次更新。Phase 5 扩展为完整 PlayerSnapshot 表时复用此字段。

3. **AABB 工具放到 `voxweb_core::geometry`**
   client physics 和 server physics 共享同一套碰撞定义，避免两边复制粘贴导致参数漂移（如 width 改 0.7 只改了一边）。

4. **`Renderer::render_selection` 独立 Pass 而非 egui 投影**
   线框走 GPU 深度测试，在世界空间正确显示（被前景遮挡时隐去、共面时可见）。egui Painter 屏幕空间投影虽代码量更小但无法做深度测试。

5. **Camera 职责收窄**
   `Camera::position` 由 `LocalPhysics` 每帧 `eye_position()` 覆盖；相机只管理朝向和矩阵。原 `apply_fly_input` 删除，Fly 移动逻辑归入 `LocalPhysics::step_fly`。

---

## 已知问题 / 后续

1. **未跑浏览器人工验收** — 必须 `trunk serve` 在 WebGPU 环境下确认物理手感、挖放视觉、hotbar 交互、线框渲染。
2. **离散碰撞隧道** — 位移 > 1 方块/帧时可能穿过薄墙（1 方块厚）。当前 60Hz 下 WALK_SPEED=4.3 每帧 0.07m，不会发生。若未来加速药水/疾跑提高速度过快，需补 sweep-based 扫动检测。
3. **连续跳跃无冷却** — 当前 Space 边沿即可再跳，无跳跃冷却（与 Minecraft 1.9 前一致）。无计划改动。
4. **`Aabb::intersects` 触面不算碰撞** — 玩家贴墙站时 `max.x == wall_min.x` 不触发碰撞，保证滑动不卡。但极端精度（f32 EPSILON 级）下可能让玩家微微嵌入方块表面 1e-7 米量级——肉眼不可见，暂不处理。
5. **PlayerInput 用 server.tick** — Phase 5 应切到客户端本地 tick 计数（与 prediction 协调对齐），Phase 3 单机模式同进程共享 tick 无所谓。
6. **Hotbar 仅文本标签** — Phase 6 可升级为图标 + 数字叠加（egui ImageButton）。

---

## 下一步：Phase 4 · P2P 通道

入口文档：[`docs/networking/signaling.md`](docs/networking/signaling.md) · [`docs/networking/protocol.md`](docs/networking/protocol.md)

要点：
- `signaling/` Cloudflare Workers + Durable Object 完整实现
- Rust 端 WebSocket 客户端 + PeerConnection + DataChannel（双通道）
- 大厅"创建房间 / 加入房间" UI
- 简单 ping-pong 验证 RTT
