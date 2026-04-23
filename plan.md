# delayMirror Go Modules 代理设计缺陷分析与改进方案

> **范围**: 基于 [gomod.rs](../delayMirror/src/workers/handlers/gomod.rs)、[router.rs](../delayMirror/src/workers/router.rs)、[config.rs](../delayMirror/src/core/config.rs)、[delay_check.rs](../delayMirror/src/core/delay_check.rs) 的完整代码审查
> **参照**: [GOPROXY 协议规范](https://go.dev/ref/mod#goproxy-protocol)、[Go Modules 下载流程分析](./gomod%20流程分析.md)
> **原则**: 只分析 + 方案，不执行代码修改

---

## 缺陷总览

| 编号 | 严重度 | 类别 | 缺陷摘要 | 影响面 |
|------|:------:|------|---------|--------|
| D-01 | 🔴 P0 | 性能 | `.info` **双重请求**: 延迟检查 + 放行各发一次 | 所有 info/mod/zip 端点 |
| D-02 | 🔴 P0 | 安全 | `list` 端点**完全绕过延迟检查**，泄露新版本信息 | 信息泄露 |
| D-03 | 🔴 P0 | 合规 | **缺少 `@latest` 端点**，且 HTTP 状态码不符合 GOPROXY 规范 | Go client fallback 行为异常 |
| D-04 | 🟠 P1 | 性能 | **三端点独立延迟检查**，同一版本产生 3 次 .info 请求 | MVS 递归时放大 3x |
| D-05 | 🟠 P1 | 一致性 | **双 Registry 数据不一致风险**: 延迟检查用 A 源，下载用 B 源 | 可能放行不存在版本 |
| D-06 | 🟠 P1 | 可靠性 | **无超时控制 + 无重试 + 无 fallback 链** | 上游抖动直接影响客户端 |
| D-07 | 🟠 P1 | 正确性 | **伪版本时间戳未被利用**: 可零网络开销完成延迟检查 | pseudo-version 场景 |
| D-08 | 🟡 P2 | 可维护性 | **~60% 代码重复**: 3 个 handler 结构几乎相同 | 改一处漏两处 |
| D-09 | 🟡 P2 | 正确性 | **compare_versions 不支持 semver**: 无法正确排序 pre-release | find_eligible_version 结果偏差 |
| D-10 | 🟡 P2 | 配置 | **Registry URL 尾部斜杠不一致**: 一个带 `/` 一个不带 | 潜在拼接 bug |
| D-11 | 🟢 P3 | 可观测性 | **无 metrics / 无耗时分解 / 无缓存命中率** | 排障困难 |

---

## D-01: `.info` 双重请求（性能 — P0）

### 问题详情

`check_version_with_delay()` 内部向上游请求 `.info` 获取 Time → 判断延迟 → 返回 `Allowed` → handler **再次请求同一个 `.info`** 返回给客户端。

```
handle_gomod_version_info("gin@v1.9.1"):
  ├── check_version_with_delay()     ← 第 1 次 GET gin/@v/v1.9.1.info  (Aliyun)
  │   └── 解析 JSON → Time="2023-11-15" → Allowed
  │
  └── [放行分支]                     ← 第 2 次 GET gin/@v/v1.9.1.info  (Aliyun) ⚠️ 冗余!
      └── 返回 body 给 client
```

同理适用于 `.mod` 和 `.zip` 端点——每次都是 **check 一次 + 放行再请求一次**。

**量化影响**: 典型项目 ~250 个模块 × 3 端点 × 2 次冗余 = **~1500 次多余的上游请求**

### 相关代码位置

- [gomod.rs:113-167](../delayMirror/src/workers/handlers/gomod.rs#L113-L167) — `check_version_with_delay` 消费掉 response body
- [gomod.rs:267-294](../delayMirror/src/workers/handlers/gomod.rs#L267-L294) — `handle_gomod_version_info` 的 Allowed 分支重新请求
- [gomod.rs:355-401](../delayMirror/src/workers/handlers/gomod.rs#L355-L401) — `handle_gomod_go_mod` 同样的问题
- [gomod.rs:466-475](../delayMirror/src/workers/handlers/gomod.rs#L466-L475) — `handle_gomod_download` 同样的问题

### 改进方案

**核心思路**: 让 `check_version_with_delay` 返回已获取的数据体，而非仅返回枚举。

```rust
// 新增: 带缓存的延迟检查结果
enum DelayCheckResult {
    Allowed { info_body: String, info_headers: Headers },
    Denied { publish_time: DateTime<Utc> },
    NotFound,
    UpstreamError(u16),
}

async fn check_version_with_delay_cached(
    module: &str, version: &str, config: &Config, checker: &DelayChecker,
) -> Result<DelayCheckResult> {
    let escaped_module = escape_module_path(module);
    let url = format!("{}/{}/@v/{}.info", config.gomod_registry.trim_end_matches('/'), escaped_module, version);

    let req = new_get_request(&url)?;
    let mut resp = Fetch::Request(req).send().await?;

    if resp.status_code() == 404 { return Ok(DelayCheckResult::NotFound); }
    if resp.status_code() < 200 || resp.status_code() >= 300 {
        return Ok(DelayCheckResult::UpstreamError(resp.status_code()));
    }

    let body = resp.text().await?;
    let info: GoModVersionInfo = serde_json::from_str(&body)?;
    let publish_time = parse_version_time(&info.Time)?;

    if checker.is_version_allowed(&publish_time) {
        // ✅ 关键改进: 连同 body 一起返回，避免二次请求
        Ok(DelayCheckResult::Allowed { info_body: body })
    } else {
        Ok(DelayCheckResult::Denied { publish_time })
    }
}
```

**Handler 端改造示例** (`handle_gomod_version_info`):

```rust
match check_version_with_delay_cached(&module, version, config, checker).await? {
    DelayCheckResult::Allowed { info_body } => {
        // 直接复用已获取的 body，零额外网络请求
        let mut headers = Headers::new();
        headers.set("Content-Type", "application/json")?;
        Ok(Response::ok(info_body)?.with_headers(headers))
    }
    // ... 其他分支不变
}
```

**收益**:
- 每个 `(module, version)` 组合节省 **1 次** `.info` 请求
- 对于典型项目的 ~250 个模块 × 3 端点 ≈ **减少 ~500 次上游请求**
- 改动量小，向后兼容

---

## D-02: `list` 端点完全绕过延迟检查（安全 — P0）

### 问题详情

[handle_gomod_version_list](../delayMirror/src/workers/handlers/gomod.rs#L169-L234) 直接透传上游 `@v/list` 响应，不做任何过滤：

```rust
pub async fn handle_gomod_version_list(req: Request, config: &Config) -> Result<Response> {
    // ...
    let body = match resp.text().await { Ok(b) => b, ... };
    // ❌ 直接返回所有版本，包括发布不到 3 天的新版本!
    Ok(Response::ok(body)?.with_headers(headers))
}
```

代码中仅添加了一个 warning header:
```rust
headers.set("X-Delay-Warning",
    "Go Modules list endpoint does not provide version timestamps...")?;
```

**这是一个安全漏洞**:

```
攻击者:
  ① GET /gomod/github.com/victim/lib/@v/list
  ② 获得: v1.0.0\nv1.1.0\nv2.0.0(刚发布!)\n
  ③ 直接请求: GET /gomod/github.com/victim/lib/@v/v2.0.0.info
  ④ 绕过 list → 直接触发 info/mod/zip 的延迟检查
  ⑤ 如果 delay_days=0 或配置不当 → 仍然可能获取到新版本
```

即使 info/mod/zip 有延迟保护，**list 本身已经泄露了"有新版本存在"这一敏感信息**，违反了延迟安全的核心假设。

### 改进方案

**方案 A: List 后过滤（推荐）**

```rust
pub async fn handle_gomod_version_list(req, config, checker) -> Result<Response> {
    // 1. 获取原始 list
    let raw_list = fetch_upstream_list(&module).await?;

    // 2. 解析出所有版本
    let versions: Vec<&str> = raw_list.lines().collect();

    // 3. 并发批量查询各版本的 .info（或使用缓存）
    let filtered = filter_versions_by_delay(&versions, &module, config, checker).await;

    // 4. 只返回通过延迟检查的版本
    Ok(Response::ok(filtered.join("\n"))?.with_headers(headers))
}

async fn filter_versions_by_delay(
    versions: &[&str], module: &str, config: &Config, checker: &DelayChecker,
) -> Vec<String> {
    let mut allowed = Vec::new();
    for ver in versions {
        match check_version_with_delay(module, ver, config, checker).await {
            Ok(DelayCheckOutcome::Allowed) => allowed.push(ver.to_string()),
            _ => {} // 过滤掉 Denied/NotFound/Error
        }
    }
    allowed
}
```

**性能考量**: List 通常返回几十到几百个版本，全量查询确实有开销。
- **优化**: 引入模块级版本时间缓存（见 D-04），首次 list 触发后后续请求命中缓存
- **进一步优化**: 对热门模块可预取版本时间数据

**方案 B: 最小改动版（折中）**

如果不想引入额外复杂度，至少可以做到:
- 从 list 中**移除太新的版本**（即使不查 .info，也可以用启发式规则）
- 在响应中添加 `X-Delay-Filtered` header 说明过滤情况
- 记录审计日志

**方案 C: 保持现状 + 文档说明风险**

如果认为 list 泄露可接受（因为最终下载仍有保护），则应在文档中明确标注此为 **known limitation** 并评估业务风险。

---

## D-03: 缺少 `@latest` 端点 + HTTP 状态码不符合 GOPROXY 规范（合规 — P0）

### 问题详情 1: 缺少 `@latest`

GOPROXY 协议定义了 **5 个端点**:

| 端点 | 实现? |
|------|:-----:|
| `GET $module/@v/list` | ✅ |
| `GET $module/@v/latest` | ❌ **缺失** |
| `GET $module/@v/$version.info` | ✅ |
| `GET $module/@v/$version.mod` | ✅ |
| `GET $module/@v/$version.zip` | ✅ |

Go command 的行为 ([proxy.go](https://go.dev/src/cmd/go/internal/modfetch/proxy.go)):
```go
func (p *proxyRepo) Latest(ctx) (*RevInfo, error) {
    info, err := p.query(ctx, "latest")  // 先尝试 @latest
    if err == nil { return info, nil }
    
    // fallback: 从 @v/list 取最高版本
    list, _ := p.Versions(ctx, "")
    return latestFromList(ctx, list)       // 多一次 list 请求!
}
```

**影响**: 缺失 `@latest` 导致每次版本解析多一次 `@v/list` 请求（且 list 响应体通常远大于 latest）。

### 问题详情 2: 错误状态码不规范

GOPROXY 协议规定的状态码语义:

| 状态码 | 含义 | Go command 行为 |
|--------|------|----------------|
| 200 | 成功 | 使用响应数据 |
| 404 | 此 proxy 无此版本 | **尝试下一个 proxy 或 direct** |
| 410 | 此版本已被撤回 | 报错停止 |
| 4xx 其他 | 请求格式错误 | 报错停止 |
| 5xx | 服务器临时错误 | **尝试下一个 proxy 或 direct** |

**当前实现的问题**:

```rust
// 当前: 所有非 200 都返回 JSON 错误 + 自定义状态码
if resp.status_code() == 404 {
    return Response::error(json!({"error":"..."}).to_string(), 404);  // ✅ 404 还算对
}
if resp.status_code() < 200 || resp.status_code() >= 300 {
    return Response::error(json!({"error":"Upstream error","status": status}).to_string(), 502);
    // ❌ 问题: 将上游的 404 也转成了 502!
}
```

**关键 Bug**: 当上游返回 404 时，当前代码的第一个 `if` 能捕获精确的 404，但第二个 `if` 会把其他所有非 2xx（包括 403/401 等）都转成 502。这会导致 Go command **无法正确触发 fallback 链**——它看到的是 delayMirror 的 502，而不是"此版本不存在"的信号。

更严重的是，**延迟拒绝返回 403**:
```rust
Ok(Response::error(body.to_string(), 403)?)  // ❌ Go 把 403 当作权限问题
```
Go command 收到 403 后的行为是**报错退出**，而不是尝试 fallback 到 direct。这意味着如果 delayMirror 拒绝了一个合法（只是太新）的版本，用户无法通过 `GOPROXY=...,direct` 自动降级。

### 改进方案

**1. 添加 `@latest` 端点**:

```rust
// router.rs dispatch_gomod 中添加:
if *last == "latest" {
    return Some(super::handlers::gomod::handle_gomod_latest(req, config, checker).await);
}

// gomod.rs 中实现:
pub async fn handle_gomod_latest(req, config, checker) -> Result<Response> {
    // 复用与 .info 相同的延迟检查逻辑
    // @latest 返回的也是 {"Version":"...","Time":"..."} 格式
    match check_version_with_delay_cached(module, "latest", config, checker).await? {
        DelayCheckResult::Allowed { info_body } => {
            let mut headers = Headers::new();
            headers.set("Content-Type", "application/json")?;
            Ok(Response::ok(info_body)?.with_headers(headers))
        }
        DelayCheckResult::Denied { publish_time } => {
            // ⚠️ 注意: 这里不应返回 403，见下方讨论
            build_forbidden_response(...)
        }
        // ...
    }
}
```

**2. 修正状态码语义**:

```rust
// 延迟拒绝: 应返回 404 而非 403
// 理由: 让 Go command 认为"此版本在此 proxy 中不可用"
// 从而自动 fallback 到 GOPROXY 链中的下一个（如 direct）
fn build_forbidden_response(...) -> Result<Response> {
    // 改为 404: "此版本因延迟策略暂时不可用"
    Ok(Response::error(body.to_string(), 404)?.with_headers(headers))
}

// 上游透传: 保持原始状态码
if resp.status_code() == 404 {
    return Ok(Response::not_found()?);  // 404 → Go 会尝试下一个 proxy
}
if resp.status_code() >= 500 {
    return Ok(Response::error(...)?);    // 5xx → Go 会尝试下一个 proxy
}
// 4xx (除 404): 直接透传，Go 会报错（这是正确的）
```

**关于 403 vs 404 的决策矩阵**:

| 场景 | 推荐状态码 | 理由 |
|------|-----------|------|
| 延迟拒绝（版本太新） | **404** | 让 Go fallback 到 direct 获取旧版本 |
| 白名单拒绝 | **403** | 明确告知这是权限问题 |
| 上游 404 | **404** | 透传，Go 自动 fallback |
| 上游 5xx | **502/503** | 透传，Go 自动 fallback |
| 上游 4xx (auth等) | **原状态码透传** | 保留原始语义 |

---

## D-04: 三端点独立延迟检查，无跨端点缓存（性能 — P1）

### 问题详情

当 Go command 下载 `gin@v1.9.1` 时，按协议顺序发起:

```
GET /gin/@v/v1.9.1.info   → check_version_with_delay()   ← 第 1 次 .info 请求
GET /gin/@v/v1.9.1.mod    → check_version_with_delay()   ← 第 2 次 .info 请求 (同样的!)
GET /gin/@v/v1.9.1.zip    → check_version_with_delay()   ← 第 3 次 .info 请求 (又是同样的!)
```

**同一 `(module, version)` 的延迟检查被执行了 3 次**，每次都是完整的网络往返。

结合 D-01 的双重请求问题，最坏情况下:
- `info` 端点: 2 次 .info 请求 (check + re-fetch)
- `mod` 端点: 1 次 .info 请求 (check only) + 1 次 .mod 请求
- `zip` 端点: 1 次 .info 请求 (check only) + 1 次 .zip 请求
- **总计: 4 次 .info + 1 次 .mod + 1 次 .zip = 6 次请求**（最优只需 3 次）

### 改进方案: 引入请求级缓存

由于 Cloudflare Worker 是**无状态**的（每次请求独立），真正的跨请求缓存需要外部存储（KV/Redis）。但在**单次 Go command 的多次请求**场景下，可以利用 HTTP 层面的优化:

**方案 A: In-Memory Cache (单次请求内)**

Cloudflare Workers 可以使用 `WeakMap` 或模块级变量做**短暂缓存**（在同一 Worker 实例的生命周期内有效）:

```rust
use std::sync::Mutex;
use once_cell::sync::Lazy;

struct VersionInfoCache {
    entries: Mutex<std::collections::HashMap<String, CachedInfo>>,
}

struct CachedInfo {
    body: String,
    fetched_at: Instant,
    ttl: Duration,  // e.g., 30 seconds
}

static VERSION_CACHE: Lazy<VersionInfoCache> = Lazy::new(|| VersionInfoCache {
    entries: Mutex::new(std::collections::HashMap::new()),
});

async fn get_or_fetch_info(module: &str, version: &str, config: &Config) -> Result<String> {
    let cache_key = format!("{}@{}", module, version);

    // 1. 检查缓存
    if let Some(cached) = VERSION_CACHE.entries.lock().unwrap().get(&cache_key) {
        if cached.fetched_at.elapsed() < cached.ttl {
            return Ok(cached.body.clone());
        }
    }

    // 2. 缓存未命中 → 发起网络请求
    let url = format!("{}/{}/@v/{}.info", config.gomod_registry, escape_module_path(module), version);
    let body = fetch_url(&url).await?;

    // 3. 写入缓存
    VERSION_CACHE.entries.lock().unwrap().insert(cache_key, CachedInfo {
        body: body.clone(),
        fetched_at: Instant::now(),
        ttl: Duration::from_secs(30),
    });

    Ok(body)
}
```

**方案 B: Cloudflare KV Cache (跨请求持久化)**

对于生产环境，使用 CF KV 做持久化缓存:

```rust
const VERSION_INFO_KV: &str = "GOMOD_VERSION_INFO";

async fn get_or_fetch_info_kv(module: &str, version: &str, config: &Config) -> Result<String> {
    let kv_key = format!("info:{}:{}", module, version);

    // 1. 检查 KV
    if let Some(cached) = kv::get(VERSION_INFO_KV, kv_key.clone()).await? {
        if !cached.is_expired() {
            return Ok(String::from_utf8(cached.bytes())?);
        }
    }

    // 2. 未命中 → fetch
    let url = format!(...);
    let body = fetch_url(&url).await?;

    // 3. 写入 KV (TTL = delay_days * 2, 因为超过 delay_days后不会再被拒绝)
    let ttl = config.delay_days * 2 * 86400;
    kv::put(VERSION_INFO_KV, kv_key, body.clone(), Some(ttl as u64)).await?;

    Ok(body)
}
```

**收益对比**:

| 方案 | 命中率 | 实现成本 | 适用场景 |
|------|-------|---------|---------|
| 无缓存 (当前) | 0% | — | baseline |
| In-Memory | ~60%* | 低 | 单次构建内多次请求复用 |
| CF KV Cache | ~95%+ | 中 | 生产环境，跨用户复用 |

*\* 估算: 同一 Go command 的 3 个端点请求通常在数秒内发出，Worker 实例可能被复用*

---

## D-05: 双 Registry 数据不一致风险（一致性 — P1）

### 问题详情

配置中存在两个独立的 registry:

```rust
// config.rs
gomod_registry: "https://mirrors.aliyun.com/goproxy/",       // 元数据源 (A)
gomod_download_registry: "https://proxy.golang.org",          // 下载源 (B)
```

**数据流**:

```
延迟判断:  A (Aliyun) ──GET /.info──▶ Time="2023-11-15" ──▶ Allowed ✓
                                              ↓
ZIP 下载:  B (proxy.golang.org) ──GET /.zip──▶ 404 Not Found ✗
```

**可能的不一致场景**:
1. Aliyun 缓存更新延迟（比官方慢几分钟到几小时）
2. Aliyun 缓存了某个版本，但 proxy.golang.org 因法律/许可原因撤回了该版本
3. 两个源的版本列表不同步（如 Aliyun 只缓存了热门子集）
4. 网络分区导致 A 可达但 B 不可达

**当前代码中没有对此风险的任何处理**:

```rust
// handle_gomod_download: 延迟检查用 A，下载用 B —— 两者之间无任何一致性保证
let _ = check_version_with_delay(&module, version_clean, config, checker).await?;  // → A
let upstream_url = format!("{}/{}/@v/{}.zip", config.gomod_download_registry, ...); // → B
Fetch::Request(upstream_req).send().await  // B 返回什么? 不知道
```

### 改进方案

**方案 A: 统一 Registry（推荐用于简化部署）**

```rust
// config.rs - 移除 gomod_download_registry，统一使用 gomod_registry
pub struct Config {
    pub gomod_registry: String,          // 唯一的 Go 模块源
    // pub gomod_download_registry: String,  // 删除此字段
}
```

所有端点（list/info/mod/zip）都走同一个 registry，消除不一致根源。

**代价**: 如果阿里云镜像对大文件下载有限制或速度慢，会影响 zip 下载体验。

**方案 B: Download Fallback（保持双 Registry 但增加容错）**

```rust
pub async fn handle_gomod_download(req, config, checker) -> Result<Response> {
    match check_version_with_delay(...).await? {
        DelayCheckOutcome::Allowed => {
            // 先尝试主下载源
            let primary_url = format!("{}/{}/@v/{}.zip",
                config.gomod_download_registry, escaped, version);
            match try_download(&primary_url).await {
                Ok(resp) if resp.status_code() == 200 => return Ok(resp),
                // 主源失败 → fallback 到元数据源
                _ => {
                    let fallback_url = format!("{}/{}/@v/{}.zip",
                        config.gomod_registry, escaped, version);
                    try_download(&fallback_url).await?
                }
            }
        }
        // ...
    }
}
```

**方案 C: 延迟检查也使用 download registry 的 .info**

确保"判断可用性"和"实际下载"基于同一数据源:

```rust
async fn check_version_with_delay(module, version, config, checker) -> Result<DelayCheckOutcome> {
    // 对 zip 端点的延迟检查，使用 gomod_download_registry 而非 gomod_registry
    let registry = &config.gomod_download_registry;  // 与下载源一致
    let url = format!("{}/{}/@v/{}.info", registry, escaped, version);
    // ...
}
```

这需要让 `check_version_with_delay` 接受一个 `registry` 参数或在 `DelayCheckOutcome` 中标记来源。

**推荐**: 方案 C 作为最小改动，方案 A 作为长期目标。

---

## D-06: 无超时控制 + 无重试 + 无 fallback（可靠性 — P1）

### 问题详情

当前所有上游请求都是简单的 fire-and-forget:

```rust
let resp = Fetch::Request(upstream_req).send().await?;
// ❌ 没有 AbortController/timeout
// ❌ 没有 retry 逻辑
// ❌ 没有多 registry fallback
// ❌ 上游挂了 → 客户端直接收到 502
```

**Cloudflare Worker 的 Fetch API 特性**:
- 默认无超时限制（但 CF 平台有 30 秒 CPU 时间限制）
- 支持 `AbortController` 设置超时
- 不原生支持 retry（需手动实现）
- 免费 plan 有每日请求数限制

### 改进方案

**1. 请求超时**:

```rust
use web_sys::AbortController;

async fn fetch_with_timeout(url: &str, timeout_ms: u32) -> Result<Response> {
    let controller = AbortController::new()?;
    controller.set_timeout_with_ms(timeout_ms)?;

    let timer_id = setTimeout(
        || { controller.abort(); },
        timeout_ms,
    );

    let req = Request::new(url, Method::Get)?;
    req.set_signal(Some(controller.signal()))?;

    match Fetch::Request(req).send().await {
        Ok(resp) => {
            clearTimeout(timer_id);
            Ok(resp)
        }
        Err(e) if is_abort_error(&e) => Err(format!("Request timed out after {}ms", timeout_ms).into()),
        Err(e) => Err(e.into()),
    }
}

// 使用建议的超时:
// - .info / .mod: 5s (小文件)
// - .list: 10s (中等文件)
// - .zip: 30s (大文件)
// - 全局兜底: 15s
```

**2. 重试机制** (仅对 5xx 和网络错误):

```rust
async fn fetch_with_retry(url: &str, max_retries: u32, timeout_ms: u32) -> Result<Response> {
    let mut last_err = None;
    for attempt in 0..=max_retries {
        match fetch_with_timeout(url, timeout_ms).await {
            Ok(resp) if resp.status_code() < 500 => return Ok(resp),
            Ok(resp) => {
                // 5xx: 记录并重试
                console_warn!("Attempt {}: upstream returned {}", attempt + 1, resp.status_code());
                last_err = Some(format!("upstream {}", resp.status_code()));
            }
            Err(e) => {
                console_warn!("Attempt {}: fetch failed: {:?}", attempt + 1, e);
                last_err = Some(e.to_string());
            }
        }
        if attempt < max_retries {
            // 指数退避: 100ms, 400ms, 900ms...
            let delay_ms = 100 * (2_u64.pow(attempt));
            sleep(Duration::from_millis(delay_ms)).await;
        }
    }
    Err(last_err.unwrap_or_else(|| "all retries exhausted".into()))
}
```

**3. GOPROXY 链式 Fallback**:

```rust
// Config 中支持逗号分隔的 fallback 链 (与 Go 的 GOPROXY 语法兼容):
// GOMOD_REGISTRY=https://mirror1.example.com,https://mirror2.example.com,direct

async fn fetch_from_chain(
    registries: &[&str],
    path: &str,
    max_retries: u32,
) -> Result<(Response, &str)> {
    for (i, registry) in registries.iter().enumerate() {
        if *registry == "direct" || *registry == "off" {
            continue; // direct/off 在 Worker 环境下暂不支持
        }

        let url = format!("{}/{}", registry.trim_end_matches('/'), path);
        match fetch_with_retry(&url, max_retries, 10_000).await {
            Ok(resp) => return Ok((resp, *registry)),
            Err(e) => {
                console_warn!("Registry #{} ({}) failed: {}", i + 1, registry, e);
                continue; // 尝试下一个
            }
        }
    }
    Err("all registries in chain exhausted".into())
}
```

---

## D-07: 伪版本时间戳未被利用（正确性 — P1）

### 问题详情

Go 的伪版本 (pseudo-version) 格式**内置了提交时间戳**:

```
v0.0.0-20240101120000-abc123def456
 ││ │  │              │
 ││ │  │              └─ commit hash (12 hex chars)
 ││ │  └──────────────── YYYYMMDDHHmmss (UTC timestamp!)
 ││ └──────────────────── 伪版本固定前缀
 │└─────────────────────── 补丁版本号
 └────────────────────────── 主版本号 (0 = 无正式版本)
```

**当前实现对伪版本的处理**:

```rust
let version = version_raw.trim_end_matches(".zip");
// 不管 version 是 "v1.9.1" 还是 "v0.0.0-20240101..." ，都走相同的流程:
check_version_with_delay(module, version, config, checker).await?
// → 向上游请求 .info → 解析 Time → 比较
```

对于伪版本，我们可以**直接从版本字符串中提取时间戳**，**零网络请求**完成延迟检查!

### 改进方案

```rust
fn extract_pseudo_version_time(version: &str) -> Option<DateTime<Utc>> {
    // 匹配伪版本格式: vX.Y.Z-YYYYMMDDHHmmss-xxxxxxxxxxxx
    // 示例: v0.0.0-20240101120000-abc123def456
    let parts: Vec<&str> = version.split('-').collect();
    if parts.len() >= 3 {
        let time_str = parts.get(parts.len() - 2)?;
        // 必须是 14 位纯数字 (YYYYMMDDHHmmss)
        if time_str.len() == 14 && time_str.chars().all(|c| c.is_ascii_digit()) {
            // 转换为 RFC3339: "20240101120000" → "2024-01-01T12:00:00Z"
            let formatted = format!(
                "{}-{}-{}T{}:{}:{}Z",
                &time_str[0..4], &time_str[4..6], &time_str[6..8],
                &time_str[8..10], &time_str[10..12], &time_str[12..14]
            );
            return formatted.parse::<DateTime<Utc>>().ok();
        }
    }
    None
}

async fn smart_check_version(
    module: &str, version: &str, config: &Config, checker: &DelayChecker,
) -> Result<DelayCheckOutcome> {
    // 快速路径: 伪版本 → 零网络开销
    if let Some(pseudo_time) = extract_pseudo_version_time(version) {
        return if checker.is_version_allowed(&pseudo_time) {
            Ok(DelayCheckOutcome::Allowed)
        } else {
            Ok(DelayCheckOutcome::Denied { publish_time: pseudo_time })
        };
    }

    // 慢速路径: 正式版本 → 需要 .info 查询
    check_version_with_delay(module, version, config, checker).await
}
```

**收益**:
- 伪版本在 Go 生态中非常常见（尤其是没有打 tag 的 commit）
- 每个伪版本的延迟检查节省 **1 次 DNS + TCP + TLS + HTTP 往返**
- 实现简单，正则匹配即可

---

## D-08: 大量代码重复（可维护性 — P2）

### 问题详情

三个 handler 函数的结构高度相似:

```
handle_gomod_version_info:     (~86 行)
  ├─ parse path → module, version (.info)
  ├─ check_version_with_delay()
  ├─ match outcome { Allowed / Denied / NotFound / Error }
  │   ├─ Allowed: log → fetch .info → return JSON
  │   ├─ Denied: build_forbidden_response()
  │   ├─ NotFound: return 404 JSON
  │   └─ Error: return 502 JSON

handle_gomod_go_mod:           (~109 行)
  ├─ parse path → module, version (.mod)
  ├─ check_version_with_delay()                          ← 相同
  ├─ match outcome { Allowed / Denied / NotFound / Error } ← 相同
  │   ├─ Allowed: log → fetch .mod → return text         ← 仅 URL/CT 不同
  │   ├─ Denied: build_forbidden_response()               ← 相同
  │   ├─ NotFound: return 404 JSON                         ← 相同
  │   └─ Error: return 502 JSON                           ← 相同

handle_gomod_download:         (~68 行)
  ├─ parse path → module, version (.zip)
  ├─ check_version_with_delay()                          ← 相同
  ├─ match outcome { Allowed / Denied / NotFound / Error } ← 相同
  │   ├─ Allowed: log → fetch .zip → stream response     ← 仅 URL 不同
  │   ├─ Denied: build_forbidden_response()               ← 相同
  │   ├─ NotFound: return 404 JSON                         ← 相同
  │   └─ Error: return 502 JSON                           ← 相同
```

**重复代码占比约 60%**。修改延迟检查逻辑需要改 3 处，容易遗漏。

### 改进方案: 统一调度函数

```rust
/// GOPROXY 端点类型
#[derive(Clone, Copy)]
enum GomodEndpoint {
    List,
    Latest,
    Info,
    Mod,
    Zip,
}

impl GomodEndpoint {
    fn extension(&self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Latest => "latest",
            Self::Info => ".info",
            Self::Mod => ".mod",
            Self::Zip => ".zip",
        }
    }

    fn content_type(&self) -> &'static str {
        match self {
            Self::List => "text/plain; charset=utf-8",
            Self::Latest | Self::Info => "application/json",
            Self::Mod => "text/plain; charset=utf-8",
            Self::Zip => "application/octet-stream",
        }
    }

    /// 是否需要延迟检查 (list/latest 不需要或特殊处理)
    fn needs_delay_check(&self) -> bool {
        !matches!(self, Self::List)
    }

    /// 是否应该流式转发 (zip 大文件)
    fn should_stream(&self) -> bool {
        matches!(self, Self::Zip)
    }
}

/// 统一的 gomod 请求处理器
pub async fn handle_gomod_request(
    req: Request,
    config: &Config,
    checker: &DelayChecker,
    endpoint: GomodEndpoint,
) -> Result<Response> {
    let logger = DelayLogger::new();
    let client_ip = extract_client_ip(&req);

    // 1. 解析路径
    let (module, version) = parse_gomod_path(&req.path(), endpoint)?;

    // 2. 延迟检查 (list 端点跳过或特殊处理)
    if endpoint.needs_delay_check() {
        match smart_check_version(&module, &version, config, checker).await? {
            DelayCheckResult::Allowed { info_body } => {
                logger.log_allowed(PackageType::GoMod, &module, &version, &client_ip);
                // 3a. 构造上游 URL 并请求
                let upstream_url = build_upstream_url(&module, &version, config, endpoint)?;
                return fetch_and_respond(upstream_url, endpoint, info_body).await;
            }
            DelayCheckResult::Denied { publish_time } => {
                return build_forbidden_response(&module, &version, &publish_time, checker, &logger, client_ip.as_deref());
            }
            DelayCheckResult::NotFound => {
                return Response::error(not_found_json(&module, &version), 404);
            }
            DelayCheckResult::UpstreamError(status) => {
                return Response::error(upstream_error_json(status), 502);
            }
        }
    }

    // 3b. 无需延迟检查的端点 (list/latest): 直接代理
    let upstream_url = build_upstream_url(&module, &version, config, endpoint)?;
    fetch_and_respond(upstream_url, endpoint, None).await
}

/// Router 中的分发逻辑简化为:
async fn dispatch_gomod(req, parts, config, checker) -> Option<HandlerResult> {
    let last = parts.last()?;
    let endpoint = match *last {
        "list" => Some(GomodEndpoint::List),
        "latest" => Some(GomodEndpoint::Latest),
        v if v.ends_with(".info") => Some(GomodEndpoint::Info),
        v if v.ends_with(".mod") => Some(GomodEndpoint::Mod),
        v if v.endswith(".zip") => Some(GomodEndpoint::Zip),
        _ => None,
    }?;
    Some(handle_gomod_request(req, config, checker, endpoint).await)
}
```

**重构前后对比**:

| 指标 | 重构前 | 重构后 |
|------|--------|--------|
| Handler 总行数 | ~263 行 (3 个函数) | ~80 行 (1 个通用函数 + 辅助) |
| 延迟检查逻辑副本 | 3 份 | 1 份 |
| 路径解析逻辑副本 | 3 份 | 1 份 |
| 错误处理逻辑副本 | 3 份 (×4 分支 = 12 处) | 1 份 (×4 分支 = 4 处) |
| 新增端点成本 | 复制 ~80 行 + 改 4 处 | 加 1 个 enum variant |

---

## D-09: compare_versions 不支持语义化版本（正确性 — P2）

### 问题详情

[delay_check.rs:245-249](../delayMirror/src/core/delay_check.rs#L245-L249) 中的版本比较:

```rust
fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let a_parts: Vec<u64> = a.split('.').filter_map(|s| s.parse().ok()).collect();
    let b_parts: Vec<u64> = b.split('.').filter_map(|s| s.parse().ok()).collect();
    a_parts.cmp(&b_parts)
}
```

**问题**:

| 输入 | 期望结果 | 实际结果 | 影响 |
|------|---------|---------|------|
| `"1.0.0-beta"` vs `"1.0.0-alpha"` | beta > alpha | **Equal** (都 parse 为 `[1,0,0]`) | pre-release 排序错误 |
| `"1.0.0"` vs `"1.0.0-rc.1"` | release > rc | **Equal** (rc.1 被 filter_map 丢弃) | 无法区分正式版和预发布版 |
| `"2.0.0+meta"` vs `"2.0.0"` | Equal | Equal (侥幸正确) | +build 元数据丢失 |
| `"v1.9.1"` vs `"1.9.1"` | Equal | **Less** ("v" 导致 parse 失败) | 带 v 前缀的比较出错 |

**影响链路**: `find_eligible_version` → `compare_versions` → 返回**错误的最佳可用版本**。

例如: 用户请求 `v3.0.0`(太新)，系统应在 `{v1.0.0, v2.0.0-beta, v2.0.0}` 中选 `v2.0.0`。但由于 `v2.0.0-beta` 和 `v2.0.0` 被视为相等，可能返回错误结果。

### 改进方案

**方案 A: 引入轻量 semver 库**

```toml
# Cargo.toml
[dependencies]
semver = "1"  # 纯 Rust 实现，no_std 友好
```

```rust
use semver::{Version, Prerelease};

fn compare_versions_semver(a: &str, b: &str) -> std::cmp::Ordering {
    let normalize = |v: &str| -> Option<Version> {
        let v = v.strip_prefix('v').unwrap_or(v);
        Version::parse(v).ok()
    };

    match (normalize(a), normalize(b)) {
        (Some(va), Some(vb)) => va.cmp(&vb),
        _ => a.cmp(b),  // fallback: 字符串比较
    }
}
```

**方案 B: 手动实现最小化 semver 比较 (零依赖)**

如果不想增加依赖，可以实现一个精简版的 semver comparator:

```rust
fn compare_semver(a: &str, b: &str) -> Ordering {
    let (maj_a, min_a, patch_a, pre_a) = parse_semver(a);
    let (maj_b, min_b, patch_b, pre_b) = parse_semver(b);

    maj_a.cmp(&maj_b)
        .then(min_a.cmp(&min_b))
        .then(patch_a.cmp(&patch_b))
        .then(compare_pre_release(&pre_a, &pre_b))  // "" > "alpha" < "beta" < "rc"
}

/// Pre-release 排序: 无 < alpha < beta < rc (字典序)
fn compare_preRelease(a: &str, b: &str) -> Ordering {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,  // 正式版 > 预发布版
        (false, true) => Ordering::Less,
        (false, false) => a.cmp(b),           // 字典序: alpha < beta < rc
    }
}
```

**推荐**: 方案 A（semver crate 成熟稳定，~500KB 增量可接受）。

---

## D-10: Registry URL 尾部斜杠不一致（配置 — P2）

### 问题详情

```rust
// config.rs Default:
gomod_registry: "https://mirrors.aliyun.com/goproxy/",       // ← 有尾部 /
gomod_download_registry: "https://proxy.golang.org",          // ← 无尾部 /
```

虽然代码中有 `trim_end_matches('/')` 保护:
```rust
config.gomod_registry.trim_end_matches('/')
config.gomod_download_registry.trim_end_matches('/')
```

但这种**隐式的不一致**容易在未来维护中引入 bug（比如新增代码忘记 trim）。

### 改进方案

**统一约定: Config 存储时不带尾部斜杠，在 display/序列化时按需添加**:

```rust
impl Config {
    fn gomod_base_url(&self) -> &str {
        self.gomod_registry.trim_end_matches('/')
    }

    fn gomod_download_base_url(&self) -> &str {
        self.gomod_download_registry.trim_end_matches('/')
    }

    /// 构造完整 GOPROXY URL (统一处理斜杠)
    fn gomod_url(&self, module: &str, endpoint_path: &str) -> String {
        format!("{}/{}{}", self.gomod_base_url(), module, endpoint_path)
    }
}

// 使用:
let url = config.gomod_url(&escaped_module, "/@v/list");
let url = config.gomod_url(&escaped_module, &format!("/@v/{}.info", version));
```

Default 值统一不带 `/`:
```rust
gomod_registry: "https://mirrors.aliyun.com/goproxy".to_string(),         // 无 /
gomod_download_registry: "https://proxy.golang.org".to_string(),          // 无 /
```

---

## D-11: 缺乏可观测性（P3 — Low Priority）

### 问题详情

当前只有两种日志输出:
1. `console_error!` — 上游请求失败时
2. `logger.log_blocked()` / `logger.log_allowed()` — 延迟检查结果

**缺失的可观测性维度**:

| 维度 | 当前状态 | 价值 |
|------|---------|------|
| 请求级耗时 | ❌ 无 | 定位慢请求根因 |
| 上游响应时间 | ❌ 无 | 评估 registry 性能 |
| 缓存命中率 | ❌ 无 | 评估缓存效果 |
| 各端点 QPS | ❌ 无 | 容量规划 |
| 延迟拦截率 | ✅ 有 (blocked log) | 安全审计 |
| 错误分类统计 | ❌ 部分 | SLA 监控 |
| 版本分布 | ❌ 无 | 了解用户需求 |

### 改进方案 (简要)

```rust
// 1. 结构化计时
struct RequestTiming {
    start: Instant,
    labels: HashMap<String, String>,  // module, version, endpoint, outcome
}

// 2. 在关键节点记录
timing.mark("delay_check_start");
let outcome = check_version_with_delay(...).await;
timing.mark("delay_check_end");

timing.mark("upstream_fetch_start");
let body = fetch_upstream(...).await;
timing.mark("upstream_fetch_end");

// 3. 输出 (CF Workers 可用 console.log 或写入自定义 metric service)
console_log!("{}", timing.summary());

// 4. 响应头中添加调试信息 (debug_mode 下)
headers.set("X-Timing-Delay-Check-ms", &timing.duration("delay_check"))?;
headers.set("X-Timing-Upstream-ms", &timing.duration("upstream"))?;
headers.set("X-Cache-Hit", cache_hit.to_string())?;
```

---

## 改进优先级路线图

### Phase 1: 紧急修复 (预计 1-2 天)

| 缺陷 | 改动量 | 风险 | 收益 |
|------|-------|------|------|
| **D-01** .info 双重请求 | 小 (改 1 函数签名 + 3 处调用) | 低 | 减少 ~50% 上游请求 |
| **D-03** 状态码修正 (403→404) | 小 (改 1 处) | 低 | Go client 正确 fallback |
| **D-07** 伪版本快速路径 | 小 (新增 1 函数 +15 行) | 低 | 伪版本零开销 |

### Phase 2: 重要增强 (预计 2-3 天)

| 缺陷 | 改动量 | 风险 | 收益 |
|------|-------|------|------|
| **D-08** 代码去重重构 | 中 (重写 3→1 handler) | 中 | 维护成本降低 60% |
| **D-03** 添加 @latest 端点 | 小 (新增 1 函数 + router 1 行) | 低 | 减少 list 请求 |
| **D-06** 请求超时 | 小 (封装 Fetch) | 低 | 防止无限挂起 |
| **D-09** semver 比较 | 小 (加依赖 + 替换 1 函数) | 低 | 版本选择正确性 |

### Phase 3: 架构优化 (预计 3-5 天)

| 缺陷 | 改动量 | 风险 | 收益 |
|------|-------|------|------|
| **D-02** list 延迟过滤 | 中 (新增并发查询逻辑) | 中 | 消除信息泄露 |
| **D-04** 请求级缓存 | 中 (新增 cache 模块) | 中 | 再减 ~60% 请求 |
| **D-05** Registry 一致性 | 中 (改 config + fallback 逻辑) | 中 | 消除数据不一致 |
| **D-06** Retry + Fallback 链 | 中 (改 fetch 层) | 中 | 高可用性 |

### Phase 4: 可观测性 (持续)

| 缺陷 | 改动量 | 风险 | 收益 |
|------|-------|------|------|
| **D-11** 结构化指标 | 中 (新增 metrics 模块) | 低 | 运维效率提升 |

---

## 附录: 代码改动量估算

```
gomod.rs (当前: 750 行)
├── D-01:  check_version_with_delay → check_version_with_delay_cached    ~+15/-5 行
├── D-02:  handle_gomod_version_list → 添加过滤逻辑                       ~+25 行
├── D-03:  新增 handle_gomod_latest + 状态码修正                            ~+35/-3 行
├── D-04:  新增 VERSION_CACHE / get_or_fetch_info                          ~+40 行
├── D-05:  handle_gomod_download 添加 fallback                              ~+15 行
├── D-06:  封装 fetch_with_timeout / fetch_with_retry                      ~+50 行
├── D-07:  新增 extract_pseudo_version_time + smart_check_version           ~+25 行
├── D-08:  重构为 handle_gomod_request + GomodEndpoint enum                 ~-180/+90 行 (净减 ~90)
├── D-09:  替换 compare_versions → compare_versions_semver                  ~+5/-5 行
├── D-10:  Config 增加 helper 方法                                          ~+15/-10 行
└── D-11:  新增 RequestTiming 结构                                         ~+40 行

预估净变化: +230/-200 行 (约 +10%，主要来自新增功能)
Phase 1 完成后即可解决最关键的 3 个 P0 问题
```
