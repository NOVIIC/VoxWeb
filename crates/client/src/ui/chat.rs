//! 聊天界面（Phase 6）。
//!
//! 两套独立的 UI：
//! - [`draw_chat_window`]：聊天框（T 键触发后调用）。带历史滚动 + 单行输入。
//! - [`draw_recent_overlay`]：游戏中常驻的浮窗，显示最近 5 秒内最多 5 条消息，按剩余时间淡出。
//!
//! 数据模型见 [`crate::chat::ChatHistory`]；UI 层仅做渲染与输入捕获，不负责发送 / 持久化。

use crate::chat::{ChatHistory, ChatKind};

/// 聊天 UI 单帧返回的动作。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatUiAction {
    None,
    /// 用户按回车提交一条消息（已从 input_buffer 取出）。
    Submit(String),
    /// 用户按 ESC 取消（input_buffer 已被清空）。
    Cancel,
}

/// 聊天输入窗口。
///
/// 调用方仅在 `chat_open == true` 的那一帧调用本函数。
/// - 锚 LEFT_BOTTOM, (20, -20)
/// - 无标题栏，min_width = 400
/// - ScrollArea max_height 200, stick_to_bottom
/// - text_edit_singleline + request_focus
/// - 回车 → [`ChatUiAction::Submit`]（input_buffer 已被 take）
/// - ESC → [`ChatUiAction::Cancel`]（input_buffer 已被 clear）
pub fn draw_chat_window(ctx: &egui::Context, history: &mut ChatHistory) -> ChatUiAction {
    let mut action = ChatUiAction::None;

    egui::Area::new(egui::Id::new("chat_input"))
        .anchor(egui::Align2::LEFT_BOTTOM, egui::vec2(20.0, -20.0))
        .movable(false)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style())
                .inner_margin(egui::Margin::same(8))
                .show(ui, |ui| {
                    ui.set_min_width(400.0);

                    // —— 历史滚动区 ——
                    egui::ScrollArea::vertical()
                        .max_height(200.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for msg in history.recent(50) {
                                ui.horizontal_wrapped(|ui| match &msg.kind {
                                    ChatKind::System => {
                                        ui.colored_label(
                                            egui::Color32::from_rgb(160, 170, 180),
                                            format!("[System] {}", msg.content),
                                        );
                                    }
                                    ChatKind::User { from_name, .. } => {
                                        ui.label(
                                            egui::RichText::new(format!("{from_name}: "))
                                                .strong()
                                                .color(egui::Color32::from_rgb(220, 230, 240)),
                                        );
                                        ui.label(
                                            egui::RichText::new(&msg.content)
                                                .color(egui::Color32::from_rgb(230, 235, 240)),
                                        );
                                    }
                                });
                            }
                        });

                    ui.separator();

                    // —— 单行输入框 ——
                    // 256 字符限制由服务端兜底；客户端不强制截断，让用户能看到自己输入的全部内容。
                    let resp = ui.text_edit_singleline(&mut history.input_buffer);
                    resp.request_focus();

                    // Submit: Enter pressed. input_buffer already contains the typed text.
                    let enter = ctx.input(|i| i.key_pressed(egui::Key::Enter));
                    let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));

                    if enter {
                        let content = std::mem::take(&mut history.input_buffer);
                        if content.trim().is_empty() {
                            // 空消息：丢弃并取消（外层关闭聊天框）。
                            action = ChatUiAction::Cancel;
                        } else {
                            action = ChatUiAction::Submit(content);
                        }
                    } else if esc {
                        history.input_buffer.clear();
                        action = ChatUiAction::Cancel;
                    }
                });
        });

    action
}

/// 平时（聊天框关闭）的浮窗：最近 5 条且在 5 秒窗口内的消息，按剩余时间淡出。
///
/// - `now_ms`：`performance.now()` 当前值（毫秒），由调用方注入。
/// - 锚 LEFT_BOTTOM, (20, -240)（让位给 hotbar）
/// - `interactable(false)`：不拦截鼠标
/// - 每条按 `(remaining_ms / 1500.0).clamp(0, 1)` 计算 alpha；剩余 > 1500ms 时 alpha=1。
///
/// 简化设计：不画背景框（背景框 alpha 控制易与文字 alpha 错位），
/// 仅用文本颜色的 alpha 通道做淡出，配合左下角的低对比度调性。
pub fn draw_recent_overlay(ctx: &egui::Context, history: &ChatHistory, now_ms: f64) {
    let recent = history.recent_within(now_ms, 5000.0, 5);
    if recent.is_empty() {
        return;
    }

    egui::Area::new(egui::Id::new("chat_recent_overlay"))
        .anchor(egui::Align2::LEFT_BOTTOM, egui::vec2(20.0, -240.0))
        .interactable(false)
        .show(ctx, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for msg in recent {
                // 剩余存活时间（ms）。最后 1500ms 内线性淡出，更早时保持 alpha=1。
                let remaining = msg.received_at_ms + 5000.0 - now_ms;
                let alpha = ((remaining / 1500.0).clamp(0.0, 1.0) * 255.0) as u8;

                match &msg.kind {
                    ChatKind::System => {
                        // 系统消息：淡灰色。
                        let color = egui::Color32::from_rgba_unmultiplied(170, 180, 195, alpha);
                        ui.label(
                            egui::RichText::new(format!("[System] {}", msg.content)).color(color),
                        );
                    }
                    ChatKind::User { from_name, .. } => {
                        // 用户消息：发送者加粗稍亮 + 内容白底。
                        let name_color =
                            egui::Color32::from_rgba_unmultiplied(235, 215, 130, alpha);
                        let body_color =
                            egui::Color32::from_rgba_unmultiplied(240, 240, 240, alpha);
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                egui::RichText::new(format!("{from_name}: "))
                                    .strong()
                                    .color(name_color),
                            );
                            ui.label(egui::RichText::new(&msg.content).color(body_color));
                        });
                    }
                }
            }
        });
}
