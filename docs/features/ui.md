# UI 系统

> **何时阅读**：改任何 UI 页面；调指针锁；改大厅/HUD/聊天/玩家列表/名牌
> **关联文档**：[`README.md`](../../README.md) · [`modules/client.md`](../modules/client.md) · [`modules/render.md`](../modules/render.md) · [`networking/protocol.md`](../networking/protocol.md)

---

## 一、技术与原则

- **框架**：`egui` + `egui-wgpu`（即时渲染 GUI）
- **集成**：`egui-winit` 处理窗口事件；UI Pass 在 Render Graph 末尾叠加
- **设计原则**：
  - 大厅在游戏外的"顶层 UI"（占满 viewport）；其余在游戏内的叠加层
  - HUD 为常驻只读悬浮（`interactable(false)`），避免拦截鼠标
  - 暂停菜单 / 聊天 / 大厅是"模态"层，会接管输入
  - 远端玩家名牌是 3D 内的 billboard，**特殊处理**（深度感知、屏幕空间投影）

---

## 二、UI 状态路由

每帧 `client::ui::draw` 按 `AppState` 决定渲染什么。Phase 6 的终态枚举把 `EscMenu` / `ChatOpen` 升级为 `InGame` 子状态位，让 HUD / 名牌可无视暂停 / 聊天叠加层始终绘制。参见 [`crates/client/src/app.rs:48-66`](../../crates/client/src/app.rs)：

```rust
pub enum AppState {
    Loading,
    Lobby,
    Connecting,
    /// 游戏进行中（可叠加暂停 / 聊天两个子状态）
    InGame {
        paused: bool,
        chat_open: bool,
    },
    Disconnected,
}

impl AppState {
    pub fn is_in_game(&self) -> bool {
        matches!(self, AppState::InGame { .. })
    }
    pub fn ingame_default() -> Self {
        AppState::InGame { paused: false, chat_open: false }
    }
}
```

```rust
pub fn draw(app: &mut App, ctx: &egui::Context) {
    match &app.state {
        AppState::Lobby => lobby::draw(app, ctx),
        AppState::Connecting => connecting::draw(app, ctx),
        AppState::Disconnected { reason } => disconnected::draw(app, ctx, reason),
        AppState::InGame { paused, chat_open } => {
            // 总是显示 HUD
            hud::draw(app, ctx);
            // 名牌（特殊：用 painter 直接画到屏幕）
            players::draw_nameplates(app, ctx);
            if *chat_open { chat::draw(app, ctx); }
            if *paused { pause::draw(app, ctx); }
        }
    }
}
```

---

## 三、`ui/lobby.rs` — 大厅

**职责**：项目首屏，玩家选择"创建房间 / 加入房间 / 单机模式"。

### 布局

```
┌──────────────────────────────────────────────┐
│                                              │
│            VoxWeb                        │
│                                              │
│   昵称：[__________________]                  │
│                                              │
│   ┌─────────────┐  ┌─────────────┐           │
│   │  创建房间    │  │  加入房间    │           │
│   │             │  │             │           │
│   │ 房间号：     │  │ 房间号：     │           │
│   │ [______6位]  │  │ [______6位]  │           │
│   │             │  │             │           │
│   │ 世界种子(可选)│  │             │           │
│   │ [______]    │  │             │           │
│   │             │  │             │           │
│   │ [创建]       │  │ [加入]       │           │
│   └─────────────┘  └─────────────┘           │
│                                              │
│   ┌─────────────┐                            │
│   │  单机模式    │                            │
│   │ [开始]       │                            │
│   └─────────────┘                            │
│                                              │
│  浏览器要求：Chrome/Edge/Safari 17+ 支持 WebGPU │
│                         VoxWeb {Cargo version}│
└──────────────────────────────────────────────┘
```

底部版本号由 `env!("CARGO_PKG_VERSION")` 读取当前客户端包版本；由于 `crates/client/Cargo.toml` 继承 workspace 版本，实际来源是根 [`Cargo.toml`](../../Cargo.toml) 的 `[workspace.package].version`。
大厅"我的存档"区域还会显示浏览器存储 `已用 / 总空间`，并在每个世界条目后显示该世界自己的 OPFS 占用空间（`world.json + chunks/*.bin`）。

### 交互

```rust
pub fn draw(app: &mut App, ctx: &egui::Context) {
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.vertical_centered(|ui| {
            ui.heading("VoxWeb");
            ui.add_space(20.0);

            ui.horizontal(|ui| {
                ui.label("昵称：");
                ui.text_edit_singleline(&mut app.lobby.display_name);
            });

            ui.add_space(20.0);

            ui.columns(2, |cols| {
                // 创建房间
                cols[0].group(|ui| {
                    ui.label("创建房间");
                    ui.text_edit_singleline(&mut app.lobby.host_room_id);
                    if app.lobby.host_room_id.is_empty() {
                        ui.label("（留空将自动生成）");
                    }
                    ui.text_edit_singleline(&mut app.lobby.seed_input);
                    if ui.button("创建").clicked() && !app.lobby.display_name.is_empty() {
                        // 必须在用户手势内！
                        let _ = app.canvas.request_pointer_lock();
                        let room_id = if app.lobby.host_room_id.is_empty() {
                            generate_room_id()
                        } else {
                            app.lobby.host_room_id.clone()
                        };
                        app.start_host(room_id, parse_seed(&app.lobby.seed_input));
                    }
                });

                // 加入房间
                cols[1].group(|ui| {
                    ui.label("加入房间");
                    ui.text_edit_singleline(&mut app.lobby.join_room_id);
                    if ui.button("加入").clicked() && valid_room_id(&app.lobby.join_room_id) {
                        let _ = app.canvas.request_pointer_lock();
                        app.start_join(app.lobby.join_room_id.clone());
                    }
                });
            });

            ui.add_space(20.0);
            if ui.button("单机模式").clicked() {
                let _ = app.canvas.request_pointer_lock();
                app.start_local_only();
            }

            ui.add_space(40.0);
            ui.label(egui::RichText::new("浏览器要求：Chrome/Edge/Safari 17+ 支持 WebGPU")
                .small().weak());
        });
    });
}
```

**关键**：`request_pointer_lock` 必须在用户点击事件内同步发起，否则浏览器拒绝。

---

## 四、`ui/lobby.rs` — 连接中视图

`draw_connecting()` 负责 Connecting 状态下的加载界面（[Phase 4+ 网络协商] + [Phase 5+ 区块预载]）。

函数签名：
```rust
pub fn draw_connecting(
    ctx: &egui::Context,
    mode: GameMode,
    room_id: &str,
    steps: &[LoadingStep],  // 来自 RoomSession::loading_steps() + 区块预载步骤
    error: Option<&str>,
) -> Option<ConnectingAction>;
```

每步的 `StepStatus` 决定显示样式：
- `Done` → `✓` 绿色
- `InProgress` → `⟳` 黄色
- `Pending` → `○` 灰色

```
┌──────────────────────────────────────────────┐
│                                              │
│          Joining room abc123…                 │
│                                              │
│     ✓  Connecting to signaling server…        │
│     ✓  Registering with room…                 │
│     ⟳  Exchanging offer…                     │
│     ○  Exchanging answer…                     │
│     ○  Establishing data channel…             │
│     ○  Loading spawn chunks (0/169)           │
│                                              │
│               [Cancel]                        │
└──────────────────────────────────────────────┘
```

步骤列表组成：
1. 前 5 步来自 `RoomSession::loading_steps()`（网络协商）
2. 最后一步"Loading spawn chunks"由客户端根据 `App::preload_state` 追加
   - 网络 Connected 后状态从 `Pending` 变为 `InProgress`
   - 进行中时显示 `(received/total)` 计数
   - 完成后状态变为 `Done` → 进入 InGame

Local 模式跳过网络步骤，仅显示区块预载那一步。

异常时显示错误 + Cancel 按钮返回大厅。

---

## 五、`ui/hud.rs` — 平视显示

**位置**：屏幕 4 个角各一个 `egui::Area`，全部 `interactable(false)`。

### 左上角：调试信息

```
FPS: 60.2
坐标: 12.34, 65.0, -8.21
区块: (0, -1)
朝向: yaw=45° pitch=-12°
延迟: 78ms (peer)
状态: Connected
VISIBLE / CULLED / DRAW_V/I
MESH ms / jobs / Phase2→Greedy 顶点
PASS world / player / selection / ui ms
SAVE 当前世界占用 / (总空间 - 其它世界占用)
```

仅在 `app.settings.show_stats == true` 显示（暂停菜单可切换）。Phase 7 起左上角统计面板也显示视锥剔除数量、draw 顶点/索引数、网格化批次耗时和各 pass 的 CPU 编码耗时。
空间数值由 `format_storage_bytes` 统一格式化，保留一位小数并在 B/KB/MB/GB/TB 间自适应切换。

### 右上角：玩家列表

```
┌──────────────────┐
│ 在线玩家 (3)      │
├──────────────────┤
│ ⚪ Alice (主机)   │
│ 🟢 Bob (你)       │
│ 🟢 Charlie        │
└──────────────────┘
```

```rust
// 实际定义见 crate::ui::players
pub struct PlayerListEntry {
    pub entity_id: EntityId,
    pub display_name: String,
    pub color_rgb: [f32; 3],   // 由 app::entity_color(eid) 确定性派生
    pub is_host: bool,         // entity_id == game.host_entity_id
    pub is_me: bool,           // entity_id == game.entity_id
}

pub fn draw_player_list(ctx: &egui::Context, entries: &[PlayerListEntry]) {
    egui::Area::new(egui::Id::new("player_list"))
        .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-10.0, 10.0))
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.label(format!("在线玩家 ({})", entries.len()));
                ui.separator();
                for entry in entries {
                    // 彩色圆点 + 显示名 + 「（主机 / 你 / 主机, 你）」后缀
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("⚫").color(color_for(entry.color_rgb)));
                        ui.label(format_with_suffix(entry));
                    });
                }
            });
        });
}
```

`lib.rs` 每帧从 `game.remote_players` + 自身条目构造按 `entity_id` 升序排列的 `Vec<PlayerListEntry>`；`is_host` 由 `entity_id == game.host_entity_id` 判定，`is_me` 由 `entity_id == game.entity_id` 判定。

### 屏幕中心：准星

```rust
pub fn draw_crosshair(ctx: &egui::Context) {
    let screen = ctx.screen_rect();
    let center = screen.center();
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, "crosshair".into()));
    let len = 8.0;
    let stroke = egui::Stroke::new(2.0, egui::Color32::WHITE);
    painter.line_segment([center - egui::vec2(len, 0.0), center + egui::vec2(len, 0.0)], stroke);
    painter.line_segment([center - egui::vec2(0.0, len), center + egui::vec2(0.0, len)], stroke);
}
```

### 选中方块线框

不在 egui 内画，由 `render::passes::opaque` 在画完世界后画一组线（或单独 Wireframe Pass）。线框的位置 = `app.current_hit.block_pos` 的方块 AABB 顶点。颜色：`vec4(0,0,0,0.6)`，深度测试 `LessEqual`，深度写入 `false`。

### 左下角：聊天历史（最近消息）

最近 5 条消息浮窗，5 秒后渐隐。聊天框打开时显示完整历史。详见下文。

### 屏幕底部中央：Hotbar（Phase 3 ✅ · 9 格）

横向 9 格，每格 54×36，底部居中（屏幕 anchor `CENTER_BOTTOM` + (0, -16)）。每格上行显示槽位号 1-9，下行显示方块标签（`STONE` / `DIRT` / ...）；选中格金色高亮（240, 200, 80, 220），文字反色为黑；非选中格深灰底（60, 70, 80, 200）+ 浅色文字。

```rust
egui::Area::new(egui::Id::new("hud_hotbar"))
    .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -16.0))
    .show(ctx, |ui| {
        egui::Frame::default().fill(BG_HALF_ALPHA).inner_margin(8).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for (i, block) in hotbar_items.iter().enumerate() {
                    let selected = i == hotbar_selected;
                    egui::Frame::default().fill(cell_bg(selected)).show(ui, |ui| {
                        // ⚠️ 必须用 allocate_ui_with_layout 显式分配尺寸，
                        // 不能用 set_min_size + vertical_centered（vertical_centered
                        // 会取走 horizontal 父级的全部剩余宽度，9 格全叠成 1 格）
                        ui.allocate_ui_with_layout(
                            egui::vec2(54.0, 36.0),
                            egui::Layout::top_down(egui::Align::Center),
                            |ui| { ui.colored_label(cell_fg(selected),
                                       format!("{}\n{}", i + 1, block_label(*block))); },
                        );
                    });
                }
            });
        });
    });
```

切换：1-9 数字键（`InputState::hotbar_request` 边沿）。鼠标滚轮切换 + 图标渲染留给 Phase 6（egui ImageButton + 纹理图集裁剪）。

> **历史坑**：早期实现用 `set_min_size + vertical_centered` 组合，导致 9 个 Frame 均摊到完整屏幕宽，只剩第 1 格可见（commit 95b262a 修复）。新代码不要再用这两个 API 嵌套在 horizontal 内。

---

## 六、`ui/pause.rs` — 暂停菜单（ESC）

```
┌──────────────────────────────────────────────┐
│                                              │
│              已暂停                            │
│                                              │
│   FOV：       [============●===========]      │
│              30°            70°       110°    │
│                                              │
│   鼠标灵敏度：[==●========================]   │
│              0.1                          5.0 │
│                                              │
│   渲染距离：  [4 ▼]                            │
│                                              │
│   插值延迟：  ( ) 50ms  (●) 100ms  ( ) 150ms   │
│                                              │
│   ☑ 显示统计信息                              │
│   ☐ 启用 Depth Pre-Pass                       │
│                                              │
│   [立即保存]    [继续游戏]    [退出到大厅]       │
└──────────────────────────────────────────────┘
```

```rust
pub fn draw(app: &mut App, ctx: &egui::Context) {
    egui::Window::new("已暂停")
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .resizable(false)
        .collapsible(false)
        .show(ctx, |ui| {
            ui.add(egui::Slider::new(&mut app.settings.fov_degrees, 30.0..=110.0).text("FOV"));
            ui.add(egui::Slider::new(&mut app.settings.mouse_sensitivity, 0.1..=5.0).text("灵敏度"));

            egui::ComboBox::from_label("渲染距离")
                .selected_text(format!("{}", app.settings.render_distance_chunks))
                .show_ui(ui, |ui| {
                    for d in [2, 4, 6, 8, 10] {
                        ui.selectable_value(&mut app.settings.render_distance_chunks, d, format!("{}", d));
                    }
                });

            ui.horizontal(|ui| {
                ui.label("插值延迟：");
                ui.radio_value(&mut app.settings.interp_delay_ms, 50.0, "50ms");
                ui.radio_value(&mut app.settings.interp_delay_ms, 100.0, "100ms");
                ui.radio_value(&mut app.settings.interp_delay_ms, 150.0, "150ms");
            });

            ui.checkbox(&mut app.settings.show_stats, "显示统计信息");
            ui.checkbox(&mut app.settings.depth_prepass, "启用 Depth Pre-Pass");

            ui.add_space(20.0);
            ui.horizontal(|ui| {
                if ui.button("立即保存").clicked() {
                    app.save_now();
                }
                if ui.button("继续游戏").clicked() {
                    app.resume_game();
                }
                if ui.button("退出到大厅").clicked() {
                    app.disconnect_and_return_to_lobby();
                }
            });
        });
}
```

`resume_game` 必须重新发起 `request_pointer_lock`（在按钮的点击 closure 内同步发起）。

> **Phase 6 实装差异**：
> - 上图与示例代码里的「☐ 启用 Depth Pre-Pass」复选框**Phase 6 内未暴露**，留到 Phase 8 的多 Pass 重构一起加（届时整个渲染管线会切到 Render Graph）。当前 [`crates/client/src/ui/pause.rs`](../../crates/client/src/ui/pause.rs) 只渲染 FOV / 灵敏度 / 渲染距离 / 插值延迟 / 显示统计 5 项。
> - 实际函数签名为 `draw_pause_menu(ctx, &mut AppSettings) -> PauseAction`，模块只 mutate `AppSettings`，调用方根据返回的 [`PauseAction::{None, Resume, SaveNow, ExitToLobby}`](../../crates/client/src/ui/pause.rs) 决定关闭叠加层 / 立即保存 / 切回大厅 / 重新请求指针锁。
> - 设置在 `Resume` / `ExitToLobby` 时通过 [`settings_storage::save`](../../crates/client/src/settings_storage.rs) 写入 localStorage（键 `voxweb.settings.v1`，JSON 编码 + schema 版本头）。下次进入大厅时由 `settings_storage::load()` 读回，失败回退 `AppSettings::default()`。

---

## 七、`ui/chat.rs` — 聊天

### 触发
- T 键打开（`chat_open = true`）
- Enter 提交并关闭
- ESC 取消并关闭
- 聊天输入接收普通文本事件；Space 既要转成 `egui::Event::Key(Key::Space)`，也要转成
  `egui::Event::Text(" ")`，否则 `TextEdit` 只能感知按键状态，无法真正插入空格。
- 聊天历史和最近消息浮窗按单行显示：Label 使用 `TextWrapMode::Extend`，显式 CR/LF
  只在显示层折成空格，不改变协议里的原始消息内容。

### 布局

```
┌──────────────────────────────────────────────┐
│ Alice: 大家好！                                │
│ Bob: hi                                      │
│ Charlie: 这边是新手村                          │
│ Alice: 我去山那边采石                          │
│                                              │
│ ┌──────────────────────────────────────┐    │
│ │ 输入消息...                           │    │
│ └──────────────────────────────────────┘    │
└──────────────────────────────────────────────┘
```

```rust
pub fn draw(app: &mut App, ctx: &egui::Context) {
    egui::Window::new("聊天")
        .anchor(egui::Align2::LEFT_BOTTOM, egui::vec2(20.0, -20.0))
        .resizable(false)
        .collapsible(false)
        .title_bar(false)
        .min_width(400.0)
        .show(ctx, |ui| {
            // 历史：一条消息固定渲染为单行，避免发送者名和正文在窄宽度下被拆行。
            egui::ScrollArea::vertical().max_height(200.0).stick_to_bottom(true).show(ui, |ui| {
                for msg in app.chat.recent(50) {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(format!("{}: ", msg.from)).strong());
                        ui.add(egui::Label::new(&msg.content)
                            .wrap_mode(egui::TextWrapMode::Extend));
                    });
                }
            });
            ui.separator();
            // 输入
            let response = ui.text_edit_singleline(&mut app.chat.input_buffer);
            response.request_focus();
            if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                if !app.chat.input_buffer.is_empty() {
                    app.send_chat(app.chat.input_buffer.clone());
                    app.chat.input_buffer.clear();
                }
                app.close_chat();
            }
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                app.chat.input_buffer.clear();
                app.close_chat();
            }
        });
}
```

### 系统消息（Join/Leave）
```rust
fn on_peer_joined(&mut self, name: &str) {
    self.chat.history.push(ChatMessage {
        from: "[系统]".into(),
        content: format!("{} 加入了房间", name),
        kind: ChatKind::System,
    });
}
```

### 平时（聊天框关闭）
最近 5 条消息浮窗在屏幕左下，浅色背景，5 秒后渐隐：
```rust
let recent = app.chat.recent(5)
    .into_iter().filter(|m| m.received_at_ms > now - 5000.0).collect();
// 用 egui::Area + 自定义渐隐 alpha 渲染
```

---

## 八、`ui/players.rs` — 远端玩家名牌

**特殊**：名牌跟随 3D 中的玩家头顶，需要把世界坐标投影到屏幕，然后用 egui 直接画。

```rust
pub fn draw_nameplates(app: &App, ctx: &egui::Context) {
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, "nameplate".into()));
    let view_proj = app.camera.view_proj();
    let screen = ctx.screen_rect();

    for (entity_id, player_view) in app.interp.iter() {
        if entity_id == app.self_id { continue; }

        let head_pos = player_view.position + Vec3::new(0.0, PLAYER_HEIGHT + 0.3, 0.0);
        let clip = view_proj * Vec4::new(head_pos.x, head_pos.y, head_pos.z, 1.0);
        if clip.w <= 0.0 { continue; }   // 在相机后方
        let ndc = Vec3::new(clip.x / clip.w, clip.y / clip.w, clip.z / clip.w);
        if ndc.z > 1.0 || ndc.z < -1.0 { continue; }   // 超出深度范围

        let screen_pos = egui::pos2(
            screen.left() + (ndc.x * 0.5 + 0.5) * screen.width(),
            screen.top() + (1.0 - (ndc.y * 0.5 + 0.5)) * screen.height(),
        );

        let display_name = app.world_view.players.get(&entity_id)
            .map(|p| p.display_name.as_str()).unwrap_or("?");

        painter.rect_filled(
            egui::Rect::from_center_size(screen_pos, egui::vec2(80.0, 22.0)),
            egui::Rounding::same(4.0),
            egui::Color32::from_rgba_unmultiplied(0, 0, 0, 180),
        );
        painter.text(
            screen_pos,
            egui::Align2::CENTER_CENTER,
            display_name,
            egui::FontId::proportional(14.0),
            egui::Color32::WHITE,
        );
    }
}
```

**深度遮挡**：本期不实现"被墙挡住时变半透明"（需要从深度纹理采样，复杂）。v2 可以加。

**距离衰减**：超过 32m 不显示（避免远处密密麻麻）。

---

## 九、指针锁

### 触发时机
- 进入 InGame：在用户点击大厅按钮时同步调用 `canvas.request_pointer_lock()`（必须在用户手势内）
- 关闭暂停菜单：再次请求
- 关闭聊天：再次请求

### 浏览器事件
监听 `pointerlockchange`：
```rust
let cb = Closure::wrap(Box::new(move || {
    let locked = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.pointer_lock_element())
        .is_some();
    sender.send(locked).unwrap();
}) as Box<dyn FnMut()>);
document.add_event_listener_with_callback("pointerlockchange", cb.as_ref().unchecked_ref())?;
```

如果用户按 ESC 主动释放指针锁，自动切到 EscMenu。

---

## 十、字体

- 默认字体：`egui` 内置 ProggyClean（小尺寸像素风）
- 中文：嵌入 `Noto Sans SC`（或类似 CJK 字体）的子集（仅常用 5000 字 + 标点）
- 字体文件大小：约 2-3 MB（gz 后），通过 `include_bytes!` 嵌入
- 注册：

```rust
pub fn install_chinese_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("noto_sc".into(),
        egui::FontData::from_static(include_bytes!("../assets/NotoSansSC-Regular.subset.ttf")));
    fonts.families.entry(egui::FontFamily::Proportional).or_default()
        .insert(0, "noto_sc".into());
    ctx.set_fonts(fonts);
}
```

---

## 十一、DPI 与 viewport

```rust
let dpr = window.device_pixel_ratio() as f32;
ctx.set_pixels_per_point(dpr);
```

每次 canvas resize 同步更新。

---

## 十二、性能

| 项目 | 目标 |
|---|---|
| `egui::Context::run`（UI 重建） | < 1ms |
| UI Pass GPU 编码 + draw | < 1ms |
| 聊天历史保留条数 | 100 |

---

## 十三、不在范围

- 拖拽 / 调整窗口大小（egui 自带；本期使用 anchor 固定布局）
- 主题切换 / 亮色暗色
- 国际化（仅中文）
- 复杂 hotbar UI（图标渲染）— v2
- 玩家头像（v2）
- 设置导出/导入
- 屏幕截图按钮（用浏览器自带快捷键即可）
