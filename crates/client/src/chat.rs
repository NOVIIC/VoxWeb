//! 聊天历史数据模型（Phase 6）。
//!
//! 持有最近 [`ChatHistory::cap`] 条消息，超出则 FIFO 驱逐。
//! UI 层（[`crate::ui::chat`]）只读取 / 提交消息文本，写入操作由
//! [`crate::lib::apply_server_message`] 与 PeerJoined / PeerLeft 处理路径调用。
//!
//! 设计取舍：
//! - 不放在 `ui::chat` 是因为数据模型独立于渲染；测试也无需 egui 上下文。
//! - `received_at_ms` 由调用方在 push 时注入（performance.now()），便于单测控时间。
//! - 系统消息（`ChatKind::System`）由客户端合成，不走协议；服务端只发 `PeerJoined` / `PeerLeft`。

use std::collections::VecDeque;

use voxweb_core::protocol::EntityId;

/// 聊天历史最大保留条数（[`docs/features/ui.md`] §十二 性能基线）。
pub const CHAT_HISTORY_CAP: usize = 100;

/// 聊天消息类型。User 携带发送者身份；System 用于"X 加入了房间"等本地合成提示。
#[derive(Clone, Debug, PartialEq)]
pub enum ChatKind {
    User {
        from_eid: EntityId,
        from_name: String,
    },
    System,
}

/// 一条聊天历史。
#[derive(Clone, Debug, PartialEq)]
pub struct ChatMessage {
    pub kind: ChatKind,
    pub content: String,
    /// 接收时刻（performance.now() 毫秒）。浮窗淡出窗口判定依此。
    pub received_at_ms: f64,
}

/// 聊天历史 + 当前输入缓冲。
#[derive(Clone, Debug)]
pub struct ChatHistory {
    history: VecDeque<ChatMessage>,
    cap: usize,
    /// 当前正在编辑的输入文本（聊天框打开时由 UI 修改；提交后清空）。
    pub input_buffer: String,
}

impl Default for ChatHistory {
    fn default() -> Self {
        Self::with_cap(CHAT_HISTORY_CAP)
    }
}

impl ChatHistory {
    pub fn with_cap(cap: usize) -> Self {
        Self {
            history: VecDeque::with_capacity(cap.min(256)),
            cap,
            input_buffer: String::new(),
        }
    }

    /// 推入一条用户消息。
    pub fn push_user(
        &mut self,
        from_eid: EntityId,
        from_name: String,
        content: String,
        now_ms: f64,
    ) {
        self.push(ChatMessage {
            kind: ChatKind::User {
                from_eid,
                from_name,
            },
            content,
            received_at_ms: now_ms,
        });
    }

    /// 推入一条系统消息（本地合成，不上协议）。
    pub fn push_system(&mut self, content: String, now_ms: f64) {
        self.push(ChatMessage {
            kind: ChatKind::System,
            content,
            received_at_ms: now_ms,
        });
    }

    fn push(&mut self, msg: ChatMessage) {
        if self.history.len() >= self.cap {
            self.history.pop_front();
        }
        self.history.push_back(msg);
    }

    /// 最近 n 条消息，按时间从旧到新排列。
    pub fn recent(&self, n: usize) -> impl Iterator<Item = &ChatMessage> + '_ {
        let start = self.history.len().saturating_sub(n);
        self.history.iter().skip(start)
    }

    /// 最近 n 条且 `received_at_ms > now_ms - window_ms` 的消息（用于 5 秒淡出浮窗）。
    pub fn recent_within(&self, now_ms: f64, window_ms: f64, n: usize) -> Vec<&ChatMessage> {
        let cutoff = now_ms - window_ms;
        self.recent(n)
            .filter(|m| m.received_at_ms > cutoff)
            .collect()
    }

    pub fn len(&self) -> usize {
        self.history.len()
    }

    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_user_and_system_are_distinguishable() {
        let mut h = ChatHistory::default();
        h.push_user(1, "Alice".into(), "hi".into(), 100.0);
        h.push_system("Bob 加入了房间".into(), 200.0);
        assert_eq!(h.len(), 2);
        let msgs: Vec<_> = h.recent(10).collect();
        assert!(matches!(msgs[0].kind, ChatKind::User { from_eid: 1, .. }));
        assert!(matches!(msgs[1].kind, ChatKind::System));
        assert_eq!(msgs[0].content, "hi");
        assert_eq!(msgs[1].content, "Bob 加入了房间");
    }

    #[test]
    fn cap_evicts_oldest_in_fifo_order() {
        let mut h = ChatHistory::with_cap(3);
        h.push_system("a".into(), 0.0);
        h.push_system("b".into(), 1.0);
        h.push_system("c".into(), 2.0);
        h.push_system("d".into(), 3.0);
        let msgs: Vec<_> = h.recent(10).collect();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].content, "b");
        assert_eq!(msgs[1].content, "c");
        assert_eq!(msgs[2].content, "d");
    }

    #[test]
    fn recent_returns_oldest_to_newest_subset() {
        let mut h = ChatHistory::default();
        for i in 0..10 {
            h.push_system(format!("m{i}"), i as f64);
        }
        let last3: Vec<_> = h.recent(3).map(|m| m.content.as_str()).collect();
        assert_eq!(last3, vec!["m7", "m8", "m9"]);
    }

    #[test]
    fn recent_within_filters_by_received_at_ms() {
        let mut h = ChatHistory::default();
        h.push_system("old".into(), 0.0);
        h.push_system("recent".into(), 4000.0);
        h.push_system("now".into(), 5500.0);

        // 当前时间 5500，5s 窗口 → cutoff = 500，留下 "recent" 与 "now"
        let kept = h.recent_within(5500.0, 5000.0, 10);
        let texts: Vec<_> = kept.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(texts, vec!["recent", "now"]);
    }

    #[test]
    fn recent_within_caps_at_n() {
        let mut h = ChatHistory::default();
        for i in 0..10 {
            h.push_system(format!("m{i}"), 1000.0 + i as f64);
        }
        // 全在 5s 窗口内，但只取最后 5 条
        let kept = h.recent_within(2000.0, 5000.0, 5);
        let texts: Vec<_> = kept.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(texts, vec!["m5", "m6", "m7", "m8", "m9"]);
    }
}
