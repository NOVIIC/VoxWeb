//! 物理仲裁：服务端权威的挖放验证与局部材质松弛。
//!
//! Phase 3：检查射程（≤ MAX_REACH）、目标方块状态、放置位置不与玩家 AABB 重叠。
//! Phase 5 引入完整玩家表后，多人挖放的范围/重叠校验复用同一套函数。

use std::collections::{HashSet, VecDeque};

use glam::Vec3;

use voxweb_core::block::{BlockID, MaterialCell, StabilityPolicy, properties};
use voxweb_core::chunk::{CHUNK_Y, Position};
use voxweb_core::geometry::{Aabb, PLAYER_EYE_OFFSET, player_aabb};
use voxweb_core::object::FreeObjectState;
use voxweb_core::protocol::AckReason;

use super::world::World;

/// 玩家操作距离上限（眼睛到方块中心，单位：方块/米）。
pub const MAX_REACH: f32 = 6.0;
/// 单次挖放后最多执行多少个颗粒移动，避免一次编辑卡住主线程。
const RELAXATION_MOVE_BUDGET: usize = 64;
/// 单次挖放后最多检查多少个候选 cell，限制局部松弛范围。
const RELAXATION_VISIT_BUDGET: usize = 256;
/// 单次稳定性检查最多提取多少个硬材质 cell。超过后视为大型地形/建筑，保持静态。
const FLOATING_COMPONENT_CELL_LIMIT: usize = 4096;
const FREE_OBJECT_DT: f32 = 1.0 / 60.0;
const FREE_OBJECT_GRAVITY: f32 = -32.0;
const FREE_OBJECT_TERMINAL_VELOCITY: f32 = -78.0;
/// 软材质颗粒斜滑时的水平初速（m/s）。越大滑得越远、越"流动"。
const GRAIN_SLIDE_SPEED: f32 = 3.0;
/// 颗粒离地飞行时的水平阻尼，避免斜滑后横向漂移过远。
const GRAIN_AIR_HORIZONTAL_DAMPING: f32 = 0.8;
/// 同时活跃的软材质颗粒上限。超过后新的不稳定 cell 回退到瞬间松弛。
const MAX_ACTIVE_GRAINS: usize = 128;
/// 单 tick 最多提取多少个新颗粒，平滑突发坍塌的 CPU / 协议峰值。
const GRAIN_SPAWN_BUDGET_PER_TICK: usize = 32;
/// 单 tick 最多检查多少个不稳定候选 cell。
const UNSTABLE_VISIT_BUDGET_PER_TICK: usize = 256;

/// 挖放后需要重新做软材质稳定性判定的邻域偏移（cell 自身 + 上方 + 四侧 + 四斜上）。
const RELAX_REGION_OFFSETS: [(i32, i32, i32); 10] = [
    (0, 0, 0),
    (0, 1, 0),
    (1, 0, 0),
    (-1, 0, 0),
    (0, 0, 1),
    (0, 0, -1),
    (1, 1, 0),
    (-1, 1, 0),
    (0, 1, 1),
    (0, 1, -1),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FreeObjectSpawn {
    pub object_id: voxweb_core::ObjectID,
    pub cells: Vec<(Position, MaterialCell)>,
}

/// `step_soft_grains` 一个 tick 的产物：新提取的颗粒 + 超预算时瞬间松弛的 FieldDelta。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SoftStepEvents {
    pub spawns: Vec<FreeObjectSpawn>,
    pub updates: Vec<(Position, MaterialCell)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FreeObjectStateUpdate {
    pub object_id: voxweb_core::ObjectID,
    pub position: Vec3,
    pub velocity: Vec3,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FreeObjectTickEvents {
    pub states: Vec<FreeObjectStateUpdate>,
    pub projections: Vec<FreeObjectProjection>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FreeObjectProjection {
    pub object_id: voxweb_core::ObjectID,
    pub deltas: Vec<(Position, MaterialCell)>,
}

/// 验证一次挖掘操作。
/// `player_feet` 是玩家脚底世界坐标；眼睛位置 = `player_feet + Y * PLAYER_EYE_OFFSET`。
pub fn validate_break(world: &World, pos: Position, player_feet: Vec3) -> AckReason {
    if pos.y < 0 || pos.y >= CHUNK_Y as i32 {
        return AckReason::OutOfRange;
    }
    if distance_to_block_center(player_feet, pos) > MAX_REACH {
        return AckReason::OutOfRange;
    }
    if pos.y == 0 {
        return AckReason::BlockNotEmpty;
    }
    let cell = world.get_cell(pos);
    if cell.is_empty() {
        // 试图挖空气：复用 BlockNotEmpty 语义表达 "目标方块状态不允许操作"
        return AckReason::BlockNotEmpty;
    }
    if !properties(cell.primary).breakable {
        return AckReason::BlockNotEmpty;
    }
    AckReason::Ok
}

/// 验证一次放置操作。
pub fn validate_place(
    world: &World,
    pos: Position,
    block: BlockID,
    player_feet: Vec3,
) -> AckReason {
    if pos.y < 0 || pos.y >= CHUNK_Y as i32 {
        return AckReason::OutOfRange;
    }
    if distance_to_block_center(player_feet, pos) > MAX_REACH {
        return AckReason::OutOfRange;
    }
    if pos.y == 0 {
        return AckReason::BlockNotEmpty;
    }
    if block == BlockID::AIR || !properties(block).appears_in_hotbar {
        return AckReason::BlockNotEmpty;
    }
    if !world.get_cell(pos).is_empty() {
        return AckReason::BlockNotEmpty;
    }
    let target_aabb = Aabb::block_at(pos);
    if world
        .dynamic_object_aabbs()
        .iter()
        .any(|dynamic_aabb| target_aabb.intersects(dynamic_aabb))
    {
        return AckReason::BlockNotEmpty;
    }
    if player_aabb(player_feet).intersects(&Aabb::block_at(pos)) {
        return AckReason::Overlap;
    }
    AckReason::Ok
}

/// 玩家眼睛到方块中心的欧氏距离（单位：方块/米）。
fn distance_to_block_center(player_feet: Vec3, pos: Position) -> f32 {
    let eye = player_feet + Vec3::Y * PLAYER_EYE_OFFSET;
    let block_center = Vec3::new(pos.x as f32 + 0.5, pos.y as f32 + 0.5, pos.z as f32 + 0.5);
    (block_center - eye).length()
}

/// 挖放后立即运行一小段局部颗粒松弛。
///
/// `ImmediateRelaxation` 材质会优先竖直下落，受阻后尝试向斜下方滑落。
/// 返回值是需要广播给客户端的权威 `FieldDelta` 序列。
pub fn relax_after_edit(world: &mut World, origin: Position) -> Vec<(Position, MaterialCell)> {
    let mut updates = Vec::new();
    let mut queue = VecDeque::new();

    enqueue_relaxation_region(&mut queue, origin);

    let mut visited = 0usize;
    let mut moves = 0usize;
    while let Some(pos) = queue.pop_front() {
        if visited >= RELAXATION_VISIT_BUDGET || moves >= RELAXATION_MOVE_BUDGET {
            break;
        }
        visited += 1;

        let Some((from, to, cell)) = try_relax_one(world, pos) else {
            continue;
        };
        moves += 1;
        world.set_cell(from, MaterialCell::EMPTY);
        world.set_cell(to, cell);
        updates.push((from, MaterialCell::EMPTY));
        updates.push((to, cell));

        enqueue_relaxation_region(&mut queue, from);
        enqueue_relaxation_region(&mut queue, to);
    }

    updates
}

/// 挖放后把编辑点邻域的软材质 cell 塞进 `world.unstable_soft` 队列。
/// 真正是否下落、何时提取成颗粒由 `step_soft_grains` 每 tick 判定，从而形成可见的级联坍塌。
pub fn mark_edit_unstable(world: &mut World, origin: Position) {
    for (dx, dy, dz) in RELAX_REGION_OFFSETS {
        enqueue_if_soft(
            world,
            Position::new(origin.x + dx, origin.y + dy, origin.z + dz),
        );
    }
}

/// 每 tick 推进软材质颗粒级联：从 `unstable_soft` 队列取候选，
/// 不稳定的软材质 cell 在预算内提取为 active grain（后续由 `tick_free_objects` 下落），
/// 超预算 / 超上限时回退到瞬间松弛一步以保证收敛。
pub fn step_soft_grains(world: &mut World) -> SoftStepEvents {
    let mut events = SoftStepEvents::default();
    let mut visited = 0usize;
    let mut spawned = 0usize;
    let mut refs_dirty = false;

    while visited < UNSTABLE_VISIT_BUDGET_PER_TICK {
        let Some(pos) = world.next_unstable() else {
            break;
        };
        visited += 1;

        if !is_unstable_soft(world, pos) {
            continue;
        }

        let can_spawn =
            spawned < GRAIN_SPAWN_BUDGET_PER_TICK && world.active_grain_count() < MAX_ACTIVE_GRAINS;
        if can_spawn {
            let cell = world.get_cell(pos);
            world.set_cell(pos, MaterialCell::EMPTY);
            if let Some(object_id) = world.spawn_grain(pos, cell) {
                spawned += 1;
                refs_dirty = true;
                events.spawns.push(FreeObjectSpawn {
                    object_id,
                    cells: vec![(pos, cell)],
                });
                wake_support_above(world, pos);
            } else {
                // 单 cell 理论上不会失败；万一失败则还原，避免丢方块。
                world.set_cell(pos, cell);
            }
        } else if let Some((from, to, cell)) = try_relax_one(world, pos) {
            // 兜底：超预算/超上限，瞬间松弛一步并广播 FieldDelta。
            world.set_cell(from, MaterialCell::EMPTY);
            world.set_cell(to, cell);
            events.updates.push((from, MaterialCell::EMPTY));
            events.updates.push((to, cell));
            wake_support_above(world, from);
            enqueue_if_soft(world, to);
        }
    }

    if refs_dirty {
        world.rebuild_free_object_refs();
    }
    events
}

/// cell 是否为可下落/斜滑的软材质：复用 `try_relax_one` 的落点判定。
fn is_unstable_soft(world: &World, pos: Position) -> bool {
    try_relax_one(world, pos).is_some()
}

/// 把 `pos` 上方及四斜上方的软材质 cell 唤醒入队（它们可能因 `pos` 空出而失去支撑）。
fn wake_support_above(world: &mut World, pos: Position) {
    for (dx, dz) in [(0, 0), (1, 0), (-1, 0), (0, 1), (0, -1)] {
        enqueue_if_soft(world, Position::new(pos.x + dx, pos.y + 1, pos.z + dz));
    }
}

/// 仅当 `pos` 是已加载、非空、`ImmediateRelaxation` 软材质时入队，避免队列被硬材质/空气膨胀。
fn enqueue_if_soft(world: &mut World, pos: Position) {
    if pos.y <= 1 || pos.y >= CHUNK_Y as i32 || !chunk_loaded(world, pos) {
        return;
    }
    let cell = world.get_cell(pos);
    if cell.is_empty() {
        return;
    }
    if properties(cell.primary).stability != StabilityPolicy::ImmediateRelaxation {
        return;
    }
    world.enqueue_unstable(pos);
}
/// 就从静态场提取为 active FreeObject，后续由 tick 中的 AABB 动态体推进。
pub fn resolve_floating_after_edit(world: &mut World, origin: Position) -> Vec<FreeObjectSpawn> {
    let mut spawns = Vec::new();
    let mut checked = HashSet::new();

    for candidate in floating_candidates(origin) {
        if checked.contains(&candidate) || !chunk_loaded(world, candidate) {
            continue;
        }
        let cell = world.get_cell(candidate);
        if properties(cell.primary).stability != StabilityPolicy::FloatingOnly {
            continue;
        }
        let Some(component) = collect_floating_component(world, candidate, &mut checked) else {
            continue;
        };
        if component_is_supported(world, &component) {
            continue;
        }

        let Some(object_id) = world.spawn_dynamic_free_object(&component) else {
            continue;
        };
        for (pos, _) in &component {
            world.set_cell(*pos, MaterialCell::EMPTY);
        }
        spawns.push(FreeObjectSpawn {
            object_id,
            cells: component,
        });
    }

    spawns
}

pub fn tick_free_objects(world: &mut World) -> FreeObjectTickEvents {
    let ids = world
        .free_objects
        .iter()
        .filter_map(|(id, object)| (object.state == FreeObjectState::Dynamic).then_some(*id))
        .collect::<Vec<_>>();
    let mut events = FreeObjectTickEvents::default();
    let mut refs_dirty = false;

    for id in ids {
        let Some(object) = world.free_objects.get(&id).cloned() else {
            continue;
        };
        let dirty = if object.granular {
            tick_grain(world, id, object, &mut events)
        } else {
            tick_rigid_object(world, id, object, &mut events)
        };
        refs_dirty |= dirty;
    }

    if refs_dirty {
        world.rebuild_free_object_refs();
    }

    events
}

/// 硬材质刚性对象：竖直自由落体，碰撞后二分求落点并整体投影回场。行为与旧实现一致。
fn tick_rigid_object(
    world: &mut World,
    id: voxweb_core::ObjectID,
    mut object: voxweb_core::FreeObject,
    events: &mut FreeObjectTickEvents,
) -> bool {
    object.velocity.y = (object.velocity.y + FREE_OBJECT_GRAVITY * FREE_OBJECT_DT)
        .max(FREE_OBJECT_TERMINAL_VELOCITY);
    let next_position = object.transform.position + object.velocity * FREE_OBJECT_DT;

    if static_collides_with_object(world, &object, next_position) {
        let settled_position =
            settle_position(world, &object, object.transform.position, next_position);
        let Some(project_position) = find_projectable_position(world, &object, settled_position)
        else {
            if let Some(stored) = world.free_objects.get_mut(&id) {
                stored.velocity = Vec3::ZERO;
                stored.transform.position = object.transform.position;
                events.states.push(FreeObjectStateUpdate {
                    object_id: id,
                    position: stored.transform.position,
                    velocity: stored.velocity,
                });
            }
            return false;
        };
        let cells = object.cells_at_position(project_position);
        let mut deltas = Vec::with_capacity(cells.len());
        for (pos, cell) in cells {
            world.set_cell(pos, cell);
            deltas.push((pos, cell));
        }
        world.free_objects.remove(&id);
        events.projections.push(FreeObjectProjection {
            object_id: id,
            deltas,
        });
        return true;
    }

    if let Some(stored) = world.free_objects.get_mut(&id) {
        stored.velocity = object.velocity;
        stored.transform.position = next_position;
        events.states.push(FreeObjectStateUpdate {
            object_id: id,
            position: next_position,
            velocity: object.velocity,
        });
        return true;
    }
    false
}

/// 软材质颗粒：真实自由落体 + 落到边缘时朝下坡方向抛物线斜滑；静止后单格投影回场。
///
/// 分轴推进（先 Y 再 X/Z）：落地后若旁边存在更低的空格，就给一个朝该方向的水平初速，
/// 颗粒沿顶面爬到边缘后在重力下抛物线滑入低处——这就是"斜滑摊平成堆"的可见过程。
fn tick_grain(
    world: &mut World,
    id: voxweb_core::ObjectID,
    mut object: voxweb_core::FreeObject,
    events: &mut FreeObjectTickEvents,
) -> bool {
    let dt = FREE_OBJECT_DT;
    object.velocity.y =
        (object.velocity.y + FREE_OBJECT_GRAVITY * dt).max(FREE_OBJECT_TERMINAL_VELOCITY);

    let mut pos = object.transform.position;

    // —— Y 轴：竖直自由落体，碰撞则吸附到落面 ——
    let y_target = Vec3::new(pos.x, pos.y + object.velocity.y * dt, pos.z);
    let grounded = if static_collides_with_object(world, &object, y_target) {
        pos = settle_position(world, &object, pos, y_target);
        object.velocity.y = 0.0;
        true
    } else {
        pos.y = y_target.y;
        false
    };
    object.transform.position = pos;

    // —— 水平：落地时朝下坡方向给初速；离地时阻尼收敛，避免横向漂移过远 ——
    if grounded {
        let cell = grain_cell(pos);
        // 已在滑动且当前方向仍是有效下坡就保持，避免相邻 cell 间方向翻转导致来回抖动、永不静止。
        let keep = dominant_horizontal_dir(object.velocity)
            .filter(|&(dx, dz)| is_downhill_dir(world, cell, dx, dz));
        match keep.or_else(|| grain_downhill_dir(world, &object, pos)) {
            Some((dx, dz)) => {
                object.velocity.x = dx as f32 * GRAIN_SLIDE_SPEED;
                object.velocity.z = dz as f32 * GRAIN_SLIDE_SPEED;
            }
            None => {
                object.velocity.x = 0.0;
                object.velocity.z = 0.0;
            }
        }
    } else {
        object.velocity.x *= GRAIN_AIR_HORIZONTAL_DAMPING;
        object.velocity.z *= GRAIN_AIR_HORIZONTAL_DAMPING;
        if object.velocity.x.abs() < 0.01 {
            object.velocity.x = 0.0;
        }
        if object.velocity.z.abs() < 0.01 {
            object.velocity.z = 0.0;
        }
    }

    // —— X / Z 轴：分轴推进，撞静态场则该轴归零 ——
    if object.velocity.x != 0.0 {
        let x_target = Vec3::new(pos.x + object.velocity.x * dt, pos.y, pos.z);
        if static_collides_with_object(world, &object, x_target) {
            object.velocity.x = 0.0;
        } else {
            pos.x = x_target.x;
        }
    }
    if object.velocity.z != 0.0 {
        let z_target = Vec3::new(pos.x, pos.y, pos.z + object.velocity.z * dt);
        if static_collides_with_object(world, &object, z_target) {
            object.velocity.z = 0.0;
        } else {
            pos.z = z_target.z;
        }
    }
    object.transform.position = pos;

    // —— 静止判定：落地 + 无水平速度 → 单格投影回场，并唤醒上方邻居继续级联 ——
    // 落点被占（另一个颗粒先落）时 find_projectable_position 返回 None：悬停，下 tick 再试。
    let at_rest = grounded && object.velocity.x == 0.0 && object.velocity.z == 0.0;
    if at_rest && let Some(project_position) = find_projectable_position(world, &object, pos) {
        let cells = object.cells_at_position(project_position);
        let mut deltas = Vec::with_capacity(cells.len());
        for (cell_pos, cell) in cells {
            world.set_cell(cell_pos, cell);
            deltas.push((cell_pos, cell));
            wake_support_above(world, cell_pos);
        }
        world.free_objects.remove(&id);
        events.projections.push(FreeObjectProjection {
            object_id: id,
            deltas,
        });
        return true;
    }

    if let Some(stored) = world.free_objects.get_mut(&id) {
        stored.velocity = object.velocity;
        stored.transform.position = object.transform.position;
        events.states.push(FreeObjectStateUpdate {
            object_id: id,
            position: object.transform.position,
            velocity: object.velocity,
        });
        return true;
    }
    false
}

/// 颗粒落地后可斜滑的方向：旁边同高为空、且斜下方可容纳。无则返回 None（原地静止）。
fn grain_downhill_dir(
    world: &World,
    object: &voxweb_core::FreeObject,
    pos: Vec3,
) -> Option<(i32, i32)> {
    let cell = grain_cell(pos);
    let material = object
        .samples
        .first()
        .map(|sample| sample.cell().primary)
        .unwrap_or(BlockID::AIR);
    ordered_slide_dirs(cell, material)
        .into_iter()
        .find(|&(dx, dz)| is_downhill_dir(world, cell, dx, dz))
}

/// 从 `cell` 朝 (dx,dz) 是否可斜滑：同高侧格为空、斜下方格可容纳。
fn is_downhill_dir(world: &World, cell: Position, dx: i32, dz: i32) -> bool {
    let side = Position::new(cell.x + dx, cell.y, cell.z + dz);
    let target = Position::new(cell.x + dx, cell.y - 1, cell.z + dz);
    can_receive_granular(world, side) && can_receive_granular(world, target)
}

/// 颗粒当前所在的整数 cell。
fn grain_cell(pos: Vec3) -> Position {
    Position::new(
        pos.x.round() as i32,
        pos.y.round() as i32,
        pos.z.round() as i32,
    )
}

/// 从速度提取主导水平方向（用于保持滑动方向一致，避免抖动）。
fn dominant_horizontal_dir(velocity: Vec3) -> Option<(i32, i32)> {
    let (ax, az) = (velocity.x.abs(), velocity.z.abs());
    if ax < 0.01 && az < 0.01 {
        None
    } else if ax >= az {
        Some((velocity.x.signum() as i32, 0))
    } else {
        Some((0, velocity.z.signum() as i32))
    }
}

fn floating_candidates(origin: Position) -> [Position; 11] {
    [
        origin,
        Position::new(origin.x, origin.y + 1, origin.z),
        Position::new(origin.x, origin.y - 1, origin.z),
        Position::new(origin.x + 1, origin.y, origin.z),
        Position::new(origin.x - 1, origin.y, origin.z),
        Position::new(origin.x, origin.y, origin.z + 1),
        Position::new(origin.x, origin.y, origin.z - 1),
        Position::new(origin.x + 1, origin.y + 1, origin.z),
        Position::new(origin.x - 1, origin.y + 1, origin.z),
        Position::new(origin.x, origin.y + 1, origin.z + 1),
        Position::new(origin.x, origin.y + 1, origin.z - 1),
    ]
}

fn collect_floating_component(
    world: &World,
    start: Position,
    checked: &mut HashSet<Position>,
) -> Option<Vec<(Position, MaterialCell)>> {
    let mut component = Vec::new();
    let mut queue = VecDeque::from([start]);

    while let Some(pos) = queue.pop_front() {
        if !checked.insert(pos) {
            continue;
        }
        if !chunk_loaded(world, pos) {
            continue;
        }
        let cell = world.get_cell(pos);
        if properties(cell.primary).stability != StabilityPolicy::FloatingOnly {
            continue;
        }
        component.push((pos, cell));
        if component.len() > FLOATING_COMPONENT_CELL_LIMIT {
            return None;
        }
        for neighbor in six_neighbors(pos) {
            if !checked.contains(&neighbor) {
                queue.push_back(neighbor);
            }
        }
    }

    Some(component)
}

fn component_is_supported(world: &World, component: &[(Position, MaterialCell)]) -> bool {
    let positions = component
        .iter()
        .map(|(pos, _)| *pos)
        .collect::<HashSet<_>>();
    component.iter().any(|(pos, _)| {
        six_neighbors(*pos).into_iter().any(|neighbor| {
            if positions.contains(&neighbor) || neighbor.y < 0 || neighbor.y >= CHUNK_Y as i32 {
                return false;
            }
            let cell = world.get_cell(neighbor);
            !cell.is_empty() && properties(cell.primary).solid
        })
    })
}

fn six_neighbors(pos: Position) -> [Position; 6] {
    [
        Position::new(pos.x + 1, pos.y, pos.z),
        Position::new(pos.x - 1, pos.y, pos.z),
        Position::new(pos.x, pos.y + 1, pos.z),
        Position::new(pos.x, pos.y - 1, pos.z),
        Position::new(pos.x, pos.y, pos.z + 1),
        Position::new(pos.x, pos.y, pos.z - 1),
    ]
}

fn static_collides_with_object(
    world: &World,
    object: &voxweb_core::FreeObject,
    position: Vec3,
) -> bool {
    let aabb = object.aabb_at(position);
    let min_x = aabb.min.x.floor() as i32;
    let max_x = (aabb.max.x - f32::EPSILON).floor() as i32;
    let min_y = aabb.min.y.floor() as i32;
    let max_y = (aabb.max.y - f32::EPSILON).floor() as i32;
    let min_z = aabb.min.z.floor() as i32;
    let max_z = (aabb.max.z - f32::EPSILON).floor() as i32;
    for x in min_x..=max_x {
        for y in min_y..=max_y {
            for z in min_z..=max_z {
                if y <= 0 || y >= CHUNK_Y as i32 {
                    return true;
                }
                let cell = world.get_cell(Position::new(x, y, z));
                if !cell.is_empty() && properties(cell.primary).solid {
                    return true;
                }
            }
        }
    }
    false
}

fn settle_position(
    world: &World,
    object: &voxweb_core::FreeObject,
    start: Vec3,
    blocked: Vec3,
) -> Vec3 {
    let mut lo = blocked.y;
    let mut hi = start.y;
    for _ in 0..10 {
        let mid = (lo + hi) * 0.5;
        let pos = Vec3::new(start.x, mid, start.z);
        if static_collides_with_object(world, object, pos) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Vec3::new(start.x, hi.round(), start.z)
}

fn find_projectable_position(
    world: &World,
    object: &voxweb_core::FreeObject,
    settled: Vec3,
) -> Option<Vec3> {
    let rounded = Vec3::new(settled.x.round(), settled.y.round(), settled.z.round());
    for dy in [0.0, 1.0, -1.0, 2.0, -2.0, 3.0] {
        let candidate = rounded + Vec3::Y * dy;
        if projection_cells_clear(world, object, candidate)
            && !static_collides_with_object(world, object, candidate)
        {
            return Some(candidate);
        }
    }
    None
}

fn projection_cells_clear(world: &World, object: &voxweb_core::FreeObject, position: Vec3) -> bool {
    object
        .cells_at_position(position)
        .into_iter()
        .all(|(pos, _)| {
            pos.y > 0
                && pos.y < CHUNK_Y as i32
                && chunk_loaded(world, pos)
                && world.get_cell(pos).is_empty()
        })
}

fn enqueue_relaxation_region(queue: &mut VecDeque<Position>, origin: Position) {
    for (dx, dy, dz) in RELAX_REGION_OFFSETS {
        queue.push_back(Position::new(origin.x + dx, origin.y + dy, origin.z + dz));
    }
}

fn try_relax_one(world: &World, pos: Position) -> Option<(Position, Position, MaterialCell)> {
    if pos.y <= 1 || pos.y >= CHUNK_Y as i32 {
        return None;
    }
    if !chunk_loaded(world, pos) {
        return None;
    }

    let cell = world.get_cell(pos);
    if properties(cell.primary).stability != StabilityPolicy::ImmediateRelaxation {
        return None;
    }

    let down = Position::new(pos.x, pos.y - 1, pos.z);
    if can_receive_granular(world, down) {
        return Some((pos, down, cell));
    }

    for (dx, dz) in ordered_slide_dirs(pos, cell.primary) {
        let side = Position::new(pos.x + dx, pos.y, pos.z + dz);
        let target = Position::new(pos.x + dx, pos.y - 1, pos.z + dz);
        if can_receive_granular(world, side) && can_receive_granular(world, target) {
            return Some((pos, target, cell));
        }
    }

    None
}

fn can_receive_granular(world: &World, pos: Position) -> bool {
    pos.y > 0
        && pos.y < CHUNK_Y as i32
        && chunk_loaded(world, pos)
        && world.get_cell(pos).is_empty()
}

fn chunk_loaded(world: &World, pos: Position) -> bool {
    world.chunks.contains_key(&pos.to_chunk_pos())
}

fn ordered_slide_dirs(pos: Position, block: BlockID) -> [(i32, i32); 4] {
    const DIRS: [(i32, i32); 4] = [(1, 0), (-1, 0), (0, 1), (0, -1)];
    let start = ((pos.x as u32)
        .wrapping_mul(17)
        .wrapping_add((pos.z as u32).wrapping_mul(31))
        .wrapping_add(block.0 as u32)
        % 4) as usize;
    [
        DIRS[start],
        DIRS[(start + 1) % 4],
        DIRS[(start + 2) % 4],
        DIRS[(start + 3) % 4],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use voxweb_core::chunk::ChunkPos;

    /// 构造一个 chunk(0,0) 全 STONE、其它 chunks 空的 world。
    fn world_with_stone_chunk() -> World {
        let mut w = World::new(0);
        w.ensure_chunk_generated(ChunkPos::new(0, 0));
        // 用 STONE 覆盖整个 (0..16, 64, 0..16) 平面，方便测试
        for x in 0..16 {
            for z in 0..16 {
                w.set_block(Position::new(x, 64, z), BlockID::STONE);
            }
        }
        // 同时把上方一格清空，便于放置测试
        for x in 0..16 {
            for z in 0..16 {
                w.set_block(Position::new(x, 65, z), BlockID::AIR);
            }
        }
        w
    }

    #[test]
    fn break_out_of_y_range() {
        let w = World::new(0);
        let player = Vec3::new(0.0, 64.0, 0.0);
        assert_eq!(
            validate_break(&w, Position::new(0, -1, 0), player),
            AckReason::OutOfRange
        );
        assert_eq!(
            validate_break(&w, Position::new(0, CHUNK_Y as i32, 0), player),
            AckReason::OutOfRange
        );
    }

    #[test]
    fn break_air_returns_block_not_empty() {
        let w = world_with_stone_chunk();
        let player = Vec3::new(3.0, 65.0, 3.0);
        // (3, 65, 3) 是 AIR
        assert_eq!(
            validate_break(&w, Position::new(3, 65, 3), player),
            AckReason::BlockNotEmpty
        );
    }

    #[test]
    fn break_out_of_reach() {
        let w = world_with_stone_chunk();
        let player = Vec3::new(0.5, 65.0, 0.5);
        // (12, 64, 12) 距玩家约 sqrt(11.5^2 + (-0.6)^2 + 11.5^2) ≈ 16.3m
        assert_eq!(
            validate_break(&w, Position::new(12, 64, 12), player),
            AckReason::OutOfRange
        );
    }

    #[test]
    fn break_in_reach_returns_ok() {
        let w = world_with_stone_chunk();
        let player = Vec3::new(3.5, 65.0, 3.5);
        // (3, 64, 3) 距玩家眼睛约 sqrt(0^2 + 2.12^2 + 0^2) ≈ 2.12m
        assert_eq!(
            validate_break(&w, Position::new(3, 64, 3), player),
            AckReason::Ok
        );
    }

    #[test]
    fn break_bedrock_is_rejected() {
        let mut w = World::new(0);
        w.ensure_chunk_generated(ChunkPos::new(0, 0));
        w.set_block(Position::new(3, 64, 3), BlockID::BEDROCK);
        let player = Vec3::new(3.5, 65.0, 3.5);
        assert_eq!(
            validate_break(&w, Position::new(3, 64, 3), player),
            AckReason::BlockNotEmpty
        );
    }

    #[test]
    fn break_bottom_layer_is_rejected_even_when_manually_set_to_stone() {
        let mut w = World::new(0);
        w.ensure_chunk_generated(ChunkPos::new(0, 0));
        w.set_block(Position::new(3, 0, 3), BlockID::STONE);
        let player = Vec3::new(3.5, 1.0, 3.5);
        assert_eq!(
            validate_break(&w, Position::new(3, 0, 3), player),
            AckReason::BlockNotEmpty
        );
    }

    #[test]
    fn place_out_of_y_range() {
        let w = World::new(0);
        let player = Vec3::new(0.0, 64.0, 0.0);
        assert_eq!(
            validate_place(&w, Position::new(0, 300, 0), BlockID::STONE, player),
            AckReason::OutOfRange
        );
    }

    #[test]
    fn place_on_existing_block() {
        let w = world_with_stone_chunk();
        let player = Vec3::new(3.5, 65.0, 3.5);
        // (3, 64, 3) 已是 STONE
        assert_eq!(
            validate_place(&w, Position::new(3, 64, 3), BlockID::DIRT, player),
            AckReason::BlockNotEmpty
        );
    }

    #[test]
    fn place_in_air_within_reach_ok() {
        let w = world_with_stone_chunk();
        let player = Vec3::new(3.5, 65.0, 3.5);
        // (5, 65, 3) 是 AIR、距玩家约 1.6m、与玩家 AABB（x ∈ [3.2, 3.8]）不重叠
        assert_eq!(
            validate_place(&w, Position::new(5, 65, 3), BlockID::STONE, player),
            AckReason::Ok
        );
    }

    #[test]
    fn place_non_hotbar_material_is_rejected() {
        let w = world_with_stone_chunk();
        let player = Vec3::new(3.5, 65.0, 3.5);
        assert_eq!(
            validate_place(&w, Position::new(5, 65, 3), BlockID::BEDROCK, player),
            AckReason::BlockNotEmpty
        );
    }

    #[test]
    fn place_on_bottom_layer_is_rejected() {
        let w = World::new(0);
        let player = Vec3::new(3.5, 1.0, 3.5);
        assert_eq!(
            validate_place(&w, Position::new(3, 0, 3), BlockID::STONE, player),
            AckReason::BlockNotEmpty
        );
    }

    #[test]
    fn place_overlapping_player_is_rejected() {
        let w = world_with_stone_chunk();
        let player = Vec3::new(3.5, 65.0, 3.5);
        // (3, 65, 3) 是 AIR 且就在玩家脚底位置 → 重叠
        assert_eq!(
            validate_place(&w, Position::new(3, 65, 3), BlockID::STONE, player),
            AckReason::Overlap
        );
    }

    #[test]
    fn sand_falls_to_nearest_support_after_edit() {
        let mut w = world_with_stone_chunk();
        let x = 4;
        let z = 4;
        for y in 61..=70 {
            w.set_block(Position::new(x, y, z), BlockID::AIR);
        }
        for dx in -1..=1 {
            for dz in -1..=1 {
                w.set_block(Position::new(x + dx, 60, z + dz), BlockID::STONE);
            }
        }
        w.set_block(Position::new(x, 65, z), BlockID::SAND);

        let updates = relax_after_edit(&mut w, Position::new(x, 65, z));

        assert_eq!(w.get_block(Position::new(x, 65, z)), BlockID::AIR);
        assert_eq!(w.get_block(Position::new(x, 61, z)), BlockID::SAND);
        assert!(updates.contains(&(Position::new(x, 65, z), MaterialCell::EMPTY)));
        assert!(updates.contains(&(
            Position::new(x, 61, z),
            MaterialCell::from_block_id(BlockID::SAND)
        )));
    }

    #[test]
    fn granular_block_above_broken_cell_falls_down() {
        let mut w = world_with_stone_chunk();
        let x = 6;
        let z = 6;
        for dx in -1..=1 {
            for dz in -1..=1 {
                w.set_block(Position::new(x + dx, 60, z + dz), BlockID::STONE);
            }
        }
        w.set_block(Position::new(x, 61, z), BlockID::AIR);
        w.set_block(Position::new(x, 62, z), BlockID::DIRT);

        let updates = relax_after_edit(&mut w, Position::new(x, 61, z));

        assert_eq!(w.get_block(Position::new(x, 62, z)), BlockID::AIR);
        assert_eq!(w.get_block(Position::new(x, 61, z)), BlockID::DIRT);
        assert_eq!(updates.len(), 2);
    }

    #[test]
    fn granular_relaxation_preserves_material_cell() {
        let mut w = world_with_stone_chunk();
        let from = Position::new(7, 62, 7);
        let to = Position::new(7, 61, 7);
        for dx in -1..=1 {
            for dz in -1..=1 {
                w.set_block(Position::new(from.x + dx, 60, from.z + dz), BlockID::STONE);
            }
        }
        w.set_cell(to, MaterialCell::EMPTY);
        let cell = MaterialCell {
            occupancy: 190,
            primary: BlockID::SAND,
            secondary: Some(voxweb_core::block::MixSlot {
                material: BlockID::DIRT,
                occupancy: 30,
            }),
            flags: voxweb_core::block::CellFlags(voxweb_core::block::CellFlags::DIRTY),
        };
        w.set_cell(from, cell);

        let updates = relax_after_edit(&mut w, to);

        assert_eq!(w.get_cell(from), MaterialCell::EMPTY);
        assert_eq!(w.get_cell(to), cell);
        assert!(updates.contains(&(from, MaterialCell::EMPTY)));
        assert!(updates.contains(&(to, cell)));
    }

    #[test]
    fn granular_block_slides_diagonally_when_supported_below() {
        let mut w = world_with_stone_chunk();
        let from = Position::new(8, 65, 8);
        let target = ordered_slide_dirs(from, BlockID::SAND)
            .into_iter()
            .map(|(dx, dz)| Position::new(from.x + dx, from.y - 1, from.z + dz))
            .next()
            .unwrap();
        w.set_block(Position::new(from.x, from.y - 1, from.z), BlockID::STONE);
        w.set_block(from, BlockID::SAND);
        w.set_block(Position::new(target.x, from.y, target.z), BlockID::AIR);
        w.set_block(target, BlockID::AIR);
        w.set_block(
            Position::new(target.x, target.y - 1, target.z),
            BlockID::STONE,
        );
        for (dx, dz) in ordered_slide_dirs(target, BlockID::SAND) {
            w.set_block(
                Position::new(target.x + dx, target.y - 1, target.z + dz),
                BlockID::STONE,
            );
        }

        relax_after_edit(&mut w, from);

        assert_eq!(w.get_block(from), BlockID::AIR);
        assert_eq!(w.get_block(target), BlockID::SAND);
    }

    #[test]
    fn floating_hard_block_spawns_dynamic_object_then_projects() {
        let mut w = World::new(0);
        w.ensure_chunk_generated(ChunkPos::new(0, 0));
        clear_box(&mut w, 6..=10, 1..=8, 6..=10);
        let floating = Position::new(8, 5, 8);
        let settled = Position::new(8, 1, 8);
        w.set_block(floating, BlockID::STONE_BRICKS);

        let spawns = resolve_floating_after_edit(&mut w, floating);

        assert_eq!(w.get_block(floating), BlockID::AIR);
        assert_eq!(w.get_block(settled), BlockID::AIR);
        assert_eq!(spawns.len(), 1);
        assert_eq!(w.free_objects.len(), 1);
        let object = w.free_objects.values().next().unwrap();
        assert_eq!(object.samples.len(), 1);
        assert_eq!(object.state, voxweb_core::FreeObjectState::Dynamic);

        let mut projected = Vec::new();
        for _ in 0..90 {
            let events = tick_free_objects(&mut w);
            projected.extend(projection_deltas(&events.projections));
            if !projected.is_empty() {
                break;
            }
        }
        assert_eq!(w.get_block(settled), BlockID::STONE_BRICKS);
        assert!(projected.contains(&(settled, MaterialCell::from_block_id(BlockID::STONE_BRICKS))));
        assert!(w.free_objects.is_empty());
    }

    #[test]
    fn floating_hard_component_spawns_as_one_object() {
        let mut w = World::new(0);
        w.ensure_chunk_generated(ChunkPos::new(0, 0));
        clear_box(&mut w, 6..=11, 1..=8, 6..=10);
        let a = Position::new(8, 5, 8);
        let b = Position::new(9, 5, 8);
        w.set_block(a, BlockID::STONE_BRICKS);
        w.set_block(b, BlockID::WOOD);

        let spawns = resolve_floating_after_edit(&mut w, a);

        assert_eq!(w.get_block(a), BlockID::AIR);
        assert_eq!(w.get_block(b), BlockID::AIR);
        assert_eq!(w.get_block(Position::new(8, 1, 8)), BlockID::AIR);
        assert_eq!(w.get_block(Position::new(9, 1, 8)), BlockID::AIR);
        assert_eq!(spawns.len(), 1);
        assert_eq!(w.free_objects.len(), 1);
        assert_eq!(w.free_objects.values().next().unwrap().samples.len(), 2);
    }

    #[test]
    fn grounded_floating_only_component_stays_static() {
        let mut w = World::new(0);
        w.ensure_chunk_generated(ChunkPos::new(0, 0));
        clear_box(&mut w, 6..=10, 1..=8, 6..=10);
        let base = Position::new(8, 1, 8);
        let top = Position::new(8, 2, 8);
        w.set_block(base, BlockID::STONE_BRICKS);
        w.set_block(top, BlockID::STONE_BRICKS);

        let spawns = resolve_floating_after_edit(&mut w, top);

        assert!(spawns.is_empty());
        assert_eq!(w.get_block(base), BlockID::STONE_BRICKS);
        assert_eq!(w.get_block(top), BlockID::STONE_BRICKS);
        assert!(w.free_objects.is_empty());
    }

    fn clear_box(
        world: &mut World,
        xs: std::ops::RangeInclusive<i32>,
        ys: std::ops::RangeInclusive<i32>,
        zs: std::ops::RangeInclusive<i32>,
    ) {
        for x in xs.clone() {
            for y in ys.clone() {
                for z in zs.clone() {
                    world.set_block(Position::new(x, y, z), BlockID::AIR);
                }
            }
        }
    }

    /// 反复推进 step_soft_grains + tick_free_objects 直到没有活跃颗粒且队列清空，返回耗时 tick 数。
    fn run_soft_sim(world: &mut World, max_ticks: usize) -> usize {
        for tick in 0..max_ticks {
            if world.active_grain_count() == 0 && world.unstable_len() == 0 {
                return tick;
            }
            step_soft_grains(world);
            tick_free_objects(world);
        }
        max_ticks
    }

    #[test]
    fn unstable_soft_cell_extracts_to_grain_not_instant() {
        // 悬空沙子：mark + 一步 step_soft_grains 应提取成 active grain，而不是瞬间落定。
        let mut w = world_with_stone_chunk();
        let sand = Position::new(4, 66, 4);
        clear_box(&mut w, 4..=4, 65..=70, 4..=4); // 65 起留空，让沙子能落
        w.set_block(sand, BlockID::SAND);

        mark_edit_unstable(&mut w, sand);
        let events = step_soft_grains(&mut w);

        assert_eq!(events.spawns.len(), 1, "应提取出 1 个颗粒");
        assert!(events.updates.is_empty(), "预算内不应走瞬间松弛兜底");
        assert_eq!(w.get_block(sand), BlockID::AIR, "颗粒 cell 已从静态场移除");
        assert_eq!(w.active_grain_count(), 1);
        let grain = w.free_objects.values().next().unwrap();
        assert!(grain.granular);
        assert_eq!(grain.samples.len(), 1);
    }

    #[test]
    fn grain_falls_over_ticks_and_projects_to_support() {
        let mut w = world_with_stone_chunk(); // stone plane at y=64, air at 65
        let sand = Position::new(4, 68, 4);
        clear_box(&mut w, 4..=4, 65..=70, 4..=4);
        w.set_block(sand, BlockID::SAND);

        mark_edit_unstable(&mut w, sand);
        let ticks = run_soft_sim(&mut w, 600);

        assert!(ticks > 1, "应经过多帧下落而非瞬间到位，实际 {ticks} tick");
        assert!(ticks < 600, "应在上限内稳定");
        assert_eq!(w.get_block(sand), BlockID::AIR);
        assert_eq!(
            w.get_block(Position::new(4, 65, 4)),
            BlockID::SAND,
            "应落在石头顶上 y=65"
        );
        assert!(w.free_objects.is_empty());
    }

    #[test]
    fn grain_slides_off_edge_and_settles_lower() {
        // 沙子落在一个 1 宽石柱顶上，直下受阻但旁边是空的 → 抛物线滑落到低处。
        let mut w = World::new(0);
        w.ensure_chunk_generated(ChunkPos::new(0, 0));
        clear_box(&mut w, 2..=6, 1..=12, 2..=6);
        for x in 2..=6 {
            for z in 2..=6 {
                w.set_block(Position::new(x, 1, z), BlockID::STONE); // 地板 y=1
            }
        }
        // (4,2,4) 石柱，沙子放柱顶上方
        w.set_block(Position::new(4, 2, 4), BlockID::STONE);
        let sand = Position::new(4, 4, 4);
        w.set_block(sand, BlockID::SAND);

        mark_edit_unstable(&mut w, sand);
        let ticks = run_soft_sim(&mut w, 600);

        assert!(ticks < 600, "应稳定");
        assert_eq!(w.get_block(sand), BlockID::AIR, "沙子应离开原位");
        assert!(w.free_objects.is_empty());
        // 沙子应落在地板上（y=2），且偏离了原来的 x=4,z=4 柱心（滑到旁边）。
        let mut found = None;
        for x in 2..=6 {
            for z in 2..=6 {
                if w.get_block(Position::new(x, 2, z)) == BlockID::SAND {
                    found = Some((x, z));
                }
            }
        }
        let (fx, fz) = found.expect("沙子应落在地板层 y=2");
        assert!(fx != 4 || fz != 4, "应滑到柱心以外，实际落点 ({fx},{fz})");
    }

    #[test]
    fn floating_sand_column_cascades_and_conserves_mass() {
        // 悬空沙柱应逐格坍落并按 1:1 休止角摊平成堆：质量守恒、无悬空、最终稳定。
        // 用干净平地（四周全空），避免地形噪声制造额外斜滑机会干扰断言。
        let mut w = World::new(0);
        w.ensure_chunk_generated(ChunkPos::new(0, 0));
        clear_box(&mut w, 0..=15, 1..=40, 0..=15);
        for x in 0..16 {
            for z in 0..16 {
                w.set_block(Position::new(x, 1, z), BlockID::STONE); // 地板 y=1
            }
        }
        let column = [2, 3, 4, 5]; // (8, 2..=5, 8) 4 格沙柱，底格落在地板上
        for y in column {
            w.set_block(Position::new(8, y, 8), BlockID::SAND);
        }

        mark_edit_unstable(&mut w, Position::new(8, 2, 8));
        let ticks = run_soft_sim(&mut w, 2000);

        assert!(ticks > 1, "应经过多帧坍落而非瞬间");
        assert!(ticks < 2000, "应在上限内稳定");
        assert!(w.free_objects.is_empty(), "所有颗粒应落定");

        // 质量守恒：整片区域内沙子总数不变。
        let mut total = 0usize;
        let mut column_height = 0usize;
        for x in 0..16 {
            for y in 1..40 {
                for z in 0..16 {
                    if w.get_block(Position::new(x, y, z)) == BlockID::SAND {
                        total += 1;
                        if x == 8 && z == 8 {
                            column_height += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(total, column.len(), "沙子数量应守恒");
        // 底格落在地板上，且沙堆应摊开（不再是 4 高的单柱）。
        assert_eq!(
            w.get_block(Position::new(8, 2, 8)),
            BlockID::SAND,
            "柱底应留在地板上"
        );
        assert!(
            column_height < column.len(),
            "1 宽沙柱应摊平成堆，而不是保持原样"
        );
    }

    #[test]
    fn per_tick_spawn_budget_caps_grains_and_falls_back() {
        // 大片悬空沙板：单步 step 最多提取 GRAIN_SPAWN_BUDGET_PER_TICK 个颗粒，
        // 其余在访问预算内走瞬间松弛兜底（updates 非空）。
        let mut w = world_with_stone_chunk();
        let mut cells = Vec::new();
        for x in 0..15 {
            for z in 0..15 {
                let pos = Position::new(x, 66, z);
                clear_box(&mut w, x..=x, 65..=67, z..=z);
                w.set_block(pos, BlockID::SAND);
                cells.push(pos);
            }
        }
        for pos in &cells {
            w.enqueue_unstable(*pos);
        }

        let events = step_soft_grains(&mut w);

        assert_eq!(
            events.spawns.len(),
            GRAIN_SPAWN_BUDGET_PER_TICK,
            "单步提取应被 per-tick 预算限住"
        );
        assert_eq!(w.active_grain_count(), GRAIN_SPAWN_BUDGET_PER_TICK);
        assert!(!events.updates.is_empty(), "超预算的 cell 应走瞬间松弛兜底");
    }

    fn projection_deltas(projections: &[FreeObjectProjection]) -> Vec<(Position, MaterialCell)> {
        projections
            .iter()
            .flat_map(|projection| projection.deltas.iter().copied())
            .collect()
    }
}
