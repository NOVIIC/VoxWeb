# 网格化：贪婪算法 + 跨区块剔除 + 顶点压缩

> **何时阅读**：改 chunk 网格性能；调顶点格式；加新方块属性影响渲染；加 AO/光照
> **关联文档**：[`README.md`](../../README.md) · [`modules/render.md`](../modules/render.md) · [`modules/core.md`](../modules/core.md) · [`modules/client.md`](../modules/client.md) · [`architecture.md`](../architecture.md)

---

## 一、为什么需要专门的网格化策略

体素世界的每个区块（16×256×16）有 65536 个方块。如果每个方块都生成 6 个面 → 单 chunk 最多 390K 面。即使剔除掉中心被遮挡的，仍是几万个面。**朴素方案对 GPU 顶点带宽和 CPU 编码都是灾难**。

本项目采用三个组合优化：

1. **跨区块面剔除（Cross-Chunk Face Culling）**：相邻 chunk 边界处也做面剔除
2. **贪婪网格化（Greedy Meshing）**：相邻同材质的方块面合并成大矩形
3. **u32 顶点压缩**：单顶点 4 字节而非 36 字节

---

### 阶段实装范围

| 阶段 | 包含 |
|---|---|
| **Phase 1 ✅** | u32 顶点压缩格式（§2）；朴素逐面（`generate_opaque_mesh`） |
| **Phase 2 ✅**（设计已批准） | `generate_with_neighbors`（§4 跨区块面剔除）；`MeshJobQueue` + `MeshPriority` 4 档枚举（§6） |
| Phase 7 | 贪婪网格化（§3 替换朴素逐面）；AO（§5）；视锥剔除（§9） |

下面 §3 贪婪算法、§5 AO、§9 视锥剔除均为 **Phase 7 设计**；Phase 2 实装使用朴素逐面 + 跨区块剔除。

---

## 二、u32 顶点压缩格式

### 位段布局

```
位 31 ────────────────────────────── 位 0
┌─────────────────────────────────────────┐
│  AO  │ Tex │  Face  │   z   │   y    │   x   │
│ 2bit │8bit │  3bit  │ 4bit  │ 8bit   │ 4bit  │
└─────────────────────────────────────────┘
       未使用：3 位（位段从右往左排列）
```

| 字段 | 位数 | 含义 | 范围 |
|---|---|---|---|
| `local_x` | 4 | 区块内 X | 0..16（含边界 16，故需 4+1 即可，但贪婪合并后位置可达 16，故用 5 位也行；保险用 4 位时把"16"用单独一位标记。本期方案：实际上贪婪面合并后顶点 x 范围 0..16，含 16 → 至少 5 位。**调整：x 用 5 bit，z 用 5 bit，face 占 3 bit，y 用 8 bit, tex 8 bit, ao 2 bit, 余 1 bit**） |

> **修正布局**（贪婪合并要求顶点能取到边界值，含 16）：

```
位 31 ─────────────────────────────────────── 位 0
┌──────────────────────────────────────────────┐
│ rsv │ AO  │ Tex │ Face │   z   │   y   │   x  │
│ 1bit│2bit │8bit │ 3bit │ 5bit  │ 8bit  │ 5bit │
└──────────────────────────────────────────────┘
   = 1 + 2 + 8 + 3 + 5 + 8 + 5 = 32 bits ✓
```

| 字段 | 位数 | 范围 | 含义 |
|---|---|---|---|
| `local_x` | 5 | 0..32 | 区块内 X（含上界 16）|
| `local_y` | 8 | 0..256 | 区块内 Y |
| `local_z` | 5 | 0..32 | 区块内 Z |
| `face` | 3 | 0..7 | 法线方向（PosX/NegX/PosY/NegY/PosZ/NegZ） |
| `texture_index` | 8 | 0..256 | 纹理图集槽位 |
| `ao_factor` | 2 | 0..4 | 4 等级 AO |
| `reserved` | 1 | — | 预留 |

> 5 位 X/Z 略奢侈（实际只用 0..17），但保留扩展余量更安全。

### Rust 编码

```rust
pub fn pack(local_x: u8, local_y: u8, local_z: u8,
            face: Face, texture: u8, ao: u8) -> u32 {
    debug_assert!(local_x <= 31);
    debug_assert!(local_y <= 255);
    debug_assert!(local_z <= 31);
    debug_assert!(face as u8 <= 7);
    debug_assert!(ao <= 3);

    (local_x as u32) & 0x1F
        | ((local_y as u32) & 0xFF) << 5
        | ((local_z as u32) & 0x1F) << 13
        | ((face as u32) & 0x7) << 18
        | ((texture as u32) & 0xFF) << 21
        | ((ao as u32) & 0x3) << 29
        // 第 31 位 reserved
}
```

### WGSL 解码

```wgsl
struct UnpackedVertex {
    world_pos: vec3<f32>,
    normal: vec3<f32>,
    uv_atlas: vec2<f32>,
    ao: f32,
};

fn unpack_vertex(packed: u32, chunk_origin: vec3<f32>) -> UnpackedVertex {
    let lx = f32(packed & 0x1Fu);
    let ly = f32((packed >> 5u) & 0xFFu);
    let lz = f32((packed >> 13u) & 0x1Fu);
    let face = (packed >> 18u) & 0x7u;
    let tex = (packed >> 21u) & 0xFFu;
    let ao_raw = (packed >> 29u) & 0x3u;

    var out: UnpackedVertex;
    out.world_pos = chunk_origin + vec3<f32>(lx, ly, lz);
    out.normal = face_normals[face];     // 查表
    // UV 由顶点的 (lx, ly, lz) % face_axes 决定（贪婪合并后宽高 > 1）
    out.uv_atlas = compute_uv(face, lx, ly, lz, tex);
    out.ao = f32(ao_raw) / 3.0;          // 0, 1/3, 2/3, 1
    return out;
}
```

UV 计算：贪婪合并后的大矩形，UV 从 (0,0) 一直平铺到 (w, h) — 平铺由片段着色器中 `frac(uv) + atlas_offset` 完成。

### 内存收益

| 顶点数 | 朴素 36 字节 | 压缩 4 字节 | 节省 |
|---|---|---|---|
| 1M | 36 MB | 4 MB | 89% |
| 10M | 360 MB | 40 MB | 89% |

---

## 三、贪婪网格化算法

> **Phase 7** 引入。Phase 2 实装使用 `generate_with_neighbors` 朴素逐面（每方块 6 面遍历），先把跨区块剔除做对，再在 Phase 7 替换为贪婪算法。下面是 Phase 7 设计。

### 思路
对每个面方向（6 个），按层（如 PosY 面就按 Y 层）扫描每一层 16×16 平面，把"需要绘制此面"的格子加入 mask，然后从 mask 中提取最大矩形（贪婪扩展宽高），生成一个大四边形。

### 算法步骤（以 PosY 面为例）

```
for ly in 0..CHUNK_Y {
    let mut mask: [[Option<MaskCell>; 16]; 16] = empty();  // mask[lx][lz]

    // 1. 建立 mask
    for lx in 0..16 {
        for lz in 0..16 {
            let here = chunk.get(lx, ly, lz);
            let above = if ly + 1 < CHUNK_Y {
                chunk.get(lx, ly + 1, lz)
            } else {
                neighbors.get_top(lx, lz)  // 跨区块查询
            };
            if needs_face(here, above, Face::PosY) {
                mask[lx][lz] = Some(MaskCell {
                    block: here,
                    texture: properties(here).texture_index,
                    ao: compute_ao(chunk, neighbors, lx, ly, lz, Face::PosY),
                });
            }
        }
    }

    // 2. 贪婪合并
    for lx in 0..16 {
        for lz in 0..16 {
            let Some(start) = mask[lx][lz].clone() else { continue; };

            // 向 +z 扩展宽度 w
            let mut w = 1;
            while lz + w < 16
                && mask[lx][lz + w].as_ref() == Some(&start) {
                w += 1;
            }

            // 向 +x 扩展高度 h
            let mut h = 1;
            'outer: while lx + h < 16 {
                for k in 0..w {
                    if mask[lx + h][lz + k].as_ref() != Some(&start) {
                        break 'outer;
                    }
                }
                h += 1;
            }

            // 生成 quad: 4 个 packed 顶点 + 索引
            emit_quad(lx, ly + 1, lz, h, w, Face::PosY, start);

            // 清掉已合并区域
            for dx in 0..h {
                for dz in 0..w {
                    mask[lx + dx][lz + dz] = None;
                }
            }
        }
    }
}
```

`MaskCell` 必须实现 `PartialEq`：纹理 + AO 都相同才能合并（不同 AO 不合并，避免颜色断层）。

### 6 个面方向

对每个方向重复一次以上流程，注意：
- PosY/NegY 按 Y 层扫描，mask 维度 (lx, lz)，扩展轴 (z 优先 → x)
- PosX/NegX 按 X 层扫描，mask 维度 (ly, lz)，扩展轴 (z 优先 → y)
- PosZ/NegZ 按 Z 层扫描，mask 维度 (lx, ly)，扩展轴 (x 优先 → y)

### 性能预期

| 场景 | 朴素顶点 | 贪婪顶点 | 比例 |
|---|---|---|---|
| 平地（一层草） | 65536 × 4 ≈ 260K | 4 | -99.99% |
| 山丘地形 | ~30K | ~3K | -90% |
| 复杂地形 | ~50K | ~10K | -80% |

---

## 四、跨区块面剔除

> **Phase 2** 实装。这是 Phase 2 体素单人模式的视觉正确性关键。

### 问题
普通"面剔除"只看同一 chunk 内的相邻方块。chunk 边界处的方块永远找不到邻居（其它 chunk 数据），导致边界面被多绘制。

### 解决
`generate_with_neighbors` 接收一个回调，能查询世界坐标的方块：

```rust
/// [Phase 2] 跨区块面剔除版本的网格化。
/// 朴素逐面遍历 chunk 内每个方块，对每个面查询 get_block_world 决定是否发射。
/// 同 chunk 内的查询也走这个回调（统一接口，回调内部自行判断 chunk 内/外）。
pub fn generate_with_neighbors(
    chunk: &Chunk,
    chunk_pos: ChunkPos,
    get_block_world: &dyn Fn(i32, i32, i32) -> BlockID,
) -> ChunkMeshCpu;
```

调用方（`client::mesh_jobs::run_until_budget`）：

```rust
let server_ref = server;  // &Server，从 Rc<RefCell<Server>> 借出
let mesh = chunk_mesh::generate_with_neighbors(
    chunk, chunk_pos,
    &|wx, wy, wz| server_ref.world.get_block_world(wx, wy, wz),
);
```

### 边界情况
- 邻居 chunk **未加载**：`get_block_world` 返回 `BlockID::AIR`（保守，绘制更多面，但不会撕裂网格）。当邻居 chunk 加载完成后，**当前 chunk 也需重网格化**（作为 dirty 触发条件之一）
- 邻居 chunk **加载但未生成**：同上

---

## 五、AO 计算

> **Phase 7** 引入。Phase 2 顶点 AO 一律为 3（最亮，无遮蔽）。

每个顶点的 AO 取决于附近 3 个方块（同面方向的两个邻接边 + 一个对角）：

```rust
fn vertex_ao(side1: bool, side2: bool, corner: bool) -> u8 {
    if side1 && side2 { return 0; }   // 完全遮蔽
    let count = side1 as u8 + side2 as u8 + corner as u8;
    3 - count.min(3)                  // 0..=3
}
```

参考：经典体素 AO 算法（Mikolalysenko / 0fps）。

> 注意：贪婪合并要求合并的方块所有顶点 AO 相同；不同 AO 阻断合并。

---

## 六、网格化任务调度

> **Phase 2** 实装。

### 优先级

```rust
pub enum MeshPriority {
    Critical,   // 玩家正站立的 chunk
    High,       // 玩家附近 1 chunk 范围
    Medium,     // 渲染距离内
    Low,        // 边界 chunk / 因邻居加载触发的重网格化
}
```

### 分帧预算

每渲染帧最多花 `MESH_BUDGET_MS = 4ms` 跑网格化。超过预算时停下，剩余下一帧继续。

```rust
pub fn run_until_budget(&mut self, budget_ms: f32, server: &Server, renderer: &mut Renderer) {
    let start = performance_now();
    while performance_now() - start < budget_ms as f64 {
        let Some(pos) = self.pop_highest_priority() else { break; };
        let Some(chunk) = server.world.chunks.get(&pos) else { continue; };
        let mesh = chunk_mesh::generate_with_neighbors(
            chunk, pos,
            &|wx, wy, wz| server.world.get_block_world(wx, wy, wz),
        );
        renderer.upload_chunk_mesh(pos, &mesh);
        self.pending.remove(&pos);
    }
}
```

`performance_now()` 通过 `web_sys::Performance::now()` 获取毫秒级时间戳。

### 触发条件

将 chunk 加入网格化队列：
- 首次加载（Local 模式由 `ChunkLoader.update` 触发；Phase 5 Remote 模式由 ChunkSnapshot 组装完成触发）
- 方块更新（自身或相邻 chunk 边界方块）— Phase 3
- 邻居 chunk 由"未加载"变"已加载"时 — Phase 2 由 `ChunkLoader.update` 显式触发（见 `docs/modules/client.md` §6.7）
- 玩家走出后再回来 — Phase 2 由 `ChunkLoader.update` 触发

### 防重复
`pending: HashSet<ChunkPos>` 防止同一 chunk 入队两次。需要重新网格化时设 `dirty` 标记，下一帧 dequeue 后再判断是否仍 dirty。

---

## 七、与远程数据流的协同

### Local-Only / Host
- 本地 `server.world` 改 → 标记 dirty → 入队（Phase 3 后挖放，Phase 2 仅 ChunkLoader 滚动触发）
- 同步快

### Remote（[Phase 5]）
- 收到 `ChunkSnapshot` → assembler 组装完成 → `world_view.insert(pos, chunk)` → 入队
- 收到 `BlockUpdate` → `world_view.set_block` → 该 chunk + 6 邻居（如果方块在边界）入队

### 边界方块更新
方块在 `lx == 0` / `lx == 15` 等边界时，邻居 chunk 也需重网格化（因为跨区块剔除可能改变）。

```rust
fn enqueue_with_neighbors(&mut self, pos: Position, priority: Priority) {
    self.enqueue(pos.to_chunk_pos(), priority);
    let lx = pos.x.rem_euclid(16);
    let lz = pos.z.rem_euclid(16);
    if lx == 0 { self.enqueue(ChunkPos { x: pos.to_chunk_pos().x - 1, z: pos.to_chunk_pos().z }, priority); }
    if lx == 15 { self.enqueue(ChunkPos { x: pos.to_chunk_pos().x + 1, z: pos.to_chunk_pos().z }, priority); }
    if lz == 0 { /* z-1 */ }
    if lz == 15 { /* z+1 */ }
}
```

---

## 八、CPU 数据结构

```rust
pub struct ChunkMeshCpu {
    pub vertices: Vec<u32>,        // packed
    pub indices: Vec<u32>,         // 每 quad 6 个索引
    pub bounds: Aabb,              // 视锥剔除用
}

pub struct ChunkMeshGpu {
    pub vertex_buffer: wgpu::Buffer,
    pub index_buffer: wgpu::Buffer,
    pub index_count: u32,
    pub bounds: Aabb,
}
```

`Renderer::upload_chunk_mesh` 把 `ChunkMeshCpu` 转 `ChunkMeshGpu`（创建 buffers + 写数据）。

---

## 九、视锥剔除

> **Phase 7** 引入。Phase 2 渲染距离 6 时 chunk 数 ≈ 169，朴素逐面 + 跨区块剔除下顶点量可控，本期不做视锥剔除。

每帧渲染前根据相机视锥过滤需要 draw 的 chunk：

```rust
fn visible_chunks(camera: &Camera, mesh_map: &HashMap<ChunkPos, ChunkMeshGpu>) -> Vec<ChunkPos> {
    let frustum = Frustum::from_view_proj(camera.view_proj());
    mesh_map.iter()
        .filter(|(_, mesh)| frustum.intersects_aabb(&mesh.bounds))
        .map(|(pos, _)| *pos)
        .collect()
}
```

视锥与 AABB 交叉测试用 6 平面 dot 测试。

---

## 十、不在范围

- 多分辨率 LOD（远处 chunk 用更粗网格）— v3
- GPU-driven culling / GPU 网格化（compute shader）— v3
- 动态光照网格更新 — 不实装，光照仅 AO
- Voxel cone tracing — 不做
- 自定义方块形状（楼梯、半砖）— 仅完整方块
