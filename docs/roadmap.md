# 当前能力与后续方向

> **何时阅读**：判断项目现在能做什么、还缺什么、哪些增强值得排期。
> **关联文档**：[`../README.md`](../README.md) · [`architecture.md`](architecture.md) · 各 `modules/*` · 各 `features/*`

---

## 一、当前能力

VoxWeb 已经具备完整的浏览器内体素沙盒闭环：

- **启动与部署**：`trunk` 构建 WASM 客户端，Caddy 静态托管，`signaling/` 独立部署到 Cloudflare Workers + Durable Objects
- **浏览器前置检测**：在加载 WASM 前检查 WebAssembly、WebGPU、OPFS、WebRTC、WebSocket、指针锁；移动/触屏设备默认展示不可用提示
- **世界与交互**：Perlin 地形、动态 chunk 加载/卸载、玩家 AABB、Walk/Fly、跳跃、DDA 射线、挖放、hotbar、选中方块线框
- **材质物理**：软材质（沙/土/草）颗粒下落动画——不稳定 cell 提取为单格 active FreeObject，真实自由落体 + 抛物线斜滑摊平 + 级联坍塌，活跃颗粒有上限、超预算回退瞬间松弛；硬材质 `FloatingOnly` 小连通块提取为 active FreeObject，按 tick 刚性下落，使用 AABB 参与玩家碰撞/raycast，静止后投影；提取、下落状态、预测回滚和投影保留完整 `MaterialCell`
- **多人同步**：Local-Only / Host / Remote 三角色；Hello/Welcome 握手；FieldSnapshot 分片；FieldRequest；PlayerInput / PlayerTick；FieldDelta；FreeObjectSpawn/State/Project 及批量 Spawn/State/Project Batch；ActionAck；PeerJoined / PeerLeft
- **网络兜底**：WebRTC 双 DataChannel 为主；ICE 失败或协商超时时，Host 可把指定 peer 对升级为 Cloudflare Worker WebSocket 字节中继
- **UI**：大厅、连接进度、HUD、玩家列表、聊天、系统消息、名牌、暂停菜单、设置持久化、断线页面；统一 egui 主题和固定尺寸 HUD 控件
- **渲染**：WebGPU 多 Pass 主路径，程序化天空、程序化方块纹理图集、自然距离雾、轻量 tone mapping、Depth Pre-Pass、实体方块、`SmoothGranular` 高度场平滑提面、玩家/active FreeObject 盒体、透明方块、选中线框、egui UI
- **网格化与性能**：跨区块面剔除、贪婪网格化、AO、u32 顶点压缩、index buffer、视锥剔除、mesh 分帧预算和 HUD 统计
- **持久化 / 世界表示**：OPFS FieldChunk 存档、运行时 MaterialCell 读写 + dense Chunk 镜像、active FreeObject world.json 恢复、周期 flush、手动保存、删档、配额 UI、LRU chunk cache、`storage_version` 严格校验

---

## 二、验收场景

这些场景覆盖当前主功能，适合作为改动后的人工回归清单：

- **单机**：进入大厅，启动单机世界，走路/跳跃/Fly 切换正常；地形持续加载，区块边界无漏面；挖放后 mesh 立即刷新
- **双人同步**：一个 Tab 创建房间，另一个 Tab 加入；Remote 能收到初始地形和玩家列表；任一玩家挖方块，另一端立即看到变化
- **移动预测**：Host 和 Remote 互相看到平滑移动；高 RTT 下本地玩家不会明显被旧快照拉回
- **聊天与 UI**：聊天、系统消息、玩家名牌、暂停设置、断线返回大厅均可用；大厅/连接/HUD/聊天/暂停视觉风格统一；设置写入 localStorage
- **存档**：同一 room/seed 重进后保留已修改方块；手动保存和删档按钮行为正确；配额用量显示合理
- **中继兜底**：模拟 WebRTC 直连失败时，指定 peer 对升级到 Worker 字节中继，HUD 显示中继状态，其它直连 peer 不受影响
- **渲染检查**：天空、太阳、自然雾化、程序化方块材质、透明水/玻璃、选中线框和 UI 同时可见；Depth Pre-Pass 开关不改变画面正确性

---

## 三、已知限制

- **OPFS 退出保存**：当前主线程 async 写入在 `pagehide` 时只能尽力完成；若真实使用中出现丢数据，再引入 Dedicated Worker + `FileSystemSyncAccessHandle`
- **透明排序**：TransparentPass 按 chunk 中心到相机距离排序，不做每个透明面的精细排序；水/玻璃数量较少时可接受
- **性能计时**：HUD 中的 Pass 耗时是 CPU 编码耗时，不代表 GPU timestamp query
- **网络语义**：Worker 字节中继基于 WebSocket，原 unreliable channel 的“可丢”语义在中继下退化为可靠传输；接收侧按消息类型分发，正确性不依赖 channel kind
- **触屏设备**：当前没有虚拟摇杆和触屏挖放按钮，因此移动/触屏 UA 默认被前置检测拦截
- **主机生命周期**：Host 退出会销毁房间，Remote 返回大厅；尚未实现主机迁移

---

## 四、可选增强

- **存档稳定性**：Dedicated Worker + sync handle；导入/导出存档文件；更细的存档损坏修复 UI
- **网络增强**：TURN 凭据下发；TURN 与 Worker 字节中继的选择策略；更完整的重连流程
- **渲染增强**：Bloom、SSAO、阴影/光照传播、背面剔除收益验证、GPU timestamp query、外部美术贴图资源管线
- **玩法增强**：群系、光照传播、声音、录制/回放、主机迁移
- **触屏支持**：虚拟摇杆、触屏跳跃/挖/放按钮、移动端 UI 缩放策略

---

## 五、统一体素 · 待办清单

> 来源：对 [`unified-voxel-design.md`](unified-voxel-design.md) §16 原型路线 + §17 待解决问题的一次实现审计（2026-07）。
> 勾选状态反映与「设计全量」的差距，不是「能不能玩」——当前第一版闭环可玩，下列是通向设计目标的剩余工作。

### 已落地（对照 §16 原型路线）

- [x] 阶段 1 Cell 语义：`MaterialCell` + `FieldChunk` column store + 材质属性表（数据模型层）
- [x] 阶段 2 部分：blocky 贪婪网格 + `SmoothGranular` 高度场提面，mesh / raycast / 选中 / 客户端碰撞共用查询
- [x] 阶段 4 颗粒松弛：逐格 grain 自由落体 + 抛物线斜滑 + 级联坍塌 + 活跃上限 / 超预算兜底 + 分帧预算
- [x] 阶段 5 FreeObject 第一版：失稳提取 → 平移下落 → 静止投影，质量守恒有测试
- [x] 阶段 7 部分：`FieldSnapshot` / `FieldDelta` / `FreeObject{Spawn,State,Project}`（含 Batch）/ `ActionAck` / Host 权威 / 完整 `MaterialCell` 预测回滚
- [x] 持久化：OPFS `FieldChunk` v2 span 压缩存档 + active object `world.json` 恢复 + 严格 `storage_version` 校验

### 🔴 高优先级（最影响可玩性 / 设计闭环）

- [ ] **材质属性驱动颗粒松弛**（large）：把 `angle_of_repose` / `cohesion` 接进 solver，让沙 / 土 / 草表现分化。当前这些属性**运行时零读取**，堆积角恒为硬编码 1:1（≈45°）、滑速恒为常量，三种软材质行为完全一致——这是「统一体素」立论的根基却尚未通电。落点 `crates/server/src/physics.rs`（`is_downhill_dir` / `try_relax_one` / `ordered_slide_dirs`）。对应设计 §5.4、§17.Q7。
- [ ] **地形自然度**（low → medium）：低频主导 fBm 振幅重排 + 坡度限制 / 轻量 thermal erosion + 平原 / 丘陵 / 山地 biome + 出生点平缓区。落点 `crates/server/src/terrain.rs`。⚠️ 残留台阶感的根因是「连续 1m 单位阶梯 + 满格立方体渲染」，需与下一项配套才有完整视觉收益。对应设计 §9.5、§17.Q7。
- [ ] **硬软交界 seam / skirt + occupancy 驱动提面**（large）：软表面采样硬 cell 作边界、侧面补 skirt 几何、`SmoothGranular` 改用 column top height / occupancy 输入而非满格 `BlockID`（现平滑面只抬顶角、侧面仍是 1m 竖直 quad、交界露格、AO 恒定无遮蔽）。落点 `crates/render/src/chunk_mesh.rs`、`crates/core/src/surface.rs`。对应设计 §9.2、§9.5、§17.Q2/Q8。

### 🟡 中优先级

- [ ] **硬材质坍塌级联复检**（medium）：现「石块搭在沙上、沙流走后石块仍悬空」，要等下次就近编辑才复检。给硬材质加不稳定队列，grain 移除后唤醒上方硬块复检。落点 `crates/server/src/physics.rs`（`wake_support_above` 目前只唤醒软材质）、`crates/server/src/world.rs`。对应设计 §8.4。
- [ ] **收紧 `component_is_supported` 支撑语义**（small）：现把任意 solid 邻居都当支撑，包含会自行滑走的沙 / 土 / 草，导致浮空硬块被不稳定软邻居「撑住」。落点 `crates/server/src/physics.rs`。对应设计 §8.1。
- [ ] **客户端放置预测查动态 AABB**（small）：Remote 乐观放置会短暂误放到下落体侧面再被服务端回滚闪烁；服务端 `validate_place` 已查 active FreeObject AABB，客户端预测未查。落点 `crates/client/src/lib.rs`（放置预测路径）。对应设计 §9.4、§17.Q5。
- [ ] **placement / break kernel 派发**（large，前瞻架构）：读 `placement_kernel` / `break_kernel` 决定 SingleCell vs GranularLocal；实现 §6.2 放置四步与 §6.4 多材质叠加优先级；新增 `ApplyKernel` 协议消息（`PROTOCOL_VERSION` 需递增）。当前单格路径对现有材质功能完整、无 bug，属架构补齐。落点 `crates/server/src/lib.rs`、`crates/core/src/protocol.rs`。对应设计 §6、§12.1、§17.Q1/Q9。
- [ ] **FreeObject transform / velocity 量化**（medium）：去掉 unreliable 通道上的裸 `f32` 权威状态，改定点 / 半精度。当前 Host 权威 + Remote 只渲染，无功能 bug，价值在带宽与未来预测确定性。落点 `crates/core/src/protocol.rs`。对应设计 §12.2、§17.Q4。
- [ ] **region hash 纠偏 + `material_registry_version`**（medium）：给 `FieldChunk` / region 加 checksum 供丢包乱序纠偏；存档追加材质迁移编号。单人 v1 下「拒绝 + 删档」是设计认可策略，此项面向多人与长期演进。落点 `crates/core/src/field.rs`、`crates/server/src/persistence.rs`、`crates/client/src/storage.rs`。对应设计 §12.2、§13、§17.Q10/Q11。

### 🟢 低优先级

- [ ] **清理死脚手架**（small）：运行时零使用的定义——`StabilityPolicy::FutureFluid`、全部 `MechanicsClass` 变体、`PlacementKernel/BreakKernel::GranularLocal`、`CollisionProxy::SampleCloud`（及缺失的 `ConvexHull`）、`FreeObjectState::Settled/Projected`、`MaterialCell.secondary/MixSlot`、`FreeObject.angular_velocity/mass`、`world.json.protocol_version` 死校验字段。逐一删除或补实现，避免后续读者误判为既有能力。
- [ ] 多材质叠加 / 反应规则（§6.4）
- [ ] 存档 `MaterialID` 迁移 remap（§13、§17.Q10）
- [ ] FreeObject 空间索引 broadphase + local mesh cache（§7.3、§9.1）
- [ ] 硬 FreeObject 全局帧预算 + 投影兜底 splatting（§7.4、§17.Q4/Q6）
- [ ] 完整支撑图：`SupportEdge` / 重心投影 / 悬挑 / 抗剪（§8.2）
- [ ] 真实流体 solver（§11，明确延后至固体 / 颗粒闭环稳定之后）
- [ ] 完整 Surface Nets / Marching Cubes（视 occupancy 提面收益再评估，§16 阶段 2）

---

## 六、文档维护

- 当前态说明放入对应专题文档，避免在根目录新增历史报告。
- 协议字段、存档 schema、部署命令或用户流程变化时，同步更新本页验收场景和对应专题文档。
- 完成源代码改动后按 README 要求跑 `cargo fmt` 与 `cargo clippy --all-targets`；仅改文档时至少跑 README 中的文档引用检查。
