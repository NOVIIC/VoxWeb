//! 物理仲裁：服务端权威的挖放验证与局部材质松弛。
//!
//! Phase 3：检查射程（≤ MAX_REACH）、目标方块状态、放置位置不与玩家 AABB 重叠。
//! Phase 5 引入完整玩家表后，多人挖放的范围/重叠校验复用同一套函数。

use std::collections::VecDeque;

use glam::Vec3;

use voxweb_core::block::{BlockID, StabilityPolicy, properties};
use voxweb_core::chunk::{CHUNK_Y, Position};
use voxweb_core::geometry::{Aabb, PLAYER_EYE_OFFSET, player_aabb};
use voxweb_core::protocol::AckReason;

use super::world::World;

/// 玩家操作距离上限（眼睛到方块中心，单位：方块/米）。
pub const MAX_REACH: f32 = 6.0;
/// 单次挖放后最多执行多少个颗粒移动，避免一次编辑卡住主线程。
const RELAXATION_MOVE_BUDGET: usize = 64;
/// 单次挖放后最多检查多少个候选 cell，限制局部松弛范围。
const RELAXATION_VISIT_BUDGET: usize = 256;

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
    let block = world.get_block(pos);
    if block == BlockID::AIR {
        // 试图挖空气：复用 BlockNotEmpty 语义表达 "目标方块状态不允许操作"
        return AckReason::BlockNotEmpty;
    }
    if !properties(block).breakable {
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
    if world.get_block(pos) != BlockID::AIR {
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
/// 当前仍基于 `BlockID` dense chunk 做兼容实现：`ImmediateRelaxation` 材质会优先竖直下落，
/// 受阻后尝试向斜下方滑落。返回值是需要广播给客户端的权威 `FieldDelta` 序列。
pub fn relax_after_edit(world: &mut World, origin: Position) -> Vec<(Position, BlockID)> {
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

        let Some((from, to, block)) = try_relax_one(world, pos) else {
            continue;
        };
        moves += 1;
        world.set_block(from, BlockID::AIR);
        world.set_block(to, block);
        updates.push((from, BlockID::AIR));
        updates.push((to, block));

        enqueue_relaxation_region(&mut queue, from);
        enqueue_relaxation_region(&mut queue, to);
    }

    updates
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

fn try_relax_one(world: &World, pos: Position) -> Option<(Position, Position, BlockID)> {
    if pos.y <= 1 || pos.y >= CHUNK_Y as i32 {
        return None;
    }
    if !chunk_loaded(world, pos) {
        return None;
    }

    let block = world.get_block(pos);
    if properties(block).stability != StabilityPolicy::ImmediateRelaxation {
        return None;
    }

    let down = Position::new(pos.x, pos.y - 1, pos.z);
    if can_receive_granular(world, down) {
        return Some((pos, down, block));
    }

    for (dx, dz) in ordered_slide_dirs(pos, block) {
        let side = Position::new(pos.x + dx, pos.y, pos.z + dz);
        let target = Position::new(pos.x + dx, pos.y - 1, pos.z + dz);
        if can_receive_granular(world, side) && can_receive_granular(world, target) {
            return Some((pos, target, block));
        }
    }

    None
}

fn can_receive_granular(world: &World, pos: Position) -> bool {
    pos.y > 0
        && pos.y < CHUNK_Y as i32
        && chunk_loaded(world, pos)
        && world.get_block(pos) == BlockID::AIR
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
        assert!(updates.contains(&(Position::new(x, 65, z), BlockID::AIR)));
        assert!(updates.contains(&(Position::new(x, 61, z), BlockID::SAND)));
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
}
