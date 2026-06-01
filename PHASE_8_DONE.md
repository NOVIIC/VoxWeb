# Phase 8 · 多 Pass + 存档完善 · 完成报告

> 完成日期：2026-06-01  
> 关联：[`docs/roadmap.md`](docs/roadmap.md) Phase 8

## 完成项

- ✅ 多 Pass 渲染主路径：程序化天空、可开关 Depth Pre-Pass、不透明方块、玩家实体、透明方块、选中线框、egui UI 依次编码。
- ✅ 透明方块独立 mesh buffer：水 / 玻璃从不透明网格拆出，TransparentPass 使用 alpha blend，并按 chunk 距离远到近绘制。
- ✅ OPFS Variant A：`OpfsStorage` 实现 open/list/load/save/delete/quota，启动申请 `navigator.storage.persist()`，存档 key 为 `room_id__seed` 或 `local__seed`。
- ✅ Chunk 存盘格式：`core::chunk::encode/decode` 增加 `storage_version` 包装；网络 `encode_chunk/decode_chunk` 保持不变，`PROTOCOL_VERSION` 不提升。
- ✅ 持久化调度：`PersistenceManager` 支持 snapshot / commit / failure retry；client 每 1 秒 flush dirty chunk。
- ✅ World LRU：`World` 增加 std 实现的 LRU 顺序、pinned 集合和 runtime capacity，默认 4096。
- ✅ 暂停菜单与 HUD：Depth Pre-Pass、Chunk Cache、Save Now、Delete Save、配额用量与红黄阈值提示。
- ✅ 名牌遮挡：使用现有 DDA 射线检测相机到玩家头顶路径，被方块遮挡时名牌淡出。
- ✅ 调试入口：`window.voxwebDebug.fillDirty(n)` 与 `window.voxwebDebug.quota()`。

## 验证

- ✅ `cargo check --workspace --target wasm32-unknown-unknown`

## 已知限制

- OPFS 使用主线程 async 路径；关闭 Tab 时仍是尽力保存，Worker + sync handle 留作后续升级。
- TransparentPass 以 chunk 为单位排序，不做每个透明面的精细排序；当前水/玻璃数量有限，足够 Phase 8 验收。
- Pass 耗时仍是 CPU 编码耗时，不是 GPU timestamp query。
