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

## 五、文档维护

- 当前态说明放入对应专题文档，避免在根目录新增历史报告。
- 协议字段、存档 schema、部署命令或用户流程变化时，同步更新本页验收场景和对应专题文档。
- 完成源代码改动后按 README 要求跑 `cargo fmt` 与 `cargo clippy --all-targets`；仅改文档时至少跑 README 中的文档引用检查。
