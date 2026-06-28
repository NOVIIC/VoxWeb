//! FreeObject 数据结构：从静态 MaterialField 提取出的动态材质团。

use glam::{Vec3, i16vec3};
use serde::{Deserialize, Serialize};

use crate::block::MaterialID;
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
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transform {
    pub position: Vec3,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectSample {
    /// 相对 `transform.position` 的 cell 偏移。第一版只提取整格材质。
    pub local_pos: [i16; 3],
    pub material: MaterialID,
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
    pub fn projected_from_cells(
        id: ObjectID,
        cells: &[(Position, MaterialID)],
        final_offset_y: i32,
    ) -> Option<Self> {
        let first = cells.first()?;
        let mut min = first.0;
        let mut max = first.0;
        let mut total_mass = 0u32;

        for (pos, material) in cells {
            min.x = min.x.min(pos.x);
            min.y = min.y.min(pos.y);
            min.z = min.z.min(pos.z);
            max.x = max.x.max(pos.x);
            max.y = max.y.max(pos.y);
            max.z = max.z.max(pos.z);
            total_mass += u32::from(crate::block::MaterialCell::full(*material).occupancy);
        }

        let origin = Position::new(min.x, min.y + final_offset_y, min.z);
        let samples = cells
            .iter()
            .map(|(pos, material)| {
                let local = i16vec3(
                    (pos.x - min.x) as i16,
                    (pos.y - min.y) as i16,
                    (pos.z - min.z) as i16,
                );
                ObjectSample {
                    local_pos: local.to_array(),
                    material: *material,
                    mass: u8::MAX,
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
            .map(|(_, material)| crate::block::properties(*material).density_kg_m3)
            .sum();

        Some(Self {
            id,
            transform: Transform {
                position: final_min,
            },
            velocity: Vec3::ZERO,
            angular_velocity: Vec3::ZERO,
            samples,
            material_summary: MaterialSummary {
                dominant: first.1,
                sample_count: cells.len() as u32,
                total_mass,
            },
            mass,
            collision_proxy: CollisionProxy::Aabb(Aabb::new(final_min, final_max)),
            state: FreeObjectState::Projected,
        })
    }
}
