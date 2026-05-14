//! 房间会话状态机。
//!
//! 用作 [`crate::NetEndpoint::Host`] / [`crate::NetEndpoint::Remote`] 当前阶段的"标签"，
//! 主要供 UI 显示进度（"Connecting…"、"Waiting for host…"）。
//! 真正的协商驱动逻辑在 [`crate::NetEndpoint::poll`] 中。

/// 加载步骤状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepStatus {
    /// 尚未开始。
    Pending,
    /// 正在进行中。
    InProgress,
    /// 已完成。
    Done,
}

/// 一条加载步骤（供 UI 列表渲染）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadingStep {
    pub label: String,
    pub status: StepStatus,
}

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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RoomSession {
    /// 大厅：尚未启动连接。
    #[default]
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

    /// 返回网络连接相关的加载步骤列表（信令 → 注册 → offer → answer → DC）。
    /// 区块预载步骤由客户端追加，不在此列。
    pub fn loading_steps(&self) -> Vec<LoadingStep> {
        let steps: &[&str] = &[
            "Connecting to signaling server…",
            "Registering with room…",
            "Exchanging offer…",
            "Exchanging answer…",
            "Establishing data channel…",
        ];

        match self {
            RoomSession::Idle => steps
                .iter()
                .map(|&label| LoadingStep {
                    label: label.to_string(),
                    status: StepStatus::Pending,
                })
                .collect(),

            RoomSession::SignalingConnect => steps
                .iter()
                .enumerate()
                .map(|(i, &label)| LoadingStep {
                    label: label.to_string(),
                    status: if i == 0 {
                        StepStatus::InProgress
                    } else {
                        StepStatus::Pending
                    },
                })
                .collect(),

            RoomSession::AwaitRegistered => steps
                .iter()
                .enumerate()
                .map(|(i, &label)| LoadingStep {
                    label: label.to_string(),
                    status: if i == 0 {
                        StepStatus::Done
                    } else if i == 1 {
                        StepStatus::InProgress
                    } else {
                        StepStatus::Pending
                    },
                })
                .collect(),

            RoomSession::Negotiating(p) => {
                let offer_done = p.offer_exchanged || p.answer_exchanged || p.data_channel_opened;
                let answer_done = p.answer_exchanged || p.data_channel_opened;
                let dc_done = p.data_channel_opened;
                let in_progress_idx = if !p.offer_exchanged {
                    2
                } else if !p.answer_exchanged {
                    3
                } else if !p.data_channel_opened {
                    4
                } else {
                    5 // 全部完成，无 InProgress
                };

                steps
                    .iter()
                    .enumerate()
                    .map(|(i, &label)| {
                        let status = match i {
                            0 | 1 => StepStatus::Done,
                            2 => {
                                if offer_done {
                                    StepStatus::Done
                                } else if i == in_progress_idx {
                                    StepStatus::InProgress
                                } else {
                                    StepStatus::Pending
                                }
                            }
                            3 => {
                                if answer_done {
                                    StepStatus::Done
                                } else if i == in_progress_idx {
                                    StepStatus::InProgress
                                } else {
                                    StepStatus::Pending
                                }
                            }
                            4 => {
                                if dc_done {
                                    StepStatus::Done
                                } else if i == in_progress_idx {
                                    StepStatus::InProgress
                                } else {
                                    StepStatus::Pending
                                }
                            }
                            _ => StepStatus::Pending,
                        };
                        LoadingStep {
                            label: label.to_string(),
                            status,
                        }
                    })
                    .collect()
            }

            RoomSession::Connected => steps
                .iter()
                .map(|&label| LoadingStep {
                    label: label.to_string(),
                    status: StepStatus::Done,
                })
                .collect(),

            RoomSession::Disconnected { .. } => steps
                .iter()
                .map(|&label| LoadingStep {
                    label: label.to_string(),
                    status: StepStatus::Done,
                })
                .collect(),
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
    fn loading_steps_idle_all_pending() {
        let s = RoomSession::Idle;
        let steps = s.loading_steps();
        assert_eq!(steps.len(), 5);
        assert!(steps.iter().all(|s| s.status == StepStatus::Pending));
    }

    #[test]
    fn loading_steps_signaling_connect() {
        let s = RoomSession::SignalingConnect;
        let steps = s.loading_steps();
        assert_eq!(steps[0].status, StepStatus::InProgress);
        assert_eq!(steps[1].status, StepStatus::Pending);
    }

    #[test]
    fn loading_steps_await_registered() {
        let s = RoomSession::AwaitRegistered;
        let steps = s.loading_steps();
        assert_eq!(steps[0].status, StepStatus::Done);
        assert_eq!(steps[1].status, StepStatus::InProgress);
    }

    #[test]
    fn loading_steps_negotiating_offer() {
        let s = RoomSession::Negotiating(NegotiationProgress::default());
        let steps = s.loading_steps();
        // 0: 信令 Done, 1: 注册 Done, 2: offer InProgress, 3-4: Pending
        assert_eq!(steps[0].status, StepStatus::Done);
        assert_eq!(steps[1].status, StepStatus::Done);
        assert_eq!(steps[2].status, StepStatus::InProgress);
        assert_eq!(steps[3].status, StepStatus::Pending);
        assert_eq!(steps[4].status, StepStatus::Pending);
    }

    #[test]
    fn loading_steps_negotiating_answer() {
        let mut s = RoomSession::Negotiating(NegotiationProgress::default());
        s.mark_offer_exchanged();
        let steps = s.loading_steps();
        assert_eq!(steps[2].status, StepStatus::Done);
        assert_eq!(steps[3].status, StepStatus::InProgress);
        assert_eq!(steps[4].status, StepStatus::Pending);
    }

    #[test]
    fn loading_steps_negotiating_dc() {
        let mut s = RoomSession::Negotiating(NegotiationProgress::default());
        s.mark_offer_exchanged();
        s.mark_answer_exchanged();
        let steps = s.loading_steps();
        assert_eq!(steps[2].status, StepStatus::Done);
        assert_eq!(steps[3].status, StepStatus::Done);
        assert_eq!(steps[4].status, StepStatus::InProgress);
    }

    #[test]
    fn loading_steps_all_done() {
        let mut s = RoomSession::Negotiating(NegotiationProgress::default());
        s.mark_offer_exchanged();
        s.mark_answer_exchanged();
        s.mark_dc_open();
        let steps = s.loading_steps();
        assert!(steps.iter().all(|s| s.status == StepStatus::Done));
    }

    #[test]
    fn loading_steps_connected() {
        let s = RoomSession::Connected;
        let steps = s.loading_steps();
        assert_eq!(steps.len(), 5);
        assert!(steps.iter().all(|s| s.status == StepStatus::Done));
    }
}
