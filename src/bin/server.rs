use axum::{
    extract::{Path, State},
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use reqwest::Client;
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use delay_mirror::core::{Config, DelayChecker};
use delay_mirror::pypi_handler;

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    checker: Arc<DelayChecker>,
    client: Arc<Client>,
    npm_cache: Arc<RwLock<HashMap<String, (serde_json::Value, Instant)>>>,
}

#[tokio::main]
async fn main() {
    let config = Config::from_std_env();
    let checker = DelayChecker::with_delay_days(config.delay_days).expect("Invalid DELAY_DAYS");

    let client = Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .pool_max_idle_per_host(10)
        .user_agent("Go-http-client/2.0")
        .build()
        .expect("Failed to create HTTP client");

    let state = AppState {
        config: Arc::new(config),
        checker: Arc::new(checker),
        client: Arc::new(client),
        npm_cache: Arc::new(RwLock::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/npm/*path", get(npm_handler))
        .route("/dl/*path", get(dl_handler))
        .route("/gomod/*path", get(gomod_handler))
        .route("/pypi/*path", get(pypi_handler))
        .with_state(state);

    let addr: SocketAddr = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(([0, 0, 0, 0], 8080).into());

    println!("Delay Mirror server listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

async fn index() -> impl IntoResponse {
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

    Json(json!({
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
                "metadata": { "method": "GET", "path": "/npm/{package}" },
                "download": { "method": "GET", "path": "/dl/{package}@{version}.tgz" }
            },
            "gomod": {
                "version_list": { "method": "GET", "path": "/gomod/{module}/@v/list" }
            },
            "pypi": {
                "simple_index": { "method": "GET", "path": "/pypi/simple/" }
            }
        }
    }))
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
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

    Json(json!({
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
            "delay_days": state.config.delay_days,
            "npm_registry": state.config.npm_registry,
            "npm_download_registry": state.config.npm_download_registry,
            "gomod_registry": state.config.gomod_registry,
            "gomod_download_registry": state.config.gomod_download_registry,
            "pypi_registry": state.config.pypi_registry,
            "pypi_simple_index": state.config.pypi_simple_index,
            "pypi_json_api_base": state.config.pypi_json_api_base,
            "pypi_download_base": state.config.pypi_download_base,
            "pypi_download_mirror": state.config.pypi_download_mirror.clone(),
            "allowlist_enabled": state.config.allowlist_enabled,
            "debug_mode": state.config.debug_mode,
        }
    }))
}

async fn npm_handler(
    State(state): State<AppState>,
    Path(path): Path<String>,
    _uri: Uri,
) -> impl IntoResponse {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    if segments.is_empty() {
        return error_response(400, "Missing package name");
    }

    let package = segments[0];

    if let Some(version) = segments.get(1) {
        handle_npm_version(&state, package, version).await
    } else {
        handle_npm_metadata(&state, package).await
    }
}

async fn handle_npm_metadata(state: &AppState, package: &str) -> Response {
    let url = format!("{}/{}", state.config.npm_registry, package);

    match reqwest::get(&url).await {
        Ok(resp) => {
            if !resp.status().is_success() {
                return error_response(
                    resp.status().as_u16(),
                    &format!("Upstream error for package: {}", package),
                );
            }

            match resp.json::<serde_json::Value>().await {
                Ok(metadata) => {
                    {
                        let mut cache = state.npm_cache.write().await;
                        cache.insert(package.to_string(), (metadata.clone(), Instant::now()));
                    }
                    let filtered = filter_npm_metadata(metadata, &state.checker);
                    json_response(200, &filtered)
                }
                Err(e) => error_response(502, &format!("Failed to parse upstream response: {}", e)),
            }
        }
        Err(e) => error_response(502, &format!("Upstream fetch failed: {}", e)),
    }
}

async fn handle_npm_version(state: &AppState, package: &str, version: &str) -> Response {
    let cached_metadata = {
        let cache = state.npm_cache.read().await;
        if let Some((cached_meta, cached_time)) = cache.get(package) {
            if cached_time.elapsed() < Duration::from_secs(300) {
                Some(cached_meta.clone())
            } else {
                None
            }
        } else {
            None
        }
    };

    let metadata = match cached_metadata {
        Some(m) => m,
        None => {
            let url = format!("{}/{}", state.config.npm_registry, package);
            match reqwest::get(&url).await {
                Ok(resp) => {
                    if !resp.status().is_success() {
                        return error_response(
                            resp.status().as_u16(),
                            &format!("Upstream error for package: {}", package),
                        );
                    }
                    match resp.json::<serde_json::Value>().await {
                        Ok(meta) => {
                            let mut cache = state.npm_cache.write().await;
                            cache.insert(package.to_string(), (meta.clone(), Instant::now()));
                            meta
                        }
                        Err(e) => {
                            return error_response(
                                502,
                                &format!("Failed to parse upstream response: {}", e),
                            )
                        }
                    }
                }
                Err(e) => return error_response(502, &format!("Upstream fetch failed: {}", e)),
            }
        }
    };

    let time = metadata.get("time").cloned().unwrap_or(json!({}));

    match state.checker.parse_time_field(&time) {
        Ok(time_info) => match state.checker.check_version(version, &time_info) {
            Ok(delay_mirror::core::VersionCheckResult::Allowed) => {
                let tarball_url = format!(
                    "{}/{}/-/{}-{}.tgz",
                    state.config.npm_download_registry,
                    package,
                    package.split('/').next_back().unwrap_or(package),
                    version
                );
                redirect_response(&tarball_url)
            }
            Ok(delay_mirror::core::VersionCheckResult::Denied { publish_time }) => {
                let body = json!({
                    "error": "Version too recent for download",
                    "package": package,
                    "requested_version": version,
                    "reason": format!("Version was published within the last {} day(s)", state.config.delay_days),
                    "publish_time": publish_time.to_rfc3339()
                });
                error_response_json_with_headers(
                    403,
                    &body,
                    vec![
                        ("X-Delay-Original-Version", version.to_string()),
                        ("X-Delay-Reason", "Version too recent".to_string()),
                    ],
                )
            }
            Ok(delay_mirror::core::VersionCheckResult::Downgraded {
                suggested_version, ..
            }) => {
                let tarball_url = format!(
                    "{}/{}/-/{}-{}.tgz",
                    state.config.npm_download_registry,
                    package,
                    package.split('/').next_back().unwrap_or(package),
                    suggested_version
                );
                redirect_response_with_headers(
                    &tarball_url,
                    vec![("X-Delay-Redirected-Version", suggested_version.clone())],
                )
            }
            Err(e) => error_response(500, &format!("Version check error: {}", e)),
        },
        Err(e) => error_response(500, &format!("Time parse error: {}", e)),
    }
}

fn filter_npm_metadata(
    mut metadata: serde_json::Value,
    checker: &DelayChecker,
) -> serde_json::Value {
    let time = metadata.get("time").cloned().unwrap_or(json!({}));

    if let Ok(time_info) = checker.parse_time_field(&time) {
        if let Some(versions) = metadata.get_mut("versions").and_then(|v| v.as_object_mut()) {
            versions.retain(|version, _| {
                time_info
                    .get_publish_time(version)
                    .map(|t| checker.is_version_allowed(t))
                    .unwrap_or(true)
            });
        }

        if let Some(dist_tags) = metadata
            .get_mut("dist-tags")
            .and_then(|v| v.as_object_mut())
        {
            if let Some(latest) = dist_tags.get("latest").and_then(|v| v.as_str()) {
                if time_info
                    .get_publish_time(latest)
                    .map(|t| !checker.is_version_allowed(t))
                    .unwrap_or(false)
                {
                    let eligible_latest = time_info
                        .versions()
                        .filter(|v| {
                            time_info
                                .get_publish_time(v)
                                .map(|t| checker.is_version_allowed(t))
                                .unwrap_or(false)
                        })
                        .max_by(|a, b| compare_versions(a, b))
                        .cloned();

                    if let Some(new_latest) = eligible_latest {
                        dist_tags.insert("latest".to_string(), json!(new_latest));
                    }
                }
            }
        }
    }

    metadata
}

async fn dl_handler(State(state): State<AppState>, Path(path): Path<String>) -> impl IntoResponse {
    let filename = path.split('/').next_back().unwrap_or(&path);

    let (package, version) = if let Some(captures) =
        regex::Regex::new(r"^(.+)-(\d+\.\d+\.\d+(?:-.+)?)\.tgz$")
            .ok()
            .and_then(|re| re.captures(filename))
    {
        (
            captures.get(1).map(|m| m.as_str().to_string()),
            captures.get(2).map(|m| m.as_str().to_string()),
        )
    } else {
        (None, None)
    };

    match (package, version) {
        (Some(pkg), Some(ver)) => {
            let url = format!(
                "{}/{}/-/{}-{}.tgz",
                state.config.npm_download_registry, pkg, pkg, ver
            );
            redirect_response(&url)
        }
        _ => error_response(400, "Invalid tarball filename format"),
    }
}

async fn gomod_handler(
    State(state): State<AppState>,
    Path(path): Path<String>,
) -> impl IntoResponse {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    let at_v_pos = segments.iter().position(|s| *s == "@v");
    if at_v_pos.is_none() {
        let fallback_path = segments.join("/");
        let url = format!(
            "{}/{}",
            state.config.gomod_registry.trim_end_matches('/'),
            fallback_path
        );
        return redirect_response(&url);
    }

    let at_v_pos = at_v_pos.unwrap();
    let module = segments[..at_v_pos].join("/");
    let rest = &segments[at_v_pos + 1..];

    if rest.is_empty() {
        return error_response(400, "Missing version information");
    }

    let endpoint = rest[0];

    match endpoint {
        "list" => {
            let url = format!(
                "{}/{}/@v/list",
                state.config.gomod_registry.trim_end_matches('/'),
                module
            );
            proxy_upstream(&url).await
        }
        endpoint if endpoint.ends_with(".info") => {
            let version = endpoint.trim_end_matches(".info");
            handle_gomod_version(&state, &module, version, "info").await
        }
        endpoint if endpoint.ends_with(".mod") => {
            let version = endpoint.trim_end_matches(".mod");
            handle_gomod_version(&state, &module, version, "mod").await
        }
        endpoint if endpoint.ends_with(".zip") => {
            let version = endpoint.trim_end_matches(".zip");
            handle_gomod_version(&state, &module, version, "zip").await
        }
        _ => error_response(400, &format!("Unknown Go module endpoint: {}", endpoint)),
    }
}

async fn handle_gomod_version(
    state: &AppState,
    module: &str,
    version: &str,
    endpoint_type: &str,
) -> Response {
    if endpoint_type == "zip" {
        let url = format!(
            "{}/{}/@v/{}.zip",
            state.config.gomod_download_registry.trim_end_matches('/'),
            module,
            version
        );
        return redirect_response(&url);
    }

    let url = format!(
        "{}/{}/@v/{}.{}",
        state.config.gomod_registry.trim_end_matches('/'),
        module,
        version,
        endpoint_type
    );

    if endpoint_type == "info" {
        match reqwest::get(&url).await {
            Ok(resp) => {
                if !resp.status().is_success() {
                    return error_response(resp.status().as_u16(), "Upstream error");
                }

                match resp.json::<serde_json::Value>().await {
                    Ok(info) => {
                        if let Some(time_str) = info.get("Time").and_then(|v| v.as_str()) {
                            if let Ok(publish_time) =
                                time_str.parse::<chrono::DateTime<chrono::Utc>>()
                            {
                                if !state.checker.is_version_allowed(&publish_time) {
                                    let body = json!({
                                        "error": "Version too recent for access",
                                        "module": module,
                                        "requested_version": version,
                                        "reason": format!("Version was published within the last {} day(s)", state.config.delay_days),
                                        "publish_time": time_str
                                    });
                                    return error_response_json(403, &body);
                                }
                            }
                        }
                        json_response(200, &info)
                    }
                    Err(e) => error_response(502, &format!("Failed to parse response: {}", e)),
                }
            }
            Err(e) => error_response(502, &format!("Upstream fetch failed: {}", e)),
        }
    } else {
        proxy_upstream(&url).await
    }
}

async fn pypi_handler(
    State(state): State<AppState>,
    Path(path): Path<String>,
) -> impl IntoResponse {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    if segments.is_empty() {
        return error_response(400, "Missing path");
    }

    match segments[0] {
        "simple" => {
            if segments.len() == 1 {
                pypi_handler::handle_pypi_simple_index(&state.config, &state.client).await
            } else {
                let package = segments[1..].join("/");
                pypi_handler::handle_pypi_package_list(
                    &state.config,
                    &state.checker,
                    &package,
                    &state.client,
                )
                .await
            }
        }
        "packages" => {
            let filename = segments[1..].join("/");
            pypi_handler::handle_pypi_download(
                &state.config,
                &state.checker,
                &filename,
                &state.client,
            )
            .await
        }
        _ => error_response(400, &format!("Unknown PyPI endpoint: {}", segments[0])),
    }
}

async fn proxy_upstream(url: &str) -> Response {
    match reqwest::get(url).await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::OK);
            let headers = resp.headers().clone();

            match resp.bytes().await {
                Ok(body) => {
                    let mut builder = Response::builder().status(status);
                    for (name, value) in headers {
                        if let Some(name) = name {
                            builder = builder.header(name, value);
                        }
                    }
                    builder
                        .body(axum::body::boxed(axum::body::Full::from(body.to_vec())))
                        .unwrap()
                }
                Err(e) => error_response(502, &format!("Failed to read response body: {}", e)),
            }
        }
        Err(e) => error_response(502, &format!("Upstream fetch failed: {}", e)),
    }
}

fn json_response(status: u16, body: &impl Serialize) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    (status, Json(body)).into_response()
}

fn error_response(status: u16, message: &str) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(json!({ "error": message }))).into_response()
}

fn error_response_json(status: u16, body: &impl Serialize) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(body)).into_response()
}

fn error_response_json_with_headers(
    status: u16,
    body: &impl Serialize,
    headers: Vec<(&'static str, String)>,
) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut resp = (status, Json(body)).into_response();

    for (name, value) in headers {
        resp.headers_mut().insert(name, value.parse().unwrap());
    }

    resp
}

fn redirect_response(url: &str) -> Response {
    Response::builder()
        .status(StatusCode::TEMPORARY_REDIRECT)
        .header(header::LOCATION, url)
        .body(axum::body::boxed(axum::body::Full::from(vec![])))
        .unwrap()
}

fn redirect_response_with_headers(url: &str, headers: Vec<(&'static str, String)>) -> Response {
    let mut builder = Response::builder()
        .status(StatusCode::TEMPORARY_REDIRECT)
        .header(header::LOCATION, url);

    for (name, value) in headers {
        builder = builder.header(name, value);
    }

    builder
        .body(axum::body::boxed(axum::body::Full::from(vec![])))
        .unwrap()
}

fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let a_parts: Vec<u64> = a.split('.').filter_map(|s| s.parse().ok()).collect();
    let b_parts: Vec<u64> = b.split('.').filter_map(|s| s.parse().ok()).collect();
    a_parts.cmp(&b_parts)
}
