use glam::{Mat4, Vec3, Vec4};
use voxweb_core::Aabb;

#[derive(Clone, Copy, Debug)]
struct Plane {
    normal: Vec3,
    d: f32,
}

impl Plane {
    fn from_vec4(v: Vec4) -> Self {
        let normal = Vec3::new(v.x, v.y, v.z);
        let len = normal.length();
        if len > 0.0 {
            Self {
                normal: normal / len,
                d: v.w / len,
            }
        } else {
            Self { normal, d: v.w }
        }
    }

    fn distance_to_positive_vertex(&self, aabb: &Aabb) -> f32 {
        let p = Vec3::new(
            if self.normal.x >= 0.0 {
                aabb.max.x
            } else {
                aabb.min.x
            },
            if self.normal.y >= 0.0 {
                aabb.max.y
            } else {
                aabb.min.y
            },
            if self.normal.z >= 0.0 {
                aabb.max.z
            } else {
                aabb.min.z
            },
        );
        self.normal.dot(p) + self.d
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Frustum {
    planes: [Plane; 6],
}

impl Frustum {
    pub(crate) fn from_view_proj(view_proj: Mat4) -> Self {
        // glam::Mat4 是列主序；先取出四个 row，再按 WebGPU 0..1 深度范围抽取平面。
        let c0 = view_proj.x_axis;
        let c1 = view_proj.y_axis;
        let c2 = view_proj.z_axis;
        let c3 = view_proj.w_axis;
        let row0 = Vec4::new(c0.x, c1.x, c2.x, c3.x);
        let row1 = Vec4::new(c0.y, c1.y, c2.y, c3.y);
        let row2 = Vec4::new(c0.z, c1.z, c2.z, c3.z);
        let row3 = Vec4::new(c0.w, c1.w, c2.w, c3.w);

        Self {
            planes: [
                Plane::from_vec4(row3 + row0), // left
                Plane::from_vec4(row3 - row0), // right
                Plane::from_vec4(row3 + row1), // bottom
                Plane::from_vec4(row3 - row1), // top
                Plane::from_vec4(row2),        // near: z >= 0
                Plane::from_vec4(row3 - row2), // far: z <= w
            ],
        }
    }

    pub(crate) fn intersects_aabb(&self, aabb: &Aabb) -> bool {
        self.planes
            .iter()
            .all(|plane| plane.distance_to_positive_vertex(aabb) >= 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_frustum_accepts_ndc_unit_box() {
        let f = Frustum::from_view_proj(Mat4::IDENTITY);
        let inside = Aabb::new(Vec3::new(-0.5, -0.5, 0.1), Vec3::new(0.5, 0.5, 0.9));
        assert!(f.intersects_aabb(&inside));
    }

    #[test]
    fn identity_frustum_rejects_box_past_right_plane() {
        let f = Frustum::from_view_proj(Mat4::IDENTITY);
        let outside = Aabb::new(Vec3::new(1.2, -0.5, 0.1), Vec3::new(2.0, 0.5, 0.9));
        assert!(!f.intersects_aabb(&outside));
    }
}
