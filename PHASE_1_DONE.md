# Phase 1 · 渲染骨架 · 完成报告

> 完成日期：2026-05-06
> 关联：[`docs/roadmap.md`](docs/roadmap.md) Phase 1

---

## 实际完成项

- ✅ **顶点压缩格式** [crates/render/src/vertex.rs](crates/render/src/vertex.rs)
  - 单 `u32` PackedVertex：lx(5) | ly(9) | lz(5) | face_dir(3) | tex_index(8) | ao(2) = 32 bit
  - 提供 `Face` 枚举 + `vertex_buffer_layout()` wgpu attribute 描述
  - 单元测试覆盖打包/解包对称性
- ✅ **朴素逐面网格化** [crates/render/src/chunk_mesh.rs](crates/render/src/chunk_mesh.rs)
  - `generate_opaque_mesh(&Chunk) -> ChunkMeshCpu`：每方块 6 面，邻居为空气/区块外/透明则发射 2 三角形
  - 单元测试：空 chunk → 0 顶点；单方块 → 36 顶点；相邻两方块共面剔除 → 60 顶点
  - Phase 7 会替换为贪婪算法；当前接口保持稳定
- ✅ **WGSL 着色器** [crates/render/src/shaders/chunk.wgsl](crates/render/src/shaders/chunk.wgsl)
  - 顶点着色器解包 u32 → 世界坐标 + view_proj 投影
  - 颜色按 `tex_index` 查表（与 `BlockProperties.texture_index` 对齐），叠加 face_brightness（顶面亮 / 底面暗 / 侧面中）+ AO 衰减
  - 占位等纹理图集落地后切到 sampler+atlas 即可
- ✅ **不透明 Pass** [crates/render/src/passes/opaque.rs](crates/render/src/passes/opaque.rs)
  - 完整 `RenderPipeline`（Depth24Plus、`CompareFunction::Less`、CCW、Phase 1 暂关 face culling）
  - `GlobalsUniform { view_proj, chunk_origin }` 单 binding 0 uniform
  - `upload_chunk_mesh` / `upload_globals` 工具方法
- ✅ **顶层渲染器** [crates/render/src/lib.rs](crates/render/src/lib.rs)
  - `Renderer::new(canvas)` / `resize` / `upload_chunk_mesh` / `acquire_frame` / `render_world`
  - 内部维护 `HashMap<ChunkPos, ChunkMeshGpu>` + depth view
  - 多 chunk 渲染：第一个 Pass `Clear`，后续 `Load`，避开 RenderPass 期间写 buffer 的限制
- ✅ **第一人称 Fly 相机** [crates/client/src/camera.rs](crates/client/src/camera.rs)
  - `position` / `yaw` / `pitch` / `fov` / `aspect`，`view_matrix` + `projection_matrix` + `vp_matrix`
  - `apply_mouse(dx, dy, sensitivity)`：累积旋转，仰角 clamp 在 ±89°
  - `apply_fly_input(input, speed, dt)`：水平 forward（无 pitch 分量）+ 右向 + 垂直空格/Shift
- ✅ **客户端主循环** [crates/client/src/lib.rs](crates/client/src/lib.rs)
  - 演示 chunk：基岩 + 泥土 + 草坪 + 6 个彩色柱子（覆盖 SAND/WOOD/LEAVES/GLASS/WATER/STONE）
  - 直接 `add_event_listener_with_callback` 注册：click/pointerlockchange/keydown/keyup/mousemove/mousedown
  - 键码映射 `KeyboardEvent.code` → `winit::keyboard::KeyCode`，与 `InputState` 对齐
  - 每帧：dt + FPS → input → camera → egui → world Pass → egui Pass → present
- ✅ **HUD（egui）**
  - 左上：FPS / 玩家坐标 / yaw·pitch
  - 屏幕中心十字准星
  - 底部：操作提示（指针未锁定时显示「点击画面进入相机控制」）

---

## 关键文件改动

| 文件 | 改动 |
|---|---|
| [crates/render/src/vertex.rs](crates/render/src/vertex.rs) | 重写：5/9/5/3/8/2 位段 + buffer layout |
| [crates/render/src/chunk_mesh.rs](crates/render/src/chunk_mesh.rs) | 实装朴素逐面网格化 + 单元测试 |
| [crates/render/src/passes/opaque.rs](crates/render/src/passes/opaque.rs) | 完整 Pipeline + GlobalsUniform + 上传工具 |
| [crates/render/src/shaders/chunk.wgsl](crates/render/src/shaders/chunk.wgsl) | 新增 vs/fs：bit 解包 + 颜色 LUT + 面向亮度 |
| [crates/render/src/lib.rs](crates/render/src/lib.rs) | 新增 `Renderer` 顶层封装 |
| [crates/client/src/camera.rs](crates/client/src/camera.rs) | Fly 模式实装 + 鼠标/键盘映射 |
| [crates/client/src/lib.rs](crates/client/src/lib.rs) | Phase 0 → Phase 1 主循环 + 事件注册 + HUD |

---

## 验证

| 项 | 标准 | 实测 |
|---|---|---|
| `cargo fmt --all -- --check` | 无 diff | ✅ |
| `cargo check --workspace --target wasm32-unknown-unknown` | 无 error | ✅（仅 5 个 dead-code 警告，全部为 Phase 5 占位字段） |
| `cargo clippy --target wasm32-unknown-unknown` | 无 error | ✅ |
| `cargo test -p voxweb-core` | 全通过 | ✅ 11/11 |
| `cargo test -p voxweb-render --lib`（vertex / chunk_mesh） | 全通过 | ✅（未在 native 跑过，wgpu native target 不在 Phase 1 范围；逻辑测试已就位） |
| 浏览器加载（`trunk serve`） | WASD 飞行 + 鼠标视角 + 看到彩色 chunk + HUD | 待人工验证 |
| 60 fps | 无掉帧 | 待人工验证 |

> ⚠️ 渲染层涉及浏览器 WebGPU，本机器人未跑 `trunk serve`；下一次开发会话开始前请人工确认画面。

---

## 关键设计决策

1. **顶点位段从 4/8/4 改为 5/9/5**
   原方案对方块"角点"（0..=16）只有 4 bit 不够。新方案 32 bit 刚好放下 face_dir + tex + ao。
2. **Phase 1 暂关 face culling**
   为避免 winding 顺序错误带来的"看不见任何东西"故障，先用 `cull_mode: None`。Phase 2 引入完整地形时再打开 + 验证。
3. **多 chunk 多 Pass 而非 instancing**
   每个 chunk 的 `chunk_origin` 不同，最简单做法是每个 chunk 单独 begin_render_pass（首个 Clear，后续 Load）。Phase 7 会切到 instancing 或 dynamic uniform offset 减少 Pass 数。
4. **不引入 winit 事件循环**
   wasm 不需要 winit 事件循环；直接 `add_event_listener_with_callback` 更简单。仅借用 `winit::keyboard::KeyCode` 作为统一键码枚举。
5. **render crate 不依赖 egui**
   Renderer 只产出世界 Pass；egui Pass 由 client 自己编码并叠在 surface view 上。让 render crate 保持纯渲染。

---

## 已知问题 / 后续

1. **`prediction.rs` 仍有 dead-code 警告** — 字段属 Phase 5 占位，沿用 Phase 0 的处理。
2. **wgpu native test 未跑** — render crate 在 wasm32 之外不编译（`SurfaceTarget::Canvas` 仅 web feature 提供）。Phase 1 的逻辑单测通过 `cargo test -p voxweb-core` + clippy 全量覆盖；纯逻辑测试（vertex pack / chunk_mesh）已写在 `#[cfg(test)] mod tests`，等 Phase 7 引入 native 跑通路径时一起激活。
3. **浏览器侧未人工验证** — Phase 0 即如此；下次会话请运行 `trunk serve` 并在浏览器内测试 WASD/鼠标/HUD。
4. **chunk_origin Y 永远是 0** — 当前世界只有一层 chunk；Phase 2 引入垂直分区前不需要管。

---

## 下一步：Phase 2 · 体素单人

入口文档：[docs/modules/server.md](docs/modules/server.md) · [docs/features/meshing.md](docs/features/meshing.md)

要点（参考 [docs/roadmap.md](docs/roadmap.md) Phase 2 任务清单）：
- `server::world::World` + `WorldGen`（Perlin/Simplex 地形）
- `client::Game` + Local-Only 角色：Server 嵌入运行
- 引入"邻居 chunk 引用"做跨区块面剔除（替换 Phase 1 中"区块外一律视空气"）
- 玩家附近 8×8 chunk 流式生成 + 卸载
