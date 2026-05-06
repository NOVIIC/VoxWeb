# 部署与构建

> **何时阅读**：搭建本地开发环境；改 CI；调构建产物体积；改部署流程
> **关联文档**：[`../README.md`](../../README.md) · [`networking/signaling.md`](networking/signaling.md) · [`reference.md`](reference.md) · [`roadmap.md`](roadmap.md)

---

## 一、部署拓扑回顾

```
┌──────────────────────────────────────────────────┐
│  Caddy（静态站，HTTPS）                            │
│  https://voxweb.example.com                       │
│  ├── /        → dist/index.html  Monet 风格 landing │
│  ├── /start   → dist/start.html  游戏画布 + WASM    │
│  ├── /*.wasm  → dist/*.wasm      （Caddy 标 MIME）  │
│  └── /*.js    → dist/*.js                          │
└──────────────────────────────────────────────────┘
       ↓ HTTP GET（首屏，用户访问）

┌──────────────────────────────────────────┐
│  Cloudflare Workers + Durable Objects     │
│  wss://signal.example.com/room/:id        │
│  （独立部署，不与静态站绑定）                │
└──────────────────────────────────────────┘
       ↑ WSS（信令握手，仅 P2P 建连前）
```

**严格分离**：游戏代码部署到 Caddy；信令服务部署到 CF Workers。两者都是无状态 / 边缘 / 独立可替换。

> **页面拆分**：项目根有两个 HTML —— `index.html`（Monet 风格 landing，纯 HTML/CSS/JS，不依赖 wasm）和 `start.html`（trunk 入口，注入 wasm-bindgen 胶水 + canvas）。Landing 页的 Start 按钮链接到 `/start`，由 Caddy 的 `try_files` 把 `/start` 内部重写到 `/start.html`。

---

## 二、本地开发环境搭建

### 2.1 工具链

| 工具 | 安装 | 用途 |
|---|---|---|
| Rust | `rustup`（≥ 1.85，Edition 2024） | 编译 |
| `wasm32-unknown-unknown` target | `rustup target add wasm32-unknown-unknown` | WASM 编译目标 |
| `wasm-bindgen-cli` | `cargo install wasm-bindgen-cli` | WASM JS 绑定生成（trunk 内部已含，但本地调试可用） |
| `trunk` | `cargo install trunk` | 主要构建工具 |
| `wasm-opt` | binaryen 套件（pacman/brew/winget） | 体积优化 |
| Node.js | ≥ 20 | 信令服务开发 |
| `wrangler` | `npm i -g wrangler` | CF Workers CLI |
| Caddy | 官方 binary | 静态站点本地预览 |

### 2.2 项目根目录

```
VoxWeb/
├── crates/                    Rust workspace
├── docs/                      本项目文档
├── signaling/                 TS Workers 项目（独立 npm 项目）
├── index.html                 landing 页（Monet 风格，非 trunk 入口）
├── start.html                 游戏页（trunk 入口；构建时复制 index.html 进 dist）
├── trunk.toml                 target = "start.html"
├── Cargo.toml
├── Caddyfile                  本地预览用
└── README.md
```

### 2.3 开发命令

```bash
# 终端 1：游戏前端开发（trunk 监听 start.html + crates/）
trunk serve --port 8080
# → 监视 crates/* + start.html，热重载浏览器

# 终端 2：信令服务（本地）
cd signaling && wrangler dev --local --port 8787

# 浏览器访问：
#   http://localhost:8080/         → landing 页（index.html）
#   http://localhost:8080/start.html → 游戏页（trunk 默认输出名）
# 客户端代码用 query string 切换信令地址：
# http://localhost:8080/start.html?signaling=ws://localhost:8787
```

> ⚠️ 在 `trunk serve` 下访问"干净 URL" `/start` 不会自动重写到 `/start.html`（trunk 的内置 server 没有 try_files）。本地开发请用 `/start.html`；生产 Caddy 配置了 `try_files`，`/start` 可直接命中。

### 2.4 增量检查

不重新构建只验证编译错误：

```bash
cargo check --target wasm32-unknown-unknown -p voxweb-client
cargo check -p voxweb-core            # core 也支持原生 target，便于 IDE
```

### 2.5 单元测试

```bash
# 原生 target 跑（core / server）
cargo test -p voxweb-core
cargo test -p voxweb-server

# WASM target（仅当需要测试浏览器 API 包装）
wasm-pack test --headless --chrome -p voxweb-client
```

### 2.6 多窗口本地联机测试

打开两个浏览器窗口（或同浏览器两个 Tab）：
- 窗口 A：访问 `http://localhost:8080?signaling=ws://localhost:8787` → 创建房间 `test01`
- 窗口 B：同地址 → 加入房间 `test01`

预期：B 窗口能看到 A 创建的世界，并互相能看到对方移动。

---

## 三、`trunk.toml` 与 HTML 入口

### `trunk.toml`（项目实际配置）

```toml
[build]
target = "start.html"
html_output = "start.html"
dist = "dist"

[serve]
address = "0.0.0.0"
port = 8080
open = false

[watch]
ignore = ["signaling", "docs", ".git"]
```

> trunk 把 `start.html` 当作 HTML 模板，扫描 `<link data-trunk rel="...">` 注入 wasm-bindgen 胶水 + WASM；并通过 `<link data-trunk rel="copy-file" href="index.html" />` 把 landing 页一并复制到 `dist/`。

### `start.html`（游戏页 / trunk 入口）

```html
<!DOCTYPE html>
<html lang="zh-Hans">
<head>
  <meta charset="UTF-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1.0" />
  <meta name="signaling-url" content="wss://signal.voxweb.example.com" />

  <!-- 把 landing 页一并复制到 dist -->
  <link data-trunk rel="copy-file" href="index.html" />

  <!-- 编译期注入 wasm-bindgen 胶水 + WASM -->
  <link data-trunk rel="rust" href="crates/client/Cargo.toml" data-type="main"
        data-wasm-opt="z" />

  <style>
    html, body { margin: 0; padding: 0; height: 100%; overflow: hidden; background: #1a1a2e; }
    #game { width: 100vw; height: 100vh; display: block; }
  </style>
</head>
<body>
  <canvas id="game"></canvas>
</body>
</html>
```

### `index.html`（landing 页）

纯 HTML/CSS/JS，不被 trunk 处理（仅作 copy-file 拷到 dist）。Start 按钮指向 `/start`：

```html
<a href="/start" class="start-button">Start</a>
```

干净 URL `/start` 由 Caddy 的 `try_files` 内部重写到 `/start.html`（见第五节）。

---

## 四、生产构建

### 4.1 命令

```bash
trunk build --release
# 输出（hash 由 wasm-bindgen 自动加在文件名）：
#   dist/
#     index.html                                 ← landing
#     start.html                                 ← 游戏页（trunk 默认输出名同源）
#     voxweb-client-<hash>.js
#     voxweb-client-<hash>_bg.wasm
```

> ⚠️ **`wasm-opt` 当前未在工具链中**：`start.html` 的 `data-wasm-opt="z"` 已配置好，trunk 检测到 `wasm-opt` 在 PATH 时会自动调用；否则跳过并 warn。本机器人当前观察到 `wasm-opt` 缺失，导致 wasm 体积偏大（Phase 0 实测 2.10 MB gz，目标 1.5 MB）。
> 安装：
> - Windows：`winget install -e --id WebAssembly.WABT` 或下载 [binaryen release](https://github.com/WebAssembly/binaryen/releases) 解压加 PATH
> - macOS：`brew install binaryen`
> - Ubuntu：`apt install binaryen`

### 4.2 体积优化

`Cargo.toml`：

```toml
[profile.release]
opt-level = "z"          # 体积优先（牺牲一些性能）
lto = "fat"
codegen-units = 1
panic = "abort"
strip = "symbols"
```

`wasm-opt` 后处理：在 trunk hooks 配置（如上）；典型可再缩 15-25%。

### 4.3 体积目标

| 阶段 | WASM 大小（gz） | 总下载（gz） |
|---|---|---|
| Phase 0-2 | < 2 MB | < 3 MB |
| Phase 5 完成 | < 4 MB | < 5 MB |
| 全功能（Phase 9） | < 6 MB | < 8 MB |

超出预期时检查：
```bash
twiggy top -n 30 dist/pkg/*.wasm    # 看哪些函数最大
cargo bloat --release --target wasm32-unknown-unknown
```

---

## 五、Caddy 部署

### 5.1 `Caddyfile`

```caddy
voxweb.example.com {
    root * /srv/voxweb/dist
    encode gzip zstd

    # COOP/COEP 头：为日后启用 SharedArrayBuffer 留口
    header {
        Cross-Origin-Opener-Policy "same-origin"
        Cross-Origin-Embedder-Policy "require-corp"
        ?Content-Type-Options "nosniff"
    }

    # WASM 文件 MIME
    @wasm path *.wasm
    header @wasm Content-Type "application/wasm"

    # hash 文件名长缓存
    @hashed path *.wasm *.js
    header @hashed Cache-Control "public, max-age=31536000, immutable"

    # HTML 短缓存（包含干净 URL "/start"）
    @html path /index.html / /start.html /start
    header @html Cache-Control "public, max-age=300"

    # 干净 URL：/start → /start.html
    try_files {path} {path}.html

    file_server
}
```

> **COEP 注意**：开启 `require-corp` 后，任何外部资源必须带 `Cross-Origin-Resource-Policy: cross-origin`。本项目不引外部资源（除可选的字体 CDN），可放心开。
> 如不需要 SharedArrayBuffer，可去掉 COOP/COEP 减少配置复杂度。

### 5.2 部署流程

```bash
# 服务器侧
cd /srv/voxweb
git pull origin main
trunk build --release
systemctl reload caddy
```

或用 CI（见下文）。

---

## 六、Cloudflare Workers 部署（信令）

### 6.1 准备

```bash
cd signaling
npm install
wrangler login
```

### 6.2 部署

```bash
wrangler deploy
# 输出：worker URL，如 https://voxweb-signaling.YOUR_ACCOUNT.workers.dev
```

### 6.3 自定义域名

在 Cloudflare 仪表盘：
1. 添加 DNS：`signal.example.com` → CNAME `voxweb-signaling.YOUR_ACCOUNT.workers.dev`（实际通过"Workers Routes"绑定）
2. Workers → 路由 → 添加 `signal.example.com/*` → 选择 `voxweb-signaling`
3. SSL/TLS → Full（strict）

客户端连接 `wss://signal.example.com/room/abc123`。

### 6.4 环境变量

```bash
wrangler secret put TURN_SECRET     # v2 阶段
wrangler secret put ALLOWED_ORIGINS # 仅放游戏域名
```

`worker.ts` 中通过 `env.ALLOWED_ORIGINS` 读取。

### 6.5 监控

```bash
wrangler tail   # 实时日志
```

Cloudflare 仪表盘 → Workers → voxweb-signaling → 流量 / 错误率。

---

## 七、CI（GitHub Actions 建议）

`.github/workflows/ci.yml`：

```yaml
name: CI
on:
  push:
    branches: [main]
  pull_request:

jobs:
  rust-check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: wasm32-unknown-unknown
      - uses: Swatinem/rust-cache@v2
      - name: Format
        run: cargo fmt --all --check
      - name: Clippy
        run: cargo clippy --target wasm32-unknown-unknown -- -D warnings
      - name: Test (native)
        run: cargo test -p voxweb-core -p voxweb-server

  wasm-build:
    runs-on: ubuntu-latest
    needs: rust-check
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: wasm32-unknown-unknown
      - uses: Swatinem/rust-cache@v2
      - run: cargo install --locked trunk
      - run: trunk build --release
      - run: ls -la dist/pkg
      - uses: actions/upload-artifact@v4
        with:
          name: VoxWeb-dist
          path: dist/

  signaling-check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with: { node-version: 20 }
      - run: cd signaling && npm ci && npx tsc --noEmit
```

### 部署 Workflow（可选）

```yaml
deploy-signaling:
  if: github.ref == 'refs/heads/main'
  runs-on: ubuntu-latest
  needs: signaling-check
  steps:
    - uses: actions/checkout@v4
    - uses: cloudflare/wrangler-action@v3
      with:
        apiToken: ${{ secrets.CF_API_TOKEN }}
        workingDirectory: signaling

deploy-static:
  if: github.ref == 'refs/heads/main'
  runs-on: ubuntu-latest
  needs: wasm-build
  steps:
    - uses: actions/download-artifact@v4
      with: { name: VoxWeb-dist, path: dist }
    - name: rsync to caddy server
      run: rsync -azv --delete dist/ ${{ secrets.CADDY_USER }}@${{ secrets.CADDY_HOST }}:/srv/voxweb/dist/
      # 需要 SSH key 通过 secret 注入
```

---

## 八、客户端配置注入

游戏需要知道信令地址。三种方式（按优先级）：

1. URL query string：`?signaling=wss://signal.example.com`（开发/测试用）
2. `start.html` 内 `<meta name="signaling-url" content="...">`（生产推荐）
3. 默认值：编译期常量

```rust
fn signaling_url() -> String {
    // 1. URL query
    if let Some(url) = url_query("signaling") {
        return url;
    }
    // 2. meta tag
    if let Some(url) = meta_content("signaling-url") {
        return url;
    }
    // 3. fallback
    "wss://signal.voxweb.example.com".into()
}
```

---

## 九、本地预览生产构建

```bash
trunk build --release
caddy file-server --root dist --listen :8080
# 访问 http://localhost:8080
```

注意：本地 HTTP 下 WebGPU 与指针锁仍可用，但部分 API（如 `navigator.storage.persist()`）仅 HTTPS 可用。生产必须 HTTPS。

---

## 十、版本管理与发布流程

1. 推 commit 到 `main` → CI 跑 lint + 测试 + WASM 构建 → 构建产物上传 artifact
2. 手动触发 `deploy-static` workflow（或自动）→ rsync 到 Caddy
3. 信令服务有变化时手动触发 `deploy-signaling`
4. 协议版本号变化（`PROTOCOL_VERSION` 递增）：
   - 提前发布客户端新版（用户访问时获取新 wasm，需要 hard refresh）
   - 信令服务升级时同步发布

---

## 十一、不在范围

- 多区域部署 / CDN（项目流量不大，单一 Caddy 节点足够）
- A/B 测试 / 灰度发布
- 容器化（Docker）— 简单 Rust 构建无需容器
- Kubernetes / Helm chart — 同上
- 健康监测告警（v2 加 Cloudflare Analytics + 简易 Slack 告警）
- 自动 rollback 机制（手动回滚足够）
