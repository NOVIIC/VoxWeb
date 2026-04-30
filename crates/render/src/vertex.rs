//! 顶点压缩格式定义。
//!
//! 每个顶点编码为单个 u32，在 WGSL 中按字段 bit 段解码。
//! 字段划分（共计 32 bit）：
//!   - position 局部坐标: x(4bit) | y(8bit) | z(4bit) = 16bit
//!   - tex_index: 8bit（纹理图集索引）
//!   - ao: 4bit（环境光遮蔽等级 0-3）
//!   - face_dir: 3bit（法线/面朝向 0-5）
//!   - unused: 1bit

use bytemuck::{Pod, Zeroable};

/// 压缩后的单顶点（u32）。WGSL 中按位段解码。
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct PackedVertex(pub u32);

impl PackedVertex {
    /// 构造一个压缩顶点。
    ///
    /// # 参数
    /// - `lx, ly, lz`: 方块在 Chunk 内的局部坐标 (0..16, 0..256, 0..16)
    /// - `tex_index`: 纹理图集索引 (0..255)
    /// - `ao`: 环境光遮蔽等级 (0..3)
    /// - `face_dir`: 面朝向 (0..5)
    pub fn new(lx: u8, ly: u8, lz: u8, tex_index: u8, ao: u8, face_dir: u8) -> Self {
        let packed = ((lx as u32) & 0xF)
            | (((ly as u32) & 0xFF) << 4)
            | (((lz as u32) & 0xF) << 12)
            | (((tex_index as u32) & 0xFF) << 16)
            | (((ao as u32) & 0x3) << 24)
            | (((face_dir as u32) & 0x7) << 26);
        Self(packed)
    }
}
