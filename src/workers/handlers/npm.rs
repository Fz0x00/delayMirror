use regex::Regex;
use serde_json::{json, Value};
use worker::{Fetch, Headers, Request, Response};

use crate::core::delay_logger::{DelayLogger, PackageType};
use crate::core::Config;
use crate::core::{DelayCheckError, DelayChecker, VersionCheckResult, MetadataCache};

fn get_base_url(req: &Request) -> String {
    let host = req
        .headers()
        .get("Host")
        .map(|v| v.unwrap_or_default())
        .unwrap_or_default();
    if host.is_empty() {
        String::new()
    } else {
        format!("https://{}", host)
    }
}

async fn proxy_upstream(url: &str, extra_headers: Option<&Headers>) -> worker::Result<Response> {
    let mut upstream_req = Request::new(url, worker::Method::Get)?;
    upstream_req
        .headers_mut()?
        .set("Accept", "application/octet-stream, */*")?;

    let mut upstream_resp = match Fetch::Request(upstream_req).send().await {
        Ok(r) => r,
        Err(e) => {
            return Response::error(format!("Upstream fetch failed: {}", e), 502);
        }
    };

    let status = upstream_resp.status_code();
    let body = upstream_resp
        .bytes()
        .await
        .map_err(|e| worker::Error::RustError(e.to_string()))?;
    let mut headers = Headers::new();
    let has_extra = extra_headers.is_some();

    if let Some(extra) = extra_headers {
        for (name, value) in extra.entries() {
            headers.set(&name, &value).ok();
        }
    }

    let mut resp = Response::from_bytes(body)?.with_status(status);
    if has_extra {
        resp = resp.with_headers(headers);
    }

    Ok(resp)
}

#[allow(dead_code)]
fn extract_version_from_filename(filename: &str) -> Option<String> {
    if !filename.ends_with(".tgz") {
        return None;
    }

    let without_extension = &filename[..filename.len() - 4];

    let version_pattern = Regex::new(r"-(\d+\.\d+\.\d+(?:-[\w\.]+)?(?:\+[\w\.]+)?)$").ok()?;

    if let Some(captures) = version_pattern.captures(without_extension) {
        captures.get(1).map(|m| m.as_str().to_string())
    } else {
        None
    }
}

async fn fetch_package_metadata(package: &str, registry: &str, cache: Option<&MetadataCache>) -> Result<Value, Response> {
    // 检查缓存
    if let Some(cache) = cache {
        if let Ok(true) = cache.is_valid(package, 24) { // 24小时缓存
            if let Ok(Some(metadata)) = cache.get(package) {
                return Ok(metadata.metadata);
            }
        }
    }
    
    let url = format!("{}/{}", registry, package);

    let upstream_req = match Request::new(&url, worker::Method::Get) {
        Ok(r) => r,
        Err(e) => {
            return Err(Response::error(
                json!({
                    "error": "Failed to create upstream request",
                    "details": e.to_string()
                })
                .to_string(),
                502,
            )
            .unwrap_or_else(|_| Response::error("Bad Gateway", 502).unwrap()));
        }
    };

    let mut resp = match Fetch::Request(upstream_req).send().await {
        Ok(r) => r,
        Err(e) => {
            return Err(Response::error(
                json!({
                    "error": "Failed to connect to upstream registry",
                    "details": e.to_string()
                })
                .to_string(),
                502,
            )
            .unwrap_or_else(|_| Response::error("Bad Gateway", 502).unwrap()));
        }
    };

    let status = resp.status_code();

    if status == 404 {
        return Err(Response::error(
            json!({
                "error": "Package not found",
                "package": package
            })
            .to_string(),
            404,
        )
        .unwrap_or_else(|_| Response::error("Not Found", 404).unwrap()));
    }

    if !(200..300).contains(&status) && status != 404 {
        return Err(Response::error(
            json!({
                "error": "Upstream registry error",
                "status": status
            })
            .to_string(),
            502,
        )
        .unwrap_or_else(|_| Response::error("Bad Gateway", 502).unwrap()));
    }

    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => {
            return Err(Response::error(
                json!({
                    "error": "Failed to read upstream response",
                    "details": e.to_string()
                })
                .to_string(),
                500,
            )
            .unwrap_or_else(|_| Response::error("Internal Server Error", 500).unwrap()))
        }
    };

    let metadata: Value = serde_json::from_str(&body).map_err(|e| {
        Response::error(
            json!({
                "error": "Failed to parse package metadata",
                "details": e.to_string()
            })
            .to_string(),
            500,
        )
        .unwrap_or_else(|_| Response::error("Internal Server Error", 500).unwrap())
    })?;
    
    // 存储到缓存
    if let Some(cache) = cache {
        let _ = cache.set(package, &metadata);
    }

    Ok(metadata)
}

fn filter_versions_by_delay(
    metadata: &mut Value,
    checker: &DelayChecker,
) -> Result<bool, DelayCheckError> {
    let time_value = match metadata.get("time") {
        Some(t) => t.clone(),
        None => return Err(DelayCheckError::MissingTimeField),
    };

    let time_info = checker.parse_time_field(&time_value)?;

    let eligible_versions: Vec<String> = time_info
        .versions()
        .filter_map(|v| {
            if let Some(publish_time) = time_info.get_publish_time(v) {
                if checker.is_version_allowed(publish_time) {
                    Some((*v).clone())
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect();

    let all_recent = eligible_versions.is_empty();

    if !all_recent {
        if let Some(versions_obj) = metadata.get_mut("versions").and_then(|v| v.as_object_mut()) {
            versions_obj.retain(|version, _| eligible_versions.contains(version));
        }

        if let Some(dist_tags) = metadata
            .get_mut("dist-tags")
            .and_then(|t| t.as_object_mut())
        {
            if let Some(latest_eligible) = eligible_versions.iter().max_by(|a, b| {
                let a_parts: Vec<u64> = a.split('.').filter_map(|s| s.parse().ok()).collect();
                let b_parts: Vec<u64> = b.split('.').filter_map(|s| s.parse().ok()).collect();
                a_parts.cmp(&b_parts)
            }) {
                dist_tags.insert("latest".to_string(), Value::String(latest_eligible.clone()));
            }
        }

        if let Some(time) = metadata.get_mut("time").and_then(|t| t.as_object_mut()) {
            time.retain(|version, _| {
                version == "created" || version == "modified" || eligible_versions.contains(version)
            });
        }
    }

    Ok(all_recent)
}

fn rewrite_tarball_urls(metadata: &mut Value, base_url: &str, _registry_url: &str) {
    let package_name = metadata
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("unknown")
        .to_string();

    if let Some(versions_obj) = metadata.get_mut("versions").and_then(|v| v.as_object_mut()) {
        for (version_key, version_data) in versions_obj.iter_mut() {
            if let Some(dist) = version_data.get_mut("dist") {
                if let Some(tarball) = dist.get_mut("tarball") {
                    if tarball.as_str().is_some() {
                        let new_tarball =
                            format!("{}/dl/{}@{}", base_url, package_name, version_key);
                        *tarball = Value::String(new_tarball);
                    }
                }
            }
        }
    }
}

fn make_error_response(error_body: &Value, status: u16) -> worker::Result<Response> {
    Response::error(error_body.to_string(), status)
}

fn make_redirect_response(url: &str, extra_headers: Option<&Headers>) -> worker::Result<Response> {
    let url = worker::Url::parse(url)
        .map_err(|_| worker::Error::RustError("Invalid redirect URL".to_string()))?;
    let mut resp = Response::redirect(url)?;
    if let Some(headers) = extra_headers {
        for (name, value) in headers.entries() {
            resp.headers_mut().set(&name, &value).ok();
        }
    }
    Ok(resp)
}

pub async fn handle_npm_metadata(
    req: Request,
    config: &Config,
    checker: &DelayChecker,
    cache: Option<&MetadataCache>,
) -> worker::Result<Response> {
    let path = req.path();
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();

    let package = match parts.get(1) {
        Some(&p) if !p.is_empty() => p,
        _ => {
            return make_error_response(&json!({ "error": "Package name is required" }), 400);
        }
    };

    let mut metadata = match fetch_package_metadata(package, &config.npm_registry, cache).await {
        Ok(m) => m,
        Err(resp) => return Ok(resp),
    };

    let base_url = get_base_url(&req);

    match filter_versions_by_delay(&mut metadata, checker) {
        Ok(all_recent) => {
            if all_recent {
                rewrite_tarball_urls(&mut metadata, &base_url, &config.npm_registry);
                let body = serde_json::to_string(&metadata)?;
                let mut headers = Headers::new();
                headers.set("Content-Type", "application/json")?;
                headers.set("X-Delay-Warning", "All versions are recent")?;
                return Ok(Response::from_bytes(body.into_bytes())?
                    .with_status(200)
                    .with_headers(headers));
            }
        }
        Err(DelayCheckError::MissingTimeField) => {
            return make_error_response(
                &json!({
                    "error": "Unable to determine version ages",
                    "details": "Package metadata does not contain a valid 'time' field"
                }),
                500,
            );
        }
        Err(e) => {
            return make_error_response(
                &json!({
                    "error": "Delay check failed",
                    "details": e.to_string()
                }),
                500,
            );
        }
    }

    rewrite_tarball_urls(&mut metadata, &base_url, &config.npm_registry);

    let body = serde_json::to_string(&metadata)?;
    let mut headers = Headers::new();
    headers.set("Content-Type", "application/json")?;
    Ok(Response::from_bytes(body.into_bytes())?
        .with_status(200)
        .with_headers(headers))
}

pub async fn handle_npm_version(
    req: Request,
    config: &Config,
    checker: &DelayChecker,
    cache: Option<&MetadataCache>,
) -> worker::Result<Response> {
    let logger = DelayLogger::new();
    let client_ip = req.headers().get("CF-Connecting-IP").ok().flatten();

    let path = req.path();
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();

    let package = match parts.get(1) {
        Some(&p) if !p.is_empty() => p,
        _ => {
            return make_error_response(&json!({ "error": "Package name is required" }), 400);
        }
    };

    let version = match parts.get(2) {
        Some(&v) if !v.is_empty() => v,
        _ => {
            return make_error_response(&json!({ "error": "Version is required" }), 400);
        }
    };

    let metadata = match fetch_package_metadata(package, &config.npm_registry, cache).await {
        Ok(m) => m,
        Err(resp) => return Ok(resp),
    };

    let time_value = match metadata.get("time") {
        Some(t) => t.clone(),
        None => {
            return make_error_response(
                &json!({
                    "error": "Unable to determine version age",
                    "details": "Package metadata does not contain a valid 'time' field"
                }),
                500,
            );
        }
    };

    let time_info = match checker.parse_time_field(&time_value) {
        Ok(info) => info,
        Err(e) => {
            return make_error_response(
                &json!({
                    "error": "Failed to parse version timing information",
                    "details": e.to_string()
                }),
                500,
            );
        }
    };

    match checker.check_version(version, &time_info) {
        Ok(VersionCheckResult::Allowed) => {
            let redirect_url = format!(
                "{}/{}/-/{}-{}.tgz",
                config.npm_download_registry, package, package, version
            );
            make_redirect_response(&redirect_url, None)
        }
        Ok(VersionCheckResult::Denied { .. }) | Ok(VersionCheckResult::Downgraded { .. }) => {
            let suggested = checker
                .find_eligible_version(version, &time_info)
                .ok()
                .flatten();

            logger.log_blocked(
                PackageType::Npm,
                package,
                version,
                &format!(
                    "Version was published within the last {} day(s)",
                    checker.delay_days()
                ),
                client_ip.as_deref(),
            );

            if let Some(ref sug) = suggested {
                logger.log_downgraded(
                    PackageType::Npm,
                    package,
                    version,
                    sug,
                    "Version too recent, suggesting older alternative",
                    client_ip.as_deref(),
                );
            }

            let mut headers = Headers::new();
            headers.set("X-Delay-Original-Version", version)?;
            if let Some(ref sug) = suggested {
                headers.set("X-Delay-Suggested-Version", sug)?;
            }
            headers.set(
                "X-Delay-Reason",
                "Version published too recently, below delay threshold",
            )?;

            let error_body = json!({
                "error": "Version too recent for download",
                "package": package,
                "requested_version": version,
                "reason": format!(
                    "Version was published within the last {} day(s)",
                    checker.delay_days()
                ),
                "suggested_version": suggested
            });

            let body = error_body.to_string();
            Ok(Response::from_bytes(body.into_bytes())?
                .with_status(403)
                .with_headers(headers))
        }
        Err(DelayCheckError::VersionNotFound { .. }) => make_error_response(
            &json!({
                "error": "Version not found",
                "package": package,
                "version": version
            }),
            404,
        ),
        Err(e) => make_error_response(
            &json!({
                "error": "Delay check failed",
                "details": e.to_string()
            }),
            500,
        ),
    }
}

pub async fn handle_npm_download(
    req: Request,
    config: &Config,
    checker: &DelayChecker,
    cache: Option<&MetadataCache>,
) -> worker::Result<Response> {
    let logger = DelayLogger::new();
    let client_ip = req.headers().get("CF-Connecting-IP").ok().flatten();

    let path = req.path();
    let path_trimmed = path.trim_start_matches('/');
    let parts: Vec<&str> = path_trimmed.split('/').collect();

    let raw_id = if parts.len() > 2 {
        parts[1..].join("/")
    } else {
        match parts.get(1) {
            Some(&p) if !p.is_empty() => p.to_string(),
            _ => {
                return make_error_response(
                    &json!({ "error": "Package identifier is required. Use /dl/{package@{version}" }),
                    400,
                );
            }
        }
    };

    let (package, version) = match raw_id.rfind('@') {
        Some(at_pos) if at_pos < raw_id.len() - 1 => {
            if at_pos == 0 {
                return make_error_response(
                    &json!({
                        "error": "Invalid package identifier format",
                        "expected": "/dl/{package}@{version} or /dl/{@scope/package}@{version}",
                        "got": raw_id
                    }),
                    400,
                );
            }
            (&raw_id[..at_pos], &raw_id[at_pos + 1..])
        }
        _ => {
            return make_error_response(
                &json!({
                    "error": "Invalid package identifier format",
                    "expected": "/dl/{package}@{version}",
                    "got": raw_id
                }),
                400,
            );
        }
    };

    let metadata = match fetch_package_metadata(package, &config.npm_registry, cache).await {
        Ok(m) => m,
        Err(resp) => return Ok(resp),
    };

    let time_value = match metadata.get("time") {
        Some(t) => t.clone(),
        None => {
            return make_error_response(
                &json!({
                    "error": "Unable to determine version age",
                    "details": "Package metadata does not contain a valid 'time' field"
                }),
                500,
            );
        }
    };

    let time_info = match checker.parse_time_field(&time_value) {
        Ok(info) => info,
        Err(e) => {
            return make_error_response(
                &json!({
                    "error": "Failed to parse version timing information",
                    "details": e.to_string()
                }),
                500,
            );
        }
    };

    let tarball_filename = if package.starts_with('@') {
        match package.rfind('/') {
            Some(slash_pos) => &package[slash_pos + 1..],
            None => package,
        }
    } else {
        package
    };

    match checker.resolve_version(version, &time_info) {
        Ok(VersionCheckResult::Allowed) => {
            let upstream_url = format!(
                "{}/{}/-/{}-{}.tgz",
                config.npm_download_registry, package, tarball_filename, version
            );
            proxy_upstream(&upstream_url, None).await
        }
        Ok(VersionCheckResult::Downgraded {
            suggested_version, ..
        }) => {
            let upstream_url = format!(
                "{}/{}/-/{}-{}.tgz",
                config.npm_download_registry, package, tarball_filename, suggested_version
            );

            logger.log_downgraded(
                PackageType::Npm,
                package,
                version,
                &suggested_version,
                "Version too recent, auto-downgraded for security",
                client_ip.as_deref(),
            );

            let mut headers = Headers::new();
            headers.set("X-Delay-Original-Version", version)?;
            headers.set("X-Delay-Redirected-Version", &suggested_version)?;
            headers.set(
                "X-Delay-Reason",
                "Version too recent, auto-downgraded for security",
            )?;

            proxy_upstream(&upstream_url, Some(&headers)).await
        }
        Ok(VersionCheckResult::Denied { .. }) => {
            let suggested = checker
                .find_eligible_version(version, &time_info)
                .ok()
                .flatten();

            logger.log_blocked(
                PackageType::Npm,
                package,
                version,
                &format!(
                    "Version was published within the last {} day(s) and no older alternative is available",
                    checker.delay_days()
                ),
                client_ip.as_deref(),
            );

            make_error_response(
                &json!({
                    "error": "Version too recent for download",
                    "package": package,
                    "requested_version": version,
                    "reason": format!(
                        "Version was published within the last {} day(s) and no older alternative is available",
                        checker.delay_days()
                    ),
                    "suggested_version": suggested
                }),
                403,
            )
        }
        Err(DelayCheckError::VersionNotFound { .. }) => make_error_response(
            &json!({
                "error": "Version not found in package metadata",
                "package": package,
                "version": version
            }),
            404,
        ),
        Err(e) => make_error_response(
            &json!({
                "error": "Delay check failed",
                "details": e.to_string()
            }),
            500,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use serde_json::json;

    fn make_old_metadata() -> Value {
        let now = Utc::now();
        json!({
            "name": "test-package",
            "dist-tags": {
                "latest": "3.0.0"
            },
            "versions": {
                "1.0.0": {
                    "version": "1.0.0",
                    "dist": {
                        "tarball": "https://registry.npmmirror.com/test-package/-/test-package-1.0.0.tgz"
                    }
                },
                "2.0.0": {
                    "version": "2.0.0",
                    "dist": {
                        "tarball": "https://registry.npmmirror.com/test-package/-/test-package-2.0.0.tgz"
                    }
                },
                "3.0.0": {
                    "version": "3.0.0",
                    "dist": {
                        "tarball": "https://registry.npmmirror.com/test-package/-/test-package-3.0.0.tgz"
                    }
                }
            },
            "time": {
                "created": (now - Duration::days(100)).to_rfc3339(),
                "modified": (now - Duration::days(10)).to_rfc3339(),
                "1.0.0": (now - Duration::days(100)).to_rfc3339(),
                "2.0.0": (now - Duration::days(50)).to_rfc3339(),
                "3.0.0": (now - Duration::days(10)).to_rfc3339()
            }
        })
    }

    fn make_mixed_metadata() -> Value {
        let now = Utc::now();
        json!({
            "name": "mixed-package",
            "dist-tags": {
                "latest": "4.0.0"
            },
            "versions": {
                "1.0.0": {
                    "version": "1.0.0",
                    "dist": {
                        "tarball": "https://registry.npmmirror.com/mixed-package/-/mixed-package-1.0.0.tgz"
                    }
                },
                "2.0.0": {
                    "version": "2.0.0",
                    "dist": {
                        "tarball": "https://registry.npmmirror.com/mixed-package/-/mixed-package-2.0.0.tgz"
                    }
                },
                "3.0.0": {
                    "version": "3.0.0",
                    "dist": {
                        "tarball": "https://registry.npmmirror.com/mixed-package/-/mixed-package-3.0.0.tgz"
                    }
                },
                "4.0.0": {
                    "version": "4.0.0",
                    "dist": {
                        "tarball": "https://registry.npmmirror.com/mixed-package/-/mixed-package-4.0.0.tgz"
                    }
                }
            },
            "time": {
                "created": (now - Duration::days(100)).to_rfc3339(),
                "modified": (now - Duration::days(1)).to_rfc3339(),
                "1.0.0": (now - Duration::days(100)).to_rfc3339(),
                "2.0.0": (now - Duration::days(50)).to_rfc3339(),
                "3.0.0": (now - Duration::days(5)).to_rfc3339(),
                "4.0.0": (now - Duration::days(1)).to_rfc3339()
            }
        })
    }

    fn make_all_recent_metadata() -> Value {
        let now = Utc::now();
        json!({
            "name": "recent-package",
            "dist-tags": {
                "latest": "2.0.0"
            },
            "versions": {
                "1.0.0": {
                    "version": "1.0.0",
                    "dist": {
                        "tarball": "https://registry.npmmirror.com/recent-package/-/recent-package-1.0.0.tgz"
                    }
                },
                "2.0.0": {
                    "version": "2.0.0",
                    "dist": {
                        "tarball": "https://registry.npmmirror.com/recent-package/-/recent-package-2.0.0.tgz"
                    }
                }
            },
            "time": {
                "created": (now - Duration::days(2)).to_rfc3339(),
                "modified": (now - Duration::days(1)).to_rfc3339(),
                "1.0.0": (now - Duration::days(2)).to_rfc3339(),
                "2.0.0": (now - Duration::days(1)).to_rfc3339()
            }
        })
    }

    #[test]
    fn test_extract_version_from_filename_valid() {
        assert_eq!(
            extract_version_from_filename("lodash-4.17.21.tgz"),
            Some("4.17.21".to_string())
        );
        assert_eq!(
            extract_version_from_filename("react-18.2.0.tgz"),
            Some("18.2.0".to_string())
        );
    }

    #[test]
    fn test_extract_version_from_filename_invalid() {
        assert_eq!(extract_version_from_filename("invalid.tgz"), None);
        assert_eq!(extract_version_from_filename("no-extension.tar"), None);
        assert_eq!(extract_version_from_filename(""), None);
    }

    #[test]
    fn test_extract_version_from_filename_prerelease() {
        assert_eq!(
            extract_version_from_filename("pkg-1.0.0-beta.1.tgz"),
            Some("1.0.0-beta.1".to_string())
        );
        assert_eq!(
            extract_version_from_filename("pkg-2.0.0-rc.1.tgz"),
            Some("2.0.0-rc.1".to_string())
        );
    }

    #[test]
    fn test_filter_versions_by_delay_all_old() {
        let checker = DelayChecker::new(5);
        let mut metadata = make_old_metadata();
        let result = filter_versions_by_delay(&mut metadata, &checker).unwrap();
        assert!(!result);

        let versions = metadata.get("versions").unwrap().as_object().unwrap();
        assert_eq!(versions.len(), 3);
    }

    #[test]
    fn test_filter_versions_by_delay_mixed() {
        let checker = DelayChecker::new(7);
        let mut metadata = make_mixed_metadata();
        let result = filter_versions_by_delay(&mut metadata, &checker).unwrap();
        assert!(!result);

        let versions = metadata.get("versions").unwrap().as_object().unwrap();
        assert_eq!(versions.len(), 2);
        assert!(versions.contains_key("1.0.0"));
        assert!(versions.contains_key("2.0.0"));
        assert!(!versions.contains_key("3.0.0"));
        assert!(!versions.contains_key("4.0.0"));

        let dist_tags = metadata.get("dist-tags").unwrap().as_object().unwrap();
        assert_eq!(dist_tags.get("latest").unwrap().as_str().unwrap(), "2.0.0");
    }

    #[test]
    fn test_filter_versions_by_delay_all_recent() {
        let checker = DelayChecker::new(30);
        let mut metadata = make_all_recent_metadata();
        let result = filter_versions_by_delay(&mut metadata, &checker).unwrap();
        assert!(result);

        let versions = metadata.get("versions").unwrap().as_object().unwrap();
        assert_eq!(versions.len(), 2);
    }

    #[test]
    fn test_filter_versions_by_delay_missing_time() {
        let checker = DelayChecker::default();
        let mut metadata = json!({
            "name": "no-time-pkg",
            "versions": { "1.0.0": {} }
        });
        let result = filter_versions_by_delay(&mut metadata, &checker);
        assert!(result.is_err());
        match result.unwrap_err() {
            DelayCheckError::MissingTimeField => {}
            other => panic!("Expected MissingTimeField, got {:?}", other),
        }
    }

    #[test]
    fn test_rewrite_tarball_urls_basic() {
        let mut metadata = json!({
            "name": "test-package",
            "versions": {
                "1.0.0": {
                    "dist": {
                        "tarball": "https://registry.npmmirror.com/test-package/-/test-package-1.0.0.tgz"
                    }
                },
                "2.0.0": {
                    "dist": {
                        "tarball": "https://registry.npmmirror.com/test-package/-/test-package-2.0.0.tgz"
                    }
                }
            }
        });

        rewrite_tarball_urls(
            &mut metadata,
            "http://localhost:8787",
            "https://registry.npmmirror.com",
        );

        let versions = metadata.get("versions").unwrap().as_object().unwrap();
        let v1 = versions.get("1.0.0").unwrap();
        let v2 = versions.get("2.0.0").unwrap();

        assert_eq!(
            v1.get("dist")
                .unwrap()
                .get("tarball")
                .unwrap()
                .as_str()
                .unwrap(),
            "http://localhost:8787/dl/test-package@1.0.0"
        );
        assert_eq!(
            v2.get("dist")
                .unwrap()
                .get("tarball")
                .unwrap()
                .as_str()
                .unwrap(),
            "http://localhost:8787/dl/test-package@2.0.0"
        );
    }

    #[test]
    fn test_rewrite_tarball_urls_preserves_other_fields() {
        let mut metadata = json!({
            "name": "complex-package",
            "description": "A complex package",
            "versions": {
                "1.0.0": {
                    "name": "complex-package",
                    "version": "1.0.0",
                    "dist": {
                        "tarball": "https://registry.npmmirror.com/complex/-/complex-1.0.0.tgz",
                        "shasum": "abc123",
                        "integrity": "sha512-xyz789"
                    }
                }
            }
        });

        rewrite_tarball_urls(
            &mut metadata,
            "http://gateway:8787",
            "https://registry.npmmirror.com",
        );

        assert_eq!(
            metadata.get("name").unwrap().as_str().unwrap(),
            "complex-package"
        );
        assert_eq!(
            metadata.get("description").unwrap().as_str().unwrap(),
            "A complex package"
        );

        let v1 = metadata.get("versions").unwrap().get("1.0.0").unwrap();
        assert_eq!(v1.get("name").unwrap().as_str().unwrap(), "complex-package");
        assert_eq!(v1.get("version").unwrap().as_str().unwrap(), "1.0.0");

        let dist = v1.get("dist").unwrap().as_object().unwrap();
        assert_eq!(dist.get("shasum").unwrap().as_str().unwrap(), "abc123");
        assert_eq!(
            dist.get("integrity").unwrap().as_str().unwrap(),
            "sha512-xyz789"
        );
        assert!(dist
            .get("tarball")
            .unwrap()
            .as_str()
            .unwrap()
            .starts_with("http://gateway:8787/dl/"));
    }

    #[test]
    fn test_filter_versions_maintains_time_info() {
        let checker = DelayChecker::new(7);
        let mut metadata = make_mixed_metadata();
        filter_versions_by_delay(&mut metadata, &checker).unwrap();

        let time = metadata.get("time").unwrap().as_object().unwrap();
        assert!(time.contains_key("created"));
        assert!(time.contains_key("modified"));
        assert!(time.contains_key("1.0.0"));
        assert!(time.contains_key("2.0.0"));
        assert!(!time.contains_key("3.0.0"));
        assert!(!time.contains_key("4.0.0"));
    }

    #[test]
    fn test_filter_versions_selects_latest_eligible_as_dist_tag() {
        let now = Utc::now();
        let checker = DelayChecker::new(7);
        let mut metadata = json!({
            "dist-tags": { "latest": "5.0.0" },
            "versions": {
                "1.0.0": {},
                "2.5.0": {},
                "3.1.0": {}
            },
            "time": {
                "created": "2020-01-01T00:00:00Z",
                "modified": "2024-06-15T12:00:00Z",
                "1.0.0": (now - Duration::days(100)).to_rfc3339(),
                "2.5.0": (now - Duration::days(50)).to_rfc3339(),
                "3.1.0": (now - Duration::days(2)).to_rfc3339()
            }
        });

        filter_versions_by_delay(&mut metadata, &checker).unwrap();

        let dist_tags = metadata.get("dist-tags").unwrap().as_object().unwrap();
        assert_eq!(dist_tags.get("latest").unwrap().as_str().unwrap(), "2.5.0");
    }

    #[test]
    fn test_extract_version_scoped_packages() {
        assert_eq!(
            extract_version_from_filename("@types/node-20.0.0.tgz"),
            Some("20.0.0".to_string())
        );
        assert_eq!(
            extract_version_from_filename("@scope/pkg-name-1.2.3.tgz"),
            Some("1.2.3".to_string())
        );
    }
}
