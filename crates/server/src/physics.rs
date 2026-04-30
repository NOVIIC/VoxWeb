//! 物理仲裁：服务端权威的碰撞检测、跳跃、挖放验证。
//!
//! Phase 3 实现完整物理。Phase 0 仅提供占位 API。

use voxweb_core::block::BlockID;
use voxweb_core::chunk::Position;

use super::world::World;

/// 验证一次挖掘操作是否合法。
/// 返回 true 表示允许挖掘。
pub fn validate_break(world: &World, pos: Position, player_pos: Position) -> bool {
    // Phase 3: 检查射程、目标方块非空
    let _ = (world, pos, player_pos);
    true
}

/// 验证一次放置操作是否合法。
/// 返回 true 表示允许放置。
pub fn validate_place(
    world: &World,
    pos: Position,
    block: BlockID,
    player_pos: Position,
) -> bool {
    // Phase 3: 检查射程、目标位置为空、不与玩家碰撞体积重叠
    let _ = (world, pos, block, player_pos);
    true
}

/// AABB 玩家碰撞参数。
pub struct PlayerAABB {
    pub width: f32,
    pub height: f32,
    pub eye_height: f32,
}

impl Default for PlayerAABB {
    fn default() -> Self {
        Self {
            width: 0.6,
            height: 1.8,
            eye_height: 1.65,
        }
    }
}
