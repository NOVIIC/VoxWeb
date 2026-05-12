//! 客户端物理：玩家 AABB 与世界方块的分轴扫动碰撞，含 Walk / Fly 双模式。
//!
//! Walk 模式：重力 + 跳跃 + lerp 平滑的水平加速 + Y/X/Z 三轴依次扫动；
//! Fly 模式：直接按相机朝向自由飞行，速度归零，不受重力。
//!
//! 服务端权威物理在 `voxweb_server::physics`（仅做范围/重叠校验）。
//! 此处客户端物理为本地预测：在 Phase 3 单机模式下与 server 共享同一份 world，
//! 不会出现"协调不一致"；Phase 5 起远端玩家走入 `interp` 缓冲。

use glam::Vec3;

use voxweb_core::block::{BlockID, properties};
use voxweb_core::geometry::{Aabb, PLAYER_EYE_OFFSET, player_aabb};

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
    pub fn step(
        &mut self,
        get_block: &dyn Fn(i32, i32, i32) -> BlockID,
        camera: &Camera,
        input: &InputState,
        dt: f32,
    ) {
        match self.mode {
            CameraMode::Fly => self.step_fly(camera, input, dt),
            CameraMode::Walk => self.step_walk(get_block, camera, input, dt),
        }
    }

    // —— Fly ——

    fn step_fly(&mut self, camera: &Camera, input: &InputState, dt: f32) {
        let mut dir = Vec3::ZERO;
        // Fly 用完整相机朝向（含 pitch），便于自由观察
        let f = camera.forward();
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
            dir = dir.normalize() * FLY_SPEED * dt;
            self.feet_position += dir;
        }
        self.velocity = Vec3::ZERO;
        self.on_ground = false;
    }

    // —— Walk ——

    fn step_walk(
        &mut self,
        get_block: &dyn Fn(i32, i32, i32) -> BlockID,
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
        self.move_axis_y(get_block, disp.y);
        self.move_axis_x(get_block, disp.x);
        self.move_axis_z(get_block, disp.z);

        // 6) 地面检测
        self.on_ground = check_ground(get_block, self.feet_position);
    }

    fn move_axis_y(&mut self, get_block: &dyn Fn(i32, i32, i32) -> BlockID, dy: f32) {
        if dy == 0.0 {
            return;
        }
        let new_feet = self.feet_position + Vec3::Y * dy;
        let candidate = player_aabb(new_feet);
        if collides_with_world(get_block, &candidate) {
            // 撞顶或撞地：把脚底/头顶吸附到所撞方块整数面附近，避免穿插
            self.feet_position.y = if dy < 0.0 {
                self.feet_position.y.floor()
            } else {
                self.feet_position.y.ceil()
            };
            self.velocity.y = 0.0;
        } else {
            self.feet_position.y = new_feet.y;
        }
    }

    fn move_axis_x(&mut self, get_block: &dyn Fn(i32, i32, i32) -> BlockID, dx: f32) {
        if dx == 0.0 {
            return;
        }
        let new_feet = Vec3::new(
            self.feet_position.x + dx,
            self.feet_position.y,
            self.feet_position.z,
        );
        let candidate = player_aabb(new_feet);
        if collides_with_world(get_block, &candidate) {
            self.velocity.x = 0.0;
        } else {
            self.feet_position.x = new_feet.x;
        }
    }

    fn move_axis_z(&mut self, get_block: &dyn Fn(i32, i32, i32) -> BlockID, dz: f32) {
        if dz == 0.0 {
            return;
        }
        let new_feet = Vec3::new(
            self.feet_position.x,
            self.feet_position.y,
            self.feet_position.z + dz,
        );
        let candidate = player_aabb(new_feet);
        if collides_with_world(get_block, &candidate) {
            self.velocity.z = 0.0;
        } else {
            self.feet_position.z = new_feet.z;
        }
    }
}

// —— 自由函数 ——

/// 玩家 AABB 是否与世界中任何 solid 方块重叠。
pub fn collides_with_world(get_block: &dyn Fn(i32, i32, i32) -> BlockID, aabb: &Aabb) -> bool {
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
                if aabb.intersects(&Aabb::new(
                    Vec3::new(x as f32, y as f32, z as f32),
                    Vec3::new(x as f32 + 1.0, y as f32 + 1.0, z as f32 + 1.0),
                )) {
                    return true;
                }
            }
        }
    }
    false
}

/// 玩家脚下 GROUND_PROBE 米内有 solid 方块 → 在地面。
pub fn check_ground(get_block: &dyn Fn(i32, i32, i32) -> BlockID, feet: Vec3) -> bool {
    let probe = Aabb {
        min: Vec3::new(feet.x - 0.3, feet.y - GROUND_PROBE, feet.z - 0.3),
        max: Vec3::new(feet.x + 0.3, feet.y, feet.z + 0.3),
    };
    collides_with_world(get_block, &probe)
}

/// AIR / 透明（如水）不参与碰撞；其它走 BlockProperties.solid 表。
/// 玻璃在 properties() 中标记为 solid=true，所以撞玻璃是会被挡住的（与 Minecraft 一致）。
fn block_solid(id: BlockID) -> bool {
    if id == BlockID::AIR {
        return false;
    }
    properties(id).solid
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
            &player_aabb(Vec3::new(0.5, 65.0, 0.5))
        ));
        // 脚 y=64.9 → AABB.min.y=64.9 < 65=STONE.max.y → 真重叠
        assert!(collides_with_world(
            &getter,
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
            p.step(&getter, &camera, &input, 1.0 / 60.0);
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
        p.step(&getter, &camera, &input, 1.0 / 60.0);
        assert!(p.on_ground, "tick 后玩家应在地面");

        // 模拟 just-pressed 跳跃
        input.jump_just_pressed = true;
        p.step(&getter, &camera, &input, 1.0 / 60.0);
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
            if x == 1 && y == 65 {
                BlockID::STONE
            } else if y == 64 {
                BlockID::STONE // 地面
            } else {
                BlockID::AIR
            }
        };
        let mut p = LocalPhysics::new(Vec3::new(0.5, 65.0, 0.5));
        // 用 dt=0.05 使得 dx ≈ 0.215，玩家 AABB.max.x 从 0.8 到 1.015，刚好跨入 x=1 方块
        p.velocity = Vec3::new(WALK_SPEED, 0.0, WALK_SPEED);
        let dt = 0.05;
        let disp = p.velocity * dt;
        p.move_axis_y(&getter, disp.y);
        p.move_axis_x(&getter, disp.x);
        p.move_axis_z(&getter, disp.z);
        // x 被卡住（撞 x=1 墙），位置不变
        assert!(
            (p.feet_position.x - 0.5).abs() < 1e-6,
            "x={} 应当被墙卡住",
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
}
