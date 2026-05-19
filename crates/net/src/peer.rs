//! WebRTC `RtcPeerConnection` 包装：管理一条到对等端的连接和两条 DataChannel。
//!
//! 设计概要：
//! - Host 侧调用 [`PeerConnection::create_offerer`]：在构造时主动 `createDataChannel("reliable" / "unreliable")`，
//!   随后 `start_offer()` 发起 createOffer + setLocalDescription，结果以 [`PeerEvent::OfferReady`] 形式推入 inbox；
//! - Remote 侧调用 [`PeerConnection::create_answerer`]：构造时只挂 `ondatachannel`，由 Host 通过 SDP 协商把 DC 推过来；
//!   收到 offer 后调用 `accept_offer(sdp)`，自动 setRemoteDescription → createAnswer → setLocalDescription，结果以
//!   [`PeerEvent::AnswerReady`] 推回；
//! - ICE candidate 收集：JS 端 `onicecandidate` 回调把 candidate 字符串塞进 inbox，供主循环转发到信令；
//! - 远端 SDP / candidate 由主循环调用 [`PeerConnection::apply_answer`] / [`PeerConnection::add_remote_ice`] 应用；
//! - DataChannel 收发：`send()` 同步写；接收端 `onmessage` 把字节 push 到 inbox。
//!
//! 异步设计：所有 Promise-based 操作内部用 `wasm_bindgen_futures::spawn_local` 包裹，
//! 结果通过 inbox 同步回主循环，避免主循环阻塞或在多个 `Rc<RefCell<...>>` 之间持有 `.await`。

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use js_sys::{Array, Object, Reflect};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{
    MessageEvent, RtcConfiguration, RtcDataChannel, RtcDataChannelEvent, RtcDataChannelInit,
    RtcDataChannelType, RtcIceCandidate, RtcIceCandidateInit, RtcPeerConnection,
    RtcPeerConnectionIceEvent, RtcSdpType, RtcSessionDescriptionInit,
};

use crate::NetError;
use crate::signaling::IceServerConfig;
use crate::transport::ChannelKind;

/// PeerConnection 当前状态机（向上汇报用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerState {
    /// 刚构造，未开始协商。
    New,
    /// 正在协商（已发 / 收 offer 或 ICE 收集中）。
    Negotiating,
    /// ICE 连通 + 两条 DataChannel 都 open。
    Connected,
    /// 协商失败或断线。
    Disconnected,
    /// 不可恢复的失败（PC failed）。
    Failed,
}

/// 异步操作和事件流统一通过 PeerEvent 暴露给主循环。
#[derive(Clone, Debug)]
pub enum PeerEvent {
    /// `createOffer + setLocalDescription` 完成。SDP 字符串需经信令发给对端。
    OfferReady(String),
    /// `setRemoteDescription(offer) + createAnswer + setLocalDescription` 完成。SDP 字符串经信令回。
    AnswerReady(String),
    /// `setRemoteDescription(answer)` 完成（仅 Host 侧需要等这个推进状态）。
    RemoteDescApplied,
    /// `addIceCandidate` 完成（用于诊断；本期主循环可忽略）。
    IceApplied,
    /// JS 端的 `onicecandidate` 给出的本地候选；需经信令送给对端。
    /// `candidate` 为 candidate 对象 JSON 字符串（含 sdpMid / sdpMLineIndex）。
    LocalIce(String),
    /// 某条 DataChannel 收到字节。
    Message {
        channel: ChannelKind,
        bytes: Vec<u8>,
    },
    /// 状态变化（合流 ICE 状态 + DC 状态）。
    StateChanged(PeerState),
    /// 异步操作失败（构造 RTC / setLocal / setRemote / addIce / createOffer / createAnswer）。
    NegotiationError(String),
    /// Reliable DataChannel 的 `bufferedAmount` 降到阈值以下，可恢复发送。
    BufferFreed,
}

/// 与某个对等端的 P2P 连接。
pub struct PeerConnection {
    pub peer_id: u32,
    rtc: RtcPeerConnection,

    state: Rc<Cell<PeerState>>,
    reliable: Rc<RefCell<Option<RtcDataChannel>>>,
    unreliable: Rc<RefCell<Option<RtcDataChannel>>>,
    reliable_open: Rc<Cell<bool>>,
    unreliable_open: Rc<Cell<bool>>,
    /// Reliable DC 的发送缓冲区是否因超过阈值而暂停（等待 BufferFreed 事件）。
    reliable_paused: Rc<Cell<bool>>,
    inbox: Rc<RefCell<VecDeque<PeerEvent>>>,

    /// 保活所有 JS 闭包（onicecandidate / oniceconnectionstatechange / ondatachannel / DC 各事件）。
    _closures: Vec<JsValue>,
}

impl PeerConnection {
    /// Host 侧：主动 `createDataChannel("reliable")` + `createDataChannel("unreliable")`。
    /// 构造完后调用方应在某帧调用 `start_offer()` 发起协商。
    /// Reliable DC 的 `bufferedAmountLowThreshold`（字节）。
    /// 当 `bufferedAmount` 从高于降到低于此值时触发 `onbufferedamountlow`。
    const BUFFERED_LOW_THRESHOLD: u32 = 256 * 1024; // 256 KB

    /// `bufferedAmount` 超过此值暂停发送（字节）。
    const BUFFERED_HIGH_WATER: u32 = 1024 * 1024; // 1 MB

    pub fn create_offerer(peer_id: u32, ice_servers: &[IceServerConfig]) -> Result<Self, NetError> {
        let rtc = make_rtc(ice_servers)?;

        let state = Rc::new(Cell::new(PeerState::New));
        let reliable: Rc<RefCell<Option<RtcDataChannel>>> = Rc::new(RefCell::new(None));
        let unreliable: Rc<RefCell<Option<RtcDataChannel>>> = Rc::new(RefCell::new(None));
        let reliable_open = Rc::new(Cell::new(false));
        let unreliable_open = Rc::new(Cell::new(false));
        let reliable_paused = Rc::new(Cell::new(false));
        let inbox: Rc<RefCell<VecDeque<PeerEvent>>> = Rc::new(RefCell::new(VecDeque::new()));

        let mut closures = install_pc_handlers(
            &rtc,
            &state,
            &reliable_open,
            &unreliable_open,
            &inbox,
            /* attach_ondatachannel = */ false,
        );

        // Host 端：主动创建两条 DataChannel
        let reliable_dc = {
            let init = RtcDataChannelInit::new();
            init.set_ordered(true);
            rtc.create_data_channel_with_data_channel_dict("reliable", &init)
        };
        reliable_dc.set_buffered_amount_low_threshold(Self::BUFFERED_LOW_THRESHOLD);

        let unreliable_dc = {
            let init = RtcDataChannelInit::new();
            init.set_ordered(false);
            init.set_max_retransmits(0);
            rtc.create_data_channel_with_data_channel_dict("unreliable", &init)
        };

        // onbufferedamountlow：reliable DC 缓冲区降到阈值以下 → 恢复发送
        {
            let paused_flag = reliable_paused.clone();
            let inbox_clone = inbox.clone();
            let on_buf_low = Closure::<dyn FnMut(JsValue)>::new(move |_evt: JsValue| {
                paused_flag.set(false);
                inbox_clone.borrow_mut().push_back(PeerEvent::BufferFreed);
            });
            reliable_dc.set_onbufferedamountlow(Some(on_buf_low.as_ref().unchecked_ref()));
            closures.push(on_buf_low.into_js_value());
        }

        attach_dc_handlers(
            &reliable_dc,
            ChannelKind::Reliable,
            &reliable_open,
            &state,
            &unreliable_open,
            &inbox,
            &mut closures,
        );
        attach_dc_handlers(
            &unreliable_dc,
            ChannelKind::Unreliable,
            &unreliable_open,
            &state,
            &reliable_open,
            &inbox,
            &mut closures,
        );

        *reliable.borrow_mut() = Some(reliable_dc);
        *unreliable.borrow_mut() = Some(unreliable_dc);

        Ok(Self {
            peer_id,
            rtc,
            state,
            reliable,
            unreliable,
            reliable_open,
            unreliable_open,
            reliable_paused,
            inbox,
            _closures: closures,
        })
    }

    /// Remote 侧：等 Host 通过 SDP 把 DataChannel 推过来（`ondatachannel`）。
    pub fn create_answerer(
        peer_id: u32,
        ice_servers: &[IceServerConfig],
    ) -> Result<Self, NetError> {
        let rtc = make_rtc(ice_servers)?;

        let state = Rc::new(Cell::new(PeerState::New));
        let reliable: Rc<RefCell<Option<RtcDataChannel>>> = Rc::new(RefCell::new(None));
        let unreliable: Rc<RefCell<Option<RtcDataChannel>>> = Rc::new(RefCell::new(None));
        let reliable_open = Rc::new(Cell::new(false));
        let unreliable_open = Rc::new(Cell::new(false));
        let reliable_paused = Rc::new(Cell::new(false));
        let inbox: Rc<RefCell<VecDeque<PeerEvent>>> = Rc::new(RefCell::new(VecDeque::new()));

        let mut closures = install_pc_handlers(
            &rtc,
            &state,
            &reliable_open,
            &unreliable_open,
            &inbox,
            /* attach_ondatachannel = */ true,
        );

        // 把 reliable / unreliable 句柄保存供 ondatachannel 闭包写入
        {
            let reliable_slot = reliable.clone();
            let unreliable_slot = unreliable.clone();
            let reliable_open_clone = reliable_open.clone();
            let unreliable_open_clone = unreliable_open.clone();
            let state_clone = state.clone();
            let inbox_clone = inbox.clone();

            let on_datachannel =
                Closure::<dyn FnMut(RtcDataChannelEvent)>::new(move |evt: RtcDataChannelEvent| {
                    let dc = evt.channel();
                    let label = dc.label();
                    let kind = match label.as_str() {
                        "reliable" => ChannelKind::Reliable,
                        "unreliable" => ChannelKind::Unreliable,
                        other => {
                            log::warn!("[peer] 收到未知 label DataChannel: {other}");
                            return;
                        }
                    };
                    // 注册 DC 事件
                    match kind {
                        ChannelKind::Reliable => {
                            // attach_dc_handlers 内部会保活自身闭包到 vec；
                            // 但 ondatachannel 内 closures 已经移出作用域，无法再 push。
                            // 解决方式：把 reliable DC 直接保存，所属闭包通过 dc.set_on*(...) + forget 自管。
                            attach_dc_handlers_forget(
                                &dc,
                                ChannelKind::Reliable,
                                &reliable_open_clone,
                                &state_clone,
                                &unreliable_open_clone,
                                &inbox_clone,
                            );
                            *reliable_slot.borrow_mut() = Some(dc);
                        }
                        ChannelKind::Unreliable => {
                            attach_dc_handlers_forget(
                                &dc,
                                ChannelKind::Unreliable,
                                &unreliable_open_clone,
                                &state_clone,
                                &reliable_open_clone,
                                &inbox_clone,
                            );
                            *unreliable_slot.borrow_mut() = Some(dc);
                        }
                    }
                });
            rtc.set_ondatachannel(Some(on_datachannel.as_ref().unchecked_ref()));
            closures.push(on_datachannel.into_js_value());
        }

        Ok(Self {
            peer_id,
            rtc,
            state,
            reliable,
            unreliable,
            reliable_open,
            unreliable_open,
            reliable_paused,
            inbox,
            _closures: closures,
        })
    }

    pub fn state(&self) -> PeerState {
        self.state.get()
    }

    pub fn is_open(&self, kind: ChannelKind) -> bool {
        match kind {
            ChannelKind::Reliable => self.reliable_open.get(),
            ChannelKind::Unreliable => self.unreliable_open.get(),
        }
    }

    pub fn is_connected(&self) -> bool {
        self.reliable_open.get() && self.unreliable_open.get()
    }

    /// 拉走累积的事件队列。
    pub fn poll(&self) -> Vec<PeerEvent> {
        self.inbox.borrow_mut().drain(..).collect()
    }

    /// 同步发送字节。DC 未 open / 已关闭 → 静默丢弃（unreliable 语义即可；reliable 由调用方在 open 后再发）。
    ///
    /// Reliable 通道：发送后检查 `bufferedAmount`，超过高水位则设置 `reliable_paused`，
    /// 后续发送将跳过直到 `onbufferedamountlow` 触发。
    pub fn send(&self, channel: ChannelKind, bytes: &[u8]) -> Result<(), NetError> {
        let slot = match channel {
            ChannelKind::Reliable => &self.reliable,
            ChannelKind::Unreliable => &self.unreliable,
        };
        let open = match channel {
            ChannelKind::Reliable => self.reliable_open.get(),
            ChannelKind::Unreliable => self.unreliable_open.get(),
        };
        if !open {
            return Err(NetError::DataChannelClosed);
        }
        let borrowed = slot.borrow();
        let Some(dc) = borrowed.as_ref() else {
            return Err(NetError::DataChannelClosed);
        };
        // Reliable 通道：若已暂停则跳过此次发送
        if channel == ChannelKind::Reliable && self.reliable_paused.get() {
            return Err(NetError::DataChannelClosed);
        }
        dc.send_with_u8_array(bytes)
            .map_err(|_| NetError::DataChannelClosed)?;
        // Reliable 通道：发送后检查 bufferedAmount 是否超过高水位
        if channel == ChannelKind::Reliable {
            let buffered = dc.buffered_amount();
            if buffered >= Self::BUFFERED_HIGH_WATER {
                self.reliable_paused.set(true);
            }
        }
        Ok(())
    }

    /// 查询 Reliable DataChannel 当前缓冲区积压字节数。
    /// DC 未就绪时返回 0。
    pub fn buffered_amount(&self) -> u32 {
        self.reliable
            .borrow()
            .as_ref()
            .map(|dc| dc.buffered_amount())
            .unwrap_or(0)
    }

    /// Reliable 通道是否因流控暂停。
    pub fn is_reliable_paused(&self) -> bool {
        self.reliable_paused.get()
    }

    /// Host 端：发起 createOffer → setLocalDescription → 推 OfferReady。
    pub fn start_offer(&self) {
        let rtc = self.rtc.clone();
        let inbox = self.inbox.clone();
        let state = self.state.clone();
        state.set(PeerState::Negotiating);
        spawn_local(async move {
            match negotiate_offer(&rtc).await {
                Ok(sdp) => inbox.borrow_mut().push_back(PeerEvent::OfferReady(sdp)),
                Err(msg) => inbox
                    .borrow_mut()
                    .push_back(PeerEvent::NegotiationError(msg)),
            }
        });
    }

    /// Remote 端：把 Host 发来的 offer 应用进来 → createAnswer → setLocalDescription → 推 AnswerReady。
    pub fn accept_offer(&self, sdp: String) {
        let rtc = self.rtc.clone();
        let inbox = self.inbox.clone();
        let state = self.state.clone();
        state.set(PeerState::Negotiating);
        spawn_local(async move {
            match negotiate_answer(&rtc, &sdp).await {
                Ok(answer_sdp) => inbox
                    .borrow_mut()
                    .push_back(PeerEvent::AnswerReady(answer_sdp)),
                Err(msg) => inbox
                    .borrow_mut()
                    .push_back(PeerEvent::NegotiationError(msg)),
            }
        });
    }

    /// Host 端：收到 Remote 的 answer 后调用，应用为 remote description。
    pub fn apply_answer(&self, sdp: String) {
        let rtc = self.rtc.clone();
        let inbox = self.inbox.clone();
        spawn_local(async move {
            match set_remote_description(&rtc, RtcSdpType::Answer, &sdp).await {
                Ok(_) => inbox.borrow_mut().push_back(PeerEvent::RemoteDescApplied),
                Err(msg) => inbox
                    .borrow_mut()
                    .push_back(PeerEvent::NegotiationError(msg)),
            }
        });
    }

    /// 应用远端发来的 ICE candidate（JSON 字符串形式，与 LocalIce 中的格式一致）。
    pub fn add_remote_ice(&self, candidate: String) {
        let rtc = self.rtc.clone();
        let inbox = self.inbox.clone();
        spawn_local(async move {
            match add_ice_candidate(&rtc, &candidate).await {
                Ok(_) => inbox.borrow_mut().push_back(PeerEvent::IceApplied),
                Err(msg) => inbox
                    .borrow_mut()
                    .push_back(PeerEvent::NegotiationError(msg)),
            }
        });
    }

    /// 主动断开（PeerConnection.close()）。
    pub fn close(&self) {
        self.rtc.close();
        self.state.set(PeerState::Disconnected);
    }
}

impl Drop for PeerConnection {
    fn drop(&mut self) {
        // 摘事件回调，避免回调引用已释放的 inbox
        self.rtc.set_onicecandidate(None);
        self.rtc.set_oniceconnectionstatechange(None);
        self.rtc.set_onconnectionstatechange(None);
        self.rtc.set_ondatachannel(None);
        if let Some(dc) = self.reliable.borrow().as_ref() {
            dc.set_onopen(None);
            dc.set_onmessage(None);
            dc.set_onerror(None);
            dc.set_onclose(None);
        }
        if let Some(dc) = self.unreliable.borrow().as_ref() {
            dc.set_onopen(None);
            dc.set_onmessage(None);
            dc.set_onerror(None);
            dc.set_onclose(None);
        }
        self.rtc.close();
    }
}

// ──────────────────────────────────────────────────────────────
// 内部辅助
// ──────────────────────────────────────────────────────────────

fn make_rtc(ice_servers: &[IceServerConfig]) -> Result<RtcPeerConnection, NetError> {
    let config = RtcConfiguration::new();
    let arr = Array::new();
    for srv in ice_servers {
        let obj = Object::new();
        let urls = Array::new();
        for u in &srv.urls {
            urls.push(&JsValue::from_str(u));
        }
        let _ = Reflect::set(&obj, &"urls".into(), &urls);
        if let Some(u) = &srv.username {
            let _ = Reflect::set(&obj, &"username".into(), &JsValue::from_str(u));
        }
        if let Some(c) = &srv.credential {
            let _ = Reflect::set(&obj, &"credential".into(), &JsValue::from_str(c));
        }
        arr.push(&obj);
    }
    // 没有任何 IceServer 时给默认 STUN，避免一些浏览器拒绝创建
    if arr.length() == 0 {
        let obj = Object::new();
        let urls = Array::new();
        urls.push(&JsValue::from_str("stun:stun.l.google.com:19302"));
        let _ = Reflect::set(&obj, &"urls".into(), &urls);
        arr.push(&obj);
    }
    config.set_ice_servers(&arr);
    RtcPeerConnection::new_with_configuration(&config).map_err(|_| NetError::PeerConnectionFailed)
}

/// 安装 PeerConnection 通用事件回调（icecandidate / iceconnectionstatechange / connectionstatechange）。
/// 返回保活闭包。Remote 端的 ondatachannel 由调用方自行追加。
fn install_pc_handlers(
    rtc: &RtcPeerConnection,
    state: &Rc<Cell<PeerState>>,
    reliable_open: &Rc<Cell<bool>>,
    unreliable_open: &Rc<Cell<bool>>,
    inbox: &Rc<RefCell<VecDeque<PeerEvent>>>,
    _attach_ondatachannel: bool,
) -> Vec<JsValue> {
    let mut closures: Vec<JsValue> = Vec::new();

    // onicecandidate
    {
        let inbox_clone = inbox.clone();
        let on_ice = Closure::<dyn FnMut(RtcPeerConnectionIceEvent)>::new(
            move |evt: RtcPeerConnectionIceEvent| {
                let Some(candidate) = evt.candidate() else {
                    // null 表示收集完毕（gathering complete）
                    return;
                };
                // 重写 priority,让 IPv6 候选总是排在 IPv4 之前
                let cand_str = bump_ipv6_priority(&candidate.candidate());
                // 序列化为带 sdpMid / sdpMLineIndex 的对象 JSON，便于对端 addIceCandidate
                let obj = Object::new();
                let _ = Reflect::set(&obj, &"candidate".into(), &JsValue::from_str(&cand_str));
                if let Some(mid) = candidate.sdp_mid() {
                    let _ = Reflect::set(&obj, &"sdpMid".into(), &JsValue::from_str(&mid));
                }
                if let Some(idx) = candidate.sdp_m_line_index() {
                    let _ = Reflect::set(
                        &obj,
                        &"sdpMLineIndex".into(),
                        &JsValue::from_f64(idx as f64),
                    );
                }
                let json = js_sys::JSON::stringify(&obj)
                    .ok()
                    .and_then(|s| s.as_string())
                    .unwrap_or_default();
                if !json.is_empty() {
                    inbox_clone
                        .borrow_mut()
                        .push_back(PeerEvent::LocalIce(json));
                }
            },
        );
        rtc.set_onicecandidate(Some(on_ice.as_ref().unchecked_ref()));
        closures.push(on_ice.into_js_value());
    }

    // oniceconnectionstatechange：决定 Connected / Disconnected / Failed
    {
        let rtc_weak = rtc.clone();
        let state_clone = state.clone();
        let inbox_clone = inbox.clone();
        let reliable_open_clone = reliable_open.clone();
        let unreliable_open_clone = unreliable_open.clone();
        let on_ice_state = Closure::<dyn FnMut(JsValue)>::new(move |_evt: JsValue| {
            // 通过字符串读 iceConnectionState，避免 enum 类型在不同浏览器差异
            let raw = Reflect::get(&rtc_weak, &"iceConnectionState".into())
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_default();
            let next = match raw.as_str() {
                "checking" | "new" => PeerState::Negotiating,
                "connected" | "completed" => {
                    if reliable_open_clone.get() && unreliable_open_clone.get() {
                        PeerState::Connected
                    } else {
                        PeerState::Negotiating
                    }
                }
                "disconnected" => PeerState::Disconnected,
                "failed" => PeerState::Failed,
                "closed" => PeerState::Disconnected,
                _ => state_clone.get(),
            };
            if next != state_clone.get() {
                state_clone.set(next);
                inbox_clone
                    .borrow_mut()
                    .push_back(PeerEvent::StateChanged(next));
            }
        });
        rtc.set_oniceconnectionstatechange(Some(on_ice_state.as_ref().unchecked_ref()));
        closures.push(on_ice_state.into_js_value());
    }

    closures
}

/// 给一条 DataChannel 装上 open/message/error/close 监听，闭包通过 closures 数组保活。
fn attach_dc_handlers(
    dc: &RtcDataChannel,
    kind: ChannelKind,
    own_open: &Rc<Cell<bool>>,
    state: &Rc<Cell<PeerState>>,
    other_open: &Rc<Cell<bool>>,
    inbox: &Rc<RefCell<VecDeque<PeerEvent>>>,
    closures: &mut Vec<JsValue>,
) {
    dc.set_binary_type(RtcDataChannelType::Arraybuffer);

    // onopen：标记自己 open；若另一条也 open 则状态切到 Connected。
    {
        let own_open_clone = own_open.clone();
        let other_open_clone = other_open.clone();
        let state_clone = state.clone();
        let inbox_clone = inbox.clone();
        let on_open = Closure::<dyn FnMut(JsValue)>::new(move |_evt: JsValue| {
            own_open_clone.set(true);
            if other_open_clone.get() && state_clone.get() != PeerState::Connected {
                state_clone.set(PeerState::Connected);
                inbox_clone
                    .borrow_mut()
                    .push_back(PeerEvent::StateChanged(PeerState::Connected));
            }
        });
        dc.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        closures.push(on_open.into_js_value());
    }

    // onmessage：把字节推进 inbox。仅处理 ArrayBuffer；其它类型忽略。
    {
        let inbox_clone = inbox.clone();
        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |evt: MessageEvent| {
            let data = evt.data();
            if let Ok(ab) = data.dyn_into::<js_sys::ArrayBuffer>() {
                let u8a = js_sys::Uint8Array::new(&ab);
                let mut buf = vec![0u8; u8a.length() as usize];
                u8a.copy_to(&mut buf);
                inbox_clone.borrow_mut().push_back(PeerEvent::Message {
                    channel: kind,
                    bytes: buf,
                });
            }
        });
        dc.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        closures.push(on_message.into_js_value());
    }

    // onclose
    {
        let own_open_clone = own_open.clone();
        let state_clone = state.clone();
        let inbox_clone = inbox.clone();
        let on_close = Closure::<dyn FnMut(JsValue)>::new(move |_evt: JsValue| {
            own_open_clone.set(false);
            if state_clone.get() == PeerState::Connected {
                state_clone.set(PeerState::Disconnected);
                inbox_clone
                    .borrow_mut()
                    .push_back(PeerEvent::StateChanged(PeerState::Disconnected));
            }
        });
        dc.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        closures.push(on_close.into_js_value());
    }

    // onerror：仅 log，状态由 onclose / iceconnectionstatechange 兜底
    {
        let on_error = Closure::<dyn FnMut(JsValue)>::new(move |evt: JsValue| {
            log::warn!("[peer] DataChannel {kind:?} error: {evt:?}");
        });
        dc.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        closures.push(on_error.into_js_value());
    }
}

/// Remote 端 `ondatachannel` 在闭包内动态 attach DC handlers，
/// 此时主结构体的 `_closures` 已不在作用域内，改用 `Closure::forget`
/// 让闭包随 DC 一起被 JS 引用持有，DC 销毁时 GC。
fn attach_dc_handlers_forget(
    dc: &RtcDataChannel,
    kind: ChannelKind,
    own_open: &Rc<Cell<bool>>,
    state: &Rc<Cell<PeerState>>,
    other_open: &Rc<Cell<bool>>,
    inbox: &Rc<RefCell<VecDeque<PeerEvent>>>,
) {
    dc.set_binary_type(RtcDataChannelType::Arraybuffer);

    {
        let own_open_clone = own_open.clone();
        let other_open_clone = other_open.clone();
        let state_clone = state.clone();
        let inbox_clone = inbox.clone();
        let on_open = Closure::<dyn FnMut(JsValue)>::new(move |_evt: JsValue| {
            own_open_clone.set(true);
            if other_open_clone.get() && state_clone.get() != PeerState::Connected {
                state_clone.set(PeerState::Connected);
                inbox_clone
                    .borrow_mut()
                    .push_back(PeerEvent::StateChanged(PeerState::Connected));
            }
        });
        dc.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        on_open.forget();
    }
    {
        let inbox_clone = inbox.clone();
        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |evt: MessageEvent| {
            let data = evt.data();
            if let Ok(ab) = data.dyn_into::<js_sys::ArrayBuffer>() {
                let u8a = js_sys::Uint8Array::new(&ab);
                let mut buf = vec![0u8; u8a.length() as usize];
                u8a.copy_to(&mut buf);
                inbox_clone.borrow_mut().push_back(PeerEvent::Message {
                    channel: kind,
                    bytes: buf,
                });
            }
        });
        dc.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        on_message.forget();
    }
    {
        let own_open_clone = own_open.clone();
        let state_clone = state.clone();
        let inbox_clone = inbox.clone();
        let on_close = Closure::<dyn FnMut(JsValue)>::new(move |_evt: JsValue| {
            own_open_clone.set(false);
            if state_clone.get() == PeerState::Connected {
                state_clone.set(PeerState::Disconnected);
                inbox_clone
                    .borrow_mut()
                    .push_back(PeerEvent::StateChanged(PeerState::Disconnected));
            }
        });
        dc.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        on_close.forget();
    }
    {
        let on_error = Closure::<dyn FnMut(JsValue)>::new(move |evt: JsValue| {
            log::warn!("[peer] DataChannel {kind:?} error: {evt:?}");
        });
        dc.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        on_error.forget();
    }
}

async fn negotiate_offer(rtc: &RtcPeerConnection) -> Result<String, String> {
    let offer = JsFuture::from(rtc.create_offer())
        .await
        .map_err(|e| format!("createOffer failed: {e:?}"))?;
    let sdp = Reflect::get(&offer, &"sdp".into())
        .ok()
        .and_then(|v| v.as_string())
        .ok_or_else(|| "createOffer returned no sdp".to_string())?;
    let init = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
    init.set_sdp(&sdp);
    JsFuture::from(rtc.set_local_description(&init))
        .await
        .map_err(|e| format!("setLocalDescription(offer) failed: {e:?}"))?;
    Ok(sdp)
}

async fn negotiate_answer(rtc: &RtcPeerConnection, offer_sdp: &str) -> Result<String, String> {
    set_remote_description(rtc, RtcSdpType::Offer, offer_sdp).await?;
    let answer = JsFuture::from(rtc.create_answer())
        .await
        .map_err(|e| format!("createAnswer failed: {e:?}"))?;
    let sdp = Reflect::get(&answer, &"sdp".into())
        .ok()
        .and_then(|v| v.as_string())
        .ok_or_else(|| "createAnswer returned no sdp".to_string())?;
    let init = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
    init.set_sdp(&sdp);
    JsFuture::from(rtc.set_local_description(&init))
        .await
        .map_err(|e| format!("setLocalDescription(answer) failed: {e:?}"))?;
    Ok(sdp)
}

async fn set_remote_description(
    rtc: &RtcPeerConnection,
    sdp_type: RtcSdpType,
    sdp: &str,
) -> Result<(), String> {
    let init = RtcSessionDescriptionInit::new(sdp_type);
    init.set_sdp(sdp);
    JsFuture::from(rtc.set_remote_description(&init))
        .await
        .map_err(|e| format!("setRemoteDescription failed: {e:?}"))?;
    Ok(())
}

async fn add_ice_candidate(rtc: &RtcPeerConnection, candidate_json: &str) -> Result<(), String> {
    // 反序列化 JSON → { candidate, sdpMid, sdpMLineIndex }
    let value: serde_json::Value =
        serde_json::from_str(candidate_json).map_err(|e| format!("ice candidate json: {e}"))?;
    let cand_str = value
        .get("candidate")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "ice candidate missing .candidate".to_string())?;
    let init = RtcIceCandidateInit::new(cand_str);
    if let Some(mid) = value.get("sdpMid").and_then(|v| v.as_str()) {
        init.set_sdp_mid(Some(mid));
    }
    if let Some(idx) = value.get("sdpMLineIndex").and_then(|v| v.as_u64()) {
        init.set_sdp_m_line_index(Some(idx as u16));
    }
    let cand = RtcIceCandidate::new(&init).map_err(|e| format!("ice candidate new: {e:?}"))?;
    let promise = rtc.add_ice_candidate_with_opt_rtc_ice_candidate(Some(&cand));
    JsFuture::from(promise)
        .await
        .map_err(|e| format!("addIceCandidate failed: {e:?}"))?;
    Ok(())
}

/// IPv4 candidate 的 priority 下调量(单位:priority 数值)。
///
/// 当本机同时有 IPv4 和 IPv6 网络时,ICE candidate pair 的连通性检查顺序按 priority 排。
/// 浏览器给出的默认 priority 让 IPv4 host (~2.12B) 排在 IPv6 srflx (~1.68B) 之前,
/// 但 IPv4 host 通常是内网地址(192.168.x / 10.x),跨公网注定连不通,白白消耗检查窗口。
/// 把所有 IPv4 candidate 的 priority 减去 `IPV4_PRIORITY_PENALTY`(约 1.07B)后:
/// - IPv4 host 降到 ~1.05B,低于 IPv6 srflx;
/// - 同族内部 host > srflx > relay 的相对顺序保留;
/// - relay 这种 priority 本来就低的会饱和到 0。
///
/// 对端拿到压低后的 IPv4 priority,在做 pair 配对时 IPv6 综合优先级就更高,
/// 浏览器会先把检查带宽给 IPv6,IPv6 全失败才轮到 IPv4。
const IPV4_PRIORITY_PENALTY: u32 = 0x4000_0000;

/// 重写 ICE candidate 字符串的 priority 字段,让 IPv6 候选总是排在 IPv4 之前。
///
/// 输入格式(WebRTC `RTCIceCandidate.candidate` 返回值):
/// `candidate:<foundation> <component> <transport> <priority> <address> <port> typ <type> ...`
///
/// 规则:
/// - 第 5 段(index 4)是地址,含 `:` 判为 IPv6,含 `.` 判为 IPv4;
/// - IPv6 不动;IPv4 priority 减 [`IPV4_PRIORITY_PENALTY`](饱和到 0);
/// - 非 IPv4 / 非 IPv6 字面量(比如 mDNS `.local` 主机名)原样返回;
/// - 字段数不足或 priority 解析失败也原样返回(兜底)。
fn bump_ipv6_priority(candidate: &str) -> String {
    let fields: Vec<&str> = candidate.split(' ').collect();
    if fields.len() < 6 {
        return candidate.to_string();
    }
    // 用真正的 IP 解析判定地址族,避免把 mDNS `.local` 主机名误判为 IPv4
    let address = fields[4];
    let is_ipv4 = address.parse::<std::net::Ipv4Addr>().is_ok();
    if !is_ipv4 {
        // IPv6 / mDNS hostname / 其他形式一律不动
        return candidate.to_string();
    }
    let Ok(prio) = fields[3].parse::<u32>() else {
        return candidate.to_string();
    };
    let new_prio = prio.saturating_sub(IPV4_PRIORITY_PENALTY).to_string();
    let mut new_fields = fields.clone();
    new_fields[3] = &new_prio;
    new_fields.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_host_priority_reduced() {
        let input = "candidate:2999745851 1 udp 2122260223 192.168.1.5 60169 typ host generation 0";
        let expected_prio = 2122260223u32 - 0x4000_0000;
        let expected = format!(
            "candidate:2999745851 1 udp {expected_prio} 192.168.1.5 60169 typ host generation 0"
        );
        assert_eq!(bump_ipv6_priority(input), expected);
    }

    #[test]
    fn ipv6_host_priority_unchanged() {
        let input = "candidate:840527380 1 udp 2122197247 2001:db8::1 60170 typ host generation 0";
        assert_eq!(bump_ipv6_priority(input), input);
    }

    #[test]
    fn ipv4_srflx_priority_reduced() {
        let input = "candidate:842163049 1 udp 1685987071 203.0.113.1 60169 typ srflx raddr 192.168.1.5 rport 60169";
        let expected_prio = 1685987071u32 - 0x4000_0000;
        let expected = format!(
            "candidate:842163049 1 udp {expected_prio} 203.0.113.1 60169 typ srflx raddr 192.168.1.5 rport 60169"
        );
        assert_eq!(bump_ipv6_priority(input), expected);
    }

    #[test]
    fn ipv4_low_priority_saturates_to_zero() {
        // relay 候选,priority 远低于 penalty
        let input = "candidate:1 1 udp 100 198.51.100.1 60169 typ relay raddr 0.0.0.0 rport 0";
        let expected = "candidate:1 1 udp 0 198.51.100.1 60169 typ relay raddr 0.0.0.0 rport 0";
        assert_eq!(bump_ipv6_priority(input), expected);
    }

    #[test]
    fn malformed_candidate_returned_unchanged() {
        let input = "not-a-candidate";
        assert_eq!(bump_ipv6_priority(input), input);
    }

    #[test]
    fn mdns_hostname_unchanged() {
        // 现代浏览器会用 .local mDNS 地址替代真实 IP,既非 IPv4 也非 IPv6 字面量
        let input = "candidate:1 1 udp 2122260223 abc-def.local 60169 typ host generation 0";
        assert_eq!(bump_ipv6_priority(input), input);
    }

    #[test]
    fn ipv6_srflx_unchanged() {
        let input =
            "candidate:2 1 udp 1685987071 2001:db8::abcd 60170 typ srflx raddr ::1 rport 60170";
        assert_eq!(bump_ipv6_priority(input), input);
    }
}
