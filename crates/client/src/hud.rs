use voxweb_core::block::BlockID;

use crate::app::GameMode;
use crate::camera::CameraMode;
use crate::mesh_jobs::MeshRunStats;
use crate::ui::theme;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct FramePerfStats {
    pub(super) mesh_ms: f32,
    pub(super) mesh_jobs: u32,
    pub(super) mesh_vertices: u32,
    pub(super) mesh_indices: u32,
    pub(super) mesh_phase2_vertices: u32,
    pub(super) world_pass_ms: f32,
    pub(super) depth_pass_ms: f32,
    pub(super) transparent_pass_ms: f32,
    pub(super) player_pass_ms: f32,
    pub(super) selection_pass_ms: f32,
    pub(super) egui_pass_ms: f32,
    pub(super) visible_chunks: usize,
    pub(super) culled_chunks: usize,
    pub(super) drawn_vertices: u32,
    pub(super) drawn_indices: u32,
}

impl FramePerfStats {
    pub(super) fn record_mesh(&mut self, stats: MeshRunStats) {
        self.mesh_ms = stats.elapsed_ms;
        self.mesh_jobs = stats.jobs_processed;
        self.mesh_vertices = stats.vertices_uploaded;
        self.mesh_indices = stats.indices_uploaded;
        self.mesh_phase2_vertices = stats.phase2_vertices;
    }

    fn mesh_reduction_percent(&self) -> Option<f32> {
        if self.mesh_phase2_vertices == 0 {
            return None;
        }
        Some(
            ((1.0 - self.mesh_vertices as f32 / self.mesh_phase2_vertices as f32) * 100.0).max(0.0),
        )
    }
}

#[derive(Clone)]
pub(super) struct HudData {
    pub(super) fps: f32,
    pub(super) pos: (f32, f32, f32),
    pub(super) yaw_deg: f32,
    pub(super) pitch_deg: f32,
    pub(super) pointer_locked: bool,
    pub(super) loaded_chunks: usize,
    pub(super) mesh_pending: usize,
    pub(super) mode: CameraMode,
    pub(super) on_ground: bool,
    pub(super) hotbar_items: [BlockID; 9],
    pub(super) hotbar_selected: usize,
    /// Phase 4：当前网络模式 + 房间号 + RTT。
    pub(super) game_mode: GameMode,
    pub(super) rtt_ms: Option<f32>,
    pub(super) room_id: String,
    /// 当前走信令 Worker 中继的 peer 数。> 0 时 HUD 显示「RELAY n」徽标。
    pub(super) relayed_peer_count: usize,
    /// Phase 6：[`AppSettings::show_stats`] 透传。false 时跳过左上角统计面板（保留准星 / hotbar）。
    pub(super) show_stats: bool,
    pub(super) depth_prepass_enabled: bool,
    pub(super) quota: Option<crate::storage::QuotaInfo>,
    pub(super) current_world_bytes: u64,
    pub(super) other_worlds_bytes: u64,
    pub(super) storage_error: Option<String>,
    /// Phase 7：上一帧渲染 / 网格化统计。
    pub(super) perf: FramePerfStats,
}

pub(super) fn draw_hud(ctx: &egui::Context, data: HudData) {
    // 左上角 stat（show_stats 关闭时跳过；准星 / hotbar / 提示栏照常显示）
    if data.show_stats {
        egui::Area::new(egui::Id::new("hud_topleft"))
            .anchor(egui::Align2::LEFT_TOP, egui::vec2(12.0, 12.0))
            .show(ctx, |ui| {
                theme::compact_frame().show(ui, |ui| {
                    ui.colored_label(theme::TEXT, format!("FPS  {:>5.1}", data.fps));
                    ui.colored_label(
                        theme::TEXT,
                        format!(
                            "POS  x {:+8.2}  y {:+8.2}  z {:+8.2}",
                            data.pos.0, data.pos.1, data.pos.2
                        ),
                    );
                    ui.colored_label(
                        theme::MUTED,
                        format!("YAW {:+6.1}°  PITCH {:+5.1}°", data.yaw_deg, data.pitch_deg),
                    );
                    let mode_str = match data.mode {
                        CameraMode::Walk => "Walk",
                        CameraMode::Fly => "Fly",
                    };
                    ui.colored_label(
                        theme::SUCCESS,
                        format!(
                            "MODE {}  {}",
                            mode_str,
                            if data.on_ground { "[ground]" } else { "" }
                        ),
                    );
                    ui.colored_label(
                        theme::MUTED,
                        format!(
                            "CHUNKS {}  MESH_Q {}",
                            data.loaded_chunks, data.mesh_pending
                        ),
                    );
                    ui.colored_label(
                        theme::MUTED,
                        format!(
                            "DEPTH_PRE {}",
                            if data.depth_prepass_enabled {
                                "ON"
                            } else {
                                "OFF"
                            }
                        ),
                    );
                    if let Some(q) = data.quota {
                        let available_for_world = q.quota.saturating_sub(data.other_worlds_bytes);
                        let ratio = if available_for_world == 0 {
                            1.0
                        } else {
                            data.current_world_bytes as f32 / available_for_world as f32
                        };
                        let color = if ratio > 0.95 {
                            theme::DANGER
                        } else if ratio > 0.80 {
                            theme::WARNING
                        } else {
                            theme::SUCCESS
                        };
                        ui.colored_label(
                            color,
                            format!(
                                "SAVE {} / {}",
                                crate::storage::format_storage_bytes(data.current_world_bytes),
                                crate::storage::format_storage_bytes(available_for_world)
                            ),
                        );
                    }
                    if let Some(err) = data.storage_error.as_deref() {
                        ui.colored_label(theme::DANGER, format!("SAVE ERR {err}"));
                    }
                    ui.colored_label(
                        theme::MUTED,
                        format!(
                            "VISIBLE {}  CULLED {}  DRAW_V/I {}/{}",
                            data.perf.visible_chunks,
                            data.perf.culled_chunks,
                            data.perf.drawn_vertices,
                            data.perf.drawn_indices
                        ),
                    );
                    let reduction = data
                        .perf
                        .mesh_reduction_percent()
                        .map(|v| format!("{v:>5.1}%"))
                        .unwrap_or_else(|| "  -- ".to_string());
                    ui.colored_label(
                        theme::ACCENT,
                        format!(
                            "MESH {:>4.1}ms  jobs {}  v {}→{}  i {}  -{}",
                            data.perf.mesh_ms,
                            data.perf.mesh_jobs,
                            data.perf.mesh_phase2_vertices,
                            data.perf.mesh_vertices,
                            data.perf.mesh_indices,
                            reduction
                        ),
                    );
                    ui.colored_label(
                        theme::ACCENT,
                        format!(
                            "PASS depth {:>4.1}  world {:>4.1}  player {:>4.1}  trans {:>4.1}  sel {:>4.1}  ui {:>4.1} ms",
                            data.perf.depth_pass_ms,
                            data.perf.world_pass_ms,
                            data.perf.player_pass_ms,
                            data.perf.transparent_pass_ms,
                            data.perf.selection_pass_ms,
                            data.perf.egui_pass_ms
                        ),
                    );
                    let mode_str = match data.game_mode {
                        GameMode::Local => "LOCAL",
                        GameMode::Host => "HOST",
                        GameMode::Remote => "REMOTE",
                    };
                    let rtt_str = match data.rtt_ms {
                        Some(rtt) => format!("{rtt:>5.1} ms"),
                        None => "  --  ".to_string(),
                    };
                    let room_str = if data.room_id.is_empty() {
                        String::new()
                    } else {
                        format!("  ROOM {}", data.room_id)
                    };
                    ui.colored_label(
                        theme::ACCENT_WARM,
                        format!("NET {mode_str}  RTT {rtt_str}{room_str}"),
                    );
                    if data.relayed_peer_count > 0 {
                        ui.colored_label(
                            theme::WARNING,
                            format!("RELAY {} peer(s) (relaying)", data.relayed_peer_count),
                        );
                    }
                });
            });
    }

    egui::Area::new(egui::Id::new("hud_crosshair"))
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::hover());
            let center = rect.center();
            let stroke = egui::Stroke::new(
                1.5,
                egui::Color32::from_rgba_unmultiplied(240, 248, 244, 210),
            );
            ui.painter().line_segment(
                [
                    egui::pos2(center.x - 7.0, center.y),
                    egui::pos2(center.x - 2.0, center.y),
                ],
                stroke,
            );
            ui.painter().line_segment(
                [
                    egui::pos2(center.x + 2.0, center.y),
                    egui::pos2(center.x + 7.0, center.y),
                ],
                stroke,
            );
            ui.painter().line_segment(
                [
                    egui::pos2(center.x, center.y - 7.0),
                    egui::pos2(center.x, center.y - 2.0),
                ],
                stroke,
            );
            ui.painter().line_segment(
                [
                    egui::pos2(center.x, center.y + 2.0),
                    egui::pos2(center.x, center.y + 7.0),
                ],
                stroke,
            );
        });

    egui::Area::new(egui::Id::new("hud_hint"))
        .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -80.0))
        .show(ctx, |ui| {
            let msg = if data.pointer_locked {
                "WASD walk | Space jump (×2 = fly) | LMB break | RMB place | 1-9 hotbar | ESC release"
            } else {
                "Click to enter camera control"
            };
            theme::compact_frame().show(ui, |ui| {
                ui.add(
                    egui::Label::new(egui::RichText::new(msg).color(theme::TEXT))
                        .wrap_mode(egui::TextWrapMode::Extend),
                );
            });
        });

    egui::Area::new(egui::Id::new("hud_hotbar"))
        .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -16.0))
        .show(ctx, |ui| {
            theme::compact_frame().show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for (i, block) in data.hotbar_items.iter().enumerate() {
                        let selected = i == data.hotbar_selected;
                        let label = crate::hotbar::block_label(*block);
                        let bg = if selected {
                            egui::Color32::from_rgba_unmultiplied(220, 188, 112, 230)
                        } else {
                            egui::Color32::from_rgba_unmultiplied(35, 47, 50, 220)
                        };
                        let fg = if selected {
                            egui::Color32::from_rgb(20, 24, 22)
                        } else {
                            theme::TEXT
                        };
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(58.0, 42.0), egui::Sense::hover());
                        let painter = ui.painter();
                        painter.rect_filled(rect, egui::CornerRadius::same(5), bg);
                        painter.rect_stroke(
                            rect,
                            egui::CornerRadius::same(5),
                            egui::Stroke::new(
                                if selected { 2.0 } else { 1.0 },
                                if selected {
                                    theme::ACCENT_WARM
                                } else {
                                    theme::BORDER
                                },
                            ),
                            egui::StrokeKind::Inside,
                        );
                        let swatch = egui::Rect::from_min_size(
                            rect.min + egui::vec2(7.0, 7.0),
                            egui::vec2(14.0, 14.0),
                        );
                        painter.rect_filled(
                            swatch,
                            egui::CornerRadius::same(3),
                            theme::block_swatch(*block),
                        );
                        painter.text(
                            egui::pos2(rect.center().x + 4.0, rect.min.y + 14.0),
                            egui::Align2::CENTER_CENTER,
                            format!("{}", i + 1),
                            egui::FontId::proportional(12.0),
                            fg,
                        );
                        painter.text(
                            egui::pos2(rect.center().x, rect.max.y - 11.0),
                            egui::Align2::CENTER_CENTER,
                            label,
                            egui::FontId::proportional(10.0),
                            fg,
                        );
                    }
                });
            });
        });
}

/// 在屏幕顶部居中绘制通知浮窗（信令错误等），5 秒自动消失。
/// 多条通知从上到下堆叠，半透明深色背景 + 橙红色文字。
pub(super) fn draw_toast_notifications(ctx: &egui::Context, messages: &[String]) {
    egui::Area::new(egui::Id::new("toast_notifications"))
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 60.0))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                for msg in messages {
                    theme::toast_frame().show(ui, |ui| {
                        ui.set_max_width(420.0);
                        ui.label(egui::RichText::new(msg).color(theme::DANGER).size(14.0));
                    });
                    ui.add_space(4.0);
                }
            });
        });
}
