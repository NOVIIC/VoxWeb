//! 键盘 / 鼠标输入状态采集。
//!
//! 每帧从浏览器事件中累积 WASD / 空格 / 鼠标移动等输入，
//! 供相机和物理系统消费。

use winit::keyboard::KeyCode;

/// 单帧的输入快照。
#[derive(Default, Clone)]
pub struct InputState {
    pub forward: bool,
    pub backward: bool,
    pub left: bool,
    pub right: bool,
    pub jump: bool,
    pub sneak: bool,
    pub fly_toggle: bool,
    pub break_action: bool,
    pub place_action: bool,
    pub chat_open: bool,
    pub esc_menu: bool,
    /// 鼠标在本帧的移动增量（像素）
    pub mouse_dx: f32,
    pub mouse_dy: f32,
    /// 指针是否被锁定
    pub pointer_locked: bool,
}

impl InputState {
    /// 清空本帧的增量数据（鼠标移动、单次按键）。
    pub fn reset_delta(&mut self) {
        self.mouse_dx = 0.0;
        self.mouse_dy = 0.0;
    }

    /// 处理键盘按下事件。
    pub fn on_key_down(&mut self, key: KeyCode) {
        match key {
            KeyCode::KeyW => self.forward = true,
            KeyCode::KeyS => self.backward = true,
            KeyCode::KeyA => self.left = true,
            KeyCode::KeyD => self.right = true,
            KeyCode::Space => self.jump = true,
            KeyCode::ShiftLeft | KeyCode::ShiftRight => self.sneak = true,
            KeyCode::KeyT => self.chat_open = true,
            KeyCode::Escape => self.esc_menu = true,
            _ => {}
        }
    }

    /// 处理键盘释放事件。
    pub fn on_key_up(&mut self, key: KeyCode) {
        match key {
            KeyCode::KeyW => self.forward = false,
            KeyCode::KeyS => self.backward = false,
            KeyCode::KeyA => self.left = false,
            KeyCode::KeyD => self.right = false,
            KeyCode::Space => self.jump = false,
            KeyCode::ShiftLeft | KeyCode::ShiftRight => self.sneak = false,
            _ => {}
        }
    }

    /// 处理鼠标按下事件。
    pub fn on_mouse_down(&mut self, button: u16) {
        match button {
            0 => self.break_action = true, // 左键 = 挖掘
            1 => self.place_action = true, // 右键 = 放置
            _ => {}
        }
    }

    /// 累积鼠标移动增量。
    pub fn on_mouse_move(&mut self, dx: f32, dy: f32) {
        self.mouse_dx += dx;
        self.mouse_dy += dy;
    }
}
