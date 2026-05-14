//! 远端玩家位置插值。
//!
//! 每个远端玩家维护一个带时间戳的快照缓冲区；
//! 每渲染帧以 `server_time_ms - interp_delay` 作为渲染 target，
//! 找出 bracket `[a.time, b.time]` 后 lerp position / yaw / pitch。
//!
//! Phase 5 不做外推（若 render_time 超出最新快照直接返回最新）；
//! Phase 7 可加 50ms 外推窗口。

use std::collections::HashMap;

use glam::Vec3;

use voxweb_core::protocol::EntityId;

/// 单条远端玩家快照。由 `apply_server_message::PlayerTick` 推入。
#[derive(Copy, Clone, Debug)]
struct Sample {
    server_time_ms: u64,
    position: Vec3,
    yaw: f32,
    pitch: f32,
}

/// 一个远端玩家实体的快照缓冲区。
#[derive(Debug)]
struct RemoteBuffer {
    buf: Vec<Sample>,
    /// 最多保留多少条（防止内存膨胀）。默认 20（≈ 333ms @60Hz）。
    cap: usize,
}

impl RemoteBuffer {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            cap: 20,
        }
    }

    fn push(&mut self, s: Sample) {
        // 保持按 server_time_ms 升序（防重排 / 乱序到达）
        let pos = self
            .buf
            .binary_search_by_key(&s.server_time_ms, |e| e.server_time_ms);
        match pos {
            Ok(i) => self.buf[i] = s, // 重复时间戳覆盖
            Err(i) => self.buf.insert(i, s),
        }
        while self.buf.len() > self.cap {
            self.buf.remove(0);
        }
    }

    /// 给定渲染 target（server 时间 ms），返回插值后的 (position, yaw, pitch)。
    /// - target 早于最早样本 → 返回最早
    /// - target 晚于最新样本 → 返回最新（不外推）
    /// - 在 [a, b] 之间 → lerp
    fn get(&self, render_server_time_ms: f64) -> Option<(Vec3, f32, f32)> {
        if self.buf.is_empty() {
            return None;
        }
        if self.buf.len() == 1 {
            let s = &self.buf[0];
            return Some((s.position, s.yaw, s.pitch));
        }

        // 找到第一条 server_time_ms >= target
        let idx = self
            .buf
            .partition_point(|s| (s.server_time_ms as f64) < render_server_time_ms);

        if idx == 0 {
            let s = &self.buf[0];
            Some((s.position, s.yaw, s.pitch))
        } else if idx >= self.buf.len() {
            let s = &self.buf[self.buf.len() - 1];
            Some((s.position, s.yaw, s.pitch))
        } else {
            let a = &self.buf[idx - 1];
            let b = &self.buf[idx];
            let denom = (b.server_time_ms - a.server_time_ms) as f64;
            if denom <= 0.0 {
                return Some((a.position, a.yaw, a.pitch));
            }
            let t =
                ((render_server_time_ms - a.server_time_ms as f64) / denom).clamp(0.0, 1.0) as f32;
            let position = a.position.lerp(b.position, t);
            let pitch = a.pitch + (b.pitch - a.pitch) * t;
            // yaw 最短弧插值（避免 -179° → 179° 画个大圆）
            let yaw = lerp_yaw_shortest(a.yaw, b.yaw, t);
            Some((position, yaw, pitch))
        }
    }
}

/// 整个房间的远端玩家插值状态。
pub struct PlayerInterp {
    buffers: HashMap<EntityId, RemoteBuffer>,
    /// 插值延迟（毫秒）。客户端以 `server_time_ms_now - delay_ms` 作为渲染 target。
    pub delay_ms: f64,
}

impl Default for PlayerInterp {
    fn default() -> Self {
        Self::new()
    }
}

impl PlayerInterp {
    pub fn new() -> Self {
        Self {
            buffers: HashMap::new(),
            delay_ms: 100.0,
        }
    }

    /// 主循环入口：每逻辑 tick 把 Host 发来的 PlayerTick 中包含的所有 player snapshot
    /// 喂进对应的 buffer。`server_time_ms` 是 PlayerTick 携带的 Host 时钟。
    pub fn ingest_tick(
        &mut self,
        eid: EntityId,
        server_time_ms: u64,
        position: Vec3,
        yaw: f32,
        pitch: f32,
    ) {
        let buf = self.buffers.entry(eid).or_insert_with(RemoteBuffer::new);
        buf.push(Sample {
            server_time_ms,
            position,
            yaw,
            pitch,
        });
    }

    /// 在给定渲染 time（server 时间 ms）上获取该远端玩家的插值后 pose。
    /// 返回 `(position, yaw, pitch)`；buffer 为空时返回 None。
    pub fn advance(
        &mut self,
        eid: EntityId,
        render_server_time_ms: f64,
    ) -> Option<(Vec3, f32, f32)> {
        self.buffers
            .get(&eid)
            .and_then(|buf| buf.get(render_server_time_ms))
    }

    /// PeerLeft 时移除该玩家缓冲区。
    pub fn remove(&mut self, eid: EntityId) {
        self.buffers.remove(&eid);
    }

    /// 当前有缓冲的 entity_id 列表（用于渲染时遍历）。
    pub fn ids(&self) -> impl Iterator<Item = EntityId> + '_ {
        self.buffers.keys().copied()
    }
}

/// 最短弧 yaw 线性插值。自动处理 -π → π 的 wrap。
fn lerp_yaw_shortest(a: f32, b: f32, t: f32) -> f32 {
    use std::f32::consts::TAU;
    let mut d = b - a;
    if d > TAU / 2.0 {
        d -= TAU;
    } else if d < -TAU / 2.0 {
        d += TAU;
    }
    let mut res = a + d * t;
    // 归一化回 [-π, π]
    if res < -TAU / 2.0 {
        res += TAU;
    } else if res > TAU / 2.0 {
        res -= TAU;
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interp_lerps_between_two_samples() {
        let mut interp = PlayerInterp::new();
        interp.delay_ms = 0.0; // 方便测试：render_time 直接等于 server_time
        interp.ingest_tick(1, 1000, Vec3::new(0.0, 64.0, 0.0), 0.0, 0.0);
        interp.ingest_tick(1, 2000, Vec3::new(10.0, 64.0, 10.0), 0.0, 0.0);

        // render @ 1500ms → 50% lerp → (5, 64, 5)
        let (pos, _, _) = interp.advance(1, 1500.0).expect("should have pose");
        let dist = (pos - Vec3::new(5.0, 64.0, 5.0)).length();
        assert!(dist < 0.01);
    }

    #[test]
    fn interp_handles_yaw_wraparound_shortest_arc() {
        let mut interp = PlayerInterp::new();
        interp.delay_ms = 0.0;
        // -172° → +172°：最短弧差 0.28 rad，越过 ±π 边界，50% 位置应在 ±π 附近
        interp.ingest_tick(1, 1000, Vec3::ZERO, -3.0, 0.0);
        interp.ingest_tick(1, 2000, Vec3::ZERO, 3.0, 0.0);

        let (_, yaw, _) = interp.advance(1, 1500.0).expect("should have pose");
        // 结果应在 ±π 附近（≈ ±3.1416），取绝对值 > 3.0
        assert!(
            yaw.abs() > 3.0,
            "shortest arc through pi: yaw should be near ±π, got {yaw}"
        );
    }

    #[test]
    fn interp_returns_none_for_unknown_entity() {
        let mut interp = PlayerInterp::new();
        assert!(interp.advance(999, 0.0).is_none());
    }

    #[test]
    fn interp_evicts_old_samples_past_capacity() {
        let mut interp = PlayerInterp::new();
        for i in 0..25u64 {
            interp.ingest_tick(1, 1000 + i, Vec3::new(i as f32, 64.0, 0.0), 0.0, 0.0);
        }
        // 只保留最近 20 条，最早样本的 x < 5
        let (pos, _, _) = interp
            .advance(1, 1000.0)
            .expect("should fallback to earliest kept");
        assert!(
            pos.x >= 5.0,
            "earliest kept should be sample 5+, got x={}",
            pos.x
        );
    }

    #[test]
    fn interp_clamps_to_latest_when_render_time_after_all() {
        let mut interp = PlayerInterp::new();
        interp.delay_ms = 0.0;
        interp.ingest_tick(1, 1000, Vec3::new(0.0, 64.0, 0.0), 0.5, 0.0);
        interp.ingest_tick(1, 2000, Vec3::new(10.0, 64.0, 10.0), 1.0, 0.0);
        // render 3000 > 最新 2000 → 返回最新
        let (pos, yaw, _) = interp.advance(1, 3000.0).expect("should return latest");
        assert!((pos - Vec3::new(10.0, 64.0, 10.0)).length() < 0.01);
        assert!((yaw - 1.0).abs() < 0.01);
    }
}
