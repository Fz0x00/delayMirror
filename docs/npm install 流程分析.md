# npm 包下载完整网络请求流程深度分析

## 概述

本文档基于 npm 官方源码（pacote、@npmcli/arborist、npm-registry-fetch、cacache）、官方 Registry API 文档以及 delayMirror 项目的实现，详细分析 `npm install` 过程中发起的所有网络请求的完整流程、参数、响应处理机制及设计意图。

---

## 一、整体架构与核心组件

### 1.1 分层架构图

```
┌─────────────────────────────────────────────────────────────────┐
│                      npm CLI (用户入口)                          │
│                     npm install <package>                        │
└───────────────────────────┬─────────────────────────────────────┘
                            │
┌───────────────────────────▼─────────────────────────────────────┐
│               @npmcli/arborist (依赖树管理器)                     │
│    ┌──────────────┐  ┌──────────────┐  ┌──────────────────────┐  │
│    │buildIdealTree│→ │ loadVirtual  │→ │ reify (物化到磁盘)    │  │
│    │ (构建理想树)  │  │ (读取锁文件)  │  │ (写入 node_modules)  │  │
│    └──────┬───────┘  └──────┬───────┘  └──────────┬───────────┘  │
└───────────┼─────────────────┼─────────────────────┼──────────────┘
            │                 │                     │
┌───────────▼─────────────────▼─────────────────────▼──────────────┐
│                    pacote (包获取引擎)                             │
│  ┌────────────┐  ┌────────────┐  ┌──────────┐  ┌─────────────┐  │
│  │ packument  │  │ manifest   │  │ resolve  │  │  tarball    │  │
│  │(包元数据)   │  │(单版本信息) │  │(解析地址) │  │ (下载tar包) │  │
│  └─────┬──────┘  └─────┬──────┘  └────┬─────┘  └──────┬──────┘  │
└────────┼────────────────┼─────────────┼────────────────┼─────────┘
         │                │             │                │
┌────────▼────────────────▼─────────────▼────────────────▼─────────┐
│              npm-registry-fetch (HTTP 通信层)                      │
│         统一处理: 认证、重试、限流、Accept 头                        │
└──────────────────────────────┬──────────────────────────────────┘
                               │
┌──────────────────────────────▼──────────────────────────────────┐
│              npm Registry (registry.npmjs.org)                    │
│     ┌────────────┐  ┌──────────────────┐  ┌─────────────────┐    │
│     │ GET /:pkg  │  │ GET /:pkg/-/ver  │  │ CDN tarball     │    │
│     │ (元数据API) │  │ .tgz (文件下载)   │  │ 分发            │    │
│     └────────────┘  └──────────────────┘  └─────────────────┘    │
└──────────────────────────────────────────────────────────────────┘
                               │
┌──────────────────────────────▼──────────────────────────────────┐
│                 cacache (内容可寻址缓存)                           │
│        ~/.npm/_cacache/ (基于 SHA-512 内容寻址)                   │
│        插入/读取时自动完整性校验 → 自愈能力                         │
└──────────────────────────────────────────────────────────────────┘
```

### 1.2 核心模块职责

| 模块 | 职责 | GitHub |
|------|------|--------|
| **arborist** | 依赖树的构建、冲突解决、扁平化（dedupe）、锁文件管理 | [npm/cli](https://github.com/npm/cli/tree/latest/workspaces/arborist) |
| **pacote** | 包获取的统一抽象层，支持 registry/git/dir/tarball/remote 等多种来源 | [npm/pacote](https://github.com/npm/pacote) |
| **npm-registry-fetch** | 封装 HTTP 请求，处理认证、headers、代理、重试逻辑 | [npm/npm-registry-fetch](https://github.com/npm/npm-registry-fetch) |
| **cacache** | 高性能内容可寻址缓存，支持多哈希算法、并发安全、自动校验 | [npm/cacache](https://github.com/npm/cacache) |

---

## 二、阶段一：包元数据获取（Packument Fetch）

### 2.1 网络请求详情

#### 请求 #1：获取完整包元数据（Packument）

```
GET https://registry.npmjs.org/:package
```

**关键请求头：**
```http
Accept: application/vnd.npm.install-v1+json; q=1.0, application/json; q=0.8, */*
```

**URL 编码规则：**
- 普通包：`GET /lodash`
- 作用域包（scoped）：`GET /@elastic%2Feui`（`/` 必须编码为 `%2F`）

#### 响应结构（Abbreviated 格式 - install-v1）

Registry 返回两种格式，通过 `Content-Type` 区分：

| Content-Type | 格式 | 大小对比（以 npm 包为例） |
|-------------|------|-------------------------|
| `application/vnd.npm.install-v1+json` | 精简版（abbreviated） | ~21KB (压缩后) |
| `application/json` | 完整版（full） | ~410KB (压缩后) |

**精简版响应示例：**
```json
{
  "dist-tags": {
    "latest": "1.0.0",
    "next": "2.0.0-beta.1"
  },
  "modified": "2024-01-15T10:30:00.000Z",
  "name": "tiny-tarball",
  "versions": {
    "1.0.0": {
      "_hasShrinkwrap": false,
      "directories": {},
      "dist": {
        "shasum": "bbf102d5ae73afe2c553295e0fb02230216f65b1",
        "integrity": "sha512-xxxx...==",  // SRI 格式 (SHA-512)
        "tarball": "https://registry.npmjs.org/tiny-tarball/-/tiny-tarball-1.0.0.tgz"
      },
      "name": "tiny-tarball",
      "version": "1.0.0",
      "dependencies": {           // 仅在 abbreviated 中保留的关键字段
        "foo": "^1.0.0"
      },
      "peerDependencies": {},
      "optionalDependencies": {},
      "engines": {
        "node": ">=14.0.0"
      }
    }
  }
}
```

**完整版额外包含的字段（被 abbreviated 过滤掉）：**
- `_id`, `_rev`, `_attachments` — CouchDB 内部元数据
- `_shasum`, `_from`, `_npmVersion`, `_nodeVersion`, `_npmUser` — 发布时注入
- `author`, `maintainers`, `readme`, `readmeFilename` — 人可读信息
- `repository`, `bugs`, `homepage`, `license`, `keywords`, `description` — 项目元数据
- `time` — 各版本发布时间戳（但在 abbreviated 中 `modified` 字段仍保留）
- `scripts`, `devDependencies`, `bin`, `main` 等非安装必需字段

### 2.2 pacote 内部调用链

```
pacote.packument(spec, opts)
  ├── 解析 spec → 确定 package name 和 version range/tag
  ├── npm-registry-fetch.json(registry_url + '/' + package_name, opts)
  │     ├── 设置 Accept: application/vnd.npm.install-v1+json; ...
  │     ├── 处理认证 (Bearer token / Basic auth)
  │     ├── 发起 HTTP GET 请求
  │     └── 返回 JSON 响应
  └── 返回 packument 对象
```

### 2.3 设计意图分析

#### 为什么需要 Abbreviated Metadata？

1. **带宽优化**：大型包（如 lodash、react）的 full metadata 可超过 10MB，而 abbreviated 通常 < 50KB，减少 **95%+** 的传输量
2. **解析速度**：更小的 JSON 意味着更快的 V8 解析和更少的 GC 压力
3. **按需获取**：install 流程只需要 `dependencies`、`dist.tarball`、`dist.integrity` 等少量字段
4. **向后兼容**：使用 HTTP content negotiation（Accept header），服务端和客户端均可优雅降级

#### Accept Header 的 q-value 设计

```
Accept: application/vnd.npm.install-v1+json; q=1.0, application/json; q=0.8, */*
```

- `q=1.0`：最优先选择 abbreviated 格式
- `q=0.8`：降级到完整 JSON
- `*/*`：兜底，确保任何响应都可接受
- 这遵循 RFC 7231 的内容协商规范

---

## 三、阶段二：版本解析与 Manifest 获取

### 3.1 版本解析流程

当用户执行 `npm install express@^4.18.0` 时：

```
输入: express@^4.18.0
       │
       ▼
┌──────────────────────────────────────────┐
│ 1. 从 packument.versions 中提取所有版本号  │
│    ["4.0.0", "4.17.1", "4.18.0",        │
│     "4.18.1", "4.18.2", "4.19.0", ...]  │
└──────────────────┬───────────────────────┘
                   │
                   ▼
┌──────────────────────────────────────────┐
│ 2. 使用 semver 模块进行范围匹配            │
│    semver.maxSatisfying(versions, "^4.18")│
│    → 匹配结果: "4.18.2" (假设最新满足的)    │
└──────────────────┬───────────────────────┘
                   │
                   ▼
┌──────────────────────────────────────────┐
│ 3. 如果指定了 tag（如 @latest、@next）：    │
│    查找 packument["dist-tags"][tag]       │
│    → 返回 tag 指向的具体版本号             │
└──────────────────────────────────────────┘
```

### 3.2 可选的网络请求：获取单版本 Manifest

在某些场景下（如需要完整的 package.json 信息），pacote 会发起第二个请求：

```
GET https://registry.npmjs.org/:package/:version
```

**注意**：此端点已被标记为 **deprecated**，现代 npm 更倾向于直接从 packument 的 `versions` 字段中提取所需信息，避免额外的网络往返。

### 3.3 Arborist 的依赖树构建

这是 npm v7+ 最核心的改进之一。Arborist 将依赖关系建模为**边（Edge）优先的图**，而非简单的嵌套树：

```javascript
// Arborist 构建理想树的简化流程
const arb = new Arborist({ path: '/path/to/project' })

// 步骤 1: 读取 package.json + lockfile
await arb.loadVirtual()

// 步骤 2: 构建理想依赖树
// 内部过程：
//   a. 从顶层 dependencies 开始
//   b. 对每个依赖调用 pacote.packument() 获取元数据
//   c. semver 解析确定具体版本
//   d. 递归处理该版本的 dependencies
//   e. 解决版本冲突（ERESOLVE 错误或自动 dedupe）
//   f. 应用 peerDependencies 规则
await arb.buildIdealTree()

// 步骤 3: 物化到磁盘（实际下载和安装）
await arb.reify()
```

**Edge 作为一等公民的设计优势：**

| 传统树模型问题 | Arborist Edge 模型解决方案 |
|--------------|--------------------------|
| 依赖满足判断逻辑分散在各处 | Edge.to / Edge.error 统一封装 |
| peerDependencies 位置规则复杂 | Edge.type 区分 dep/peer/optional |
| 难以追踪依赖来源 | Edge.from 明确记录消费者 |
| 冲突检测困难 | 图算法可全局检测循环和冲突 |

### 3.4 设计意图分析

#### 为什么从嵌套树改为图模型？

1. **正确的模块解析语义**：Node.js 的 `require()` 基于 realpath 解析， symlink 和实际路径的差异导致旧模型的 bug（[历史 bug 参考](https://blog.npmjs.org/post/618653678433435649/npm-v7-series-arborist-deep-dive)）
2. **peerDependencies 正确性**：npm v7 要求 peerDependencies 必须显式安装（或声明为 optional），这需要在图的同一层级进行约束检查
3. **确定性锁文件**：package-lock.json v2+ 需要精确记录每个包的位置（`node_modules/` 中的路径），图模型能更好地表达这种关系
4. **性能**：一次遍历即可完成整个依赖图的构建，避免多次递归

---

## 四、阶段三：Tarball 下载与完整性验证

### 4.1 网络请求详情

#### 请求 #N：下载 Tarball 文件

```
GET https://registry.npmjs.org/:package/-/:package-:version.tgz
```

**示例 URL：**
```
GET https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz
GET https://registry.npmjs.org/@types/node/-/types-node-20.0.0.tgz
```

**请求头：**
```http
Accept: application/octet-stream, */*
```

**响应头：**
```http
HTTP/1.1 200 OK
Content-Type: application/octet-stream
Content-Length: 12345678
ETag: "W/"abc123""
Last-Modified: Mon, 01 Jan 2024 00:00:00 GMT
Cache-Control: max-age=31536000, immutable
```

**响应体：** gzip 压缩的 tar 归档文件（`.tgz`），内部结构：
```
package/
├── package.json          (必须)
├── index.js              (入口文件)
├── lib/                  (源码)
├── README.md
└── ...
```

### 4.2 完整性验证机制（关键安全设计）

npm 使用 **双重哈希校验** 来确保包的完整性和真实性：

#### 4.2.1 dist 对象中的校验值

```json
"dist": {
  "tarball": "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz",
  "shasum": "705a34bd02dbb640bf81e7b0e93b0c9ff1a95fe1",     // SHA-1 (遗留)
  "integrity": "sha512-...base64encodedhash...=="                  // SHA-512 SRI (推荐)
}
```

#### 4.2.2 Subresource Integrity (SRI) 格式

```
integrity = "<algorithm>-<base64-hash>"
```

示例：
```
sha512-BMjzQmNDDEo/G+H/dYcqH0YMADtR2lLrY/nqMZSMfLdAqOyKHDRiRiFfkSVWHXWU0DgJxVPkNbE3RVgVQ1Fzw==
```

支持的算法：`sha512`, `sha384`, `sha256`, `sha1`（已弃用）

#### 4.2.3 验证流程

```
下载 tarball 数据流
       │
       ▼
┌──────────────────────────────────────┐
│ cacache.put(cachePath, key, data, {  │
│   integrity: expected_integrity       │
│ })                                   │
│                                      │
│ 内部执行:                             │
│ 1. 计算 data 的 hash (algorithm)      │
│ 2. 与 expected_integrity 比较         │
│ 3. 不匹配 → 抛出 EINTEGRITY 错误     │
│ 4. 匹配 → 写入缓存，返回 hash         │
└──────────────────────────────────────┘
       │
       ▼
┌──────────────────────────────────────┐
│ 后续读取时 (cacache.get):             │
│ 1. 从索引查找 content address        │
│ 2. 重新计算 hash 校验                 │
│ 3. 校验失败 → 自动删除损坏条目        │
│ 4. 返回数据 或 抛出错误               │
└──────────────────────────────────────┘
```

### 4.3 缓存架构（cacache）

#### 4.3.1 目录结构

```
~/.npm/_cacache/
├── content-v2/           # 内容寻址存储 (按 hash 存放实际文件)
│   ├── sha512-xx/       # hash 前两位作为子目录
│   │   └── abc123...    # 文件名 = 完整 hash
│   └── sha1-xx/
├── index-v5/            # 索引 (key → content address 映射)
│   ├── key-hash/        # key 的 hash 前缀
│   │   └── index-file   # 索引数据
└── tmp/                 # 临时文件 (原子写入用)
```

#### 4.3.2 缓存 Key 生成规则

对于 registry 包，key 格式为：
```
make-fetch-happen:request-cache:cacheable:GET://registry.npmjs.org/package-name
```

或者基于 `integrity` 的内容寻址查找：
```
cacache.get.byDigest(cachePath, 'sha512-xxxx==')
```

#### 4.3.3 缓存策略决策树

```
用户执行 npm install
       │
       ├─ 有 --offline 标志？
       │    └─ YES → 仅从缓存读取，失败则报错
       │
       ├─ 有 --prefer-offline 标志？
       │    └─ YES → 优先缓存，缓存未命中再联网
       │
       └─ 默认行为:
            ├── 检查 cacache 是否有有效条目
            │   ├── 命中且 integrity 匹配 → 直接使用 ✓
            │   └── 未命中或损坏 → 发起网络请求
            │
            ├── 下载完成后:
            │   ├── 写入 cacache (带 integrity 校验)
            │   └── 硬链接到 node_modules (节省空间)
            │
            └── 并发控制: maxsockets=16 (默认最大并行下载数)
```

### 4.4 pacote.extract() 的解压过程

```
pacote.extract('lodash@4.17.21', './node_modules/lodash', opts)
  │
  ├── 1. resolve(): 解析 spec → 得到 tarball URL + integrity
  │
  ├── 2. 检查缓存 (cacache.get.byDigest):
  │   ├── 命中 → 跳过下载
  │   └── 未命中 → 继续
  │
  ├── 3. tarball.stream(): 下载 tarball
  │   ├── npm-registry-fetch 发起 GET 请求
  │   ├── 流式接收数据 (不全部加载到内存)
  │   └── 同时写入 cacache (带完整性校验)
  │
  ├── 4. 解压 tar 流:
  │   ├── 使用 minipass-pipeline 管道
  │   ├── strip-components=1 (去掉外层 package/ 目录)
  │   ├── 设置文件权限 (fmode: 0o666, dmode: 0o777 & ~umask)
  │   └── 写入目标目录
  │
  └── 5. 返回 { from, resolved, integrity }
```

### 4.5 设计意图分析

#### 为什么使用内容可寻址存储（CAS）？

1. **自动去重**：相同内容的包只存一份（即使被不同项目引用）
2. **自愈能力**：读取时自动校验，损坏的数据会被清理并重新获取
3. **并发安全**：无锁设计，使用原子写入 + Bloom filter 优化
4. **不可变性**：一旦写入，内容不会被修改（immutable by default）
5. **GC 友好**：可通过引用计数进行垃圾回收

#### 为什么同时保留 shasum 和 integrity？

- **向后兼容**：2017 年 4 月前发布的包只有 shasum（SHA-1）
- **安全性升级**：SHA-1 已被证明存在碰撞攻击风险，SHA-512 是当前推荐
- **渐进迁移**：新包必须有 integrity，旧包逐步淘汰 shasum

#### 为什么 tarball 使用 immutable Cache-Control？

```
Cache-Control: max-age=31536000, immutable
```

- tarball URL 中包含精确版本号，内容永远不会改变
- CDN 可以无限期缓存，大幅降低 registry 负载
- 配合 integrity 校验，即使 CDN 被污染也能检测到

---

## 五、阶段四：递归依赖处理（完整网络交互序列）

### 5.1 典型安装过程的完整请求序列

以安装 `express@^4.18.0` 为例（简化版）：

```
时间线 →

[请求 1] GET /express                              ← Packument (abbreviated)
         ↓ 响应: versions, dist-tags, 各版本的 dist.tarball + dependencies
         ↓ 解析: ^4.18.0 → 4.18.2

[请求 2] GET /accepts/-/accepts-1.3.8.tgz           ← 下载 direct dependency
[请求 3] GET /array-flatten/-/array-flatten-1.1.1.tgz
[请求 4] GET /body-parser/-/body-parser-1.20.2.tgz
[请求 5] GET /content-disposition/-/content-disposition-0.5.4.tgz
[请求 6] GET /content-type/-/content-type-1.0.5.tgz
[请求 7] GET /cookie/-/cookie-0.5.0.tgz
[请求 8] GET /cookie-signature/-/cookie-signature-1.0.6.tgz
[请求 9] GET /debug/-/debug-4.3.4.tgz
         ↓ debug 的 dependencies:
         └── ms@2.1.2

[请求 10] GET /ms/-/ms-2.1.2.tgz                     ← 间接依赖
...
[请求 N] GET /vary/-/vary-1.1.2.tgz

总计: ~30 个直接/间接依赖 → ~30 个 tarball 下载请求
+ 若干个 packument 请求 (用于解析依赖版本)
```

### 5.2 并发控制

npm 默认使用 `fetch-retry` 和 `make-fetch-happen` 进行并发管理：

| 参数 | 默认值 | 说明 |
|------|-------|------|
| `maxSockets` | 16 | 同一主机最大并发连接数 |
| `fetchRetries` | 2 | 失败重试次数 |
| `fetchRetryFactor` | 2 | 重试延迟倍数 (指数退避) |
| `fetchRetryMsec` | 1000 | 初始重试延迟 (ms) |

### 5.3 网络请求优化策略

#### 策略 1：packument 缓存

```
第一次: GET /express → 200, 存入 cacache (TTL: 由 ETag/Last-Modified 控制)
后续: If-None-Match: "xxx" → 304 Not Modified (零带宽)
```

#### 策略 2：批量依赖解析

Arborist 在 `buildIdealTree()` 阶段会：
1. 先收集所有需要的包名
2. 批量获取 packuments（利用已有的缓存）
3. 一次性完成整个图的版本解析
4. 最后才进入 reify 阶段开始下载

#### 策略 3：锁文件复用

当 `package-lock.json` 存在且有效时：
- **跳过所有 packument 请求**（锁文件已包含 resolved URL + integrity）
- **仅下载缺失或变更的 tarballs**
- 这就是为什么 `npm ci` 比 `npm install` 快得多

---

## 六、delayMirror 项目的网关视角

基于对 [delayMirror/src/workers/handlers/npm.rs](file:///Users/fz/Documents/npmGateway/delayMirror/src/workers/handlers/npm.rs) 的分析，该项目作为 npm registry 的中间代理，实现了以下网络请求拦截和处理：

### 6.1 三个关键路由及其对应的网络操作

| 路由 | 函数 | 上游请求 | 特殊处理 |
|------|------|---------|---------|
| `GET /:package` | `handle_npm_metadata()` | `GET {registry}/{package}` | 版本过滤、tarball URL 重写 |
| `GET /:package/:version` | `handle_npm_version()` | `GET {registry}/{package}` | 版本延迟检查、302 重定向 |
| `GET /dl/{pkg}@{ver}` | `handle_npm_download()` | `GET {download_registry}/{pkg}/-/...tgz` | 代理转发、版本降级 |

### 6.2 元数据拦截流程（handle_npm_metadata）

```rust
async fn handle_npm_metadata(req, config, checker) {
    // 1. 向上游 registry 发起 packument 请求
    let metadata = fetch_package_metadata(package, &config.npm_registry).await;
    //    ↑ 内部: GET https://registry.npmjs.org/{package}
    //    ↑ Accept: (透传或默认)

    // 2. 基于时间戳过滤版本 (filter_versions_by_delay)
    //    - 解析 metadata.time 字段
    //    - 移除发布时间 < delay_days 的版本
    //    - 更新 dist-tags.latest 指向最高允许版本

    // 3. 重写 tarball URL (rewrite_tarball_urls)
    //    原始: https://registry.npmjs.org/pkg/-/pkg-1.0.0.tgz
    //    重写: http://gateway:8787/dl/pkg@1.0.0
    //    目的: 让客户端的 tarball 请求也经过网关

    // 4. 返回修改后的 metadata
}
```

### 6.3 Tarball 下载代理流程（handle_npm_download）

```rust
async fn handle_npm_download(req, config, checker) {
    // 1. 解析 /dl/{package}@{version} 格式

    // 2. 获取包元数据 (用于时间检查)
    let metadata = fetch_package_metadata(package, &config.npm_registry).await;

    // 3. 版本延迟检查 (checker.resolve_version)
    match checker.resolve_version(version, &time_info) {
        VersionCheckResult::Allowed => {
            // 4a. 直接代理到上游 tarball URL
            proxy_upstream(&upstream_url).await
            // GET {download_registry}/{package}/-/{filename}-{version}.tgz
        }
        VersionCheckResult::Downgraded { suggested_version } => {
            // 4b. 自动降级到允许的旧版本
            proxy_upstream(&old_version_url).await
            // 添加 X-Delay-* 响应头告知客户端
        }
        VersionCheckResult::Denied => {
            // 4c. 返回 403 Forbidden
        }
    }
}
```

### 6.4 与标准 npm 流程的对比

```
标准 npm 流程:                    delayMirror 代理流程:

Client                            Client
  │                                 │
  ├── GET /express                  ├── GET /express
  │   (to registry)       │         │   (to gateway)
  │   ← metadata                       │   ← filtered metadata
  │                                    │      (tarball URLs rewritten)
  │                                    │
  ├── GET /express/-/exp-4.18.tgz     ├── GET /dl/express@4.18.2
  │   (to registry)       │         │   (to gateway)
  │   ← .tgz file                      │   ← .tgz file (or 403/redirected)
  │                                    │         │
  │                                    │         ├── if allowed:
  │                                    │         │   proxy to upstream
  │                                    │         │     GET registry/../-.tgz
  │                                    │         │
  │                                    │         ├── if too new:
  │                                    │         │   auto-downgrade or 403
```

---

## 七、各阶段设计意图总结

### 7.1 工程考量总表

| 阶段 | 核心挑战 | 解决方案 | 工程价值 |
|------|---------|---------|---------|
| **元数据获取** | 大包元数据 >10MB | Abbreviated format + Accept negotiation | 节省 95%+ 带宽，加速解析 |
| **版本解析** | SemVer 范围匹配复杂 | node-semver 库 + dist-tags | 确定性的版本选择算法 |
| **依赖树构建** | 幻影依赖、钻石依赖、peer deps | Arborist 图模型 + Edge 抽象 | 正确的 node_modules 结构 |
| **Tarball 下载** | 网络不稳定、CDN 一致性 | Streaming + Retry + Integrity check | 安全、可靠的内容分发 |
| **缓存管理** | 磁盘空间、并发读写、数据损坏 | CAS + Auto-verification + Lockless | 高性能、自愈、去重 |
| **安全验证** | 供应链攻击、包篡改 | SRI (SHA-512) + 双重校验 | 防止中间人攻击和恶意替换 |

### 7.2 关键技术决策的理由

#### 1. 为什么用 JSON API 而非 GraphQL/gRPC？
- ** simplicity**: RESTful JSON 是 Web 生态的标准协议
- **cacheability**: HTTP 层面的缓存控制（ETag, Cache-Control）天然可用
- **debugability**: curl 一行命令即可调试
- **evolution**: 通过 Accept header 实现格式演进，无需破坏性变更

#### 2. 为什么 tarball 用 `/pkg/-/pkg-ver.tgz` 这种 URL 格式？
- `-` 是虚拟目录分隔符，避免与 scope 包的 `/` 冲突
- URL 中编码了包名和版本，具有自描述性
- CDN 友好：每个版本有唯一 URL，可以永久缓存

#### 3. 为什么 npm v7 重写 Arborist 而不是继续修补旧代码？
- 旧代码（read-package-tree）的核心数据模型无法正确表达 Node.js 的模块解析语义
- 10 年的技术债务积累使得 patch 成本超过 rewrite
- 新模型使得 peerDependencies 强制安装等长期需求得以实现

#### 4. 为什么缓存使用单独的库（cacache）而不是简单的文件系统？
- **原子写入**: 先写 tmp 再 rename，防止读到半写完的文件
- **Bloom filter**: 快速判断 key 是否存在，减少磁盘 I/O
- **内容去重**: 相同 hash 的内容只存一份（跨包共享）
- **自动 GC**: 引用计数 + compact 操作清理无用数据

---

## 八、参考资源

### 官方文档
- [npm Registry API - Package Metadata](https://github.com/npm/registry/blob/main/docs/responses/package-metadata.md)
- [npm Install Documentation](https://docs.npmjs.com/cli/v11/commands/npm-install/)
- [npm Cache Documentation](https://docs.npmjs.com/cli/v10/commands/npm-cache/)
- [Arborist Deep Dive (npm Blog)](https://blog.npmjs.org/post/618653678433435649/npm-v7-series-arborist-deep-dive)

### 核心源代码仓库
- [pacote - npm fetcher](https://github.com/npm/pacote)
- [@npmcli/arborist - dependency tree manager](https://github.com/npm/cli/tree/latest/workspaces/arborist)
- [npm-registry-fetch](https://github.com/npm/npm-registry-fetch)
- [cacache - content-addressable cache](https://github.com/npm/cacache)

### 本项目相关
- [delayMirror npm handler implementation](file:///Users/fz/Documents/npmGateway/delayMirror/src/workers/handlers/npm.rs) - 网关代理实现的参考实现
