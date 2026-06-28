//! 物理仲裁：服务端权威的挖放验证。
//!
//! Phase 3：检查射程（≤ MAX_REACH）、目标方块状态、放置位置不与玩家 AABB 重叠。
//! Phase 5 引入完整玩家表后，多人挖放的范围/重叠校验复用同一套函数。

use glam::Vec3;

use voxweb_core::block::{BlockID, properties};
use voxweb_core::chunk::{CHUNK_Y, Position};
use voxweb_core::geometry::{Aabb, PLAYER_EYE_OFFSET, player_aabb};
use voxweb_core::protocol::AckReason;

use super::world::World;

/// 玩家操作距离上限（眼睛到方块中心，单位：方块/米）。
pub const MAX_REACH: f32 = 6.0;

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
    fn break_bottom_layer_is_rejected_even_for_legacy_stone() {
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
}
