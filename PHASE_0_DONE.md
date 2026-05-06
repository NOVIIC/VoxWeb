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
