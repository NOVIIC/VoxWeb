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

每帧 `client::ui::draw` 按 `AppState` 决定渲染什么：

```rust
pub fn draw(app: &mut App, ctx: &egui::Context) {
    match &app.state {
        AppState::Lobby => lobby::draw(app, ctx),
        AppState::Connecting { progress } => connecting::draw(app, ctx, progress),
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
└──────────────────────────────────────────────┘
```

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

## 四、`ui/connecting.rs` — 连接中

显示信令进度：

```
┌──────────────────────────────────────────────┐
│                                              │
│           正在加入房间 abc123...               │
│                                              │
│   [✓] 连接信令服务                             │
│   [✓] 找到主机                                │
│   [⠋] 协商 ICE 候选                           │
│   [ ] 建立数据通道                            │
│   [ ] 接收世界数据 (12/49 chunks)             │
│                                              │
│            [取消]                             │
└──────────────────────────────────────────────┘
```

异常时显示错误 + 重试 / 返回大厅按钮。

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
```

仅在 `app.settings.show_stats == true` 显示（按 F3 切换）。

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
pub fn draw_player_list(app: &App, ctx: &egui::Context) {
    egui::Area::new(egui::Id::new("player_list"))
        .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-10.0, 10.0))
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.label(format!("在线玩家 ({})", app.world_view.players.len()));
                ui.separator();
                for p in app.world_view.players.values() {
                    let role = if p.entity_id == app.host_id { "（主机）" } else { "" };
                    let me = if p.entity_id == app.self_id { "（你）" } else { "" };
                    ui.label(format!("{} {}{}", p.display_name, role, me));
                }
            });
        });
}
```

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

### 右下角：Hotbar（v2 完整）

本期简化：屏幕底部中央显示当前手持方块名称。
```rust
egui::Area::new("hotbar".into())
    .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -20.0))
    .interactable(false)
    .show(ctx, |ui| {
        ui.label(format!("[{}] {}", app.hotbar.selected + 1,
            properties(app.hotbar.items[app.hotbar.selected]).display_name));
    });
```

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
│   ☑ 显示统计信息 (F3)                          │
│   ☐ 启用 Depth Pre-Pass                       │
│                                              │
│   [继续游戏]    [退出到大厅]                    │
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

            ui.checkbox(&mut app.settings.show_stats, "显示统计信息 (F3)");
            ui.checkbox(&mut app.settings.depth_prepass, "启用 Depth Pre-Pass");

            ui.add_space(20.0);
            ui.horizontal(|ui| {
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

---

## 七、`ui/chat.rs` — 聊天

### 触发
- T 键打开（`chat_open = true`）
- Enter 提交并关闭
- ESC 取消并关闭

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
            // 历史
            egui::ScrollArea::vertical().max_height(200.0).stick_to_bottom(true).show(ui, |ui| {
                for msg in app.chat.recent(50) {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new(format!("{}: ", msg.from)).strong());
                        ui.label(&msg.content);
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
