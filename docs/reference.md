# 参考资料：技术栈、浏览器约束、已知坑

> **何时阅读**：升级依赖；排查浏览器兼容问题；估技术风险
> **关联文档**：[`../README.md`](../README.md) · [`deployment.md`](deployment.md) · [`modules/render.md`](modules/render.md) · [`modules/net.md`](modules/net.md)

---

## 一、技术栈版本表

> 实际编码阶段会锁定具体版本到 Cargo.lock；下表是规划版本范围，新成员对齐时使用。

### Rust 与 Cargo

| 项 | 版本 | 备注 |
|---|---|---|
| Rust | 1.85+ | Edition 2024 |
| Cargo workspace | 单一 root | 根 `Cargo.toml` 含 `[workspace]` |
| target | `wasm32-unknown-unknown` | 唯一发布目标 |

### 核心依赖

| Crate | 推荐版本 | 用途 | 备注 |
|---|---|---|---|
| `wgpu` | ≥ 29 | 图形 API | 仅 BROWSER_WEBGPU 后端 |
| `winit` | 0.30+ | 窗口/事件 | web 后端通过 `event_loop_extra_traits` |
| `egui` | 0.34+ | UI | 必须与 egui-wgpu / egui-winit 版本对齐 |
| `egui-wgpu` | 配套 | wgpu 集成 | |
| `egui-winit` | 配套 | winit 输入桥接 | |
| `glam` | 0.30+ | 数学 | 含 `Vec3`/`Mat4`/`Quat` |
| `serde` | 1 | 序列化 | derive feature |
| `bincode` | 2.x | 二进制序列化 | `serde` integration |
| `bytemuck` | 1 | Pod/Zeroable trait | 顶点/uniform 内存 |
| `noise` | 0.9+ | Perlin 噪声 | |
| `wasm-bindgen` | 0.2.x | Rust↔JS 互操作 | |
| `wasm-bindgen-futures` | 0.4.x | async/await 支持 | |
| `web-sys` | 0.3.x | 浏览器 API 绑定 | feature flag 按需启用 |
| `js-sys` | 0.3.x | JS 类型 | |
| `async-trait` | 0.1+ | trait 内 async fn | `WorldStorage` trait 使用 `?Send` |
| `futures-channel` | 0.3 | mpsc/oneshot | 跨 async 通信 |
| `console_error_panic_hook` | 0.1 | panic 输出到 console | |
| `tracing` + `tracing-wasm` | 0.1+ / 0.2+ | 日志 | |
| `bitflags` | 2 | 位标记 | |

### web-sys feature 列表（典型）

```toml
[dependencies.web-sys]
version = "0.3"
features = [
  "Window", "Document", "HtmlCanvasElement", "Performance",
  "Storage", "StorageManager", "Location", "Navigator", "ResizeObserver",
  "WebSocket", "MessageEvent", "BinaryType", "ErrorEvent",
  "RtcPeerConnection", "RtcConfiguration", "RtcDataChannel",
  "RtcDataChannelInit", "RtcSessionDescription", "RtcSessionDescriptionInit",
  "RtcIceCandidate", "RtcIceCandidateInit", "RtcSdpType",
  "PointerEvent", "KeyboardEvent", "MouseEvent",
  "PageTransitionEvent", "Element", "EventTarget",
  # —— OPFS（存档） ——
  "FileSystemDirectoryHandle", "FileSystemFileHandle",
  "FileSystemGetFileOptions", "FileSystemGetDirectoryOptions",
  "FileSystemRemoveOptions", "FileSystemWritableFileStream",
  "FileSystemHandle", "FileSystemHandleKind",
  "File", "Blob",
  # —— WebGPU ——
  "Gpu", "GpuAdapter",
]
```

> 已移除 `Idb*` / `BeforeUnloadEvent`：本项目 Phase 5 起改用 OPFS（详见 [`features/persistence.md`](features/persistence.md)），退出 flush 监听 `pagehide` 而非 `beforeunload`。

### 信令服务（TS）

| 包 | 版本 | 用途 |
|---|---|---|
| `@cloudflare/workers-types` | latest | TS 类型 |
| `wrangler` | latest（CLI） | 部署 |
| TypeScript | 5+ | |

---

## 二、浏览器支持矩阵

| 浏览器 | WebGPU | WebRTC | OPFS | 指针锁 | 推荐度 |
|---|---|---|---|---|---|
| Chrome 113+ | ✅ | ✅ | ✅（102+） | ✅ | 推荐 |
| Edge 113+ | ✅ | ✅ | ✅（102+） | ✅ | 推荐 |
| Safari 17+ | ✅（macOS Sonoma+ / iOS 17+） | ✅ | ✅（17+） | ✅（macOS） | 推荐（桌面） |
| Firefox 115+ stable | ❌ WebGPU 默认（需 nightly） | ✅ | ✅（111+） | ✅ | 不推荐 |
| Firefox Nightly | ✅（about:config 开启 dom.webgpu.enabled） | ✅ | ✅ | ✅ | 可用 |
| 移动 Chrome (Android) | ✅（114+） | ✅ | ✅ | ❌ 部分 | 不在本期范围 |
| 移动 Safari (iOS) | ✅（17+） | ✅ | ✅（17+） | ❌ | 不在本期范围 |

### 浏览器能力前置检测（wasm 加载之前）

由于 OPFS / WebGPU / WebRTC 等是硬依赖，**必须在 `<script>` 加载 `.wasm` 之前**用纯 JS 检测，避免下载几 MB wasm 后才在运行时报错。脚本放 [`index.html`](../index.html) 与 [`start.html`](../start.html) 的 `<head>` 内联（< 2 KB gz）。

检测项：

```js
const checks = {
  wasm:        () => typeof WebAssembly === 'object' && typeof WebAssembly.instantiate === 'function',
  webgpu:      () => 'gpu' in navigator,
  opfs:        () => navigator.storage && typeof navigator.storage.getDirectory === 'function',
  webrtc:      () => typeof RTCPeerConnection === 'function' && 'createDataChannel' in RTCPeerConnection.prototype,
  websocket:   () => typeof WebSocket === 'function',
  pointerlock: () => 'requestPointerLock' in HTMLElement.prototype,
};
const required = ['wasm', 'webgpu', 'opfs', 'webrtc', 'websocket', 'pointerlock'];
const missing = required.filter(k => !checks[k]());
```

**降级提示文案表**（写在内联脚本内常量）：

| 缺失项 | 提示 |
|---|---|
| `wasm` | 浏览器太旧，不支持 WebAssembly。请升级浏览器。 |
| `webgpu` | 浏览器不支持 WebGPU。Firefox 用户请切换 nightly；Safari 请升级到 17+。 |
| `opfs` | 浏览器不支持 OPFS（存档系统所需）。请升级到 Chrome / Edge 102+、Firefox 111+、Safari 17+。 |
| `webrtc` | 不支持 WebRTC，无法多人联机（仅单机模式可继续）。 |
| `websocket` | 不支持 WebSocket，无法连接信令服务器。 |
| `pointerlock` | 不支持指针锁，第一人称视角无法启用。 |

**特殊处理**：若 **仅 `webrtc` 缺失** 而其他必备项都满足 → 提供"仅单机模式"按钮，wasm 启动后强制走 Local-Only 路径，禁用大厅"创建/加入房间"。其他必备项缺失 → 拒绝加载 wasm，显示降级页面（纯 HTML/CSS）。

**移动设备拦截（commit 34ebd73）**：即便 API 全部满足，UA 命中 `Mobi|Android|iPhone|iPad|iPod` 或"Macintosh + maxTouchPoints > 2"（iPad 桌面模式）也会拒绝加载 wasm，提示"VoxWeb 暂不支持触屏操作，请使用桌面浏览器"。理由：当前无虚拟摇杆 / 触屏挖放按钮（[`roadmap.md`](roadmap.md) Phase 9.3 stretch），即使强行加载也无法操作。降级页面会把 `mobile` 行排在最前，便于用户立即理解。

**最低浏览器版本**（取所有必备项交集）：

| 浏览器 | 最低版本 | 缺什么 |
|---|---|---|
| Chrome / Edge | 113+ | 113 起 WebGPU 默认开启 |
| Firefox | 115+ stable，WebGPU 需 nightly | stable 仍缺 WebGPU |
| Safari (macOS) | 17+ | 17 起 OPFS + WebGPU 同步可用 |

**`?force=1` 跳过**：URL 加 query `?force=1` 跳过检测直接加载 wasm，仅用于开发/调试，正式版可移除。

---

## 三、浏览器 API 限制与坑

### 3.1 WebGPU

- **Adapter 选择**：`request_adapter` 在桌面 Linux 集显上可能选错；本期使用 `power_preference: HighPerformance`
- **Surface 配置**：`alpha_mode` 必须设置（否则 Safari 可能透明），用 `Opaque`
- **Texture 格式**：用 `surface.get_capabilities(&adapter).formats[0]` 取 preferred；不要硬编码
- **Storage Buffer**：WebGPU 已支持，但本期未使用（贪婪网格化在 CPU 跑）；v2 GPU 网格化时启用
- **Compute shader**：本期不用
- **错误处理**：浏览器 `unhandledpromiserejection` 可能因为 wgpu 内部错误触发；`set_uncaught_error_handler` 截获并 console.error
- **`queue.write_buffer` 在 submit 前会合并到同一 buffer 的最后一次写入**：
  错误模式：
  ```rust
  for chunk in chunks {
      queue.write_buffer(&shared_globals_buf, 0, &globals_for(chunk));  // ❌
      encoder.begin_render_pass(...).draw(...);
  }
  queue.submit(...);  // 所有 draw 都看到最后一次写入的值
  ```
  WebGPU 模型：单次 submit 内所有 `queue.write_buffer` 在所有 command buffer 命令前执行，对同一 byte range 的多次写入只保留最后一次。
  正确做法：**每个需要不同 uniform 的 draw 持有独立 buffer**（如 `ChunkMeshGpu` 自带 `globals_buffer + bind_group`），或使用 dynamic offset，或使用 instance buffer 把 per-draw 数据放顶点输入。
  本项目体现：`crates/render/src/lib.rs::Renderer::render_world` 与 `crates/render/src/passes/opaque.rs::ChunkMeshGpu`

### 3.2 WebRTC

- **NAT 穿透成功率**：ICE only-STUN ≈ 80-85%，加 TURN 中继可达 ~99%
- **MTU**：DataChannel 单消息 ≤ 16 KiB（不同浏览器实现，保守 14 KiB）
- **bufferedAmount**：Host 发 ChunkSnapshot 时必须监控，避免无限堆积
- **ICE candidate 收集**：可能持续数秒；UI 显示进度
- **Trickle ICE**：本项目使用 trickle（边收集边发），不等 `iceGatheringState == complete`
- **DataChannel 双通道顺序**：Host 必须先 `createDataChannel("reliable")` 再 `createDataChannel("unreliable")`，Remote 通过 `ondatachannel` 按顺序接收（依赖此顺序识别通道）
- **关闭事件**：`onclose` 不一定在每个浏览器都触发；超时机制兜底（详见 `modules/net.md`）

### 3.3 WebSocket

- **wss URL**：HTTPS 站点必须用 wss（混合内容会被浏览器拦截）
- **重连**：本期不实现；用户主动重新加入

### 3.4 OPFS

- **配额**：通常磁盘 60% 上限（chromium）；隐身模式下大幅缩水至几十 MB；可调 `navigator.storage.persist()` 申请持久存储以避免自动清理
- **同步 API 仅 Worker 内可用**：`FileSystemSyncAccessHandle.write()`，主线程只有 async 路径；Phase 8 默认走 async（Variant A），视情况升级到 Worker（Variant B）
- **`removeEntry({recursive:true})`**：Chromium / Firefox 较新版本支持；旧 Safari 需逐文件 `removeEntry` 自实现递归删
- **大目录性能**：单目录 ≥ 10k 文件时 OPFS 内部 SQLite-like 索引仍可承担；但 `entries()` 异步迭代会拖到 100-300 ms，需要 loading UI 遮蔽
- **DevTools 入口**：
  - Chrome / Edge：DevTools → Application → Storage → 选 origin → "Open Origin Private File System"
  - Firefox 111+：DevTools → Storage → 同级有 OPFS 节点
  - Safari：暂无 UI，靠控制台命令 `window.voxwebDebug.*`
- **`pagehide` 是退出 flush 的事件**（不是 `beforeunload`）：在移动 Safari 比 `beforeunload` 可靠；BFCache 路径下浏览器会 await 完异步任务
- **跨域隔离**：每个 origin 的 OPFS 互相隔离，不能直接共享存档

完整设计与读写时机：[`features/persistence.md`](features/persistence.md)。

### 3.5 指针锁

- **手势要求**：`requestPointerLock` 必须在用户手势 callback 同步发起
- **失败静默**：失败不抛错，需要监听 `pointerlockerror` 事件
- **ESC 释放**：浏览器强制行为，无法阻止
- **Safari**：macOS Safari 偶尔不释放鼠标，刷新页面可解决

### 3.6 鼠标 movement 单位差异

| 浏览器 | `movementX/Y` 单位 |
|---|---|
| Chrome / Edge | CSS 像素 |
| Firefox | device 像素（受 DPI 影响） |
| Safari | CSS 像素 |

修正：
```rust
let dpr = window.device_pixel_ratio() as f32;
let normalized = if is_firefox() { dx / dpr } else { dx };
```

`is_firefox` 通过 `navigator.userAgent.contains("Firefox")` 判断。

### 3.7 Canvas resize

- 监听 `ResizeObserver`（不是 `window.onresize`，更精确到 canvas 元素）
- HiDPI：`canvas.width = css_width * devicePixelRatio`
- wgpu Surface 重建必须在下一帧进行（避免帧中破坏正在用的 surface）

### 3.8 后台 Tab

- 浏览器后台 Tab 的 RAF 频率会降到 1Hz 或暂停
- 影响：游戏暂停感（实际逻辑帧 dt 变大）
- 处理：在 RAF 回调中 dt > 0.5s 时跳过逻辑模拟（避免大跳跃位置预测错乱）

### 3.9 性能 API

- `performance.now()` 在 Spectre mitigation 下分辨率被降到 100μs（部分浏览器）
- 测网格化耗时仍够用
- 高精度需求用 `performance.measure`

---

## 四、Rust + WASM 已知坑

### 4.1 panic 与 unwind

- WASM 默认不支持 unwind，`panic = "abort"` 推荐
- 任何 panic 终结整个 WASM 实例 → console_error_panic_hook 必装
- 业务代码用 `Result` 替代 panic

### 4.2 体积膨胀

- 默认编译产物 5-10 MB（debug）/ 1-3 MB（release without opt）
- 启用 `opt-level = "z"` + `lto = "fat"` + `wasm-opt -Oz` 后通常 1-2 MB
- 引入 `winit` + `egui` + `wgpu` 后激增到 4-6 MB
- 用 `twiggy` / `wasm-stats` 分析

### 4.3 浮点确定性

- WASM 的浮点是 IEEE 754 标准但 JIT 实现可能差异
- 物理仲裁不依赖跨机器一致（Host 单方计算，无 lockstep）→ 无问题

### 4.4 单线程限制

- `std::thread::spawn` panic
- 并发要用 `wasm_bindgen_futures::spawn_local`
- `Mutex` / `RwLock` 在单线程下永远不阻塞，但仍有运行时开销 → 优先用 `RefCell`

### 4.5 时间类型

- `std::time::Instant::now()` 在 WASM 上 panic
- 用 `web_sys::Performance::now()` 拿毫秒
- `Duration` 结构体本身可用

### 4.6 随机数

- `rand::thread_rng` 在 WASM 上 panic
- 用 `getrandom` crate 加 `js` feature → 内部走 `crypto.getRandomValues`

### 4.7 网络

- 不能直接 `std::net::TcpStream`（panic）
- HTTP 请求用 `web_sys::Window::fetch` + `JsFuture`
- WebSocket 用 `web_sys::WebSocket`
- 本项目所有网络都走 P2P + WebSocket，无需 HTTP fetch

### 4.8 文件系统

- 无 `std::fs`
- 持久化使用 **OPFS**（[`features/persistence.md`](features/persistence.md)）；localStorage 仅存轻量配置；File System Access API 仅 Phase 9 stretch 用作可选导出
- 加载 assets 用 `include_bytes!`（编译期嵌入）或运行时 `fetch`

---

## 五、性能基线（参考目标）

测试设备：MacBook Air M2 / Chrome 130

| 场景 | 帧率 |
|---|---|
| 渲染距离 4，本地单人，平地 | 120 fps |
| 渲染距离 6，本地单人，山地 | 100+ fps |
| 渲染距离 8，本地单人 | 80+ fps |
| 渲染距离 6，3 人房间 | 90+ fps |
| 渲染距离 6，8 人房间 | 60-80 fps |

> 这些是**目标**值，编码期间持续 benchmark。

---

## 六、调试技巧

### 6.1 浏览器 DevTools

- **Console**：Rust panic / `tracing::error!` 全在这里
- **Performance**：录制 5-10s 帧，看每帧时间分布
- **Network → WS**：信令 WebSocket 消息
- **Application → Storage → OPFS**：存档查看（详见 [`features/persistence.md`](features/persistence.md) §十四）
- **Memory**：堆快照（注意 WASM 堆不在此显示，需要 wasm-bindgen 助记）

### 6.2 chrome://gpu

WebGPU 状态、当前后端、错误日志。

### 6.3 chrome://webrtc-internals

实时查看 RTC PeerConnection 状态、ICE candidate、DataChannel 统计、丢包率。**联机调试必看**。

### 6.4 性能 overlay（项目内）

启用 `RenderSettings.show_stats` 或按 F3，HUD 显示各阶段耗时。

### 6.5 远程 Source Map

trunk 在 release 模式下默认不带 sourcemap（节省体积）。debug 模式带；如需在 release 下排查可单独编译加 `--features debug-symbols`（需自己加）。

---

## 七、风险登记表

| 风险 | 等级 | 缓解 |
|---|---|---|
| WebGPU 在 Firefox 稳定版不支持 | 高 | 文档与 UI 引导用户切换浏览器 |
| WebRTC NAT 穿透失败 | 中 | v2 加 TURN 中继 |
| 单线程网格化卡顿 | 中 | 分帧 budget + 优先级队列 |
| WASM 体积超 6MB | 中 | 持续 twiggy 监测 |
| OPFS 配额耗尽（隐身模式 / 长期世界） | 中 | UI 显示用量；> 80% 警告；> 95% 暂停 dirty 写入；引导导出/删档 |
| OPFS 写入失败 / `pagehide` 未完成 flush | 中 | 1s 周期 flush 兜底；Variant B（Worker + sync handle）作为升级路径 |
| 浏览器后台 Tab 时间漂移 | 低 | dt 上限 + 跳过过大逻辑步 |
| egui 中文字体嵌入体积 | 中 | 用 subset font 仅嵌入常用字 |
| 协议版本不兼容 | 中 | `world.json.storage_version` 校验 + migrations 数组（详见 [`features/persistence.md`](features/persistence.md) §十） |
| Cloudflare Workers 配额 | 低 | 信令通量极小，难以触顶 |
| 主机退出导致房间销毁 | 中 | 文档说明，v2 stretch goal 实现迁移 |

---

## 八、术语对照（中英）

| 中文 | 英文 | 用法 |
|---|---|---|
| 区块 | Chunk | |
| 体素 | Voxel | |
| 主机权威 | Host-Authoritative | |
| 客户端预测 | Client-Side Prediction | |
| 协调 | Reconciliation | |
| 插值 | Interpolation | |
| 信令 | Signaling | |
| 候选 | ICE Candidate | |
| 直连 | Peer-to-Peer (P2P) | |
| 中继 | TURN Relay | |
| 贪婪网格化 | Greedy Meshing | |
| 跨区块面剔除 | Cross-Chunk Face Culling | |
| 顶点压缩 | Vertex Packing | |
| 环境光遮蔽 | Ambient Occlusion (AO) | |
| 视锥剔除 | Frustum Culling | |
| 渲染图 | Render Graph | |
| 包围盒 | AABB | |
| 数字微分分析器 | DDA | |
| 持久化 | Persistence | |
| 配额 | Quota | |

---

## 九、推荐学习资源（编码时参考）

- WebGPU API：[https://www.w3.org/TR/webgpu/](https://www.w3.org/TR/webgpu/)
- wgpu 文档：[https://docs.rs/wgpu/](https://docs.rs/wgpu/)
- WebRTC（MDN）：[https://developer.mozilla.org/en-US/docs/Web/API/WebRTC_API](https://developer.mozilla.org/en-US/docs/Web/API/WebRTC_API)
- 贪婪网格化（0fps）：搜索 "greedy meshing voxel 0fps"
- 客户端预测（Gabriel Gambetta）：搜索 "fast-paced multiplayer client-server"
- DDA Voxel Traversal：搜索 "Amanatides Woo voxel raycast"

> 上述资源仅在编码期间手动查阅，不需要 agent 主动 fetch。

---

## 十、文档维护规则（提醒）

- 升级依赖时 → 同步更新本文档第一节版本表
- 发现新坑 → 加入第三节"已知坑"
- 性能基线变化 → 更新第五节
- 新风险 → 第七节登记
