# Changelog

## [0.1.2] - 2026-04-05

### Added
- Go Modules 完整测试项目 (`test/gomod/`)
  - 自动化测试脚本 `run_test.sh`
  - 测试应用 `main.go` (gin, zap, uuid, chi)

### Fixed
- **Go Modules URL 拼接错误**
  - 问题：registry/download URL 与模块路径之间缺少 `/`
  - 影响：导致 DNS 解析失败 (`goproxy.cngithub.com`)
  - 修复：使用 `trim_end_matches('/')` 确保路径分隔符正确
- **Go Modules 镜像源选择**
  - 问题：阿里云 CDN 拦截请求 (UA ACL blacklist)
  - 解决：改用 goproxy.cn（七牛云）作为 Go Modules 统一镜像源
- **User-Agent 更新为通用格式**
  - 从 `pip/23.0` 改为 `Go-http-client/2.0`

### Changed
- 推荐使用 goproxy.cn 作为 Go Modules 统一镜像：
  - `GOMOD_REGISTRY=https://goproxy.cn`
  - `GOMOD_DOWNLOAD_REGISTRY=https://goproxy.cn`

### Test Results
- ✅ go mod tidy 成功 (20+ 依赖包)
- ✅ go mod download 成功 (所有请求 < 0.07s)
- ✅ go build 成功 (0.58s)
- ✅ 单个请求响应时间: **0.065-0.070s** ⚡

---

## [0.1.1] - 2026-04-05

### Added
- PyPI 镜像源完整测试项目 (`test/pip/`)
  - 自动化测试脚本 `run_test.sh`
  - 安装验证脚本 `verify_install.py`
  - 功能测试应用 `test_app.py`

### Fixed
- **PyPI 文件下载 URL 路径错误**
  - 问题：下载文件时使用纯文件名而非完整 hash 前缀路径
  - 影响：导致回退到官方 PyPI 源（速度慢 28.7x）
  - 修复：统一使用完整路径构建镜像 URL
  - 性能提升：从 4.3 kB/s 提升到最高 30 MB/s

### Changed
- 移除 `/simple/<package>/` 端点中不必要的 JSON API 查询
  - Simple Index 请求不再阻塞等待 JSON API 响应
  - 延迟检查仅在下载时进行
- 推荐使用清华镜像作为统一源：
  - `PYPI_SIMPLE_INDEX=https://pypi.tuna.tsinghua.edu.cn/simple`
  - `PYPI_JSON_API_BASE=https://pypi.tuna.tsinghua.edu.cn/pypi`

### Test Results
- ✅ 14/14 个依赖包成功安装
- ✅ 4/4 核心包版本验证通过
- ✅ Simple Index 响应 < 0.05s
- ✅ JSON API 响应 < 0.05s
- ✅ 完整 pip install 时间：62s (14个包)
