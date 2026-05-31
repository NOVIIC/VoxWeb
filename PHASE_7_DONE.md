# Phase 7 完成记录：渲染优化

完成日期：2026-05-31

---

## 完成内容

- `crates/render/src/chunk_mesh.rs`
  - `generate_with_neighbors` 默认切到贪婪网格化，继续保留跨区块面剔除入口。
  - 同材质、同 4 角 AO 的单位面合并为大 quad。
  - CPU mesh 输出 `vertices + indices + bounds + visible_faces`，其中 `visible_faces` 用于统计 Phase 2 等价顶点数。
  - 顶点 AO 使用经典 3 邻域采样，shader 继续消费 packed vertex 的 2-bit AO。

- `crates/render/src/passes/opaque.rs`
  - `ChunkMeshGpu` 增加 index buffer、index_count、world-space bounds。
  - OpaquePass 上传 vertex/index 两类 buffer，draw 路径改为 `draw_indexed`。

- `crates/render/src/lib.rs`
  - `Renderer::render_world` 每帧从 `view_proj` 抽取 frustum 平面，对 chunk bounds 做 AABB 视锥剔除。
  - OpaquePass 从“每 chunk 一个 render pass”改为“单 render pass 多 draw”，减少 CPU 编码开销。
  - 返回 `WorldRenderStats`：总 chunk、可见 chunk、剔除 chunk、draw 顶点/索引数。

- `crates/client/src/mesh_jobs.rs`
  - `MeshJobQueue` 的 pending 结构从 `HashSet` 升级为 `HashMap<ChunkPos, MeshPriority>`。
  - 重复入队时允许升级优先级，玩家附近 `Critical/High` chunk 不会被旧任务卡住。
  - `run_until_budget` 返回 `MeshRunStats`，默认预算仍为 4ms/帧。

- `crates/client/src/lib.rs`
  - HUD stat 增加 `VISIBLE / CULLED / DRAW_V/I`。
  - HUD stat 增加 mesh 批次耗时、任务数、Phase 2 等价顶点数、贪婪后顶点/索引数和减少比例。
  - HUD stat 增加 world/player/selection/ui pass 的 CPU 编码耗时。

---

## 验证

- `cargo fmt`
- `cargo test -p voxweb-render`
- `cargo test -p voxweb-client`
- `cargo check --target wasm32-unknown-unknown`
- `cargo test`

---

## 已知限制 / 留给 Phase 8+

- HUD pass 耗时是 CPU 编码耗时，不是 GPU timestamp query；WebGPU timestamp query 可作为后续精确 profiling 增强。
- OpaquePass 仍保留每 chunk 独立 globals bind group；Phase 8 接 RenderGraph / camera uniform 后可进一步整理资源绑定。
- 透明方块仍留给 Phase 8 的 TransparentPass，不参与本期贪婪合并。
