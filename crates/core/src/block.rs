//! 方块/材质定义：BlockID 过渡层 + MaterialProperties 属性表。

use serde::{Deserialize, Serialize};

/// 方块标识，u16 保证可扩展至 65535 种方块。
/// 0 = 空气（AIR），不参与渲染和碰撞。
#[derive(Copy, Clone, Default, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct BlockID(pub u16);

/// 第一版统一体素方案中，MaterialID 暂时复用现有 BlockID 编号。
/// 这样网络协议、OPFS chunk 编码和渲染路径可以分阶段迁移。
pub type MaterialID = BlockID;

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
    pub const STONE_BRICKS: BlockID = BlockID(9);
    pub const BEDROCK: BlockID = BlockID(10);

    /// 方块是否非空气（可用于碰撞和渲染判断）。
    pub fn is_solid(self) -> bool {
        if self.0 == 0 {
            return false;
        }
        properties(self).solid
    }
}

/// 材质视觉路由。第一版渲染仍以 blocky mesh 为主，但属性已按新方案落表。
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum VisualClass {
    HardBlocky,
    SmoothGranular,
    Fluid,
    Foliage,
}

/// 材质模拟路由。
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum MechanicsClass {
    StaticSolid,
    Granular,
    RigidBreakable,
    Fluid,
    Decorative,
}

/// 稳定性策略。当前只执行 NoPhysics / 放置挖掘规则，完整 solver 后续接入。
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum StabilityPolicy {
    NoPhysics,
    FloatingOnly,
    ImmediateRelaxation,
    FutureFluid,
}

/// 材质放置 kernel（设计路由字段）；当前运行时未按 kernel 分派，放置一律走单格路径。
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum PlacementKernel {
    None,
    SingleCell,
    GranularLocal,
}

/// 材质挖掘 kernel（设计路由字段）；当前运行时未按 kernel 分派，挖掘一律走单格路径。
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum BreakKernel {
    None,
    SingleCell,
    GranularLocal,
}

/// MaterialCell 的 secondary 通道，用于后续少量混合材质。
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct MixSlot {
    pub material: MaterialID,
    pub occupancy: u8,
}

/// 每格的轻量标志位。保持为透明 newtype，避免依赖 bitflags serde 配置。
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Default, Serialize, Deserialize)]
pub struct CellFlags(pub u8);

impl CellFlags {
    pub const GENERATED: u8 = 1 << 0;
    pub const DIRTY: u8 = 1 << 1;
    pub const STABLE_HINT: u8 = 1 << 2;

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn contains(self, flag: u8) -> bool {
        self.0 & flag != 0
    }
}

/// 统一体素方案的权威 cell 原型。当前 Chunk 仍保存 BlockID；
/// 这个类型先服务协议/存档演进和后续 FieldChunk 原型。
#[derive(Copy, Clone, Default, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct MaterialCell {
    pub occupancy: u8,
    pub primary: MaterialID,
    pub secondary: Option<MixSlot>,
    pub flags: CellFlags,
}

impl MaterialCell {
    pub const EMPTY: MaterialCell = MaterialCell {
        occupancy: 0,
        primary: BlockID::AIR,
        secondary: None,
        flags: CellFlags::empty(),
    };

    pub const fn full(material: MaterialID) -> Self {
        Self {
            occupancy: u8::MAX,
            primary: material,
            secondary: None,
            flags: CellFlags::empty(),
        }
    }

    pub const fn is_empty(self) -> bool {
        self.occupancy == 0 || self.primary.0 == 0
    }

    pub const fn from_block_id(block: BlockID) -> Self {
        if block.0 == 0 {
            Self::EMPTY
        } else {
            Self::full(block)
        }
    }

    pub const fn to_block_id(self) -> BlockID {
        if self.is_empty() {
            BlockID::AIR
        } else {
            self.primary
        }
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
    /// 材质视觉路由
    pub visual_class: VisualClass,
    /// 材质模拟路由
    pub mechanics: MechanicsClass,
    /// 默认放置 kernel
    pub placement_kernel: PlacementKernel,
    /// 默认挖掘 kernel
    pub break_kernel: BreakKernel,
    /// 创造模式一次放置的质量单位
    pub placement_unit_mass: u16,
    /// 硬度，后续用于挖掘耗时/工具
    pub hardness: f32,
    /// 是否出现在创造模式快捷栏/材料表
    pub appears_in_hotbar: bool,
    /// 是否可被玩家挖掘
    pub breakable: bool,
    /// 休止角，颗粒材质后续使用
    pub angle_of_repose: f32,
    /// 凝聚力，结构/颗粒 solver 后续使用
    pub cohesion: f32,
    /// 抗压强度
    pub compressive_strength: f32,
    /// 抗剪强度
    pub shear_strength: f32,
    /// 密度
    pub density_kg_m3: f32,
    /// 弹性
    pub restitution: f32,
    /// 摩擦
    pub friction: f32,
    /// 稳定性策略
    pub stability: StabilityPolicy,
}

/// 统一体素文档中的 MaterialProperties 名称。当前与 BlockProperties 同源。
pub type MaterialProperties = BlockProperties;

/// 编译期 MaterialRegistry 过渡入口。
pub struct MaterialRegistry;

impl MaterialRegistry {
    pub fn properties(&self, id: MaterialID) -> &'static MaterialProperties {
        properties(id)
    }
}

pub static MATERIAL_REGISTRY: MaterialRegistry = MaterialRegistry;

/// 编译期常量属性表，按 BlockID.0 索引。
static PROPERTIES: &[BlockProperties] = &[
    // 0: AIR（不应被查询，占位用）
    BlockProperties {
        solid: false,
        transparent: true,
        emits_light: false,
        texture_index: 0,
        display_name: "空气",
        visual_class: VisualClass::HardBlocky,
        mechanics: MechanicsClass::Decorative,
        placement_kernel: PlacementKernel::None,
        break_kernel: BreakKernel::None,
        placement_unit_mass: 0,
        hardness: 0.0,
        appears_in_hotbar: false,
        breakable: false,
        angle_of_repose: 0.0,
        cohesion: 0.0,
        compressive_strength: 0.0,
        shear_strength: 0.0,
        density_kg_m3: 0.0,
        restitution: 0.0,
        friction: 0.0,
        stability: StabilityPolicy::NoPhysics,
    },
    // 1: STONE
    BlockProperties {
        solid: true,
        transparent: false,
        emits_light: false,
        texture_index: 1,
        display_name: "石头",
        visual_class: VisualClass::HardBlocky,
        mechanics: MechanicsClass::StaticSolid,
        placement_kernel: PlacementKernel::SingleCell,
        break_kernel: BreakKernel::SingleCell,
        placement_unit_mass: 255,
        hardness: 1.5,
        appears_in_hotbar: true,
        breakable: true,
        angle_of_repose: 90.0,
        cohesion: 0.95,
        compressive_strength: 8_000.0,
        shear_strength: 4_000.0,
        density_kg_m3: 2_600.0,
        restitution: 0.15,
        friction: 0.85,
        stability: StabilityPolicy::FloatingOnly,
    },
    // 2: GRASS
    BlockProperties {
        solid: true,
        transparent: false,
        emits_light: false,
        texture_index: 2,
        display_name: "草",
        visual_class: VisualClass::SmoothGranular,
        mechanics: MechanicsClass::Granular,
        placement_kernel: PlacementKernel::GranularLocal,
        break_kernel: BreakKernel::GranularLocal,
        placement_unit_mass: 255,
        hardness: 0.6,
        appears_in_hotbar: true,
        breakable: true,
        angle_of_repose: 42.0,
        cohesion: 0.45,
        compressive_strength: 900.0,
        shear_strength: 450.0,
        density_kg_m3: 1_400.0,
        restitution: 0.05,
        friction: 0.9,
        stability: StabilityPolicy::ImmediateRelaxation,
    },
    // 3: DIRT
    BlockProperties {
        solid: true,
        transparent: false,
        emits_light: false,
        texture_index: 3,
        display_name: "泥土",
        visual_class: VisualClass::SmoothGranular,
        mechanics: MechanicsClass::Granular,
        placement_kernel: PlacementKernel::GranularLocal,
        break_kernel: BreakKernel::GranularLocal,
        placement_unit_mass: 255,
        hardness: 0.5,
        appears_in_hotbar: true,
        breakable: true,
        angle_of_repose: 40.0,
        cohesion: 0.35,
        compressive_strength: 800.0,
        shear_strength: 350.0,
        density_kg_m3: 1_500.0,
        restitution: 0.04,
        friction: 0.88,
        stability: StabilityPolicy::ImmediateRelaxation,
    },
    // 4: WATER
    BlockProperties {
        solid: false,
        transparent: true,
        emits_light: false,
        texture_index: 4,
        display_name: "水",
        visual_class: VisualClass::Fluid,
        mechanics: MechanicsClass::Fluid,
        placement_kernel: PlacementKernel::SingleCell,
        break_kernel: BreakKernel::SingleCell,
        placement_unit_mass: 255,
        hardness: 0.0,
        appears_in_hotbar: true,
        breakable: true,
        angle_of_repose: 0.0,
        cohesion: 0.0,
        compressive_strength: 0.0,
        shear_strength: 0.0,
        density_kg_m3: 1_000.0,
        restitution: 0.0,
        friction: 0.0,
        stability: StabilityPolicy::NoPhysics,
    },
    // 5: GLASS
    BlockProperties {
        solid: true,
        transparent: true,
        emits_light: false,
        texture_index: 5,
        display_name: "玻璃",
        visual_class: VisualClass::HardBlocky,
        mechanics: MechanicsClass::StaticSolid,
        placement_kernel: PlacementKernel::SingleCell,
        break_kernel: BreakKernel::SingleCell,
        placement_unit_mass: 255,
        hardness: 0.9,
        appears_in_hotbar: true,
        breakable: true,
        angle_of_repose: 90.0,
        cohesion: 0.85,
        compressive_strength: 2_000.0,
        shear_strength: 1_200.0,
        density_kg_m3: 2_500.0,
        restitution: 0.05,
        friction: 0.55,
        stability: StabilityPolicy::FloatingOnly,
    },
    // 6: SAND
    BlockProperties {
        solid: true,
        transparent: false,
        emits_light: false,
        texture_index: 6,
        display_name: "沙子",
        visual_class: VisualClass::SmoothGranular,
        mechanics: MechanicsClass::Granular,
        placement_kernel: PlacementKernel::GranularLocal,
        break_kernel: BreakKernel::GranularLocal,
        placement_unit_mass: 255,
        hardness: 0.4,
        appears_in_hotbar: true,
        breakable: true,
        angle_of_repose: 34.0,
        cohesion: 0.05,
        compressive_strength: 500.0,
        shear_strength: 120.0,
        density_kg_m3: 1_600.0,
        restitution: 0.02,
        friction: 0.8,
        stability: StabilityPolicy::ImmediateRelaxation,
    },
    // 7: WOOD
    BlockProperties {
        solid: true,
        transparent: false,
        emits_light: false,
        texture_index: 7,
        display_name: "木头",
        visual_class: VisualClass::HardBlocky,
        mechanics: MechanicsClass::StaticSolid,
        placement_kernel: PlacementKernel::SingleCell,
        break_kernel: BreakKernel::SingleCell,
        placement_unit_mass: 255,
        hardness: 1.0,
        appears_in_hotbar: true,
        breakable: true,
        angle_of_repose: 90.0,
        cohesion: 0.8,
        compressive_strength: 3_000.0,
        shear_strength: 1_500.0,
        density_kg_m3: 700.0,
        restitution: 0.2,
        friction: 0.7,
        stability: StabilityPolicy::FloatingOnly,
    },
    // 8: LEAVES
    BlockProperties {
        solid: true,
        transparent: false,
        emits_light: false,
        texture_index: 8,
        display_name: "树叶",
        visual_class: VisualClass::Foliage,
        mechanics: MechanicsClass::Decorative,
        placement_kernel: PlacementKernel::SingleCell,
        break_kernel: BreakKernel::SingleCell,
        placement_unit_mass: 255,
        hardness: 0.2,
        appears_in_hotbar: true,
        breakable: true,
        angle_of_repose: 90.0,
        cohesion: 0.15,
        compressive_strength: 100.0,
        shear_strength: 60.0,
        density_kg_m3: 250.0,
        restitution: 0.1,
        friction: 0.6,
        stability: StabilityPolicy::NoPhysics,
    },
    // 9: STONE_BRICKS
    BlockProperties {
        solid: true,
        transparent: false,
        emits_light: false,
        texture_index: 9,
        display_name: "石砖",
        visual_class: VisualClass::HardBlocky,
        mechanics: MechanicsClass::StaticSolid,
        placement_kernel: PlacementKernel::SingleCell,
        break_kernel: BreakKernel::SingleCell,
        placement_unit_mass: 255,
        hardness: 1.7,
        appears_in_hotbar: true,
        breakable: true,
        angle_of_repose: 90.0,
        cohesion: 0.98,
        compressive_strength: 9_000.0,
        shear_strength: 4_500.0,
        density_kg_m3: 2_500.0,
        restitution: 0.12,
        friction: 0.85,
        stability: StabilityPolicy::FloatingOnly,
    },
    // 10: BEDROCK
    BlockProperties {
        solid: true,
        transparent: false,
        emits_light: false,
        texture_index: 10,
        display_name: "基岩",
        visual_class: VisualClass::HardBlocky,
        mechanics: MechanicsClass::StaticSolid,
        placement_kernel: PlacementKernel::None,
        break_kernel: BreakKernel::None,
        placement_unit_mass: 0,
        hardness: f32::INFINITY,
        appears_in_hotbar: false,
        breakable: false,
        angle_of_repose: 90.0,
        cohesion: 1.0,
        compressive_strength: f32::INFINITY,
        shear_strength: f32::INFINITY,
        density_kg_m3: 3_000.0,
        restitution: 0.0,
        friction: 1.0,
        stability: StabilityPolicy::NoPhysics,
    },
];

/// 根据 BlockID 查询其属性。越界时返回 AIR 属性（容错）。
pub fn properties(id: BlockID) -> &'static BlockProperties {
    PROPERTIES.get(id.0 as usize).unwrap_or(&PROPERTIES[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bedrock_is_world_boundary_material() {
        let props = properties(BlockID::BEDROCK);
        assert!(props.solid);
        assert!(!props.transparent);
        assert!(!props.breakable);
        assert!(!props.appears_in_hotbar);
        assert_eq!(props.stability, StabilityPolicy::NoPhysics);
        assert_eq!(props.placement_kernel, PlacementKernel::None);
    }

    #[test]
    fn stone_bricks_are_creative_hard_material() {
        let props = properties(BlockID::STONE_BRICKS);
        assert!(props.solid);
        assert!(props.breakable);
        assert!(props.appears_in_hotbar);
        assert_eq!(props.visual_class, VisualClass::HardBlocky);
        assert_eq!(props.stability, StabilityPolicy::FloatingOnly);
    }

    #[test]
    fn material_cell_empty_and_full_semantics() {
        assert!(MaterialCell::EMPTY.is_empty());
        let cell = MaterialCell::full(BlockID::SAND);
        assert_eq!(cell.occupancy, u8::MAX);
        assert_eq!(cell.primary, BlockID::SAND);
        assert!(!cell.is_empty());
        assert_eq!(
            MaterialCell::from_block_id(BlockID::AIR),
            MaterialCell::EMPTY
        );
        assert_eq!(
            MaterialCell::from_block_id(BlockID::SAND).to_block_id(),
            BlockID::SAND
        );
    }

    #[test]
    fn unknown_material_falls_back_to_air_properties() {
        let props = properties(BlockID(999));
        assert_eq!(props.display_name, "空气");
        assert!(!props.appears_in_hotbar);
    }
}
