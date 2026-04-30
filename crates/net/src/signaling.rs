//! WebSocket 信令客户端。
//! 连接到 Cloudflare Workers 信令服务，收发 Offer/Answer SDP 和 ICE Candidate。
//!
//! Phase 4 实现。

/// 信令客户端占位（Phase 4 实现）。
pub struct SignalingClient {
    // Phase 4: ws URL、websocket、回调闭包
}

impl SignalingClient {
    pub fn new(_signaling_url: &str) -> Self {
        Self {}
    }
}
