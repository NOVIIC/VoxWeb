//! 客户端全局状态机。
//! 定义所有应用状态及合法转换路径。

/// 应用全局状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppState {
    /// 初始加载阶段（等待 wasm + 资源初始化和 WebGPU 检测）
    Loading,
    /// 大厅：选择单机 / 创建 / 加入
    Lobby,
    /// 正在连接信令服务（创建/加入房间）
    Connecting,
    /// 游戏进行中（指针锁锁定，HUD 显示）
    InGame,
    /// ESC 暂停菜单（指针释放，游戏逻辑暂停）
    EscMenu,
    /// 聊天输入框打开（指针释放，游戏继续）
    ChatOpen,
    /// 连接断开提示
    Disconnected,
}

impl Default for AppState {
    fn default() -> Self {
        AppState::Loading
    }
}
