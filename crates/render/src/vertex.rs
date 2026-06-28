//! 顶点压缩格式定义。
//!
//! 每个顶点编码为单个 u32，在 WGSL 中按字段 bit 段解码。
//! 顶点位置是"方块角点"坐标，因此 X/Z 取值 0..=16（17 个值，需 5 bit），
//! Y 取值 0..=256（257 个值，需 9 bit）。
//!
//! 字段划分（共计 32 bit）：
//!   - bits  0..5  : lx       (5 bit, 0..=31)   方块角点 X
//!   - bits  5..14 : ly       (9 bit, 0..=511)  方块角点 Y
//!   - bits 14..19 : lz       (5 bit, 0..=31)   方块角点 Z
//!   - bits 19..22 : face_dir (3 bit, 0..=7)    面朝向（0=PosX..5=NegZ）
//!   - bits 22..30 : tex      (8 bit, 0..=255)  纹理图集索引
//!   - bits 30..32 : ao       (2 bit, 0..=3)    AO 等级（Phase 7 启用）

use bytemuck::{Pod, Zeroable};

/// 压缩后的单顶点（u32）。WGSL 中按位段解码。
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct PackedVertex(pub u32);

/// 平滑材质使用的普通顶点格式。
///
/// `PackedVertex` 只能表达整数方块角点，适合硬方块贪婪网格；颗粒材质需要
/// 非整数高度和真实法线，因此单独走这条 float 顶点路径。
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct SmoothVertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    pub raw_uv: [f32; 2],
    pub tex_index: f32,
    pub ao: f32,
}

impl SmoothVertex {
    pub fn new(position: [f32; 3], normal: [f32; 3], raw_uv: [f32; 2], tex_index: u8) -> Self {
        Self {
            position,
            normal,
            raw_uv,
            tex_index: tex_index as f32,
            ao: 3.0,
        }
    }
}

/// 面朝向枚举。值与 WGSL 中 face_dir 字段对齐。
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Face {
    PosX = 0,
    NegX = 1,
    PosY = 2,
    NegY = 3,
    PosZ = 4,
    NegZ = 5,
}

impl PackedVertex {
    /// 构造一个压缩顶点。
    ///
    /// # 参数
    /// - `lx`, `ly`, `lz`: 顶点（角点）在 Chunk 内的局部坐标
    /// - `face`: 当前顶点所属面的朝向
    /// - `tex_index`: 纹理图集索引
    /// - `ao`: 环境光遮蔽等级 (0..=3)，Phase 1 全填 3（最亮）
    pub fn new(lx: u8, ly: u16, lz: u8, face: Face, tex_index: u8, ao: u8) -> Self {
        let packed = ((lx as u32) & 0x1F)
            | (((ly as u32) & 0x1FF) << 5)
            | (((lz as u32) & 0x1F) << 14)
            | (((face as u32) & 0x7) << 19)
            | (((tex_index as u32) & 0xFF) << 22)
            | (((ao as u32) & 0x3) << 30);
        Self(packed)
    }
}

impl Face {
    pub fn from_index(index: u8) -> Self {
        match index {
            0 => Face::PosX,
            1 => Face::NegX,
            2 => Face::PosY,
            3 => Face::NegY,
            4 => Face::PosZ,
            _ => Face::NegZ,
        }
    }
}

/// wgpu 顶点缓冲布局描述：单个 u32 attribute。
pub fn vertex_buffer_layout() -> wgpu::VertexBufferLayout<'static> {
    // VertexAttribute 数组需 'static，使用 const 数组。
    const ATTRS: &[wgpu::VertexAttribute] = &[wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Uint32,
        offset: 0,
        shader_location: 0,
    }];
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<PackedVertex>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: ATTRS,
    }
}

/// 平滑材质顶点缓冲布局。
pub fn smooth_vertex_buffer_layout() -> wgpu::VertexBufferLayout<'static> {
    const ATTRS: &[wgpu::VertexAttribute] = &[
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x3,
            offset: 0,
            shader_location: 0,
        },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x3,
            offset: 12,
            shader_location: 1,
        },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x2,
            offset: 24,
            shader_location: 2,
        },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32,
            offset: 32,
            shader_location: 3,
        },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32,
            offset: 36,
            shader_location: 4,
        },
    ];
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<SmoothVertex>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: ATTRS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_roundtrip() {
        let v = PackedVertex::new(15, 256, 7, Face::PosY, 200, 3);
        // 解包验证
        let p = v.0;
        assert_eq!(p & 0x1F, 15);
        assert_eq!((p >> 5) & 0x1FF, 256);
        assert_eq!((p >> 14) & 0x1F, 7);
        assert_eq!((p >> 19) & 0x7, Face::PosY as u32);
        assert_eq!((p >> 22) & 0xFF, 200);
        assert_eq!((p >> 30) & 0x3, 3);
    }
}
