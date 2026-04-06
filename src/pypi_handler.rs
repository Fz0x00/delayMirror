use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use regex::Regex;
use reqwest::Client;
use serde::Serialize;
use serde_json::{json, Value};

use crate::core::{Config, DelayCheckError, DelayChecker, VersionCheckResult};

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
    client: &Client,
) -> Result<Value, (u16, String)> {
    let normalized = normalize_package_name(package);
    let url = format!("{}/{}/json", json_api_base, normalized);

    match client.get(&url).send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();

            if status == 404 {
                return Err((404, format!("Package not found on PyPI: {}", package)));
            }

            if !(200..300).contains(&status) {
                return Err((502, format!("PyPI JSON API error, status: {}", status)));
            }

            match resp.json::<Value>().await {
                Ok(metadata) => Ok(metadata),
                Err(e) => Err((
                    500,
                    format!("Failed to parse PyPI JSON API response: {}", e),
                )),
            }
        }
        Err(e) => Err((502, format!("Failed to connect to PyPI JSON API: {}", e))),
    }
}

fn parse_upload_time(time_str: &str) -> Result<DateTime<Utc>, DelayCheckError> {
    crate::core::delay_check::parse_datetime_flexible(time_str).map_err(|e| {
        DelayCheckError::InvalidTimeFormat(format!("invalid upload_time format: {}", e))
    })
}

fn build_version_time_info_from_releases(releases: &Value) -> Result<Value, DelayCheckError> {
    let releases_obj = releases.as_object().ok_or_else(|| {
        DelayCheckError::InvalidTimeFormat("releases field is not a JSON object".to_string())
    })?;

    let mut time_map: serde_json::Map<String, Value> = serde_json::Map::new();

    for (version, files_arr) in releases_obj {
        if let Some(files) = files_arr.as_array() {
            if let Some(first_file) = files.first() {
                if let Some(upload_time) = first_file.get("upload_time").and_then(|t| t.as_str()) {
                    let _ = time_map
                        .insert(version.to_string(), Value::String(upload_time.to_string()));
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

fn extract_filename_from_path(path: &str) -> String {
    let path = path.split('#').next().unwrap_or(path);
    let filename = path.rsplit('/').next().unwrap_or(path);
    filename.to_string()
}

pub async fn handle_pypi_simple_index(config: &Config, _client: &Client) -> Response {
    let url = config.pypi_simple_index.trim_end_matches('/');
    let simple_url = format!("{}/", url);
    proxy_redirect(&simple_url)
}

pub async fn handle_pypi_package_list(
    config: &Config,
    checker: &DelayChecker,
    package: &str,
    client: &Client,
) -> Response {
    match handle_pypi_package_list_inner(config, checker, package, client).await {
        Ok(resp) => resp,
        Err((status, message)) => error_response(status, &message),
    }
}

async fn handle_pypi_package_list_inner(
    config: &Config,
    _checker: &DelayChecker,
    package: &str,
    _client: &Client,
) -> Result<Response, (u16, String)> {
    let normalized_package = normalize_package_name(package);

    let pypi_simple_url = format!(
        "{}/{}/",
        config.pypi_simple_index.trim_end_matches('/'),
        normalized_package
    );

    Ok(proxy_redirect(&pypi_simple_url))
}

pub async fn handle_pypi_download(
    config: &Config,
    checker: &DelayChecker,
    path: &str,
    client: &Client,
) -> Response {
    let filename = extract_filename_from_path(path);

    let (package, version) = match parse_package_filename(&filename) {
        Some(result) => result,
        None => {
            return error_response(
                400,
                "Invalid filename format: Filename must be a valid wheel (.whl) or sdist (.tar.gz) format",
            );
        }
    };

    let metadata = match fetch_pypi_release_info(&package, &config.pypi_json_api_base, client).await
    {
        Ok(m) => m,
        Err((status, msg)) => return error_response(status, &msg),
    };

    let releases = match metadata.get("releases") {
        Some(r) => r.clone(),
        None => {
            return error_response(
                500,
                "Unable to determine version age: PyPI JSON API response does not contain 'releases' field",
            );
        }
    };

    let release_files = match releases.get(&version) {
        Some(files) => files,
        None => {
            if !config.pypi_download_mirror.is_empty() {
                let mirror_url = format!(
                    "{}/{}",
                    config.pypi_download_mirror.trim_end_matches('/'),
                    path
                );
                if let Ok(resp) = client.get(&mirror_url).send().await {
                    if resp.status().is_success() {
                        return Response::builder()
                            .status(StatusCode::FOUND)
                            .header("Location", &mirror_url)
                            .header(
                                "X-Delay-Warning",
                                "Version not in JSON API, using mirror directly",
                            )
                            .body(axum::body::boxed(axum::body::Full::from(vec![])))
                            .unwrap();
                    }
                }
            }
            let upstream_url = format!("{}/{}", config.pypi_download_base, path);
            return proxy_redirect(&upstream_url);
        }
    };

    let _upload_time = match release_files
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|f| f.get("upload_time"))
        .and_then(|t| t.as_str())
    {
        Some(t) => match parse_upload_time(t) {
            Ok(dt) => dt,
            Err(e) => return error_response(500, &format!("Failed to parse upload time: {}", e)),
        },
        None => {
            return error_response(
                500,
                "Upload time not found: Release files do not contain upload_time information",
            );
        }
    };

    let time_value = match build_version_time_info_from_releases(&releases) {
        Ok(tv) => tv,
        Err(e) => {
            return error_response(500, &format!("Failed to build version timing info: {}", e))
        }
    };

    let time_info = match checker.parse_time_field(&time_value) {
        Ok(info) => info,
        Err(e) => {
            return error_response(
                500,
                &format!("Failed to parse version timing information: {}", e),
            );
        }
    };

    match checker.resolve_version(&version, &time_info) {
        Ok(VersionCheckResult::Allowed) => {
            if !config.pypi_download_mirror.is_empty() {
                let mirror_url = format!(
                    "{}/{}",
                    config.pypi_download_mirror.trim_end_matches('/'),
                    path
                );
                if let Ok(resp) = client.get(&mirror_url).send().await {
                    if resp.status().is_success() {
                        return Response::builder()
                            .status(StatusCode::FOUND)
                            .header("Location", &mirror_url)
                            .body(axum::body::boxed(axum::body::Full::from(vec![])))
                            .unwrap();
                    }
                }
            }

            let upstream_url = find_download_url_for_version(&releases, &version, &filename)
                .unwrap_or_else(|| format!("{}/{}", config.pypi_download_base, path));
            proxy_redirect(&upstream_url)
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

            if !config.pypi_download_mirror.is_empty() {
                let mirror_url = format!(
                    "{}/{}",
                    config.pypi_download_mirror.trim_end_matches('/'),
                    redirected_filename
                );
                if let Ok(resp) = client.get(&mirror_url).send().await {
                    if resp.status().is_success() {
                        return redirect_response_with_headers(
                            &mirror_url,
                            vec![
                                ("X-Delay-Original-Version", version.to_string()),
                                ("X-Delay-Redirected-Version", suggested_version.clone()),
                                (
                                    "X-Delay-Reason",
                                    "Version too recent, auto-downloaded for security".to_string(),
                                ),
                            ],
                        );
                    }
                }
            }

            let upstream_url =
                find_download_url_for_version(&releases, &suggested_version, &redirected_filename)
                    .unwrap_or_else(|| {
                        format!("{}/{}", config.pypi_download_base, redirected_filename)
                    });

            redirect_response_with_headers(
                &upstream_url,
                vec![
                    ("X-Delay-Original-Version", version.to_string()),
                    ("X-Delay-Redirected-Version", suggested_version.clone()),
                    (
                        "X-Delay-Reason",
                        "Version too recent, auto-downloaded for security".to_string(),
                    ),
                ],
            )
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

            let body = json!({
                "error": "Version too recent for download",
                "package": package,
                "requested_version": version,
                "reason": reason,
                "suggested_version": suggested
            });

            error_response_json_with_headers(
                403,
                &body,
                vec![
                    ("X-Delay-Original-Version", version.to_string()),
                    (
                        "X-Delay-Reason",
                        "Version published too recently, below delay threshold".to_string(),
                    ),
                ],
            )
        }
        Err(DelayCheckError::VersionNotFound { .. }) => error_response(
            404,
            &format!(
                "Version not found in package releases: package={}, version={}",
                package, version
            ),
        ),
        Err(e) => error_response(500, &format!("Delay check failed: {}", e)),
    }
}

fn proxy_redirect(url: &str) -> Response {
    Response::builder()
        .status(StatusCode::TEMPORARY_REDIRECT)
        .header(header::LOCATION, url)
        .body(axum::body::boxed(axum::body::Full::from(vec![])))
        .unwrap()
}

fn error_response(status: u16, message: &str) -> Response {
    let status_code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status_code, axum::Json(json!({ "error": message }))).into_response()
}

fn error_response_json_with_headers(
    status: u16,
    body: &impl Serialize,
    headers: Vec<(&'static str, String)>,
) -> Response {
    let status_code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut resp = (status_code, axum::Json(body)).into_response();

    for (name, value) in headers {
        if let Ok(hdr_val) = value.parse() {
            resp.headers_mut().insert(name, hdr_val);
        }
    }

    resp
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

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use chrono::{Datelike, Duration};

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
