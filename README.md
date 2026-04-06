# Delay Mirror

**基于时间延迟策略的包管理器安全代理网关**

[特性](#特性) • [架构](#架构) • [快速开始](#快速开始) • [API 文档](#api-文档) • [配置](#配置) • [延迟策略](#延迟策略)

---

Delay Mirror 是一个轻量级安全镜像网关，使用 **Rust** 编写。它作为 NPM / Go Modules / PyPI 的透明代理层，通过**时间延迟安全策略**来降低供应链攻击风险——只有发布超过指定天数的包版本才会被允许下载。

支持两种部署方式：
- **Cloudflare Workers** — Serverless 架构，零运维成本
- **独立服务器** — 传统部署，完全自主控制

## 特性

- **NPM Registry 完整代理** — 元数据过滤、tarball URL 重写、版本降级
- **Go Modules 全端点支持** — `@v/list`、`@v/{version}.info`、`.mod`、`.zip`
- **PyPI 双协议兼容** — Simple API + JSON API，wheel/sdist 自动解析
- **时间延迟安全门控** — 可配置冷却期（默认 3 天），未到期版本自动拒绝或降级
- **白名单机制** — 按包类型（npm/gomod/pypi）配置允许列表，支持通配符匹配
- **结构化审计日志** — JSON 格式输出，便于日志分析平台接入
- **双部署模式** — 支持 Cloudflare Workers 和独立服务器两种部署方式

## 架构

```
                        ┌──────────────────────────────────────┐
                        │    Delay Mirror (Rust → WASM/Binary) │
                        │                                      │
   Client Request ─────▶│  Router                               │
                        │                                      │
 (npm/pip/go mod)      │  ┌── NPM Handler  ◄── 双源分离         │
                        │  │  ├── 元数据 → NPM_REGISTRY          │
                        │  │  └── 下载   → NPM_DOWNLOAD_REGISTRY │
                        │  │                                     │
                        │  ├── GoMod Handler  ◄── 双源分离        │
                        │  │  ├── 元数据 → GOMOD_REGISTRY        │
                        │  │  └── 下载   → GOMOD_DOWNLOAD_REG    │
                        │  │                                     │
                        │  └── PyPI Handler  ◄── 双源分离        │
                        │     ├── 元数据 → PYPI + JSON API      │
                        │     └── 下载   → PYPI_DOWNLOAD_BASE   │
                        │           │                          │
                        │  ┌────────▼────────┐                 │
                        │  │  DelayChecker   │◀─ DELAY_DAYS     │
                        │  │  (时间门控)      │                  │
                        │  └────────┬────────┘                 │
                        │           │                          │
              Allowed ──┴───▶ Proxy Upstream                   │
              Downgraded ─▶ 旧版本替代                         │
              Denied ────▶ 403 + 建议版本                      │
                        └──────────────────────────────────────┘
                                    │
            ┌───────────────────────┼───────────────────────┐
            ▼                       ▼                       ▼
   NPM_DOWNLOAD_REGISTRY    GOMOD_DOWNLOAD_REGISTRY   PYPI_DOWNLOAD_BASE
   (或 NPM_REGISTRY 回退)    (或 GOMOD_REGISTRY 回退)  (或 JSON API URL)
```

## 快速开始

### 部署方式选择

| 特性 | Cloudflare Workers | 独立服务器 |
|------|-------------------|-----------|
| 运维成本 | 零运维 | 需自行管理 |
| 部署复杂度 | 中等（需 Cloudflare 账户） | 简单（单一二进制） |
| 扩展性 | 自动扩展 | 需手动扩展 |
| 成本 | 免费额度 + 按量计费 | 服务器费用 |
| 适用场景 | 全球分布、轻量负载 | 内网部署、高吞吐量 |

---

### 方式一：独立服务器部署

#### 前置要求

- [Rust toolchain](https://rustup.rs/) (stable, edition 2021)

#### 1. 构建

```bash
git clone https://github.com/your-org/delay-mirror.git
cd delay-mirror

# 构建发布版本
cargo build --release --features server
```

#### 2. 运行

```bash
# 设置环境变量
export DELAY_DAYS=3
export NPM_REGISTRY=https://registry.npmmirror.com
export NPM_DOWNLOAD_REGISTRY=https://registry.npmjs.org
export PORT=8080

# 运行服务器
./target/release/delay-mirror-server
```

服务运行在 `http://localhost:8080`

#### 3. Docker 部署（可选）

```dockerfile
FROM rust:1.75 as builder
WORKDIR /app
COPY . .
RUN cargo build --release --features server

FROM debian:bookworm-slim
COPY --from=builder /app/target/release/delay-mirror-server /usr/local/bin/
EXPOSE 8080
CMD ["delay-mirror-server"]
```

---

### 方式二：Cloudflare Workers 部署

#### 前置要求

- [Rust toolchain](https://rustup.rs/) (stable, edition 2021)
- [wasm-pack](https://rustwasm.github.io/wasm-pack/installer/)
- [Node.js](https://nodejs.org/) >= 18
- [Wrangler CLI](https://developers.cloudflare.com/workers/wrangler/install-and-update/)
- Cloudflare 账户

#### 1. 构建 WASM

```bash
git clone https://github.com/your-org/delay-mirror.git
cd delay-mirror

# 构建 WASM
wasm-pack build --target nodejs --out-dir pkg --out-name delay_mirror

# 安装 Node.js 依赖
cd workers && npm install
```

#### 2. 本地开发

```bash
# 启动本地开发服务器 (在 workers 目录下运行)
cd workers
npx wrangler dev

# 服务运行在 http://localhost:8787
curl http://localhost:8787/health
# => {"status":"ok"}
```

#### 3. 部署到 Cloudflare

```bash
# 登录 Cloudflare
cd workers
npx wrangler login

# 部署
npm run deploy
```

## 配置

通过环境变量配置：

| 变量名 | 类型 | 默认值 | 说明 |
|--------|------|--------|------|
| `DELAY_DAYS` | number | `3` | 版本发布后需等待的天数（必须 > 0） |
| **NPM 双源** ||||
| `NPM_REGISTRY` | url | `https://registry.npmmirror.com` | NPM **元数据查询**的上游 registry（版本列表 + 时间戳） |
| `NPM_DOWNLOAD_REGISTRY` | url | `https://registry.npmjs.org` | NPM **tarball 实际下载**的上游地址 |
| **Go Modules 双源** ||||
| `GOMOD_REGISTRY` | url | `https://mirrors.aliyun.com/goproxy/` | Go Modules **元数据查询**的上游 proxy（`.list` / `.info` / `.mod`） |
| `GOMOD_DOWNLOAD_REGISTRY` | url | `https://proxy.golang.org` | Go Modules **zip 包实际下载**的上游地址 |
| **PyPI 双源** ||||
| `PYPI_REGISTRY` | url | `https://pypi.org/simple/` | PyPI **Simple API** 上游地址（包索引） |
| `PYPI_JSON_API_BASE` | url | `https://pypi.org/pypi` | PyPI **JSON API** 基础地址（版本元数据 + upload_time） |
| `PYPI_DOWNLOAD_BASE` | url | `https://files.pythonhosted.org/packages` | PyPI **文件下载**的基础 URL（回退下载时使用） |
| **通用** ||||
| `ALLOWLIST_ENABLED` | bool | `false` | 是否启用白名单模式 |
| `ALLOWLIST_JSON` | json | — | 白名单 JSON（见下方格式） |
| `DEBUG_MODE` | bool | `false` | 是否启用 debug 端点（仅开发环境） |
| `PORT` | number | `8080` | 服务器端口（仅独立服务器模式） |

### 三大包管理器双源配置说明

Delay Mirror 对 **NPM / Go Modules / PyPI** 全部支持**元数据源**与**下载源**分离：

```
┌──────────────────────────────────────────────────────────────┐
│                      Delay Mirror                            │
│                                                              │
│  ┌─ NPM Handler ─────────────────────────────────────────┐   │
│  │  元数据 → NPM_REGISTRY (npmmirror)                   │   │
│  │  下载   → NPM_DOWNLOAD_REGISTRY (npmjs)              │   │
│  └───────────────────────────────────────────────────────┘   │
│                                                              │
│  ┌─ GoMod Handler ───────────────────────────────────────┐   │
│  │  元数据 → GOMOD_REGISTRY (aliyun)  ← .list/.info/.mod │   │
│  │  下载   → GOMOD_DOWNLOAD_REGISTRY (goproxy) ← .zip    │   │
│  └───────────────────────────────────────────────────────┘   │
│                                                              │
│  ┌─ PyPI Handler ────────────────────────────────────────┐   │
│  │  元数据 → PYPI_REGISTRY (Simple) + PYPI_JSON_API_BASE│   │
│  │  下载   → releases[].url 或 PYPI_DOWNLOAD_BASE (回退) │   │
│  └───────────────────────────────────────────────────────┘   │
└──────────────────────────────────────────────────────────────┘
```

典型场景：
- **国内部署**: 元数据全部用国内镜像（淘宝 npmmirror / 阿里云 goproxy），下载走官方源确保完整性
- **企业内网**: 元数据用内部缓存源加速查询，下载走官方或 CDN 镜像
- **全球部署**: 元数据和下载都用同一源（不配 `_DOWNLOAD` / `_DOWNLOAD_BASE` 即可回退为单源模式）

> 若不配置下载源变量，系统会自动回退到对应的元数据源地址，即**完全向后兼容的单源模式**。

### 白名单配置示例

```json
{
  "npm": ["lodash", "axios", "react-*"],
  "gomod": ["github.com/gin-gonic/gin"],
  "pypi": ["requests", "numpy"]
}
```

支持精确匹配和通配符（`*` 匹配任意字符序列）。

## API 文档

### 公开端点

#### `GET /` — API 索引

返回所有可用端点的文档。

```json
{
  "service": "delay-mirror",
  "description": "Delay-based security mirror for NPM / Go Modules / PyPI",
  "endpoints": { ... }
}
```

#### `GET /health` — 健康检查

```json
{"status": "ok"}
```

---

### NPM 端点

#### `GET /npm/{package}` — 包元数据

返回经过延迟过滤的包元数据。未到期的版本会被移除，`dist-tags.latest` 会自动指向最新的合规版本。

```bash
curl https://your-worker.dev/npm/lodash
```

响应头：
- `X-Delay-Warning`: 当所有版本都较新时出现

#### `GET /npm/{package}/{version}` — 版本检查

检查特定版本是否可下载。

- **200** + redirect → 允许访问，重定向到上游 tarball
- **403** → 版本太新，响应体包含建议的替代版本

```json
{
  "error": "Version too recent for download",
  "package": "lodash",
  "requested_version": "4.18.0",
  "reason": "Version was published within the last 3 day(s)",
  "suggested_version": "4.17.21"
}
```

响应头：
- `X-Delay-Original-Version`: 请求的原始版本
- `X-Delay-Suggested-Version`: 建议的替代版本
- `X-Delay-Reason`: 拒绝原因

#### `GET /dl/{package}@{version}` — Tarball 下载

下载包 tarball。如果请求版本太新，会自动降级到最近的合规版本。

- **200** + tarball 数据 → 直接下载（可能是降级后的版本）
- **403** → 无可用合规版本

响应头（降级时）：
- `X-Delay-Original-Version`: 原始请求版本
- `X-Delay-Redirected-Version`: 实际返回的版本

---

### Go Modules 端点

#### `GET /gomod/{module}/@v/list` — 版本列表

返回模块的所有可用版本列表。注意：此端点不包含时间戳信息，不做延迟过滤。

#### `GET /gomod/{module}/@v/{version}.info` — 版本信息

返回 Go Module 版本的元数据 JSON（含 `Version` 和 `Time` 字段）。会执行延迟检查。

#### `GET /gomod/{module}/@v/{version}.mod` — go.mod 文件

返回指定版本的 go.mod 内容。会执行延迟检查。

#### `GET /gomod/{module}/@v/{version}.zip` — 模块下载

返回模块 zip 文件。会执行延迟检查。

所有 GoMod 端点的错误响应格式：

```json
{
  "error": "Version too recent for access",
  "module": "github.com/example/repo",
  "requested_version": "v2.0.0",
  "reason": "Version was published within the last 3 day(s)",
  "publish_time": "2024-06-15T12:00:00Z",
  "suggestion": "Try again later or use an older version"
}
```

---

### PyPI 端点

#### `GET /pypi/simple/` — Simple Index 根

返回 PyPI Simple API 的根索引页面。

#### `GET /pypi/simple/{package}/` — 包文件列表

返回指定包的所有可用文件列表。响应头中包含被阻止的新版本警告：

```
X-Delay-Warning: Recent versions blocked by delay policy: 2.0.0, 1.9.0
```

#### `GET /pypi/packages/{filename}` — 文件下载

下载 wheel (.whl) 或 sdist (.tar.gz) 文件。文件名自动解析出包名和版本号。

- **200** + 文件数据 → 直接下载（可能已降级）
- **403** → 版本太新且无替代

## 延迟策略工作原理

Delay Mirror 的核心安全机制是**时间延迟门控**：

```
                    当前时间
                       │
    ┌──────────────────┼──────────────────┐
    │                  │                  │
    │  🟢 安全区域      │  🔴 冷却区域       │
    │  (Allowed)       │  (Denied)         │
    │                  │                  │
    │  v1.0.0  v2.0.0  │  v3.0.0  v4.0.0  │
    │                  │                  │
    └──────────────────┼──────────────────┘
                       │
                阈值 = 现在 - DELAY_DAYS
```

### 三种判定结果

| 结果 | 条件 | 行为 |
|------|------|------|
| **Allowed** | 发布时间 ≤ 阈值 | 正常代理到上游 |
| **Downgraded** | 请求版本在冷却区，但存在更旧的合规版本 | 自动替换为最新合规版本 |
| **Denied** | 请求版本在冷却区，且无任何合规版本 | 返回 403 + 建议版本 |

### 版本选择逻辑（NPM）

当请求的版本被拒绝时，系统会在所有合规版本中选择**语义版本号最大**的那个作为替代：

```
请求: lodash@4.18.0 (发布于 2 天前, DELAY_DAYS=7)
可用合规版本: 4.17.21 (100天前), 4.17.20 (200天前)
→ 降级到: 4.17.21 (最新合规版)
```

## 开发指南

### 项目结构

```
delayMirror/
├── src/
│   ├── lib.rs              # 库入口，导出核心模块
│   ├── workers.rs          # Cloudflare Workers 入口
│   ├── bin/
│   │   └── server.rs       # 独立服务器入口
│   ├── core/               # 平台无关的核心逻辑
│   │   ├── mod.rs
│   │   ├── config.rs       # 配置解析
│   │   ├── delay_check.rs  # 延迟检查核心逻辑
│   │   └── delay_logger.rs # 结构化审计日志
│   ├── platform/           # 平台抽象层
│   │   ├── mod.rs
│   │   └── http.rs         # HTTP 抽象
│   ├── router.rs           # 请求路由分发（Workers）
│   ├── allowlist.rs        # 白名单管理（Workers）
│   └── handlers/           # 处理器（Workers）
│       ├── mod.rs
│       ├── npm.rs          # NPM Registry 处理
│       ├── gomod.rs        # Go Modules 处理
│       └── pypi.rs         # PyPI 处理
├── workers/
│   ├── worker.js           # Cloudflare Worker WASM 加载器
│   ├── wrangler.toml       # Cloudflare 部署配置
│   └── package.json        # npm 脚本 (wrangler)
├── Cargo.toml              # Rust 项目配置
└── LICENSE                 # MIT 许可证
```

### 本地开发

```bash
# 独立服务器模式
cargo run --features server

# Cloudflare Workers 模式
cd workers && npx wrangler dev

# 运行单元测试
cargo test

# 代码检查
cargo clippy --all-targets --all-features
cargo fmt --all
```

### Debug 端点（仅 DEBUG_MODE=true）

| 端点 | 功能 |
|------|------|
| `/debug/fetch` | 测试上游 fetch 能力 |
| `/debug/routes` | 测试路由解析逻辑 |
| `/debug/pypi` | 测试 PyPI 上游连通性 |
| `/debug/gomod` | 测试 Go Modules 上游连通性 |

## 日志格式

每次版本检查都会输出结构化 JSON 日志：

```json
{
  "timestamp": "2024-06-15T12:00:00Z",
  "event": "version_check",
  "package_type": "npm",
  "package": "lodash",
  "original_version": "4.18.0",
  "actual_version": "4.17.21",
  "action": "downgraded",
  "reason": "Version too recent, auto-downgraded for security",
  "client_ip": "203.0.113.1"
}
```

## 贡献

欢迎提交 Issue 和 Pull Request！

## License

本项目采用 [MIT License](./LICENSE) 开源。
