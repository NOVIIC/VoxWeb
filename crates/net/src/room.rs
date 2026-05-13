//! 房间会话状态机。
//!
//! 用作 [`crate::NetEndpoint::Host`] / [`crate::NetEndpoint::Remote`] 当前阶段的"标签"，
//! 主要供 UI 显示进度（"Connecting…"、"Waiting for host…"）。
//! 真正的协商驱动逻辑在 [`crate::NetEndpoint::poll`] 中。

/// 协商各子步骤是否完成（用于 UI 显示更精细的进度条）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NegotiationProgress {
    /// 信令 WebSocket 已 open。
    pub signaling_ok: bool,
    /// 服务端已下发 registered（拿到 peer_id）。
    pub registered: bool,
    /// Host 端：offer 已发出；Remote 端：offer 已收到。
    pub offer_exchanged: bool,
    /// Host 端：answer 已收到；Remote 端：answer 已发出。
    pub answer_exchanged: bool,
    /// 至少一条 DataChannel 已 open。
    pub data_channel_opened: bool,
}

/// 房间会话状态机。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoomSession {
    /// 大厅：尚未启动连接。
    Idle,
    /// 正在连信令 WebSocket。
    SignalingConnect,
    /// 已 connect，等服务端 `registered` 应答。
    AwaitRegistered,
    /// 协商中（SDP / ICE 交换）。
    Negotiating(NegotiationProgress),
    /// 已建连，DC open。
    Connected,
    /// 断开（reason 描述原因，用于 UI 提示）。
    Disconnected { reason: String },
}

impl Default for RoomSession {
    fn default() -> Self {
        RoomSession::Idle
    }
}

impl RoomSession {
    /// 进入 Negotiating 状态时调用，从 AwaitRegistered 平滑过渡。
    pub fn enter_negotiating(&mut self) {
        if !matches!(self, RoomSession::Negotiating(_)) {
            *self = RoomSession::Negotiating(NegotiationProgress {
                signaling_ok: true,
                registered: true,
                ..Default::default()
            });
        }
    }

    pub fn mark_offer_exchanged(&mut self) {
        if let RoomSession::Negotiating(p) = self {
            p.offer_exchanged = true;
        }
    }

    pub fn mark_answer_exchanged(&mut self) {
        if let RoomSession::Negotiating(p) = self {
            p.answer_exchanged = true;
        }
    }

    pub fn mark_dc_open(&mut self) {
        if let RoomSession::Negotiating(p) = self {
            p.data_channel_opened = true;
        }
    }

    /// 一句话进度描述（给 UI 用）。
    pub fn progress_label(&self) -> &'static str {
        match self {
            RoomSession::Idle => "Idle",
            RoomSession::SignalingConnect => "Connecting to signaling server…",
            RoomSession::AwaitRegistered => "Registering with room…",
            RoomSession::Negotiating(p) => {
                if !p.offer_exchanged {
                    "Exchanging offer…"
                } else if !p.answer_exchanged {
                    "Exchanging answer…"
                } else if !p.data_channel_opened {
                    "Establishing data channel…"
                } else {
                    "Almost there…"
                }
            }
            RoomSession::Connected => "Connected",
            RoomSession::Disconnected { .. } => "Disconnected",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_transitions() {
        let mut s = RoomSession::AwaitRegistered;
        s.enter_negotiating();
        assert!(matches!(s, RoomSession::Negotiating(_)));
        s.mark_offer_exchanged();
        if let RoomSession::Negotiating(p) = &s {
            assert!(p.offer_exchanged);
            assert!(!p.answer_exchanged);
        }
        s.mark_answer_exchanged();
        s.mark_dc_open();
        if let RoomSession::Negotiating(p) = &s {
            assert!(p.answer_exchanged && p.data_channel_opened);
        }
    }

    #[test]
    fn progress_labels_distinct() {
        let mut s = RoomSession::Negotiating(NegotiationProgress::default());
        assert_eq!(s.progress_label(), "Exchanging offer…");
        s.mark_offer_exchanged();
        assert_eq!(s.progress_label(), "Exchanging answer…");
        s.mark_answer_exchanged();
        assert_eq!(s.progress_label(), "Establishing data channel…");
        s.mark_dc_open();
        assert_eq!(s.progress_label(), "Almost there…");
    }
}
