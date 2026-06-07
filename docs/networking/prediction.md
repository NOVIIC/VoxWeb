# 客户端预测、协调与远端插值

> **何时阅读**：改延迟体验；调位置同步抖动；改方块挖放体验；调时钟偏移
> **关联文档**：[`README.md`](../../README.md) · [`networking/protocol.md`](protocol.md) · [`modules/client.md`](../modules/client.md) · [`modules/server.md`](../modules/server.md) · [`features/physics.md`](../features/physics.md)

---

## 一、设计目标

P2P 联机在 RTT ≥ 50ms 的环境下也要保证：
- **本地操作零感知延迟**：移动、转视角、左右键挖放，按下立即反映在画面
- **远端玩家平滑移动**：不抖动、不卡顿、不瞬移
- **状态最终一致性**：服务端权威决议被采纳，客户端误差被悄无声息修正

主要技术：
1. **客户端预测（Client-Side Prediction）**：本地立即应用输入
2. **服务端协调（Reconciliation）**：服务端权威反馈差值后软修正
3. **远端插值（Entity Interpolation）**：远端玩家延迟一段时间后插值显示

---

## 二、本地玩家预测

### 2.1 移动预测

```rust
// 渲染帧（客户端立即响应）
fn update_camera(&mut self, dt: f32) {
    let acc = compute_acceleration(&self.input, &self.camera);
    self.physics.tick(&self.world_view, &mut self.camera, acc, dt);
    // camera.position 已立即更新
}

// 逻辑帧（60Hz 上报）
fn upload_input(&mut self) {
    self.next_input_tick += 1;
    let msg = ClientMessage::PlayerInput {
        tick: self.next_input_tick,
        position: self.camera.position,
        yaw: self.camera.yaw,
        pitch: self.camera.pitch,
    };
    // 同时记入 history，供协调时回滚使用
    self.prediction.input_history.push_back(InputRecord {
        tick: self.next_input_tick,
        position: self.camera.position,
    });
    self.net.send_to_server(msg);
}
```

`InputRecord` 历史保留最近 ~2 秒（120 条）。

### 2.2 协调（Reconciliation）

当客户端收到 `PlayerTick`，找到自己 entity 的服务端权威位置。关键点是用
`PlayerSnapshot.last_input_tick` 对齐本地历史：它表示 Host 已经接受到该玩家的哪一条
`PlayerInput`。这样 RTT 200ms+ 时也不会把“200ms 前自己的位置回声”和当前预测位置直接相减。

```rust
fn reconcile_self(&mut self, snap: &PlayerSnapshot) {
    // Host 已处理的客户端输入 tick 对应本地 input_history 中的一条记录
    let our_record = self.prediction.input_history.iter()
        .find(|r| r.tick == snap.last_input_tick);

    if let Some(record) = our_record {
        let error = (snap.position - record.position).length();
        let correction = snap.position - record.position;

        if error < SOFT_THRESHOLD {
            // 误差小：忽略，本地预测继续
        } else if error < HARD_THRESHOLD {
            // 误差中等：把同一输入时刻的差值软插补到当前预测位置
            self.prediction.pending_correction = Some(correction);
        } else {
            // 误差大：立刻把差值加到当前预测位置，而不是回到旧快照位置
            self.camera.position += correction;
            self.prediction.input_history.clear();
        }
    }

    // 清理过旧记录
    self.prediction.input_history.retain(|r| r.tick > snap.last_input_tick);
}
```

阈值参考：
- `SOFT_THRESHOLD = 0.1m`
- `HARD_THRESHOLD = 2.0m`

### 2.3 软插补的实现

`pending_correction` 存在时，每渲染帧把一部分 correction 应用到 camera：

```rust
fn apply_pending_correction(&mut self, dt: f32) {
    if let Some(correction) = self.prediction.pending_correction {
        let blend_rate = 5.0;  // 完成 1.0 correction 需 0.2 秒
        let step = correction * (dt * blend_rate).min(1.0);
        self.camera.position += step;

        if step.length() >= correction.length() - 0.001 {
            self.prediction.pending_correction = None;
        } else {
            self.prediction.pending_correction = Some(correction - step);
        }
    }
}
```

效果：玩家会感到轻微"漂移"修正，但不会瞬移。

---

## 三、方块挖放预测

### 3.1 状态记录

```rust
pub struct PendingAction {
    pub request_id: u32,
    pub kind: PendingActionKind,
    pub backup: BlockID,           // 修改前的方块（rollback 用）
    pub pos: Position,
    pub input_tick: u32,           // 点击时本地输入序号
    pub since_tick: u32,
}

pub enum PendingActionKind {
    Break,
    Place(BlockID),
}
```

### 3.2 乐观执行

```rust
fn handle_break_input(&mut self, hit: RaycastHit) {
    let request_id = self.prediction.next_request_id();
    let backup = self.world_view.get_block(hit.pos);
    let input_tick = self.local_input_tick;
    let player_position = self.physics.feet_position;

    // 本地立即修改（仅 Remote 视角；Local/Host 与 server 共享世界，仍等权威 BlockUpdate）
    if matches!(self.role, Role::Remote) {
        self.world_view.set_block(hit.pos, BlockID::AIR);
        self.mesh_jobs.enqueue(hit.pos.to_chunk_pos(), Priority::High);
    }

    self.prediction.pending_actions.insert(request_id, PendingAction {
        request_id, kind: PendingActionKind::Break,
        backup, pos: hit.pos, input_tick, since_tick: self.current_tick,
    });

    self.net.send_to_server(ClientMessage::Break {
        pos: hit.pos,
        request_id,
        input_tick,
        player_position,
    });
}
```

`Place` 同样携带 `input_tick` 与点击时 `player_position`，并在 Remote 本地世界视图中先写入目标方块。
Host 仍以权威世界状态决定最终是否接受；拒绝时客户端用 `backup` 回滚。

### 3.3 ActionAck 处理

```rust
fn handle_action_ack(&mut self, ack: ActionAck) {
    let pending = self.prediction.pending_actions.remove(&ack.request_id);
    let Some(action) = pending else { return; };

    if ack.accepted {
        // 等 BlockUpdate 真正 commit；这里仅留作日志（也可以直接什么都不做）
    } else {
        // 回滚
        self.world_view.set_block(action.pos, action.backup);
        self.mesh_jobs.enqueue(action.pos.to_chunk_pos(), Priority::High);
        self.ui.toast(format!("操作被拒绝：{:?}", ack.reason));
    }
}
```

### 3.4 BlockUpdate 处理

```rust
fn handle_block_update(&mut self, update: BlockUpdate) {
    self.world_view.set_block(update.pos, update.block);
    self.mesh_jobs.enqueue(update.pos.to_chunk_pos(), Priority::High);
}
```

> 注意：BlockUpdate 可能在 ActionAck 之前到达（reliable 但分通道乱序在某些实现下可能发生；保险起见两条都按"幂等更新"处理）。

### 3.5 超时处理

如果 `pending_action.since_tick` 超过 5 秒还没收到 ActionAck：
- 视为网络异常，回滚本地修改
- 显示连接异常提示

---

## 四、时钟同步

服务端在 `PlayerTick` 中携带 `server_time_ms`。客户端用它估算 server-client 时钟偏移：

```rust
pub struct ClockSync {
    offset_ms: f32,    // server_time = client_time + offset
    initialized: bool,
}

impl ClockSync {
    pub fn ingest_pong(&mut self, sent_client_ms: u64, server_ms: u64) {
        let now = client_time_ms_now();
        let rtt = (now - sent_client_ms) as f32;
        let one_way = rtt / 2.0;
        let estimated_server_at_now = server_ms as f32 + one_way;
        let new_offset = estimated_server_at_now - now as f32;

        // 指数平滑；第一条样本直接采用，避免从 0 慢慢漂移
        self.offset_ms = if self.initialized {
            self.offset_ms * 0.8 + new_offset * 0.2
        } else {
            self.initialized = true;
            new_offset
        };
    }

    pub fn server_time_ms_now(&self) -> f32 {
        client_time_ms_now() as f32 + self.offset_ms
    }
}
```

可选：每 5 秒发 Ping 估算偏移。

`PlayerTick.server_time_ms` 也会提供低频校正样本：若当前已有 RTT，就按
`server_time_ms + rtt / 2 - client_now` 估算；否则暂按 `server_time_ms - client_now` 估算。
这类样本使用更小的平滑权重，避免网络抖动直接改变远端玩家渲染 target。

主要用于：
- 远端玩家插值缓冲区按 `server_time_ms` 排序
- HUD 显示延迟

---

## 五、远端玩家插值

### 5.1 缓冲区结构

```rust
pub struct RemotePlayerBuffer {
    pub snapshots: VecDeque<TimedSnapshot>,
    pub current_pos: Vec3,
    pub current_yaw: f32,
    pub current_pitch: f32,
}

pub struct TimedSnapshot {
    pub server_time_ms: f32,
    pub position: Vec3,
    pub yaw: f32,
    pub pitch: f32,
}

pub struct PlayerInterp {
    pub buffers: HashMap<EntityId, RemotePlayerBuffer>,
    pub interp_delay_ms: f32,    // 100ms 默认
    pub clock: ClockSync,
}
```

### 5.2 ingest

```rust
pub fn ingest_tick(&mut self, players: &[PlayerSnapshot], server_time_ms: u64) {
    for snap in players {
        let buf = self.buffers.entry(snap.entity_id)
            .or_insert_with(RemotePlayerBuffer::new);
        buf.snapshots.push_back(TimedSnapshot {
            server_time_ms: server_time_ms as f32,
            position: snap.position,
            yaw: snap.yaw,
            pitch: snap.pitch,
        });
        // 限长，防止缓冲区无限增长
        while buf.snapshots.len() > 30 {
            buf.snapshots.pop_front();
        }
    }
}
```

### 5.3 推进（每渲染帧）

目标：计算"当前应该展示"的远端玩家位置 = 在 `server_time - interp_delay` 时刻的插值。

```rust
pub fn advance(&mut self, _dt: f32) {
    let render_server_time = self.clock.server_time_ms_now() - self.interp_delay_ms;

    for (_, buf) in self.buffers.iter_mut() {
        // 找到 a, b 满足 a.time ≤ render_server_time ≤ b.time
        let (a, b) = find_bracket(&buf.snapshots, render_server_time);

        if let (Some(a), Some(b)) = (a, b) {
            let t = ((render_server_time - a.server_time_ms) / (b.server_time_ms - a.server_time_ms)).clamp(0.0, 1.0);
            buf.current_pos = a.position.lerp(b.position, t);
            buf.current_yaw = lerp_angle(a.yaw, b.yaw, t);
            buf.current_pitch = lerp_angle(a.pitch, b.pitch, t);
        } else if let Some((prev, latest)) = latest_pair(&buf.snapshots) {
            // 没有未来快照可插值：按最近两帧速度短外推（最多 50ms）防止丢包冻结
            let dt = (render_server_time - latest.server_time_ms).clamp(0.0, 50.0) * 0.001;
            let sample_dt = ((latest.server_time_ms - prev.server_time_ms) * 0.001).max(0.001);
            let velocity = (latest.position - prev.position) / sample_dt;
            buf.current_pos = latest.position + velocity * dt;
            buf.current_yaw = latest.yaw;
            buf.current_pitch = latest.pitch;
        } else if let Some(latest) = buf.snapshots.back() {
            buf.current_pos = latest.position;
            buf.current_yaw = latest.yaw;
            buf.current_pitch = latest.pitch;
        }
    }
}
```

### 5.4 渲染时使用

`render` 模块通过 `frame_data.remote_players` 拿到所有远端玩家的 `current_pos / yaw / pitch`，渲染：
- 玩家身体（一个简单 box / capsule）
- 名牌（egui billboard，详见 [`features/ui.md`](../features/ui.md)）

### 5.5 参数调优

| 参数 | 默认 | 调高的影响 | 调低的影响 |
|---|---|---|---|
| `interp_delay_ms` | 100 | 更稳定，延迟感更强 | 更跟手，丢包时易卡顿 |
| Buffer 长度 | 30 | 内存多用 | 网络抖动时插值失败 |
| 外推容许 | 50ms | 延迟下也保持移动 | 丢包时画面冻结 |

UI 设置面板提供 50/100/150ms 三档可选。

---

## 六、特殊情况

### 6.1 玩家从 Tick 中消失
- 表示 `PeerLeft` 即将到来
- 不立即从渲染列表移除（保留 1 秒 grace period，避免短暂丢包导致玩家闪烁）
- 收到 `PeerLeft` 才真删除

### 6.2 新玩家加入但首个 Tick 未到
- `PeerJoined` 仅添加占位 entity，不渲染
- 收到第一个含其位置的 `PlayerTick` → 创建 buffer，开始插值
- 在那之前其它玩家的玩家列表 widget 可显示但不在 3D 世界中渲染

### 6.3 远端玩家瞬移（如挖墙快速穿过）
- 单帧位置差 > 10m → 视为瞬移而非平滑移动
- 不插值，直接跳到新位置
- 通过比较相邻 snapshot 的 distance 检测

```rust
if (b.position - a.position).length() > 10.0 {
    buf.current_pos = b.position;   // 跳过 a→b 插值
}
```

---

## 七、调试可视化

调试模式（设置面板开启）下渲染：
- 远端玩家"鬼影"：实际接收到的最新 snapshot 位置（不插值）
- 当前插值位置（实体本身）
- 自己的预测路径与服务端权威路径（短线段）

辅助排查抖动 / 协调失败 / 时钟偏差。

---

## 八、性能预算

| 项目 | 目标 |
|---|---|
| `prediction.reconcile_self` | < 0.05ms / 次 |
| `interp.advance` | < 0.2ms（8 玩家） |
| `prediction.input_history` 内存 | < 50KB（120 条 × ~400 字节） |

---

## 九、不在范围

- 弹幕/扔物等高速实体 — 当前没有这种内容
- 服务端 lag compensation（用户射击时回退服务端时间到客户端看到时） — 不做（项目无瞬时打击命中判定）
- 多 Tick 输入合批 — 直接每帧一发（带宽足够）
- 服务端权威动画状态（玩家姿态机、动作）— 仅同步位置朝向
