//! 已断开连接页面：
//! - 当 Host 主动断开 / WebRTC `DataChannel` 中断 / 信令出错时进入。
//! - 显示原因文本，提供单一"返回大厅"按钮。
//!
//! 由 `lib.rs` 主循环在 [`crate::app::AppState::Disconnected`] 分支调用 [`draw_disconnected`]。

use crate::ui::theme;

/// 断开页面返回的动作。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisconnectedAction {
    None,
    /// 用户点了"返回大厅"。调用方走统一的大厅重置流程。
    BackToLobby,
}

/// 绘制"已断开连接"页面。
///
/// - `reason` 为空时显示通用文案；否则以醒目的橙黄色显示原因，并允许自动换行。
pub fn draw_disconnected(ctx: &egui::Context, reason: &str) -> DisconnectedAction {
    let mut action = DisconnectedAction::None;

    #[allow(deprecated)]
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.add_space(70.0);
        ui.vertical_centered(|ui| {
            theme::panel_frame().show(ui, |ui| {
                ui.set_width(460.0);
                ui.vertical_centered(|ui| {
                    ui.heading(
                        egui::RichText::new("Disconnected")
                            .size(32.0)
                            .color(theme::TEXT),
                    );
                    ui.add_space(20.0);

                    if reason.is_empty() {
                        ui.colored_label(theme::MUTED, "Connection to the room was interrupted.");
                    } else {
                        // 原因可能较长（例如 "ICE failed: ..."），允许 wrap。
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(reason).size(15.0).color(theme::WARNING),
                            )
                            .wrap_mode(egui::TextWrapMode::Wrap),
                        );
                    }

                    ui.add_space(30.0);

                    let btn =
                        theme::primary_button("Back to Lobby").min_size(egui::vec2(160.0, 36.0));
                    if ui.add(btn).clicked() {
                        action = DisconnectedAction::BackToLobby;
                    }
                });
            });
        });
    });

    action
}
