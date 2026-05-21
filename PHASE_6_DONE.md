# Phase 6 · 多人 UI / HUD — 完成报告

> 完成日期：2026-05-21
> 设计文档：[`docs/features/ui.md`](docs/features/ui.md) · [`docs/networking/protocol.md`](docs/networking/protocol.md)

---

## 目标回顾

把 Phase 5 的"能联机"升级为"完整可玩"：玩家列表、聊天、3D 名牌、暂停设置菜单（localStorage 持久化）、断开页面。让 4 人房间的社交体验达到可演示级别。

---

## 实装清单

### 1. 协议层（`voxweb-core`）

- [`crates/core/src/protocol.rs`](crates/core/src/protocol.rs)：
  - `PROTOCOL_VERSION: u32 = 2`（v1 → v2）
  - `ServerMessage::Welcome` 扩展：`host_entity_id: u32` + `players: Vec<PlayerEntry>`
  - 新增 `pub struct PlayerEntry { entity_id: EntityId, display_name: String }`；re-export 于 `core::lib.rs`
  - 3 个新测试：`protocol_version_is_two`、`roundtrip_welcome_with_roster`；旧 Welcome roundtrip 更新

### 2. Server 层（`voxweb-server`）

- [`crates/server/src/lib.rs`](crates/server/src/lib.rs)：
  - `Server` 加字段 `host_entity_id: Option<EntityId>`（首次 `add_player` 时设）+ 读方法 `host_entity_id()`
  - `add_player` 重写：Welcome 携带 `host_entity_id` + 完整 roster（含新人）；移除 Phase 5 的"逐个 PeerJoined 补发"逻辑
  - Chat 处理：`chars().count() > 256` 静默丢弃 + `chat_window: HashMap<eid, VecDeque<u32>>` 速率限制（5 条 / 180 tick ≈ 3s 滑窗）
  - 5 个新测试：`host_eid_set_on_first_add_player`、`welcome_carries_full_roster_and_host_eid`、`chat_drops_messages_over_256_chars`、`chat_drop_counts_unicode_scalars_not_bytes`、`chat_rate_limit_drops_after_5_per_3s`

### 3. Client 数据模型（`voxweb-client`）

- [`crates/client/src/app.rs`](crates/client/src/app.rs)：
  - `AppState` 重构为 `InGame { paused: bool, chat_open: bool }`（移除 `EscMenu`、`ChatOpen` unit variant）；`Disconnected` 保留
  - `is_in_game()` 和 `ingame_default()` helper
  - `GameSettings` → `AppSettings`，新用户字段 `fov_degrees` / `mouse_sensitivity` / `render_distance` / `interp_delay_ms` / `show_stats`（serde 序列化）；开发字段 `#[serde(skip)]`
  - `Game` 新增 `display_name: String`、`host_entity_id: EntityId`、`chat: ChatHistory`
  - `Game::apply_settings()` 同步 FOV / 插值延迟 / 渲染距离到运行时组件
  - `entity_color` 升级为 `pub fn`，供 player list 使用
  - 9 个新测试：AppState is_in_game / subflag / ingame_default / 等式不变量；AppSettings 默认值 / PartialEq / dev-field 忽略

- [`crates/client/src/chat.rs`](crates/client/src/chat.rs)（新建）：
  - `ChatHistory { cap: 100, input_buffer, .. }`；`ChatKind::{User, System}`；`ChatMessage { kind, content, received_at_ms }`
  - `push_user` / `push_system` / `recent(n)` / `recent_within(now, window, n)`
  - 5 个单测

- [`crates/client/src/settings_storage.rs`](crates/client/src/settings_storage.rs)（新建）：
  - `load() -> Option<AppSettings>` / `save(&AppSettings)`；键 `"voxweb.settings.v1"`
  - 纯函数 `serialize` / `deserialize` 暴露给单测；schema 不匹配 → None
  - 5 个单测

- [`crates/client/src/chunk_loader.rs`](crates/client/src/chunk_loader.rs) 新增 `set_render_distance(u32)` 方法
- [`crates/client/src/interp.rs`](crates/client/src/interp.rs) 新增 `set_delay_ms(f64)` 方法

### 4. Client UI 模块

- [`crates/client/src/ui/players.rs`](crates/client/src/ui/players.rs)（替换 stub）：
  - `PlayerListEntry` / `NameplateEntry` / `draw_player_list` / `draw_nameplates`
  - 纯函数 `project_world_to_screen`（wgpu NDC 0..1）和 `nameplate_alpha`（≤24m full, >32m hidden, 线性过渡）
  - 6 个单测

- [`crates/client/src/ui/chat.rs`](crates/client/src/ui/chat.rs)（替换 stub）：
  - `ChatUiAction { None, Submit(String), Cancel }` / `draw_chat_window` / `draw_recent_overlay`
  - 5s 浮窗 + 1.5s 淡出；Enter 提交，ESC 取消

- [`crates/client/src/ui/pause.rs`](crates/client/src/ui/pause.rs)（替换 stub）：
  - `PauseAction { None, Resume, ExitToLobby }` / `draw_pause_menu`
  - FOV / 灵敏度 / 渲染距离 / 插值延迟 / show_stats；省略 Depth Pre-Pass（Phase 8）

- [`crates/client/src/ui/disconnected.rs`](crates/client/src/ui/disconnected.rs)（新建）：
  - `DisconnectedAction { None, BackToLobby }` / `draw_disconnected`
  - 显示断开原因 + "返回大厅"按钮

### 5. Client 主循环集成（`lib.rs`）

- AppState match 站点全面迁移：`state == AppState::InGame` → `state.is_in_game()`；赋值 → `ingame_default()`
- 新增 `render_disconnected_frame`（纯 egui，不走 wgpu world pass）
- `apply_room_event::Disconnected` 从直接回 Lobby 改为 `AppState::Disconnected`
- Welcome v2 处理：解析 `host_entity_id` + `players`，预填充 `remote_players` 表 + 设 `host_entity_id`
- Chat 历史挂钩：PeerJoined → push_system "X 加入了房间"；PeerLeft → "X 离开了房间"；Chat → push_user
- Lobby：`LobbyState` 加 `display_name: String` 输入框 + nickname UI 行；`LobbyAction` 携带 display_name
- `start_*` 系列从 `settings_storage::load().unwrap_or_default()` 取设置
- 输入分流：paused / chat_open 时键盘转发给 egui；ESC 优先级 chat → pause → enter-pause；T 仅无叠加层时打开聊天
- HUD 集成：player list（右上有色圆点 + 角色后缀）、nameplates（foreground painter billboard）、chat overlay、chat window（条件）、pause menu（条件）
- `HudData` 加 `show_stats: bool` 字段，左上角统计面板条件化
- 新增 `send_chat(game, content)` helper

---

## 测试覆盖

| 维度 | 位置 | 测试数 |
|---|---|---|
| core protocol | `crates/core/src/protocol.rs` | 27（+3 Phase 6） |
| server | `crates/server/src/lib.rs` | 33（+5 Phase 6） |
| net | `crates/net/src/lib.rs` + `transport.rs` | 39（不变） |
| render | `crates/render/src/lib.rs` + `passes/` | 9（不变） |
| client app/chat/settings/players | 多个 | 89（+44 Phase 6） |
| **合计** | | **197**（Phase 5: 139；+58） |

---

## 设计取舍

| 决策 | 选择 | 理由 |
|---|---|---|
| AppState 升级为子状态位 | `InGame { paused, chat_open }` 替代 3 个 unit variant | HUD 永远绘制，暂停与聊天可并存，与 ui.md 文档一致 |
| Welcome v2 带 roster vs PeerJoined backfill | Welcome 携带 `host_entity_id` + `players` | Remote 一次性建好玩家表，无"Host 无名"瞬态；PROTOCOL_VERSION 升至 2 |
| 鼠标灵敏度 | 用户面向的倍率 0.1..=5.0（乘 BASE=0.0025） | 0.0025 弧度/像素 对一般玩家不直观；1.0 默认 + 简单倍数更友好 |
| Chat 速率限制时钟 | `self.tick` 而非 `current_time_ms` | Local-Only 模式也跑完整速率限制逻辑 |
| 本地聊天回显 | 不本地 push，等 Server Chat 回到自己 | 避免本地 + 服务端双重 push；Phase 5 的 `Recipient::All` 包含本人 |
| Depth Pre-Pass 设置项 | 省略（Phase 8 再加） | 功能未实装，死复选框会误导 |
| Disconnected 为独立页面 | 保留 `AppState::Disconnected`，不直接回 Lobby | 显示原因 + 明显 CTA 比静默回大厅好 |
| Pin 码 / 安全 | 不做 | 纯体素沙盒游戏，DTLS 加密足够 |

---

## 已知限制 / 留给 Phase 7-8

- 名牌无深度遮挡（Phase 8 读深度纹理做半透明处理）
- show_stats 现仅 gate 左上角统计面板；未扩展为多级 verbosity（F3 切）
- 聊天框无"复制文本"能力（浏览器环境自带 select+copy，非刚性需求）
- 渲染距离热切换不立即生效——`ChunkLoader::invalidate` 让下一帧识别但加载新 chunk 有延迟
- 暂停菜单无"断开连接"按钮（关闭 Tab 即可，非刚性）

---

## 文件改动概要

| 文件 | 性质 |
|---|---|
| `crates/core/src/protocol.rs` | 改写：PROTOCOL_VERSION=2 / Welcome 扩展 + PlayerEntry 新增 |
| `crates/core/src/lib.rs` | re-export PlayerEntry |
| `crates/server/src/lib.rs` | 改写：host_entity_id / add_player / chat 处理 + 5 个新测试 |
| `crates/client/src/app.rs` | 改写：AppState 重构 / AppSettings 改名扩展 / Game 加 display_name/host_entity_id/chat + 7 个新测试 |
| `crates/client/src/lib.rs` | 改写：match 站点迁移 / Welcome v2 / chat hook / 输入分流 / HUD 集成 / send_chat |
| `crates/client/src/chat.rs` | **新增**：ChatHistory + 5 测试 |
| `crates/client/src/settings_storage.rs` | **新增**：localStorage 持久化 + 5 测试 |
| `crates/client/src/interp.rs` | 加 `set_delay_ms()` |
| `crates/client/src/chunk_loader.rs` | 加 `set_render_distance()` |
| `crates/client/src/ui/players.rs` | **重写**：玩家列表 + 名牌 + 投影函数 + 6 测试 |
| `crates/client/src/ui/chat.rs` | **重写**：聊天窗口 + 浮窗叠加层 |
| `crates/client/src/ui/pause.rs` | **重写**：暂停设置菜单 |
| `crates/client/src/ui/disconnected.rs` | **新增**：断开页面 |
| `crates/client/src/ui/lobby.rs` | `LobbyState` 加 display_name 输入 |
| `crates/client/src/ui/mod.rs` | 注册 `disconnected` 模块 |
| `crates/client/Cargo.toml` | 加 `serde_json` |

---

## 下一阶段

进入 Phase 7：渲染优化（[`docs/roadmap.md`](docs/roadmap.md) §Phase 7）。要做的事：
- 贪婪网格化算法替换 Phase 2 朴素逐面
- AO 计算（顶点级 4 等级）
- 视锥剔除
- mesh_jobs 优先级队列优化
- 性能 stat HUD
