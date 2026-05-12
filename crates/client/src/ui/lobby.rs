//! 大厅 UI：Phase 2 仅"单机模式"按钮 + 可选种子输入。
//! Phase 4 起补"创建房间 / 加入房间"按钮。

/// 大厅按钮触发的动作。lib.rs 主循环消费。
#[derive(Clone, Debug)]
pub enum LobbyAction {
    /// 用户点了"单机模式"。seed 为 None 则随机生成。
    StartSinglePlayer { seed: Option<u64> },
}

/// 大厅 UI 持久状态（输入框文本等）。
#[derive(Default)]
pub struct LobbyState {
    pub seed_input: String,
}

/// 绘制大厅 UI。返回触发的动作（点击按钮时）。
pub fn draw_lobby(ctx: &egui::Context, state: &mut LobbyState) -> Option<LobbyAction> {
    let mut action = None;

    // 主面板（egui 0.34 起 CentralPanel::show 被标记 deprecated，但根面板用法无更好替代，保留）
    #[allow(deprecated)]
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.add_space(80.0);
        ui.vertical_centered(|ui| {
            ui.heading(
                egui::RichText::new("VoxWeb")
                    .size(48.0)
                    .color(egui::Color32::from_rgb(230, 240, 245)),
            );
            ui.add_space(8.0);
            ui.colored_label(
                egui::Color32::from_rgb(160, 170, 180),
                "Browser Voxel Sandbox (Phase 3)",
            );

            ui.add_space(48.0);

            // —— 单机模式按钮 ——
            let btn = egui::Button::new(
                egui::RichText::new("Single Player")
                    .size(20.0)
                    .color(egui::Color32::from_rgb(230, 240, 245)),
            )
            .min_size(egui::vec2(240.0, 48.0))
            .fill(egui::Color32::from_rgb(60, 90, 120));
            if ui.add(btn).clicked() {
                let seed = parse_seed(&state.seed_input);
                action = Some(LobbyAction::StartSinglePlayer { seed });
            }

            ui.add_space(16.0);

            // —— 种子输入（折叠区）——
            egui::CollapsingHeader::new("Advanced / Seed")
                .default_open(false)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Seed (u64, blank = random):");
                        ui.add(
                            egui::TextEdit::singleline(&mut state.seed_input)
                                .desired_width(180.0)
                                .hint_text("e.g. 1234567"),
                        );
                    });
                });

            ui.add_space(80.0);
            ui.colored_label(
                egui::Color32::from_rgb(120, 130, 140),
                "Phase 4: Create/Join room (coming soon)",
            );
        });
    });

    // 底部版本提示
    egui::Area::new(egui::Id::new("lobby_version"))
        .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -12.0))
        .show(ctx, |ui| {
            ui.colored_label(
                egui::Color32::from_rgb(100, 110, 120),
                "VoxWeb 0.1.0 · Phase 3",
            );
        });

    action
}

/// 把输入框文本解析为 Option<u64>。空字符串 → None（随机）。
fn parse_seed(input: &str) -> Option<u64> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_seed_empty_is_none() {
        assert_eq!(parse_seed(""), None);
        assert_eq!(parse_seed("   "), None);
    }

    #[test]
    fn parse_seed_valid_u64() {
        assert_eq!(parse_seed("42"), Some(42));
        assert_eq!(parse_seed("18446744073709551615"), Some(u64::MAX));
    }

    #[test]
    fn parse_seed_invalid_is_none() {
        assert_eq!(parse_seed("not_a_number"), None);
        assert_eq!(parse_seed("-1"), None); // 负数不是合法 u64
    }
}
