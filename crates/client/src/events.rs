use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

use crate::app::AppState;
use crate::input::InputState;
use crate::settings_storage;

use super::{App, flush_dirty_best_effort, now_ms};

pub(super) fn install_event_listeners(
    canvas: &HtmlCanvasElement,
    document: &web_sys::Document,
    input: Rc<RefCell<InputState>>,
    egui_events: Rc<RefCell<Vec<egui::Event>>>,
    app: Rc<RefCell<App>>,
) -> Result<(), JsValue> {
    // —— 点击 canvas → 请求指针锁（游戏态）或从暂停菜单外的空白区域恢复 ——
    {
        let canvas_clone = canvas.clone();
        let app_clone = app.clone();
        let on_click = Closure::<dyn FnMut(_)>::new(move |_e: web_sys::MouseEvent| {
            let mut a = app_clone.borrow_mut();
            match a.state {
                AppState::InGame {
                    paused: false,
                    chat_open: false,
                } => {
                    canvas_clone.request_pointer_lock();
                }
                AppState::InGame {
                    paused: true,
                    chat_open: false,
                } if !a.egui_ctx.is_pointer_over_egui() => {
                    if let Some(g) = a.game.as_ref() {
                        settings_storage::save(&g.settings);
                    }
                    a.state = AppState::InGame {
                        paused: false,
                        chat_open: false,
                    };
                    a.request_pointer_lock_next = false;
                    a.input.borrow_mut().clear_held();
                    canvas_clone.request_pointer_lock();
                }
                _ => {}
            }
        });
        canvas.add_event_listener_with_callback("click", on_click.as_ref().unchecked_ref())?;
        on_click.forget();
    }

    // —— pointerlockchange ——
    {
        let input_clone = input.clone();
        let document_clone = document.clone();
        let canvas_id = canvas.clone();
        let app_clone = app.clone();
        let on_lock_change = Closure::<dyn FnMut()>::new(move || {
            let locked = document_clone
                .pointer_lock_element()
                .map(|el| el == *canvas_id.as_ref())
                .unwrap_or(false);
            let mut s = input_clone.borrow_mut();
            let was_locked = s.pointer_locked;
            if was_locked != locked {
                s.clear_held();
                // 当指针锁因为用户按 ESC 而无预期释放时（纯游戏态、未暂停未聊天），
                // 浏览器可能吞掉 ESC keydown 事件，导致 esc_menu 边沿永远不会被设。
                // 这里从 pointerlockchange 补设 esc_menu，保证暂停菜单能正常弹出。
                if !locked && was_locked {
                    let a = app_clone.borrow();
                    if matches!(
                        a.state,
                        AppState::InGame {
                            paused: false,
                            chat_open: false
                        }
                    ) {
                        drop(a);
                        s.esc_menu = true;
                    }
                }
            }
            s.pointer_locked = locked;
        });
        document.add_event_listener_with_callback(
            "pointerlockchange",
            on_lock_change.as_ref().unchecked_ref(),
        )?;
        on_lock_change.forget();
    }

    // —— 键盘 ——
    // 活跃游戏（InGame 且未暂停未聊天）：用 e.code() 映射物理键到 KeyCode 给物理/相机/hotbar；
    // 其它状态（Lobby/Connecting/InGame 暂停或聊天聚焦）：用 e.key() 转 egui::Event::Text / Event::Key，
    // 让 TextEdit 收到输入。
    {
        let input_clone = input.clone();
        let app_clone = app.clone();
        let egui_events_clone = egui_events.clone();
        let on_keydown = Closure::<dyn FnMut(_)>::new(move |e: web_sys::KeyboardEvent| {
            let forward_to_egui = !matches!(
                app_clone.borrow().state,
                AppState::InGame {
                    paused: false,
                    chat_open: false
                }
            );
            if forward_to_egui {
                forward_keydown_to_egui(&e, &egui_events_clone);
                // 注意：即便已转给 egui，依然让 InputState 接到边沿事件（ESC/T 等），
                // 让主循环能消费这些 edge-trigger 字段切换 paused / chat_open。
                if let Some(key) = map_key(&e.code()) {
                    input_clone.borrow_mut().on_key_down(key, now_ms());
                }
                return;
            }
            if let Some(key) = map_key(&e.code()) {
                input_clone.borrow_mut().on_key_down(key, now_ms());
            }
            if input_clone.borrow().pointer_locked {
                e.prevent_default();
            }
        });
        document
            .add_event_listener_with_callback("keydown", on_keydown.as_ref().unchecked_ref())?;
        on_keydown.forget();
    }
    {
        let input_clone = input.clone();
        let app_clone = app.clone();
        let egui_events_clone = egui_events.clone();
        let on_keyup = Closure::<dyn FnMut(_)>::new(move |e: web_sys::KeyboardEvent| {
            let forward_to_egui = !matches!(
                app_clone.borrow().state,
                AppState::InGame {
                    paused: false,
                    chat_open: false
                }
            );
            if forward_to_egui {
                forward_keyup_to_egui(&e, &egui_events_clone);
                if let Some(key) = map_key(&e.code()) {
                    input_clone.borrow_mut().on_key_up(key);
                }
                return;
            }
            if let Some(key) = map_key(&e.code()) {
                input_clone.borrow_mut().on_key_up(key);
            }
        });
        document.add_event_listener_with_callback("keyup", on_keyup.as_ref().unchecked_ref())?;
        on_keyup.forget();
    }

    // —— 鼠标移动 ——
    // 指针锁时累积 dx/dy 给相机；否则上报位置给 egui（UI 命中测试）。
    {
        let input_clone = input.clone();
        let egui_events_clone = egui_events.clone();
        let on_mousemove = Closure::<dyn FnMut(_)>::new(move |e: web_sys::MouseEvent| {
            let mut s = input_clone.borrow_mut();
            if s.pointer_locked {
                s.on_mouse_move(e.movement_x() as f32, e.movement_y() as f32);
            } else {
                egui_events_clone
                    .borrow_mut()
                    .push(egui::Event::PointerMoved(egui::pos2(
                        e.client_x() as f32,
                        e.client_y() as f32,
                    )));
            }
        });
        document
            .add_event_listener_with_callback("mousemove", on_mousemove.as_ref().unchecked_ref())?;
        on_mousemove.forget();
    }

    // —— 鼠标按下 ——
    // InGame：转给 InputState；Lobby：转 egui PointerButton 事件。
    {
        let input_clone = input.clone();
        let egui_events_clone = egui_events.clone();
        let app_clone = app.clone();
        let on_mousedown = Closure::<dyn FnMut(_)>::new(move |e: web_sys::MouseEvent| {
            // 防止右键弹出浏览器上下文菜单（仅在 InGame 锁定指针时）
            let is_ingame_active = matches!(
                app_clone.borrow().state,
                AppState::InGame {
                    paused: false,
                    chat_open: false
                }
            );
            if is_ingame_active {
                input_clone.borrow_mut().on_mouse_down(e.button() as u16);
                if e.button() == 2 {
                    e.prevent_default();
                }
            } else if let Some(button) = map_pointer_button(e.button()) {
                egui_events_clone
                    .borrow_mut()
                    .push(egui::Event::PointerButton {
                        pos: egui::pos2(e.client_x() as f32, e.client_y() as f32),
                        button,
                        pressed: true,
                        modifiers: egui::Modifiers::default(),
                    });
            }
        });
        canvas
            .add_event_listener_with_callback("mousedown", on_mousedown.as_ref().unchecked_ref())?;
        on_mousedown.forget();
    }

    // —— 鼠标松开：InGame 转给 InputState、Lobby 转 egui ——
    {
        let input_clone = input.clone();
        let egui_events_clone = egui_events.clone();
        let app_clone = app.clone();
        let on_mouseup = Closure::<dyn FnMut(_)>::new(move |e: web_sys::MouseEvent| {
            let is_ingame_active = matches!(
                app_clone.borrow().state,
                AppState::InGame {
                    paused: false,
                    chat_open: false
                }
            );
            if is_ingame_active {
                input_clone.borrow_mut().on_mouse_up(e.button() as u16);
            } else if let Some(button) = map_pointer_button(e.button()) {
                egui_events_clone
                    .borrow_mut()
                    .push(egui::Event::PointerButton {
                        pos: egui::pos2(e.client_x() as f32, e.client_y() as f32),
                        button,
                        pressed: false,
                        modifiers: egui::Modifiers::default(),
                    });
            }
        });
        document
            .add_event_listener_with_callback("mouseup", on_mouseup.as_ref().unchecked_ref())?;
        on_mouseup.forget();
    }

    // —— 阻止右键上下文菜单（InGame 时）——
    {
        let app_clone = app.clone();
        let on_contextmenu = Closure::<dyn FnMut(_)>::new(move |e: web_sys::MouseEvent| {
            if matches!(
                app_clone.borrow().state,
                AppState::InGame {
                    paused: false,
                    chat_open: false
                }
            ) {
                e.prevent_default();
            }
        });
        canvas.add_event_listener_with_callback(
            "contextmenu",
            on_contextmenu.as_ref().unchecked_ref(),
        )?;
        on_contextmenu.forget();
    }

    // —— 鼠标滚轮：指针锁定时切换 hotbar ——
    {
        let input_clone = input.clone();
        let app_clone = app.clone();
        let on_wheel = Closure::<dyn FnMut(_)>::new(move |e: web_sys::WheelEvent| {
            let is_ingame_active = matches!(
                app_clone.borrow().state,
                AppState::InGame {
                    paused: false,
                    chat_open: false
                }
            );
            if is_ingame_active {
                input_clone.borrow_mut().on_mouse_wheel(e.delta_y());
                e.prevent_default();
            }
        });
        canvas.add_event_listener_with_callback("wheel", on_wheel.as_ref().unchecked_ref())?;
        on_wheel.forget();
    }

    // —— 页面离开 / 进入 BFCache 前尽力保存剩余 dirty chunk ——
    if let Some(window) = web_sys::window() {
        let app_clone = app.clone();
        let on_pagehide = Closure::<dyn FnMut()>::new(move || {
            flush_dirty_best_effort(&app_clone, "pagehide");
        });
        window
            .add_event_listener_with_callback("pagehide", on_pagehide.as_ref().unchecked_ref())?;
        on_pagehide.forget();
    }

    Ok(())
}

/// 浏览器 MouseEvent.button() 数值 → egui PointerButton。
fn map_pointer_button(button: i16) -> Option<egui::PointerButton> {
    match button {
        0 => Some(egui::PointerButton::Primary),
        1 => Some(egui::PointerButton::Middle),
        2 => Some(egui::PointerButton::Secondary),
        _ => None,
    }
}

/// 在 Lobby / Connecting 等 InGame 之外的状态下，把 keydown 事件转 egui Event。
/// - 可识别的功能键（Backspace / Enter / 箭头键 / Tab / Esc / Home / End …）→ Event::Key{pressed=true}
/// - 单字符 + 无 Ctrl/Alt/Meta → Event::Text，让 TextEdit 接收
fn forward_keydown_to_egui(
    e: &web_sys::KeyboardEvent,
    egui_events: &Rc<RefCell<Vec<egui::Event>>>,
) {
    let modifiers = egui::Modifiers {
        alt: e.alt_key(),
        ctrl: e.ctrl_key(),
        shift: e.shift_key(),
        mac_cmd: e.meta_key(),
        command: e.ctrl_key() || e.meta_key(),
    };
    let key_str = e.key();

    if let Some(egui_key) = map_web_key_to_egui(&key_str) {
        egui_events.borrow_mut().push(egui::Event::Key {
            key: egui_key,
            physical_key: None,
            pressed: true,
            repeat: e.repeat(),
            modifiers,
        });
        // 阻止浏览器默认行为：Tab 切换焦点、Backspace 后退、空格滚动等
        if matches!(
            egui_key,
            egui::Key::Backspace
                | egui::Key::Tab
                | egui::Key::ArrowUp
                | egui::Key::ArrowDown
                | egui::Key::ArrowLeft
                | egui::Key::ArrowRight
                | egui::Key::Space
        ) {
            e.prevent_default();
        }
        // egui 的 TextEdit 依赖 Text 事件插入字符；Space 同时也是功能键，
        // 因此需要在没有组合键修饰时额外补一个文本空格，聊天输入框才能输入空格。
        if egui_key == egui::Key::Space && !modifiers.ctrl && !modifiers.alt && !modifiers.mac_cmd {
            egui_events
                .borrow_mut()
                .push(egui::Event::Text(" ".to_string()));
        }
    } else if key_str.chars().count() == 1
        && !modifiers.ctrl
        && !modifiers.alt
        && !modifiers.mac_cmd
    {
        // 单个可见字符：作为文本输入
        let c = key_str.chars().next().unwrap();
        if !c.is_control() {
            egui_events.borrow_mut().push(egui::Event::Text(key_str));
        }
    }
}

/// 同上的 keyup 版本：只发 Key{pressed=false}（egui 用它跟踪按住状态）。
fn forward_keyup_to_egui(e: &web_sys::KeyboardEvent, egui_events: &Rc<RefCell<Vec<egui::Event>>>) {
    let modifiers = egui::Modifiers {
        alt: e.alt_key(),
        ctrl: e.ctrl_key(),
        shift: e.shift_key(),
        mac_cmd: e.meta_key(),
        command: e.ctrl_key() || e.meta_key(),
    };
    if let Some(egui_key) = map_web_key_to_egui(&e.key()) {
        egui_events.borrow_mut().push(egui::Event::Key {
            key: egui_key,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers,
        });
    }
}

/// `KeyboardEvent.key`（如 "Backspace", "ArrowLeft"）→ egui::Key。
/// 单字符键（如 "a", "1"）返回 None，由 Text 事件处理。
fn map_web_key_to_egui(key: &str) -> Option<egui::Key> {
    use egui::Key;
    Some(match key {
        "Backspace" => Key::Backspace,
        "Delete" => Key::Delete,
        "Enter" => Key::Enter,
        "Tab" => Key::Tab,
        "Escape" => Key::Escape,
        "ArrowLeft" => Key::ArrowLeft,
        "ArrowRight" => Key::ArrowRight,
        "ArrowUp" => Key::ArrowUp,
        "ArrowDown" => Key::ArrowDown,
        "Home" => Key::Home,
        "End" => Key::End,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        " " => Key::Space,
        _ => return None,
    })
}

/// 把 web_sys::KeyboardEvent.code() 映射到 winit::KeyCode（输入层使用统一枚举）。
fn map_key(code: &str) -> Option<winit::keyboard::KeyCode> {
    use winit::keyboard::KeyCode;
    Some(match code {
        "KeyW" => KeyCode::KeyW,
        "KeyA" => KeyCode::KeyA,
        "KeyS" => KeyCode::KeyS,
        "KeyD" => KeyCode::KeyD,
        "KeyT" => KeyCode::KeyT,
        "Space" => KeyCode::Space,
        "ShiftLeft" => KeyCode::ShiftLeft,
        "ShiftRight" => KeyCode::ShiftRight,
        "Escape" => KeyCode::Escape,
        "Digit1" => KeyCode::Digit1,
        "Digit2" => KeyCode::Digit2,
        "Digit3" => KeyCode::Digit3,
        "Digit4" => KeyCode::Digit4,
        "Digit5" => KeyCode::Digit5,
        "Digit6" => KeyCode::Digit6,
        "Digit7" => KeyCode::Digit7,
        "Digit8" => KeyCode::Digit8,
        "Digit9" => KeyCode::Digit9,
        _ => return None,
    })
}
