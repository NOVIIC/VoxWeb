# 物理与方块交互

> **何时阅读**：改物理手感（跳跃高度、走路速度）；改 AABB 碰撞；改射线射程；改挖放交互
> **关联文档**：[`README.md`](../../README.md) · [`modules/client.md`](../modules/client.md) · [`modules/server.md`](../modules/server.md) · [`networking/protocol.md`](../networking/protocol.md) · [`networking/prediction.md`](../networking/prediction.md)

---

## 一、设计目标

把"可看的体素世界"变成"可玩"：
- 玩家有重量，不会穿墙
- 能跳跃、能走路、能在斜坡边缘留住自己
- 能看准目标方块挖一下；右键能放方块
- 在主机权威架构下，本地操作即时反馈，主机仲裁后协调

物理参数沿用 Minecraft 风格（已被广泛验证手感舒适）。

---

## 二、玩家 AABB

```rust
pub const PLAYER_WIDTH: f32 = 0.6;
pub const PLAYER_HEIGHT: f32 = 1.8;
pub const PLAYER_EYE_OFFSET: f32 = 1.62;   // 眼高（相机位置 = position + (0, 1.62, 0)）
```

`position` 表示玩家**脚底中心**。AABB 在 `position` 周围对称延伸：

```
     ┌──────┐  ← y = position.y + 1.8
     │      │
     │  ⊙   │  ← y = position.y + 1.62（相机眼睛）
     │      │
     │      │
     └──────┘  ← y = position.y
   x = px ± 0.3
   z = pz ± 0.3
```

```rust
pub fn player_aabb(position: Vec3) -> Aabb {
    Aabb {
        min: Vec3::new(position.x - 0.3, position.y, position.z - 0.3),
        max: Vec3::new(position.x + 0.3, position.y + 1.8, position.z + 0.3),
    }
}
```

---

## 三、移动模式

```rust
pub enum CameraMode {
    Walk,    // 受重力，碰撞
    Fly,     // 无重力，有碰撞（分轴扫动）；WASD 仅沿水平面，Space/Shift 控制升降
}
```

切换：
- 默认 `Walk`
- 双击空格切 `Fly`，再次双击空格切回 `Walk`

---

## 四、Walk 模式物理

### 4.1 速度计算

```rust
pub struct ClientPhysics {
    pub velocity: Vec3,
    pub on_ground: bool,
    pub mode: CameraMode,
}
```

每渲染帧：

```rust
pub fn tick(&mut self, world: &WorldView, camera: &mut Camera, input: &InputManager, dt: f32) {
    if matches!(self.mode, CameraMode::Fly) {
        self.tick_fly(camera, input, dt);
        return;
    }

    // 1. 计算期望水平速度（基于 input + 相机朝向）
    let mut target_horizontal = Vec3::ZERO;
    let forward = camera.forward_horizontal();   // 投影到 xz 平面后单位化
    let right = camera.right();

    if input.key_held(KeyCode::W) { target_horizontal += forward; }
    if input.key_held(KeyCode::S) { target_horizontal -= forward; }
    if input.key_held(KeyCode::A) { target_horizontal -= right; }
    if input.key_held(KeyCode::D) { target_horizontal += right; }

    if target_horizontal.length_squared() > 0.0 {
        target_horizontal = target_horizontal.normalize() * WALK_SPEED;
    }

    // 2. 应用平滑加速（避免像贴地一样的瞬移）
    self.velocity.x = lerp(self.velocity.x, target_horizontal.x, ACC * dt);
    self.velocity.z = lerp(self.velocity.z, target_horizontal.z, ACC * dt);

    // 3. 跳跃
    if input.key_just_pressed(KeyCode::Space) && self.on_ground {
        self.velocity.y = JUMP_SPEED;
        self.on_ground = false;
    }

    // 4. 重力
    self.velocity.y += GRAVITY * dt;
    self.velocity.y = self.velocity.y.max(-TERMINAL_VELOCITY);

    // 5. 分轴碰撞
    let displacement = self.velocity * dt;
    self.move_with_collision(world, &mut camera.position, displacement);

    // 6. 更新 on_ground
    self.on_ground = check_ground(world, camera.position);
}
```

### 4.2 参数表

| 参数 | 值 | 说明 |
|---|---|---|
| `WALK_SPEED` | 4.3 m/s | 平地走路速度 |
| `SPRINT_SPEED` | 5.6 m/s | 按住 Ctrl 时（v2） |
| `JUMP_SPEED` | 8.4 m/s | 跳跃初速度（约 1.25m 跳高） |
| `GRAVITY` | -32.0 m/s² | 比真实重力大，让游戏手感紧凑 |
| `TERMINAL_VELOCITY` | 78.0 m/s | 落地最快速度 |
| `ACC` | 12.0 1/s | 速度平滑系数（lerp rate） |

### 4.3 分轴碰撞

不能一次性应用 `velocity * dt`，否则边角处会出问题。分 3 步：

```rust
fn move_with_collision(&mut self, world: &WorldView, pos: &mut Vec3, disp: Vec3) {
    // Y 轴优先（处理跳跃和下落）
    if disp.y != 0.0 {
        let new_y = pos.y + disp.y;
        let test_aabb = player_aabb(Vec3::new(pos.x, new_y, pos.z));
        if let Some(_) = collide_first(world, &test_aabb, Axis::Y, disp.y > 0.0) {
            // 碰撞：吸附到方块表面
            let snapped = snap_to_block_face(pos.y, disp.y, world);
            pos.y = snapped;
            self.velocity.y = 0.0;
        } else {
            pos.y = new_y;
        }
    }

    // X 轴
    if disp.x != 0.0 {
        let new_x = pos.x + disp.x;
        let test_aabb = player_aabb(Vec3::new(new_x, pos.y, pos.z));
        if collides_with_world(world, &test_aabb) {
            self.velocity.x = 0.0;
            // 不更新 pos.x
        } else {
            pos.x = new_x;
        }
    }

    // Z 轴
    if disp.z != 0.0 {
        let new_z = pos.z + disp.z;
        let test_aabb = player_aabb(Vec3::new(pos.x, pos.y, new_z));
        if collides_with_world(world, &test_aabb) {
            self.velocity.z = 0.0;
        } else {
            pos.z = new_z;
        }
    }
}
```

**关键**：Y 轴单独先算，再独立测试 X 和 Z。这样玩家擦着墙走时不会被卡住（X 撞墙不影响 Z 移动）。

### 4.4 collides_with_world 实现

```rust
fn collides_with_world(world: &WorldView, aabb: &Aabb) -> bool {
    let min_block = aabb.min.floor().as_ivec3();
    let max_block = aabb.max.ceil().as_ivec3();
    for x in min_block.x..max_block.x {
        for y in min_block.y..max_block.y {
            for z in min_block.z..max_block.z {
                let block = world.get_block_world(x, y, z);
                if !properties(block).solid { continue; }
                let block_aabb = Aabb::block_at(x, y, z);
                if aabb.intersects(&block_aabb) { return true; }
            }
        }
    }
    false
}
```

`properties(block).solid` 区分实体与非实体：AIR/WATER/GLASS 不参与碰撞（玻璃是否碰撞取决于方块表配置；当前默认玻璃可碰撞）。

### 4.5 地面检测

```rust
fn check_ground(world: &WorldView, position: Vec3) -> bool {
    let probe_aabb = Aabb {
        min: Vec3::new(position.x - 0.3, position.y - 0.05, position.z - 0.3),
        max: Vec3::new(position.x + 0.3, position.y, position.z + 0.3),
    };
    collides_with_world(world, &probe_aabb)
}
```

向脚底下方探 5cm，有方块即在地面。

---

## 五、Fly 模式物理

简化版（实际实装见 [`crates/client/src/physics.rs::step_fly`](../../crates/client/src/physics.rs)）：
```rust
fn step_fly(&mut self, get_block: &dyn Fn(i32, i32, i32) -> BlockID, camera: &Camera, input: &InputState, dt: f32) {
    let mut dir = Vec3::ZERO;
    // WASD 沿水平面（不含 pitch），避免视角朝下时按 W 反而往地里钻
    let f = camera.forward_horizontal();
    let r = camera.right();
    let u = Vec3::Y;
    if input.forward { dir += f; }
    if input.backward { dir -= f; }
    if input.left { dir -= r; }
    if input.right { dir += r; }
    if input.jump_held { dir += u; }   // Space 上升
    if input.sneak { dir -= u; }       // Shift 下降
    if dir.length_squared() > 0.0 {
        let disp = dir.normalize() * FLY_SPEED * dt;
        // 分轴碰撞（与 Walk 一致）：先 Y 再 X 再 Z
        self.move_axis_y(get_block, disp.y);
        self.move_axis_x(get_block, disp.x);
        self.move_axis_z(get_block, disp.z);
    }
    self.velocity = Vec3::ZERO;
    self.on_ground = false;
}
```

`FLY_SPEED = 12.0 m/s`。垂直方向单独由 Space/Shift 控制，让"看哪里"和"飞哪里"解耦——观察俯视地形时按 W 仍只前进，需要下降时按 Shift。

Fly 模式同样执行 Y/X/Z 分轴碰撞检测，避免穿墙。X/Z 轴碰撞时会吸附到墙面（与 Y 轴吸附到方块面同理），解决浮点累积误差导致的穿墙问题。

---

## 六、视角控制

```rust
pub fn handle_mouse_motion(camera: &mut Camera, dx: f32, dy: f32, sensitivity: f32) {
    camera.yaw -= dx * sensitivity * DEG_TO_RAD;
    camera.pitch -= dy * sensitivity * DEG_TO_RAD;
    camera.pitch = camera.pitch.clamp(-PITCH_LIMIT, PITCH_LIMIT);
    // PITCH_LIMIT = 89° → 弧度 = 1.5533
}
```

`dx` / `dy` 是浏览器 `MouseEvent.movementX/Y`（指针锁状态下，无屏幕边界）。
`sensitivity` 默认 0.15，可在设置面板 0.1..=5.0 调。

> **Firefox 兼容性坑**：FF 中 `movementX/Y` 单位与 Chrome 不同（FF: device pixels, Chrome: CSS pixels）。需要按 `devicePixelRatio` 修正：
> ```rust
> let dpr = window.device_pixel_ratio() as f32;
> let normalized_dx = if is_firefox() { dx / dpr } else { dx };
> ```
> 详见 [`reference.md`](../reference.md)。

---

## 七、DDA 射线（Raycasting）

### 算法原理
"Amanatides & Woo" 网格步进算法（DDA）：沿视线方向逐个体素步进，每步检查所在体素是否实体。

### Rust 实现

```rust
pub struct RaycastHit {
    pub block_pos: Position,
    pub face: Face,
    pub block_id: BlockID,
    pub distance: f32,
}

pub fn raycast(world: &WorldView, origin: Vec3, dir: Vec3, max_distance: f32) -> Option<RaycastHit> {
    let dir = dir.normalize();

    let mut current = origin.floor().as_ivec3();
    let step = IVec3::new(dir.x.signum() as i32, dir.y.signum() as i32, dir.z.signum() as i32);

    let t_delta = Vec3::new(
        if dir.x != 0.0 { (1.0 / dir.x.abs()) } else { f32::INFINITY },
        if dir.y != 0.0 { (1.0 / dir.y.abs()) } else { f32::INFINITY },
        if dir.z != 0.0 { (1.0 / dir.z.abs()) } else { f32::INFINITY },
    );

    let mut t_max = Vec3::new(
        if step.x > 0 { ((current.x + 1) as f32 - origin.x) / dir.x } else if step.x < 0 { (origin.x - current.x as f32) / -dir.x } else { f32::INFINITY },
        // 同理 y, z
        ..
    );

    let mut last_step_axis = Axis::X;
    let mut traveled = 0.0;

    while traveled < max_distance {
        let block = world.get_block_world(current.x, current.y, current.z);
        if !matches!(block, BlockID::AIR) {
            return Some(RaycastHit {
                block_pos: Position { x: current.x, y: current.y, z: current.z },
                face: face_from_step(last_step_axis, step),
                block_id: block,
                distance: traveled,
            });
        }

        // 选择最近的下一个网格平面
        if t_max.x < t_max.y && t_max.x < t_max.z {
            traveled = t_max.x;
            current.x += step.x;
            t_max.x += t_delta.x;
            last_step_axis = Axis::X;
        } else if t_max.y < t_max.z {
            traveled = t_max.y;
            current.y += step.y;
            t_max.y += t_delta.y;
            last_step_axis = Axis::Y;
        } else {
            traveled = t_max.z;
            current.z += step.z;
            t_max.z += t_delta.z;
            last_step_axis = Axis::Z;
        }
    }
    None
}
```

`face_from_step(axis, step)`：根据进入的方向轴和步进方向，得到命中面（用于放方块时计算邻居位置）。

`MAX_BREAK_RANGE = 6.0 m`、`MAX_PLACE_RANGE = 6.0 m`。

---

## 八、挖放交互流程

完整时序见 [`networking/protocol.md` 6.2 章节](../networking/protocol.md#62-方块挖放reliable--ack)；本文重点是客户端体验。

服务端仲裁按材质属性执行额外规则：y=0 永远视为世界底部边界；`properties(block).breakable == false` 的材质不可挖（当前为 BEDROCK）；放置请求只能使用 `appears_in_hotbar == true` 的材质，防止客户端通过协议放置 AIR / BEDROCK 等非创造模式材料。

`ImmediateRelaxation` 材质（当前沙、泥土、草）在 Host / Local-Only 权威侧做局部松弛：挖放成功后从编辑点附近入队，优先竖直下落，受阻后按确定性方向尝试斜下滑落。单次操作有移动与访问预算，结果通过多条 `FieldDelta` 同步给 Remote。

`FloatingOnly` 硬材质（石头、木头、玻璃、石砖）在同一权威侧做第一版稳定性检查：挖放成功后从编辑点附近收集小型硬材质连通块；若连通块没有接触任何稳定 solid 支撑，就记录一个 `FreeObject`，整体下落到最近支撑面，并立即投影回 `MaterialField`。当前不做可见刚体飞行、碰撞反弹或碎裂，结果通过可靠通道上的 `FreeObjectProject` 同步。

### 8.1 视觉反馈

每渲染帧：
```rust
let hit = raycast(&world_view, camera.position + Vec3::Y * PLAYER_EYE_OFFSET, camera.forward(), 6.0);
self.current_hit = hit;
```

`current_hit` 用于 HUD 渲染：
- 选中方块的线框（半透明黑边）
- 准星（HUD 中心，固定）

### 8.2 鼠标左键挖

```rust
fn on_left_click(&mut self) {
    let Some(hit) = self.current_hit else { return; };

    let request_id = self.prediction.next_request_id();

    // 乐观更新（仅 Remote 角色；Local-Only 等服务端 FieldDelta）
    // 软材质后续滑落以 Host 返回的额外 FieldDelta 为准。
    if matches!(self.role, Role::Remote) {
        let backup = self.world_view.get_block(hit.block_pos);
        self.prediction.pending_actions.insert(request_id, PendingAction {
            request_id, kind: PendingActionKind::Break,
            backup, pos: hit.block_pos, since_tick: self.current_tick,
        });
        self.world_view.set_block(hit.block_pos, BlockID::AIR);
        self.mesh_jobs.enqueue_with_neighbors(hit.block_pos, Priority::High);
    }

    self.net.send_to_server(ClientMessage::Break {
        pos: hit.block_pos,
        request_id,
        input_tick: self.local_input_tick,
        player_position: self.physics.feet_position,
    });
}
```

### 8.3 鼠标右键放

```rust
fn on_right_click(&mut self) {
    let Some(hit) = self.current_hit else { return; };

    // 计算放置位置 = 命中面外侧
    let neighbor_pos = neighbor_of(hit.block_pos, hit.face);
    let block_to_place = self.selected_block;   // HUD hotbar 选择的方块

    // 校验本地：放置位置不能与玩家 AABB 重叠
    let block_aabb = Aabb::block_at(neighbor_pos.x, neighbor_pos.y, neighbor_pos.z);
    let player_aabb_now = player_aabb(self.camera.position);
    if block_aabb.intersects(&player_aabb_now) {
        self.ui.toast("无法在此放置");
        return;
    }

    let request_id = self.prediction.next_request_id();

    if matches!(self.role, Role::Remote) {
        let backup = self.world_view.get_block(neighbor_pos);
        self.prediction.pending_actions.insert(request_id, PendingAction {
            request_id, kind: PendingActionKind::Place(block_to_place),
            backup, pos: neighbor_pos, since_tick: self.current_tick,
        });
        self.world_view.set_block(neighbor_pos, block_to_place);
        self.mesh_jobs.enqueue_with_neighbors(neighbor_pos, Priority::High);
    }

    self.net.send_to_server(ClientMessage::Place {
        pos: neighbor_pos,
        block: block_to_place,
        request_id,
        input_tick: self.local_input_tick,
        player_position: self.physics.feet_position,
    });
}
```

`neighbor_of` 根据命中面：
| Face | 邻居偏移 |
|---|---|
| PosX | (+1, 0, 0) |
| NegX | (-1, 0, 0) |
| PosY | (0, +1, 0) |
| NegY | (0, -1, 0) |
| PosZ | (0, 0, +1) |
| NegZ | (0, 0, -1) |

### 8.4 ActionAck 响应

详见 [`networking/prediction.md` 3.3-3.4](../networking/prediction.md#33-actionack-处理)。

---

## 九、连续挖掘 / 放置

按住左键持续挖：
- 防滥用：每次操作有 `MIN_ACTION_INTERVAL = 100ms` 冷却
- 体验上接近持续挖

```rust
fn handle_action_input(&mut self) {
    let now = performance_now();
    if self.input.button(MouseButton::Left).held && now - self.last_break_at > MIN_ACTION_INTERVAL {
        self.on_left_click();
        self.last_break_at = now;
    }
    if self.input.button(MouseButton::Right).just_pressed {
        self.on_right_click();
    }
    // 右键持续放置不开放（避免快速建造垃圾）
}
```

---

## 十、Hotbar（方块选择）

简化版：
- 1-9 键切换当前手持方块
- 内置可选方块：STONE / DIRT / GRASS / SAND / WOOD / LEAVES / GLASS / WATER / STONE_BRICKS
- BEDROCK 是世界底部边界材质，不进 hotbar，不能挖掘或放置
- HUD 底部居中显示当前选中方块图标（v2）

```rust
pub struct Hotbar {
    pub items: [BlockID; 9],
    pub selected: usize,    // 0..9
}
```

---

## 十一、不在范围

- 蹲伏（Shift 慢走、自动避免边缘掉落）— v2
- 冲刺（Ctrl 跑步）— v2
- 楼梯 / 半砖（非完整方块碰撞）— 不做
- 流体推动（玩家被水冲走）— 不做
- 摔伤 / 血量 / 死亡 — 不做
- 工具耐久 / 挖掘速度差异 — 不做
- 头顶上方撞天花板 — 当前实现已正确处理（Y 轴正向碰撞 → 吸附到方块底面 + 速度归零；已嵌入方块时搜索最近安全位置）
