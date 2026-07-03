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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FreeObjectSpawn {
    pub object_id: voxweb_core::ObjectID,
    pub cells: Vec<(Position, MaterialCell)>,
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

/// 第一版硬材质稳定性：`FloatingOnly` 连通块如果完全没有接触任何稳定材质，
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
        let Some(mut object) = world.free_objects.get(&id).cloned() else {
            continue;
        };

        object.velocity.y = (object.velocity.y + FREE_OBJECT_GRAVITY * FREE_OBJECT_DT)
            .max(FREE_OBJECT_TERMINAL_VELOCITY);
        let next_position = object.transform.position + object.velocity * FREE_OBJECT_DT;

        if static_collides_with_object(world, &object, next_position) {
            let settled_position =
                settle_position(world, &object, object.transform.position, next_position);
            let Some(project_position) =
                find_projectable_position(world, &object, settled_position)
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
                continue;
            };
            let cells = object.cells_at_position(project_position);
            let mut deltas = Vec::with_capacity(cells.len());
            for (pos, cell) in cells {
                world.set_cell(pos, cell);
                deltas.push((pos, cell));
            }
            world.free_objects.remove(&id);
            refs_dirty = true;
            events.projections.push(FreeObjectProjection {
                object_id: id,
                deltas,
            });
            continue;
        }

        if let Some(stored) = world.free_objects.get_mut(&id) {
            stored.velocity = object.velocity;
            stored.transform.position = next_position;
            events.states.push(FreeObjectStateUpdate {
                object_id: id,
                position: next_position,
                velocity: object.velocity,
            });
            refs_dirty = true;
        }
    }

    if refs_dirty {
        world.rebuild_free_object_refs();
    }

    events
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
    for (dx, dy, dz) in [
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
    ] {
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

    fn projection_deltas(projections: &[FreeObjectProjection]) -> Vec<(Position, MaterialCell)> {
        projections
            .iter()
            .flat_map(|projection| projection.deltas.iter().copied())
            .collect()
    }
}
