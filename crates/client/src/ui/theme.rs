//! VoxWeb 统一 UI 主题。

use voxweb_core::block::BlockID;

pub const BG: egui::Color32 = egui::Color32::from_rgb(18, 23, 26);
pub const PANEL: egui::Color32 = egui::Color32::from_rgba_unmultiplied_const(22, 29, 31, 218);
pub const PANEL_SOFT: egui::Color32 = egui::Color32::from_rgba_unmultiplied_const(30, 40, 42, 180);
pub const BORDER: egui::Color32 = egui::Color32::from_rgba_unmultiplied_const(180, 198, 190, 40);
pub const TEXT: egui::Color32 = egui::Color32::from_rgb(226, 234, 229);
pub const MUTED: egui::Color32 = egui::Color32::from_rgb(150, 166, 164);
pub const SUBTLE: egui::Color32 = egui::Color32::from_rgb(104, 122, 124);
pub const ACCENT: egui::Color32 = egui::Color32::from_rgb(118, 165, 150);
pub const ACCENT_WARM: egui::Color32 = egui::Color32::from_rgb(210, 176, 112);
pub const SUCCESS: egui::Color32 = egui::Color32::from_rgb(124, 190, 137);
pub const WARNING: egui::Color32 = egui::Color32::from_rgb(220, 184, 96);
pub const DANGER: egui::Color32 = egui::Color32::from_rgb(216, 116, 98);

pub fn apply(ctx: &egui::Context) {
    let mut style = (*ctx.global_style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.override_text_color = Some(TEXT);
    style.visuals.weak_text_color = Some(MUTED);
    style.visuals.panel_fill = BG;
    style.visuals.window_fill = PANEL;
    style.visuals.window_stroke = egui::Stroke::new(1.0, BORDER);
    style.visuals.window_corner_radius = egui::CornerRadius::same(6);
    style.visuals.menu_corner_radius = egui::CornerRadius::same(6);
    style.visuals.faint_bg_color = egui::Color32::from_rgba_unmultiplied(255, 255, 255, 8);
    style.visuals.extreme_bg_color = egui::Color32::from_rgb(13, 18, 20);
    style.visuals.text_edit_bg_color = Some(egui::Color32::from_rgb(15, 21, 23));
    style.visuals.warn_fg_color = WARNING;
    style.visuals.error_fg_color = DANGER;
    style.visuals.hyperlink_color = ACCENT;
    style.visuals.button_frame = true;

    style.visuals.widgets.noninteractive.bg_fill = PANEL;
    style.visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, BORDER);
    style.visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, TEXT);
    style.visuals.widgets.inactive.weak_bg_fill = egui::Color32::from_rgb(34, 47, 49);
    style.visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(27, 38, 40);
    style.visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, BORDER);
    style.visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, TEXT);
    style.visuals.widgets.hovered.weak_bg_fill = egui::Color32::from_rgb(49, 68, 68);
    style.visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(48, 70, 68);
    style.visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, ACCENT);
    style.visuals.widgets.active.weak_bg_fill = egui::Color32::from_rgb(69, 93, 86);
    style.visuals.widgets.active.bg_fill = egui::Color32::from_rgb(80, 112, 100);
    style.visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, ACCENT_WARM);

    for widget in [
        &mut style.visuals.widgets.noninteractive,
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
        &mut style.visuals.widgets.open,
    ] {
        widget.corner_radius = egui::CornerRadius::same(5);
    }

    style.spacing.item_spacing = egui::vec2(8.0, 7.0);
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    style.spacing.interact_size = egui::vec2(44.0, 32.0);
    ctx.set_global_style(style);
}

pub fn panel_frame() -> egui::Frame {
    egui::Frame::default()
        .fill(PANEL)
        .stroke(egui::Stroke::new(1.0, BORDER))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(18, 14))
}

pub fn compact_frame() -> egui::Frame {
    egui::Frame::default()
        .fill(PANEL_SOFT)
        .stroke(egui::Stroke::new(1.0, BORDER))
        .corner_radius(egui::CornerRadius::same(5))
        .inner_margin(egui::Margin::symmetric(10, 7))
}

pub fn toast_frame() -> egui::Frame {
    egui::Frame::default()
        .fill(egui::Color32::from_rgba_unmultiplied(38, 27, 25, 220))
        .stroke(egui::Stroke::new(
            1.0,
            egui::Color32::from_rgba_unmultiplied(240, 160, 120, 70),
        ))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(16, 8))
}

pub fn primary_button(text: &'static str) -> egui::Button<'static> {
    egui::Button::new(text)
        .fill(egui::Color32::from_rgb(54, 92, 86))
        .stroke(egui::Stroke::new(1.0, ACCENT))
}

pub fn secondary_button(text: &'static str) -> egui::Button<'static> {
    egui::Button::new(text)
        .fill(egui::Color32::from_rgb(43, 55, 60))
        .stroke(egui::Stroke::new(1.0, BORDER))
}

pub fn danger_button(text: &'static str) -> egui::Button<'static> {
    egui::Button::new(text)
        .fill(egui::Color32::from_rgb(91, 55, 48))
        .stroke(egui::Stroke::new(1.0, DANGER))
}

pub fn block_swatch(id: BlockID) -> egui::Color32 {
    match id {
        BlockID::STONE => egui::Color32::from_rgb(118, 124, 124),
        BlockID::DIRT => egui::Color32::from_rgb(126, 89, 60),
        BlockID::GRASS => egui::Color32::from_rgb(78, 135, 72),
        BlockID::SAND => egui::Color32::from_rgb(205, 184, 125),
        BlockID::WOOD => egui::Color32::from_rgb(126, 84, 45),
        BlockID::LEAVES => egui::Color32::from_rgb(58, 124, 62),
        BlockID::GLASS => egui::Color32::from_rgb(164, 208, 224),
        BlockID::WATER => egui::Color32::from_rgb(58, 125, 172),
        BlockID::STONE_BRICKS => egui::Color32::from_rgb(112, 118, 116),
        BlockID::BEDROCK => egui::Color32::from_rgb(48, 52, 56),
        _ => egui::Color32::from_rgb(200, 52, 196),
    }
}
