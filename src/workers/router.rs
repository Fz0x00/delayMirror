use serde_json::json;
use worker::{Headers, Method, Request, Response};

use crate::core::delay_logger::DelayLogger;
use crate::core::Config;
use crate::core::DelayChecker;
use crate::core::MetadataCache;

type HandlerResult = worker::Result<Response>;

fn add_cors_headers(resp: HandlerResult) -> HandlerResult {
    resp.map(|r| {
        let mut headers = Headers::new();
        headers.set("Access-Control-Allow-Origin", "*").ok();
        r.with_headers(headers)
    })
}

fn make_json_response(status: u16, body: serde_json::Value) -> HandlerResult {
    let mut headers = Headers::new();
    headers.set("Content-Type", "application/json")?;
    headers.set("Access-Control-Allow-Origin", "*")?;
    Ok(Response::from_json(&body)?
        .with_status(status)
        .with_headers(headers))
}

fn api_index() -> HandlerResult {
    let version = env!("CARGO_PKG_VERSION");
    let git_sha = env!("GIT_SHA");
    let git_dirty = env!("GIT_DIRTY") == "true";
    let git_branch = env!("GIT_BRANCH");
    let build_time = env!("BUILD_TIME");

    let full_version = if git_dirty {
        format!("{}-dev.{}+{}", version, git_sha, git_branch)
    } else {
        format!("{}-{}+{}", version, git_sha, git_branch)
    };

    make_json_response(
        200,
        json!({
            "service": "delay-mirror",
            "version": {
                "semver": version,
                "full": full_version,
                "git": {
                    "sha": git_sha,
                    "branch": git_branch,
                    "dirty": git_dirty,
                },
                "build_time": build_time,
            },
            "description": "Delay-based security mirror for NPM / Go Modules / PyPI",
            "endpoints": {
                "health": { "method": "GET", "path": "/health", "description": "Health check and configuration status" },
                "npm": {
                    "metadata": { "method": "GET", "path": "/npm/{package}", "description": "Get filtered package metadata" },
                    "version": { "method": "GET", "path": "/npm/{package}/{version}", "description": "Check version availability" },
                    "download": { "method": "GET", "path": "/dl/{package}@{version}", "description": "Download package tarball" }
                },
                "gomod": {
                    "version_list": { "method": "GET", "path": "/gomod/{module}/@v/list", "description": "List available versions" },
                    "version_info": { "method": "GET", "path": "/gomod/{module}/@v/{version}.info", "description": "Get version info with delay check" },
                    "go_mod": { "method": "GET", "path": "/gomod/{module}/@v/{version}.mod", "description": "Get go.mod file" },
                    "download": { "method": "GET", "path": "/gomod/{module}/@v/{version}.zip", "description": "Download module zip" }
                },
                "pypi": {
                    "simple_index": { "method": "GET", "path": "/pypi/simple/", "description": "PyPI simple index" },
                    "package_list": { "method": "GET", "path": "/pypi/simple/{package}/", "description": "List package files" },
                    "download": { "method": "GET", "path": "/pypi/packages/{filename}", "description": "Download package file" }
                }
            }
        }),
    )
}

pub async fn route_request(req: Request, config: &Config, checker: &DelayChecker) -> HandlerResult {
    // 创建缓存实例
    let cache = MetadataCache::new("metadata_cache.db").ok();
    
    if req.method() != Method::Get && req.method() != Method::Head {
        return make_json_response(
            405,
            json!({ "error": "Method Not Allowed", "allowed_methods": ["GET", "HEAD"] }),
        );
    }

    let path = req.path();
    let path = path.trim_matches('/');

    if path.is_empty() {
        return api_index();
    }

    if path == "health" {
        let version = env!("CARGO_PKG_VERSION");
        let git_sha = env!("GIT_SHA");
        let git_dirty = env!("GIT_DIRTY") == "true";
        let git_branch = env!("GIT_BRANCH");
        let build_time = env!("BUILD_TIME");

        let full_version = if git_dirty {
            format!("{}-dev.{}+{}", version, git_sha, git_branch)
        } else {
            format!("{}-{}+{}", version, git_sha, git_branch)
        };

        return make_json_response(
            200,
            json!({
                "status": "ok",
                "service": "delay-mirror",
                "version": {
                    "semver": version,
                    "full": full_version,
                    "git": {
                        "sha": git_sha,
                        "branch": git_branch,
                        "dirty": git_dirty,
                    },
                    "build_time": build_time,
                },
                "config": {
                    "delay_days": config.delay_days,
                    "npm_registry": config.npm_registry,
                    "npm_download_registry": config.npm_download_registry,
                    "gomod_registry": config.gomod_registry,
                    "gomod_download_registry": config.gomod_download_registry,
                    "pypi_registry": config.pypi_registry,
                    "pypi_json_api_base": config.pypi_json_api_base,
                    "pypi_download_base": config.pypi_download_base,
                    "allowlist_enabled": config.allowlist_enabled,
                    "debug_mode": config.debug_mode,
                }
            }),
        );
    }

    if config.debug_mode && path == "debug/fetch" {
        let url = "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz";
        match worker::Fetch::Request(Request::new(url, Method::Get)?)
            .send()
            .await
        {
            Ok(mut resp) => {
                let status = resp.status_code();
                let headers: std::collections::HashMap<String, String> =
                    resp.headers().entries().collect();
                let body = resp.text().await.unwrap_or_default();
                return make_json_response(
                    200,
                    json!({
                        "url": url,
                        "status": status,
                        "headers": headers,
                        "body_len": body.len(),
                        "body_preview": &body[..body.len().min(200)],
                    }),
                );
            }
            Err(e) => {
                return make_json_response(
                    500,
                    json!({
                        "error": format!("Fetch failed: {:?}", e),
                    }),
                );
            }
        }
    }

    if config.debug_mode && path == "debug/routes" {
        let test_path = "dl/lodash@4.17.23";
        let parts: Vec<&str> = test_path.split('/').collect();
        let first = parts.first().copied().unwrap_or("");
        return make_json_response(
            200,
            json!({
                "test_path": test_path,
                "parts": parts,
                "parts_0": first,
                "parts_len": parts.len(),
                "matches_npm_or_dl": first == "npm" || first == "dl",
                "dispatch_npm_condition_download": first == "dl" && parts.len() >= 2,
            }),
        );
    }

    if config.debug_mode && path == "debug/pypi" {
        let pypi_simple_url = format!("{}/", config.pypi_registry.trim_end_matches('/'));
        let pypi_pkg_simple_url =
            format!("{}/requests/", config.pypi_registry.trim_end_matches('/'));
        let pypi_json_url = format!("{}/requests/json", config.pypi_json_api_base);

        let mut results = json!({});

        let simple_req = Request::new(&pypi_simple_url, Method::Get);
        if let Ok(sr) = simple_req {
            match worker::Fetch::Request(sr).send().await {
                Ok(mut resp) => {
                    let blen = resp.text().await.map(|b| b.len()).unwrap_or(0);
                    results["pypi_simple_root"] = json!({"url": pypi_simple_url, "status": resp.status_code(), "body_len": blen});
                }
                Err(e) => {
                    results["pypi_simple_root"] = json!({"error": e.to_string()});
                }
            }
        } else {
            results["pypi_simple_root"] = json!({"error": "failed to create request"});
        }

        let pkg_simple_req = Request::new(&pypi_pkg_simple_url, Method::Get);
        if let Ok(psr) = pkg_simple_req {
            match worker::Fetch::Request(psr).send().await {
                Ok(mut resp) => {
                    let blen = resp.text().await.map(|b| b.len()).unwrap_or(0);
                    results["pypi_simple_pkg"] = json!({"url": pypi_pkg_simple_url, "status": resp.status_code(), "body_len": blen});
                }
                Err(e) => {
                    results["pypi_simple_pkg"] = json!({"error": e.to_string()});
                }
            }
        } else {
            results["pypi_simple_pkg"] = json!({"error": "failed to create request"});
        }

        let json_req = Request::new(&pypi_json_url, Method::Get);
        if let Ok(jr) = json_req {
            match worker::Fetch::Request(jr).send().await {
                Ok(mut resp) => {
                    let body = resp.text().await.unwrap_or_default();
                    let preview = &body[..body.len().min(200)];
                    results["pypi_json"] = json!({"url": pypi_json_url, "status": resp.status_code(), "body_len": body.len(), "body_preview": preview});
                }
                Err(e) => {
                    results["pypi_json"] = json!({"error": e.to_string()});
                }
            }
        } else {
            results["pypi_json"] = json!({"error": "failed to create request"});
        }

        return make_json_response(200, results);
    }

    if config.debug_mode && path == "debug/gomod" {
        let gomod_list_url = format!(
            "{}/github.com/gin-gonic/gin/@v/list",
            config.gomod_registry.trim_end_matches('/')
        );

        let mut results = json!({});

        let list_req = Request::new(&gomod_list_url, Method::Get);
        if let Ok(lr) = list_req {
            match worker::Fetch::Request(lr).send().await {
                Ok(mut resp) => {
                    let body = resp.text().await.unwrap_or_default();
                    let preview = &body[..body.len().min(300)];
                    results["gomod_list"] = json!({"url": gomod_list_url, "status": resp.status_code(), "body_len": body.len(), "body_preview": preview});
                }
                Err(e) => {
                    results["gomod_list"] = json!({"error": e.to_string()});
                }
            }
        } else {
            results["gomod_list"] = json!({"error": "failed to create request"});
        }

        return make_json_response(200, results);
    }

    let result = dispatch(&req, path, config, checker, cache.as_ref()).await;
    match result {
        Some(resp) => add_cors_headers(resp),
        None => {
            let parts: Vec<&str> = path.split('/').collect();
            if !parts.is_empty()
                && matches!(parts[0], "npm" | "dl" | "gomod" | "pypi")
                && parts.len() == 1
            {
                make_json_response(
                    200,
                    json!({
                        "endpoint": parts[0],
                        "usage": format!("See available routes under /{}", parts[0]),
                        "example": match parts[0] {
                            "npm" | "dl" => "/npm/axios",
                            "gomod" => "/gomod/github.com/gin-gonic/gin/@v/list",
                            "pypi" => "/pypi/simple/",
                            _ => ""
                        }
                    }),
                )
            } else {
                make_json_response(
                    404,
                    json!({ "error": "Not Found", "path": format!("/{}", path), "hint": "Visit / for API documentation" }),
                )
            }
        }
    }
}

async fn dispatch(
    req: &Request,
    path: &str,
    config: &Config,
    checker: &DelayChecker,
    cache: Option<&MetadataCache>,
) -> Option<HandlerResult> {
    let parts: Vec<&str> = path.split('/').collect();

    if parts.is_empty() {
        return None;
    }

    match parts[0] {
        "npm" | "dl" => dispatch_npm(req, &parts, config, checker).await,
        "gomod" => dispatch_gomod(req, &parts, config, checker).await,
        "pypi" => dispatch_pypi(req, &parts, config, checker, cache).await,
        _ => None,
    }
}

async fn dispatch_npm(
    req: &Request,
    parts: &[&str],
    config: &Config,
    checker: &DelayChecker,
) -> Option<HandlerResult> {
    let req = req.clone().ok()?;

    if parts[0] == "npm" && parts.len() == 2 {
        return Some(super::handlers::npm::handle_npm_metadata(req, config, checker).await);
    }

    if parts[0] == "npm" && parts.len() == 3 {
        return Some(super::handlers::npm::handle_npm_version(req, config, checker).await);
    }

    if parts[0] == "dl" && parts.len() >= 2 {
        return Some(super::handlers::npm::handle_npm_download(req, config, checker).await);
    }

    None
}

async fn dispatch_gomod(
    req: &Request,
    parts: &[&str],
    config: &Config,
    checker: &DelayChecker,
) -> Option<HandlerResult> {
    let req = req.clone().ok()?;
    let last = parts.last()?;

    if *last == "list" {
        return Some(super::handlers::gomod::handle_gomod_version_list(req, config).await);
    }

    if last.ends_with(".info") {
        return Some(super::handlers::gomod::handle_gomod_version_info(req, config, checker).await);
    }

    if last.ends_with(".mod") {
        return Some(super::handlers::gomod::handle_gomod_go_mod(req, config, checker).await);
    }

    if last.ends_with(".zip") {
        return Some(super::handlers::gomod::handle_gomod_download(req, config, checker).await);
    }

    None
}

async fn dispatch_pypi(
    req: &Request,
    parts: &[&str],
    config: &Config,
    checker: &DelayChecker,
    cache: Option<&MetadataCache>,
) -> Option<HandlerResult> {
    if parts.len() == 1 || (parts.len() == 2 && parts[1] == "simple") {
        return Some(super::handlers::pypi::handle_pypi_simple_index(config).await);
    }

    if parts.len() >= 3 && parts[1] == "simple" {
        let package = parts[2];
        let req = req.clone().ok()?;
        return Some(
            super::handlers::pypi::handle_pypi_package_list(req, config, checker, package, cache).await,
        );
    }

    if parts.len() >= 3 && parts[1] == "packages" {
        let filename = parts.last()?.to_string();
        let req = req.clone().ok()?;
        let logger = DelayLogger::new();
        return Some(
            super::handlers::pypi::handle_pypi_download(req, config, checker, &logger, &filename, cache)
                .await,
        );
    }

    None
}
