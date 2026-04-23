# Pip 下载库完整网络请求流程深度技术分析

## 概述

本文档基于 **PyPI 官方源代码实现**（PEP 503、PEP 691）、**pip 官方 API 文档**以及本项目 `delayMirror` 和 `gateway` 中的 PyPI 代理实现，对 `pip install` 命令执行过程中发起的所有网络请求进行完整的技术分析。

---

## 一、整体架构与请求流程概览

### 1.1 Pip 下载流程的五个核心阶段

```
┌─────────────────────────────────────────────────────────────────────┐
│                    pip install <package> 完整流程                     │
├─────────────────────────────────────────────────────────────────────┤
│                                                                     │
│  阶段1: 索引发现          阶段2: 包元数据获取      阶段3: 版本选择     │
│  ┌──────────────┐      ┌──────────────┐      ┌──────────────┐       │
│  │ GET /simple/ │ ───► │ GET /simple/  │ ───► │ JSON API     │       │
│  │              │      │ <package>/    │      │ /pypi/<pkg>  │       │
│  │ 可选(缓存)   │      │              │      │ /json        │       │
│  └──────────────┘      └──────────────┘      └──────┬───────┘       │
│                                                      │               │
│                              阶段4: 依赖树构建 ◄──────┘               │
│                         ┌──────────────────────────┐                 │
│                         │ 递归解析每个依赖包         │                 │
│                         │ (重复阶段2-3)             │                 │
│                         └────────────┬─────────────┘                 │
│                                      │                               │
│                              阶段5: 文件下载                          │
│                         ┌──────────────────────────┐                 │
│                         │ GET <file_url>            │                 │
│                         │ + SHA256 校验             │                 │
│                         └──────────────────────────┘                 │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

### 1.2 关键设计原则（工程考量）

| 原则 | 说明 | 对应 PEP |
|------|------|----------|
| **零配置发现** | 客户端无需预先配置即可确定仓库支持的 API 版本 | PEP 691 |
| **向后兼容** | 新 API 必须不破坏仅支持 HTML 的旧客户端 | PEP 691 |
| **最小化请求** | 使用新 API 不应显著增加 HTTP 请求数量 | PEP 691 |
| **内容协商** | 通过 Accept 头而非 URL 路径区分格式 | HTTP/1.1 |
| **哈希安全** | 下载后必须校验完整性，防止供应链攻击 | PEP 458/TUF |

---

## 二、阶段一：索引发现（Index Discovery）

### 2.1 请求详情

```http
GET /simple/ HTTP/1.1
Host: pypi.org
Accept: text/html
User-Agent: pip/23.x.x
```

**或 JSON 格式（PEP 691，pip >= 23.x）：**

```http
GET /simple/ HTTP/1.1
Host: pypi.org
Accept: application/vnd.pypi.simple.v1+json;q=0.9,
        text/html;q=0.8,*/*;q=0.5
User-Agent: pip/23.x.x
```

### 2.2 响应格式

**HTML 格式（PEP 503 兼容）：**

```html
<!DOCTYPE html>
<html>
<head><title>Simple Index</title></head>
<body>
    <a href="/simple/requests/">requests</a>
    <a href="/simple/numpy/">numpy</a>
    <a href="/simple/django/">django</a>
</body>
</html>
```

**JSON 格式（PEP 691）：**

```json
{
  "meta": {"api-version": "1.0"},
  "projects": [
    {"name": "requests"},
    {"name": "numpy"},
    {"name": "django"}
  ]
}
```

### 2.3 设计意图分析

#### 为什么需要索引发现？

1. **验证仓库可用性**：确认目标仓库是否在线且可访问
2. **API 能力探测**：通过响应头或内容判断仓库支持的功能集
3. **缓存策略**：HTTP 缓存头（ETag, Last-Modified）支持增量更新

#### 内容协商机制（Content Negotiation）

从项目实现 [pypi.rs:330-341](delayMirror/src/workers/handlers/pypi.rs#L330-L341) 可以看到：

```rust
let accept_header = _req
    .headers()
    .get("Accept")
    .map(|v| v.unwrap_or_default())
    .unwrap_or_default();

let is_json_api = accept_header.contains("application/vnd.pypi.simple.v1+json");
let content_type = if is_json_api {
    "application/vnd.pypi.simple.v1+json"
} else {
    "text/html; charset=utf-8"
};
```

**设计决策**：
- 使用标准 HTTP `Accept` 头进行内容协商，而非引入新的 URL 路径
- 支持 `q` 值权重排序，允许客户端表达偏好优先级
- 保持向后兼容：不发送 Accept 或只接受 `text/html` 的客户端仍能正常工作

#### 本项目网关层的过滤实现

从 [gateway/pypi.rs:256-336](gateway/src/handlers/pypi.rs#L256-L336) 可以看到，网关在转发索引时会进行白名单过滤：

```rust
let filtered_links: Vec<String> = re
    .captures_iter(&body)
    .filter_map(|cap| {
        let package_name = cap.get(2)?.as_str();
        let normalized = normalize_package_name(package_name);
        if allowed_packages.contains(&normalized) {
            Some(format!(
                r#"<a href="/pypi/simple/{}/">{}</a>"#,
                package_name, package_name
            ))
        } else {
            None  // 不在白名单中的包被隐藏
        }
    })
    .collect();
```

**安全意义**：企业级网关可以通过隐藏不在白名单中的包来实施"隐式拒绝"策略。

---

## 三、阶段二：包文件列表获取（Package File Listing）

### 3.1 请求详情

```http
GET /simple/requests/ HTTP/1.1
Host: pypi.org
Accept: application/vnd.pypi.simple.v1+json;q=0.9,
        text/html;q=0.8
User-Agent: pip/23.x.x (CPython 3.11)
```

### 3.2 响应格式对比

#### HTML 格式（PEP 503 - 传统格式）

```html
<!DOCTYPE html>
<html>
<head>
    <meta name="pypi:repository-version" content="1.0">
    <title>Links for requests</title>
</head>
<body>
    <h1>Links for requests</h1>

    <!-- Wheel 文件 -->
    <a href="https://files.pythonhosted.org/packages/.../requests-2.31.0-py3-none-any.whl#sha256=abc123def..."
       data-requires-python=">=3.7">
        requests-2.31.0-py3-none-any.whl
    </a><br/>

    <!-- Source Distribution -->
    <a href="https://files.pythonhosted.org/packages/.../requests-2.31.0.tar.gz#sha256=xyz789...">
        requests-2.31.0.tar.gz
    </a><br/>

    <!-- 已撤销版本（Yanked） -->
    <a href="https://.../requests-2.30.0-py3-none-any.whl#sha256=..."
       data-yanked="Reason: security vulnerability">
        requests-2.30.0-py3-none-any.whl
    </a><br/>
</body>
</html>
```

#### JSON 格式（PEP 691 - 推荐格式）

```json
{
  "meta": {"api-version": "1.0"},
  "name": "requests",
  "files": [
    {
      "filename": "requests-2.31.0-py3-none-any.whl",
      "url": "https://files.pythonhosted.org/packages/.../requests-2.31.0-py3-none-any.whl",
      "hashes": {
        "sha256": "abc123def456...",
        "blake2b-256": "789def..."
      },
      "requires-python": ">=3.7",
      "dist-info-metadata": false,
      "gpg-sig": false,
      "yanked": false
    },
    {
      "filename": "requests-2.31.0.tar.gz",
      "url": "https://files.pythonhosted.org/packages/.../requests-2.31.0.tar.gz",
      "hashes": {
        "sha256": "xyz789abc..."
      },
      "requires-python": ">=3.7"
    }
  ]
}
```

### 3.3 HTML 解析的复杂性（为什么需要 PEP 691）

从项目实现 [pypi.rs:139-178](gateway/src/handlers/pypi.rs#L139-L178) 可以看到 HTML 解析的复杂性：

```rust
fn parse_html_to_files(html: &str, base_url: &str) -> Vec<FileEntry> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("a").unwrap();

    document.select(&selector).filter_map(|element| {
        // 1. 提取 href 属性
        let href = element.value().attr("href")?;
        // 2. 提取链接文本作为文件名
        let filename = element.text().next()?.to_string();
        // 3. 处理相对 URL
        let url = if href.starts_with("http") { ... } else { ... };
        // 4. 从 URL fragment中解析哈希值（如 #sha256=xxx&md5=yyy）
        if let Some(fragment) = href.split('#').nth(1) {
            for part in fragment.split('&') {
                if let Some((algo, hash)) = part.split_once('=') {
                    hashes.insert(algo.to_string(), hash.to_string());
                }
            }
        }
        // 5. 提取 data-requires-python 属性
        let requires_python = element.value().attr("data-requires-python")...;
        ...
    }).collect()
}
```

**PEP 691 解决的问题**：

| 问题 | HTML (PEP 503) | JSON (PEP 691) |
|------|----------------|----------------|
| 哈希值位置 | 嵌入在 URL 片段中（非标准用法） | 独立的 `hashes` 字典对象 |
| 数据类型 | 全部是字符串，需手动解析 | 强类型（布尔值、字符串、对象） |
| 解析复杂度 | 需要 HTML5 解析器（如 scraper/lxml） | 标准库 `json` 即可 |
| 扩展性 | 依赖自定义 data-* 属性 | 结构化的 schema 易于扩展 |

### 3.4 项目实现中的双重代理模式

本项目实现了两种不同的 PyPI 代理模式：

#### 模式 A：延迟镜像（delayMirror）- [pypi.rs:251-356](delayMirror/src/workers/handlers/pypi.rs#L251-L356)

```rust
async fn handle_pypi_package_list_inner(...) -> Result<Response> {
    // 1. 转发请求到上游 PyPI Simple API
    let pypi_simple_url = format!("{}{}/", config.pypi_registry, normalized_package);
    let simple_resp = Fetch::Request(simple_req).send().await?;

    // 2. 额外调用 JSON API 获取发布时间信息（用于延迟检查）
    let release_info = fetch_pypi_release_info(package, &config.pypi_json_api_base).await;

    // 3. 基于延迟策略检查版本是否允许下载
    let recent_versions_warning = match release_info {
        Ok(info) => {
            // 检查哪些版本因太新而被阻止
            ...
        }
        ...
    };

    // 4. 返回原始 Simple API 响应（不做文件级过滤）
    // 延迟检查在下载时才强制执行
}
```

**设计意图**：
- 列表阶段只做"警告"，不做"阻断"——让用户能看到所有可用版本
- 延迟策略在**下载阶段**才生效，避免影响 `pip list` 等只读操作
- 额外的 JSON API 调用用于获取精确的上传时间戳

#### 模式 B：网关层（gateway）- [pypi.rs:41-137](gateway/src/handlers/pypi.rs#L41-L137)

```rust
pub async fn get_package_list(...) -> impl Responder {
    // 1. 从上游获取 Simple API 响应
    let response = http_client.get(&url).send().await?;
    let body = response.text().await?;
    let files = parse_html_to_files(&body, &config.pypi_registry);

    // 2. 基于白名单过滤版本
    let allowed_versions = allowlist_manager
        .get_pypi_config()
        .entries
        .iter()
        .filter(...)
        .flat_map(|entry| entry.versions.iter().cloned())
        .collect::<Vec<String>>();

    // 3. 过滤掉不允许的版本的文件
    let filtered_files: Vec<FileEntry> = files
        .into_iter()
        .filter(|file| {
            if let Some((_, version)) = parse_package_filename(&file.filename) {
                allowed_versions.contains(&version)
            } else { false }
        })
        .collect();

    // 4. 根据 Accept 头返回 HTML 或 JSON
}
```

**设计差异**：
- Gateway 模式在列表阶段就**完全隐藏**不允许的版本
- 更严格的安全控制，但可能影响用户体验（看不到被屏蔽的版本）

---

## 四、阶段三：JSON 元数据 API（可选但重要）

### 4.1 请求详情

```http
GET /pypi/requests/json HTTP/1.1
Host: pypi.org
Accept: application/json
User-Agent: pip/23.x.x
```

### 4.2 响应结构

```json
{
  "info": {
    "name": "requests",
    "version": "2.31.0",
    "summary": "Python HTTP for Humans.",
    "home_page": "https://docs.python-requests.org/",
    "author": "Kenneth Reitz",
    "license": "Apache 2.0",
    "requires_dist": [
      "charset-normalizer>=2,<4",
      "idna>=2.5,<4",
      "urllib3>=1.21.1,<3",
      "certifi>=2017.4.17"
    ],
    "requires_python": ">=3.7",
    ...
  },
  "last_serial": 12345678,
  "urls": [...],
  "releases": {
    "2.31.0": [
      {
        "filename": "requests-2.31.0-py3-none-any.whl",
        "url": "https://files.pythonhosted.org/packages/.../requests-2.31.0-py3-none-any.whl",
        "packagetype": "bdist_wheel",
        "python_version": "py3",
        "size": 62745,
        "upload_time": "2023-10-15T12:34:56Z",
        "has_sig": false,
        "md5_digest": "...",
        "sha256_digest": "abc123..."
      },
      {
        "filename": "requests-2.31.0.tar.gz",
        "url": "https://files.pythonhosted.org/packages/.../requests-2.31.0.tar.gz",
        "packagetype": "sdist",
        "python_version": "source",
        "size": 102400,
        "upload_time": "2023-10-15T12:34:55Z",
        ...
      }
    ],
    "2.30.0": [...],
    "2.29.0": [...]
  }
}
```

### 4.3 关键字段解析

#### info.requires_dist — 依赖声明

这是构建依赖树的**核心数据源**：

```json
"requires_dist": [
  "charset-normalizer>=2,<4",           // 版本范围约束
  "idna>=2.5,<4; extra == 'security'",  // 条件依赖（仅在安装 security extra 时）
  "urllib3>=1.21.1,<3",
  "certifi>=2017.4.17; python_version < '3'"  // 环境条件依赖
]
```

**设计意图**：
- 支持环境标记（environment markers）：`python_version < '3'`, `sys_platform == 'linux'`
- 支持 extras：`; extra == 'security'` 允许可选功能分组
- 版本使用 PEP 440 规范：`>=2,<4` 表示 `[2, 4)` 区间

#### releases 字段 — 历史版本时间线

从项目实现 [pypi.rs:147-171](delayMirror/src/workers/handlers/pypi.rs#L147-L171)：

```rust
fn build_version_time_info_from_releases(releases: &Value) -> Result<Value, DelayCheckError> {
    let releases_obj = releases.as_object()?;

    let mut time_map = serde_json::Map::new();
    for (version, files_arr) in releases_obj {
        if let Some(files) = files_arr.as_array() {
            if let Some(first_file) = files.first() {
                if let Some(upload_time) = first_file.get("upload_time").and_then(|t| t.as_str()) {
                    time_map.insert(version.clone(), Value::String(upload_time.to_string()));
                }
            }
        }
    }
    // ...
}
```

**关键点**：
- `upload_time` 是每个版本的**首次上传时间**
- 用于实现"延迟策略"——阻止下载最近 N 天内发布的版本
- 这是供应链安全的重要机制：给安全审计留出缓冲期

### 4.4 项目中的 JSON API 调用实现

从 [pypi.rs:73-139](delayMirror/src/workers/handlers/pypi.rs#L73-L139)：

```rust
async fn fetch_pypi_release_info(
    package: &str,
    json_api_base: &str,
) -> Result<Value, Response> {
    let normalized = normalize_package_name(package);
    let url = format!("{}/{}/json", json_api_base, normalized);

    let req = Request::new(&url, Method::Get)?;
    let mut resp = Fetch::Request(req).send().await?;

    // 错误处理：404 表示包不存在
    if status == 404 {
        return Err(Response::error(format!("Package not found on PyPI: {}", package), 404));
    }

    // 非 2xx 状态码视为上游错误
    if !(200..300).contains(&status) {
        return Err(Response::error(format!("PyPI JSON API error, status: {}", status), 502));
    }

    // 解析 JSON 响应
    let body = resp.text().await?;
    let metadata: Value = serde_json::from_str(&body)?;
    Ok(metadata)
}
```

**工程实践要点**：
1. **包名规范化**：所有包名先经过 `normalize_package_name()` 统一转换为小写并用连字符连接
2. **分层错误处理**：404（用户错误）vs 5xx（系统错误）vs 502（上游故障）
3. **序列化容错**：JSON 解析失败返回 500 而非崩溃

---

## 五、阶段四：依赖树构建（Dependency Resolution）

### 5.1 递归解析过程

```
pip install requests
    │
    ├── [请求] GET /simple/requests/ → 获取文件列表
    ├── [请求] GET /pypi/requests/json → 获取元数据和依赖
    │
    ├── 解析 requires_dist:
    │   ├── charset-normalizer>=2,<4
    │   │   ├── [请求] GET /simple/charset-normalizer/
    │   │   ├── [请求] GET /pypi/charset-normalizer/json
    │   │   └── (递归解析其依赖...)
    │   │
    │   ├── idna>=2.5,<4
    │   │   ├── [请求] GET /simple/idna/
    │   │   └── [请求] GET /pypi/idna/json
    │   │
    │   ├── urllib3>=1.21.1,<3
    │   │   └── (同上)
    │   │
    │   └── certifi>=2017.4.17
    │       └── (同上)
    │
    └── 构建有向无环图（DAG），解决版本冲突
```

### 5.2 版本冲突解决算法

pip 使用 **resolvlib** 库实现约束求解：

根据 [pip 官方文档](https://pip.pypa.io/en/stable/topics/more-dependency-resolution/)：

> Pip's interface to resolvelib is in the form of a "provider", which is the interface between pip's model of packages and the resolution algorithm. The provider deals in "candidates" and "requirements" and implements the following operations:
> - **identify** - implements identity for candidates and requirements
> - **get_preference** - returns preference values for candidates
> - **find_matches** - finds all candidates that match a requirement
> - **is_satisfied_by** - checks if a candidate satisfies a requirement
> - **get_dependencies** - gets dependencies of a candidate

### 5.3 网络请求优化策略

#### 缓存机制（减少重复请求）

从 [pip issue #12921](https://github.com/pypa/pip/issues/12921) 的讨论：

> We can also cache the result of querying the simple repository API for the list of dists available for a given dependency name! This additional caching requires messing around with HTTP caching headers to see if a given page has changed...

**优化手段**：
1. **HTTP 缓存头**：利用 `ETag` / `Last-Modified` / `If-None-Match` 实现条件请求
2. **内存缓存**：同一会话内的元数据查询结果复用
3. **本地磁盘缓存**：`~/.cache/pip/` 目录持久化存储

#### 并发请求

现代 pip（>=21.x）支持并发获取多个依赖的元数据：

```python
# 伪代码示意
async def resolve_all(dependencies):
    tasks = [fetch_metadata(dep) for dep in dependencies]
    results = await asyncio.gather(*tasks)
    return results
```

### 5.4 项目中的依赖树处理

本项目的网关和延迟镜像**不参与依赖解析**——这是 pip 的职责。项目的角色是：

1. **透明代理**：转发 pip 与 PyPI 之间的所有请求
2. **延迟拦截**：在下载阶段检查版本年龄
3. **日志记录**：记录被阻止/降级的下载尝试

---

## 六、阶段五：文件下载与校验（File Download & Verification）

### 6.1 下载请求详情

```http
GET /packages/.../requests-2.31.0-py3-none-any.whl HTTP/1.1
Host: files.pythonhosted.org
Range: bytes=0-
User-Agent: pip/23.x.x (CPython 3.11)
```

**注意**：实际下载域名是 `files.pythonhosted.org`，不是 `pypi.org`——这是出于 CDN 和性能考虑的设计。

### 6.2 下载后的校验流程

```
下载完成
    │
    ▼
┌─────────────────────────────────────────┐
│  计算 SHA256 哈希                        │
│  hashlib.sha256(downloaded_content)      │
└─────────────────┬───────────────────────┘
                  │
                  ▼
┌─────────────────────────────────────────┐
│  与 Simple API 返回的哈希值比较           │
│  expected_hash == computed_hash ?        │
└─────────────────┬───────────────────────┘
                  │
         ┌────────┴────────┐
         ▼                 ▼
      匹配              不匹配
         │                 │
         ▼                 ▼
    安装文件          抛出 HashMismatchError
                       删除已下载文件
                       重试或报错退出
```

### 6.3 项目中的下载拦截实现

从 [pypi.rs:358-539](delayMirror/src/workers/handlers/pypi.rs#L358-L539)，这是最核心的安全控制逻辑：

```rust
pub async fn handle_pypi_download(...) -> Result<Response> {
    // 1. 从文件名解析包名和版本
    let (package, version) = match parse_package_filename(filename) {
        Some(result) => result,
        None => return Response::error("Invalid filename format...", 400),
    };

    // 2. 获取客户端 IP（用于审计日志）
    let client_ip = _req.headers().get("CF-Connecting-IP").ok().flatten();

    // 3. 调用 JSON API 获取完整的发布信息
    let metadata = fetch_pypi_release_info(&package, &config.pypi_json_api_base).await?;

    // 4. 提取该版本的文件列表和时间戳
    let releases = metadata.get("releases");
    let upload_time_str = release_files?.first()?.get("upload_time")?;

    // 5. 构建版本→时间 映射
    let time_value = build_version_time_info_from_releases(&releases)?;
    let time_info = checker.parse_time_field(&time_value)?;

    // 6. 三种裁决结果：
    match checker.resolve_version(&version, &time_info)? {
        // ====== 情况A：允许下载 ======
        VersionCheckResult::Allowed => {
            let upstream_url = find_download_url_for_version(...)?;
            Fetch::Request(upstream_req).send().await  // 直接代理到上游
        }

        // ====== 情况B：自动降级到旧版本 ======
        VersionCheckResult::Downgraded { suggested_version } => {
            // 替换文件名中的版本号
            let redirected_filename = if filename.ends_with(".whl") {
                filename.replace(&format!("-{}-", version),
                                 &format!("-{}-", suggested_version))
            } else {
                build_sdist_filename(&package, &suggested_version)
            };

            // 记录降级事件
            logger.log_downgraded(PackageType::PyPI, &package, &version,
                                  &suggested_version, "Version too recent...", client_ip);

            // 在响应头中标注原始请求版本和实际下载版本
            headers.set("X-Delay-Original-Version", &version)?;
            headers.set("X-Delay-Redirected-Version", &suggested_version)?;

            Fetch::Request(upstream_req).send().await?
        }

        // ====== 情况C：拒绝下载 ======
        VersionCheckResult::Denied { .. } => {
            logger.log_blocked(PackageType::PyPI, &package, &version, &reason, client_ip);

            let body = serde_json::json!({
                "error": "Version too recent for download",
                "package": package,
                "requested_version": version,
                "reason": reason,
                "suggested_version": suggested  // 推荐一个允许的版本
            });

            Response::from_json(&body)?.with_status(403).with_headers(headers)
        }
    }
}
```

### 6.4 三种裁决策略的设计哲学

| 策略 | 触发条件 | 行为 | 适用场景 |
|------|---------|------|---------|
| **Allowed** | 版本年龄 > 延迟天数 | 直接透传下载 | 正常情况 |
| **Downgraded** | 存在满足条件的旧版本 | 自动替换为旧版并下载 | 开发环境，保证可用性 |
| **Denied** | 所有版本都太新或无替代 | 返回 403 + 建议 | 生产环境，强制合规 |

**降级策略的实现细节**：

Wheel 文件名格式：`{distribution}-{version}(-{build tag})?-{python tag}-{abi tag}-{platform tag}.whl`

例如：`numpy-1.24.3-cp310-cp310-manylinux_2_17_x86_64.whl`

降级时只需替换 `{version}` 部分，其他标签保持不变。

Sdist 文件名更简单：`{package}-{version}.tar.gz`

---

## 七、包名规范化（Normalization）— 跨阶段的关键操作

### 7.1 规范化规则（PEP 503）

从项目实现 [pypi.rs:13-16](delayMirror/src/workers/handlers/pypi.rs#L13-L16) 和 [pypi.rs:37-39](gateway/src/handlers/pypi.rs#L37-L39)：

```rust
pub fn normalize_package_name(name: &str) -> String {
    let re = Regex::new(r"[-_.]+").unwrap();
    re.replace_all(name, "-").to_lowercase()
}
```

**转换示例**：

| 输入 | 规范化后 | 说明 |
|------|---------|------|
| `Django` | `django` | 大写转小写 |
| `DJA_NGO` | `dja-ngo` | 下划线转连字符 |
| `some.package` | `some-package` | 点号转连字符 |
| `some_Package-Name` | `some-package-name` | 混合分隔符统一 |
| `Py-YAML` | `py-yaml` | 连字符保留 |

**设计意图**：
- PyPI 将这些变体视为**同一个包**
- 避免因大小写或分隔符差异导致的重复注册
- 所有 API 调用前都必须先规范化

### 7.2 文件名解析（Filename Parsing）

从 [pypi.rs:18-71](delayMirror/src/workers/handlers/pypi.rs#L18-L71)：

#### Wheel 文件解析

```rust
fn parse_wheel_filename(filename: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = filename.trim_end_matches(".whl").split('-').collect();

    // parts[0] = distribution name (e.g., "numpy")
    // parts[1] = version (e.g., "1.24.3")
    // parts[2] = Python tag (e.g., "cp310")
    // parts[3] = ABI tag (e.g., "cp310")
    // parts[4..] = Platform tag (e.g., "manylinux_2_17_x86_64")

    if parts.len() < 5 { return None; }

    let distribution = parts[0];
    let version = parts[1];

    // 验证最后一个部分包含平台标识符（含点号如 x86_64）
    let last_part = parts.last()?;
    let is_valid_wheel = last_part.contains('.') || ...;

    Some((distribution.to_string(), version.to_string()))
}
```

#### Sdist 文件解析

```rust
fn parse_sdist_filename(filename: &str) -> Option<(String, String)> {
    let filename = filename.trim_end_matches(".tar.gz");

    // 找到第一个数字的位置——版本号从这里开始
    let version_start = filename.find(|c: char| c.is_ascii_digit())?;

    let distribution = &filename[..version_start];  // 版本号之前的部分
    let version = &filename[version_start..];        // 从数字开始到最后

    Some((distribution.to_string(), version.to_string()))
}
```

**测试覆盖**（从项目单元测试可见）：

```rust
#[test]
fn test_parse_wheel_filename_basic() {
    assert_eq!(
        parse_package_filename("lodash-4.17.21-py3-none-any.whl"),
        Some(("lodash".to_string(), "4.17.21".to_string()))
    );
}

#[test]
fn test_parse_sdist_filename_basic() {
    assert_eq!(
        parse_package_filename("lodash-4.17.21.tar.gz"),
        Some(("lodash".to_string(), "4.17.21".to_string()))
    );
}
```

---

## 八、完整请求时序图（以 `pip install requests==2.31.0` 为例）

```
时间轴 →

Client (pip)              Gateway/Mirror              PyPI Upstream
    │                         │                            │
    │  ① GET /simple/         │                            │
    │  ─────────────────────► │                            │
    │                         │  GET /simple/              │
    │                         │ ──────────────────────────►│
    │                         │  ←── 200 OK (HTML/JSON) ──│
    │  ←── 200 OK ─────────── │                            │
    │                         │                            │
    │  ② GET /simple/requestst│                            │
    │  ─────────────────────► │                            │
    │                         │  GET /simple/requests/     │
    │                         │ ──────────────────────────►│
    │                         │  ←── 200 OK ──────────────│
    │                         │                            │
    │  ③ [内部] GET /pypi/    │                            │
    │     requests/json       │  GET /pypi/requests/json   │
    │  (可选，用于依赖解析)    │ ──────────────────────────►│
    │                         │  ←── 200 OK (JSON) ────────│
    │  ←── 200 OK ─────────── │                            │
    │                         │                            │
    │  ④ 解析依赖树           │                            │
    │  (递归步骤②③用于每个依赖)                           │
    │                         │                            │
    │  ⑤ GET /packages/.../   │                            │
    │     requests-2.31.0.whl │  延迟检查                  │
    │  ─────────────────────► │  (检查 upload_time)        │
    │                         │  ├─ Allowed → 转发          │
    │                         │  ├─ Downgraded → 重定向     │
    │                         │  └─ Denied → 403           │
    │                         │                            │
    │  [如果 Allowed]          │  GET files.pythonhosted.org│
    │                         │ ──────────────────────────►│
    │                         │  ←── 200 OK (binary) ──────│
    │  ←── 200 OK (文件) ──── │                            │
    │                         │                            │
    │  ⑥ 本地 SHA256 校验     │                            │
    │  (与 Simple API 返回的   │                            │
    │   hashes 字段比对)       │                            │
    │                         │                            │
    │  ✓ 安装成功             │                            │
```

---

## 九、各环节网络请求设计的工程考量总结

### 9.1 为什么分离 Simple API 和 JSON API？

| 维度 | Simple API (/simple/) | JSON API (/pypi//json) |
|------|----------------------|------------------------|
| **用途** | 文件列表+下载链接 | 完整元数据+依赖信息 |
| **历史** | 从 PyPI 早期就存在 | 后来添加的补充接口 |
| **数据量** | 轻量（只有文件名和哈希） | 重量（包含所有版本的所有字段） |
| **更新频率** | 高（每次发布新文件） | 低（主要在发布新版本时） |
| **缓存策略** | 短 TTL 或 ETag | 长 TTL |

**设计意图**：Simple API 用于快速查找"有哪些文件可以下载"，JSON API 用于深入了解"这个包的详细信息"。这种分离使得：
- 大部分情况下只需要轻量的 Simple API 请求
- 只有在需要依赖解析时才调用重量级的 JSON API
- 可以独立优化两者的缓存策略

### 9.2 为什么使用 Content Negotiation？

从 [pypi.rs:330-356](delayMirror/src/workers/handlers/pypi.rs#L330-L356) 的实现可以看出：

```rust
let accept_header = _req.headers().get("Accept")...;
let is_json_api = accept_header.contains("application/vnd.pypi.simple.v1+json");
```

**优势**：
1. **URL 稳定性**：同一个 URL `/simple/package/` 可以同时服务新旧客户端
2. **渐进迁移**：服务器可以逐步引导客户端升级到新格式
3. **未来扩展**：可以添加新的 MIME 类型而不破坏现有客户端

### 9.3 为什么哈希值嵌入 URL 片段？（HTML 格式的遗留设计）

在 PEP 503 HTML 格式中：

```html
<a href="url#sha256=abc&md5=def">filename</a>
```

这是一个**历史遗留设计**：
- 最初 PyPI 只是一个简单的静态文件服务器
- 哈希值放在片段中不会影响 URL 路由
- 浏览器不会将片段发送到服务器，所以不影响缓存键

**PEP 691 的改进**：将哈希值移到独立的 `hashes` 字典中，更加规范。

### 9.4 为什么下载域名与 API 域名不同？

- **API 域名**：`pypi.org` —— 动态内容，需要应用服务器
- **文件域名**：`files.pythonhosted.org` —— 静态文件，适合 CDN 分发

**好处**：
1. 文件下载可以使用全球 CDN 加速
2. API 服务器不被大文件下载拖慢
3. 可以独立扩展和优化两者

### 9.5 延迟策略（Delay Policy）的安全意义

从项目实现的 [pypi.rs:443-538](delayMirror/src/workers/handlers/pypi.rs#L443-L538)：

**问题场景**：
- 攻击者发布恶意包 v1.0.0 到 PyPI
- 用户立即 `pip install package` 就会下载到恶意版本
- 安全研究员来不及审查新发布的包

**解决方案**：
- 设置延迟天数（如 7 天）
- 发布后 7 天内的版本无法直接下载
- 给安全审计和自动化扫描留出时间窗口

**三种模式的权衡**：

| 模式 | 可用性 | 安全性 | 适用环境 |
|------|--------|--------|---------|
| Allowed（透传） | ★★★★★ | ★★☆☆☆ | 开发/测试 |
| Downgraded（降级） | ★★★★☆ | ★★★★☆ | 预生产 |
| Denied（拒绝） | ★★☆☆☆ | ★★★★★ | 生产/高合规 |

---

## 十、参考资源

### 官方规范
- **PEP 503**: Simple Repository API (HTML 格式)
  - https://www.python.org/dev/peps/pep-0503/
- **PEP 691**: JSON-based Simple API
  - https://www.python.org/dev/peps/pep-0691/
- **PEP 458**: Secure Package Installs using TUF
  - https://www.python.org/dev/peps/pep-0458/

### pip 源码
- **依赖解析器**: https://github.com/pypa/pip/blob/main/src/pip/_internal/resolution/
- **下载模块**: https://github.com/pypa/pip/blob/main/src/pip/_internal/network/download.py
- **架构文档**: https://pradyunsg-pip.readthedocs.io/en/latest/development/architecture/overview/

### 项目代码
- **延迟镜像 PyPI 处理器**: [delayMirror/src/workers/handlers/pypi.rs](delayMirror/src/workers/handlers/pypi.rs)
- **网关层 PyPI 处理器**: [gateway/src/handlers/pypi.rs](gateway/src/handlers/pypi.rs)
- **元数据处理**: [gateway/src/handlers/metadata.rs](gateway/src/handlers/metadata.rs)

---

*文档生成时间：2026-04-07*
*基于 PEP 503/691 规范、pip 官方文档及 npmGateway 项目源码分析*
