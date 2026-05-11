//! 客户端全局状态机 + Game 子结构定义。
//!
//! Phase 2：AppState 仅使用 Lobby / InGame；其余态留给后续 Phase。
//! Game 子结构持有 InGame 状态下的所有运行时（Server / NetEndpoint / Camera / 调度器等）。

use std::cell::RefCell;
use std::rc::Rc;

use voxweb_net::{NetEndpoint, ServerInbox};
use voxweb_server::Server;

use crate::camera::Camera;
use crate::chunk_loader::ChunkLoader;
use crate::mesh_jobs::MeshJobQueue;

/// 应用全局状态。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum AppState {
    /// 初始加载阶段（等待 wasm + 资源初始化和 WebGPU 检测）
    #[default]
    Loading,
    /// 大厅：选择单机 / 创建 / 加入
    Lobby,
    /// 正在连接信令服务（Phase 4 起使用）
    Connecting,
    /// 游戏进行中
    InGame,
    /// ESC 暂停菜单（Phase 6 起使用）
    EscMenu,
    /// 聊天输入框打开（Phase 6 起使用）
    ChatOpen,
    /// 连接断开提示（Phase 4+ 起使用）
    Disconnected,
}

/// Phase 2 游戏运行时设置。Phase 6 起会扩展为 AppSettings 全集。
#[derive(Clone, Debug)]
pub struct GameSettings {
    /// 渲染距离（单位：chunk）。默认 6，UI 可调 2..=10（Phase 6 落地）。
    pub render_distance: u32,
    /// 鼠标灵敏度。
    pub mouse_sensitivity: f32,
    /// 飞行速度（方块/秒）。
    pub fly_speed: f32,
    /// 每帧网格化预算（毫秒）。
    pub mesh_budget_ms: f32,
}

impl Default for GameSettings {
    fn default() -> Self {
        Self {
            render_distance: 6,
            mouse_sensitivity: 0.0025,
            fly_speed: 12.0,
            mesh_budget_ms: 4.0,
        }
    }
}

/// 60Hz 逻辑帧累加器。
pub struct FrameClock {
    accumulator: f32,
    step: f32,
}

impl FrameClock {
    pub fn new() -> Self {
        Self {
            accumulator: 0.0,
            step: 1.0 / 60.0,
        }
    }

    /// 累加本次 RAF 的 dt（秒）。
    pub fn accumulate(&mut self, dt: f32) {
        self.accumulator += dt;
        // 防止极端帧导致循环过长（如 tab 切到后台再回来）
        if self.accumulator > 0.25 {
            self.accumulator = 0.25;
        }
    }

    /// 若累加器 ≥ step，扣除一次返回 true。
    pub fn consume_logic_step(&mut self) -> bool {
        if self.accumulator >= self.step {
            self.accumulator -= self.step;
            true
        } else {
            false
        }
    }
}

impl Default for FrameClock {
    fn default() -> Self {
        Self::new()
    }
}

/// InGame 状态下的所有运行时资源。
pub struct Game {
    pub server: Rc<RefCell<Server>>,
    pub server_inbox: ServerInbox,
    pub net: NetEndpoint,
    pub camera: Camera,
    pub mesh_jobs: MeshJobQueue,
    pub chunk_loader: ChunkLoader,
    pub frame_clock: FrameClock,
    pub settings: GameSettings,
    /// 自己的 entity_id（由 Welcome 提供）。
    pub entity_id: u32,
}

impl Game {
    /// 启动一个单机游戏：创建 Server + 配对 NetEndpoint + 初始相机。
    /// 调用方负责后续：发 Hello、初始 chunk_loader.update。
    pub fn new_local(seed: u64, settings: GameSettings) -> Self {
        let server = Rc::new(RefCell::new(Server::new(seed)));
        let (net, server_inbox) = NetEndpoint::new_local_pair();
        let camera = Camera::default();
        let render_distance = settings.render_distance;
        Self {
            server,
            server_inbox,
            net,
            camera,
            mesh_jobs: MeshJobQueue::new(),
            chunk_loader: ChunkLoader::new(render_distance),
            frame_clock: FrameClock::new(),
            settings,
            entity_id: 0, // 待 Welcome 填充
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_clock_consume_60hz() {
        let mut fc = FrameClock::new();
        fc.accumulate(1.0 / 60.0 + 0.0001);
        assert!(fc.consume_logic_step());
        assert!(!fc.consume_logic_step());
    }

    #[test]
    fn frame_clock_caps_huge_dt() {
        let mut fc = FrameClock::new();
        fc.accumulate(10.0); // tab 切到后台
        // 累加器被限到 0.25，最多 15 个 step
        let mut steps = 0;
        while fc.consume_logic_step() {
            steps += 1;
        }
        assert!(steps <= 16, "got {steps}");
    }
}
