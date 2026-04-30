//! 纹理图集管理。
//!
//! 将多种方块的纹理打包到一张大纹理中，顶点通过 `tex_index` 引用对应区域。

/// 纹理图集管理器（Phase 1 占位）。
pub struct TextureAtlas {
    // Phase 1: 图集纹理、各 BlockID 对应的 UV 区域
}

impl TextureAtlas {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for TextureAtlas {
    fn default() -> Self {
        Self::new()
    }
}
