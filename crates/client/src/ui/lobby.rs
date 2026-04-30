//! 大厅 UI：选择单人模式 / 创建房间 / 加入房间。

/// 绘制大厅 UI（Phase 1+ 实现）。
pub fn draw_lobby(ui: &mut egui::Ui) {
    // Phase 1: 三个按钮 + 房间号输入框
    ui.centered_and_justified(|ui| {
        ui.label("VoxWeb 大厅");
    });
}
