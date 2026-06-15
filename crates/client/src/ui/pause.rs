//! 暂停菜单（ESC）：调整 FOV / 鼠标灵敏度 / 渲染距离 / 远端插值延迟 / 显示统计开关，
//! 并提供「继续游戏」「退出到大厅」两个动作。
//!
//! 设计要点：
//! - 该模块只负责绘制 + 直接 mutate [`AppSettings`]，不持有也不修改 `Game`/`App`；
//!   按钮触发的高层副作用（重新请求指针锁、断开网络回大厅）由调用方根据
//!   返回的 [`PauseAction`] 完成。
//! - 「省略 Depth Pre-Pass 复选框」属于 docs/features/ui.md §六 的 Phase 8 范畴，
//!   本期暂不暴露 UI；其余字段（fly_speed / mesh_budget_ms / min_action_interval_ms）
//!   属于开发调优字段，也不在用户菜单中暴露。

use crate::app::AppSettings;
use crate::ui::theme;

/// 暂停菜单返回的动作。
///
/// `None` 表示本帧只是普通的设置项修改（slider/checkbox 拖动），
/// 调用方维持暂停态即可；`Resume` 与 `ExitToLobby` 由两个底部按钮触发。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PauseAction {
    /// 无显式动作（可能修改了设置项，调用方按需调用 `Game::apply_settings`）。
    None,
    /// 继续游戏：调用方关闭菜单并请求重新锁定指针。
    Resume,
    /// 立即保存当前 dirty chunks。
    SaveNow,
    /// 退出到大厅：调用方 drop game + state = Lobby。
    ExitToLobby,
}

/// 绘制居中模态暂停菜单。
///
/// 点击菜单外的游戏空白区域等同于点击 Resume：调用方会关闭暂停层并重新请求指针锁。
///
/// 返回本帧的用户动作；同时 `settings` 中受 UI 控制的字段可能已被原地修改，
/// 调用方需在动作处理完毕后（或每帧）调用 [`crate::app::Game::apply_settings`]
/// 把新设置同步到 camera / interp / chunk_loader 等运行时组件。
pub fn draw_pause_menu(ctx: &egui::Context, settings: &mut AppSettings) -> PauseAction {
    let mut action = PauseAction::None;

    egui::Window::new("Paused")
        // 始终居中、不可拉伸 / 折叠，模态感更强。
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .resizable(false)
        .collapsible(false)
        .frame(theme::panel_frame())
        .show(ctx, |ui| {
            ui.set_width(360.0);
            // FOV：30°–110°，超广角到长焦的常用范围。
            ui.add(egui::Slider::new(&mut settings.fov_degrees, 30.0..=110.0).text("FOV (°)"));

            // 鼠标灵敏度倍率：1.0 为默认，乘到 BASE_SENSITIVITY_RAD_PER_PIXEL 上。
            ui.add(
                egui::Slider::new(&mut settings.mouse_sensitivity, 0.1..=5.0)
                    .text("Mouse Sensitivity"),
            );

            // 渲染距离：离散选项，使用 ComboBox 而非连续滑条，避免触发频繁的
            // ChunkLoader 半径重算。
            egui::ComboBox::from_label("Render Distance")
                .selected_text(format!("{}", settings.render_distance))
                .show_ui(ui, |ui| {
                    for d in [2u32, 4, 6, 8, 10] {
                        ui.selectable_value(&mut settings.render_distance, d, format!("{d}"));
                    }
                });

            // 远端玩家位置插值延迟：50/100/150 ms 三档单选。
            // 数值越大越平滑但越滞后；推荐 100ms（典型 RTT + 抖动余量）。
            ui.horizontal(|ui| {
                ui.label("Interpolation Delay:");
                ui.radio_value(&mut settings.interp_delay_ms, 50.0, "50ms");
                ui.radio_value(&mut settings.interp_delay_ms, 100.0, "100ms");
                ui.radio_value(&mut settings.interp_delay_ms, 150.0, "150ms");
            });

            ui.checkbox(&mut settings.show_stats, "Show Stats");
            ui.checkbox(&mut settings.depth_prepass_enabled, "Depth Pre-Pass");
            ui.add(
                egui::Slider::new(&mut settings.chunk_cache_capacity, 512..=10_000)
                    .text("Chunk Cache"),
            );

            ui.add_space(20.0);

            // 底部动作按钮：继续游戏 / 退出到大厅。
            // 注意：实际的 pointer lock 请求需要在用户手势同步发起，
            // 这里只返回 Action，由调用方在同一帧的事件处理路径中调用
            // canvas.request_pointer_lock()。
            ui.horizontal(|ui| {
                if ui.add(theme::secondary_button("Save Now")).clicked() {
                    action = PauseAction::SaveNow;
                }
                if ui.add(theme::primary_button("Resume")).clicked() {
                    action = PauseAction::Resume;
                }
                if ui.add(theme::danger_button("Exit to Lobby")).clicked() {
                    action = PauseAction::ExitToLobby;
                }
            });
        });

    if action == PauseAction::None && clicked_empty_game_area(ctx) {
        action = PauseAction::Resume;
    }

    action
}

fn clicked_empty_game_area(ctx: &egui::Context) -> bool {
    let primary_clicked = ctx.input(|i| i.pointer.button_clicked(egui::PointerButton::Primary));
    let pointer_over_egui = ctx.is_pointer_over_egui();
    empty_click_should_resume(primary_clicked, pointer_over_egui)
}

fn empty_click_should_resume(primary_clicked: bool, pointer_over_egui: bool) -> bool {
    primary_clicked && !pointer_over_egui
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `PauseAction` 派生了 `PartialEq`，调用方可以直接用 `==` 比较。
    /// 这里做一个 sanity check，防止后续误删 derive。
    #[test]
    fn pause_action_partial_eq() {
        assert_eq!(PauseAction::None, PauseAction::None);
        assert_ne!(PauseAction::Resume, PauseAction::ExitToLobby);
        assert_ne!(PauseAction::None, PauseAction::Resume);
    }

    #[test]
    fn empty_click_resume_predicate_only_accepts_primary_blank_clicks() {
        assert!(empty_click_should_resume(true, false));
        assert!(!empty_click_should_resume(false, false));
        assert!(!empty_click_should_resume(true, true));
    }
}
