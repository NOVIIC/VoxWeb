//! 客户端物理：玩家 AABB 与世界方块的分轴扫动碰撞，含 Walk / Fly 双模式。
//!
//! Walk 模式：重力 + 跳跃 + lerp 平滑的水平加速 + Y/X/Z 三轴依次扫动；
//! Fly 模式：WASD 沿水平面方向（不含 pitch）飞行，Space/Shift 控制升/降，速度归零不受重力，
//! 同样执行 Y/X/Z 分轴碰撞检测，避免穿墙。
//!
//! 服务端权威物理在 `voxweb_server::physics`（仅做范围/重叠校验）。
//! 此处客户端物理为本地预测：在 Phase 3 单机模式下与 server 共享同一份 world，
//! 不会出现"协调不一致"；Phase 5 起远端玩家走入 `interp` 缓冲。

use glam::Vec3;

use voxweb_core::block::{BlockID, properties};
use voxweb_core::geometry::{Aabb, PLAYER_EYE_OFFSET, player_aabb};
use voxweb_core::{is_smooth_granular, smooth_cell_top_height};

use crate::camera::{Camera, CameraMode};
use crate::input::InputState;

// —— 调参常量 ——

/// Walk 模式平地走路速度（米/秒）。
pub const WALK_SPEED: f32 = 4.3;
/// 跳跃初速度（米/秒），约等于 1.25 m 跳高（在 GRAVITY = -32 下）。
pub const JUMP_SPEED: f32 = 8.4;
/// 重力加速度（米/秒²）。比真实重力 (-9.8) 大，让手感紧凑。
pub const GRAVITY: f32 = -32.0;
/// 落地终极速度上限（米/秒），防止无限加速。
pub const TERMINAL_VELOCITY: f32 = 78.0;
/// 水平速度 lerp 系数（1/秒）。值越大，加/减速越快。
pub const HORIZ_ACC: f32 = 12.0;
/// Fly 模式速度（米/秒）。
pub const FLY_SPEED: f32 = 12.0;
/// 地面探测距离（脚底下方多少米内有方块算作 on_ground）。
const GROUND_PROBE: f32 = 0.05;
/// 单个 sub-step 上限（秒）。让单步最大位移 = TERMINAL_VELOCITY / 60 ≈ 1.3m，
/// 小于玩家 AABB 高度（1.8m），保证不会一次性穿透 1 格厚的地板。
const MAX_SUBSTEP_DT: f32 = 1.0 / 60.0;
/// 单帧物理总时长上限（秒）。tab 切到后台时 RAF 被节流到 ~1Hz，回前台首帧 dt 可达数秒；
/// 超过该值视为"非正常间隔"，按上限折算，避免一次 step 拆出几百个 sub-step 卡死帧。
/// 与 FrameClock.accumulator 上限一致，让物理与 server.tick 在恢复时步调对齐。
const MAX_FRAME_DT: f32 = 0.25;

/// 客户端玩家物理体。`feet_position` 为脚底中心；`eye_position()` = 该值 + EYE_OFFSET。
pub struct LocalPhysics {
    pub feet_position: Vec3,
    pub velocity: Vec3,
    pub on_ground: bool,
    pub mode: CameraMode,
}

impl LocalPhysics {
    /// 出生位置（高空，由重力拉到地面）。
    pub fn new(spawn_feet: Vec3) -> Self {
        Self {
            feet_position: spawn_feet,
            velocity: Vec3::ZERO,
            on_ground: false,
            mode: CameraMode::Walk,
        }
    }

    /// 当前眼睛位置（= 脚底 + EYE_OFFSET）。供 Camera.position 同步。
    pub fn eye_position(&self) -> Vec3 {
        self.feet_position + Vec3::Y * PLAYER_EYE_OFFSET
    }

    /// 双击空格触发的模式切换。
    pub fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            CameraMode::Walk => CameraMode::Fly,
            CameraMode::Fly => CameraMode::Walk,
        };
        // 切到 Fly 时清零 velocity，避免重力残余把玩家拽走；
        // 切回 Walk 时同样清零，让玩家从"悬停点"重新自然落地。
        self.velocity = Vec3::ZERO;
        self.on_ground = false;
    }

    /// 每帧物理步进。
    /// `get_block` 是世界坐标 → BlockID 的查询闭包（chunk 未加载返回 AIR 即可）。
    /// `dt` 为渲染帧步长（秒）。
    ///
    /// 内部会按 [`MAX_SUBSTEP_DT`] 拆分 sub-step：正常 60Hz 帧拆出 1 步、行为与之前一致；
    /// tab 后台导致 dt 过大时拆出多步，每步独立 AABB 碰撞检测，
    /// 避免单帧重力位移大于玩家 AABB 高度从而穿透地板。
    pub fn step(
        &mut self,
        get_block: &dyn Fn(i32, i32, i32) -> BlockID,
        dynamic_aabbs: &[Aabb],
        camera: &Camera,
        input: &InputState,
        dt: f32,
    ) {
        // 1) 总时长上限：超大 dt 视为"暂停后恢复"，按上限补一段物理，剩余时间丢弃。
        let mut remaining = dt.clamp(0.0, MAX_FRAME_DT);
        // 2) 拆分 sub-step：每步 ≤ MAX_SUBSTEP_DT，最后一步取余数。
        while remaining > 0.0 {
            let step_dt = remaining.min(MAX_SUBSTEP_DT);
            match self.mode {
                CameraMode::Fly => self.step_fly(get_block, dynamic_aabbs, camera, input, step_dt),
                CameraMode::Walk => {
                    self.step_walk(get_block, dynamic_aabbs, camera, input, step_dt)
                }
            }
            remaining -= step_dt;
        }
    }

    // —— Fly ——

    fn step_fly(
        &mut self,
        get_block: &dyn Fn(i32, i32, i32) -> BlockID,
        dynamic_aabbs: &[Aabb],
        camera: &Camera,
        input: &InputState,
        dt: f32,
    ) {
        let mut dir = Vec3::ZERO;
        // Fly 模式下 WASD 仅控制水平面方向（与 Walk 一致），避免视线朝下时按 W
        // 反而往地里钻。垂直方向交给 Space（上升）/ Shift（下降）。
        let f = camera.forward_horizontal();
        let r = camera.right();
        let u = Vec3::Y;
        if input.forward {
            dir += f;
        }
        if input.backward {
            dir -= f;
        }
        if input.right {
            dir += r;
        }
        if input.left {
            dir -= r;
        }
        if input.jump_held {
            dir += u;
        }
        if input.sneak {
            dir -= u;
        }
        if dir.length_squared() > 0.0 {
            let disp = dir.normalize() * FLY_SPEED * dt;
            // 分轴扫动碰撞（与 Walk 一致）：先 Y 再 X 再 Z，撞墙时吸附并停止该轴
            self.move_axis_y(get_block, dynamic_aabbs, disp.y);
            self.move_axis_x(get_block, dynamic_aabbs, disp.x);
            self.move_axis_z(get_block, dynamic_aabbs, disp.z);
        }
        self.velocity = Vec3::ZERO;
        self.on_ground = false;
    }

    // —— Walk ——

    fn step_walk(
        &mut self,
        get_block: &dyn Fn(i32, i32, i32) -> BlockID,
        dynamic_aabbs: &[Aabb],
        camera: &Camera,
        input: &InputState,
        dt: f32,
    ) {
        // 1) 期望水平速度（基于输入 + 相机水平朝向）
        let mut target = Vec3::ZERO;
        let forward = camera.forward_horizontal();
        let right = camera.right();
        if input.forward {
            target += forward;
        }
        if input.backward {
            target -= forward;
        }
        if input.right {
            target += right;
        }
        if input.left {
            target -= right;
        }
        if target.length_squared() > 0.0 {
            target = target.normalize() * WALK_SPEED;
        }

        // 2) 水平速度 lerp 平滑（避免瞬时启停的"贴地走"手感）
        let blend = (HORIZ_ACC * dt).clamp(0.0, 1.0);
        self.velocity.x = lerp(self.velocity.x, target.x, blend);
        self.velocity.z = lerp(self.velocity.z, target.z, blend);

        // 3) 跳跃（仅在 on_ground 且本帧按下）
        if input.jump_just_pressed && self.on_ground {
            self.velocity.y = JUMP_SPEED;
            self.on_ground = false;
        }

        // 4) 重力
        self.velocity.y += GRAVITY * dt;
        if self.velocity.y < -TERMINAL_VELOCITY {
            self.velocity.y = -TERMINAL_VELOCITY;
        }

        // 5) 分轴扫动碰撞：Y → X → Z
        let disp = self.velocity * dt;
        self.move_axis_y(get_block, dynamic_aabbs, disp.y);
        self.move_axis_x(get_block, dynamic_aabbs, disp.x);
        self.move_axis_z(get_block, dynamic_aabbs, disp.z);

        // 6) 地面检测
        self.on_ground = check_ground(get_block, dynamic_aabbs, self.feet_position);
    }

    fn move_axis_y(
        &mut self,
        get_block: &dyn Fn(i32, i32, i32) -> BlockID,
        dynamic_aabbs: &[Aabb],
        dy: f32,
    ) {
        if dy == 0.0 {
            return;
        }
        let new_feet = self.feet_position + Vec3::Y * dy;
        let candidate = player_aabb(new_feet);
        if collides_with_world(get_block, dynamic_aabbs, &candidate) {
            self.velocity.y = 0.0;
            if dy < 0.0
                && let Some(snap) =
                    find_dynamic_floor_snap(dynamic_aabbs, &candidate, self.feet_position.y)
            {
                self.feet_position.y = snap;
                return;
            }
            if dy < 0.0
                && let Some(snap) =
                    find_smooth_floor_snap(get_block, &candidate, self.feet_position.y)
            {
                self.feet_position.y = snap;
                return;
            }
            // 吸附到最近的整数 Y 面：floor 脚底 →
            //   下落时站在方块顶面；上跳时头顶刚好在方块底面以下
            // （因玩家高度 >1 格，ceil 会穿入方块，故统一用 floor）。
            let snap = self.feet_position.y.floor();
            if !collides_with_world(
                get_block,
                dynamic_aabbs,
                &player_aabb(Vec3::new(self.feet_position.x, snap, self.feet_position.z)),
            ) {
                self.feet_position.y = snap;
                return;
            }
            // 仍碰撞（玩家已嵌入方块）：向搜索最近安全位置
            for i in 1i32..=4 {
                // 优先向下（重力方向更常见）
                let down = snap - i as f32;
                let aabb_down =
                    player_aabb(Vec3::new(self.feet_position.x, down, self.feet_position.z));
                if !collides_with_world(get_block, dynamic_aabbs, &aabb_down) {
                    self.feet_position.y = down;
                    return;
                }
                // 再试向上
                let up = snap + i as f32;
                let aabb_up =
                    player_aabb(Vec3::new(self.feet_position.x, up, self.feet_position.z));
                if !collides_with_world(get_block, dynamic_aabbs, &aabb_up) {
                    self.feet_position.y = up;
                    return;
                }
            }
            // 极端情况找不到安全位置，保持原位
        } else {
            self.feet_position.y = new_feet.y;
        }
    }

    fn move_axis_x(
        &mut self,
        get_block: &dyn Fn(i32, i32, i32) -> BlockID,
        dynamic_aabbs: &[Aabb],
        dx: f32,
    ) {
        if dx == 0.0 {
            return;
        }
        let new_feet = Vec3::new(
            self.feet_position.x + dx,
            self.feet_position.y,
            self.feet_position.z,
        );
        let candidate = player_aabb(new_feet);
        if collides_with_world(get_block, dynamic_aabbs, &candidate) {
            // 吸附到墙面：在位移方向上找到最近的 solid 方块，把玩家 AABB 贴到其面上。
            // 直接零速度不吸附会导致浮点累积误差让玩家逐渐"远离"墙壁。
            let half = voxweb_core::geometry::PLAYER_WIDTH * 0.5;
            if let Some(snap_x) = find_wall_snap_x(get_block, dynamic_aabbs, &candidate, dx, half) {
                self.feet_position.x = snap_x;
            }
            self.velocity.x = 0.0;
        } else {
            self.feet_position.x = new_feet.x;
        }
    }

    fn move_axis_z(
        &mut self,
        get_block: &dyn Fn(i32, i32, i32) -> BlockID,
        dynamic_aabbs: &[Aabb],
        dz: f32,
    ) {
        if dz == 0.0 {
            return;
        }
        let new_feet = Vec3::new(
            self.feet_position.x,
            self.feet_position.y,
            self.feet_position.z + dz,
        );
        let candidate = player_aabb(new_feet);
        if collides_with_world(get_block, dynamic_aabbs, &candidate) {
            let half = voxweb_core::geometry::PLAYER_WIDTH * 0.5;
            if let Some(snap_z) = find_wall_snap_z(get_block, dynamic_aabbs, &candidate, dz, half) {
                self.feet_position.z = snap_z;
            }
            self.velocity.z = 0.0;
        } else {
            self.feet_position.z = new_feet.z;
        }
    }
}

// —— 自由函数 ——

/// 玩家 AABB 是否与世界中任何 solid 方块重叠。
pub fn collides_with_world(
    get_block: &dyn Fn(i32, i32, i32) -> BlockID,
    dynamic_aabbs: &[Aabb],
    aabb: &Aabb,
) -> bool {
    if dynamic_aabbs
        .iter()
        .any(|dynamic_aabb| aabb.intersects(dynamic_aabb))
    {
        return true;
    }
    // 扫描覆盖 AABB 的所有整数方块单元（floor..ceil）
    let min_x = aabb.min.x.floor() as i32;
    let max_x = (aabb.max.x - f32::EPSILON).floor() as i32;
    let min_y = aabb.min.y.floor() as i32;
    let max_y = (aabb.max.y - f32::EPSILON).floor() as i32;
    let min_z = aabb.min.z.floor() as i32;
    let max_z = (aabb.max.z - f32::EPSILON).floor() as i32;
    for x in min_x..=max_x {
        for y in min_y..=max_y {
            for z in min_z..=max_z {
                if !block_solid(get_block(x, y, z)) {
                    continue;
                }
                if block_intersects_aabb(get_block, x, y, z, aabb) {
                    return true;
                }
            }
        }
    }
    false
}

fn block_intersects_aabb(
    get_block: &dyn Fn(i32, i32, i32) -> BlockID,
    x: i32,
    y: i32,
    z: i32,
    aabb: &Aabb,
) -> bool {
    let block = get_block(x, y, z);
    if is_smooth_granular(block) {
        if aabb.max.x <= x as f32
            || aabb.min.x >= x as f32 + 1.0
            || aabb.max.z <= z as f32
            || aabb.min.z >= z as f32 + 1.0
        {
            return false;
        }
        let top = smooth_cell_top_height(get_block, x, y, z, block);
        // 露出表面可能缩到 cell_y 以下（斜坡），用较小底边避免脚下漏碰。
        let bottom = y as f32;
        return aabb.min.y < top && aabb.max.y > bottom.min(top - 0.05);
    }
    aabb.intersects(&Aabb::new(
        Vec3::new(x as f32, y as f32, z as f32),
        Vec3::new(x as f32 + 1.0, y as f32 + 1.0, z as f32 + 1.0),
    ))
}

fn find_smooth_floor_snap(
    get_block: &dyn Fn(i32, i32, i32) -> BlockID,
    aabb: &Aabb,
    previous_feet_y: f32,
) -> Option<f32> {
    let min_x = aabb.min.x.floor() as i32;
    let max_x = (aabb.max.x - f32::EPSILON).floor() as i32;
    let min_z = aabb.min.z.floor() as i32;
    let max_z = (aabb.max.z - f32::EPSILON).floor() as i32;
    let min_y = (aabb.min.y.floor() as i32 - 1).max(0);
    let max_y = (previous_feet_y.ceil() as i32 + 1).min(voxweb_core::CHUNK_Y as i32 - 1);
    let mut best: Option<f32> = None;
    for x in min_x..=max_x {
        for y in min_y..=max_y {
            for z in min_z..=max_z {
                let block = get_block(x, y, z);
                if !is_smooth_granular(block) {
                    continue;
                }
                let top = smooth_cell_top_height(get_block, x, y, z, block);
                if top < aabb.min.y - 0.08 || top > previous_feet_y + 0.12 {
                    continue;
                }
                best = Some(best.map_or(top, |old| old.max(top)));
            }
        }
    }
    best
}

fn find_dynamic_floor_snap(
    dynamic_aabbs: &[Aabb],
    aabb: &Aabb,
    previous_feet_y: f32,
) -> Option<f32> {
    dynamic_aabbs
        .iter()
        .filter(|dynamic| {
            aabb.max.x > dynamic.min.x
                && aabb.min.x < dynamic.max.x
                && aabb.max.z > dynamic.min.z
                && aabb.min.z < dynamic.max.z
                && dynamic.max.y <= previous_feet_y + 0.12
                && dynamic.max.y >= aabb.min.y - 0.08
        })
        .map(|dynamic| dynamic.max.y)
        .max_by(|a, b| a.total_cmp(b))
}

/// 玩家脚下 GROUND_PROBE 米内有 solid 方块 → 在地面。
pub fn check_ground(
    get_block: &dyn Fn(i32, i32, i32) -> BlockID,
    dynamic_aabbs: &[Aabb],
    feet: Vec3,
) -> bool {
    let probe = Aabb {
        min: Vec3::new(feet.x - 0.3, feet.y - GROUND_PROBE, feet.z - 0.3),
        max: Vec3::new(feet.x + 0.3, feet.y, feet.z + 0.3),
    };
    collides_with_world(get_block, dynamic_aabbs, &probe)
}

/// AIR / 透明（如水）不参与碰撞；其它走 BlockProperties.solid 表。
/// 玻璃在 properties() 中标记为 solid=true，所以撞玻璃是会被挡住的（与 Minecraft 一致）。
fn block_solid(id: BlockID) -> bool {
    if id == BlockID::AIR {
        return false;
    }
    properties(id).solid
}

/// X 轴墙面吸附：在位移方向上找到最近的 solid 方块，返回吸附后的 feet.x。
fn find_wall_snap_x(
    get_block: &dyn Fn(i32, i32, i32) -> BlockID,
    dynamic_aabbs: &[Aabb],
    aabb: &Aabb,
    dx: f32,
    half_width: f32,
) -> Option<f32> {
    let mut best = dynamic_wall_snap_x(dynamic_aabbs, aabb, dx, half_width);
    let min_x = aabb.min.x.floor() as i32;
    let max_x = aabb.max.x.ceil() as i32; // 用 ceil 确保覆盖触碰面的方块
    let min_y = aabb.min.y.floor() as i32;
    let max_y = aabb.max.y.ceil() as i32;
    let min_z = aabb.min.z.floor() as i32;
    let max_z = aabb.max.z.ceil() as i32;
    for bx in min_x..max_x {
        for by in min_y..max_y {
            for bz in min_z..max_z {
                if !block_solid(get_block(bx, by, bz)) {
                    continue;
                }
                let block_min_x = bx as f32;
                let block_max_x = bx as f32 + 1.0;
                if !block_intersects_aabb(get_block, bx, by, bz, aabb) {
                    continue;
                }
                // 只在位移方向上吸附：右移贴左面，左移贴右面
                let snap = if dx > 0.0 {
                    block_min_x - half_width
                } else {
                    block_max_x + half_width
                };
                let closer = match best {
                    None => true,
                    Some(prev) if dx > 0.0 => snap < prev,
                    Some(prev) => snap > prev,
                };
                if closer {
                    best = Some(snap);
                }
            }
        }
    }
    best
}

fn dynamic_wall_snap_x(
    dynamic_aabbs: &[Aabb],
    aabb: &Aabb,
    dx: f32,
    half_width: f32,
) -> Option<f32> {
    dynamic_aabbs
        .iter()
        .filter(|dynamic| aabb.intersects(dynamic))
        .map(|dynamic| {
            if dx > 0.0 {
                dynamic.min.x - half_width
            } else {
                dynamic.max.x + half_width
            }
        })
        .reduce(|best, snap| {
            if (dx > 0.0 && snap < best) || (dx <= 0.0 && snap > best) {
                snap
            } else {
                best
            }
        })
}

/// Z 轴墙面吸附：与 find_wall_snap_x 对称。
fn find_wall_snap_z(
    get_block: &dyn Fn(i32, i32, i32) -> BlockID,
    dynamic_aabbs: &[Aabb],
    aabb: &Aabb,
    dz: f32,
    half_width: f32,
) -> Option<f32> {
    let mut best = dynamic_wall_snap_z(dynamic_aabbs, aabb, dz, half_width);
    let min_x = aabb.min.x.floor() as i32;
    let max_x = aabb.max.x.ceil() as i32;
    let min_y = aabb.min.y.floor() as i32;
    let max_y = aabb.max.y.ceil() as i32;
    let min_z = aabb.min.z.floor() as i32;
    let max_z = aabb.max.z.ceil() as i32;
    for bx in min_x..max_x {
        for by in min_y..max_y {
            for bz in min_z..max_z {
                if !block_solid(get_block(bx, by, bz)) {
                    continue;
                }
                let block_min_z = bz as f32;
                let block_max_z = bz as f32 + 1.0;
                if !block_intersects_aabb(get_block, bx, by, bz, aabb) {
                    continue;
                }
                let snap = if dz > 0.0 {
                    block_min_z - half_width
                } else {
                    block_max_z + half_width
                };
                let closer = match best {
                    None => true,
                    Some(prev) if dz > 0.0 => snap < prev,
                    Some(prev) => snap > prev,
                };
                if closer {
                    best = Some(snap);
                }
            }
        }
    }
    best
}

fn dynamic_wall_snap_z(
    dynamic_aabbs: &[Aabb],
    aabb: &Aabb,
    dz: f32,
    half_width: f32,
) -> Option<f32> {
    dynamic_aabbs
        .iter()
        .filter(|dynamic| aabb.intersects(dynamic))
        .map(|dynamic| {
            if dz > 0.0 {
                dynamic.min.z - half_width
            } else {
                dynamic.max.z + half_width
            }
        })
        .reduce(|best, snap| {
            if (dz > 0.0 && snap < best) || (dz <= 0.0 && snap > best) {
                snap
            } else {
                best
            }
        })
}

#[inline]
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

// —— 测试 ——

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::InputState;
    use voxweb_core::geometry::PLAYER_HEIGHT;

    /// 构造测试用 get_block 闭包：在 y=64 平面填一整层 STONE，其它都是 AIR。
    fn floor_at_y64() -> impl Fn(i32, i32, i32) -> BlockID {
        |_x, y, _z| {
            if y == 64 {
                BlockID::STONE
            } else {
                BlockID::AIR
            }
        }
    }

    #[test]
    fn lerp_basic() {
        assert!((lerp(0.0, 10.0, 0.5) - 5.0).abs() < 1e-6);
        assert!((lerp(2.0, 2.0, 1.0) - 2.0).abs() < 1e-6);
    }

    #[test]
    fn collides_detects_block_under_player() {
        let getter = floor_at_y64();
        // 脚 y=65 → 玩家站在 y=64 STONE 顶面正上方，AABB.min.y=65 == block.max.y=65 触面不算碰撞
        assert!(!collides_with_world(
            &getter,
            &[],
            &player_aabb(Vec3::new(0.5, 65.0, 0.5))
        ));
        // 脚 y=64.9 → AABB.min.y=64.9 < 65=STONE.max.y → 真重叠
        assert!(collides_with_world(
            &getter,
            &[],
            &player_aabb(Vec3::new(0.5, 64.9, 0.5))
        ));
    }

    #[test]
    fn gravity_brings_player_to_ground() {
        let getter = floor_at_y64();
        let mut p = LocalPhysics::new(Vec3::new(0.5, 80.0, 0.5));
        let input = InputState::default();
        let camera = Camera::default();
        // 60Hz 跑 5 秒，足够从 y=80 落到 y=65
        for _ in 0..(60 * 5) {
            p.step(&getter, &[], &camera, &input, 1.0 / 60.0);
        }
        assert!(p.on_ground, "玩家应当落地");
        // 脚底 y 应在 65 附近（STONE 顶面 = 65.0）
        assert!(
            (p.feet_position.y - 65.0).abs() < 0.5,
            "feet.y = {}",
            p.feet_position.y
        );
    }

    #[test]
    fn jump_increases_y_velocity() {
        let getter = floor_at_y64();
        let mut p = LocalPhysics::new(Vec3::new(0.5, 65.0, 0.5));
        let mut input = InputState::default();
        let camera = Camera::default();
        // 先 tick 一次让 on_ground=true
        p.step(&getter, &[], &camera, &input, 1.0 / 60.0);
        assert!(p.on_ground, "tick 后玩家应在地面");

        // 模拟 just-pressed 跳跃
        input.jump_just_pressed = true;
        p.step(&getter, &[], &camera, &input, 1.0 / 60.0);
        // 跳起后 velocity.y 应接近 JUMP_SPEED 减一帧重力（8.4 - 32/60 ≈ 7.87）
        assert!(
            p.velocity.y > JUMP_SPEED * 0.9,
            "velocity.y = {}",
            p.velocity.y
        );
        assert!(!p.on_ground);
    }

    #[test]
    fn axis_split_lets_player_slide_along_wall() {
        // 在 x=1 处摆一面墙（y=65 位置有块）
        let getter = |x: i32, y: i32, _z: i32| {
            if (x == 1 && y == 65) || y == 64 {
                BlockID::STONE
            } else {
                BlockID::AIR
            }
        };
        let mut p = LocalPhysics::new(Vec3::new(0.5, 65.0, 0.5));
        // 用 dt=0.05 使得 dx ≈ 0.215，玩家 AABB.max.x 从 0.8 到 1.015，刚好跨入 x=1 方块
        p.velocity = Vec3::new(WALK_SPEED, 0.0, WALK_SPEED);
        let dt = 0.05;
        let disp = p.velocity * dt;
        p.move_axis_y(&getter, &[], disp.y);
        p.move_axis_x(&getter, &[], disp.x);
        p.move_axis_z(&getter, &[], disp.z);
        // x 被墙吸附：AABB.max.x 贴到 x=1 墙面（feet.x = 1.0 - 0.3 = 0.7）
        assert!(
            (p.feet_position.x - 0.7).abs() < 0.01,
            "x={} 应当被墙吸附到 0.7",
            p.feet_position.x
        );
        assert!(p.velocity.x.abs() < 1e-6, "x 方向速度被清零");
        // z 应顺利前进（无障碍）
        assert!(
            p.feet_position.z > 0.5,
            "z={} 应当顺利前进",
            p.feet_position.z
        );
    }

    #[test]
    fn eye_position_offset_correctly() {
        let p = LocalPhysics::new(Vec3::new(1.0, 60.0, 2.0));
        let eye = p.eye_position();
        assert!((eye.y - (60.0 + PLAYER_EYE_OFFSET)).abs() < 1e-6);
        assert!((eye.x - 1.0).abs() < 1e-6);
        assert!((eye.z - 2.0).abs() < 1e-6);
    }

    #[test]
    fn fly_forward_is_horizontal_even_when_looking_down() {
        // 即便相机朝下盯地，按 W（forward）也应只在水平面上移动，不下沉。
        let getter = |_x: i32, _y: i32, _z: i32| BlockID::AIR;
        let mut p = LocalPhysics::new(Vec3::new(0.0, 100.0, 0.0));
        p.mode = CameraMode::Fly;
        let camera = Camera {
            yaw: 0.0,
            pitch: -1.2,
            ..Camera::default()
        };
        let mut input = InputState::default();
        input.forward = true;
        let y_before = p.feet_position.y;
        p.step(&getter, &[], &camera, &input, 0.5);
        assert!(
            (p.feet_position.y - y_before).abs() < 1e-4,
            "Fly 模式按 W 不应改变 Y，但 y_before={} y_after={}",
            y_before,
            p.feet_position.y
        );
        assert!(
            p.feet_position.x > 0.0,
            "应当沿 +X 前进，得到 x={}",
            p.feet_position.x
        );
    }

    #[test]
    fn fly_space_raises_player() {
        // Fly 模式下空格应当让玩家上升（垂直方向交给 Space/Shift）。
        let getter = |_x: i32, _y: i32, _z: i32| BlockID::AIR;
        let mut p = LocalPhysics::new(Vec3::new(0.0, 100.0, 0.0));
        p.mode = CameraMode::Fly;
        let camera = Camera::default();
        let mut input = InputState::default();
        input.jump_held = true;
        let y_before = p.feet_position.y;
        p.step(&getter, &[], &camera, &input, 0.5);
        assert!(
            p.feet_position.y > y_before,
            "Space 应让玩家上升，y_before={} y_after={}",
            y_before,
            p.feet_position.y
        );
    }

    #[test]
    fn step_with_huge_dt_does_not_tunnel_through_floor() {
        // 回归测试：浏览器切换到其他 tab 时 RAF 被节流，下一帧 dt 可达数秒。
        // 没有 sub-step 时，重力把 disp.y 推到远超玩家 AABB 高度（1.8m），
        // 单次 AABB 碰撞检测会"跨越"整层地板，玩家穿透到虚空连续下坠。
        let getter = floor_at_y64();
        let mut p = LocalPhysics::new(Vec3::new(0.5, 70.0, 0.5));
        let camera = Camera::default();
        let input = InputState::default();
        // 先稳定落地
        for _ in 0..(60 * 5) {
            p.step(&getter, &[], &camera, &input, 1.0 / 60.0);
        }
        assert!(p.on_ground, "前置：玩家应当先落地");
        let y_landed = p.feet_position.y;

        // 模拟 tab 后台 10 秒（RAF 节流后单次回调 dt=10s）
        p.step(&getter, &[], &camera, &input, 10.0);

        // 玩家应仍贴在地面上，不应穿透到 STONE 层（y=64）以下
        assert!(
            p.feet_position.y >= 64.0,
            "玩家穿透地板掉入虚空：feet.y = {}（着陆时 {}）",
            p.feet_position.y,
            y_landed
        );
        assert!(p.on_ground, "玩家应仍在地面上，而不是悬空下坠");
    }

    #[test]
    fn jump_into_low_ceiling_does_not_get_stuck() {
        // 回归测试：头顶有方块时跳跃，玩家不应卡在方块内部。
        // 构造：地面 y=64，天花板 y=68（方块占 [68,69)），玩家脚底 y=65。
        // 玩家高度 1.8，跳跃峰值约 1.1m，头顶会触及天花板。
        let getter = |_x: i32, y: i32, _z: i32| {
            if y == 64 || y == 68 {
                BlockID::STONE
            } else {
                BlockID::AIR
            }
        };
        let mut p = LocalPhysics::new(Vec3::new(0.5, 65.0, 0.5));
        let mut input = InputState::default();
        let camera = Camera::default();
        // 先 tick 一次让 on_ground=true
        p.step(&getter, &[], &camera, &input, 1.0 / 60.0);
        assert!(p.on_ground, "前置：玩家应在地面");

        // 跳跃
        input.jump_just_pressed = true;
        p.step(&getter, &[], &camera, &input, 1.0 / 60.0);
        // 持续几帧让玩家上升撞到天花板
        for _ in 0..20 {
            p.step(&getter, &[], &camera, &input, 1.0 / 60.0);
        }
        // 玩家不应卡在天花板内部：头顶（feet.y + 1.8）不应超过天花板底面（y=68）
        assert!(
            p.feet_position.y + PLAYER_HEIGHT <= 68.0 + 0.01,
            "玩家头顶 ({}) 不应穿入天花板方块 (y=68~69)",
            p.feet_position.y + PLAYER_HEIGHT
        );
        // 玩家也不应低于地面
        assert!(
            p.feet_position.y >= 65.0 - 0.01,
            "玩家脚底 ({}) 不应低于地面 (y=65)",
            p.feet_position.y
        );
    }

    #[test]
    fn fly_into_wall_stops_at_wall() {
        // 回归测试：飞行模式不应穿墙。
        // 构造：x=2 处有一面墙（y=64~68 都是 STONE），玩家从 x=0 飞向 +X。
        // 注意：不放地面，否则地面方块会阻挡水平飞行（AABB 触面碰撞）。
        let getter = |x: i32, y: i32, _z: i32| {
            if x == 2 && (64..=68).contains(&y) {
                BlockID::STONE
            } else {
                BlockID::AIR
            }
        };
        let mut p = LocalPhysics::new(Vec3::new(0.5, 65.0, 0.5));
        p.mode = CameraMode::Fly;
        let camera = Camera {
            yaw: 0.0, // 朝 +X
            pitch: 0.0,
            ..Camera::default()
        };
        let mut input = InputState::default();
        input.forward = true;
        // 飞行足够长时间确保撞墙
        for frame in 0..60 {
            let old_x = p.feet_position.x;
            p.step(&getter, &[], &camera, &input, 1.0 / 60.0);
            if frame < 10 || (p.feet_position.x - old_x).abs() > 1e-6 {
                eprintln!(
                    "frame {}: x {:.6} -> {:.6} (dx={:.6})",
                    frame,
                    old_x,
                    p.feet_position.x,
                    p.feet_position.x - old_x
                );
            }
        }
        // 玩家 AABB.max.x = feet.x + 0.3，不应超过 x=2（墙 min.x）
        // 触面不算碰撞（strict inequality），所以 AABB.max.x 可以 == 2.0
        assert!(
            p.feet_position.x + 0.3 <= 2.0 + 0.01,
            "玩家 ({}) 不应穿入墙壁 (x=2)",
            p.feet_position.x
        );
        // 但应紧贴墙壁（feet.x + 0.3 ≈ 2.0，即 feet.x ≈ 1.7）
        assert!(
            p.feet_position.x + 0.3 >= 1.99,
            "玩家应紧贴墙壁，feet.x={}",
            p.feet_position.x
        );
    }

    #[test]
    fn fly_up_blocked_by_ceiling() {
        // 回归测试：飞行模式上升不应穿过天花板。
        let getter = |_x: i32, y: i32, _z: i32| {
            if y == 70 || y == 64 {
                BlockID::STONE
            } else {
                BlockID::AIR
            }
        };
        let mut p = LocalPhysics::new(Vec3::new(0.5, 65.0, 0.5));
        p.mode = CameraMode::Fly;
        let camera = Camera::default();
        let mut input = InputState::default();
        input.jump_held = true; // 上升
        // 飞行足够长时间确保撞天花板
        for _ in 0..60 {
            p.step(&getter, &[], &camera, &input, 1.0 / 60.0);
        }
        // 玩家头顶（feet.y + 1.8）不应超过天花板底面（y=70）
        assert!(
            p.feet_position.y + PLAYER_HEIGHT <= 70.0 + 0.01,
            "玩家头顶 ({}) 不应穿入天花板 (y=70)",
            p.feet_position.y + PLAYER_HEIGHT
        );
    }
}
