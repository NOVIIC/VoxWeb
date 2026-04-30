//! 贪婪网格化算法 + 跨区块面剔除。
//!
//! 将 Chunk 内相邻同材质方块的可见面合并为大矩形，减少顶点数。
//! Phase 2 先上朴素逐面网格化，Phase 7 升级为贪婪算法。

use voxweb_core::chunk::Chunk;

/// 生成一个 Chunk 的不透明网格顶点（仅包含可见面）。
/// Phase 2: 朴素逐面实现。
pub fn generate_opaque_mesh(_chunk: &Chunk, _neighbors: &[(i32, i32); 4]) -> Vec<u32> {
    // Phase 2 实现：遍历每个非空气方块，检查六个面是否暴露，
    // 暴露则生成 2 个三角形（6 个 PackedVertex）
    Vec::new()
}

/// 生成一个 Chunk 的透明（水/玻璃等）网格顶点。
/// Phase 8 实现。
pub fn generate_transparent_mesh(_chunk: &Chunk, _neighbors: &[(i32, i32); 4]) -> Vec<u32> {
    Vec::new()
}
