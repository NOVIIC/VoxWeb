//! 玩家列表 widget 与远端玩家 3D 名牌 billboard。
//!
//! Phase 6：
//! - `draw_player_list`：屏幕右上角的浮窗，显示当前房间内玩家（含主机/自己标记 + 彩色圆点）。
//! - `draw_nameplates`：把每个远端玩家头顶的名牌投影到屏幕，用 egui painter 直接画。
//!   投影遵循 wgpu / WebGPU 的 NDC 深度约定（z ∈ [0, 1]，非 OpenGL 的 [-1, 1]）。
//!
//! 数据装配由 `lib.rs` 在每渲染帧完成：
//! - `PlayerListEntry`：按 `entity_id` 升序传入，含彩色派生（`color_rgb`）。
//! - `NameplateEntry`：脚部世界坐标 + 显示名 + 玩家到相机的米距离；头顶偏移在本模块内加。

use glam::{Mat4, Vec3, Vec4};
use voxweb_core::PLAYER_HEIGHT;
use voxweb_core::protocol::EntityId;

use crate::ui::theme;

/// 单条玩家列表条目。`lib.rs` 每帧按 `entity_id` 升序构造。
pub struct PlayerListEntry {
    pub entity_id: EntityId,
    pub display_name: String,
    /// 0..=1 线性 RGB。来源于 `app::entity_color`，所有客户端确定性一致。
    pub color_rgb: [f32; 3],
    pub is_host: bool,
    pub is_me: bool,
}

/// 单个名牌的渲染输入。`lib.rs` 用 `game.interp` + `game.remote_players` + camera 装配。
pub struct NameplateEntry {
    /// 玩家脚部世界坐标。头顶偏移（PLAYER_HEIGHT + 0.3m）在本模块内统一加。
    pub world_position: Vec3,
    pub display_name: String,
    /// 玩家到相机的距离（米）；用来做距离衰减 + 远处隐藏。
    pub distance: f32,
    /// Phase 8：若玩家与相机之间被实体方块挡住，则名牌半透明淡出。
    pub occluded: bool,
}

/// 绘制右上角玩家列表。空列表也照画（显示 "在线玩家 (0)"）。
pub fn draw_player_list(ctx: &egui::Context, entries: &[PlayerListEntry]) {
    egui::Area::new(egui::Id::new("player_list"))
        .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-10.0, 10.0))
        .interactable(false)
        .show(ctx, |ui| {
            theme::compact_frame().show(ui, |ui| {
                ui.label(format!("Online Players ({})", entries.len()));
                ui.separator();
                for entry in entries {
                    let color = rgb_to_color32(entry.color_rgb);
                    let suffix = role_suffix(entry.is_host, entry.is_me);
                    ui.horizontal(|ui| {
                        // 彩色圆点：用 RichText 上彩色，不引入图片资源。
                        ui.label(egui::RichText::new("⚫").color(color));
                        if suffix.is_empty() {
                            ui.label(&entry.display_name);
                        } else {
                            ui.label(format!("{}{}", entry.display_name, suffix));
                        }
                    });
                }
            });
        });
}

/// 绘制所有远端玩家头顶的名牌 billboard。
///
/// 流程：
/// 1. 取屏幕尺寸；
/// 2. 取一个 Foreground 层 painter（保证盖在世界渲染之上）；
/// 3. 对每个 entry 计算头顶世界坐标 → 投影屏幕；
/// 4. 距离衰减 alpha；超过 32m 直接跳过。
pub fn draw_nameplates(ctx: &egui::Context, entries: &[NameplateEntry], view_proj: Mat4) {
    let screen = ctx.content_rect();
    let screen_size = (screen.width(), screen.height());
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("nameplate"),
    ));

    for entry in entries {
        let Some(mut alpha) = nameplate_alpha(entry.distance) else {
            continue;
        };
        if entry.occluded {
            alpha *= 0.25;
        }
        let head_pos = entry.world_position + Vec3::new(0.0, PLAYER_HEIGHT + 0.3, 0.0);
        let Some(screen_pos) = project_world_to_screen(view_proj, head_pos, screen_size) else {
            continue;
        };

        // 半透明底色 + 居中白字。宽度随昵称增长，避免长名字被硬裁。
        let bg_alpha = (alpha * 180.0).round().clamp(0.0, 255.0) as u8;
        let text_alpha = (alpha * 255.0).round().clamp(0.0, 255.0) as u8;
        let width = (entry.display_name.chars().count() as f32 * 8.0 + 22.0).clamp(72.0, 180.0);
        let rect = egui::Rect::from_center_size(screen_pos, egui::vec2(width, 22.0));
        painter.rect_filled(
            rect,
            egui::CornerRadius::same(4),
            egui::Color32::from_rgba_unmultiplied(22, 29, 31, bg_alpha),
        );
        painter.rect_stroke(
            rect,
            egui::CornerRadius::same(4),
            egui::Stroke::new(
                1.0,
                egui::Color32::from_rgba_unmultiplied(180, 198, 190, (alpha * 70.0) as u8),
            ),
            egui::StrokeKind::Inside,
        );
        painter.text(
            screen_pos,
            egui::Align2::CENTER_CENTER,
            &entry.display_name,
            egui::FontId::proportional(14.0),
            egui::Color32::from_rgba_unmultiplied(226, 234, 229, text_alpha),
        );
    }
}

/// 世界坐标 → 屏幕像素坐标。
///
/// 返回 `None`：
/// - 点在相机后方（`clip.w <= 0`）；
/// - NDC 深度超出 `[0, 1]`（wgpu / WebGPU 约定）。
///
/// 屏幕坐标系：左上为原点，y 向下增。
pub fn project_world_to_screen(
    view_proj: Mat4,
    world: Vec3,
    screen_size: (f32, f32),
) -> Option<egui::Pos2> {
    let clip = view_proj * Vec4::new(world.x, world.y, world.z, 1.0);
    if clip.w <= 0.0 {
        return None;
    }
    let ndc = Vec3::new(clip.x / clip.w, clip.y / clip.w, clip.z / clip.w);
    if !(0.0..=1.0).contains(&ndc.z) {
        return None;
    }
    let sx = screen_size.0 * (ndc.x * 0.5 + 0.5);
    // NDC.y 朝上为正，屏幕 y 朝下为正，所以翻转。
    let sy = screen_size.1 * (1.0 - (ndc.y * 0.5 + 0.5));
    Some(egui::pos2(sx, sy))
}

/// 根据玩家到相机的距离决定名牌的可见性与 alpha。
///
/// - `dist > 32m`：返回 `None`（完全隐藏）；
/// - `dist <= 24m`：完全不透明 `Some(1.0)`；
/// - `24m < dist <= 32m`：在 8m 衰减区间内线性从 1.0 → 0.0。
pub fn nameplate_alpha(dist: f32) -> Option<f32> {
    if dist > 32.0 {
        return None;
    }
    if dist <= 24.0 {
        return Some(1.0);
    }
    let alpha = ((32.0 - dist) / 8.0).clamp(0.0, 1.0);
    Some(alpha)
}

/// 把线性 RGB（0..=1）转为 egui `Color32`（unmultiplied，alpha=255）。
fn rgb_to_color32(rgb: [f32; 3]) -> egui::Color32 {
    let to_u8 = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    egui::Color32::from_rgb(to_u8(rgb[0]), to_u8(rgb[1]), to_u8(rgb[2]))
}

/// 拼接玩家列表显示名后缀（主机 / 你 / 主机且你 / 无）。
fn role_suffix(is_host: bool, is_me: bool) -> &'static str {
    match (is_host, is_me) {
        (true, true) => " (Host, You)",
        (true, false) => " (Host)",
        (false, true) => " (You)",
        (false, false) => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个常用的透视 + look_at 矩阵（相机在原点向 +Z 看），方便测试投影。
    fn vp_camera_at_origin_looking_to_z() -> Mat4 {
        let proj = Mat4::perspective_rh(70_f32.to_radians(), 16.0 / 9.0, 0.1, 1000.0);
        let view = Mat4::look_at_rh(Vec3::ZERO, Vec3::Z, Vec3::Y);
        proj * view
    }

    const SCREEN: (f32, f32) = (1920.0, 1080.0);

    #[test]
    fn project_returns_none_when_point_behind_camera() {
        // 相机看向 +Z，被相机方向反着的点（-Z 方向）应该在相机后方。
        let vp = vp_camera_at_origin_looking_to_z();
        let world_behind = Vec3::new(0.0, 0.0, -5.0);
        assert!(project_world_to_screen(vp, world_behind, SCREEN).is_none());
    }

    #[test]
    fn project_returns_none_when_z_outside_unit_range() {
        // 在相机正前但比 far 还远 → NDC.z 会 > 1（wgpu 约定）。
        let vp = vp_camera_at_origin_looking_to_z();
        let world_too_far = Vec3::new(0.0, 0.0, 2000.0);
        assert!(project_world_to_screen(vp, world_too_far, SCREEN).is_none());

        // 比 near 还近（注意 look_at_rh 看向 +Z，所以"前方"是 +Z 方向）：
        // 把点放在相机和 near 平面之间。near = 0.1，所以 0.05 应被裁掉。
        let world_too_close = Vec3::new(0.0, 0.0, 0.05);
        assert!(project_world_to_screen(vp, world_too_close, SCREEN).is_none());
    }

    #[test]
    fn project_happy_path_origin_target_lands_in_screen_center_ish() {
        // 相机在 (0, 0, -5) 看向原点 → 原点投影必在屏幕中央。
        let proj = Mat4::perspective_rh(70_f32.to_radians(), 16.0 / 9.0, 0.1, 1000.0);
        let view = Mat4::look_at_rh(Vec3::new(0.0, 0.0, -5.0), Vec3::ZERO, Vec3::Y);
        let vp = proj * view;
        let p = project_world_to_screen(vp, Vec3::ZERO, SCREEN)
            .expect("origin in front of camera should project");
        // 屏幕中央 ≈ (960, 540)，浮点误差允许 1px。
        assert!((p.x - 960.0).abs() < 1.0, "expected ~960, got {}", p.x);
        assert!((p.y - 540.0).abs() < 1.0, "expected ~540, got {}", p.y);
        // 同时确保整体仍在屏幕范围内（哨兵）。
        assert!(p.x >= 0.0 && p.x <= SCREEN.0);
        assert!(p.y >= 0.0 && p.y <= SCREEN.1);
    }

    #[test]
    fn nameplate_alpha_close_full_opacity() {
        assert_eq!(nameplate_alpha(10.0), Some(1.0));
        assert_eq!(nameplate_alpha(24.0), Some(1.0));
    }

    #[test]
    fn nameplate_alpha_linear_falloff_24_to_32() {
        let a = nameplate_alpha(28.0).expect("28m should be visible");
        assert!((a - 0.5).abs() < 1e-6, "expected 0.5, got {a}");
        let a = nameplate_alpha(32.0).expect("32m boundary still visible");
        assert!(a.abs() < 1e-6, "expected 0.0, got {a}");
    }

    #[test]
    fn nameplate_alpha_far_returns_none() {
        assert!(nameplate_alpha(32.1).is_none());
        assert!(nameplate_alpha(100.0).is_none());
    }
}
