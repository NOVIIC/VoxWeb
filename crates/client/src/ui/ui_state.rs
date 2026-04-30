//! UI 全局状态（egui 上下文、窗口尺寸缓存等）。

/// 全局 UI 辅助状态。
#[derive(Default)]
pub struct UiState {
    /// 聊天历史
    pub chat_history: Vec<String>,
    /// HUD 可见性
    pub show_hud: bool,
    /// 性能统计 overlay
    pub show_stats: bool,
}
