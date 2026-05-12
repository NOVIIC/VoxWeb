# Phase 0 · 脚手架 · 完成报告

> 完成日期：2026-05-05
> 关联：[`docs/roadmap.md`](docs/roadmap.md) Phase 0

---

## 实际完成项

- ✅ Workspace + 五个 crate 空骨架（`core` / `render` / `server` / `net` / `client`）已存在
- ✅ `start.html` + `trunk.toml` 跑通 `trunk build --release`
- ✅ `index.html` 起始页（Monet 风格）通过 `data-trunk rel="copy-file"` 一并发布
- ✅ `client::start` 创建 wgpu Surface 并每帧清屏（深色背景 `#0f0f1a`）
- ✅ 集成 egui + egui-wgpu，画布中央渲染 "Hello VoxWeb"（56pt）
- ✅ 主循环走 `requestAnimationFrame`（自重排闭包链），每帧同步 canvas 尺寸 → surface 重配置
- ✅ `console_error_panic_hook::set_once()` + `tracing_wasm::set_as_global_default()`
- ✅ CI workflow [.github/workflows/ci.yml](.github/workflows/ci.yml)：
  - `cargo fmt --all -- --check`
  - `cargo check --workspace --target wasm32-unknown-unknown`
- ✅ `signaling/` Cloudflare Worker 骨架（[signaling/src/worker.ts](signaling/src/worker.ts)）：根路径返回 `200 "VoxWeb Signaling v0.1"`，`/room/:id` 走 Durable Object

---

## 关键文件改动

- [crates/client/src/lib.rs](crates/client/src/lib.rs)：Phase 0 运行时（Phase0Runtime + RAF 循环 + egui 集成）
- [.github/workflows/ci.yml](.github/workflows/ci.yml)：CI

---

## 验证

| 项 | 标准 | 实测 |
|---|---|---|
| `trunk build --release` | 成功 | 38s 完成 |
| `cargo clippy --target wasm32-unknown-unknown --workspace` | 无 error | 仅若干 warning |
| `cargo fmt --all -- --check` | 无 diff | ✅ |
| 浏览器加载 | 看到深色背景 + 居中 "Hello VoxWeb"，控制台无错误 | 待人工验证（需 `trunk serve`） |
| WASM gz 体积 < 1.5 MB | < 1.5 MB | ❌ **2.10 MB** — 见下方已知问题 |

---

## 已知问题 / 后续

1. **WASM gz 体积超标（2.10 MB > 1.5 MB 目标）**
   - 原因：本地未运行 wasm-opt（`start.html` 已配置 `data-wasm-opt="z"`，但工具链可能未就位）。
   - 应对：在 CI / 部署机上确保安装 `binaryen` 的 `wasm-opt`；Phase 1 再评估是否需要剪裁 `web-sys` features。
2. **`prediction.rs` 死代码警告**
   - `InputHistory::entries` 等字段尚未使用。属预留结构，Phase 5 客户端预测落地时即被消费；保留警告作为提醒。
3. **`render` 模块占位** — `RenderGraph` / `OpaquePass` 等已建好骨架但未参与 Phase 0 渲染（清屏由 client 直接编码）。Phase 1 渲染真正方块时会接入。

---

## 下一步：Phase 1 · 渲染骨架

入口文档：[docs/modules/render.md](docs/modules/render.md) · [docs/modules/client.md](docs/modules/client.md)

---

## 回填（2026-05-12）

Phase 0 初版完成于 2026-05-05；在 Phase 2 之后评估"大存档"压力时持久化方案由 IndexedDB 切换到 OPFS（决策见 [docs/features/persistence.md §二](docs/features/persistence.md#二为什么选-opfs而非-indexeddb--file-system-access-api)），相应给 Phase 0 增补一项"浏览器能力前置检测"硬依赖前置项。该任务回填到 Phase 0 任务清单，本日完成：

- ✅ **浏览器能力前置检测**：[start.html](start.html) 在 `<head>` 内联同步检测脚本，覆盖 WebAssembly / WebGPU / OPFS / WebRTC / WebSocket / 指针锁；不通过时 `window.stop()` 中止 trunk 预加载并 `document.open()/write()` 替换为深色降级页（列出缺失能力 + 浏览器升级建议 + `?force=1` 跳过链接）。仅 WebRTC 缺失 → 设 `window.__VOXWEB_FORCE_LOCAL_ONLY = true` 放行（Phase 5/6 大厅 UI 据此禁用多人按钮）。
- ✅ **landing 软提示**：[index.html](index.html) 加同等检测，不阻止跳转但把 Start 按钮置灰 + 弹框列出缺失项 + 升级建议。
- ✅ **trunk 注入顺序验证**：检测脚本在 `<head>` 起始位置，trunk 的 `<script type="module">` 与 `<link rel="modulepreload">` 均排在其后（见 `dist/start.html` line 15 / 121 / 153），确保 `window.stop() + document.write` 路径下 wasm 不会被下载。
- ✅ **idb 依赖与 web-sys IDB feature 移除**：[Cargo.toml](Cargo.toml) workspace 删除 `idb = "0.6"`、web-sys feature 列表删除 `IdbFactory / IdbDatabase / IdbObjectStore / IdbTransaction / IdbVersionChangeEvent / IdbRequest / IdbCursor / BeforeUnloadEvent`；[crates/client/Cargo.toml](crates/client/Cargo.toml) 删 `idb.workspace = true`。
- ✅ **stub 命名对齐新方案**：[crates/client/src/storage.rs](crates/client/src/storage.rs) `IndexedDbStorage` → `OpfsStorage`，方法签名对齐 [`docs/features/persistence.md` §五](docs/features/persistence.md#五rust-接口) 的 `WorldStorage` trait（`open / list_chunks / load_chunk / save_chunks / delete_world / quota`，全部 Phase 5 占位）；新增 `StorageError` / `QuotaInfo` 类型骨架。Phase 5 真实装时仅替换函数体。

### 回填验证

| 项 | 标准 | 实测 |
|---|---|---|
| `cargo check --target wasm32-unknown-unknown` | 全 workspace 通过 | ✅ |
| `cargo test -p voxweb-core -p voxweb-server` | 既有 7 个测试不受影响 | ✅ |
| `trunk build` | dist 产物正常生成 + 检测脚本位置正确 | ✅ |
| Chrome 113+ 访问 `/start` | 进入大厅 | 待人工 |
| Firefox stable 访问 `/start` | 看到降级页，显示 `webgpu` 缺失 | 待人工 |
| `/start?force=1` | 跳过检测继续加载 wasm | 待人工 |
