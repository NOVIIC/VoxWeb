//! 大厅 UI：
//! - 单机模式（Phase 2）
//! - 创建房间 / 加入房间（Phase 4）
//!
//! 还提供"Connecting…"视图：[`draw_connecting`]。

use crate::app::GameMode;
use voxweb_net::{LoadingStep, StepStatus};

/// 大厅按钮触发的动作。lib.rs 主循环消费。
///
/// Phase 6：所有进入游戏的动作都额外携带 `display_name`，从大厅顶部的昵称输入框抓取，
/// 经修剪后若为空则在主循环里回退为 "Player"。
#[derive(Clone, Debug)]
pub enum LobbyAction {
    /// 用户点了"单机模式"。seed 为 None 则随机生成。
    StartSinglePlayer {
        seed: Option<u64>,
        display_name: String,
    },
    /// 用户点了"创建房间"。`room_id` 空时主循环自动生成 6 位随机字符。
    CreateRoom {
        room_id: String,
        seed: Option<u64>,
        display_name: String,
    },
    /// 用户点了"加入房间"。
    JoinRoom {
        room_id: String,
        display_name: String,
    },
}

/// Connecting 视图触发的动作。
#[derive(Clone, Debug)]
pub enum ConnectingAction {
    /// 用户点了 Cancel。
    Cancel,
}

/// 大厅 UI 持久状态（输入框文本等）。
pub struct LobbyState {
    /// Phase 6：玩家昵称输入框。默认 "Player"，进入游戏时若 trim 后为空则回退为 "Player"。
    pub display_name: String,
    pub seed_input: String,
    pub room_id_input: String,
    /// 简单错误提示（join 时校验失败、create 时空字段自动生成的回填等）。
    pub error_message: Option<String>,
    /// info 区显示的最近一次自动生成的房间号（让用户能记住分享）。
    pub last_generated_room: Option<String>,
}

impl Default for LobbyState {
    fn default() -> Self {
        Self {
            display_name: "Player".to_string(),
            seed_input: String::new(),
            room_id_input: String::new(),
            error_message: None,
            last_generated_room: None,
        }
    }
}

/// 从输入框抓取昵称，trim 后若为空则回退到 "Player"。
fn resolve_display_name(state: &LobbyState) -> String {
    let trimmed = state.display_name.trim();
    if trimmed.is_empty() {
        "Player".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 绘制大厅 UI。返回触发的动作（点击按钮时）。
pub fn draw_lobby(ctx: &egui::Context, state: &mut LobbyState) -> Option<LobbyAction> {
    let mut action = None;

    // 主面板
    #[allow(deprecated)]
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.add_space(60.0);
        ui.vertical_centered(|ui| {
            ui.heading(
                egui::RichText::new("VoxWeb")
                    .size(48.0)
                    .color(egui::Color32::from_rgb(230, 240, 245)),
            );
            ui.add_space(8.0);
            ui.colored_label(
                egui::Color32::from_rgb(160, 170, 180),
                "Browser Voxel Sandbox (Phase 4)",
            );

            ui.add_space(40.0);

            // —— 昵称输入（Phase 6）——
            // 放在所有进入游戏按钮上方，让用户先确定身份再选模式。
            ui.horizontal(|ui| {
                ui.add_space(120.0);
                ui.label(
                    egui::RichText::new("Nickname:").color(egui::Color32::from_rgb(180, 190, 200)),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut state.display_name)
                        .desired_width(220.0)
                        .hint_text("Player"),
                );
            });

            ui.add_space(12.0);

            // —— 单机模式按钮 ——
            let btn = egui::Button::new(
                egui::RichText::new("Single Player")
                    .size(20.0)
                    .color(egui::Color32::from_rgb(230, 240, 245)),
            )
            .min_size(egui::vec2(260.0, 44.0))
            .fill(egui::Color32::from_rgb(60, 90, 120));
            if ui.add(btn).clicked() {
                let seed = parse_seed(&state.seed_input);
                let display_name = resolve_display_name(state);
                action = Some(LobbyAction::StartSinglePlayer { seed, display_name });
            }

            ui.add_space(20.0);

            // —— Room ID 输入 ——
            ui.horizontal(|ui| {
                ui.add_space(120.0);
                ui.label(
                    egui::RichText::new("Room ID").color(egui::Color32::from_rgb(180, 190, 200)),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut state.room_id_input)
                        .desired_width(160.0)
                        .hint_text("e.g. abc123"),
                );
            });

            ui.add_space(8.0);

            // —— Create / Join 按钮 ——
            ui.horizontal(|ui| {
                ui.add_space(120.0);
                let create = egui::Button::new(
                    egui::RichText::new("Create Room")
                        .size(16.0)
                        .color(egui::Color32::from_rgb(230, 240, 245)),
                )
                .min_size(egui::vec2(120.0, 36.0))
                .fill(egui::Color32::from_rgb(90, 60, 120));
                if ui.add(create).clicked() {
                    let room_id = state.room_id_input.trim().to_string();
                    let seed = parse_seed(&state.seed_input);
                    let display_name = resolve_display_name(state);
                    state.error_message = None;
                    action = Some(LobbyAction::CreateRoom {
                        room_id,
                        seed,
                        display_name,
                    });
                }
                let join = egui::Button::new(
                    egui::RichText::new("Join Room")
                        .size(16.0)
                        .color(egui::Color32::from_rgb(230, 240, 245)),
                )
                .min_size(egui::vec2(120.0, 36.0))
                .fill(egui::Color32::from_rgb(60, 120, 90));
                if ui.add(join).clicked() {
                    let room_id = state.room_id_input.trim().to_string();
                    if let Err(msg) = validate_room_id(&room_id) {
                        state.error_message = Some(msg);
                    } else {
                        let display_name = resolve_display_name(state);
                        state.error_message = None;
                        action = Some(LobbyAction::JoinRoom {
                            room_id,
                            display_name,
                        });
                    }
                }
            });

            // —— 提示信息：自动生成的房间号 / 错误 ——
            if let Some(room) = &state.last_generated_room {
                ui.add_space(6.0);
                ui.colored_label(
                    egui::Color32::from_rgb(140, 200, 160),
                    format!("Generated room id: {room} (share with friends)"),
                );
            }
            if let Some(err) = &state.error_message {
                ui.add_space(6.0);
                ui.colored_label(egui::Color32::from_rgb(220, 130, 130), err);
            }

            ui.add_space(24.0);

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
        });
    });

    // 底部版本提示
    egui::Area::new(egui::Id::new("lobby_version"))
        .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -12.0))
        .show(ctx, |ui| {
            ui.colored_label(
                egui::Color32::from_rgb(100, 110, 120),
                "VoxWeb 0.1.0 · Phase 4",
            );
        });

    action
}

/// Connecting 视图：等待信令 + WebRTC 协商 + 区块预载时显示进度列表。
pub fn draw_connecting(
    ctx: &egui::Context,
    mode: GameMode,
    room_id: &str,
    steps: &[LoadingStep],
    error: Option<&str>,
) -> Option<ConnectingAction> {
    let mut action = None;

    #[allow(deprecated)]
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.add_space(120.0);
        ui.vertical_centered(|ui| {
            let title = match mode {
                GameMode::Host => format!("Hosting room {room_id}…"),
                GameMode::Remote => format!("Joining room {room_id}…"),
                GameMode::Local => "Loading…".to_string(),
            };
            ui.heading(
                egui::RichText::new(title)
                    .size(28.0)
                    .color(egui::Color32::from_rgb(230, 240, 245)),
            );
            ui.add_space(24.0);

            // —— 步骤列表 ——
            for step in steps {
                let (icon, color) = match step.status {
                    StepStatus::Done => ("✓", egui::Color32::from_rgb(100, 200, 120)),
                    StepStatus::InProgress => ("⟳", egui::Color32::from_rgb(220, 200, 100)),
                    StepStatus::Pending => ("○", egui::Color32::from_rgb(120, 130, 140)),
                };
                ui.horizontal(|ui| {
                    ui.add_space(40.0);
                    ui.colored_label(color, egui::RichText::new(icon).size(16.0));
                    ui.add_space(8.0);
                    ui.colored_label(
                        egui::Color32::from_rgb(180, 190, 200),
                        egui::RichText::new(&step.label).size(16.0),
                    );
                });
                ui.add_space(4.0);
            }

            if let Some(msg) = error {
                ui.add_space(12.0);
                ui.colored_label(
                    egui::Color32::from_rgb(220, 130, 130),
                    egui::RichText::new(msg).size(15.0),
                );
            }

            ui.add_space(32.0);
            let btn = egui::Button::new(
                egui::RichText::new("Cancel")
                    .size(16.0)
                    .color(egui::Color32::from_rgb(230, 240, 245)),
            )
            .min_size(egui::vec2(160.0, 36.0))
            .fill(egui::Color32::from_rgb(100, 70, 70));
            if ui.add(btn).clicked() {
                action = Some(ConnectingAction::Cancel);
            }
        });
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

/// 校验房间号：4-12 字符，仅 [a-z0-9_-]。
pub fn validate_room_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("Room ID cannot be empty".into());
    }
    let len = id.chars().count();
    if !(4..=12).contains(&len) {
        return Err("Room ID must be 4-12 characters".into());
    }
    let ok = id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !ok {
        return Err("Room ID may only contain a-z, 0-9, _, -".into());
    }
    Ok(())
}

/// 生成 6 位 [a-z0-9] 随机房间号。失败时返回 "voxweb"（不应发生）。
pub fn generate_room_id() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut buf = [0u8; 6];
    if getrandom::getrandom(&mut buf).is_err() {
        return "voxweb".to_string();
    }
    buf.iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect()
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

    #[test]
    fn room_id_valid() {
        assert!(validate_room_id("abc123").is_ok());
        assert!(validate_room_id("a-b_c").is_ok());
    }

    #[test]
    fn room_id_too_short() {
        assert!(validate_room_id("ab").is_err());
    }

    #[test]
    fn room_id_too_long() {
        assert!(validate_room_id("abcdefghijklm").is_err());
    }

    #[test]
    fn room_id_bad_chars() {
        assert!(validate_room_id("ABC123").is_err());
        assert!(validate_room_id("a b c d").is_err());
    }

    #[test]
    fn generated_room_id_valid_format() {
        let id = generate_room_id();
        assert!(validate_room_id(&id).is_ok(), "got {id}");
        assert_eq!(id.len(), 6);
    }
}
