use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use worker::*;

use crate::core::{
    Config, DelayAction, DelayChecker, DelayLogEntry, DelayLogger, PackageType, VersionCheckResult,
};

const INFO_MOD_TIMEOUT_MS: u32 = 5000;
const LIST_TIMEOUT_MS: u32 = 10000;
const ZIP_TIMEOUT_MS: u32 = 30000;

const CACHE_TTL_SECS: u64 = 30;

struct CachedInfo {
    body: String,
    fetched_at: Instant,
}

thread_local! {
    static VERSION_CACHE: RefCell<HashMap<String, CachedInfo>> = RefCell::new(HashMap::new());
}

fn new_get_request(url: &str) -> Result<Request> {
    let mut req = Request::new(url, Method::Get)?;
    req.headers_mut()?
        .set("User-Agent", "delay-mirror/1.0 (Cloudflare Worker)")?;
    Ok(req)
}

async fn fetch_upstream_with_timeout(url: &str, timeout_ms: u32) -> Result<worker::Response> {
    use wasm_bindgen::{prelude::Closure, JsCast};

    let controller = AbortController::default();
    let signal = controller.signal();

    let req = new_get_request(url)?;

    let global: web_sys::WorkerGlobalScope = js_sys::global().unchecked_into();
    let _timeout_id = {
        let closure = Closure::once(move || {
            controller.abort();
        });
        global.set_timeout_with_callback_and_timeout_and_arguments_0(
            closure.as_ref().unchecked_ref(),
            timeout_ms as i32,
        )?
    };

    Ok(Fetch::Request(req).send_with_signal(&signal).await?)
}

#[derive(Debug, Deserialize, Serialize)]
#[allow(non_snake_case)]
struct GoModVersionInfo {
    Version: String,
    Time: String,
}

#[derive(Debug)]
enum DelayCheckResult {
    Allowed { info_body: String },
    Denied { publish_time: DateTime<Utc> },
    NotFound,
    UpstreamError(u16),
}

#[derive(Clone, Copy)]
pub enum GomodEndpoint {
    List,
    Latest,
    Info,
    Mod,
    Zip,
}

impl GomodEndpoint {
    pub fn extension(&self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Latest => "latest",
            Self::Info => ".info",
            Self::Mod => ".mod",
            Self::Zip => ".zip",
        }
    }

    pub fn content_type(&self) -> &'static str {
        match self {
            Self::List | Self::Mod => "text/plain; charset=utf-8",
            Self::Latest | Self::Info => "application/json",
            Self::Zip => "application/octet-stream",
        }
    }

    pub fn needs_delay_check(&self) -> bool {
        !matches!(self, Self::List)
    }

    pub fn should_stream(&self) -> bool {
        matches!(self, Self::Zip)
    }

    pub fn uses_download_registry(&self) -> bool {
        matches!(self, Self::Zip)
    }
}

pub fn escape_module_path(module: &str) -> String {
    let mut result = String::with_capacity(module.len());
    let mut prev_is_lower = false;
    let mut prev_upper_char = None;
    for c in module.chars() {
        if c.is_uppercase() {
            let cl = c.to_ascii_lowercase();
            if !prev_is_lower && Some(cl) != prev_upper_char {
                result.push('!');
            }
            result.push(cl);
            prev_is_lower = false;
            prev_upper_char = Some(cl);
        } else {
            result.push(c);
            prev_is_lower = c.is_ascii_lowercase();
            prev_upper_char = None;
        }
    }
    result
}

fn parse_version_time(time_str: &str) -> Result<DateTime<Utc>> {
    match time_str.parse::<DateTime<Utc>>() {
        Ok(dt) => Ok(dt),
        Err(_) => Err("Invalid timestamp format".into()),
    }
}

#[allow(dead_code)]
fn extract_reject_time(result: &VersionCheckResult) -> Option<&DateTime<Utc>> {
    match result {
        VersionCheckResult::Denied { publish_time } => Some(publish_time),
        VersionCheckResult::Downgraded { original_time, .. } => Some(original_time),
        _ => None,
    }
}

fn extract_pseudo_version_time(version: &str) -> Option<DateTime<Utc>> {
    let parts: Vec<&str> = version.split('-').collect();
    if parts.len() < 3 {
        return None;
    }
    let timestamp_str = parts[parts.len() - 2];
    if timestamp_str.len() != 14 || !timestamp_str.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let rfc3339 = format!(
        "{}-{}-{}T{}:{}:{}Z",
        &timestamp_str[0..4],
        &timestamp_str[4..6],
        &timestamp_str[6..8],
        &timestamp_str[8..10],
        &timestamp_str[10..12],
        &timestamp_str[12..14]
    );
    rfc3339.parse::<DateTime<Utc>>().ok()
}

async fn smart_check_version(
    module: &str,
    version: &str,
    config: &Config,
    checker: &DelayChecker,
) -> Result<DelayCheckResult> {
    if let Some(pseudo_time) = extract_pseudo_version_time(version) {
        return Ok(if checker.is_version_allowed(&pseudo_time) {
            DelayCheckResult::Allowed {
                info_body: String::new(),
            }
        } else {
            DelayCheckResult::Denied {
                publish_time: pseudo_time,
            }
        });
    }
    check_version_with_delay_cached(module, version, config, checker).await
}

fn build_forbidden_response(
    module: &str,
    version: &str,
    reject_time: &DateTime<Utc>,
    delay_checker: &DelayChecker,
    logger: &DelayLogger,
    client_ip: Option<&str>,
) -> Result<Response> {
    let reason = format!(
        "Version was published within the last {} day(s)",
        delay_checker.delay_days()
    );

    logger.log_blocked(PackageType::GoMod, module, version, &reason, client_ip);

    let body = serde_json::json!({
        "error": "Version too recent for access",
        "module": module,
        "requested_version": version,
        "reason": reason,
        "publish_time": reject_time.to_rfc3339(),
        "suggestion": "Try again later or use an older version"
    });

    let mut headers = Headers::new();
    headers.set("X-Delay-Original-Version", version)?;
    headers.set(
        "X-Delay-Reason",
        "Version published too recently, below delay threshold",
    )?;
    headers.set("X-Delay-Publish-Time", &reject_time.to_rfc3339())?;
    headers.set("Content-Type", "application/json")?;

    Ok(Response::error(body.to_string(), 404)?.with_headers(headers))
}

fn not_found_json(module: &str, version: &str) -> String {
    serde_json::json!({
        "error": "Version not found",
        "module": module,
        "version": version
    })
    .to_string()
}

fn upstream_error_json(status: u16) -> String {
    serde_json::json!({
        "error": "Upstream registry error",
        "status": status
    })
    .to_string()
}

fn extract_client_ip(req: &Request) -> Option<String> {
    req.headers()
        .get("CF-Connecting-IP")
        .or_else(|_| req.headers().get("X-Forwarded-For"))
        .ok()
        .flatten()
}

async fn get_or_fetch_info(module: &str, version: &str, config: &Config) -> Result<Option<String>> {
    let cache_key = format!("{}@{}", module, version);

    let cached_body = VERSION_CACHE.with(|cache| {
        let cache = cache.borrow();
        cache.get(&cache_key).and_then(|entry| {
            if entry.fetched_at.elapsed() < Duration::from_secs(CACHE_TTL_SECS) {
                Some(entry.body.clone())
            } else {
                None
            }
        })
    });

    if let Some(body) = cached_body {
        return Ok(Some(body));
    }

    let escaped_module = escape_module_path(module);
    let url = config.gomod_meta_url(&escaped_module, &format!("/@v/{}.info", version));

    let req = new_get_request(&url)?;
    let mut resp = Fetch::Request(req).send().await?;

    if resp.status_code() == 404 {
        return Ok(None);
    }
    if resp.status_code() < 200 || resp.status_code() >= 300 {
        return Err(format!("Upstream error: {}", resp.status_code()).into());
    }

    let body = resp.text().await?;

    VERSION_CACHE.with(|cache| {
        cache.borrow_mut().insert(
            cache_key,
            CachedInfo {
                body: body.clone(),
                fetched_at: Instant::now(),
            },
        );
    });

    Ok(Some(body))
}

async fn check_version_with_delay_cached(
    module: &str,
    version: &str,
    config: &Config,
    delay_checker: &DelayChecker,
) -> Result<DelayCheckResult> {
    let body = match get_or_fetch_info(module, version, config).await? {
        Some(b) => b,
        None => return Ok(DelayCheckResult::NotFound),
    };

    let version_info: GoModVersionInfo = match serde_json::from_str(&body) {
        Ok(info) => info,
        Err(e) => {
            console_error!("Failed to parse version info JSON: {}", e);
            return Err("Failed to parse version info JSON".into());
        }
    };

    let publish_time = parse_version_time(&version_info.Time)?;

    if delay_checker.is_version_allowed(&publish_time) {
        Ok(DelayCheckResult::Allowed { info_body: body })
    } else {
        Ok(DelayCheckResult::Denied { publish_time })
    }
}

fn parse_gomod_path(path: &str, endpoint: GomodEndpoint) -> Result<(String, String)> {
    let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if path_parts.len() < 5 {
        return Err("Invalid Go module path format".into());
    }

    let module = path_parts[1..path_parts.len() - 2].join("/");
    let last_part = path_parts.last().unwrap_or(&"");

    let version = match endpoint {
        GomodEndpoint::List | GomodEndpoint::Latest => endpoint.extension().to_string(),
        _ => last_part
            .trim_end_matches(".info")
            .trim_end_matches(".mod")
            .trim_end_matches(".zip")
            .to_string(),
    };

    Ok((module, version))
}

fn build_upstream_url(
    module: &str,
    version: &str,
    config: &Config,
    endpoint: GomodEndpoint,
) -> String {
    let escaped_module = escape_module_path(module);
    let path_suffix = format!("/@v/{}{}", version, endpoint.extension());

    if endpoint.uses_download_registry() {
        config.gomod_download_url(&escaped_module, &path_suffix)
    } else {
        config.gomod_meta_url(&escaped_module, &path_suffix)
    }
}

async fn fetch_and_respond(
    url: &str,
    endpoint: GomodEndpoint,
    cached_body: Option<String>,
) -> Result<Response> {
    let mut headers = Headers::new();
    headers.set("Content-Type", endpoint.content_type())?;

    if let Some(body) = cached_body {
        return Ok(Response::ok(body)?.with_headers(headers));
    }

    let timeout_ms = match endpoint {
        GomodEndpoint::Zip => ZIP_TIMEOUT_MS,
        GomodEndpoint::List => LIST_TIMEOUT_MS,
        _ => INFO_MOD_TIMEOUT_MS,
    };

    let stream = endpoint.should_stream();
    let mut resp = fetch_upstream_with_timeout(url, timeout_ms).await?;

    if stream {
        let mut headers = Headers::new();
        headers.set("Content-Type", endpoint.content_type())?;
        Ok(resp.with_headers(headers))
    } else {
        let body = resp.text().await?;
        Ok(Response::ok(body)?.with_headers(headers))
    }
}

async fn try_fetch_zip(url: &str) -> Result<worker::Response> {
    fetch_upstream_with_timeout(url, ZIP_TIMEOUT_MS).await
}

async fn fetch_zip_with_fallback(module: &str, version: &str, config: &Config) -> Result<Response> {
    let escaped_module = escape_module_path(module);
    let path_suffix = format!("/@v/{}.zip", version);

    let primary_url = config.gomod_download_url(&escaped_module, &path_suffix);
    let fallback_url = config.gomod_meta_url(&escaped_module, &path_suffix);

    match try_fetch_zip(&primary_url).await {
        Ok(resp) if resp.status_code() == 200 => {
            let mut headers = Headers::new();
            headers.set("Content-Type", "application/octet-stream")?;
            Ok(resp.with_headers(headers))
        }
        _ => {
            console_error!("Primary download source failed, trying fallback");
            match try_fetch_zip(&fallback_url).await {
                Ok(resp) if resp.status_code() == 200 => {
                    let mut headers = Headers::new();
                    headers.set("Content-Type", "application/octet-stream")?;
                    Ok(resp.with_headers(headers))
                }
                Ok(resp) => {
                    Response::error(upstream_error_json(resp.status_code()), resp.status_code())
                }
                Err(e) => Response::error(format!("{{\"error\":\"{}\"}}", e), 502),
            }
        }
    }
}

async fn handle_list_endpoint(
    module: &str,
    config: &Config,
    checker: &DelayChecker,
) -> Result<Response> {
    let escaped_module = escape_module_path(module);
    let url = config.gomod_meta_url(&escaped_module, "/@v/list");

    let mut resp = fetch_upstream_with_timeout(&url, LIST_TIMEOUT_MS).await?;

    let status_code = resp.status_code();
    if status_code == 404 {
        return Response::error(not_found_json(module, "list"), 404);
    }
    if status_code < 200 || status_code >= 300 {
        return Response::error(upstream_error_json(status_code), 502);
    }

    let raw_body = resp.text().await?;

    let versions: Vec<&str> = raw_body.lines().collect();
    let mut allowed_versions = Vec::new();
    let logger = DelayLogger::new();

    for version in versions {
        match smart_check_version(module, version, config, checker).await {
            Ok(DelayCheckResult::Allowed { .. }) => {
                allowed_versions.push(version.to_string());
            }
            Ok(DelayCheckResult::Denied { publish_time: _ }) => {
                logger.log_blocked(
                    PackageType::GoMod,
                    module,
                    version,
                    &format!(
                        "Version was published within the last {} day(s)",
                        checker.delay_days()
                    ),
                    None,
                );
            }
            _ => {
                allowed_versions.push(version.to_string());
            }
        }
    }

    let filtered_body = allowed_versions.join("\n");

    let mut headers = Headers::new();
    headers.set("Content-Type", "text/plain; charset=utf-8")?;
    headers.set(
        "X-Delay-Warning",
        "Go Modules list endpoint has been filtered by delay policy. Some recent versions may be hidden.",
    )?;
    Ok(Response::ok(filtered_body)?.with_headers(headers))
}

pub async fn handle_gomod_request(
    req: Request,
    config: &Config,
    checker: &DelayChecker,
    endpoint: GomodEndpoint,
) -> Result<Response> {
    let logger = DelayLogger::new();
    let client_ip = extract_client_ip(&req);

    let (module, version) = parse_gomod_path(&req.path(), endpoint)?;

    if matches!(endpoint, GomodEndpoint::List) {
        return handle_list_endpoint(&module, config, checker).await;
    }

    match smart_check_version(&module, &version, config, checker).await? {
        DelayCheckResult::Allowed { info_body } => {
            logger.log(&DelayLogEntry::new(
                PackageType::GoMod,
                module.clone(),
                version.to_string(),
                None,
                DelayAction::Allowed,
                "Version passed delay check".to_string(),
                client_ip,
            ));

            if matches!(endpoint, GomodEndpoint::Zip) {
                return fetch_zip_with_fallback(&module, &version, config).await;
            }

            let upstream_url = build_upstream_url(&module, &version, config, endpoint);
            let cached = if info_body.is_empty() {
                None
            } else {
                Some(info_body)
            };
            fetch_and_respond(&upstream_url, endpoint, cached).await
        }
        DelayCheckResult::Denied { publish_time } => build_forbidden_response(
            &module,
            &version,
            &publish_time,
            checker,
            &logger,
            client_ip.as_deref(),
        ),
        DelayCheckResult::NotFound => Response::error(not_found_json(&module, &version), 404),
        DelayCheckResult::UpstreamError(status) => {
            Response::error(upstream_error_json(status), status)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Timelike};

    #[test]
    fn test_parse_version_time_valid() {
        let result = parse_version_time("2024-01-01T00:00:00Z");
        assert!(result.is_ok());
        let dt = result.unwrap();
        assert_eq!(dt.year(), 2024);
        assert_eq!(dt.month(), 1);
        assert_eq!(dt.day(), 1);
    }

    #[test]
    fn test_parse_version_time_invalid() {
        let result = parse_version_time("not-a-timestamp");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_version_time_rfc3339() {
        let result = parse_version_time("2024-06-15T10:30:45+08:00");
        assert!(result.is_ok());
    }

    #[test]
    fn test_go_mod_version_info_deserialization() {
        let json = r#"{"Version":"v1.2.3","Time":"2024-03-15T12:00:00Z"}"#;
        let info: GoModVersionInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.Version, "v1.2.3");
        assert_eq!(info.Time, "2024-03-15T12:00:00Z");
    }

    #[test]
    fn test_go_mod_version_info_with_prerelease() {
        let json = r#"{"Version":"v1.0.0-beta.1","Time":"2024-01-01T00:00:00Z"}"#;
        let info: GoModVersionInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.Version, "v1.0.0-beta.1");
    }

    #[test]
    fn test_go_mod_version_info_pseudo_version() {
        let json =
            r#"{"Version":"v0.0.0-20240101120000-abc123def456","Time":"2024-01-01T12:00:00Z"}"#;
        let info: GoModVersionInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.Version, "v0.0.0-20240101120000-abc123def456");
        assert_eq!(info.Time, "2024-01-01T12:00:00Z");
    }

    #[test]
    fn test_escape_module_path_basic() {
        assert_eq!(
            escape_module_path("github.com/gin-gonic/gin"),
            "github.com/gin-gonic/gin"
        );
    }

    #[test]
    fn test_escape_module_path_uppercase() {
        assert_eq!(
            escape_module_path("github.com/Google/uuid"),
            "github.com/!google/uuid"
        );
    }

    #[test]
    fn test_escape_module_path_multiple_uppercase() {
        assert_eq!(
            escape_module_path("github.com/MyOrg/MyRepo"),
            "github.com/!myorg/!myrepo"
        );
    }

    #[test]
    fn test_escape_module_path_empty() {
        let result = escape_module_path("");
        assert!(result.is_empty());
    }

    #[test]
    fn test_escape_module_path_all_uppercase() {
        assert_eq!(escape_module_path("GOOGLE"), "!g!oo!g!l!e");
    }

    #[test]
    fn test_escape_module_path_mixed_case() {
        assert_eq!(escape_module_path("Gin-Gonic"), "!gin-!gonic");
    }

    #[test]
    fn test_escape_module_path_special_chars() {
        assert_eq!(
            escape_module_path("github.com/user-name/repo_name"),
            "github.com/user-name/repo_name"
        );
    }

    #[test]
    fn test_escape_module_path_numbers() {
        assert_eq!(
            escape_module_path("github.com/v2ray/v2ray-core"),
            "github.com/v2ray/v2ray-core"
        );
    }

    #[test]
    fn test_escape_module_path_single_char() {
        assert_eq!(escape_module_path("a"), "a");
        assert_eq!(escape_module_path("A"), "!a");
    }

    #[test]
    fn test_escape_module_path_single_lowercase() {
        assert_eq!(escape_module_path("g"), "g");
    }

    #[test]
    fn test_escape_module_path_consecutive_uppercase() {
        assert_eq!(
            escape_module_path("github.com/AABBCC/Test"),
            "github.com/!aa!bb!cc/!test"
        );
    }

    #[test]
    fn test_escape_module_path_with_numbers_and_uppercase() {
        assert_eq!(
            escape_module_path("github.com/V2Ray/Core"),
            "github.com/!v2!ray/!core"
        );
    }

    #[test]
    fn test_escape_module_path_preserves_hyphens() {
        assert_eq!(
            escape_module_path("github.com/my-org/my-repo"),
            "github.com/my-org/my-repo"
        );
    }

    #[test]
    fn test_escape_module_path_preserves_underscores() {
        assert_eq!(
            escape_module_path("github.com/my_org/my_repo"),
            "github.com/my_org/my_repo"
        );
    }

    #[test]
    fn test_escape_module_path_non_empty_output() {
        let module = "some/module/path";
        assert!(!escape_module_path(module).is_empty());
    }

    #[test]
    fn test_escape_module_path_idempotent_for_lowercase() {
        let input = "github.com/user/repo";
        let first = escape_module_path(input);
        let second = escape_module_path(&first);
        assert_eq!(first, second);
    }

    #[test]
    fn test_extract_reject_time_denied() {
        let time: DateTime<Utc> = "2024-06-15T00:00:00Z".parse().unwrap();
        let result = VersionCheckResult::Denied { publish_time: time };
        let extracted = extract_reject_time(&result).unwrap();
        assert_eq!(*extracted, time);
    }

    #[test]
    fn test_extract_reject_time_downgraded() {
        let orig_time: DateTime<Utc> = "2024-06-15T00:00:00Z".parse().unwrap();
        let sug_time: DateTime<Utc> = "2024-01-01T00:00:00Z".parse().unwrap();
        let result = VersionCheckResult::Downgraded {
            original_version: "v2.0.0".to_string(),
            suggested_version: "v1.0.0".to_string(),
            original_time: orig_time,
            suggested_time: sug_time,
        };
        let extracted = extract_reject_time(&result).unwrap();
        assert_eq!(*extracted, orig_time);
    }

    #[test]
    fn test_extract_reject_time_allowed() {
        let result = VersionCheckResult::Allowed;
        assert!(extract_reject_time(&result).is_none());
    }

    #[test]
    fn test_go_mod_version_info_serialization_roundtrip() {
        let original = GoModVersionInfo {
            Version: "v2.0.0".to_string(),
            Time: "2025-04-02T10:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let restored: GoModVersionInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(original.Version, restored.Version);
        assert_eq!(original.Time, restored.Time);
    }

    #[test]
    fn test_parse_various_timestamp_formats() {
        let valid_timestamps = vec![
            "2024-01-01T00:00:00Z",
            "2024-06-15T10:30:00+00:00",
            "2024-12-31T23:59:59-05:00",
        ];

        for ts in valid_timestamps {
            assert!(
                parse_version_time(ts).is_ok(),
                "Should parse timestamp: {}",
                ts
            );
        }
    }

    #[test]
    fn test_version_zip_trim() {
        let version = "v1.0.0.zip";
        let cleaned = version.trim_end_matches(".zip");
        assert_eq!(cleaned, "v1.0.0");

        let version_no_ext = "v1.0.0";
        let cleaned_no_ext = version_no_ext.trim_end_matches(".zip");
        assert_eq!(cleaned_no_ext, "v1.0.0");
    }

    #[test]
    fn test_version_info_trim() {
        let version = "v1.0.0.info";
        let cleaned = version.trim_end_matches(".info");
        assert_eq!(cleaned, "v1.0.0");
    }

    #[test]
    fn test_version_mod_trim() {
        let version = "v1.0.0.mod";
        let cleaned = version.trim_end_matches(".mod");
        assert_eq!(cleaned, "v1.0.0");
    }

    #[test]
    fn test_extract_pseudo_version_time_valid() {
        let cases = vec![
            ("v0.0.0-20240101120000-abc123def456", 2024, 1, 1, 12, 0, 0),
            ("v0.0.0-20230102150405-a1b2c3d4e5f6", 2023, 1, 2, 15, 4, 5),
            ("v1.2.3-0.20240101120000-abc123def456", 2024, 1, 1, 12, 0, 0),
            (
                "v2.0.0-20251231235959-abcdef123456",
                2025,
                12,
                31,
                23,
                59,
                59,
            ),
        ];
        for (version, y, mo, d, h, mi, s) in cases {
            let dt = extract_pseudo_version_time(version)
                .unwrap_or_else(|| panic!("Should parse: {}", version));
            assert_eq!(dt.year(), y, "year mismatch for {}", version);
            assert_eq!(dt.month() as i32, mo, "month mismatch for {}", version);
            assert_eq!(dt.day(), d, "day mismatch for {}", version);
            assert_eq!(dt.hour(), h, "hour mismatch for {}", version);
            assert_eq!(dt.minute(), mi, "minute mismatch for {}", version);
            assert_eq!(dt.second(), s, "second mismatch for {}", version);
        }
    }

    #[test]
    fn test_extract_pseudo_version_time_invalid_formats() {
        let invalid = vec!["v1.0.0", "v1.0.0-beta", "v1.0.0-beta.1", "v1.0.0+build"];
        for version in invalid {
            assert!(
                extract_pseudo_version_time(version).is_none(),
                "Should return None for: {}",
                version
            );
        }
    }

    #[test]
    fn test_extract_pseudo_version_time_edge_cases() {
        assert!(
            extract_pseudo_version_time("v0.0.0-20240101-abc123").is_none(),
            "Short timestamp (8 digits) should be None"
        );
        assert!(
            extract_pseudo_version_time("v0.0.0-2024010112000-abc123").is_none(),
            "13-digit timestamp should be None"
        );
        assert!(
            extract_pseudo_version_time("v0.0.0-202401011200000-abc123").is_none(),
            "15-digit timestamp should be None"
        );
        assert!(
            extract_pseudo_version_time("v0.0.0-2024a101120000-abc123").is_none(),
            "Timestamp with letter should be None"
        );
        assert!(
            extract_pseudo_version_time("v0.0.0-20240101120000").is_none(),
            "Only 2 parts after split should be None"
        );
    }

    #[test]
    fn test_gomod_endpoint_extension() {
        assert_eq!(GomodEndpoint::List.extension(), "list");
        assert_eq!(GomodEndpoint::Latest.extension(), "latest");
        assert_eq!(GomodEndpoint::Info.extension(), ".info");
        assert_eq!(GomodEndpoint::Mod.extension(), ".mod");
        assert_eq!(GomodEndpoint::Zip.extension(), ".zip");
    }

    #[test]
    fn test_gomod_endpoint_content_type() {
        assert_eq!(
            GomodEndpoint::List.content_type(),
            "text/plain; charset=utf-8"
        );
        assert_eq!(GomodEndpoint::Latest.content_type(), "application/json");
        assert_eq!(GomodEndpoint::Info.content_type(), "application/json");
        assert_eq!(
            GomodEndpoint::Mod.content_type(),
            "text/plain; charset=utf-8"
        );
        assert_eq!(GomodEndpoint::Zip.content_type(), "application/octet-stream");
    }

    #[test]
    fn test_gomod_endpoint_needs_delay_check() {
        assert!(!GomodEndpoint::List.needs_delay_check());
        assert!(GomodEndpoint::Latest.needs_delay_check());
        assert!(GomodEndpoint::Info.needs_delay_check());
        assert!(GomodEndpoint::Mod.needs_delay_check());
        assert!(GomodEndpoint::Zip.needs_delay_check());
    }

    #[test]
    fn test_gomod_endpoint_should_stream() {
        assert!(!GomodEndpoint::List.should_stream());
        assert!(!GomodEndpoint::Latest.should_stream());
        assert!(!GomodEndpoint::Info.should_stream());
        assert!(!GomodEndpoint::Mod.should_stream());
        assert!(GomodEndpoint::Zip.should_stream());
    }

    #[test]
    fn test_gomod_endpoint_uses_download_registry() {
        assert!(!GomodEndpoint::List.uses_download_registry());
        assert!(!GomodEndpoint::Latest.uses_download_registry());
        assert!(!GomodEndpoint::Info.uses_download_registry());
        assert!(!GomodEndpoint::Mod.uses_download_registry());
        assert!(GomodEndpoint::Zip.uses_download_registry());
    }

    #[test]
    fn test_escape_module_path_preserves_digits() {
        assert_eq!(
            escape_module_path("github.com/v2ray/v2ray-core"),
            "github.com/v2ray/v2ray-core"
        );
    }

    #[test]
    fn test_escape_module_path_leading_uppercase() {
        assert_eq!(escape_module_path("A"), "!a");
        assert_eq!(escape_module_path("AB"), "!a!b");
    }

    #[test]
    fn test_escape_module_path_trailing_uppercase() {
        assert_eq!(escape_module_path("testA"), "test!a");
    }

    #[test]
    fn test_not_found_json_format() {
        let json_str = not_found_json("github.com/foo/bar", "v1.0.0");
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["error"], "Version not found");
        assert_eq!(parsed["module"], "github.com/foo/bar");
        assert_eq!(parsed["version"], "v1.0.0");
    }

    #[test]
    fn test_upstream_error_json_format() {
        let json_str = upstream_error_json(502);
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["error"], "Upstream registry error");
        assert_eq!(parsed["status"], 502);
    }
}
