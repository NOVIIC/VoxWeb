//! 键盘 / 鼠标输入状态采集。
//!
//! 每帧从浏览器事件中累积 WASD / 空格 / 鼠标移动等输入，供相机/物理/HUD 消费。
//!
//! 字段语义：
//! - "_held" 后缀：键当前按下状态（持续，与 keydown/keyup 配对）
//! - "_just_pressed" 后缀：边沿事件，仅当此帧按下时为 true，帧末 `reset_delta` 清零
//! - `hotbar_request`: 1-9 键产生的 hotbar 切换请求（0..=8 索引）
//! - `fly_toggle_pending`: 双击空格检测出来的飞行模式切换请求

use winit::keyboard::KeyCode;

/// 双击空格判定时间窗口（毫秒）。
const DOUBLE_TAP_WINDOW_MS: f64 = 250.0;

/// 单帧的输入快照。
#[derive(Clone)]
pub struct InputState {
    // —— WASD（持续按下）——
    pub forward: bool,
    pub backward: bool,
    pub left: bool,
    pub right: bool,

    // —— 跳跃 ——
    /// 空格当前是否按下（Fly 模式上升 / Walk 模式见 jump_just_pressed）
    pub jump_held: bool,
    /// 本帧按下空格的边沿事件（Walk 模式触发起跳）
    pub jump_just_pressed: bool,

    // —— 下蹲 / 飞行下降 ——
    pub sneak: bool,

    // —— 鼠标按键 ——
    /// 左键当前是否按下（用于连续挖掘）
    pub break_held: bool,
    /// 本帧左键按下边沿
    pub break_just_pressed: bool,
    /// 右键当前是否按下
    pub place_held: bool,
    /// 本帧右键按下边沿
    pub place_just_pressed: bool,

    // —— 边沿事件 ——
    /// 1-9 数字键：选 hotbar 第 i 格（0..=8）
    pub hotbar_request: Option<u8>,
    /// 双击空格触发的飞行模式切换
    pub fly_toggle_pending: bool,
    /// 打开聊天（Phase 6 用）
    pub chat_open: bool,
    /// ESC 菜单（Phase 6 用）
    pub esc_menu: bool,

    // —— 鼠标移动 ——
    /// 本帧的移动增量（像素）
    pub mouse_dx: f32,
    pub mouse_dy: f32,
    /// 指针是否被锁定（仅锁定状态下消费 dx/dy）
    pub pointer_locked: bool,

    // —— 内部状态：双击空格判定 ——
    /// 上一次空格按下的时间（毫秒）。None 表示尚未按过（初始态）。
    last_space_press_at_ms: Option<f64>,
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            forward: false,
            backward: false,
            left: false,
            right: false,
            jump_held: false,
            jump_just_pressed: false,
            sneak: false,
            break_held: false,
            break_just_pressed: false,
            place_held: false,
            place_just_pressed: false,
            hotbar_request: None,
            fly_toggle_pending: false,
            chat_open: false,
            esc_menu: false,
            mouse_dx: 0.0,
            mouse_dy: 0.0,
            pointer_locked: false,
            last_space_press_at_ms: None,
        }
    }
}

impl InputState {
    /// 帧末清掉单帧边沿数据（鼠标 dx/dy、just_pressed 边沿、hotbar/fly 切换请求）。
    /// 持续按下的状态（_held / sneak 等）不清。
    pub fn reset_delta(&mut self) {
        self.mouse_dx = 0.0;
        self.mouse_dy = 0.0;
        self.jump_just_pressed = false;
        self.break_just_pressed = false;
        self.place_just_pressed = false;
        self.hotbar_request = None;
        self.fly_toggle_pending = false;
        self.chat_open = false;
        self.esc_menu = false;
    }

    /// 处理键盘按下事件。
    /// `now_ms` 为当前 performance.now() 毫秒值，用于双击空格判定（由调用方注入便于测试）。
    pub fn on_key_down(&mut self, key: KeyCode, now_ms: f64) {
        match key {
            KeyCode::KeyW => self.forward = true,
            KeyCode::KeyS => self.backward = true,
            KeyCode::KeyA => self.left = true,
            KeyCode::KeyD => self.right = true,
            KeyCode::Space => {
                if !self.jump_held {
                    self.jump_just_pressed = true;
                    // 双击检测：上一次按下距现在在窗口内则触发
                    let triggered = self
                        .last_space_press_at_ms
                        .is_some_and(|last| now_ms - last < DOUBLE_TAP_WINDOW_MS);
                    if triggered {
                        self.fly_toggle_pending = true;
                        self.last_space_press_at_ms = None;
                    } else {
                        self.last_space_press_at_ms = Some(now_ms);
                    }
                }
                self.jump_held = true;
            }
            KeyCode::ShiftLeft | KeyCode::ShiftRight => self.sneak = true,
            KeyCode::KeyT => self.chat_open = true,
            KeyCode::Escape => self.esc_menu = true,
            KeyCode::Digit1 => self.hotbar_request = Some(0),
            KeyCode::Digit2 => self.hotbar_request = Some(1),
            KeyCode::Digit3 => self.hotbar_request = Some(2),
            KeyCode::Digit4 => self.hotbar_request = Some(3),
            KeyCode::Digit5 => self.hotbar_request = Some(4),
            KeyCode::Digit6 => self.hotbar_request = Some(5),
            KeyCode::Digit7 => self.hotbar_request = Some(6),
            KeyCode::Digit8 => self.hotbar_request = Some(7),
            KeyCode::Digit9 => self.hotbar_request = Some(8),
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
            KeyCode::Space => self.jump_held = false,
            KeyCode::ShiftLeft | KeyCode::ShiftRight => self.sneak = false,
            _ => {}
        }
    }

    /// 处理鼠标按下事件（浏览器 MouseEvent.button 值：0=左、1=中、2=右）。
    pub fn on_mouse_down(&mut self, button: u16) {
        match button {
            0 => {
                if !self.break_held {
                    self.break_just_pressed = true;
                }
                self.break_held = true;
            }
            2 => {
                if !self.place_held {
                    self.place_just_pressed = true;
                }
                self.place_held = true;
            }
            _ => {}
        }
    }

    /// 处理鼠标松开事件。
    pub fn on_mouse_up(&mut self, button: u16) {
        match button {
            0 => self.break_held = false,
            2 => self.place_held = false,
            _ => {}
        }
    }

    /// 累积鼠标移动增量。
    pub fn on_mouse_move(&mut self, dx: f32, dy: f32) {
        self.mouse_dx += dx;
        self.mouse_dy += dy;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn space_double_tap_within_window_triggers_fly_toggle() {
        let mut s = InputState::default();
        s.on_key_down(KeyCode::Space, 100.0);
        assert!(!s.fly_toggle_pending, "首次按下不应触发");
        s.on_key_up(KeyCode::Space);
        s.on_key_down(KeyCode::Space, 200.0);
        assert!(s.fly_toggle_pending, "200ms 内再按一下应触发");
    }

    #[test]
    fn space_two_taps_outside_window_no_toggle() {
        let mut s = InputState::default();
        s.on_key_down(KeyCode::Space, 0.0);
        s.on_key_up(KeyCode::Space);
        s.on_key_down(KeyCode::Space, 500.0); // 远大于 250ms
        assert!(!s.fly_toggle_pending);
    }

    #[test]
    fn reset_delta_clears_edges() {
        let mut s = InputState::default();
        s.on_key_down(KeyCode::Space, 0.0);
        s.on_mouse_down(0);
        s.on_mouse_move(5.0, 5.0);
        s.hotbar_request = Some(3);
        s.reset_delta();
        assert!(!s.jump_just_pressed);
        assert!(!s.break_just_pressed);
        assert_eq!(s.mouse_dx, 0.0);
        assert_eq!(s.hotbar_request, None);
        // 持续状态保留
        assert!(s.jump_held);
        assert!(s.break_held);
    }

    #[test]
    fn mouse_button_mapping_right_is_place() {
        let mut s = InputState::default();
        s.on_mouse_down(2);
        assert!(s.place_just_pressed);
        assert!(s.place_held);
        assert!(!s.break_held);
    }

    #[test]
    fn mouse_button_middle_is_ignored() {
        let mut s = InputState::default();
        s.on_mouse_down(1);
        assert!(!s.break_held && !s.place_held);
    }

    #[test]
    fn digit_keys_request_hotbar() {
        let mut s = InputState::default();
        s.on_key_down(KeyCode::Digit1, 0.0);
        assert_eq!(s.hotbar_request, Some(0));
        s.on_key_down(KeyCode::Digit9, 0.0);
        assert_eq!(s.hotbar_request, Some(8));
    }
}
