//! 客户端全局状态机 + Game 子结构定义。
//!
//! Phase 3：Game 持有 LocalPhysics（驱动 camera.position）、Hotbar、PendingActions、
//! current_hit（DDA 射线命中缓存）等运行时状态。
//! Phase 4：Game 增加 [`GameMode`] 区分 Local / Host / Remote，并补 RTT 计时字段。
//! Phase 6：AppState::EscMenu / ChatOpen 升级为 `InGame { paused, chat_open }` 子状态位；
//! Game 加入 `display_name` / `host_entity_id` / `chat` 字段；`GameSettings` 重命名为
//! `AppSettings`，加 FOV / 灵敏度倍率 / 插值延迟 / 显示统计字段并支持 serde 持久化。

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::rc::Rc;

use glam::Vec3;
use serde::{Deserialize, Serialize};
use voxweb_core::protocol::EntityId;
use voxweb_net::{NetEndpoint, NetError, ServerInbox};
use voxweb_server::Server;

use crate::camera::Camera;
use crate::chat::ChatHistory;
use crate::chunk_assembler::ChunkAssembler;
use crate::chunk_loader::ChunkLoader;
use crate::hotbar::Hotbar;
use crate::interp::PlayerInterp;
use crate::mesh_jobs::MeshJobQueue;
use crate::physics::LocalPhysics;
use crate::prediction::{InputHistory, PendingActions};
use crate::raycast::RaycastHit;
use crate::storage::{OpfsStorage, QuotaInfo};

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
///
/// Phase 6：原 `EscMenu` / `ChatOpen` unit variant 合入 `InGame { paused, chat_open }`，
/// 让 HUD 与名牌可以无视暂停/聊天叠加层始终绘制，且暂停与聊天可并存。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum AppState {
    /// 大厅：选择单机 / 创建 / 加入
    #[default]
    Lobby,
    /// 正在连接信令服务（Phase 4 起使用）
    Connecting,
    /// 游戏进行中（可叠加暂停 / 聊天两个子状态）
    InGame {
        /// ESC 暂停菜单是否展开
        paused: bool,
        /// 聊天输入框是否聚焦
        chat_open: bool,
    },
    /// 连接断开提示（Phase 4+ 起使用；Phase 6 起为独立可见页面而非直接跳回 Lobby）
    Disconnected,
}

impl AppState {
    /// 是否处于 InGame 系列状态（无视 paused / chat_open 子位）。
    /// 用于替换 Phase 5 时代的 `state == AppState::InGame` 等值比较。
    pub fn is_in_game(&self) -> bool {
        matches!(self, AppState::InGame { .. })
    }

    /// 默认进入 InGame 的初始状态（无暂停、无聊天）。
    pub fn ingame_default() -> Self {
        AppState::InGame {
            paused: false,
            chat_open: false,
        }
    }
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
pub fn entity_color(eid: EntityId) -> [f32; 3] {
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

/// 鼠标灵敏度倍率的底数（弧度 / 像素）。
/// `AppSettings::mouse_sensitivity` 是面向用户的"倍数"（默认 1.0，范围 0.1..=5.0），
/// 实际乘到该常量上作为相机 yaw / pitch 的弧度增量。
pub const BASE_SENSITIVITY_RAD_PER_PIXEL: f32 = 0.0025;

/// 用户设置（Phase 6 起；JSON 序列化到 localStorage）。
///
/// Phase 5 时期的 `GameSettings` 字段被拆分：
/// - 用户面向的（FOV / 灵敏度 / 渲染距离 / 插值延迟 / 显示统计）走 serde 持久化。
/// - 开发/调优字段（`fly_speed` / `mesh_budget_ms` / `min_action_interval_ms`）标 `#[serde(skip)]`，
///   仅运行时使用，不污染存储。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppSettings {
    /// 视野角度（度），范围 30..=110。
    pub fov_degrees: f32,
    /// 鼠标灵敏度倍率（0.1..=5.0），消费端乘以 [`BASE_SENSITIVITY_RAD_PER_PIXEL`]。
    pub mouse_sensitivity: f32,
    /// 渲染距离（单位：chunk）。范围 2..=10，离散选项 2/4/6/8/10。
    pub render_distance: u32,
    /// 远端玩家位置插值延迟（毫秒）。选项 50 / 100 / 150。
    pub interp_delay_ms: f32,
    /// 是否显示左上角统计 HUD（暂停菜单切换）。
    pub show_stats: bool,
    /// Phase 8：是否启用 Depth Pre-Pass。
    #[serde(default = "default_depth_prepass_enabled")]
    pub depth_prepass_enabled: bool,
    /// Phase 8：server world 内存 chunk cache 容量。
    #[serde(default = "default_chunk_cache_capacity")]
    pub chunk_cache_capacity: usize,

    /// Fly 模式速度（方块/秒）；不持久化。
    #[serde(skip, default = "default_fly_speed")]
    pub fly_speed: f32,
    /// 每帧网格化预算（毫秒）；不持久化。
    #[serde(skip, default = "default_mesh_budget_ms")]
    pub mesh_budget_ms: f32,
    /// 挖掘连续触发的冷却（毫秒）；不持久化。
    #[serde(skip, default = "default_min_action_interval_ms")]
    pub min_action_interval_ms: f64,
}

fn default_fly_speed() -> f32 {
    12.0
}
fn default_mesh_budget_ms() -> f32 {
    4.0
}
fn default_min_action_interval_ms() -> f64 {
    100.0
}
fn default_depth_prepass_enabled() -> bool {
    true
}
fn default_chunk_cache_capacity() -> usize {
    voxweb_server::world::DEFAULT_CHUNK_CACHE_CAPACITY
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            fov_degrees: 70.0,
            mouse_sensitivity: 1.0,
            render_distance: 6,
            interp_delay_ms: 100.0,
            show_stats: true,
            depth_prepass_enabled: default_depth_prepass_enabled(),
            chunk_cache_capacity: default_chunk_cache_capacity(),
            fly_speed: default_fly_speed(),
            mesh_budget_ms: default_mesh_budget_ms(),
            min_action_interval_ms: default_min_action_interval_ms(),
        }
    }
}

impl PartialEq for AppSettings {
    fn eq(&self, other: &Self) -> bool {
        self.fov_degrees == other.fov_degrees
            && self.mouse_sensitivity == other.mouse_sensitivity
            && self.render_distance == other.render_distance
            && self.interp_delay_ms == other.interp_delay_ms
            && self.show_stats == other.show_stats
            && self.depth_prepass_enabled == other.depth_prepass_enabled
            && self.chunk_cache_capacity == other.chunk_cache_capacity
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

    /// 本帧尚未消耗的逻辑时间（秒），用于把权威状态外推到渲染时刻。
    pub fn remainder_secs(&self) -> f32 {
        self.accumulator.clamp(0.0, self.step)
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
    pub settings: AppSettings,
    /// DDA 命中缓存（每帧更新；HUD 线框 + 挖放动作消费）。
    pub current_hit: Option<RaycastHit>,
    /// 上次挖掘成功时间（performance.now()，毫秒），用于连续挖掘冷却。
    pub last_break_at_ms: f64,
    /// 自己的 entity_id（由 Welcome 或 add_player 提供）。
    pub entity_id: u32,
    /// Phase 6：自己的显示名（由 lobby 注入；用于玩家列表 / 聊天自身回显）。
    pub display_name: String,
    /// Phase 6：房间主机的 entity_id（Local-Only / Host 模式 = 自己；Remote 模式由 Welcome 填）。
    /// 0 表示尚未知晓（Remote 端 Welcome 到达前的瞬态）。
    pub host_entity_id: EntityId,
    /// Host 允许 Remote 使用的最大视距。Remote 在 Welcome/HostSettings 后填入；
    /// Local/Host 模式等于自己的设置。
    pub host_render_distance: u32,
    /// Phase 6：聊天历史（含本地合成的系统消息）。
    pub chat: ChatHistory,
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
    /// 本地生成的 PlayerInput 序号。Remote 模式也独立递增，不能借用本地 dummy server tick。
    pub local_input_tick: u32,
    /// 本地位置预测的输入历史（60Hz 推入，PlayerTick reconcile 时修剪）。
    pub input_history: InputHistory,
    /// 中等位置误差的剩余软修正量。每个渲染帧应用一小段，避免高 RTT 下画面回弹。
    pub pending_position_correction: Vec3,
    /// Host 时钟与本地时钟的平滑偏移（ms）：server_time_ms - local_now_ms。
    /// Pong 按半 RTT 估算，PlayerTick 提供低权重补充样本；远端插值 target 使用它。
    pub server_clock_offset_ms: f64,
    /// 是否已经吃过至少一条 Host 时钟样本。第一条样本直接采用，后续再指数平滑。
    pub server_clock_synced: bool,
    /// Phase 8：Local/Host 的 OPFS 存储句柄。Remote 不写存档。
    pub storage: Option<OpfsStorage>,
    pub known_persisted: HashSet<voxweb_core::ChunkPos>,
    /// 已经从 OPFS 覆盖进运行时世界的 chunk，避免远距离存档重复异步读取。
    pub loaded_persisted_chunks: HashSet<voxweb_core::ChunkPos>,
    /// 已发起但尚未完成的 OPFS chunk 读取。
    pub pending_persisted_loads: HashSet<voxweb_core::ChunkPos>,
    /// 当前世界在 OPFS 中已落盘的字节数（world.json + chunk 文件）。
    pub current_world_bytes: u64,
    /// 除当前世界以外，世界列表中其它存档的总字节数。
    pub other_worlds_bytes: u64,
    /// 当前世界每个已落盘 chunk 文件的大小，保存成功后用于增量修正 HUD 数据。
    pub persisted_chunk_sizes: HashMap<voxweb_core::ChunkPos, u64>,
    /// 暂停菜单 "Save Now" 请求下一帧持久化泵立即 flush，并在完成后显示提示。
    pub save_now_requested: bool,
    pub last_persist_ms: f64,
    pub quota: Option<QuotaInfo>,
    pub storage_error: Option<String>,
    /// Remote：最近一次收到 FreeObjectState 的本地时刻（ms），用于渲染外推。
    pub free_object_state_at_ms: HashMap<voxweb_core::ObjectID, f64>,
}

impl Game {
    /// 启动一个单机游戏：创建 Server + 配对 NetEndpoint + 初始相机/物理。
    /// Phase 5：构造时立即调 `server.add_player(display_name)` 把 Host 本人入表，
    /// 丢弃随之产生的初始 outbox（Welcome/PeerJoined/FieldSnapshot — 对自己冗余）。
    pub fn new_local(seed: u64, settings: AppSettings, display_name: &str) -> Self {
        let server = Rc::new(RefCell::new(Server::new(seed)));
        let eid = {
            let mut s = server.borrow_mut();
            s.set_host_render_distance(settings.render_distance);
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
            display_name.to_string(),
            String::new(),
        );
        game.entity_id = eid;
        // Local-Only：自己即 Host，立即填 host_entity_id 让 UI 不显示"未知主机"瞬态。
        game.host_entity_id = eid;
        game.host_render_distance = game.settings.render_distance;
        game
    }

    /// 启动一个 Host 游戏：本地仍跑 Server（Local 风格），同时连信令接受 Remote。
    /// Phase 5：与 Local 同样调 add_player；额外把 eid 注册给 net 端做后续路由。
    pub fn new_host(
        seed: u64,
        settings: AppSettings,
        signaling_url: &str,
        room_id: &str,
        display_name: &str,
    ) -> Result<Self, NetError> {
        let server = Rc::new(RefCell::new(Server::new(seed)));
        let eid = {
            let mut s = server.borrow_mut();
            s.set_host_render_distance(settings.render_distance);
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
            display_name.to_string(),
            room_id.to_string(),
        );
        game.entity_id = eid;
        // Host：自己即 Host，host_entity_id 直接填。
        game.host_entity_id = eid;
        game.host_render_distance = game.settings.render_distance;
        Ok(game)
    }

    /// 启动一个 Remote 客户端：连信令、等 Host SDP。
    /// Phase 5：Remote 端 `server` 是**纯方块数据宿主**（接收 FieldSnapshot / FieldDelta 写入），
    /// 不调 add_player / tick / handle_message — 自身 entity_id 由 Welcome 填回。
    pub fn new_remote(
        settings: AppSettings,
        signaling_url: &str,
        room_id: &str,
        display_name: &str,
    ) -> Result<Self, NetError> {
        let server = Rc::new(RefCell::new(Server::new(0)));
        // server_inbox 在 Remote 模式不参与驱动；为保持 Game 字段不可空，造一对空 mpsc
        let (_net_local, dummy_inbox) = NetEndpoint::new_local_pair();
        let net = NetEndpoint::new_remote(signaling_url, room_id, display_name)?;
        let mut game = Self::assemble(
            GameMode::Remote,
            server,
            dummy_inbox,
            net,
            settings,
            display_name.to_string(),
            room_id.to_string(),
        );
        let spawn_center = crate::chunk_loader::chunk_pos_of(voxweb_server::DEFAULT_SPAWN);
        let bootstrap_radius = game
            .chunk_loader
            .render_distance
            .min(voxweb_server::INITIAL_SNAPSHOT_RADIUS);
        game.chunk_loader
            .mark_requested_square(spawn_center, bootstrap_radius);
        Ok(game)
    }

    fn assemble(
        mode: GameMode,
        server: Rc<RefCell<Server>>,
        server_inbox: ServerInbox,
        net: NetEndpoint,
        settings: AppSettings,
        display_name: String,
        room_id: String,
    ) -> Self {
        // Phase 6：FOV / 插值延迟从 AppSettings 派生（之前是硬编码）。
        let camera = Camera {
            fov: settings.fov_degrees.to_radians(),
            ..Camera::default()
        };
        let physics = LocalPhysics::new(Vec3::new(8.0, 100.0, 8.0));
        let render_distance = settings.render_distance;
        let mut interp = PlayerInterp::new();
        interp.set_delay_ms(settings.interp_delay_ms as f64);
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
            display_name,
            host_entity_id: 0, // Local/Host 模式在 new_* 里立即填；Remote 等 Welcome 回填
            host_render_distance: 0,
            chat: ChatHistory::default(),
            rtt_ms: None,
            last_ping_sent_ms: 0.0,
            pending_pings: HashMap::new(),
            room_id,
            // Phase 5
            remote_players: HashMap::new(),
            interp,
            chunk_assembler: ChunkAssembler::new(),
            local_input_tick: 0,
            input_history: InputHistory::new(120),
            pending_position_correction: Vec3::ZERO,
            server_clock_offset_ms: 0.0,
            server_clock_synced: false,
            storage: None,
            known_persisted: HashSet::new(),
            loaded_persisted_chunks: HashSet::new(),
            pending_persisted_loads: HashSet::new(),
            current_world_bytes: 0,
            other_worlds_bytes: 0,
            persisted_chunk_sizes: HashMap::new(),
            save_now_requested: false,
            last_persist_ms: 0.0,
            quota: None,
            storage_error: None,
            free_object_state_at_ms: HashMap::new(),
        }
    }

    /// 在 [`crate::ui::pause`] 暂停菜单修改 [`AppSettings`] 后调用，
    /// 把 FOV / 插值延迟 / 渲染距离等设置重新应用到对应运行时组件。
    pub fn apply_settings(&mut self) {
        self.camera.fov = self.settings.fov_degrees.to_radians();
        self.interp
            .set_delay_ms(self.settings.interp_delay_ms as f64);
        self.chunk_loader
            .set_render_distance(self.effective_render_distance());
        {
            let mut server = self.server.borrow_mut();
            server
                .world
                .set_chunk_cache_capacity(self.settings.chunk_cache_capacity);
            if matches!(self.mode, GameMode::Local | GameMode::Host) {
                server.set_host_render_distance(self.settings.render_distance);
                self.host_render_distance = self.settings.render_distance;
            }
        }
    }

    /// 当前实际使用的区块视距。
    ///
    /// Remote 端不能超过 Host 视距；Local/Host 端直接使用自己的设置。
    pub fn effective_render_distance(&self) -> u32 {
        if self.mode == GameMode::Remote && self.host_render_distance > 0 {
            self.settings.render_distance.min(self.host_render_distance)
        } else {
            self.settings.render_distance
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

    // ── Phase 6 ──

    #[test]
    fn appstate_is_in_game_matches_both_subflag_combos() {
        assert!(AppState::ingame_default().is_in_game());
        assert!(
            AppState::InGame {
                paused: true,
                chat_open: false
            }
            .is_in_game()
        );
        assert!(
            AppState::InGame {
                paused: false,
                chat_open: true
            }
            .is_in_game()
        );
        assert!(
            AppState::InGame {
                paused: true,
                chat_open: true
            }
            .is_in_game()
        );
        assert!(!AppState::Lobby.is_in_game());
        assert!(!AppState::Connecting.is_in_game());
        assert!(!AppState::Disconnected.is_in_game());
    }

    #[test]
    fn appstate_ingame_default_has_no_overlays() {
        match AppState::ingame_default() {
            AppState::InGame { paused, chat_open } => {
                assert!(!paused);
                assert!(!chat_open);
            }
            _ => panic!("ingame_default 应返回 InGame 变体"),
        }
    }

    #[test]
    fn appstate_default_is_lobby() {
        assert_eq!(AppState::default(), AppState::Lobby);
    }

    #[test]
    fn appstate_subflag_inequality() {
        // 两种叠加层组合是独立状态：用于驱动 paused vs chat_open 路径的状态回写。
        assert_ne!(
            AppState::InGame {
                paused: true,
                chat_open: false
            },
            AppState::InGame {
                paused: false,
                chat_open: true
            }
        );
        assert_ne!(
            AppState::ingame_default(),
            AppState::InGame {
                paused: true,
                chat_open: false
            }
        );
    }

    #[test]
    fn appsettings_default_matches_doc_spec() {
        let s = AppSettings::default();
        assert_eq!(s.fov_degrees, 70.0);
        assert_eq!(s.mouse_sensitivity, 1.0);
        assert_eq!(s.render_distance, 6);
        assert_eq!(s.interp_delay_ms, 100.0);
        assert!(s.show_stats);
    }

    #[test]
    fn appsettings_partial_eq_ignores_dev_fields() {
        let mut a = AppSettings::default();
        let mut b = AppSettings::default();
        a.fly_speed = 100.0;
        b.fly_speed = 200.0;
        a.mesh_budget_ms = 1.0;
        b.mesh_budget_ms = 8.0;
        a.min_action_interval_ms = 0.0;
        b.min_action_interval_ms = 999.0;
        // 仅 dev 字段不同时 PartialEq 仍判等（避免暂停菜单"未修改"误判触发 save）
        assert_eq!(a, b);
    }

    #[test]
    fn appsettings_partial_eq_detects_user_field_changes() {
        let a = AppSettings::default();
        let b = AppSettings {
            fov_degrees: 95.0,
            ..AppSettings::default()
        };
        assert_ne!(a, b);
    }
}
