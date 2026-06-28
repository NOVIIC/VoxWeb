use std::collections::HashMap;

use voxweb_core::protocol::{EntityId, OutboundMessage, Recipient};

const DEFAULT_RELAY_MAX_RATE: u32 = 200;
const RELAY_CLIENT_RATE_HEADROOM: f64 = 0.8;

/// outbox 路由的执行计划：哪些 peer 走 DC，是否还要送给本地 Host。
/// 纯数据结构便于单元测试，避免触碰 PeerConnection / mpsc。
#[derive(Debug, PartialEq)]
pub struct RoutingPlan {
    pub peers_to_send: Vec<u32>,
    pub send_to_local: bool,
}

/// 给定一条 OutboundMessage 与当前的 peer→entity 映射 + Host 自身 eid，
/// 计算该消息应该发往哪些 peer 以及是否要回流到本地 Host。
///
/// 提取为独立纯函数以便单元测试；`host_route_outbox` 是它的 IO 包装。
pub fn plan_route(
    msg: &OutboundMessage,
    peer_to_entity: &HashMap<u32, EntityId>,
    host_self: Option<EntityId>,
) -> RoutingPlan {
    match msg.recipient {
        Recipient::All => RoutingPlan {
            peers_to_send: peer_to_entity.keys().copied().collect(),
            send_to_local: host_self.is_some(),
        },
        Recipient::Except(excluded) => {
            let peers_to_send = peer_to_entity
                .iter()
                .filter(|(_, eid)| **eid != excluded)
                .map(|(pid, _)| *pid)
                .collect();
            let send_to_local = match host_self {
                Some(self_eid) => self_eid != excluded,
                None => false,
            };
            RoutingPlan {
                peers_to_send,
                send_to_local,
            }
        }
        Recipient::One(target) => {
            if host_self == Some(target) {
                RoutingPlan {
                    peers_to_send: vec![],
                    send_to_local: true,
                }
            } else {
                let peer = peer_to_entity
                    .iter()
                    .find(|(_, eid)| **eid == target)
                    .map(|(pid, _)| *pid);
                RoutingPlan {
                    peers_to_send: peer.into_iter().collect(),
                    send_to_local: false,
                }
            }
        }
    }
}

/// Worker 中继按"消息条数"做令牌桶限流。Host 高视距会在一帧内产生上百个
/// FieldSnapshot；本地先按服务端下发的 `max_rate` 留出余量发送，避免触发
/// `relay_closed{reason:"rate_limit"}`。
#[derive(Clone, Debug)]
pub struct RelayRateLimiter {
    capacity: f64,
    refill_per_ms: f64,
    tokens: f64,
    last_refill_ms: f64,
}

impl RelayRateLimiter {
    pub(super) fn new(max_rate: u32, now_ms: f64) -> Self {
        let safe_rate = ((max_rate.max(1) as f64) * RELAY_CLIENT_RATE_HEADROOM).max(1.0);
        Self {
            capacity: safe_rate,
            refill_per_ms: safe_rate / 1000.0,
            tokens: safe_rate,
            last_refill_ms: now_ms,
        }
    }

    pub(super) fn default(now_ms: f64) -> Self {
        Self::new(DEFAULT_RELAY_MAX_RATE, now_ms)
    }

    pub(super) fn has_token(&mut self, now_ms: f64) -> bool {
        self.refill(now_ms);
        self.tokens >= 1.0
    }

    pub(super) fn consume_token(&mut self, now_ms: f64) {
        self.refill(now_ms);
        self.tokens = (self.tokens - 1.0).max(0.0);
    }

    fn refill(&mut self, now_ms: f64) {
        let elapsed = now_ms - self.last_refill_ms;
        if elapsed <= 0.0 {
            return;
        }
        self.tokens = (self.tokens + elapsed * self.refill_per_ms).min(self.capacity);
        self.last_refill_ms = now_ms;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use voxweb_core::protocol::{AckReason, ServerMessage};

    /// 构造一个 OutboundMessage（recipient 任意；payload 固定）。
    fn outbound(recipient: Recipient) -> OutboundMessage {
        OutboundMessage {
            recipient,
            message: ServerMessage::ActionAck {
                request_id: 0,
                accepted: true,
                reason: AckReason::Ok,
            },
        }
    }

    fn mapping(pairs: &[(u32, EntityId)]) -> HashMap<u32, EntityId> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn route_all_sends_to_all_peers_and_local() {
        let map = mapping(&[(101, 2), (102, 3)]);
        let plan = plan_route(&outbound(Recipient::All), &map, Some(1));
        assert!(plan.send_to_local);
        let mut got = plan.peers_to_send.clone();
        got.sort();
        assert_eq!(got, vec![101, 102]);
    }

    #[test]
    fn route_all_without_host_self_skips_local() {
        let map = mapping(&[(101, 2)]);
        let plan = plan_route(&outbound(Recipient::All), &map, None);
        assert!(!plan.send_to_local);
        assert_eq!(plan.peers_to_send, vec![101]);
    }

    #[test]
    fn route_except_skips_target_entity_on_peers_and_local() {
        let map = mapping(&[(101, 2), (102, 3)]);
        // 排除 eid=2 → 应该跳过 peer 101，但 host_self=1 仍要收
        let plan = plan_route(&outbound(Recipient::Except(2)), &map, Some(1));
        assert_eq!(plan.peers_to_send, vec![102]);
        assert!(plan.send_to_local);

        // 排除 host_self（eid=1）→ 应该跳过 local，但所有 peer 仍要收
        let plan2 = plan_route(&outbound(Recipient::Except(1)), &map, Some(1));
        let mut got = plan2.peers_to_send.clone();
        got.sort();
        assert_eq!(got, vec![101, 102]);
        assert!(!plan2.send_to_local);
    }

    #[test]
    fn route_one_to_host_self_only_goes_local() {
        let map = mapping(&[(101, 2), (102, 3)]);
        let plan = plan_route(&outbound(Recipient::One(1)), &map, Some(1));
        assert!(plan.send_to_local);
        assert!(plan.peers_to_send.is_empty());
    }

    #[test]
    fn route_one_to_remote_peer_goes_single_peer() {
        let map = mapping(&[(101, 2), (102, 3)]);
        let plan = plan_route(&outbound(Recipient::One(3)), &map, Some(1));
        assert!(!plan.send_to_local);
        assert_eq!(plan.peers_to_send, vec![102]);
    }

    #[test]
    fn route_one_to_unknown_entity_routes_nothing() {
        let map = mapping(&[(101, 2)]);
        let plan = plan_route(&outbound(Recipient::One(999)), &map, Some(1));
        assert!(!plan.send_to_local);
        assert!(plan.peers_to_send.is_empty());
    }

    #[test]
    fn relay_rate_limiter_keeps_headroom_under_worker_cap() {
        let mut limiter = RelayRateLimiter::new(200, 0.0);

        for _ in 0..160 {
            assert!(limiter.has_token(0.0));
            limiter.consume_token(0.0);
        }
        assert!(!limiter.has_token(0.0));

        // 80% 余量下补充速率为 160/s；6ms 不足 1 条，7ms 可以再发 1 条。
        assert!(!limiter.has_token(6.0));
        assert!(limiter.has_token(7.0));
    }

    #[test]
    fn relay_rate_limiter_small_rate_still_progresses() {
        let mut limiter = RelayRateLimiter::new(1, 0.0);

        assert!(limiter.has_token(0.0));
        limiter.consume_token(0.0);
        assert!(!limiter.has_token(999.0));
        assert!(limiter.has_token(1000.0));
    }
}
