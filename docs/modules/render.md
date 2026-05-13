# `render` 模块设计

> **何时阅读**：增删 Pass；改顶点格式；改着色器接口；调资源生命周期；优化渲染性能
> **关联文档**：[`README.md`](../../README.md) · [`architecture.md`](../architecture.md) · [`features/meshing.md`](../features/meshing.md) · [`features/ui.md`](../features/ui.md) · [`reference.md`](../reference.md)

---

## 一、职责

`render` crate 封装 wgpu，对外提供：
- 与 `<canvas>` 绑定的 `Renderer` 入口
- Render Graph 多 Pass 调度
- Chunk 网格生成（贪婪算法 + 跨区块面剔除 + u32 顶点压缩）
- 纹理图集 + 深度纹理 + Uniform Buffer 资源管理

**不负责**：
- 输入处理（→ `client::input`）
- 世界数据持有（仅引用 `core::Chunk` / 玩家位置；持有方是 `client` / `server`）
- 网络（→ `net`）
- UI 业务（→ `client::ui`）；本 crate 仅提供 egui 渲染 Pass 容器

---

## 二、目录结构

```
crates/render/src/
├── lib.rs              Renderer 入口 + 公开 API
├── device.rs           Surface/Device 与 canvas 绑定
├── graph.rs            Render Graph 调度框架（trait，当前未接入渲染路径）
├── passes/
│   ├── mod.rs
│   ├── opaque.rs       实体方块 Pass
│   ├── skybox.rs       天空盒 Pass（Phase 8 实装）
│   ├── transparent.rs  半透明 Pass（Phase 8 实装）
│   └── selection.rs    选中方块线框 Pass（Phase 3）
├── chunk_mesh.rs       朴素逐面网格化 + 跨区块面剔除（贪婪算法 Phase 7）
├── vertex.rs           u32 压缩格式 + 解包工具
├── texture.rs          纹理图集
└── shaders/
    ├── chunk.wgsl      实体方块着色器
    └── selection.wgsl  选中线框着色器
```

---

## 三、`device.rs` — wgpu 设备与 Surface

**职责**：
- 创建 `wgpu::Instance`（`Backends::BROWSER_WEBGPU`）
- 通过 `web-sys::HtmlCanvasElement` 创建 `Surface`
- 协商 `Adapter` / `Device` / `Queue`
- 监听 canvas resize 事件，重建 Surface 配置和 depth texture
- 提供首选纹理格式查询（一般是 `Bgra8Unorm` 或 `Rgba8UnormSrgb`）

**关键 API**：

```rust
pub struct DeviceContext {
    pub surface: wgpu::Surface<'static>,
    pub device: Arc<wgpu::Device>,
    pub queue: Arc<wgpu::Queue>,
    pub surface_format: wgpu::TextureFormat,
}

/// 异步初始化，需在 wasm-bindgen-futures 的 spawn_local 中调用
pub async fn init_device(canvas: &web_sys::HtmlCanvasElement) -> Result<DeviceContext, String>;
```

**WebGPU 特定注意**：
- `Backends::BROWSER_WEBGPU` 不要写成 `Backends::all()`，避免 wgpu 试图启用桌面后端
- `request_adapter` 在 Firefox 稳定版会失败 → 在 client 层捕获并提示用户
- HiDPI：监听 `window.devicePixelRatio`，乘上 canvas 逻辑尺寸传给 `resize`
- canvas 大小变化通过 `ResizeObserver` 监听（client 层负责）

---

## 四、`vertex.rs` — u32 压缩顶点格式

详细布局见 [`features/meshing.md` 顶点压缩章节](../features/meshing.md#二u32-顶点压缩格式)。本文只列接口：

```rust
/// 压缩后的顶点：单 u32
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
pub struct PackedVertex(pub u32);

impl PackedVertex {
    pub fn pack(local_x: u8, local_y: u8, local_z: u8,
                face: Face, texture: u8, ao: u8) -> Self;
}

#[repr(u8)]
pub enum Face { PosX = 0, NegX = 1, PosY = 2, NegY = 3, PosZ = 4, NegZ = 5 }

/// wgpu 顶点缓冲布局描述（u32 attribute）
pub fn vertex_buffer_layout() -> wgpu::VertexBufferLayout<'static>;
```

WGSL 解包：

```wgsl
struct UnpackedVertex {
    local_pos: vec3<f32>,
    face_normal: vec3<f32>,
    texture_uv: vec2<f32>,
    ao_factor: f32,
}

fn unpack_vertex(packed: u32, chunk_origin: vec3<f32>) -> UnpackedVertex { ... }
```

---

## 五、`texture.rs` — 纹理图集

**设计**：
- 单张大图（如 256×256，每格 16×16 → 16×16 = 256 个纹理槽，足够本期所有方块）
- 每方块的纹理由 `BlockProperties.texture_index: u8` 指定槽位
- WGSL 中 UV 计算：`uv = (texture_index_to_grid(idx) + face_local_uv) / atlas_size`

**纹理来源**：
- 本期：项目内嵌（`include_bytes!` 加载，编译进 wasm）
- v2：可选远程加载（fetch + Image bitmap）

**Mipmap**：本期不开（避免远处纹理颜色混叠产生彩虹），用 `MagFilter::Nearest` + `MinFilter::Nearest` 保持像素风。

---

## 六、`graph.rs` — Render Graph

### 设计目标
- **声明式**：每个 Pass 声明输入资源（贴图/UB）+ 输出资源（render target）
- **依赖排序**：自动按依赖关系排序 Pass
- **资源复用**：同一 frame 内的中间贴图（如 SSAO 输出）可共享
- **简化版**：本期不实现完整 DAG 调度，使用**固定顺序**（Depth → Opaque → Skybox → Transparent → UI）；预留 trait 便于后期扩展

### Trait

```rust
pub trait RenderPass {
    fn name(&self) -> &'static str;

    /// 每帧调用，编码 GPU 命令
    fn execute(&mut self, ctx: &mut PassContext);
}

pub struct PassContext<'a> {
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
    pub encoder: &'a mut wgpu::CommandEncoder,
    pub surface_view: &'a wgpu::TextureView,
    pub depth_view: &'a wgpu::TextureView,
    pub camera_bind_group: &'a wgpu::BindGroup,
    pub frame_data: &'a FrameData, // 玩家位置、时间、待渲染 chunk 列表等
}
```

### 调度

```rust
pub struct RenderGraph {
    passes: Vec<Box<dyn RenderPass>>,
}

impl RenderGraph {
    pub fn new() -> Self;
    pub fn add_pass(&mut self, pass: Box<dyn RenderPass>);
    pub fn execute(&mut self, ctx: &mut PassContext);
}
```

调度模式：单 `CommandEncoder`，所有 Pass 共享，最后一次 `queue.submit()`。

### 默认 Pass 组合

```rust
let mut graph = RenderGraph::new();
graph.add_pass(Box::new(DepthPrePass::new(...)));      // 可通过 settings 关闭
graph.add_pass(Box::new(OpaquePass::new(...)));
graph.add_pass(Box::new(SkyboxPass::new(...)));
graph.add_pass(Box::new(TransparentPass::new(...)));
graph.add_pass(Box::new(UiPass::new(...)));            // egui-wgpu 容器
```

---

## 七、各 Pass 详细设计

### 7.1 `passes/depth.rs` — Depth Pre-Pass

**目的**：先渲染所有不透明几何体的深度，让后续 Opaque Pass 享受 Early-Z 优化（减少 overdraw）。

**输入**：所有可见 Chunk 网格 + 相机 UB
**输出**：写入 depth texture（`LessEqual` 测试）；color attachment 留空（`store_op = Discard`）

**着色器**：极简顶点着色器（只算 `clip_position`，无片段着色）

**何时关闭**：低端 GPU 或场景简单时，Depth Pre-Pass 可能反而变慢；提供运行时开关 `RenderSettings::depth_prepass_enabled`。

### 7.2 `passes/opaque.rs` — Opaque Pass

**目的**：渲染所有不透明方块。
**输入**：可见 Chunk 网格 + 纹理图集 + 相机 UB
**输出**：写入 color + depth（`Less`，`store`）
**Pipeline 设置**：
- 深度比较：`Less`（如果有 Depth Pre-Pass，可改 `Equal` 进一步省 fragment work）
- Cull 模式：`Back`
- Blend：禁用
- 顶点格式：`PackedVertex`（u32）

**Draw 调用顺序**：按 chunk 距离从近到远排序（虽然有深度测试，但近处先 draw 能让远处更多 fragment 被剔除）。

### 7.3 `passes/skybox.rs` — Skybox Pass

**目的**：填充背景天空（程序化天空，支持太阳方向 + 颜色梯度）。
**绘制方式**：
- 全屏三角形（覆盖整个 viewport）
- 片段着色器根据 ray direction 计算颜色（线性 azimuth-elevation 渐变）
- 深度比较：`LessEqual`，深度写入：`false`（保证天空不挡其它东西，但被前景挡住）

**程序化天空算法**：
```wgsl
fn sky_color(dir: vec3<f32>, sun_dir: vec3<f32>) -> vec3<f32> {
    let sun_dot = max(dot(dir, sun_dir), 0.0);
    let zenith = mix(horizon_color, zenith_color, smoothstep(0.0, 0.6, dir.y));
    let sun_glow = pow(sun_dot, 64.0) * sun_color;
    return zenith + sun_glow;
}
```

**v2 stretch**：可替换为 cubemap（加载 6 张 png）。

### 7.4 `passes/transparent.rs` — Transparent Pass

**目的**：渲染水、玻璃等半透明方块。
**关键差异**：
- Blend：`Alpha Blending`（`SrcAlpha, OneMinusSrcAlpha`）
- 深度比较：`Less`，**深度写入：false**
- Draw 顺序：按距离从远到近排序（保证混合顺序）
- 网格独立：透明方块不参与贪婪网格化的合并（不同方块属性不能合并）；用单独的 mesh buffer

**简化策略**：本期透明方块不超过 2 种（水 + 玻璃），不实现"Order-Independent Transparency"等高级技术。

### 7.5 `passes/ui.rs` — UI Pass

**目的**：把 egui 渲染输出叠到画面上。
**实现**：直接复用 `egui-wgpu` 的 `Renderer`，把它包装为 `RenderPass`。
**关键参数**：
- `LoadOp::Load`（不清屏，叠加到已有画面）
- 深度测试：禁用（UI 不参与 3D 深度）

**输入**：每帧由 client 层提供的 `egui::FullOutput`（`PaintJobs`）

---

## 八、相机 Uniform Buffer

```rust
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
pub struct CameraUniform {
    pub view_proj: [[f32; 4]; 4],
    pub view: [[f32; 4]; 4],            // 单独提供给天空盒（去平移）
    pub camera_pos: [f32; 4],           // xyz + 1.0 padding
    pub time_seconds: f32,              // 用于水波动画等
    pub _padding: [f32; 3],
}
```

每帧由 client 层调用 `Renderer::update_camera(&CameraUniform)`，写入 GPU buffer，绑定到所有 Pass 的 group(0)。

---

## 九、`Renderer` 主入口

```rust
pub struct Renderer {
    pub device: Arc<wgpu::Device>,
    pub queue: Arc<wgpu::Queue>,
    pub surface: wgpu::Surface<'static>,
    pub surface_format: wgpu::TextureFormat,
    // 内部字段：width, height, depth_texture, depth_view,
    // opaque_pass: OpaquePass, selection_pass: SelectionPass,
    // chunk_meshes: HashMap<ChunkPos, ChunkMeshGpu>
}

impl Renderer {
    pub async fn new(canvas: &web_sys::HtmlCanvasElement) -> Result<Self, String>;
    pub fn resize(&mut self, width: u32, height: u32);

    /// 上传或更新一个 chunk 的网格（由 mesh job 完成后调用）
    pub fn upload_chunk_mesh(&mut self, pos: ChunkPos, mesh: &ChunkMeshCpu);

    /// 卸载远处 chunk 网格
    pub fn drop_chunk_mesh(&mut self, pos: ChunkPos);

    /// 查询某个 chunk 是否已有 GPU mesh
    pub fn has_chunk_mesh(&self, pos: ChunkPos) -> bool;

    /// 取得本帧 surface texture（失败时自动重配 Surface）
    pub fn acquire_frame(&mut self) -> Option<wgpu::SurfaceTexture>;

    /// 渲染世界（OpaquePass）：清屏 + 遍历所有 chunk_meshes 绘制
    pub fn render_world(&mut self, encoder: &mut wgpu::CommandEncoder,
        color_view: &wgpu::TextureView, view_proj: Mat4, clear_color: [f64; 4]);

    /// 渲染选中方块线框（在 render_world 之后调用）
    pub fn render_selection(&mut self, encoder: &mut wgpu::CommandEncoder,
        color_view: &wgpu::TextureView, view_proj: Mat4, block_pos: Option<Position>);

    pub fn depth_view(&self) -> &wgpu::TextureView;
    pub fn loaded_chunk_count(&self) -> usize;
}
```

> **注**：当前 Renderer 未持有 egui renderer / camera buffer / RenderSettings。egui 渲染由 client 自行管理；camera uniform 通过 `render_world` 的 `view_proj` 参数每帧传入。`RenderGraph` trait 在 [graph.rs](graph.rs) 中已定义但渲染路径未使用（直接调用 `render_world`/`render_selection`），待 Phase 8 多 Pass 接入。

---

## 十、资源生命周期

| 资源 | 创建时机 | 销毁时机 |
|---|---|---|
| `Surface` / `Device` / `Queue` | `Renderer::new` 一次 | Tab 关闭 |
| `depth_texture` | 启动 + 每次 resize | 重建时 |
| `chunk_mesh_gpu` | `upload_chunk_mesh` | `drop_chunk_mesh`（玩家走远）/ chunk 修改时（重建） |
| Pass pipelines | 各 Pass `new` 一次 | 程序退出 |

**Chunk 网格更新规则**：
- 方块修改 → `client::game` 把对应 chunk 加入 dirty 集合
- 下一帧 mesh budget：取出 dirty chunk → `chunk_mesh::generate_with_neighbors` 生成 CPU 数据 → `Renderer::upload_chunk_mesh` 上传 GPU
- 玩家走远（超出渲染距离）→ `Renderer::drop_chunk_mesh` 卸载

---

## 十一、性能预算

| 项目 | 目标 |
|---|---|
| 单帧总时间 | < 16.6ms（60fps） |
| Render Graph 执行（CPU 编码） | < 2ms |
| GPU draw（典型场景，渲染距离 6） | < 8ms |
| 网格化（每帧最多） | 4ms |
| 其余（输入、相机、UI build） | < 2ms |

监测手段：`RenderSettings.show_stats` 开启时，HUD 显示每项耗时。详见 [`features/ui.md`](../features/ui.md)。

---

## 十二、不在范围

- 阴影贴图（v2 / stretch）
- SSAO（v2 / stretch）
- Bloom / 色调映射（v2 / stretch）
- 粒子系统
- 镜面反射 / 屏幕空间反射
- 体积光
- WebGL2 后端
- 着色器热重载（见 `README.md` Out-of-Scope）
