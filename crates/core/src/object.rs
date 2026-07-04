//! FreeObject 数据结构：从静态 MaterialField 提取出的动态材质团。

use glam::{Vec3, i16vec3};
use serde::{Deserialize, Serialize};

use crate::block::{MaterialCell, MaterialID};
use crate::chunk::Position;
use crate::geometry::Aabb;

pub type ObjectID = u64;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FreeObject {
    pub id: ObjectID,
    pub transform: Transform,
    pub velocity: Vec3,
    pub angular_velocity: Vec3,
    pub samples: Vec<ObjectSample>,
    pub material_summary: MaterialSummary,
    pub mass: f32,
    pub collision_proxy: CollisionProxy,
    pub state: FreeObjectState,
    /// 是否为软材质颗粒（sand/dirt/grass）。true 时在 tick 中走抛物线斜滑分支；
    /// 硬材质 `FloatingOnly` 对象为 false，保持刚性落定。旧存档默认 false。
    #[serde(default)]
    pub granular: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transform {
    pub position: Vec3,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectSample {
    /// 相对 `transform.position` 的 cell 偏移。第一版只提取整格材质。
    pub local_pos: [i16; 3],
    /// 提取时的完整 MaterialCell。旧存档若没有该字段，会由 `material/mass` 回退重建。
    #[serde(default)]
    pub cell: MaterialCell,
    /// 兼容旧 active_free_objects JSON，并保留给现有渲染快捷读取。
    #[serde(default)]
    pub material: MaterialID,
    #[serde(default)]
    pub mass: u8,
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MaterialSummary {
    pub dominant: MaterialID,
    pub sample_count: u32,
    pub total_mass: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CollisionProxy {
    Aabb(Aabb),
    SampleCloud,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FreeObjectState {
    Dynamic,
    Settled,
    Projected,
}

impl FreeObject {
    pub fn dynamic_from_cells(id: ObjectID, cells: &[(Position, MaterialCell)]) -> Option<Self> {
        Self::from_cells(id, cells, 0, FreeObjectState::Dynamic, Vec3::ZERO, false)
    }

    /// 软材质颗粒：单/小 cell 的动态对象，tick 中走抛物线斜滑分支。
    pub fn dynamic_grain_from_cells(
        id: ObjectID,
        cells: &[(Position, MaterialCell)],
    ) -> Option<Self> {
        Self::from_cells(id, cells, 0, FreeObjectState::Dynamic, Vec3::ZERO, true)
    }

    pub fn projected_from_cells(
        id: ObjectID,
        cells: &[(Position, MaterialCell)],
        final_offset_y: i32,
    ) -> Option<Self> {
        Self::from_cells(
            id,
            cells,
            final_offset_y,
            FreeObjectState::Projected,
            Vec3::ZERO,
            false,
        )
    }

    fn from_cells(
        id: ObjectID,
        cells: &[(Position, MaterialCell)],
        final_offset_y: i32,
        state: FreeObjectState,
        velocity: Vec3,
        granular: bool,
    ) -> Option<Self> {
        let first = cells.first()?;
        let mut min = first.0;
        let mut max = first.0;
        let mut total_mass = 0u32;

        for (pos, cell) in cells {
            min.x = min.x.min(pos.x);
            min.y = min.y.min(pos.y);
            min.z = min.z.min(pos.z);
            max.x = max.x.max(pos.x);
            max.y = max.y.max(pos.y);
            max.z = max.z.max(pos.z);
            total_mass += u32::from(cell.occupancy);
            if let Some(secondary) = cell.secondary {
                total_mass += u32::from(secondary.occupancy);
            }
        }

        let origin = Position::new(min.x, min.y + final_offset_y, min.z);
        let samples = cells
            .iter()
            .map(|(pos, cell)| {
                let local = i16vec3(
                    (pos.x - min.x) as i16,
                    (pos.y - min.y) as i16,
                    (pos.z - min.z) as i16,
                );
                ObjectSample {
                    local_pos: local.to_array(),
                    cell: *cell,
                    material: cell.primary,
                    mass: cell.occupancy,
                }
            })
            .collect::<Vec<_>>();

        let final_min = Vec3::new(origin.x as f32, origin.y as f32, origin.z as f32);
        let final_max = Vec3::new(
            (max.x + 1) as f32,
            (max.y + 1 + final_offset_y) as f32,
            (max.z + 1) as f32,
        );
        let mass = cells
            .iter()
            .map(|(_, cell)| {
                let mut mass = crate::block::properties(cell.primary).density_kg_m3
                    * (f32::from(cell.occupancy) / f32::from(u8::MAX));
                if let Some(secondary) = cell.secondary {
                    mass += crate::block::properties(secondary.material).density_kg_m3
                        * (f32::from(secondary.occupancy) / f32::from(u8::MAX));
                }
                mass
            })
            .sum();

        Some(Self {
            id,
            transform: Transform {
                position: final_min,
            },
            velocity,
            angular_velocity: Vec3::ZERO,
            samples,
            material_summary: MaterialSummary {
                dominant: first.1.primary,
                sample_count: cells.len() as u32,
                total_mass,
            },
            mass,
            collision_proxy: CollisionProxy::Aabb(Aabb::new(final_min, final_max)),
            state,
            granular,
        })
    }

    pub fn aabb(&self) -> Aabb {
        self.aabb_at(self.transform.position)
    }

    pub fn aabb_at(&self, position: Vec3) -> Aabb {
        let Some(first) = self.samples.first() else {
            return match &self.collision_proxy {
                CollisionProxy::Aabb(aabb) => *aabb,
                CollisionProxy::SampleCloud => Aabb::new(position, position),
            };
        };
        let mut min = first.local_pos;
        let mut max = first.local_pos;
        for sample in &self.samples {
            for axis in 0..3 {
                min[axis] = min[axis].min(sample.local_pos[axis]);
                max[axis] = max[axis].max(sample.local_pos[axis]);
            }
        }
        Aabb::new(
            position + Vec3::new(min[0] as f32, min[1] as f32, min[2] as f32),
            position
                + Vec3::new(
                    max[0] as f32 + 1.0,
                    max[1] as f32 + 1.0,
                    max[2] as f32 + 1.0,
                ),
        )
    }

    pub fn cells_at_position(&self, position: Vec3) -> Vec<(Position, MaterialCell)> {
        let base = Position::new(
            position.x.round() as i32,
            position.y.round() as i32,
            position.z.round() as i32,
        );
        self.samples
            .iter()
            .map(|sample| {
                (
                    Position::new(
                        base.x + sample.local_pos[0] as i32,
                        base.y + sample.local_pos[1] as i32,
                        base.z + sample.local_pos[2] as i32,
                    ),
                    sample.cell(),
                )
            })
            .collect()
    }

    pub fn cells_at_current_position(&self) -> Vec<(Position, MaterialCell)> {
        self.cells_at_position(self.transform.position)
    }
}

impl ObjectSample {
    pub fn cell(&self) -> MaterialCell {
        if !self.cell.is_empty() {
            self.cell
        } else if self.material != crate::block::BlockID::AIR && self.mass > 0 {
            MaterialCell {
                occupancy: self.mass,
                primary: self.material,
                secondary: None,
                flags: crate::block::CellFlags::empty(),
            }
        } else {
            MaterialCell::EMPTY
        }
    }
}
