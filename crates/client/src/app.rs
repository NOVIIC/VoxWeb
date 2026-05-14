//! 客户端全局状态机 + Game 子结构定义。
//!
//! Phase 3：Game 持有 LocalPhysics（驱动 camera.position）、Hotbar、PendingActions、
//! current_hit（DDA 射线命中缓存）等运行时状态。
//! Phase 4：Game 增加 [`GameMode`] 区分 Local / Host / Remote，并补 RTT 计时字段。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use glam::Vec3;
use voxweb_core::protocol::EntityId;
use voxweb_net::{NetEndpoint, NetError, ServerInbox};
use voxweb_server::Server;

use crate::camera::Camera;
use crate::chunk_assembler::ChunkAssembler;
use crate::chunk_loader::ChunkLoader;
use crate::hotbar::Hotbar;
use crate::interp::PlayerInterp;
use crate::mesh_jobs::MeshJobQueue;
use crate::physics::LocalPhysics;
use crate::prediction::{InputHistory, PendingActions};
use crate::raycast::RaycastHit;

/// 区块预载进度（进入游戏前的最后一项加载步骤）。
#[derive(Clone, Debug, Default)]
pub struct PreloadState {
    /// 出生点渲染范围内预期的总区块数。
    pub total: usize,
    /// 已生成/接收到的区块数（存在于 world.chunks 中）。
    pub received: usize,
    /// 已完成 GPU 网格化的区块数。
    pub meshed: usize,
    /// 预载阶段是否进行中。
    pub active: bool,
}

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

/// 当前 Game 实例的网络模式（决定主循环要不要 server.tick / chunk_loader 等）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GameMode {
    /// 单机：内部 mpsc，server 全权处理。
    Local,
    /// 房主：本地 mpsc + 信令 + 多 Remote PC。本地 server 仍正常 tick。
    Host,
    /// 远端客户端：仅持有 PeerConnection 到 Host。Phase 4 仍跑本地 server 做 placeholder
    /// （世界与 Host 不同步，Phase 5 改为 Host 推送）。
    Remote,
}

/// 远端玩家运行时状态（渲染 + UI 用）。
#[derive(Clone, Debug)]
pub struct RemotePlayerState {
    pub display_name: String,
    /// 最近一次收到 PlayerTick 中该玩家的 server tick。
    pub last_seen_tick: u32,
    /// 确定性派生颜色（entity_id → HSV → RGB），同一玩家在所有终端颜色一致。
    pub color_rgb: [f32; 3],
}

impl RemotePlayerState {
    pub fn new(display_name: String, entity_id: EntityId) -> Self {
        Self {
            display_name,
            last_seen_tick: 0,
            color_rgb: entity_color(entity_id),
        }
    }
}

/// 按 entity_id 派生一个 HSV 颜色 → RGB。确定性函数，所有客户端一致。
fn entity_color(eid: EntityId) -> [f32; 3] {
    // 简单 hash：Gold ratio multiplier
    let h = (eid.wrapping_mul(2_654_435_761)) as f32 / u32::MAX as f32; // hue ∈ [0, 1)
    hsv_to_rgb(h, 0.7, 0.9)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [f32; 3] {
    let c = v * s;
    let x = c * (1.0 - ((h * 6.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match (h * 6.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [r + m, g + m, b + m]
}

/// Phase 3 游戏运行时设置。Phase 6 起会扩展为 AppSettings 全集。
#[derive(Clone, Debug)]
pub struct GameSettings {
    /// 渲染距离（单位：chunk）。默认 6，UI 可调 2..=10（Phase 6 落地）。
    pub render_distance: u32,
    /// 鼠标灵敏度。
    pub mouse_sensitivity: f32,
    /// Fly 模式速度（方块/秒）；Phase 3 起 Walk 速度走 physics 常量。
    pub fly_speed: f32,
    /// 每帧网格化预算（毫秒）。
    pub mesh_budget_ms: f32,
    /// 挖掘连续触发的冷却（毫秒）。
    pub min_action_interval_ms: f64,
}

impl Default for GameSettings {
    fn default() -> Self {
        Self {
            render_distance: 6,
            mouse_sensitivity: 0.0025,
            fly_speed: 12.0,
            mesh_budget_ms: 4.0,
            min_action_interval_ms: 100.0,
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
    pub mode: GameMode,
    pub server: Rc<RefCell<Server>>,
    pub server_inbox: ServerInbox,
    pub net: NetEndpoint,
    pub camera: Camera,
    pub physics: LocalPhysics,
    pub hotbar: Hotbar,
    pub pending: PendingActions,
    pub mesh_jobs: MeshJobQueue,
    pub chunk_loader: ChunkLoader,
    pub frame_clock: FrameClock,
    pub settings: GameSettings,
    /// DDA 命中缓存（每帧更新；HUD 线框 + 挖放动作消费）。
    pub current_hit: Option<RaycastHit>,
    /// 上次挖掘成功时间（performance.now()，毫秒），用于连续挖掘冷却。
    pub last_break_at_ms: f64,
    /// 自己的 entity_id（由 Welcome 或 add_player 提供）。
    pub entity_id: u32,
    /// Phase 4：RTT（毫秒）。`None` 表示未测过 / 上次 Ping 还没回。Local 模式永远 None。
    pub rtt_ms: Option<f32>,
    /// 上次发 Ping 的 performance.now() 毫秒。0 表示从未发过。
    pub last_ping_sent_ms: f64,
    /// 待响应的 Ping 集合：client_time_ms → 发送时刻 performance.now() ms。
    pub pending_pings: HashMap<u64, f64>,
    /// 房间号（Host/Remote 模式有效；Local 留空）。
    pub room_id: String,
    // ── Phase 5 新字段 ──
    /// 远端玩家实体表（PeerJoined 插入，PeerLeft 移除）。
    pub remote_players: HashMap<EntityId, RemotePlayerState>,
    /// 远端玩家位置插值器（PlayerTick 摄入，每渲染帧 advance）。
    pub interp: PlayerInterp,
    /// Chunk 快照接收组装器（Remote 端用，Host/Local 闲置）。
    pub chunk_assembler: ChunkAssembler,
    /// 本地位置预测的输入历史（60Hz 推入，PlayerTick reconcile 时修剪）。
    pub input_history: InputHistory,
    /// Host 时钟与本地时钟的瞬态偏移（ms）：server_time_ms - local_now_ms。
    /// PlayerTick 每帧覆盖；远端的 rendering target 用。
    pub server_clock_offset_ms: i64,
}

impl Game {
    /// 启动一个单机游戏：创建 Server + 配对 NetEndpoint + 初始相机/物理。
    /// Phase 5：构造时立即调 `server.add_player(display_name)` 把 Host 本人入表，
    /// 丢弃随之产生的初始 outbox（Welcome/PeerJoined/ChunkSnapshot — 对自己冗余）。
    pub fn new_local(seed: u64, settings: GameSettings, display_name: &str) -> Self {
        let server = Rc::new(RefCell::new(Server::new(seed)));
        let eid = {
            let mut s = server.borrow_mut();
            let id = s.add_player(display_name.to_string());
            let _ = s.drain_outbox();
            id
        };
        let (net, server_inbox) = NetEndpoint::new_local_pair();
        let mut game = Self::assemble(
            GameMode::Local,
            server,
            server_inbox,
            net,
            settings,
            String::new(),
        );
        game.entity_id = eid;
        game
    }

    /// 启动一个 Host 游戏：本地仍跑 Server（Local 风格），同时连信令接受 Remote。
    /// Phase 5：与 Local 同样调 add_player；额外把 eid 注册给 net 端做后续路由。
    pub fn new_host(
        seed: u64,
        settings: GameSettings,
        signaling_url: &str,
        room_id: &str,
        display_name: &str,
    ) -> Result<Self, NetError> {
        let server = Rc::new(RefCell::new(Server::new(seed)));
        let eid = {
            let mut s = server.borrow_mut();
            let id = s.add_player(display_name.to_string());
            let _ = s.drain_outbox();
            id
        };
        let (mut net, server_inbox) = NetEndpoint::new_host(signaling_url, room_id, display_name)?;
        net.host_set_self_entity(eid);
        let mut game = Self::assemble(
            GameMode::Host,
            server,
            server_inbox,
            net,
            settings,
            room_id.to_string(),
        );
        game.entity_id = eid;
        Ok(game)
    }

    /// 启动一个 Remote 客户端：连信令、等 Host SDP。
    /// Phase 5：Remote 端 `server` 是**纯方块数据宿主**（接收 ChunkSnapshot / BlockUpdate 写入），
    /// 不调 add_player / tick / handle_message — 自身 entity_id 由 Welcome 填回。
    pub fn new_remote(
        settings: GameSettings,
        signaling_url: &str,
        room_id: &str,
        display_name: &str,
    ) -> Result<Self, NetError> {
        let server = Rc::new(RefCell::new(Server::new(0)));
        // server_inbox 在 Remote 模式不参与驱动；为保持 Game 字段不可空，造一对空 mpsc
        let (_net_local, dummy_inbox) = NetEndpoint::new_local_pair();
        let net = NetEndpoint::new_remote(signaling_url, room_id, display_name)?;
        Ok(Self::assemble(
            GameMode::Remote,
            server,
            dummy_inbox,
            net,
            settings,
            room_id.to_string(),
        ))
    }

    fn assemble(
        mode: GameMode,
        server: Rc<RefCell<Server>>,
        server_inbox: ServerInbox,
        net: NetEndpoint,
        settings: GameSettings,
        room_id: String,
    ) -> Self {
        let camera = Camera::default();
        let physics = LocalPhysics::new(Vec3::new(8.0, 100.0, 8.0));
        let render_distance = settings.render_distance;
        Self {
            mode,
            server,
            server_inbox,
            net,
            camera,
            physics,
            hotbar: Hotbar::default(),
            pending: PendingActions::new(),
            mesh_jobs: MeshJobQueue::new(),
            chunk_loader: ChunkLoader::new(render_distance),
            frame_clock: FrameClock::new(),
            settings,
            current_hit: None,
            last_break_at_ms: 0.0,
            entity_id: 0, // 由 add_player（Local/Host）或 Welcome（Remote）填
            rtt_ms: None,
            last_ping_sent_ms: 0.0,
            pending_pings: HashMap::new(),
            room_id,
            // Phase 5
            remote_players: HashMap::new(),
            interp: PlayerInterp::new(),
            chunk_assembler: ChunkAssembler::new(),
            input_history: InputHistory::new(120),
            server_clock_offset_ms: 0,
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
