//! 方块定义：BlockID 枚举 + BlockProperties 属性表。

use serde::{Deserialize, Serialize};

/// 方块标识，u16 保证可扩展至 65535 种方块。
/// 0 = 空气（AIR），不参与渲染和碰撞。
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct BlockID(pub u16);

impl BlockID {
    pub const AIR: BlockID = BlockID(0);
    pub const STONE: BlockID = BlockID(1);
    pub const GRASS: BlockID = BlockID(2);
    pub const DIRT: BlockID = BlockID(3);
    pub const WATER: BlockID = BlockID(4);
    pub const GLASS: BlockID = BlockID(5);
    pub const SAND: BlockID = BlockID(6);
    pub const WOOD: BlockID = BlockID(7);
    pub const LEAVES: BlockID = BlockID(8);

    /// 方块是否非空气（可用于碰撞和渲染判断）。
    pub fn is_solid(self) -> bool {
        if self.0 == 0 {
            return false;
        }
        properties(self).solid
    }
}

/// 方块的静态属性，用于碰撞检测、渲染 Pass 选择、纹理索引。
pub struct BlockProperties {
    /// 是否有碰撞体积
    pub solid: bool,
    /// 是否走 Transparent Pass（半透明 / 混合渲染）
    pub transparent: bool,
    /// （预留 v2）是否自身发光
    pub emits_light: bool,
    /// 纹理图集中的起始索引（顶点压缩时写入）
    pub texture_index: u8,
    /// 方块显示名
    pub display_name: &'static str,
}

/// 编译期常量属性表，按 BlockID.0 索引。
static PROPERTIES: &[BlockProperties] = &[
    // 0: AIR（不应被查询，占位用）
    BlockProperties {
        solid: false,
        transparent: true,
        emits_light: false,
        texture_index: 0,
        display_name: "空气",
    },
    // 1: STONE
    BlockProperties {
        solid: true,
        transparent: false,
        emits_light: false,
        texture_index: 1,
        display_name: "石头",
    },
    // 2: GRASS
    BlockProperties {
        solid: true,
        transparent: false,
        emits_light: false,
        texture_index: 2,
        display_name: "草",
    },
    // 3: DIRT
    BlockProperties {
        solid: true,
        transparent: false,
        emits_light: false,
        texture_index: 3,
        display_name: "泥土",
    },
    // 4: WATER
    BlockProperties {
        solid: false,
        transparent: true,
        emits_light: false,
        texture_index: 4,
        display_name: "水",
    },
    // 5: GLASS
    BlockProperties {
        solid: true,
        transparent: true,
        emits_light: false,
        texture_index: 5,
        display_name: "玻璃",
    },
    // 6: SAND
    BlockProperties {
        solid: true,
        transparent: false,
        emits_light: false,
        texture_index: 6,
        display_name: "沙子",
    },
    // 7: WOOD
    BlockProperties {
        solid: true,
        transparent: false,
        emits_light: false,
        texture_index: 7,
        display_name: "木头",
    },
    // 8: LEAVES
    BlockProperties {
        solid: true,
        transparent: false,
        emits_light: false,
        texture_index: 8,
        display_name: "树叶",
    },
];

/// 根据 BlockID 查询其属性。越界时返回 AIR 属性（容错）。
pub fn properties(id: BlockID) -> &'static BlockProperties {
    PROPERTIES.get(id.0 as usize).unwrap_or(&PROPERTIES[0])
}
