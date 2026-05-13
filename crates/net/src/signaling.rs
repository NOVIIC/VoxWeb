//! WebSocket 信令客户端。
//!
//! 与 Cloudflare Workers 信令服务（[`signaling/`](../../../../signaling/)）通过 WebSocket
//! 交换 SDP / ICE candidate。协议详见 docs/networking/signaling.md。
//!
//! 设计：
//! - 浏览器 WebSocket 是事件驱动的；这里的回调（onopen/onmessage/onerror/onclose）只做
//!   "解析 + push 到 inbox"，不做任何长时间工作；
//! - 主线程在 RAF 内每帧 `poll()` 拿事件队列，再驱动后续状态机。

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::*;
use web_sys::{CloseEvent, ErrorEvent, MessageEvent, WebSocket};

use crate::NetError;

/// 注册时声明的角色。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Host,
    Join,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::Host => "host",
            Role::Join => "join",
        }
    }
}

/// 信令层产生的事件（按到达顺序排）。
#[derive(Clone, Debug)]
pub enum SignalingEvent {
    /// WebSocket open（已发出 register 但还未收到 registered）。
    Open,
    /// 服务端分配的 peer_id + 当前已存在的 peer 列表（join 端会拿到 host 的 id）。
    Registered {
        peer_id: u32,
        existing_peers: Vec<u32>,
        ice_servers: Vec<IceServerConfig>,
    },
    /// host 端收到的"有新 peer 加入"。
    PeerJoined { peer_id: u32 },
    /// 某个 peer 离开（host 会收到）。
    PeerLeft { peer_id: u32 },
    /// 收到来自 from 的 SDP offer（join 端会收到来自 host 的 offer；本期 host 主动发起）。
    Offer { from: u32, sdp: String },
    /// 收到来自 from 的 SDP answer。
    Answer { from: u32, sdp: String },
    /// 收到来自 from 的 ICE candidate（JSON 文本，原样传给 RTCPeerConnection.addIceCandidate）。
    Ice { from: u32, candidate: String },
    /// 房间被销毁（一般是 host_left）。
    RoomClosed { reason: String },
    /// 服务端发的错误消息（如 host_already_exists / no_host / room_full）。
    ServerError { message: String },
    /// WebSocket 本身异常（onerror）。
    SocketError { message: String },
    /// WebSocket 已关闭。
    Closed,
}

/// 从信令服务下发的 ICE Server 配置项。
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct IceServerConfig {
    pub urls: Vec<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub credential: Option<String>,
}

pub struct SignalingClient {
    socket: WebSocket,
    inbox: Rc<RefCell<VecDeque<SignalingEvent>>>,
    peer_id: Rc<Cell<Option<u32>>>,
    /// 持有所有事件回调闭包，保活到 SignalingClient drop 为止。
    _closures: Vec<JsValue>,
}

impl SignalingClient {
    /// 立即返回；后续连接结果通过 `poll()` 获取。
    /// `url` 是基础 URL（如 `ws://localhost:8787`），最终连接到 `{url}/room/{room}`。
    pub fn connect(
        url: &str,
        room: &str,
        role: Role,
        display_name: &str,
    ) -> Result<Self, NetError> {
        let full_url = format!("{}/room/{}", url.trim_end_matches('/'), room);
        let socket = WebSocket::new(&full_url).map_err(|_| NetError::SignalingUnreachable)?;
        // 二进制类型其实用不上（信令全 JSON 文本），保持默认 `Blob` 也行；显式设为 ArrayBuffer 兼顾未来扩展。
        socket.set_binary_type(web_sys::BinaryType::Arraybuffer);

        let inbox: Rc<RefCell<VecDeque<SignalingEvent>>> = Rc::new(RefCell::new(VecDeque::new()));
        let peer_id: Rc<Cell<Option<u32>>> = Rc::new(Cell::new(None));

        let mut closures: Vec<JsValue> = Vec::new();

        // onopen：立即发 register，并向 inbox 推 Open
        {
            let socket_clone = socket.clone();
            let inbox_clone = inbox.clone();
            let role_str = role.as_str().to_owned();
            let display = display_name.to_owned();
            let on_open = Closure::<dyn FnMut(JsValue)>::new(move |_evt: JsValue| {
                let register = serde_json::json!({
                    "kind": "register",
                    "role": role_str,
                    "display_name": display,
                });
                if let Ok(text) = serde_json::to_string(&register) {
                    let _ = socket_clone.send_with_str(&text);
                }
                inbox_clone.borrow_mut().push_back(SignalingEvent::Open);
            });
            socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));
            closures.push(on_open.into_js_value());
        }

        // onmessage：JSON parse → 转 SignalingEvent → push inbox
        {
            let inbox_clone = inbox.clone();
            let peer_id_clone = peer_id.clone();
            let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |evt: MessageEvent| {
                let Some(text) = evt.data().as_string() else {
                    // 二进制帧 ignore（信令协议只有 JSON 文本）
                    return;
                };
                match parse_event(&text) {
                    Ok(event) => {
                        if let SignalingEvent::Registered { peer_id, .. } = &event {
                            peer_id_clone.set(Some(*peer_id));
                        }
                        inbox_clone.borrow_mut().push_back(event);
                    }
                    Err(err) => {
                        log::warn!("[signaling] 无法解析消息：{err} / raw={text}");
                    }
                }
            });
            socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
            closures.push(on_message.into_js_value());
        }

        // onerror
        {
            let inbox_clone = inbox.clone();
            let on_error = Closure::<dyn FnMut(ErrorEvent)>::new(move |evt: ErrorEvent| {
                let message = evt.message();
                inbox_clone
                    .borrow_mut()
                    .push_back(SignalingEvent::SocketError { message });
            });
            socket.set_onerror(Some(on_error.as_ref().unchecked_ref()));
            closures.push(on_error.into_js_value());
        }

        // onclose
        {
            let inbox_clone = inbox.clone();
            let on_close = Closure::<dyn FnMut(CloseEvent)>::new(move |_evt: CloseEvent| {
                inbox_clone.borrow_mut().push_back(SignalingEvent::Closed);
            });
            socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));
            closures.push(on_close.into_js_value());
        }

        Ok(Self {
            socket,
            inbox,
            peer_id,
            _closures: closures,
        })
    }

    /// 当前自己被分配的 peer_id（Registered 之后才有）。
    pub fn peer_id(&self) -> Option<u32> {
        self.peer_id.get()
    }

    /// 拉走当前累积的事件队列（按 FIFO）。
    pub fn poll(&self) -> Vec<SignalingEvent> {
        self.inbox.borrow_mut().drain(..).collect()
    }

    pub fn send_offer(&self, to: u32, sdp: &str) {
        self.send_json(serde_json::json!({
            "kind": "offer",
            "to": to,
            "sdp": sdp,
        }));
    }

    pub fn send_answer(&self, to: u32, sdp: &str) {
        self.send_json(serde_json::json!({
            "kind": "answer",
            "to": to,
            "sdp": sdp,
        }));
    }

    pub fn send_ice(&self, to: u32, candidate: &str) {
        self.send_json(serde_json::json!({
            "kind": "ice",
            "to": to,
            "candidate": candidate,
        }));
    }

    pub fn send_leave(&self) {
        self.send_json(serde_json::json!({ "kind": "leave" }));
    }

    fn send_json(&self, value: serde_json::Value) {
        if self.socket.ready_state() != WebSocket::OPEN {
            // 还没连上 / 已关闭，丢弃
            return;
        }
        if let Ok(text) = serde_json::to_string(&value) {
            let _ = self.socket.send_with_str(&text);
        }
    }
}

impl Drop for SignalingClient {
    fn drop(&mut self) {
        // 摘掉回调，避免 close 触发已释放的 inbox
        self.socket.set_onopen(None);
        self.socket.set_onmessage(None);
        self.socket.set_onerror(None);
        self.socket.set_onclose(None);
        let _ = self.socket.close();
    }
}

// 将 JSON 帧解析为 SignalingEvent。
// 失败时返回错误（onmessage 调用方仅 log，不传播）。
fn parse_event(text: &str) -> Result<SignalingEvent, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("json: {e}"))?;
    let kind = value
        .get("kind")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing kind".to_string())?;

    match kind {
        "registered" => {
            let peer_id = value
                .get("peer_id")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| "registered.peer_id missing".to_string())?
                as u32;
            let existing_peers = value
                .get("existing_peers")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_u64().map(|n| n as u32))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let ice_servers = value
                .get("ice_servers")
                .cloned()
                .map(|v| serde_json::from_value::<Vec<IceServerConfig>>(v).unwrap_or_default())
                .unwrap_or_default();
            Ok(SignalingEvent::Registered {
                peer_id,
                existing_peers,
                ice_servers,
            })
        }
        "peer_joined" => {
            let peer_id = value
                .get("peer_id")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| "peer_joined.peer_id missing".to_string())?
                as u32;
            Ok(SignalingEvent::PeerJoined { peer_id })
        }
        "peer_left" => {
            let peer_id = value
                .get("peer_id")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| "peer_left.peer_id missing".to_string())?
                as u32;
            Ok(SignalingEvent::PeerLeft { peer_id })
        }
        "offer" | "answer" => {
            let from = value
                .get("from")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| format!("{kind}.from missing"))? as u32;
            let sdp = value
                .get("sdp")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("{kind}.sdp missing"))?
                .to_owned();
            if kind == "offer" {
                Ok(SignalingEvent::Offer { from, sdp })
            } else {
                Ok(SignalingEvent::Answer { from, sdp })
            }
        }
        "ice" => {
            let from = value
                .get("from")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| "ice.from missing".to_string())? as u32;
            let candidate = value
                .get("candidate")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "ice.candidate missing".to_string())?
                .to_owned();
            Ok(SignalingEvent::Ice { from, candidate })
        }
        "room_closed" => {
            let reason = value
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_owned();
            Ok(SignalingEvent::RoomClosed { reason })
        }
        "error" => {
            let message = value
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_owned();
            Ok(SignalingEvent::ServerError { message })
        }
        other => Err(format!("unknown kind: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_registered() {
        let txt = r#"{"kind":"registered","peer_id":3,"existing_peers":[1,2],"ice_servers":[{"urls":["stun:stun.l.google.com:19302"]}]}"#;
        let ev = parse_event(txt).unwrap();
        match ev {
            SignalingEvent::Registered {
                peer_id,
                existing_peers,
                ice_servers,
            } => {
                assert_eq!(peer_id, 3);
                assert_eq!(existing_peers, vec![1, 2]);
                assert_eq!(ice_servers.len(), 1);
            }
            other => panic!("expected Registered, got {other:?}"),
        }
    }

    #[test]
    fn parse_offer() {
        let txt = r#"{"kind":"offer","from":1,"sdp":"v=0..."}"#;
        let ev = parse_event(txt).unwrap();
        assert!(matches!(ev, SignalingEvent::Offer { from: 1, .. }));
    }

    #[test]
    fn parse_ice() {
        let txt = r#"{"kind":"ice","from":2,"candidate":"candidate:..."}"#;
        let ev = parse_event(txt).unwrap();
        assert!(matches!(ev, SignalingEvent::Ice { from: 2, .. }));
    }

    #[test]
    fn parse_room_closed() {
        let txt = r#"{"kind":"room_closed","reason":"host_left"}"#;
        let ev = parse_event(txt).unwrap();
        assert!(matches!(ev, SignalingEvent::RoomClosed { .. }));
    }

    #[test]
    fn parse_unknown_kind_errors() {
        let txt = r#"{"kind":"oops"}"#;
        assert!(parse_event(txt).is_err());
    }
}
