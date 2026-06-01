//! 第一人称相机：位置、朝向、视图/投影矩阵。
//!
//! Phase 3 起 `Camera` 只负责朝向与矩阵，位置由 `LocalPhysics` 每帧驱动同步。

use glam::{Mat4, Vec3};

/// 相机模式：决定物理子系统跑哪条分支。
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CameraMode {
    /// 飞行：WASD + 空格/Shift，无重力、有碰撞（分轴扫动，与 Walk 一致）
    Fly,
    /// 步行：受重力、跳跃、AABB 分轴碰撞
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
        }
    }
}

impl Camera {
    /// 朝向单位向量（含 pitch 分量）。
    pub fn forward(&self) -> Vec3 {
        Vec3::new(
            self.yaw.cos() * self.pitch.cos(),
            self.pitch.sin(),
            self.yaw.sin() * self.pitch.cos(),
        )
        .normalize()
    }

    /// 水平面（XZ）上的前向单位向量，丢弃 pitch 分量。
    /// Walk 模式按这个走，避免视角朝下时反而向地里走。
    pub fn forward_horizontal(&self) -> Vec3 {
        Vec3::new(self.yaw.cos(), 0.0, self.yaw.sin()).normalize_or_zero()
    }

    /// 右向单位向量（不依赖 pitch，沿水平面）。
    /// wgpu 右手系下：forward × up = (-sin yaw, 0, cos yaw)
    pub fn right(&self) -> Vec3 {
        Vec3::new(-self.yaw.sin(), 0.0, self.yaw.cos())
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_horizontal_strips_pitch() {
        let c = Camera {
            yaw: 0.0,
            pitch: -0.5,
            ..Camera::default()
        };
        let fh = c.forward_horizontal();
        assert!(fh.y.abs() < 1e-6);
        assert!((fh.x - 1.0).abs() < 1e-6);
        assert!(fh.z.abs() < 1e-6);
    }

    #[test]
    fn right_orthogonal_to_horizontal_forward() {
        let c = Camera::default();
        let fh = c.forward_horizontal();
        let r = c.right();
        assert!(fh.dot(r).abs() < 1e-5);
    }
}
