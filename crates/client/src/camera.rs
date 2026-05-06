//! 第一人称相机：位置、朝向、视图/投影矩阵。
//!
//! Phase 1 仅 Fly 模式（无重力，自由飞行），Phase 3 引入 Walk 模式 + 重力。

use glam::{Mat4, Vec3};

/// 相机模式（Phase 1 默认 Fly）。
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CameraMode {
    /// 飞行：WASD 沿前/右向移动，空格上、Shift 下，无重力
    Fly,
    /// 步行：受重力，跳跃，碰撞（Phase 3 实装）
    Walk,
}

/// 第一人称自由视角相机。
#[derive(Clone, Debug)]
pub struct Camera {
    pub position: Vec3,
    /// yaw: 围绕 +Y 轴的水平角，0 = +X 方向，π/2 = +Z 方向（弧度）
    pub yaw: f32,
    /// pitch: 仰角，正值朝上，clamp 到 [-89°, +89°]（弧度）
    pub pitch: f32,
    pub fov: f32,
    pub aspect: f32,
    pub near: f32,
    pub far: f32,
    pub mode: CameraMode,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            position: Vec3::new(8.0, 12.0, 24.0),
            yaw: -std::f32::consts::FRAC_PI_2, // 朝向 -Z
            pitch: -0.3,
            fov: 70.0_f32.to_radians(),
            aspect: 16.0 / 9.0,
            near: 0.1,
            far: 1000.0,
            mode: CameraMode::Fly,
        }
    }
}

impl Camera {
    /// 朝向单位向量。
    pub fn forward(&self) -> Vec3 {
        Vec3::new(
            self.yaw.cos() * self.pitch.cos(),
            self.pitch.sin(),
            self.yaw.sin() * self.pitch.cos(),
        )
        .normalize()
    }

    /// 右向单位向量（不依赖 pitch，沿水平面）。
    pub fn right(&self) -> Vec3 {
        // 水平面 forward = (cos yaw, 0, sin yaw)；右 = forward × +Y
        Vec3::new(self.yaw.sin(), 0.0, -self.yaw.cos())
    }

    pub fn view_matrix(&self) -> Mat4 {
        Mat4::look_to_rh(self.position, self.forward(), Vec3::Y)
    }

    pub fn projection_matrix(&self) -> Mat4 {
        // wgpu / WebGPU 的 NDC 深度范围是 0..1，glam 的 perspective_rh 输出符合此约定
        Mat4::perspective_rh(self.fov, self.aspect, self.near, self.far)
    }

    pub fn vp_matrix(&self) -> Mat4 {
        self.projection_matrix() * self.view_matrix()
    }

    /// 鼠标移动 → yaw / pitch 更新。
    /// `dx` / `dy` 单位为像素，`sensitivity` 为弧度/像素。
    pub fn apply_mouse(&mut self, dx: f32, dy: f32, sensitivity: f32) {
        self.yaw += dx * sensitivity;
        self.pitch -= dy * sensitivity;
        // 限制仰角，避免万向锁（gimbal lock）
        let limit = 89.0_f32.to_radians();
        self.pitch = self.pitch.clamp(-limit, limit);
    }

    /// 应用 WASD/空格/Shift 移动（Fly 模式）。`dt` 单位秒。
    pub fn apply_fly_input(&mut self, input: &crate::input::InputState, speed: f32, dt: f32) {
        let mut delta = Vec3::ZERO;
        // 水平方向 forward（无 pitch 分量），保持飞行手感
        let horiz_forward = Vec3::new(self.yaw.cos(), 0.0, self.yaw.sin()).normalize_or_zero();
        let right = self.right();
        if input.forward {
            delta += horiz_forward;
        }
        if input.backward {
            delta -= horiz_forward;
        }
        if input.right {
            delta += right;
        }
        if input.left {
            delta -= right;
        }
        if input.jump {
            delta += Vec3::Y;
        }
        if input.sneak {
            delta -= Vec3::Y;
        }
        if delta.length_squared() > 0.0 {
            delta = delta.normalize() * speed * dt;
            self.position += delta;
        }
    }
}
