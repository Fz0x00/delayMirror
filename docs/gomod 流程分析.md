# Go Modules (gomod) 下载流程完整网络请求分析报告

> **目标**: 以 `go mod download github.com/gin-gonic/gin@v1.9.1` 为例，完整追踪从命令触发到文件落地的每一步网络交互
> **依据**: Go 官方源码 (`cmd/go/internal/modfetch/`)、GOPROXY 协议规范 ([go.dev/ref/mod#goproxy-protocol](https://go.dev/ref/mod#goproxy-protocol))、本项目 delayMirror 实现

---

## 一、架构总览

### 1.1 参与角色

```
┌─────────────┐     ┌──────────────┐     ┌─────────────────┐
│  go command  │────▶│  GOPROXY 代理  │────▶│   上游源/直连    │
│  (客户端)    │◀────│ (delayMirror) │◀────│ (proxy.golang.org│
└──────┬──────┘     └──────────────┘     └────────┬────────┘
       │                                           │
       ▼                                           │
 ┌─────────────┐                                   │
 │ GOMODCACHE  │                                   │
 │ (本地缓存)   │◀────────── zip 落地 ──────────────┘
 └─────────────┘

       │
       ▼
 ┌─────────────┐     ┌──────────────────┐
 │ sum.golang.org│◀──▶│  校验和数据库查询   │
 │ (checksum DB)│     │  (GOSUMDB)        │
 └─────────────┘     └──────────────────┘
```

### 1.2 核心源码位置（Go 官方）

| 文件 | 职责 |
|------|------|
| `cmd/go/internal/modfetch/proxy.go` | **GOPROXY 协议客户端** — 所有 5 个端点的请求封装 |
| `cmd/go/internal/modfetch/fetch.go` | **下载调度器** — 缓存管理、去重、zip 解压 |
| `cmd/go/internal/modfetch/cache.go` | **多级缓存** — Repo 级缓存避免重复网络请求 |
| `cmd/go/internal/modfetch/repo.go` | **Repo 抽象层** — GOPROXY 链查找 / direct 直连切换 |
| `cmd/go/internal/modfetch/coderepo.go` | **VCS 直连实现** — Git/Hg/Bazaar 直接克隆 |
| `cmd/go/internal/modfetch/sumdb.go` | **校验和验证** — 与 sum.golang.org 通信 |
| `cmd/go/internal/mvs/mvs.go` | **MVS 算法** — 最小版本选择，构建依赖图 |

---

## 二、阶段零：前置决策（无网络请求）

在发起任何网络请求之前，`go` 命令执行一系列纯本地判断：

### 2.1 GOPROXY 链解析

```go
// cmd/go/internal/modfetch/repo.go -> Lookup()
// GOPROXY="https://proxy.golang.org,https://goproxy.cn,direct"
func (f *Fetcher) Lookup(path string) (Repo, error) {
    // 1. 检查 GONOPROXY / GOPRIVATE 是否匹配此模块路径
    if module.MatchPrefixPatterns(cfg.GONOPROXY, path) {
        return newCodeHostRepo(path)  // 跳过代理，直接 VCS
    }
    // 2. 按逗号分割 GOPROXY，构造 proxyRepo 链
    for _, url := range splitProxyList(cfg.GOPROXY) {
        if url == "direct" { return newCodeHostRepo(path) }
        if url == "off"    { return errorRepo }
        chain = append(chain, newProxyRepo(url, path))
    }
    return newProxyChain(chain)
}
```

**设计意图**: 
- **逗号分隔的回退链**允许企业配置「私有代理 → 公共镜像 → 直连」三级回退
- **GONOPROXY/GOPRIVATE** 让内网模块完全绕过代理，避免敏感代码经过公共镜像
- `direct` 作为最后一个兜底，确保即使所有代理不可用也能工作

### 2.2 版本查询语义解析

用户输入 `gin@v1.9.1` 可能是：
- **精确语义化版本**: `v1.9.1` → 直接使用
- **版本范围**: `^1.0.0`, `>=1.0.0 <2.0.0` → 需要 list 后选择
- **分支名/Tag**: `master`, `main` → 需要转换为 pseudo-version
- **伪版本**: `v0.0.0-20240101...` → 基于时间戳和 commit hash

```go
// cmd/go/internal/mod-semver.go -> semver.Compare()
// 版本排序: pre-release < release < pseudo-version
// 伪版本格式: vX.Y.Z-pre.{YYYYMMDDHHmmss}-{abcdef123456}
```

### 2.3 本地缓存命中检查

```go
// cmd/go/internal/modfetch/cache.go -> cachingRepo.Stat()
func (r *cachingRepo) Stat(ctx, rev) (*RevInfo, error) {
    if cached, ok := r.cache.Load(path+"@"+rev); ok {
        return cached, nil  // 命中缓存，跳过所有网络请求
    }
    return r.underlying.Stat(ctx, rev)  // 未命中，走网络
}
```

**设计意图**: `$GOMODCACHE/pkg/mod/cache/download/` 下的缓存目录结构为 `{module}/@v/{version}.{info|mod|zip}`，通过文件系统 mtime 判断新鲜度，避免重复网络请求。

---

## 三、阶段一：版本发现（Version Discovery）

### 3.1 请求 #1: GET `/$module/@v/list`

**目的**: 获取模块所有可用版本的列表

**URL 构造规则**:
```
GET {GOPROXY_BASE}/{escaped_module_path}/@v/list
```

**关键细节 — Module Path 编码 (Escape)**:

Go 要求对模块路径中的**大写字母**进行特殊编码。这是 GOPROXY 协议最容易被忽视的细节：

```rust
// delayMirror/src/workers/handlers/gomod.rs:31-51
// Go 官方规则: 每个大写字母前插入 '!'，然后转为小写
pub fn escape_module_path(module: &str) -> String {
    // "github.com/Google/uuid"
    // → "github.com/!google/uuid"
    //
    // "github.com/AABB/Test"
    // → "github.com/!aa!bb!cc/!test"
    //
    // 规则:
    // 1. 连续大写字母块只在块首插入一个 '!'
    // 2. 单独的大写字母也插入 '!'
    // 3. 小写字母重置状态
}
```

**为什么这样设计?**
- HTTP URL 路径**大小写敏感**（不同于域名）
- Git 等 VCS 系统**大小写不敏感**（尤其在 Windows/macOS）
- 如果不编码，`GitHub.com/user` 和 `github.com/user` 会指向不同路径但实际是同一仓库
- `!` 前缀方案让代理可以用纯静态文件系统实现（无需 URL 重写逻辑）

**实际请求示例**:
```http
GET /github.com/gin-gonic/gin/@v/list HTTP/1.1
Host: mirrors.aliyun.com/goproxy/
User-Agent: Go/1.22.0 (darwin/arm64)

--- 响应 ---
HTTP/1.1 200 OK
Content-Type: text/plain; charset=utf-8

v1.0.0
v1.1.0
v1.2.0
...
v1.9.0
v1.9.1
v1.10.0
```

**响应格式**: 纯文本，每行一个版本号，**不包含伪版本(pseudo-version)**

**Go 官方处理逻辑** (`proxy.go`):
```go
func (p *proxyRepo) Versions(ctx context.Context, prefix string) ([]string, error) {
    // GET {base}/{module}/@v/list
    resp, err := ctx.Get(p.urlPrefix + "@v/list")
    if err != nil {
        return nil, err  // 404 或网络错误
    }
    // 解析响应体，按行分割
    versions := strings.Split(resp.Body, "\n")
    return cleanVersions(versions), nil
}
```

**delayMirror 的处理** ([gomod.rs:169-234](../delayMirror/src/workers/handlers/gomod.rs#L169-L234)):
- 直接透传上游响应
- **不做延迟检查**（因为 list 端点不含发布时间信息）
- 添加 `X-Delay-Warning` 头说明此限制

### 3.2 请求 #2 (可选): GET `/$module/@v/latest`

**目的**: 快速获取"推荐最新版本"，避免下载完整列表后自行判断

```http
GET /github.com/gin-gonic/gin/@v/latest HTTP/1.1

--- 响应 ---
HTTP/1.1 200 OK
Content-Type: application/json

{"Version":"v1.9.1","Time":"2023-11-15T08:30:00Z"}
```

**响应格式**: 与 `.info` 端点完全相同的 JSON 结构

**设计意图**:
- 这是一个**可选端点**，代理可以实现也可以不实现
- 当 `@v/list` 返回大量版本时（如 Kubernetes 有数千个版本），`@latest` 可以大幅减少数据传输
- 返回的不一定是最高语义版本——而是"go 命令应该使用的版本"（可能排除 pre-release）

**Go 源码中的 fallback** (`proxy.go`):
```go
func (p *proxyRepo) Latest(ctx context.Context) (*RevInfo, error) {
    // 先尝试 @latest
    info, err := p.query(ctx", "latest")
    if err == nil { return info, nil }
    
    // fallback: 从 @v/list 中取语义最高版本
    list, _ := p.Versions(ctx, "")
    return latestFromList(ctx, list)
}
```

---

## 四、阶段二：版本元数据获取（Metadata Query）

### 4.1 请求 #3: GET `/$module/@v/{version}.info`

**目的**: 获取特定版本的元数据（版本号 + 提交时间），这是**延迟检查的核心数据来源**

**URL 构造**:
```
GET {GOPROXY_BASE}/{escaped_module}/@v/{version}.info
```

**实际请求示例**:
```http
GET /github.com/gin-gonic/gin/@v/v1.9.1.info HTTP/1.1
Host: mirrors.aliyun.com/goproxy/

--- 响应 ---
HTTP/1.1 200 OK
Content-Type: application/json

{
    "Version": "v1.9.1",
    "Time": "2023-11-15T08:30:00Z"
}
```

**JSON Schema** (Go 内部定义):
```go
type RevInfo struct {
    Version string    `json:"Version"`  // 规范化的版本字符串
    Time    time.Time `json:"Time"`    // RFC3339 格式的提交时间戳
    // Go 1.21+ 可能额外包含:
    // Name  string    `json:"Name,omitempty"`  // OmniHash (未来用于替代校验和)
}
```

**delayMirror 的处理** ([gomod.rs:236-322](../delayMirror/src/workers/handlers/gomod.rs#L236-L322)):

这是 delayMirror 的**核心拦截点**：

```rust
async fn handle_gomod_version_info(req, config, checker) -> Result<Response> {
    // 步骤 1: 从 URL 解析 module 和 version
    let module = path_parts[1..path_parts.len() - 3].join("/");
    let version = version_with_ext.trim_end_matches(".info");

    // 步骤 2: 【关键】向上游请求 .info 做延迟检查
    match check_version_with_delay(&module, version, config, checker).await? {
        DelayCheckOutcome::Allowed => {
            // ✅ 发布时间超过延迟阈值 → 放行，再次请求上游返回 .info
            // 注意: 这里会发起第二次到上游的 .info 请求
            // （优化空间: 缓存第一次请求的结果）
            let url = format!("{}/{}/@v/{}.info", registry, escaped_module, version);
            fetch_upstream(url).await
        }
        DelayCheckOutcome::Denied { publish_time } => {
            // ❌ 版本太新 → 返回 403 + 详细原因
            build_forbidden_response(module, version, &publish_time, ...)
            // 返回 JSON: {"error": "Version too recent...", "publish_time": "..."}
        }
        DelayCheckOutcome::NotFound => { /* 404 */ }
        DelayCheckOutcome::UpstreamError(status) => { /* 502 */ }
    }
}

async fn check_version_with_delay(module, version, config, checker) -> Result<DelayCheckOutcome> {
    // 向上游 gomod_registry 发起 .info 请求
    let url = format!("{}/{}/@v/{}.info", config.gomod_registry, escaped_module, version);
    let resp = Fetch::Request(new_get_request(&url)?).send().await?;

    // 解析 JSON 获取 Time 字段
    let info: GoModVersionInfo = serde_json::from_str(&body)?;
    let publish_time = parse_version_time(&info.Time)?;

    // 核心判断: publish_time 是否早于 (now - delay_days)?
    if delay_checker.is_version_allowed(&publish_time) {
        Ok(DelayCheckOutcome::Allowed)
    } else {
        Ok(DelayCheckOutcome::Denied { publish_time })
    }
}
```

**⚠️ 当前实现的性能问题**: `check_version_with_delay` 和实际放行各发一次 `.info` 请求，同一版本实际产生 **2 次** 上游网络请求。

**设计意图分析**:
- `.info` 端点是整个协议中**唯一携带时间戳**的端点
- 时间戳用于: ① 延迟安全检查 ② MVS 算法中的 tie-breaking（同版本比较时选更新的）
- RFC3339 格式确保跨时区一致性

### 4.2 请求 #4: GET `/$module/@v/{version}.mod`

**目的**: 获取该版本的 `go.mod` 文件内容，用于**依赖树构建**

```http
GET /github.com/gin-gonic/gin/@v/v1.9.1.mod HTTP/1.1

--- 响应 ---
HTTP/1.1 200 OK
Content-Type: text/plain; charset=utf-8

module github.com/gin-gonic/gin

go 1.19

require (
    github.com/bytedance/sonic v1.9.1
    github.com/chenzhuoyu/base64x v0.0.0-20221115062448-fe3a3abb8f10
    github.com/gabriel-vasile/mimetype v1.4.2
    ...
)
```

**为什么需要单独请求 .mod?**
- `.zip` 包含完整源码（包括 go.mod），但体积大（可能数 MB）
- `.mod` 通常只有几 KB
- 在 MVS 构建**依赖图时只需要 go.mod 内容**，不需要源码
- 这实现了**按需加载**: 先确定需要哪些依赖及版本，再批量下载 zip

**delayMirror 处理** ([gomod.rs:324-433](../delayMirror/src/workers/handlers/gomod.rs#L324-L433)):
- 同样执行 `check_version_with_delay` 延迟检查
- 通过后透传上游 `.mod` 内容

---

## 五、阶段三：依赖图构建（MVS — 无新网络请求，但递归触发阶段一/二）

### 5.1 MVS 算法执行

当获取到 `gin@v1.9.1` 的 `go.mod` 后，MVS 算法开始工作：

```
输入: gin@v1.9.1 的 go.mod require 列表:
  - github.com/bytedance/sonic v1.9.1
  - github.com/gabriel-vasile/mimetype v1.4.2
  - github.com/gin-contrib/sse v0.0.0-20230157033857-...
  - github.com/go-playground/validator/v10 v10.14.0
  - ... (约 20+ 个直接依赖)

MVS 执行过程:
┌─────────────────────────────────────────────────┐
│ 1. 将 main module 加入图                           │
│ 2. 对每个 require 的依赖:                          │
│    ├── 检查 GOMODCACHE 是否已缓存其 go.mod          │
│    ├── 若未缓存 → 递归执行 阶段一(版本发现) + 阶段二(元数据) │
│    ├── 获取该依赖的 go.mod → 发现其间接依赖           │
│    └── 重复直到图收敛（无新节点）                     │
│                                                   │
│ 3. 对每个模块，选择所有 require 中指定的最高版本       │
│    ("Minimal" = 满足约束的最低兼容版本集合)            │
└─────────────────────────────────────────────────┘
```

**关键特性**:
- **可重现构建**: 相同 input → 相同 output，不受下载顺序影响
- **最小化原则**: 只选满足约束的最低版本，避免不必要的升级
- **无隐式升级**: 即使存在 v2.0.0，如果没人 require 它就不会被选中

### 5.2 递归的网络请求模式

对于典型的 Web 项目（如 gin），一次 `go mod download` 会触发的网络请求量级：

```
第一层 (gin 直接依赖):         ~20 个模块 × 3 请求 (list/info/mod)  = ~60
第二层 (间接依赖):             ~80 个模块 × 3 请求                   = ~240
第三层及更深:                  ~150 个模块 × 3 请求                 = ~450
─────────────────────────────────────────────────────────────────────
总计 (未命中缓存):             ~250 个模块 × 3 请求                 ≈ ~750 次 GET
总计 (全部缓存命中):           0 次
典型混合场景 (部分缓存):       ~50-200 次 GET
```

**这就是为什么 GOPROXY + 本地缓存如此重要** —— 没有 proxy 的话，每次都是 `git clone` 整个仓库。

---

## 六、阶段四：包文件下载（Zip Download）

### 6.1 请求 #5: GET `/$module/@v/{version}.zip`

**目的**: 下载模块完整源码的 zip 归档

```http
GET /github.com/gin-gonic/gin/@v/v1.9.1.zip HTTP/1.1
Host: proxy.golang.org  ← 注意: 这里用的是 gomod_download_registry

--- 响应 ---
HTTP/1.1 200 OK
Content-Type: application/octet-stream
Content-Length: 1234567

[ZIP binary data]
```

**ZIP 格式规范** (严格定义):

```
{module}@{version}.zip
├── {module}@{version/          ← 根目录必须是 module@version
│   ├── LICENSE                  ← 必须有（或 LICENSE.md, COPYING）
│   ├── go.mod                   ← 必须（与 .mod 端点内容一致）
│   ├── README.md
│   └── src/                     ← 实际源码
│       ├── gin.go
│       ├── context.go
│       └── ...
```

**关键约束**:
1. **根目录名必须精确匹配** `{module}@{version}`
2. **不允许有 symlinks**
3. **必须包含 go.mod**
4. **文件权限统一** (Unix 0644/0755)

**delayMirror 处理** ([gomod.rs:435-503](../delayMirror/src/workers/handlers/gomod.rs#L435-503)):

```rust
pub async fn handle_gomod_download(req, config, checker) -> Result<Response> {
    // 同样先做延迟检查
    match check_version_with_delay(&module, version_clean, config, checker).await? {
        DelayCheckOutcome::Allowed => {
            // ⚠️ 关键区别: zip 下载使用 gomod_download_registry（不同于 metadata 用 gomod_registry）
            let upstream_url = format!(
                "{}/{}/@v/{}.zip",
                config.gomod_download_registry.trim_end_matches('/'),  // 默认 proxy.golang.org
                escaped_module,
                version_clean
            );
            Fetch::Request(upstream_req).send().await  // 流式转发，不读 body 到内存
        }
        // ... Denied/NotFound/Error 分支同上
    }
}
```

**⚠️ 重要设计: 元数据和下载分离的两个 Registry**

从 [config.rs](../delayMirror/src/core/config.rs#L22-L23) 可以看到:

```rust
gomod_registry: "https://mirrors.aliyun.com/goproxy/",      // 用于 list/info/mod
gomod_download_registry: "https://proxy.golang.org",          // 用于 zip
```

**为什么分开?**
- **阿里云镜像** (`mirrors.aliyun.com/goproxy/`) 在国内访问快，适合轻量级元数据查询
- **官方 proxy** (`proxy.golang.org`) 的 CDN 分布更广，适合大文件 zip 下载
- **安全考量**: 元数据可以走可信镜像，但最终下载可以走官方源确保完整性
- **成本考量**: 镜像站可能对大文件下载有带宽限制

---

## 七、阶段五：校验和验证（Checksum Verification）

### 7.1 本地 go.sum 查找

下载完成后（无论是 `.mod` 还是 `.zip`），`go` 命令立即计算哈希：

```go
// cmd/go/internal/modfetch/fetch.go -> download()
func (f *Fetcher) Download(ctx, mod) (dir string, err error) {
    // 1. 下载 zip
    zipData := p.GoZip(ctx, path, version)

    // 2. 计算 hash (h1:SHA-256, 未来可能有 hx:OmniHash)
    actualHash := hashZip(zipData)

    // 3. 查找 go.sum 中的预期哈希
    expectedHash := lookupGoSum(path + " " + version + "/zip")

    // 4. 比较
    if actualHash != expectedHash {
        return "", fmt.Errorf("checksum mismatch for %s@%s", path, version)
    }

    // 5. 解压到 GOMODCACHE
    unzipToCache(zipData, path, version)
}
```

**go.sum 条目格式**:
```
github.com/gin-gonic/gin v1.9.1 h1:YXHnUJqQDzVh3aXg/lDJRZD0WpOg7cQQXjIIR+RkKxM=
github.com/gin-gonic/gin v1.9.1/go.mod h1:hM90wHboJABpGmMYQUOkbU8m6TbA/DCKyQJEBHBtoM=
```
- 第一行: zip 的 hash (`h1:` prefix = SHA-256)
- 第二行: go.mod 的 hash (`/go.mod` suffix)

### 7.2 请求 #6 (条件性): sum.golang.org 查询

如果 `go.sum` 中**没有**该条目（首次下载），则触发 checksum database 查询：

```http
GET https://sum.golang.org/lookup/github.com/gin-gonic/gin@v1.9.1 HTTP/1.1

--- 响应 ---
HTTP/1.1 200 OK

github.com/gin-gonic/gin v1.9.1 h1:YXHnUJqQDzVh3aXg/lDJRZD0WpOg7cQQXjIIR+RkKxM=/go.mod h1:hM90wHboJABpGmMYQUOkbU8m6TbA/DCKyQJEBHBtoM=
```

**GONOSUMDB 豁免**:
```bash
# 私有模块跳过 sumdb 校验
export GONOSUMDB=*.corp.example.com,github.com/my/private

# 完全禁用 (危险!)
export GOSUMDB=off
```

**设计意图**:
- **供应链安全**: 防止代理投毒（proxy 返回被篡改的内容）
- **日志审计**: sum.golang.org 由 Google 运营，所有条目可追溯
- **NoteDB 结构**: 校验和数据以 Merkle tree 存储，支持高效证明

---

## 八、完整请求序列图（以 gin@v1.9.1 为例）

```
时间线 →

[go command]              [delayMirror]            [Aliyun Mirror]         [proxy.golang.org]       [sum.golang.org]
    │                         │                        │                       │                       │
    │  ① GET /gin/.../@v/list │                        │                       │                       │
    │────────────────────────▶│                        │                       │                       │
    │                         │  ①' GET /gin/@v/list   │                       │                       │
    │                         │───────────────────────▶│                       │                       │
    │                         │  200 [版本列表]         │                       │                       │
    │                         │◀───────────────────────│                       │                       │
    │  200 [版本列表]          │                        │                       │                       │
    │◀────────────────────────│                        │                       │                       │
    │                         │                        │                       │                       │
    │  ② GET /gin/.../v1.9.1.info                     │                       │                       │
    │────────────────────────▶│                        │                       │                       │
    │                         │  ②'a GET /.info (延迟检查)                      │                       │
    │                         │───────────────────────▶│                       │                       │
    │                         │  200 {"Version":"v1.9.1","Time":"2023-11-15..."}│                       │
    │                         │◀───────────────────────│                       │                       │
    │                         │  [检查: 2023-11-15 > now-3days? → Allowed]      │                       │
    │                         │                        │                       │                       │
    │                         │  ②'b GET /.info (放行)  │                       │                       │
    │                         │───────────────────────▶│                       │                       │
    │                         │  200 [.info 内容]       │                       │                       │
    │                         │◀───────────────────────│                       │                       │
    │  200 [.info JSON]        │                        │                       │                       │
    │◀────────────────────────│                        │                       │                       │
    │                         │                        │                       │                       │
    │  ③ GET /gin/.../v1.9.1.mod (×N个依赖递归)        │                       │                       │
    │────────────────────────▶│  [同样的延迟检查流程...]  │                       │                       │
    │                         │                        │                       │                       │
    │  [MVS 图构建完成]         │                        │                       │                       │
    │                         │                        │                       │                       │
    │  ④ GET /gin/.../v1.9.1.zip                       │                       │                       │
    │────────────────────────▶│                        │                       │                       │
    │                         │  ④'a GET /.info (延迟检查→Aliyun)             │                       │
    │                         │───────────────────────▶│                       │                       │
    │                         │  200                    │                       │                       │
    │                         │◀───────────────────────│                       │                       │
    │                         │                        │                       │                       │
    │                         │  ④'b GET /.zip (下载→proxy.golang.org)         │                       │
    │                         │───────────────────────────────────────────────▶│                       │
    │                         │  200 [ZIP binary stream]                      │                       │
    │                         │◀───────────────────────────────────────────────│                       │
    │  200 [ZIP stream]        │                        │                       │                       │
    │◀────────────────────────│                        │                       │                       │
    │                         │                        │                       │                       │
    │  ⑤ (本地计算hash)         │                        │                       │                       │
    │  ⑥ GET /lookup/gin@v1.9.1 (go.sum miss时)                               │                       │
    │────────────────────────────────────────────────────────────────────────────────────────────▶│
    │  200 [hash lines]        │                        │                       │                       │
    │◀────────────────────────────────────────────────────────────────────────────────────────────│
    │                         │                        │                       │                       │
    │  [写入 go.sum + 解压到 GOMODCACHE]                │                       │                       │
    │                         │                        │                       │                       │
```

---

## 九、各阶段工程考量总结

### 9.1 为什么是 5 个端点而不是 1 个？

| 设计决策 | 原因 |
|---------|------|
| **list / info / mod / zip 分离** | 按需加载，MVS 只需 mod 不需 zip；info 只需时间戳不需内容 |
| **无查询参数，纯 RESTful 路径** | 可用静态文件系统实现代理（`file:///` URL 即可） |
| **纯 GET，无 POST** | 天然可缓存，CDN 友好 |
| **@latest 可选** | 兼容简单代理实现，复杂代理可以优化 |

### 9.2 大写字母编码 (!escape) 的深层原因

```
问题域冲突:
├── HTTP URL:     大小写敏感  (/Foo/ ≠ /foo/)
├── Git (Windows): 大小写不敏感 (Foo/ == foo/)
├── Go import:    大小写敏感  (import "Foo" ≠ import "foo")
└── 文件系统:     平台相关

解决方案: !前缀编码
├── 编码侧: GitHub → !google (确定性、可逆)
├── 代理侧: 纯字符串操作，无需理解语义
└── 解码侧: !google → Google (还原原始大小写)
```

### 9.3 延迟检查架构的安全模型

```
delayMirror 的安全假设:
┌──────────────────────────────────────────────────┐
│ 攻击者发布恶意版本 v2.0.0 (含 0-day)               │
│                                                    │
│ Day 0: 用户执行 go get victim@latest               │
│   → GOPROXY 返回 v2.0.0                           │
│   → delayMirror 检查 Time = "now"                  │
│   → now < (now - 3days) → ❌ 403 Forbidden        │
│   → 用户被迫使用 v1.9.1 (上一个安全版本)            │
│                                                    │
│ Day 4: 延迟窗口过期                                 │
│   → 安全团队已有时间: 分析/公告/patch               │
│   → 白名单机制可进一步拦截已知恶意包                 │
│   → 用户主动选择是否升级                             │
└──────────────────────────────────────────────────┘
```

### 9.4 性能优化建议（针对当前 delayMirror 实现）

| 问题 | 建议 |
|------|------|
| `.info` 双重请求（检查 + 放行） | 在 `check_version_with_delay` 中缓存响应体，放行时复用 |
| 每个端点独立延迟检查 | 引入模块级别的版本时间缓存，同一模块的多端点共享 |
| `list` 端点无法做延迟检查 | 这是协议限制，可通过定时预取热门模块的时间数据缓解 |
| zip 使用不同的 registry | 当前设计合理，但可增加自动 fallback（主 registry 不可用时切到备选） |

---

## 十、参考资源

- **GOPROXY 协议规范**: https://go.dev/ref/mod#goproxy-protocol
- **Go 官方源码 (proxy.go)**: https://go.dev/src/cmd/go/internal/modfetch/proxy.go
- **Go 官方源码 (fetch.go)**: https://go.dev/src/cmd/go/internal/modfetch/fetch.go
- **MVS 算法论文**: https://research.swtch.com/vgo-mvs (Russ Cox)
- **checksum 数据库设计**: https://go.dev/design/25530-sumdb
- **本项目实现**: [gomod.rs](../delayMirror/src/workers/handlers/gomod.rs), [router.rs](../delayMirror/src/workers/router.rs), [config.rs](../delayMirror/src/core/config.rs)
