use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::Value;
use worker::*;

use crate::core::delay_logger::{DelayLogger, PackageType};
use crate::core::Config;
use crate::core::{DelayCheckError, DelayChecker, VersionCheckResult};

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

    if status == 404 {
        return Err(
            Response::error(format!("Package not found on PyPI: {}", package), 404)
                .unwrap_or_else(|_| Response::error("Not Found", 404).unwrap()),
        );
    }

    if !(200..300).contains(&status) {
        return Err(
            Response::error(format!("PyPI JSON API error, status: {}", status), 502)
                .unwrap_or_else(|_| Response::error("Bad Gateway", 502).unwrap()),
        );
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

fn build_sdist_filename(package: &str, version: &str) -> String {
    format!("{}-{}.tar.gz", package, version)
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
        return Err(format!("Upstream PyPI registry error, status: {}", status).into());
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
    package: &str,
) -> Result<Response> {
    match handle_pypi_package_list_inner(_req, config, checker, package).await {
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
    package: &str,
) -> Result<Response> {
    let normalized_package = normalize_package_name(package);

    let pypi_simple_url = format!(
        "{}{}/",
        config.pypi_registry.trim_end_matches('/'),
        normalized_package
    );

    let simple_req = match Request::new(&pypi_simple_url, Method::Get) {
        Ok(r) => r,
        Err(e) => return Err(format!("Failed to create PyPI simple request: {}", e).into()),
    };
    let mut simple_resp = match Fetch::Request(simple_req).send().await {
        Ok(r) => r,
        Err(e) => {
            return Err(format!("Failed to connect to upstream PyPI registry: {}", e).into());
        }
    };

    let status = simple_resp.status_code();

    if !(200..300).contains(&status) {
        return Err(format!("Package not found on PyPI: {}", package).into());
    }

    let body = match simple_resp.text().await {
        Ok(b) => b,
        Err(e) => return Err(format!("Failed to read upstream response: {}", e).into()),
    };

    let release_info = fetch_pypi_release_info(package, &config.pypi_json_api_base).await;

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
    Ok(Response::ok(body)?.with_headers(headers))
}

pub async fn handle_pypi_download(
    _req: Request,
    config: &Config,
    checker: &DelayChecker,
    logger: &DelayLogger,
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

    let metadata = match fetch_pypi_release_info(&package, &config.pypi_json_api_base).await {
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
            let upstream_req = Request::new(&upstream_url, Method::Get)?;
            Fetch::Request(upstream_req).send().await
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
}
