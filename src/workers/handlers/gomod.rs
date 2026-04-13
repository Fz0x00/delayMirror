use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use worker::*;

use crate::core::{
    Config, DelayAction, DelayChecker, DelayLogEntry, DelayLogger, MetadataCache, PackageType, VersionCheckResult,
};

fn new_get_request(url: &str) -> Result<Request> {
    let mut req = Request::new(url, Method::Get)?;
    req.headers_mut()?
        .set("User-Agent", "delay-mirror/1.0 (Cloudflare Worker)")?;
    Ok(req)
}

#[derive(Debug, Deserialize, Serialize)]
#[allow(non_snake_case)]
struct GoModVersionInfo {
    Version: String,
    Time: String,
}

#[derive(Debug)]
enum DelayCheckOutcome {
    Allowed,
    Denied { publish_time: DateTime<Utc> },
    NotFound,
    UpstreamError(u16),
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

    Ok(Response::error(body.to_string(), 403)?.with_headers(headers))
}

fn extract_client_ip(req: &Request) -> Option<String> {
    req.headers()
        .get("CF-Connecting-IP")
        .or_else(|_| req.headers().get("X-Forwarded-For"))
        .ok()
        .flatten()
}

async fn check_version_with_delay(
    module: &str,
    version: &str,
    config: &Config,
    delay_checker: &DelayChecker,
    cache: Option<&dyn MetadataCache>,
) -> Result<DelayCheckOutcome> {
    // 构建缓存键
    let cache_key = format!("gomod:{module}:{version}");
    
    // 检查缓存
    if let Some(cache) = cache {
        if let Ok(true) = cache.is_valid(&cache_key, 24) { // 24小时缓存
            if let Ok(Some(metadata)) = cache.get(&cache_key) {
                if let Some(publish_time_str) = metadata.metadata.get("time").and_then(|t| t.as_str()) {
                    if let Ok(publish_time) = parse_version_time(publish_time_str) {
                        if delay_checker.is_version_allowed(&publish_time) {
                            return Ok(DelayCheckOutcome::Allowed);
                        } else {
                            return Ok(DelayCheckOutcome::Denied { publish_time });
                        }
                    }
                }
            }
        }
    }
    
    let escaped_module = escape_module_path(module);
    let url = format!(
        "{}/{}/@v/{}.info",
        config.gomod_registry.trim_end_matches('/'),
        escaped_module,
        version
    );

    let req = new_get_request(&url)?;
    let mut resp = match Fetch::Request(req).send().await {
        Ok(r) => r,
        Err(e) => {
            console_error!("Failed to fetch version info from upstream: {}", e);
            return Err("Failed to fetch version info from upstream registry".into());
        }
    };

    if resp.status_code() == 404 {
        return Ok(DelayCheckOutcome::NotFound);
    }

    if resp.status_code() < 200 || resp.status_code() >= 300 {
        return Ok(DelayCheckOutcome::UpstreamError(resp.status_code()));
    }

    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => {
            console_error!("Failed to read response body: {}", e);
            return Err("Failed to read upstream response body".into());
        }
    };

    let version_info: GoModVersionInfo = match serde_json::from_str(&body) {
        Ok(info) => info,
        Err(e) => {
            console_error!("Failed to parse version info JSON: {}", e);
            return Err("Failed to parse version info JSON".into());
        }
    };

    let publish_time = parse_version_time(&version_info.Time)?;
    
    // 存储到缓存
    if let Some(cache) = cache {
        let cache_data = serde_json::json!({
            "version": version_info.Version,
            "time": version_info.Time
        });
        let _ = cache.set(&cache_key, &cache_data);
    }

    if delay_checker.is_version_allowed(&publish_time) {
        Ok(DelayCheckOutcome::Allowed)
    } else {
        Ok(DelayCheckOutcome::Denied { publish_time })
    }
}

pub async fn handle_gomod_version_list(req: Request, config: &Config) -> Result<Response> {
    let path = req.path();
    let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    if path_parts.len() < 3 || path_parts[0] != "gomod" || path_parts.last() != Some(&"list") {
        return Response::error("Invalid request path", 400);
    }

    let module = path_parts[1..path_parts.len() - 2].join("/");

    let escaped_module = escape_module_path(&module);
    let url = format!(
        "{}/{}/@v/list",
        config.gomod_registry.trim_end_matches('/'),
        escaped_module
    );

    let upstream_req = new_get_request(&url)?;
    let mut resp = match Fetch::Request(upstream_req).send().await {
        Ok(r) => r,
        Err(e) => {
            console_error!("Failed to connect to Go modules registry: {}", e);
            return Err("Failed to connect to upstream Go modules registry".into());
        }
    };

    if resp.status_code() == 404 {
        return Response::error(
            serde_json::json!({
                "error": "Module not found",
                "module": module
            })
            .to_string(),
            404,
        );
    }

    if resp.status_code() < 200 || resp.status_code() >= 300 {
        return Response::error(
            serde_json::json!({
                "error": "Upstream registry error",
                "status": resp.status_code()
            })
            .to_string(),
            502,
        );
    }

    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => {
            console_error!("Failed to read response body: {}", e);
            return Err("Failed to read upstream response body".into());
        }
    };

    let mut headers = Headers::new();
    headers.set("X-Delay-Mode", "whitelist-fallback")?;
    headers.set(
        "X-Delay-Warning",
        "Go Modules list endpoint does not provide version timestamps. Use individual version endpoints for delay checking.",
    )?;
    headers.set("Content-Type", "text/plain; charset=utf-8")?;

    Ok(Response::ok(body)?.with_headers(headers))
}

pub async fn handle_gomod_version_info(
    req: Request,
    config: &Config,
    checker: &DelayChecker,
    cache: Option<&dyn MetadataCache>,
) -> Result<Response> {
    let logger = DelayLogger::new();
    let client_ip = extract_client_ip(&req);

    let path = req.path();
    let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    if path_parts.len() < 5 || path_parts[0] != "gomod" {
        return Response::error("Invalid request path", 400);
    }

    let module = path_parts[1..path_parts.len() - 3].join("/");
    let version_with_ext = path_parts[path_parts.len() - 2];
    let version = version_with_ext.trim_end_matches(".info");

    match check_version_with_delay(&module, version, config, checker, cache).await? {
        DelayCheckOutcome::Allowed => {
            logger.log(&DelayLogEntry::new(
                PackageType::GoMod,
                module.clone(),
                version.to_string(),
                None,
                DelayAction::Allowed,
                "Version passed delay check".to_string(),
                client_ip.clone(),
            ));

            let escaped_module = escape_module_path(&module);
            let url = format!(
                "{}/{}/@v/{}.info",
                config.gomod_registry.trim_end_matches('/'),
                escaped_module,
                version
            );

            let upstream_req = new_get_request(&url)?;
            let mut resp = match Fetch::Request(upstream_req).send().await {
                Ok(r) => r,
                Err(e) => {
                    console_error!("Failed to fetch version info: {}", e);
                    return Err("Failed to fetch version info from upstream".into());
                }
            };

            let body = match resp.text().await {
                Ok(b) => b,
                Err(e) => {
                    console_error!("Failed to read response body: {}", e);
                    return Err("Failed to read upstream response body".into());
                }
            };

            let mut headers = Headers::new();
            headers.set("Content-Type", "application/json")?;
            Ok(Response::ok(body)?.with_headers(headers))
        }
        DelayCheckOutcome::Denied { ref publish_time } => build_forbidden_response(
            &module,
            version,
            publish_time,
            checker,
            &logger,
            client_ip.as_deref(),
        ),
        DelayCheckOutcome::NotFound => Response::error(
            serde_json::json!({
                "error": "Version not found",
                "module": module,
                "version": version
            })
            .to_string(),
            404,
        ),
        DelayCheckOutcome::UpstreamError(status) => Response::error(
            serde_json::json!({
                "error": "Upstream registry error",
                "status": status
            })
            .to_string(),
            502,
        ),
    }
}

pub async fn handle_gomod_go_mod(
    req: Request,
    config: &Config,
    checker: &DelayChecker,
    cache: Option<&dyn MetadataCache>,
) -> Result<Response> {
    let logger = DelayLogger::new();
    let client_ip = extract_client_ip(&req);

    let path = req.path();
    let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    if path_parts.len() < 5 || path_parts[0] != "gomod" {
        return Response::error("Invalid request path", 400);
    }

    let module = path_parts[1..path_parts.len() - 3].join("/");
    let version_with_ext = path_parts[path_parts.len() - 2];
    let version = version_with_ext.trim_end_matches(".mod");

    match check_version_with_delay(&module, version, config, checker, cache).await? {
        DelayCheckOutcome::Allowed => {
            logger.log(&DelayLogEntry::new(
                PackageType::GoMod,
                module.clone(),
                version.to_string(),
                None,
                DelayAction::Allowed,
                "go.mod access passed delay check".to_string(),
                client_ip.clone(),
            ));

            let escaped_module = escape_module_path(&module);
            let url = format!(
                "{}/{}/@v/{}.mod",
                config.gomod_registry.trim_end_matches('/'),
                escaped_module,
                version
            );

            let upstream_req = new_get_request(&url)?;
            let mut resp = match Fetch::Request(upstream_req).send().await {
                Ok(r) => r,
                Err(e) => {
                    console_error!("Failed to fetch go.mod: {}", e);
                    return Err("Failed to fetch go.mod from upstream".into());
                }
            };

            if resp.status_code() == 404 {
                return Response::error(
                    serde_json::json!({
                        "error": "Module version not found",
                        "module": module,
                        "version": version
                    })
                    .to_string(),
                    404,
                );
            }

            if resp.status_code() < 200 || resp.status_code() >= 300 {
                return Response::error(
                    serde_json::json!({
                        "error": "Upstream registry error",
                        "status": resp.status_code()
                    })
                    .to_string(),
                    502,
                );
            }

            let body = match resp.text().await {
                Ok(b) => b,
                Err(e) => {
                    console_error!("Failed to read response body: {}", e);
                    return Err("Failed to read upstream response body".into());
                }
            };

            let mut headers = Headers::new();
            headers.set("Content-Type", "text/plain; charset=utf-8")?;
            Ok(Response::ok(body)?.with_headers(headers))
        }
        DelayCheckOutcome::Denied { ref publish_time } => build_forbidden_response(
            &module,
            version,
            publish_time,
            checker,
            &logger,
            client_ip.as_deref(),
        ),
        DelayCheckOutcome::NotFound => Response::error(
            serde_json::json!({
                "error": "Module version not found",
                "module": module,
                "version": version
            })
            .to_string(),
            404,
        ),
        DelayCheckOutcome::UpstreamError(status) => Response::error(
            serde_json::json!({
                "error": "Upstream registry error",
                "status": status
            })
            .to_string(),
            502,
        ),
    }
}

pub async fn handle_gomod_download(
    req: Request,
    config: &Config,
    checker: &DelayChecker,
    cache: Option<&dyn MetadataCache>,
) -> Result<Response> {
    let logger = DelayLogger::new();
    let client_ip = extract_client_ip(&req);

    let path = req.path();
    let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    if path_parts.len() < 5 || path_parts[0] != "gomod" {
        return Response::error("Invalid request path", 400);
    }

    let module = path_parts[1..path_parts.len() - 3].join("/");
    let version_raw = path_parts[path_parts.len() - 2];
    let version_clean = version_raw.trim_end_matches(".zip");

    match check_version_with_delay(&module, version_clean, config, checker, cache).await? {
        DelayCheckOutcome::Allowed => {
            logger.log(&DelayLogEntry::new(
                PackageType::GoMod,
                module.clone(),
                version_clean.to_string(),
                None,
                DelayAction::Allowed,
                "Download passed delay check".to_string(),
                client_ip.clone(),
            ));

            let escaped_module = escape_module_path(&module);
            let upstream_url = format!(
                "{}/{}/@v/{}.zip",
                config.gomod_download_registry.trim_end_matches('/'),
                escaped_module,
                version_clean
            );

            let upstream_req = new_get_request(&upstream_url)?;
            Fetch::Request(upstream_req).send().await
        }
        DelayCheckOutcome::Denied { ref publish_time } => build_forbidden_response(
            &module,
            version_clean,
            publish_time,
            checker,
            &logger,
            client_ip.as_deref(),
        ),
        DelayCheckOutcome::NotFound => Response::error(
            serde_json::json!({
                "error": "Version not found",
                "module": module,
                "version": version_clean
            })
            .to_string(),
            404,
        ),
        DelayCheckOutcome::UpstreamError(status) => Response::error(
            serde_json::json!({
                "error": "Upstream registry error",
                "status": status
            })
            .to_string(),
            502,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;

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
}
