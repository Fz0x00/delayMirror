use std::collections::HashMap;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::{json, Value};
use sha2::Digest as Sha2Digest;
use sha2::Sha256;
use worker::*;

use crate::core::delay_check::VersionTimeInfo;
use crate::core::delay_logger::{DelayLogger, PackageType};
use crate::core::Config;
use crate::core::{DelayCheckError, DelayChecker, VersionCheckResult};

#[derive(serde::Serialize)]
struct UpstreamErrorResponse {
    error: String,
    status: u16,
    upstream_status: u16,
    context: String,
    request_id: String,
    timestamp: String,
}

fn generate_request_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{:x}{:x}", duration.as_secs(), duration.subsec_nanos())
}

fn map_upstream_error(status: u16, context: &str, _url: &str) -> Response {
    let request_id = generate_request_id();
    let timestamp = chrono::Utc::now().to_rfc3339();

    let (http_status, message) = match status {
        400 => (400, format!("Bad request to upstream: {}", context)),
        401 | 403 => (502, "Upstream authentication failed".to_string()),
        404 => (404, format!("Resource not found on upstream: {}", context)),
        429 => (429, "Upstream rate limited, please retry later".to_string()),
        500 => (502, "Upstream internal error".to_string()),
        502 => (502, "Upstream bad gateway".to_string()),
        503 => (503, "Upstream service unavailable".to_string()),
        504 => (504, "Upstream gateway timeout".to_string()),
        _ => (502, format!("Unexpected upstream error: {}", status)),
    };

    let error_body = UpstreamErrorResponse {
        error: message,
        status: http_status,
        upstream_status: status,
        context: context.to_string(),
        request_id,
        timestamp,
    };

    let mut headers = Headers::new();
    headers.set("Content-Type", "application/json").ok();

    if status == 429 {
        headers.set("Retry-After", "60").ok();
    }

    let body_str = serde_json::to_string(&error_body).unwrap_or_default();

    Response::from_bytes(body_str.into_bytes())
        .unwrap_or_else(|_| {
            Response::error(
                &format!("upstream error, status={}", http_status),
                http_status,
            )
            .unwrap()
        })
        .with_status(http_status)
        .with_headers(headers)
}

pub struct RequestContext {
    pypi_metadata_cache: HashMap<String, Value>,
}

impl RequestContext {
    pub fn new() -> Self {
        Self {
            pypi_metadata_cache: HashMap::new(),
        }
    }

    pub async fn get_or_fetch_metadata(
        &mut self,
        package: &str,
        json_api_base: &str,
    ) -> std::result::Result<Value, Response> {
        let normalized = normalize_package_name(package);

        if let Some(cached) = self.pypi_metadata_cache.get(&normalized) {
            return Ok(cached.clone());
        }

        let metadata = fetch_pypi_release_info(package, json_api_base).await?;
        self.pypi_metadata_cache
            .insert(normalized, metadata.clone());
        Ok(metadata)
    }
}

#[allow(dead_code)]
const PYPI_JSON_API_BASE: &str = "https://pypi.org/pypi";

pub fn normalize_package_name(name: &str) -> String {
    let re = Regex::new(r"[-_.]+").unwrap();
    re.replace_all(name, "-").to_lowercase()
}

pub fn parse_package_filename(filename: &str) -> Option<(String, String)> {
    if filename.ends_with(".whl") {
        parse_wheel_filename(filename)
    } else if filename.ends_with(".tar.gz") {
        parse_sdist_filename(filename)
    } else {
        None
    }
}

fn parse_wheel_filename(filename: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = filename.trim_end_matches(".whl").split('-').collect();

    if parts.len() < 5 {
        return None;
    }

    let distribution = parts[0];
    let version = parts[1];

    let last_part = parts.last()?;
    let is_valid_wheel = last_part.contains('.')
        || parts.len() >= 5
            && parts[parts.len() - 1]
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '.');

    if !is_valid_wheel {
        return None;
    }

    Some((distribution.to_string(), version.to_string()))
}

fn parse_sdist_filename(filename: &str) -> Option<(String, String)> {
    let filename = filename.trim_end_matches(".tar.gz");

    let version_start = filename.find(|c: char| c.is_ascii_digit())?;

    let distribution = &filename[..version_start];
    let distribution = distribution.trim_end_matches('-').trim_end_matches('_');

    if distribution.is_empty() {
        return None;
    }

    let version = &filename[version_start..];

    if version.is_empty() {
        return None;
    }

    Some((distribution.to_string(), version.to_string()))
}

async fn fetch_pypi_release_info(
    package: &str,
    json_api_base: &str,
) -> std::result::Result<Value, Response> {
    let normalized = normalize_package_name(package);
    let url = format!("{}/{}/json", json_api_base, normalized);

    let req = match Request::new(&url, Method::Get) {
        Ok(r) => r,
        Err(e) => {
            return Err(
                Response::error(format!("Failed to create PyPI request: {}", e), 502)
                    .unwrap_or_else(|_| Response::error("Bad Gateway", 502).unwrap()),
            );
        }
    };

    let mut resp = match Fetch::Request(req).send().await {
        Ok(r) => r,
        Err(e) => {
            return Err(
                Response::error(format!("Failed to connect to PyPI JSON API: {}", e), 502)
                    .unwrap_or_else(|_| Response::error("Bad Gateway", 502).unwrap()),
            );
        }
    };

    let status = resp.status_code();

    if !(200..300).contains(&status) {
        return Err(map_upstream_error(
            status,
            &format!("package={}", package),
            &url,
        ));
    }

    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => {
            return Err(Response::error(
                format!("Failed to read PyPI JSON API response: {}", e),
                500,
            )
            .unwrap_or_else(|_| Response::error("Internal Server Error", 500).unwrap()));
        }
    };

    let metadata: Value = match serde_json::from_str(&body) {
        Ok(m) => m,
        Err(e) => {
            return Err(Response::error(
                format!("Failed to parse PyPI JSON API response: {}", e),
                500,
            )
            .unwrap_or_else(|_| Response::error("Internal Server Error", 500).unwrap()));
        }
    };

    Ok(metadata)
}

async fn fetch_metadata_parallel(
    package: &str,
    config: &Config,
) -> std::result::Result<(String, Value), Response> {
    let normalized = normalize_package_name(package);

    let simple_url = format!(
        "{}{}/",
        config.pypi_registry.trim_end_matches('/'),
        normalized
    );
    let json_url = format!("{}/{}/json", config.pypi_json_api_base, normalized);

    let simple_req = match Request::new(&simple_url, Method::Get) {
        Ok(r) => r,
        Err(e) => {
            return Err(Response::error(
                format!("Failed to create PyPI simple request: {}", e),
                502,
            )
            .unwrap_or_else(|_| Response::error("Bad Gateway", 502).unwrap()));
        }
    };
    let json_req = match Request::new(&json_url, Method::Get) {
        Ok(r) => r,
        Err(e) => {
            return Err(
                Response::error(format!("Failed to create PyPI JSON request: {}", e), 502)
                    .unwrap_or_else(|_| Response::error("Bad Gateway", 502).unwrap()),
            );
        }
    };

    let mut simple_resp = match Fetch::Request(simple_req).send().await {
        Ok(r) => r,
        Err(e) => {
            return Err(
                Response::error(format!("Simple API fetch failed: {}", e), 502)
                    .unwrap_or_else(|_| Response::error("Bad Gateway", 502).unwrap()),
            )
        }
    };
    let mut json_resp = match Fetch::Request(json_req).send().await {
        Ok(r) => r,
        Err(e) => {
            return Err(
                Response::error(format!("JSON API fetch failed: {}", e), 502)
                    .unwrap_or_else(|_| Response::error("Bad Gateway", 502).unwrap()),
            )
        }
    };

    let simple_status = simple_resp.status_code();
    let json_status = json_resp.status_code();

    if !(200..300).contains(&simple_status) {
        return Err(map_upstream_error(
            simple_status,
            &format!("package list (parallel), package={}", package),
            &simple_url,
        ));
    }

    let body = match simple_resp.text().await {
        Ok(b) => b,
        Err(e) => {
            return Err(
                Response::error(format!("Failed to read simple API response: {}", e), 500)
                    .unwrap_or_else(|_| Response::error("Internal Server Error", 500).unwrap()),
            )
        }
    };

    let json_body = if (200..300).contains(&json_status) {
        match json_resp.text().await {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => Value::Null,
            },
            Err(_) => Value::Null,
        }
    } else {
        console_warn!(
            "Parallel JSON API returned status {} for {}, using Null metadata",
            json_status,
            package
        );
        Value::Null
    };

    Ok((body, json_body))
}

fn parse_upload_time(time_str: &str) -> std::result::Result<DateTime<Utc>, DelayCheckError> {
    crate::core::delay_check::parse_datetime_flexible(time_str).map_err(|e| {
        DelayCheckError::InvalidTimeFormat(format!("invalid upload_time format: {}", e))
    })
}

fn build_version_time_info_from_releases(
    releases: &Value,
) -> std::result::Result<Value, DelayCheckError> {
    let releases_obj = releases.as_object().ok_or_else(|| {
        DelayCheckError::InvalidTimeFormat("releases field is not a JSON object".to_string())
    })?;

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

    if time_map.is_empty() {
        return Err(DelayCheckError::MissingTimeField);
    }

    Ok(Value::Object(time_map))
}

fn find_download_url_for_version(
    releases: &Value,
    version: &str,
    filename: &str,
) -> Option<String> {
    let files = releases.get(version).and_then(|v| v.as_array())?;

    for file in files {
        if file.get("filename").and_then(|f| f.as_str()) == Some(filename) {
            return file
                .get("url")
                .and_then(|u| u.as_str())
                .map(|s| s.to_string());
        }
    }

    files
        .first()?
        .get("url")
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
}

fn find_sha256_for_file(releases: &Value, version: &str, filename: &str) -> Option<String> {
    let files = releases.get(version).and_then(|v| v.as_array())?;

    for file in files {
        if file.get("filename").and_then(|f| f.as_str()) == Some(filename) {
            return file
                .get("sha256_digest")
                .and_then(|h| h.as_str())
                .map(|s| s.to_string());
        }
    }

    None
}

async fn download_with_hash_verification(url: &str, expected_hash: &str) -> Result<Response> {
    let req = Request::new(url, Method::Get)?;
    let mut resp = Fetch::Request(req)
        .send()
        .await
        .map_err(|e| worker::Error::from(e))?;

    if resp.status_code() != 200 {
        return Err(worker::Error::RustError(format!(
            "Download failed: status {}",
            resp.status_code()
        )));
    }

    let body = resp.bytes().await.map_err(|e| worker::Error::from(e))?;

    let mut hasher = Sha256::new();
    hasher.update(&body);
    let computed_hash = format!("{:x}", hasher.finalize());

    if computed_hash != expected_hash {
        return Err(worker::Error::RustError(format!(
            "SHA256 mismatch: expected={}, computed={}",
            expected_hash, computed_hash
        )));
    }

    let mut headers = Headers::new();
    headers.set("X-Integrity-Verified", "sha256").ok();

    Ok(Response::from_bytes(body)
        .map_err(|e| worker::Error::from(e))?
        .with_status(200)
        .with_headers(headers))
}

fn build_sdist_filename(package: &str, version: &str) -> String {
    format!("{}-{}.tar.gz", package, version)
}

fn convert_html_to_pep691_json(html: &str, base_url: &str) -> Value {
    let re = Regex::new(r#"<a[^>]+href="([^"]+)"([^>]*)>([^<]+)</a>"#).unwrap();

    let name_re = Regex::new(r#"<title>\s*Links for\s+(.*?)\s*</title>"#).unwrap();
    let package_name = name_re
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
        .unwrap_or_default();

    let base_clean = base_url.trim_end_matches('/');

    let files: Vec<Value> = re
        .captures_iter(html)
        .filter_map(|cap| {
            let href = cap.get(1)?.as_str().to_string();
            let attrs = cap.get(2)?.as_str();
            let filename = cap.get(3)?.as_str().trim().to_string();

            let (url, hashes) = extract_url_and_hashes_for_json(&href, base_clean);

            let requires_python = extract_attr_value(attrs, "data-requires-python");
            let yanked = attrs.contains("data-yanked");

            Some(json!({
                "filename": filename,
                "url": url,
                "hashes": hashes,
                "requires-python": requires_python,
                "yanked": yanked,
            }))
        })
        .collect();

    json!({
        "meta": {"api-version": "1.0"},
        "name": package_name,
        "files": files
    })
}

fn extract_url_and_hashes_for_json(href: &str, base_url: &str) -> (String, Value) {
    let (base_part, fragment) = href.split_once('#').unwrap_or((href, ""));

    let url = if base_part.starts_with("http") {
        base_part.to_string()
    } else {
        format!("{}/{}", base_url, base_part.trim_start_matches('/'))
    };

    let mut hashes = serde_json::Map::new();
    for part in fragment.split('&') {
        if let Some((algo, hash)) = part.split_once('=') {
            hashes.insert(algo.to_string(), Value::String(hash.to_string()));
        }
    }

    (url, Value::Object(hashes))
}

fn extract_attr_value(attrs: &str, attr_name: &str) -> Option<String> {
    let pattern = format!(r#"{}="([^"]*)""#, regex::escape(attr_name));
    let re = Regex::new(&pattern).ok()?;
    re.captures(attrs)?.get(1).map(|m| m.as_str().to_string())
}

fn filter_recent_versions_html(
    html: &str,
    time_info: &VersionTimeInfo,
    checker: &DelayChecker,
) -> (String, Vec<String>) {
    let re = Regex::new(r#"<a[^>]+href="([^"]+)"[^>]*>([^<]+)</a>"#).unwrap();

    let mut blocked_versions = Vec::new();
    let mut result = html.to_string();

    for cap in re.captures_iter(html) {
        let full_match = cap.get(0).unwrap().as_str().to_string();
        let _href = cap.get(1).unwrap().as_str().to_string();
        let link_text = cap.get(2).unwrap().as_str();

        if let Some((_, version)) = parse_package_filename(link_text) {
            if let Some(publish_time) = time_info.get_publish_time(&version) {
                if !checker.is_version_allowed(publish_time) {
                    result = result.replace(&full_match, "");
                    blocked_versions.push(version);
                }
            }
        }
    }

    (result, blocked_versions)
}

fn filter_recent_versions_json(
    mut json: Value,
    time_info: &VersionTimeInfo,
    checker: &DelayChecker,
) -> (Value, Vec<String>) {
    let mut blocked = Vec::new();

    if let Some(files) = json.get_mut("files").and_then(|f| f.as_array_mut()) {
        files.retain(|file| {
            let filename = file.get("filename").and_then(|f| f.as_str()).unwrap_or("");
            if let Some((_, version)) = parse_package_filename(filename) {
                if let Some(t) = time_info.get_publish_time(&version) {
                    if !checker.is_version_allowed(t) {
                        if let Some(v) = file.get("version").and_then(|v| v.as_str()) {
                            blocked.push(v.to_string());
                        } else {
                            blocked.push(version);
                        }
                        return false;
                    }
                }
            }
            true
        });
    }

    (json, blocked)
}

pub async fn handle_pypi_simple_index(config: &Config) -> Result<Response> {
    let url = config.pypi_registry.trim_end_matches('/');
    let simple_url = format!("{}/", url);

    let req = match Request::new(&simple_url, Method::Get) {
        Ok(r) => r,
        Err(e) => {
            return Err(format!("Failed to create upstream request: {}", e).into());
        }
    };
    let mut resp = match Fetch::Request(req).send().await {
        Ok(r) => r,
        Err(e) => {
            return Err(format!("Failed to connect to upstream PyPI registry: {}", e).into());
        }
    };

    let status = resp.status_code();

    if !(200..300).contains(&status) {
        let _ = map_upstream_error(status, "simple index fetch", &simple_url);
        return Err(worker::Error::RustError(format!(
            "upstream PyPI simple index error: status={}, context=simple index fetch",
            status
        )));
    }

    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => return Err(format!("Failed to read upstream response: {}", e).into()),
    };

    let mut headers = Headers::new();
    headers.set("X-Delay-Info", "Simple index does not contain timing data")?;
    headers.set("Content-Type", "text/html; charset=utf-8")?;
    Ok(Response::ok(body)?.with_headers(headers))
}

pub async fn handle_pypi_package_list(
    _req: Request,
    config: &Config,
    checker: &DelayChecker,
    ctx: &mut RequestContext,
    package: &str,
) -> Result<Response> {
    match handle_pypi_package_list_inner(_req, config, checker, ctx, package).await {
        Ok(resp) => Ok(resp),
        Err(e) => {
            let err_msg = format!("PyPI package list error for {}: {:?}", package, e);
            console_error!("{}", err_msg);
            Ok(Response::error(&err_msg, 500)
                .unwrap_or_else(|_| Response::error("Internal Server Error", 500).unwrap()))
        }
    }
}

async fn handle_pypi_package_list_inner(
    _req: Request,
    config: &Config,
    checker: &DelayChecker,
    ctx: &mut RequestContext,
    package: &str,
) -> Result<Response> {
    let normalized_package = normalize_package_name(package);

    let (body, pre_fetched_metadata) =
        fetch_metadata_parallel(package, config)
            .await
            .map_err(|e| {
                worker::Error::RustError(format!("parallel metadata fetch failed: {:?}", e))
            })?;

    if !ctx.pypi_metadata_cache.contains_key(&normalized_package)
        && pre_fetched_metadata != Value::Null
    {
        ctx.pypi_metadata_cache
            .insert(normalized_package.clone(), pre_fetched_metadata);
    }

    let release_info: std::result::Result<Value, ()> = Ok(ctx
        .pypi_metadata_cache
        .get(&normalized_package)
        .cloned()
        .unwrap_or(Value::Null));

    let filter_mode = config.pypi_filter_mode.as_deref();

    if let Some("strict") = filter_mode {
        if let Ok(info) = &release_info {
            if let Some(releases) = info.get("releases") {
                if let Ok(time_value) = build_version_time_info_from_releases(releases) {
                    if let Ok(time_info) = checker.parse_time_field(&time_value) {
                        let accept_header = _req
                            .headers()
                            .get("Accept")
                            .map(|v| v.unwrap_or_default())
                            .unwrap_or_default();
                        let is_json_api =
                            accept_header.contains("application/vnd.pypi.simple.v1+json");

                        if is_json_api {
                            let mut json_body =
                                convert_html_to_pep691_json(&body, &config.pypi_registry);
                            let (filtered, blocked) =
                                filter_recent_versions_json(json_body, &time_info, checker);
                            json_body = filtered;

                            let content_type = "application/vnd.pypi.simple.v1+json";
                            let mut headers = Headers::new();
                            headers.set("Content-Type", content_type)?;
                            if !blocked.is_empty() {
                                headers.set("X-Delay-Hidden-Versions", &blocked.join(", "))?;
                            }
                            return Ok(Response::from_json(&json_body)?.with_headers(headers));
                        } else {
                            let (filtered_html, blocked) =
                                filter_recent_versions_html(&body, &time_info, checker);

                            let content_type = "text/html; charset=utf-8";
                            let mut headers = Headers::new();
                            headers.set("Content-Type", content_type)?;
                            if !blocked.is_empty() {
                                headers.set("X-Delay-Hidden-Versions", &blocked.join(", "))?;
                            }
                            return Ok(Response::ok(filtered_html)?.with_headers(headers));
                        }
                    }
                }
            }
        }
    }

    let recent_versions_warning = match release_info {
        Ok(info) => {
            if let Some(releases) = info.get("releases") {
                match build_version_time_info_from_releases(releases) {
                    Ok(time_value) => match checker.parse_time_field(&time_value) {
                        Ok(time_info) => {
                            let recent_versions: Vec<String> = time_info
                                .versions()
                                .filter_map(|v| {
                                    if let Some(t) = time_info.get_publish_time(v) {
                                        if !checker.is_version_allowed(t) {
                                            Some((*v).clone())
                                        } else {
                                            None
                                        }
                                    } else {
                                        None
                                    }
                                })
                                .collect();

                            if recent_versions.is_empty() {
                                String::new()
                            } else {
                                format!(
                                    "Recent versions blocked by delay policy: {}",
                                    recent_versions.join(", ")
                                )
                            }
                        }
                        Err(_) => String::new(),
                    },
                    Err(_) => String::new(),
                }
            } else {
                String::new()
            }
        }
        Err(_) => String::new(),
    };

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

    let mut headers = Headers::new();

    if !recent_versions_warning.is_empty() {
        headers.set("X-Delay-Warning", &recent_versions_warning)?;
    } else {
        headers.set(
            "X-Delay-Info",
            "Package list served; use download endpoint for delay enforcement",
        )?;
    }

    headers.set("Content-Type", content_type)?;

    if is_json_api {
        let json_body = convert_html_to_pep691_json(&body, &config.pypi_registry);
        return Ok(Response::from_json(&json_body)?.with_headers(headers));
    }

    Ok(Response::ok(body)?.with_headers(headers))
}

pub async fn handle_pypi_download(
    _req: Request,
    config: &Config,
    checker: &DelayChecker,
    logger: &DelayLogger,
    ctx: &mut RequestContext,
    filename: &str,
) -> Result<Response> {
    let (package, version) = match parse_package_filename(filename) {
        Some(result) => result,
        None => {
            return Response::error(
                "Invalid filename format: Filename must be a valid wheel (.whl) or sdist (.tar.gz) format",
                400,
            );
        }
    };

    let client_ip = _req.headers().get("CF-Connecting-IP").ok().flatten();

    let metadata = match ctx
        .get_or_fetch_metadata(&package, &config.pypi_json_api_base)
        .await
    {
        Ok(m) => m,
        Err(resp) => return Ok(resp),
    };

    let releases = match metadata.get("releases") {
        Some(r) => r.clone(),
        None => {
            return Response::error(
                "Unable to determine version age: PyPI JSON API response does not contain 'releases' field",
                500,
            );
        }
    };

    let release_files = match releases.get(&version) {
        Some(files) => files,
        None => {
            return Response::error(
                format!(
                    "Version not found in package releases: package={}, version={}",
                    package, version
                ),
                404,
            );
        }
    };

    let upload_time_str = release_files
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|f| f.get("upload_time"))
        .and_then(|t| t.as_str());

    let _upload_time = match upload_time_str {
        Some(t) => match parse_upload_time(t) {
            Ok(dt) => dt,
            Err(e) => {
                return Response::error(format!("Failed to parse upload time: {}", e), 500);
            }
        },
        None => {
            return Response::error(
                "Upload time not found: Release files do not contain upload_time information",
                500,
            );
        }
    };

    let time_value = match build_version_time_info_from_releases(&releases) {
        Ok(tv) => tv,
        Err(e) => {
            return Response::error(format!("Failed to build version timing info: {}", e), 500);
        }
    };

    let time_info = match checker.parse_time_field(&time_value) {
        Ok(info) => info,
        Err(e) => {
            return Response::error(
                format!("Failed to parse version timing information: {}", e),
                500,
            );
        }
    };

    match checker.resolve_version(&version, &time_info) {
        Ok(VersionCheckResult::Allowed) => {
            let upstream_url = find_download_url_for_version(&releases, &version, filename)
                .unwrap_or_else(|| format!("{}/{}", config.pypi_download_base, filename));

            match find_sha256_for_file(&releases, &version, filename) {
                Some(expected_hash) => {
                    match download_with_hash_verification(&upstream_url, &expected_hash).await {
                        Ok(resp) => Ok(resp),
                        Err(e) => {
                            let error_body = json!({
                                "error": "Hash verification failed",
                                "detail": e.to_string(),
                                "package": package,
                                "version": version,
                                "filename": filename,
                                "url": upstream_url
                            });
                            console_error!(
                                "SHA256 verification failed for {}-{}: {}",
                                package,
                                version,
                                e
                            );
                            Ok(Response::from_json(&error_body)?.with_status(500))
                        }
                    }
                }
                None => {
                    console_warn!(
                        "No SHA256 hash available for {}/{}, falling back to direct fetch",
                        package,
                        filename
                    );
                    let upstream_req = Request::new(&upstream_url, Method::Get)?;
                    Fetch::Request(upstream_req).send().await
                }
            }
        }
        Ok(VersionCheckResult::Downgraded {
            suggested_version, ..
        }) => {
            let redirected_filename = if filename.ends_with(".whl") {
                filename.replace(
                    &format!("-{}-", version),
                    &format!("-{}-", suggested_version),
                )
            } else {
                build_sdist_filename(&package, &suggested_version)
            };

            let upstream_url =
                find_download_url_for_version(&releases, &suggested_version, &redirected_filename)
                    .unwrap_or_else(|| {
                        format!("{}/{}", config.pypi_download_base, redirected_filename)
                    });

            logger.log_downgraded(
                PackageType::PyPI,
                &package,
                &version,
                &suggested_version,
                "Version too recent, auto-downloaded for security",
                client_ip.as_deref(),
            );

            let mut headers = Headers::new();
            headers.set("X-Delay-Original-Version", &version)?;
            headers.set("X-Delay-Redirected-Version", &suggested_version)?;
            headers.set(
                "X-Delay-Reason",
                "Version too recent, auto-downloaded for security",
            )?;

            let upstream_req = Request::new(&upstream_url, Method::Get)?;
            let mut resp = Fetch::Request(upstream_req).send().await?;
            for (name, value) in headers.entries() {
                resp.headers_mut().set(&name, &value).ok();
            }
            Ok(resp)
        }
        Ok(VersionCheckResult::Denied { .. }) => {
            let suggested = checker
                .find_eligible_version(&version, &time_info)
                .ok()
                .flatten();

            let reason = format!(
                "Version was published within the last {} day(s)",
                checker.delay_days()
            );

            logger.log_blocked(
                PackageType::PyPI,
                &package,
                &version,
                &reason,
                client_ip.as_deref(),
            );

            let mut headers = Headers::new();
            headers.set("X-Delay-Original-Version", &version)?;
            headers.set(
                "X-Delay-Reason",
                "Version published too recently, below delay threshold",
            )?;

            let body = serde_json::json!({
                "error": "Version too recent for download",
                "package": package,
                "requested_version": version,
                "reason": reason,
                "suggested_version": suggested
            });

            Ok(Response::from_json(&body)?
                .with_status(403)
                .with_headers(headers))
        }
        Err(DelayCheckError::VersionNotFound { .. }) => Response::error(
            format!(
                "Version not found in package releases: package={}, version={}",
                package, version
            ),
            404,
        ),
        Err(e) => Response::error(format!("Delay check failed: {}", e), 500),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::delay_check::DelayChecker;
    use chrono::{Datelike, Duration, Utc};
    use serde_json::json;

    #[test]
    fn test_normalize_package_name_basic() {
        assert_eq!(normalize_package_name("Django"), "django");
        assert_eq!(normalize_package_name("DJA_NGO"), "dja-ngo");
        assert_eq!(normalize_package_name("numpy-1.0"), "numpy-1-0");
        assert_eq!(normalize_package_name("Pillow"), "pillow");
        assert_eq!(normalize_package_name("some_package"), "some-package");
        assert_eq!(normalize_package_name("some.package"), "some-package");
        assert_eq!(normalize_package_name("some-package"), "some-package");
    }

    #[test]
    fn test_normalize_package_name_with_dots() {
        assert_eq!(
            normalize_package_name("django-rest-framework"),
            "django-rest-framework"
        );
    }

    #[test]
    fn test_normalize_package_name_with_underscores() {
        assert_eq!(
            normalize_package_name("django_rest_framework"),
            "django-rest-framework"
        );
    }

    #[test]
    fn test_normalize_package_name_mixed_separators() {
        assert_eq!(
            normalize_package_name("Django_Rest-Framework.v2"),
            "django-rest-framework-v2"
        );
    }

    #[test]
    fn test_parse_wheel_filename_basic() {
        assert_eq!(
            parse_package_filename("lodash-4.17.21-py3-none-any.whl"),
            Some(("lodash".to_string(), "4.17.21".to_string()))
        );
        assert_eq!(
            parse_package_filename("numpy-1.21.0-cp39-cp39-win_amd64.whl"),
            Some(("numpy".to_string(), "1.21.0".to_string()))
        );
        assert_eq!(
            parse_package_filename("package-1.0-1-py3-none-any.whl"),
            Some(("package".to_string(), "1.0".to_string()))
        );
    }

    #[test]
    fn test_parse_sdist_filename_basic() {
        assert_eq!(
            parse_package_filename("lodash-4.17.21.tar.gz"),
            Some(("lodash".to_string(), "4.17.21".to_string()))
        );
        assert_eq!(
            parse_package_filename("numpy-1.21.0.tar.gz"),
            Some(("numpy".to_string(), "1.21.0".to_string()))
        );
        assert_eq!(
            parse_package_filename("my-package-2.0.0.tar.gz"),
            Some(("my-package".to_string(), "2.0.0".to_string()))
        );
    }

    #[test]
    fn test_parse_invalid_filenames() {
        assert_eq!(parse_package_filename("invalid.txt"), None);
        assert_eq!(parse_package_filename("no-extension"), None);
    }

    #[test]
    fn test_parse_wheel_with_build_tag() {
        assert_eq!(
            parse_package_filename("package-1.0.0+local-py3-none-any.whl"),
            Some(("package".to_string(), "1.0.0+local".to_string()))
        );
    }

    #[test]
    fn test_parse_upload_time_valid_rfc3339() {
        let result = parse_upload_time("2024-01-15T10:30:00Z");
        assert!(result.is_ok());
        let dt = result.unwrap();
        assert_eq!(dt.year(), 2024);
        assert_eq!(dt.month(), 1);
        assert_eq!(dt.day(), 15);
    }

    #[test]
    fn test_parse_upload_time_valid_with_offset() {
        let result = parse_upload_time("2024-06-20T14:45:00+08:00");
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_upload_time_invalid() {
        let result = parse_upload_time("not-a-date");
        assert!(result.is_err());
        match result.unwrap_err() {
            DelayCheckError::InvalidTimeFormat(_) => {}
            other => panic!("Expected InvalidTimeFormat, got {:?}", other),
        }
    }

    #[test]
    fn test_build_version_time_info_from_releases_valid() {
        let releases = json!({
            "1.0.0": [
                {"upload_time": "2024-01-01T00:00:00Z", "filename": "pkg-1.0.0.tar.gz", "url": "https://example.com/pkg-1.0.0.tar.gz"}
            ],
            "2.0.0": [
                {"upload_time": "2024-03-15T10:30:00Z", "filename": "pkg-2.0.0.tar.gz", "url": "https://example.com/pkg-2.0.0.tar.gz"}
            ],
            "3.0.0": [
                {"upload_time": "2024-06-01T12:00:00Z", "filename": "pkg-3.0.0-py3-none-any.whl", "url": "https://example.com/pkg-3.0.0-py3-none-any.whl"}
            ]
        });

        let result = build_version_time_info_from_releases(&releases);
        assert!(result.is_ok());
        let time_value = result.unwrap();
        let time_obj = time_value.as_object().unwrap();
        assert_eq!(time_obj.len(), 3);
        assert_eq!(
            time_obj.get("1.0.0").unwrap().as_str().unwrap(),
            "2024-01-01T00:00:00Z"
        );
        assert_eq!(
            time_obj.get("2.0.0").unwrap().as_str().unwrap(),
            "2024-03-15T10:30:00Z"
        );
        assert_eq!(
            time_obj.get("3.0.0").unwrap().as_str().unwrap(),
            "2024-06-01T12:00:00Z"
        );
    }

    #[test]
    fn test_build_version_time_info_from_releases_empty_releases() {
        let releases = json!({});
        let result = build_version_time_info_from_releases(&releases);
        assert!(result.is_err());
        match result.unwrap_err() {
            DelayCheckError::MissingTimeField => {}
            other => panic!("Expected MissingTimeField, got {:?}", other),
        }
    }

    #[test]
    fn test_build_version_time_info_from_releases_uses_first_file() {
        let releases = json!({
            "1.0.0": [
                {"upload_time": "2024-01-01T00:00:00Z", "filename": "pkg-1.0.0.tar.gz"},
                {"upload_time": "2024-01-02T00:00:00Z", "filename": "pkg-1.0.0-py3-none-any.whl"}
            ]
        });
        let result = build_version_time_info_from_releases(&releases).unwrap();
        let time_obj = result.as_object().unwrap();
        assert_eq!(
            time_obj.get("1.0.0").unwrap().as_str().unwrap(),
            "2024-01-01T00:00:00Z"
        );
    }

    #[test]
    fn test_find_download_url_for_version_exact_match() {
        let releases = json!({
            "1.0.0": [
                {"filename": "pkg-1.0.0.tar.gz", "url": "https://files.pythonhosted.org/packages/pkg-1.0.0.tar.gz"},
                {"filename": "pkg-1.0.0-py3-none-any.whl", "url": "https://files.pythonhosted.org/packages/pkg-1.0.0-py3-none-any.whl"}
            ]
        });
        let result = find_download_url_for_version(&releases, "1.0.0", "pkg-1.0.0.tar.gz");
        assert_eq!(
            result,
            Some("https://files.pythonhosted.org/packages/pkg-1.0.0.tar.gz".to_string())
        );
    }

    #[test]
    fn test_find_download_url_for_version_fallback_to_first() {
        let releases = json!({
            "1.0.0": [
                {"filename": "pkg-1.0.0.tar.gz", "url": "https://files.pythonhosted.org/packages/pkg-1.0.0.tar.gz"}
            ]
        });
        let result = find_download_url_for_version(&releases, "1.0.0", "nonexistent-file.whl");
        assert_eq!(
            result,
            Some("https://files.pythonhosted.org/packages/pkg-1.0.0.tar.gz".to_string())
        );
    }

    #[test]
    fn test_find_download_url_for_version_not_found() {
        let releases = json!({"1.0.0": []});
        let result = find_download_url_for_version(&releases, "1.0.0", "any-file.tar.gz");
        assert_eq!(result, None);
    }

    #[test]
    fn test_build_sdist_filename() {
        assert_eq!(
            build_sdist_filename("requests", "2.28.0"),
            "requests-2.28.0.tar.gz"
        );
        assert_eq!(
            build_sdist_filename("my-package", "1.0.0"),
            "my-package-1.0.0.tar.gz"
        );
    }

    #[test]
    fn test_end_to_delay_check_with_pypi_releases_format() {
        let now = Utc::now();
        let releases = json!({
            "1.0.0": [
                {"upload_time": (now - Duration::days(100)).to_rfc3339(), "filename": "pkg-1.0.0.tar.gz", "url": "https://example.com/1.tar.gz"}
            ],
            "2.0.0": [
                {"upload_time": (now - Duration::days(50)).to_rfc3339(), "filename": "pkg-2.0.0.tar.gz", "url": "https://example.com/2.tar.gz"}
            ],
            "3.0.0": [
                {"upload_time": (now - Duration::days(1)).to_rfc3339(), "filename": "pkg-3.0.0.tar.gz", "url": "https://example.com/3.tar.gz"}
            ]
        });

        let time_value = build_version_time_info_from_releases(&releases).unwrap();
        let checker = DelayChecker::new(7);
        let time_info = checker.parse_time_field(&time_value).unwrap();

        let result = checker.resolve_version("3.0.0", &time_info).unwrap();
        match result {
            VersionCheckResult::Downgraded {
                ref suggested_version,
                ..
            } => {
                assert_eq!(suggested_version, "2.0.0");
            }
            other => panic!("Expected Downgraded, got {:?}", other),
        }

        let result_old = checker.resolve_version("1.0.0", &time_info).unwrap();
        assert_eq!(result_old, VersionCheckResult::Allowed);
    }

    #[test]
    fn test_all_versions_recent_in_pypi_format() {
        let now = Utc::now();
        let releases = json!({
            "1.0.0": [
                {"upload_time": (now - Duration::days(2)).to_rfc3339(), "filename": "pkg-1.0.0.tar.gz"}
            ],
            "2.0.0": [
                {"upload_time": (now - Duration::days(1)).to_rfc3339(), "filename": "pkg-2.0.0.tar.gz"}
            ]
        });

        let time_value = build_version_time_info_from_releases(&releases).unwrap();
        let checker = DelayChecker::new(30);
        let time_info = checker.parse_time_field(&time_value).unwrap();

        let result = checker.resolve_version("2.0.0", &time_info).unwrap();
        match result {
            VersionCheckResult::Denied { .. } => {}
            other => panic!(
                "Expected Denied when all versions are recent, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_extract_package_info_various_formats() {
        let test_cases = vec![
            ("Django-4.2.5-py3-none-any.whl", "Django", "4.2.5"),
            ("numpy-1.24.3-cp310-cp310-manylinux.whl", "numpy", "1.24.3"),
            ("Pillow-10.0.0.tar.gz", "Pillow", "10.0.0"),
            ("my_package-0.1.0.tar.gz", "my_package", "0.1.0"),
        ];

        for (filename, expected_pkg, expected_ver) in test_cases {
            let result = parse_package_filename(filename);
            assert!(result.is_some(), "Expected Some for {}, got None", filename);
            let (pkg, ver) = result.unwrap();
            assert_eq!(pkg, expected_pkg, "Package mismatch for {}", filename);
            assert_eq!(ver, expected_ver, "Version mismatch for {}", filename);
        }
    }

    #[test]
    fn test_convert_html_to_pep691_json_basic() {
        let html = r#"<html><head><title>Links for django</title></head><body>
<a href="django-4.2.5-py3-none-any.whl#sha256=abc123">django-4.2.5-py3-none-any.whl</a>
<a href="django-4.2.5.tar.gz#sha256=def456">django-4.2.5.tar.gz</a>
</body></html>"#;

        let result = convert_html_to_pep691_json(html, "https://pypi.org/simple/django/");
        assert_eq!(result["name"], "django");
        assert_eq!(result["meta"]["api-version"], "1.0");

        let files = result["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0]["filename"], "django-4.2.5-py3-none-any.whl");
        assert_eq!(files[0]["hashes"]["sha256"], "abc123");
        assert_eq!(files[1]["filename"], "django-4.2.5.tar.gz");
    }

    #[test]
    fn test_convert_html_to_pep691_json_with_yanked() {
        let html = r#"<html><head><title>Links for testpkg</title></head><body>
<a href="testpkg-1.0.0.tar.gz#sha256=aaa" data-yanked>testpkg-1.0.0.tar.gz</a>
<a href="testpkg-2.0.0.tar.gz#sha256=bbb">testpkg-2.0.0.tar.gz</a>
</body></html>"#;

        let result = convert_html_to_pep691_json(html, "https://pypi.org/simple/testpkg/");
        let files = result["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0]["yanked"], true);
        assert_eq!(files[1]["yanked"], false);
    }

    #[test]
    fn test_convert_html_to_pep691_json_with_requires_python() {
        let html = r#"<html><head><title>Links for numpy</title></head><body>
<a href="numpy-1.24.3-cp310-cp310-manylinux.whl#sha256=abc" data-requires-python="&gt;=3.8">numpy-1.24.3-cp310-cp310-manylinux.whl</a>
</body></html>"#;

        let result = convert_html_to_pep691_json(html, "https://pypi.org/simple/numpy/");
        let files = result["files"].as_array().unwrap();
        assert_eq!(files[0]["requires-python"], ">=3.8");
    }

    #[test]
    fn test_convert_html_to_pep691_json_empty_html() {
        let html = "<html><head><title>Links for empty</title></head><body></body></html>";
        let result = convert_html_to_pep691_json(html, "https://pypi.org/simple/empty/");
        assert_eq!(result["files"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_convert_html_to_pep691_json_relative_url() {
        let html = r#"<html><head><title>Links for pkg</title></head><body>
<a href="pkg-1.0.0.tar.gz#sha256=abc">pkg-1.0.0.tar.gz</a>
</body></html>"#;

        let result = convert_html_to_pep691_json(html, "https://files.pythonhosted.org/packages/source/p/pkg/");
        let files = result["files"].as_array().unwrap();
        assert_eq!(
            files[0]["url"],
            "https://files.pythonhosted.org/packages/source/p/pkg/pkg-1.0.0.tar.gz"
        );
    }

    #[test]
    fn test_convert_html_to_pep691_json_absolute_url() {
        let html = r#"<html><head><title>Links for pkg</title></head><body>
<a href="https://example.com/pkg-1.0.0.tar.gz#sha256=abc">pkg-1.0.0.tar.gz</a>
</body></html>"#;

        let result = convert_html_to_pep691_json(html, "https://pypi.org/simple/pkg/");
        let files = result["files"].as_array().unwrap();
        assert_eq!(files[0]["url"], "https://example.com/pkg-1.0.0.tar.gz");
    }

    #[test]
    fn test_filter_recent_versions_html_allows_old() {
        let checker = DelayChecker::new(3);
        let now = Utc::now();
        let old_time = (now - Duration::days(100)).to_rfc3339();

        let time_json = json!({
            "1.0.0": old_time
        });
        let time_info = checker.parse_time_field(&time_json).unwrap();

        let html = r#"<a href="pkg-1.0.0.tar.gz#sha256=abc">pkg-1.0.0.tar.gz</a>"#;
        let (filtered, blocked) = filter_recent_versions_html(html, &time_info, &checker);

        assert!(!filtered.is_empty());
        assert!(blocked.is_empty());
    }

    #[test]
    fn test_filter_recent_versions_html_blocks_recent() {
        let checker = DelayChecker::new(365);
        let now = Utc::now();
        let recent_time = (now - Duration::days(1)).to_rfc3339();

        let time_json = json!({
            "2.0.0": recent_time
        });
        let time_info = checker.parse_time_field(&time_json).unwrap();

        let html = r#"<a href="pkg-2.0.0.tar.gz#sha256=abc">pkg-2.0.0.tar.gz</a>"#;
        let (filtered, blocked) = filter_recent_versions_html(html, &time_info, &checker);

        assert!(filtered.trim().is_empty());
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0], "2.0.0");
    }

    #[test]
    fn test_filter_recent_versions_json_allows_old() {
        let checker = DelayChecker::new(3);
        let now = Utc::now();
        let old_time = (now - Duration::days(100)).to_rfc3339();

        let time_json = json!({
            "1.0.0": old_time
        });
        let time_info = checker.parse_time_field(&time_json).unwrap();

        let json_body = json!({
            "meta": {"api-version": "1.0"},
            "name": "pkg",
            "files": [
                {"filename": "pkg-1.0.0.tar.gz", "url": "https://example.com/pkg-1.0.0.tar.gz"}
            ]
        });

        let (filtered, blocked) = filter_recent_versions_json(json_body, &time_info, &checker);
        assert_eq!(filtered["files"].as_array().unwrap().len(), 1);
        assert!(blocked.is_empty());
    }

    #[test]
    fn test_filter_recent_versions_json_blocks_recent() {
        let checker = DelayChecker::new(365);
        let now = Utc::now();
        let recent_time = (now - Duration::days(1)).to_rfc3339();

        let time_json = json!({
            "2.0.0": recent_time
        });
        let time_info = checker.parse_time_field(&time_json).unwrap();

        let json_body = json!({
            "meta": {"api-version": "1.0"},
            "name": "pkg",
            "files": [
                {"filename": "pkg-2.0.0.tar.gz", "url": "https://example.com/pkg-2.0.0.tar.gz"}
            ]
        });

        let (filtered, blocked) = filter_recent_versions_json(json_body, &time_info, &checker);
        assert_eq!(filtered["files"].as_array().unwrap().len(), 0);
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0], "2.0.0");
    }

    #[test]
    fn test_filter_recent_versions_json_mixed() {
        let checker = DelayChecker::new(30);
        let now = Utc::now();
        let time_json = json!({
            "1.0.0": (now - Duration::days(100)).to_rfc3339(),
            "2.0.0": (now - Duration::days(5)).to_rfc3339()
        });
        let time_info = checker.parse_time_field(&time_json).unwrap();

        let json_body = json!({
            "meta": {"api-version": "1.0"},
            "name": "pkg",
            "files": [
                {"filename": "pkg-1.0.0.tar.gz", "url": "https://example.com/1.tar.gz"},
                {"filename": "pkg-2.0.0.tar.gz", "url": "https://example.com/2.tar.gz"}
            ]
        });

        let (filtered, blocked) = filter_recent_versions_json(json_body, &time_info, &checker);
        assert_eq!(filtered["files"].as_array().unwrap().len(), 1);
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0], "2.0.0");
    }

    #[test]
    fn test_find_sha256_for_file_found() {
        let releases = json!({
            "1.0.0": [
                {
                    "filename": "pkg-1.0.0.tar.gz",
                    "url": "https://example.com/pkg-1.0.0.tar.gz",
                    "sha256_digest": "abc123def456"
                }
            ]
        });
        let result = find_sha256_for_file(&releases, "1.0.0", "pkg-1.0.0.tar.gz");
        assert_eq!(result, Some("abc123def456".to_string()));
    }

    #[test]
    fn test_find_sha256_for_file_not_found() {
        let releases = json!({
            "1.0.0": [
                {
                    "filename": "pkg-1.0.0.tar.gz",
                    "url": "https://example.com/pkg-1.0.0.tar.gz"
                }
            ]
        });
        let result = find_sha256_for_file(&releases, "1.0.0", "pkg-1.0.0.tar.gz");
        assert_eq!(result, None);
    }

    #[test]
    fn test_find_sha256_for_file_no_files() {
        let releases = json!({"1.0.0": []});
        let result = find_sha256_for_file(&releases, "1.0.0", "pkg-1.0.0.tar.gz");
        assert_eq!(result, None);
    }

    #[test]
    fn test_find_sha256_for_file_version_not_found() {
        let releases = json!({
            "1.0.0": [
                {"filename": "pkg-1.0.0.tar.gz", "sha256_digest": "abc"}
            ]
        });
        let result = find_sha256_for_file(&releases, "2.0.0", "pkg-2.0.0.tar.gz");
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_url_and_hashes_for_json_basic() {
        let (url, hashes) = extract_url_and_hashes_for_json(
            "pkg-1.0.0.tar.gz#sha256=abc123&md5=def456",
            "https://files.pythonhosted.org/packages",
        );
        assert_eq!(url, "https://files.pythonhosted.org/packages/pkg-1.0.0.tar.gz");
        assert_eq!(hashes["sha256"], "abc123");
        assert_eq!(hashes["md5"], "def456");
    }

    #[test]
    fn test_extract_url_and_hashes_for_json_no_fragment() {
        let (url, hashes) = extract_url_and_hashes_for_json(
            "pkg-1.0.0.tar.gz",
            "https://files.pythonhosted.org/packages",
        );
        assert_eq!(url, "https://files.pythonhosted.org/packages/pkg-1.0.0.tar.gz");
        assert!(hashes.as_object().unwrap().is_empty());
    }

    #[test]
    fn test_extract_url_and_hashes_for_json_absolute_url() {
        let (url, hashes) = extract_url_and_hashes_for_json(
            "https://example.com/pkg-1.0.0.tar.gz#sha256=abc",
            "https://files.pythonhosted.org/packages",
        );
        assert_eq!(url, "https://example.com/pkg-1.0.0.tar.gz");
        assert_eq!(hashes["sha256"], "abc");
    }

    #[test]
    fn test_extract_attr_value_found() {
        let attrs = r#"data-requires-python=">=3.8" data-yanked"#;
        let result = extract_attr_value(attrs, "data-requires-python");
        assert_eq!(result, Some(">=3.8".to_string()));
    }

    #[test]
    fn test_extract_attr_value_not_found() {
        let attrs = r#"data-yanked"#;
        let result = extract_attr_value(attrs, "data-requires-python");
        assert_eq!(result, None);
    }

    #[test]
    fn test_request_context_new() {
        let ctx = RequestContext::new();
        assert!(ctx.pypi_metadata_cache.is_empty());
    }

    #[test]
    fn test_generate_request_id_format() {
        let id1 = generate_request_id();
        let id2 = generate_request_id();
        assert!(!id1.is_empty());
        assert!(!id2.is_empty());
        assert_ne!(id1, id2);
        // Should be hex
        assert!(id1.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
