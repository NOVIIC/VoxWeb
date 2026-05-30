//! 已断开连接页面：
//! - 当 Host 主动断开 / WebRTC `DataChannel` 中断 / 信令出错时进入。
//! - 显示原因文本，提供单一"返回大厅"按钮。
//!
//! 由 `lib.rs` 主循环在 [`crate::app::AppState::Disconnected`] 分支调用 [`draw_disconnected`]。

/// 断开页面返回的动作。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisconnectedAction {
    None,
    /// 用户点了"返回大厅"。调用方设 `app.state = AppState::Lobby` 并清 `disconnect_reason`。
    BackToLobby,
}

/// 绘制"已断开连接"页面。
///
/// - `reason` 为空时显示通用文案；否则以醒目的橙黄色显示原因，并允许自动换行。
pub fn draw_disconnected(ctx: &egui::Context, reason: &str) -> DisconnectedAction {
    let mut action = DisconnectedAction::None;

    #[allow(deprecated)]
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(80.0);
            ui.heading(
                egui::RichText::new("Disconnected")
                    .size(32.0)
                    .color(egui::Color32::from_rgb(230, 240, 245)),
            );
            ui.add_space(20.0);

            if reason.is_empty() {
                ui.colored_label(
                    egui::Color32::from_rgb(180, 190, 200),
                    "Connection to the room was interrupted.",
                );
            } else {
                // 原因可能较长（例如 "ICE failed: ..."），允许 wrap。
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(reason)
                            .size(15.0)
                            .color(egui::Color32::from_rgb(220, 180, 100)),
                    )
                    .wrap_mode(egui::TextWrapMode::Wrap),
                );
            }

            ui.add_space(30.0);

            let btn = egui::Button::new(
                egui::RichText::new("Back to Lobby")
                    .size(16.0)
                    .color(egui::Color32::from_rgb(230, 240, 245)),
            )
            .min_size(egui::vec2(160.0, 36.0))
            .fill(egui::Color32::from_rgb(60, 90, 120));
            if ui.add(btn).clicked() {
                action = DisconnectedAction::BackToLobby;
            }
        });
    });

    action
}
