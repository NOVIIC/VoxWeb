# 统一体素设计

> **状态**：第一版核心闭环已落地到 cell 保真路径。已实现 `MaterialID`/`MaterialProperties` 过渡层、`MaterialCell`、`FieldChunk`/`Column`/`Span`、OPFS FieldChunk v2、active FreeObject world.json 持久化、网络 `FieldSnapshot`/`FieldDelta`、石砖/基岩材质、基岩托底、基于材质属性的基础挖放仲裁、`ImmediateRelaxation` 软材质局部下落/滑落，`FloatingOnly` 硬材质小连通块的 FreeObject 提取、active AABB 动态下落、`FreeObjectSpawn/State/Project` 同步、客户端动态碰撞/raycast 查询，以及 `SmoothGranular` 的扩邻域高度场平滑提面 / raycast / 选中 / 客户端碰撞共用查询；运行时预测回滚、颗粒松弛、FreeObject sample 和 project delta 均保留完整 `MaterialCell`。真实流体、完整 Surface Nets / Marching Cubes、旋转刚体、碰撞反弹和碎裂尚未实现，作为固体/颗粒闭环之后的增强项。
> **何时阅读**：重构世界表示、物理系统、方块交互、实体系统之前
> **关联文档**：[`README.md`](../README.md) · [`architecture.md`](architecture.md) · [`features/physics.md`](features/physics.md) · [`features/meshing.md`](features/meshing.md) · [`features/persistence.md`](features/persistence.md) · [`networking/protocol.md`](networking/protocol.md) · [`modules/core.md`](modules/core.md) · [`modules/server.md`](modules/server.md) · [`modules/render.md`](modules/render.md) · [`modules/client.md`](modules/client.md)

---

## 一、方案定义

本设计描述 VoxWeb 的下一版世界模型：把当前“`Chunk` 里存 `BlockID`、所有方块都是完整 1m 立方体”的世界，升级为一个仍然易玩的浏览器体素沙盒，但底层用材质语义表达方块、沙土、坍塌、掉落物和未来流体。

新方案的核心不是“把所有东西都交给 Marching Cubes”，而是：

> 世界的权威状态统一为 **MaterialField + FreeObject + MaterialRegistry**；
> 不同材质按自己的规则进入 blocky、smooth、granular、rigid、future fluid 等表现和模拟路径。

换句话说，统一发生在**世界数据、材质生命周期和网络/存档语义**上；渲染、碰撞、颗粒、硬体和未来流体仍然可以使用不同求解器。玩家体验仍然接近简单沙盒：选材质、挖、放、建造；拟真只体现在材质反馈更自然，而不是让玩家管理工程结构。

### 1.1 第一版目标

第一版要落地的体验和系统边界：

1. **简单沙盒手感**：保留创造模式快捷栏，材料无限，不显示库存数量；玩家只做选材质、挖掘、放置。
2. **硬材料仍像方块**：石头、木头、玻璃、未来石砖保持清晰方块视觉，建筑只有整个连通块完全浮空时才坍塌。
3. **软材料立刻模拟**：泥土、草、沙子放置后立刻进入局部松弛，自动找坡、滑落或堆积。
4. **静态世界和动态物体同源**：失稳的局部材质从 `MaterialField` 提取为 `FreeObject`，静止后再投影回场。
5. **世界底部是基岩**：最低层使用不可破坏、无物理、不进物品栏的基岩托底。
6. **水先不做真实流体**：当前水只作为静态透明占位或简化水块；真实流体 solver 留到固体和颗粒闭环稳定之后。
7. **Host 权威**：多人中挖放、坍塌、提取、投影仍由 Host / Local-Only 权威推进，Remote 只预测和纠偏。

### 1.2 明确不做

这些不是第一版内容：

- 不做玩家区域锁定、结构冻结或权限式保护工具。
- 不做生存采集、堆叠数量、背包质量守恒闭环。
- 不强制所有材质使用同一个提面算法。
- 不用渲染 mesh 作为唯一碰撞真相。
- 不让水参与传播、侵蚀、湿润或高频网络同步。

### 1.3 核心取舍

| 问题 | 绝对统一版本 | 本设计采用 |
|---|---|---|
| 世界原子 | 所有东西都是同一种 GridPoint | 权威状态统一为 MaterialCell / FreeObject |
| 渲染 | 全部 Marching Cubes / Surface Nets | 按材质选择 blocky / smooth / fluid extractor |
| 碰撞 | 全部密度对密度 | broadphase + gameplay proxy + density/hull narrowphase |
| 玩家 | 也是材质采样点集合 | 玩家保留角色控制器，读写统一世界查询 |
| 水 | 和固体同一种物理 | 第一版静态占位；未来独立流体求解器仍写入统一材质场 |
| 玩家建筑 | 额外操作绕过物理 | 由材质属性、放置规则和局部稳定性边界保证简单手感 |

### 1.4 三层模型

```
权威世界层：
  MaterialField / FreeObject / MaterialRegistry

模拟层：
  hard solid stability
  granular relaxation
  rigid FreeObject motion
  future fluid solver
  player controller query

表现与查询层：
  blocky extractor for hard materials
  surface nets / marching cubes for soft materials
  transparent renderer / future fluid renderer
  collision proxies and ray queries
```

### 1.5 必须保持的约束

1. **场内质量守恒**：坍塌、投影、沉积不能凭空复制或吞掉材质；第一版创造模式把玩家快捷栏视为外部无限源/汇，不追踪库存数量。
2. **Host 权威**：多人场景下权威世界只在 Host / Local-Only 推进，Remote 只预测和协调。
3. **材质决定默认行为**：玩家选择材质，不直接选择笔刷；kernel、休止角、凝聚力来自材质属性。
4. **求解器可以分层**：统一语义不等于统一算法。不同材质可以路由到不同求解器。
5. **代理结构合法**：AABB、碰撞 hull、SDF、空间哈希、LOD 都是实现细节，不破坏统一设计。

---

## 二、当前代码结构观察

本节只记录当前仓库的结构事实，用来校准方案边界；它不是迁移计划。

| crate / 模块 | 当前职责 | 对统一体素方案的自然归属 |
|---|---|---|
| `core::block` | `BlockID` + 编译期 `BlockProperties`；已新增 `MaterialID`/`MaterialProperties` 过渡别名、`MaterialCell` 原型和 `MaterialRegistry` 查询入口 | 后续迁移为独立 `MaterialRegistry` 与 FieldChunk schema |
| `core::chunk` | 16×256×16 `Vec<BlockID>`、坐标 | 继续作为当前渲染/碰撞适配镜像，网络、存档和运行时 MaterialCell 读写已由 FieldChunk 承担 |
| `core::field` | 已新增 `FieldChunk`、`Column::{Spans,Dense}`、`Span` 与 `Chunk` 双向转换，并由 `server::World` 同步维护；OPFS、网络快照和 `World::get_cell_world/set_cell` 均使用 FieldChunk / MaterialCell 语义；`free_object_refs` 由运行时 active object 表重建 | 后续扩展 column delta 和 region hash |
| `core::object` | `FreeObject`、`ObjectSample`、`MaterialSummary`、`CollisionProxy` 与 `FreeObjectState`；`ObjectSample` 保存完整 `MaterialCell`，并保留旧 `material/mass` 字段兼容 active object 存档 | 后续扩展旋转、碰撞代理和碎裂/降级状态 |
| `core::protocol` | bincode 消息、FieldRequest、FieldSnapshot、FieldDelta、BlockID 操作请求 | 扩展为 ApplyKernel、FreeObject 状态和投影事件 |
| `server::world` | `field_chunks` MaterialField、同步 dense `chunks` 镜像、active `free_objects`、地形、dirty、LRU；运行时读写入口已是 `get_cell_world/set_cell` | 后续进一步减少旧 BlockID 调用面，并细化 active FreeObject 模拟队列 |
| `server::physics` | 挖放范围、方块状态、玩家 AABB 重叠校验；已实现 `ImmediateRelaxation` 软材质局部下落/滑落和 `FloatingOnly` 小硬材质连通块即时提取/投影 | 后续扩展质量守恒 kernel、可见刚体运动和更细支撑图 |
| `render::chunk_mesh` | 硬方块贪婪网格化、透明方块网格、`SmoothGranular` 高度场平滑提面 | 后续扩展为更完整 extractor 集合：blocky / smooth Surface Nets 或 Marching Cubes / fluid / object mesh |
| `client::physics` | 玩家 AABB 分轴碰撞、本地预测；`SmoothGranular` 使用共享高度场做客户端碰撞 | 保留角色控制器，后续查询更完整统一世界碰撞代理 |
| `client::raycast` | DDA 命中 solid 方块；`SmoothGranular` 使用共享高度场做精确命中 | 后续扩展为更完整平滑材质 field ray query |
| `client::mesh_jobs` | 分帧 chunk mesh 任务队列 | 继续承载各 extractor 的分帧预算 |
| `client::storage` | OPFS chunk 读写；`world.json.active_free_objects` 保存跨 tick active FreeObject | 后续扩展 schema 迁移、对象压缩和损坏修复 |
| `net` | WebRTC / WS 中继字节通道 | 仍只负责传输，不理解世界语义 |

这个结构支持“统一世界层在 `core`/`server`，表现层在 `render`，编排和预测在 `client`”的分层方式。方案需要重写数据模型，但不需要打破 crate 的所有权边界。

---

## 三、基本单位：材质单元

### 3.1 为什么不用“一个格点就是一个方块”

原始设想里的“一个 GridPoint 密度 255 生成一个方块”在 Marching Cubes 语义下不成立。Marching Cubes 读取的是单元格 8 个角点的标量值；只有一个角点高密度时，通常只会生成角落小面，不会得到完整 1×1×1 方块。

因此权威世界不要把“提面算法的采样点”当作库存和物理的唯一原子。更准确的划分是：

| 概念 | 用途 | 是否权威 |
|---|---|---|
| `MaterialCell` | 世界中一个体素单元内的材质质量 | 是 |
| vertex/corner sample | smooth extractor 输入，由 cell 场过滤或采样得到 | 否 |
| mesh vertex | GPU 表现数据 | 否 |
| collision proxy | 物理加速结构 | 否 |

### 3.2 MaterialCell

```rust
struct MaterialCell {
    occupancy: u8,              // 0 = 空，255 = 满
    primary: MaterialID,         // occupancy = 0 时忽略
    secondary: Option<MixSlot>,  // 可选，用于少量混合或过渡
    flags: CellFlags,            // generated / dirty / stable hint 等
}

struct MixSlot {
    material: MaterialID,
    occupancy: u8,
}
```

规则：

- `occupancy = 0` 表示空气；空气不是需要保存的材质。
- `primary + secondary` 的总占用不得超过 255。
- 多材质只允许少量局部混合。复杂反应应产生新材质，而不是无限增加通道。
- 硬方块的“完整一格”是 `MaterialCell { occupancy: 255, primary: stone }`，由 blocky extractor 表现为立方体。
- 平滑材质通过 kernel 和 extractor 转成连续表面，不要求每个 cell 都画成立方体。

### 3.3 Cell、格点、单元格

```
整数坐标 (x, y, z) 表示一个 cell 的最小角或逻辑地址。

硬材质：
  cell occupied -> blocky extractor 输出 1m 立方体面

软材质：
  cell occupancy -> 过滤成 corner samples
  corner samples -> Surface Nets / Marching Cubes 输出平滑表面
```

这使得“石砖仍是方块”和“沙堆可以平滑”同时成立，而不需要让 Marching Cubes 独自承担所有形状。

---

## 四、存储：Column Store + Dense 退化

### 4.1 目标

存储层要服务两种完全不同的区域：

- 自然地形：大量连续同质层，适合 span 压缩。
- 玩家建筑：局部高熵编辑，适合 dense 或小块页缓存。

因此建议把 Span Column Store 定义为**逻辑格式**，并允许高熵区域自动退化。

### 4.2 结构

```rust
struct FieldChunk {
    columns: [Column; 16 * 16],
    free_object_refs: Vec<ObjectID>, // 可选：本 chunk 活跃动态物体索引
}

enum Column {
    Spans(Vec<Span>),
    Dense(Box<[MaterialCell; 256]>),
}

struct Span {
    y_start: u16,
    length: u16,
    cell: MaterialCell,
}
```

### 4.3 运行时查询

Span 适合存档和网络快照，但不一定适合每次物理查询。实现可以保留这些缓存：

- 最近活跃 chunk 的 dense column cache。
- extractor 需要的 corner sample cache。
- collision proxy cache。
- dirty column / dirty region，而不是整 chunk dirty。

关键是外部 API 查询的是 `MaterialField`，不关心内部当前是 span 还是 dense。

---

## 五、材质属性

### 5.1 定义

```rust
struct MaterialProperties {
    // 视觉
    visual_class: VisualClass,
    texture_index: u16,

    // 放置与挖掘
    placement_kernel: PlacementKernel,
    break_kernel: BreakKernel,
    placement_unit_mass: u16,
    hardness: f32,
    appears_in_hotbar: bool,
    breakable: bool,

    // 物理
    mechanics: MechanicsClass,
    angle_of_repose: f32,
    cohesion: f32,
    compressive_strength: f32,
    shear_strength: f32,
    density_kg_m3: f32,
    restitution: f32,
    friction: f32,
    stability: StabilityPolicy,
}

enum VisualClass {
    HardBlocky,
    SmoothGranular,
    Fluid,
    Foliage,
}

enum MechanicsClass {
    StaticSolid,
    Granular,
    RigidBreakable,
    Fluid,
    Decorative,
}

enum StabilityPolicy {
    NoPhysics,
    FloatingOnly,
    ImmediateRelaxation,
    FutureFluid,
}
```

### 5.2 第一版物质清单

当前游戏快捷栏有 9 格，其中 1-8 是现有物质，第 9 格已改为石砖。石砖和基岩均已有 `BlockID`、材质属性、程序化纹理和基础 UI 色块。

| hotbar | 物质 | 当前状态 | 视觉 | 物理策略 | 第一版行为 |
|---|---|---|---|---|---|
| 1 | 石头 | 已存在 | HardBlocky | FloatingOnly / RigidBreakable later | 硬质通用材料；玩家建筑只有整个连通块完全浮空时才坍塌，后续可让大岩块强冲击碎裂 |
| 2 | 泥土 | 已存在 | SmoothGranular | ImmediateRelaxation | 中等凝聚软材料；放置后立刻局部松弛，形成粗糙土坡或土堆，不适合保持垂直薄墙 |
| 3 | 草 | 已存在 | SmoothGranular + surface layer | ImmediateRelaxation | 作为泥土表层/地表材料；主体按泥土物理处理，草面只影响外观和地表识别 |
| 4 | 沙子 | 已存在 | SmoothGranular | ImmediateRelaxation | 低凝聚颗粒；放置后立刻按休止角滑落和堆积，不能稳定形成直墙 |
| 5 | 木头 | 已存在 | HardBlocky | FloatingOnly | 建筑硬材质；可做梁、柱、地板，只有完全浮空的连通块才进入坍塌/掉落流程 |
| 6 | 树叶 | 已存在 | Foliage | Decorative | 轻质装饰材料；不作为主要承重支撑，直接破坏即可移除，第一版不做枯萎或传播 |
| 7 | 玻璃 | 已存在 | HardBlocky / Transparent | FloatingOnly | 透明建筑硬材质；可稳定建造，完全浮空才坍塌，破坏时可直接消失或后续扩展为碎片 |
| 8 | 水 | 已存在 | Transparent placeholder | NoPhysics | 第一版不做真实流体；保留为静态透明占位/简化水块，不传播、不侵蚀、不参与 FreeObject |
| 9 | 石砖 | 已存在 | HardBlocky | FloatingOnly | 主要建筑块；比石头更偏人造稳定材料，只有完全浮空才坍塌，不参与自然地形生成 |
| - | 基岩 | 已存在 | HardBlocky | NoPhysics | 世界最底层材料；无法破坏、无物理模拟、不出现在物品栏，不参与掉落或坍塌 |

### 5.3 世界底层

世界最底层应生成基岩，而不是石头。基岩是世界边界材料：

- 位于最低 y 层，用作稳定基底和防止玩家挖穿世界的硬边界。
- `breakable = false`，玩家无法挖掘，也不会作为挖掘结果进入创造模式快捷栏。
- `appears_in_hotbar = false`，不出现在物品栏和默认快捷栏。
- `stability = NoPhysics`，不进入支撑图、颗粒松弛、FreeObject 提取或投影流程。

### 5.4 材质路由

材质属性不只是参数表，也决定模拟和表现的路由：

```
MaterialProperties
  -> placement kernel
  -> stability solver
  -> relaxation solver
  -> mesh extractor
  -> collision proxy builder
  -> creative palette rule
  -> optional survival stacking rule later
```

这比“所有材质只改几个数字，算法完全一样”更实际，也更容易调出好手感。

---

## 六、编辑：材质天然单位

### 6.1 玩家交互

玩家交互仍保持简单：

```
滚轮 / 快捷栏: 选择材质
左键: 挖掘一个材质单位
右键: 放置一个材质单位
```

玩家不直接选择笔刷半径和衰减曲线，也不管理额外的物理约束。材质自己决定 kernel 和稳定性边界。

### 6.2 放置

第一版保持创造模式体验：快捷栏材料无限，不显示数量，不做采集-库存闭环。一次右键操作从外部材料源写入一个 `placement_unit_mass`；kernel 只负责决定目标区域的质量分布：

```rust
struct KernelWrite {
    offset: IVec3,
    material: MaterialID,
    mass: u16,
}
```

放置必须满足局部写入规则：

1. 先模拟 kernel 写入，计算容量和冲突。
2. 若能完全写入，则直接提交；创造模式不扣库存。
3. 若局部满格，可尝试有限次邻近扩散。
4. 若仍有剩余，操作要么整体拒绝，要么只扣除实际写入量；规则必须固定，不能隐式吞掉剩余质量。

建议第一版使用**整体拒绝**，后续再支持“写入部分并返还剩余”。

### 6.3 挖掘

挖掘是放置的反操作：

- `break_kernel` 从命中位置附近取走质量。
- 优先取命中材质；混合 cell 中按 dominant material 或命中表面材质取。
- 第一版取走的质量进入创造模式外部汇，不增加玩家库存数量。
- 若取走后 cell 总质量为 0，清空 material。
- 挖掘会触发局部稳定性检查、mesh dirty 和 persistence dirty。

### 6.4 多材质叠加

同一 cell 发生不同材质写入时，采用固定优先级：

1. 同材质累加，占用不超过 255。
2. 有空 secondary slot 时写入 secondary。
3. 若两个材质定义了反应规则，生成新材质。
4. 若无法混合且目标为硬材质，放置拒绝。
5. 若无法混合且目标为软材质，可触发挤出/滑落，把多余质量写到邻近 cell。

这样能避免“无限材质通道”导致存储和提面都失控。

---

## 七、静态与动态

### 7.1 FreeObject

静态世界和动态物体共享材质语义，但不共享完全相同的运行时结构。

```rust
struct FreeObject {
    id: ObjectID,
    transform: Transform,
    velocity: Vec3,
    angular_velocity: Vec3,
    samples: Vec<ObjectSample>,
    material_summary: MaterialSummary,
    mass: f32,
    collision_proxy: CollisionProxy,
    state: FreeObjectState,
}

struct ObjectSample {
    local_pos: Vec3,
    material: MaterialID,
    mass: u8,
}

enum CollisionProxy {
    Aabb(Aabb),
    ConvexHull { vertices: Vec<Vec3> },
    SampleCloud,
}
```

FreeObject 的 `samples` 保留材质质量，`collision_proxy` 服务运行时性能。代理结构不是新实体类型，而是动态材质团的加速表示。

### 7.2 提取：Static -> Dynamic

触发条件：

- 支撑失败。
- 爆炸或强冲击。
- 玩家挖掘导致连通块失稳。
- 求解器判断某个局部区域应进入动态过程。

提取流程：

1. 找到受影响区域的连通 component。
2. 根据材质凝聚力、质量、最大对象体积决定打包粒度。
3. 从 MaterialField 移除对应质量。
4. 生成一个或多个 FreeObject。
5. 标记相关 chunk / column / mesh / collision proxy dirty。

提取必须有上限：

- 超大山体不能一次变成百万 sample 刚体。
- 超过阈值的 component 可分块、分层，或走局部崩落近似。
- 远离玩家的细节可以直接求解成静态沉积，不必创建可见 FreeObject。

### 7.3 动态过程

动态物体使用分层物理：

```
broadphase:
  AABB / spatial hash / chunk bins

narrowphase:
  hard object: hull vs field / hull vs hull
  granular object: sample cloud vs field
  future fluid: 不走 FreeObject，交给 fluid solver

resolution:
  impulse / friction / damping / fragmentation
```

**重要约束**：FreeObject 的动画必须来自同一个权威动态状态，不能是已经投影后的纯视觉补间。也就是说，渲染位置、碰撞箱、raycast 命中、玩家碰撞和最终投影都要读取同一个 `FreeObject.transform` / `collision_proxy`。

第一版升级路径不需要一步到位做完整刚体，可以先做“可见 AABB 动态体”：

1. `FreeObjectSpawn`：支撑失败时，从 `MaterialField` 移除 component，生成 active `FreeObject`，广播样本 cell、初始 transform、AABB、速度和材质摘要。
2. `FreeObjectState`：Host / Local-Only 每个逻辑 tick 推进动态对象；Remote 应用权威状态，不自己决定落点。
3. **碰撞箱真实存在**：玩家 AABB、raycast、放置校验和其它 FreeObject broadphase 都把 active FreeObject AABB 纳入查询；客户端预测可提前显示，但最终以 Host 状态为准。
4. **静止判定**：速度低于阈值、接触稳定面且一段时间内不再移动后，进入 `Settled`，再执行投影。
5. `FreeObjectProject`：只在动态物体真正静止后发送；投影 delta 是生命周期结束事件，而不是动画的起点。

这样“动画”只是动态物体状态随时间变化的可见结果。下落、滑动、撞击、反弹、碎裂都应从模拟层产生；如果某一类效果还没有模拟，就不要用无碰撞的视觉特效假装已经存在。

凝聚力控制碎裂：

| 凝聚力 | 动态行为 |
|---|---|
| 极高 | 保持整体刚体，只有强冲击碎裂 |
| 高 | 整体运动，边缘可能掉碎片 |
| 中 | 碰撞后分裂成若干团 |
| 低 | 快速散成颗粒沉积 |
| 无 | 不生成 FreeObject，直接在场中传播 |

### 7.4 投影：Dynamic -> Static

FreeObject 静止后不能简单“写入最近格点”，否则会产生穿插、质量丢失和体积漂移。投影必须是一个明确算法：

1. 根据 transform 把 sample 转到世界空间。
2. 第一版整格 sample 使用 round 后目标 cell，并在附近 Y 偏移内寻找可投影空位；后续更细颗粒/碎裂再使用 splatting 权重分配到附近 cell。
3. 检查每个目标 cell 的剩余容量；第一版要求目标 cell 为空，不覆盖已有静态场。
4. 与硬材质冲突时优先保留 active object 等待下一 tick，后续可扩展为反弹、滑落或碎裂。
5. 写入后对局部区域运行松弛和稳定性检查。
6. 若仍有无法容纳的质量，保留为小 FreeObject 或拒绝静止。

投影完成后，FreeObject 才能销毁。

---

## 八、结构完整性

### 8.1 不使用单点支撑规则作为正式模型

“下方四个角点至少一个 density > 128”可以作为原型启发，但正式模型不能只看有/无支撑。否则会出现细柱撑巨石、边缘抖动、斜向悬挂等问题。

### 8.2 支撑图

稳定性求解以局部支撑图为核心：

```rust
struct SupportEdge {
    from: CellPos,
    to: CellPos,
    normal_force_capacity: f32,
    shear_force_capacity: f32,
}

struct StabilityComponent {
    cells: Vec<CellPos>,
    total_mass: f32,
    center_of_mass: Vec3,
    support_edges: Vec<SupportEdge>,
}
```

求解关注：

- 连通区域是否连接到稳定基底。
- 支撑面积和承重能力是否足够。
- 重心投影是否落在可承受支撑区域内。
- 悬挑长度是否超过材质限制。
- shear force 是否超过材质抗剪。

第一版先对硬建筑材质使用 `FloatingOnly` 简化策略：

- 石头、石砖、木头、玻璃等硬建筑材质不做承重细算；墙、地板、台阶、小跨度悬挑默认保持稳定。
- 只有一个硬材质连通块与基岩、地面或其它稳定硬材质完全断开时，才进入坍塌/掉落流程。
- 完全浮空检测可以先用局部 BFS 判断 component 是否接触稳定基底；后续再把大型自然岩体升级为更细的支撑图。
- 基岩永远视为稳定基底，且不进入 component 提取。

### 8.3 滞后与增量

稳定性必须避免每 tick 全世界扫描：

- 编辑、爆炸、投影和未来流体侵蚀只把附近 region 入队。
- 通过 BFS / flood fill 找局部 component。
- 使用两个阈值：`collapse_threshold` 和 `settle_threshold`，避免临界状态反复坍塌/静止。
- `ImmediateRelaxation` 软材质在放置后立刻进入局部松弛；预算不足时分帧推进，但玩家看到的第一反馈必须是材质正在自然找坡。

### 8.4 坍塌结果

支撑失败不等于消失：

```
支撑失败
  -> component 分类
  -> 硬材质生成 FreeObject
  -> 软材质进入 granular relaxation 或小 FreeObject
  -> 未来流体交给 fluid solver
  -> 投影 / 沉积回 MaterialField
```

---

## 九、视觉与碰撞

### 9.1 多 extractor，而不是单一 Marching Cubes

| 材质视觉类 | Extractor | 说明 |
|---|---|---|
| HardBlocky | blocky greedy / face meshing | 保留笔直方块、锐边、低成本 |
| SmoothGranular | 当前共享高度场平滑提面 / raycast / 选中 / 客户端碰撞；后续 Surface Nets / Marching Cubes | 沙、土、雪等平滑坡面 |
| RigidBreakable FreeObject | 当前 active AABB sample cube 渲染；后续 local mesh cache + transform | 动态岩块、碎块 |
| Fluid | future fluid surface extractor + transparent pass | 水面、透明排序、波动；第一版水只做静态透明占位 |
| Mixed boundary | priority / blend / seam resolver | 处理硬软交界 |

统一的是 extractor 输出的 mesh 接口，而不是 extractor 算法。

### 9.2 硬软交界

硬材质和软材质交界需要专门规则：

- 硬材质表面优先保留锐边。
- 软材质可以贴合硬表面，但不能侵入满格硬 cell。
- 交界处法线、AO、纹理选择由 dominant material 和接触面决定。
- 如果 Surface Nets 与 blocky mesh 产生缝隙，使用 seam resolver 或让软表面采样硬材质占用作为边界条件。

### 9.3 碰撞查询

碰撞不直接使用渲染 mesh 作为唯一真相。推荐查询层：

| 查询 | 策略 |
|---|---|
| 玩家移动 | AABB / capsule vs collision field |
| 硬方块命中 | DDA |
| 平滑材质命中 | field raymarch / SDF approximation |
| FreeObject 碰撞 | broadphase AABB + hull/sample narrowphase |
| 稳定性 | cell graph / support graph |

这样既保留玩法稳定性，也不牺牲统一世界语义。

### 9.4 FreeObject 的碰撞与渲染一致性

当前代码中的硬材质坍落已从“服务端立即投影 + 客户端短动画”升级为 active AABB 动态体。运动中的石块会挡住玩家、可被 raycast 命中，并且放置校验不会允许新方块写入 active object 当前体积。当前第一版仍只做平移、重力、静态场接触和最终投影，不做旋转、反弹或碎裂。active FreeObject 纳入世界查询：

```rust
struct WorldQuery {
    static_field: MaterialField,
    active_objects: SpatialIndex<ObjectID, Aabb>,
}

enum QueryHit {
    StaticCell(Position),
    FreeObject(ObjectID),
}
```

查询规则：

- 玩家移动先做静态场碰撞，再查询 active FreeObject AABB；若碰到正在下落的硬对象，按动态对象表面吸附或推开。
- raycast 先比较静态 cell 命中距离和 FreeObject AABB / hull 命中距离，返回最近命中。
- 放置校验不能把方块放进 active FreeObject 当前占用体积。
- 投影前的 FreeObject 不应同时存在于静态场中，否则会出现“看见一个在动的块，但碰撞在终点”的双重真相。

第一版可接受的简化：

- FreeObject 只有平移，没有旋转。
- 碰撞代理先用 component AABB 或 sample-cloud AABB，不做 convex hull。
- 只处理重力、地面接触、简单滑动和阻尼；反弹、碎裂延后。
- 每 tick 限制 active object 数和 sample 数；超预算对象可以直接走静态沉积近似，但必须明确标记为降级路径。

### 9.5 软材质平滑度与自然地形

草、土、沙看起来不够平滑有两个不同原因：

1. **自然地形高度太陡**：当前地形是单通道 Perlin 直接映射高度，缺少坡度限制、侵蚀感和平原/丘陵分区。即使平滑 extractor 工作正常，相邻列高度差过大时仍会产生台阶和陡墙。
2. **当前 SmoothGranular 仍以满格 cell 为输入**：高度场只在可见顶部做插值，底层质量仍是 1m 整格；没有半格 occupancy、column mass 或真正的 Surface Nets，因此在侧面、硬软交界和悬崖处仍会暴露格子结构。

地形生成应先降低“格子台阶”的输入压力：

- 使用低频 fBm 叠加，而不是单一 Perlin 高度；高频项只做小幅细节。
- 对高度图做局部坡度限制或轻量 thermal erosion，让草/土列之间的高度差更常落在 0..1m。
- 生成平原、丘陵、山地分区；默认出生点附近偏平缓，山地放到远处或特定 biome。
- 草不应作为整格体积独立堆叠；更合理的是“泥土主体 + 顶部草层/表面材质”，草面只影响纹理、颜色和 raycast 命中材质。

提面层再继续升级：

- 以 column top height / occupancy cache 作为 SmoothGranular 输入，而不是只看 `BlockID` 是否存在。
- 顶面跨 cell 合并生成连续三角网，侧面使用 skirt 或 seam resolver 接硬材质。
- 法线从邻域高度梯度计算，并做材质相关平滑；土/草可以更圆润，沙可保留略尖的堆积脊。
- 纹理和颜色加入低频宏观变化，避免每个 1m cell 的图案重复暴露网格。

### 9.6 “仍然像一格一格方块”是不是问题

这不是单一的是/否问题。统一体素方案应区分**权威原子**和**视觉原子**：

- 对石头、石砖、木头、玻璃等建筑硬材质，方块感是产品特性。玩家需要清楚的 1m 模数、可预测放置和可靠建筑边界。
- 对草、土、沙、雪、水和自然地形，明显的一格一格视觉通常是问题。它会削弱“软材质”和“自然地貌”的可信度。
- 对交互和网络，cell 仍然是合理权威单位。消除视觉网格不等于取消 cell；它意味着 extractor、碰撞代理和材质贴图不再把 cell 边界原样暴露出来。

因此目标不是“全世界都不再像体素”，而是：

| 区域 | 应保留方块感 | 应隐藏方块感 |
|---|---|---|
| 玩家建筑、硬材质、选中框、快捷栏预期 | 是 | 否 |
| 自然草地、土坡、沙堆、未来水面 | 否 | 是 |
| 地下石层、人工挖掘断面 | 部分保留 | 只在软材质断面弱化 |

如果后续发现自然地形仍强烈读成“每格一个方块”，优先调地形坡度、软材质 occupancy 和提面/贴图，而不是把硬建筑也强行改成 Marching Cubes。

---

## 十、简单沙盒边界

更拟真的材质系统不应把玩家操作变成工程模拟器。玩家可见规则：

- 玩家只通过材质选择、挖掘、放置影响世界。
- 不提供额外的结构保护工具。
- 硬建筑材质默认可用于可靠建造；只有整个连通块完全浮空时才坍塌。
- 沙、土等软材质放置后立刻进入物理模拟并自动找坡，但沉降应局部、可预期、节制，避免玩家每次放置都被迫修补。
- 第一版保持创造模式体验：快捷栏材料无限，不显示堆叠数量。
- 水第一版不做真实流体，只作为静态透明占位或简化水块。
- 如果某种形状在拟真规则下确实无法成立，反馈应来自材质本身，例如滑落、散开、碎裂，而不是要求玩家切换额外模式。

实现含义：

- 稳定性参数应区分自然地形、建筑硬材质和颗粒材质。
- 建筑类 `StaticSolid` 使用 `FloatingOnly`：不进入低强度坍塌队列，只响应直接破坏、爆炸或完全浮空事件。
- 软材质的松弛可以有位移上限、频率上限和玩家附近预算，保证结果立刻开始但不过度打扰建造。
- 多人场景仍由 Host 仲裁挖放和物理事件，不引入额外权限/归属语义。

---

## 十一、流体

第一版**不实现真实流体**。当前水只作为静态透明占位或简化水块存在：

- 不在格子间传播。
- 不侵蚀沙、土，也不触发湿润、泥化或流沙。
- 不参与 FreeObject 生命周期。
- 不进入高频网络同步；按普通静态材质或透明方块同步即可。

未来如果升级水系统，再引入独立 solver：

```rust
struct FluidCell {
    material: MaterialID,
    volume: u8,
    velocity: Vec3Quantized,
}
```

未来规则：

- 固体 occupancy 提供边界。
- fluid volume 在邻近 cell 间传播并守恒。
- 表面由 fluid extractor 生成。
- 水与沙/土的交互通过材质规则触发：侵蚀、湿润、泥化或流沙等。
- 多人同步优先传 Host 的流体 delta 或低频 region snapshot，避免逐 cell 高频广播。

---

## 十二、网络与确定性

统一体素方案必须从一开始定义同步语义。推荐同步“操作和权威结果”，而不是让每个客户端独立跑完整物理后期待一致。

### 12.1 消息类别

| 类别 | 用途 |
|---|---|
| FieldSnapshot | 新玩家加入、纠偏、区域加载 |
| FieldDelta | 小范围 cell / column 变化 |
| ApplyKernel | 玩家放置 / 挖掘的权威操作 |
| StabilityEvent | 坍塌、提取、沉积等权威事件 |
| FreeObjectSpawn | 创建动态材质团 |
| FreeObjectState | 低频权威状态、位置、速度、旋转 |
| FreeObjectProject | 动态物体静止后投影回静态场 |

### 12.2 量化

网络状态应避免裸 `f32` 成为长期权威数据：

- cell mass 使用 `u8` / `u16`。
- transform 使用定点或量化浮点。
- 速度和角速度使用有限精度。
- FreeObject sample 局部坐标可用小整数或半精度量化。
- 每个 FieldChunk / region 可以带 hash，用于纠偏。

### 12.3 预测

Remote 可以预测：

- 本地放置/挖掘的视觉结果。
- 小 FreeObject 的插值运动。
- 颗粒松弛的临时表现。

Remote 不应权威决定：

- 质量是否成功写入。
- 支撑是否失败。
- FreeObject 是否碎裂。
- 投影后最终 cell 分布。

这些都由 Host 返回 ack / delta / snapshot 协调。

---

## 十三、持久化

持久化需要保存的不再只是 chunk 方块数组：

```rust
struct WorldRecord {
    storage_version: u32,
    material_registry_version: u32,
    seed: u64,
    active_free_objects: Vec<ObjectRecord>,
}

struct FieldChunkRecord {
    pos: ChunkPos,
    columns: Vec<ColumnRecord>,
    dirty_revision: u64,
}
```

需要明确：

- `MaterialID` 的稳定编号和迁移规则。
- FieldChunk 编码版本。
- active FreeObject 当前保存为 `world.json.active_free_objects`，chunk 文件保存移除动态对象后的静态场；sample 保存完整 `MaterialCell`，保存时不强制投影。
- 未来流体 cell 是否保存完整状态，还是从 volume 重新求解 velocity。
- 存档加载后是否需要重新运行稳定性检查。

---

## 十四、数据流总览

```
                    ┌──────────────────────────┐
                    │      MaterialField       │
                    │ cells / columns / objects│
                    └────────────┬─────────────┘
                                 │
          ┌──────────────────────┼──────────────────────┐
          ▼                      ▼                      ▼
   simulation jobs         query/proxy cache        mesh jobs
 stability / granular    collision / raycast     blocky / smooth
 future fluid / proj       support graph       future fluid / object
          │                      │                      │
          ▼                      ▼                      ▼
    FreeObject spawn       gameplay physics          GPU meshes
          │
          ▼
   dynamic simulation
          │
          ▼
   projection / sediment
          │
          ▼
    MaterialField delta
```

---

## 十五、与传统 MC 架构的关系

| 维度 | 传统 MC | 统一体素方案 |
|---|---|---|
| 地形 | BlockID dense chunk | MaterialField / FieldChunk |
| 方块 | 固定 1×1×1 cube | hard material 的一种 visual/class |
| 沙/雪/土 | 特殊方块或 hardcoded 更新 | granular material solver |
| 掉落方块 | Entity | FreeObject，一段材质场脱离静态网格 |
| 玩家 | Entity + AABB | 角色控制器，查询统一世界 |
| 水 | 特殊流体方块 | 第一版静态透明占位；未来 fluid solver 写入统一场 |
| 斜坡 | 特殊方块形状 | soft material extractor 结果 |
| 存档 | block array | field chunks + objects + solver state |
| 网络 | block update / entity state | field delta / operation / object state |

---

## 十六、原型路线

先验证模型，不要一开始追求完整世界替换。

1. **Cell 语义原型**
   - 定义 `MaterialCell`、mass、threshold、kernel。
   - 验证硬材质 cell 能稳定表现为方块。
   - 验证软材质 cell 能提取平滑表面。

2. **单 chunk extractor 原型**
   - 已有 blocky extractor 和 `SmoothGranular` 高度场平滑提面并存。
   - 先把自然地形高度输入调平缓：低频 fBm、坡度限制、出生点平缓区和轻量 erosion；否则 extractor 再平滑也会被陡峭高度场逼出台阶。
   - 后续把高度场提面升级为 column height / occupancy 驱动的连续提面，再评估 Surface Nets / Marching Cubes 是否必要，并测量 CPU 时间、mesh 大小、边界缝。
   - 重点测试硬软交界、草层作为表面材质而不是整格体积、以及远处自然地形是否仍暴露 1m 网格。

3. **创造模式编辑**
   - 放置 kernel、挖掘 kernel、外部无限材料源/汇。
   - 明确 overflow、混合和拒绝规则。

4. **颗粒松弛**
   - 已有 MaterialCell 路径：沙/土/草放置或被挖空下方支撑后，会在 Host / Local-Only 权威侧按小预算局部下落或向斜下滑落。
   - 当前通过多条 `FieldDelta` 同步完整 cell；后续加 column delta / region hash 纠偏。
   - 后续加滞后阈值和更真实的休止角模型避免抖动。

5. **FreeObject 生命周期**
   - 已有第一版：小型 `FloatingOnly` 硬材质连通块失去支撑后，从静态场提取为 active `FreeObject`。
   - `FreeObjectSpawn` 移除静态 cell，`FreeObjectState` 同步动态 transform / velocity / AABB，`FreeObjectProject` 只在静止后发送；Spawn/Project 携带完整 `MaterialCell`。
   - 客户端直接渲染 active sample cube，并把 active AABB 纳入玩家碰撞、raycast 和放置校验；不再播放投影后的假下落动画。
   - OPFS 保存 active object 本体到 `world.json`，加载时恢复 `World::free_objects` 并重建 `FieldChunk.free_object_refs`。
   - 第一版动态体只做平移 AABB、重力、地面接触和最终投影；碰撞反弹、旋转、碎裂后续扩展。

6. **结构完整性**
   - 局部 support graph。
   - component 提取。
   - 崩塌事件可重复、可同步。

7. **多人同步语义**
   - 已有操作 ack、FieldSnapshot、FieldDelta 和 FreeObjectSpawn/State/Project；Remote 预测备份和回滚使用完整 `MaterialCell`。
   - Host 仲裁坍塌、提取和投影。
   - Remote 预测和回滚。

8. **流体**
   - 第一版不做真实流体，只保留静态透明占位。
   - 在固体/颗粒闭环稳定后再加入流体 solver。

---

## 十七、待解决问题

1. **MaterialCell 的通道数量**：`primary + secondary` 是否足够？是否需要 per-cell small palette？
2. **硬软交界提面**：blocky mesh 与 smooth mesh 如何无缝连接？
3. **支撑图成本**：局部 component 分析在大建筑或大山体上如何限流？
4. **动态 FreeObject 成本**：active object 数量、sample 数和状态同步频率如何限流？超预算时如何降级但不破坏质量守恒？
5. **动态碰撞查询**：玩家碰撞、raycast、放置校验和稳定性检查如何统一查询静态场 + active FreeObject？
6. **投影质量守恒**：当前 active FreeObject 静止后仍只处理整格且目标可容纳的投影；后续 FreeObject 无法完全写入时如何处理？
7. **自然地形平滑度**：地形生成器、软材质 occupancy 和 extractor 哪一层负责消除草/土/沙的 1m 台阶感？
8. **视觉方块感边界**：哪些材质必须保留清晰格子，哪些材质必须隐藏格子，混合区域如何过渡？
9. **混合材质规则**：未来沙 + 水、土 + 水、岩石 + 矿物如何表达？
10. **存档版本**：MaterialID 变化后旧世界如何迁移？
11. **网络纠偏**：FieldDelta / FreeObjectState 丢失或乱序时如何用 region hash 修复？
12. **性能预算**：单线程 WASM 下 extractor、稳定性、颗粒松弛和动态对象如何分帧？

---

## 十八、总结

统一体素方案的价值在于：玩家、地形、坍塌、沙土、水和掉落物都围绕同一套材质语义互动。真正可落地的版本应当坚持：

```
统一权威语义；
分层模拟求解；
多 extractor 表现；
质量守恒；
Host 权威；
创造模式材料源/汇；
基岩托底；
水第一版静态占位；
简单沙盒手感优先。
```

这保留了“方块和实体是材质状态变化”的设计美感，同时避免把渲染、碰撞、流体和玩家手感都塞进一个过度理想化的算法里。
