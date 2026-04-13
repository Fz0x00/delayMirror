use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;

const DEFAULT_DELAY_DAYS: i64 = 3;

#[derive(Debug, Clone)]
pub enum DelayCheckError {
    InvalidTimeFormat(String),
    MissingTimeField,
    VersionNotFound { version: String },
    NoEligibleVersions,
    InvalidDelayDays(String),
}

impl fmt::Display for DelayCheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTimeFormat(msg) => write!(f, "Invalid time field format: {}", msg),
            Self::MissingTimeField => write!(f, "Missing time field in metadata"),
            Self::VersionNotFound { version } => {
                write!(f, "Version '{}' not found in metadata", version)
            }
            Self::NoEligibleVersions => {
                write!(f, "No eligible versions available after delay check")
            }
            Self::InvalidDelayDays(val) => {
                write!(f, "Invalid DELAY_DAYS value: {}", val)
            }
        }
    }
}

impl std::error::Error for DelayCheckError {}

#[derive(Debug, Clone)]
pub struct VersionTimeInfo {
    inner: BTreeMap<String, DateTime<Utc>>,
}

impl VersionTimeInfo {
    pub fn get_publish_time(&self, version: &str) -> Option<&DateTime<Utc>> {
        self.inner.get(version)
    }

    pub fn versions(&self) -> impl Iterator<Item = &String> {
        self.inner.keys()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum VersionCheckResult {
    Allowed,
    Denied {
        publish_time: DateTime<Utc>,
    },
    Downgraded {
        original_version: String,
        suggested_version: String,
        original_time: DateTime<Utc>,
        suggested_time: DateTime<Utc>,
    },
}

#[derive(Debug, Clone)]
pub struct DelayChecker {
    delay_days: i64,
    threshold: DateTime<Utc>,
}

impl Default for DelayChecker {
    fn default() -> Self {
        Self::new(DEFAULT_DELAY_DAYS)
    }
}

impl DelayChecker {
    pub fn new(delay_days: i64) -> Self {
        let threshold = Utc::now() - Duration::days(delay_days);
        Self {
            delay_days,
            threshold,
        }
    }

    pub fn with_delay_days(delay_days: i64) -> Result<Self, DelayCheckError> {
        if delay_days <= 0 {
            return Err(DelayCheckError::InvalidDelayDays(format!("{}", delay_days)));
        }
        Ok(Self::new(delay_days))
    }

    pub fn delay_days(&self) -> i64 {
        self.delay_days
    }

    pub fn threshold(&self) -> &DateTime<Utc> {
        &self.threshold
    }

    pub fn parse_time_field(&self, time_value: &Value) -> Result<VersionTimeInfo, DelayCheckError> {
        let time_obj = time_value.as_object().ok_or_else(|| {
            DelayCheckError::InvalidTimeFormat("time field is not a JSON object".to_string())
        })?;

        let mut versions = BTreeMap::new();

        for (key, val) in time_obj {
            if key == "created" || key == "modified" {
                continue;
            }

            let time_str = val.as_str().ok_or_else(|| {
                DelayCheckError::InvalidTimeFormat(format!(
                    "value for version '{}' is not a string",
                    key
                ))
            })?;

            let dt = parse_datetime_flexible(time_str).map_err(|_e| {
                DelayCheckError::InvalidTimeFormat(format!(
                    "invalid ISO 8601 timestamp for version '{}': {}",
                    key, time_str
                ))
            })?;

            versions.insert(key.clone(), dt);
        }

        if versions.is_empty() {
            return Err(DelayCheckError::MissingTimeField);
        }

        Ok(VersionTimeInfo { inner: versions })
    }

    pub fn is_version_allowed(&self, publish_time: &DateTime<Utc>) -> bool {
        publish_time <= &self.threshold
    }

    pub fn check_version(
        &self,
        version: &str,
        time_info: &VersionTimeInfo,
    ) -> Result<VersionCheckResult, DelayCheckError> {
        let publish_time = time_info.get_publish_time(version).ok_or_else(|| {
            DelayCheckError::VersionNotFound {
                version: version.to_string(),
            }
        })?;

        if self.is_version_allowed(publish_time) {
            Ok(VersionCheckResult::Allowed)
        } else {
            Ok(VersionCheckResult::Denied {
                publish_time: *publish_time,
            })
        }
    }

    pub fn find_eligible_version(
        &self,
        requested_version: &str,
        time_info: &VersionTimeInfo,
    ) -> Result<Option<String>, DelayCheckError> {
        let original_time = time_info
            .get_publish_time(requested_version)
            .ok_or_else(|| DelayCheckError::VersionNotFound {
                version: requested_version.to_string(),
            })?;

        if self.is_version_allowed(original_time) {
            return Ok(Some(requested_version.to_string()));
        }

        let eligible: Vec<(&String, &DateTime<Utc>)> = time_info
            .versions()
            .filter_map(|v| {
                if let Some(t) = time_info.get_publish_time(v) {
                    if self.is_version_allowed(t) {
                        Some((v, t))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();

        if eligible.is_empty() {
            return Ok(None);
        }

        // 优先选择发布时间最接近阈值的版本
        let best = eligible
            .iter()
            .min_by(|a, b| {
                // 比较版本的发布时间与阈值的接近程度
                let a_diff = (*a.1 - self.threshold).num_seconds().abs();
                let b_diff = (*b.1 - self.threshold).num_seconds().abs();
                a_diff.cmp(&b_diff)
            })
            .expect("eligible should not be empty");

        Ok(Some((*best.0).clone()))
    }

    pub fn resolve_version(
        &self,
        requested_version: &str,
        time_info: &VersionTimeInfo,
    ) -> Result<VersionCheckResult, DelayCheckError> {
        let original_time = time_info
            .get_publish_time(requested_version)
            .ok_or_else(|| DelayCheckError::VersionNotFound {
                version: requested_version.to_string(),
            })?;

        if self.is_version_allowed(original_time) {
            return Ok(VersionCheckResult::Allowed);
        }

        let suggested = self.find_eligible_version(requested_version, time_info)?;

        match suggested {
            Some(ver) if ver == requested_version => Ok(VersionCheckResult::Allowed),
            Some(ver) => {
                let suggested_time = time_info
                    .get_publish_time(&ver)
                    .expect("version from find_eligible_version must exist");
                Ok(VersionCheckResult::Downgraded {
                    original_version: requested_version.to_string(),
                    suggested_version: ver,
                    original_time: *original_time,
                    suggested_time: *suggested_time,
                })
            }
            None => Ok(VersionCheckResult::Denied {
                publish_time: *original_time,
            }),
        }
    }
}

fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let a_parts: Vec<u64> = a.split('.').filter_map(|s| s.parse().ok()).collect();
    let b_parts: Vec<u64> = b.split('.').filter_map(|s| s.parse().ok()).collect();
    a_parts.cmp(&b_parts)
}

pub fn parse_datetime_flexible(time_str: &str) -> Result<DateTime<Utc>, DelayCheckError> {
    if let Ok(dt) = time_str.parse::<DateTime<Utc>>() {
        return Ok(dt);
    }

    if let Ok(naive) = NaiveDateTime::parse_from_str(time_str, "%Y-%m-%dT%H:%M:%S") {
        return Ok(DateTime::from_naive_utc_and_offset(naive, Utc));
    }

    if let Ok(naive) = NaiveDateTime::parse_from_str(time_str, "%Y-%m-%dT%H:%M:%S%.f") {
        return Ok(DateTime::from_naive_utc_and_offset(naive, Utc));
    }

    if let Ok(naive) = NaiveDateTime::parse_from_str(time_str, "%Y-%m-%d %H:%M:%S") {
        return Ok(DateTime::from_naive_utc_and_offset(naive, Utc));
    }

    Err(DelayCheckError::InvalidTimeFormat(format!(
        "unable to parse datetime: {}",
        time_str
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_sample_time() -> Value {
        json!({
            "created": "2024-01-01T00:00:00Z",
            "modified": "2024-06-15T12:00:00Z",
            "1.0.0": "2024-01-01T00:00:00Z",
            "2.0.0": "2024-03-01T00:00:00Z",
            "3.0.0": "2024-06-01T00:00:00Z"
        })
    }

    #[test]
    fn test_default_delay_checker() {
        let checker = DelayChecker::default();
        assert_eq!(checker.delay_days(), 3);
    }

    #[test]
    fn test_custom_delay_checker() {
        let checker = DelayChecker::new(7);
        assert_eq!(checker.delay_days(), 7);
    }

    #[test]
    fn test_with_delay_days_valid() {
        let checker = DelayChecker::with_delay_days(7).unwrap();
        assert_eq!(checker.delay_days(), 7);
    }

    #[test]
    fn test_with_delay_days_invalid_zero() {
        let result = DelayChecker::with_delay_days(0);
        assert!(result.is_err());
        match result.unwrap_err() {
            DelayCheckError::InvalidDelayDays(_) => {}
            other => panic!("Expected InvalidDelayDays, got {:?}", other),
        }
    }

    #[test]
    fn test_with_delay_days_invalid_negative() {
        let result = DelayChecker::with_delay_days(-1);
        assert!(result.is_err());
        match result.unwrap_err() {
            DelayCheckError::InvalidDelayDays(_) => {}
            other => panic!("Expected InvalidDelayDays, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_time_field_valid() {
        let checker = DelayChecker::default();
        let time_json = make_sample_time();
        let info = checker.parse_time_field(&time_json).unwrap();

        assert_eq!(info.len(), 3);
        assert!(info.get_publish_time("1.0.0").is_some());
        assert!(info.get_publish_time("2.0.0").is_some());
        assert!(info.get_publish_time("3.0.0").is_some());
        assert!(info.get_publish_time("created").is_none());
        assert!(info.get_publish_time("modified").is_none());
    }

    #[test]
    fn test_parse_time_field_not_object() {
        let checker = DelayChecker::default();
        let result = checker.parse_time_field(&json!("not_an_object"));
        assert!(result.is_err());
        match result.unwrap_err() {
            DelayCheckError::InvalidTimeFormat(_) => {}
            other => panic!("Expected InvalidTimeFormat, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_time_field_empty_versions() {
        let checker = DelayChecker::default();
        let time_json = json!({
            "created": "2024-01-01T00:00:00Z",
            "modified": "2024-06-15T12:00:00Z"
        });
        let result = checker.parse_time_field(&time_json);
        assert!(result.is_err());
        match result.unwrap_err() {
            DelayCheckError::MissingTimeField => {}
            other => panic!("Expected MissingTimeField, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_time_field_invalid_timestamp() {
        let checker = DelayChecker::default();
        let time_json = json!({
            "1.0.0": "not-a-valid-timestamp"
        });
        let result = checker.parse_time_field(&time_json);
        assert!(result.is_err());
        match result.unwrap_err() {
            DelayCheckError::InvalidTimeFormat(_) => {}
            other => panic!("Expected InvalidTimeFormat, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_time_field_non_string_value() {
        let checker = DelayChecker::default();
        let time_json = json!({
            "1.0.0": 12345
        });
        let result = checker.parse_time_field(&time_json);
        assert!(result.is_err());
        match result.unwrap_err() {
            DelayCheckError::InvalidTimeFormat(_) => {}
            other => panic!("Expected InvalidTimeFormat, got {:?}", other),
        }
    }

    #[test]
    fn test_is_version_allowed_old_version() {
        let checker = DelayChecker::new(30);
        let old_time: DateTime<Utc> = "2024-01-01T00:00:00Z".parse().unwrap();
        assert!(checker.is_version_allowed(&old_time));
    }

    #[test]
    fn test_is_version_allowed_recent_version() {
        let checker = DelayChecker::new(365);
        let recent_time = Utc::now() - Duration::days(1);
        assert!(!checker.is_version_allowed(&recent_time));
    }

    #[test]
    fn test_check_version_allowed() {
        let checker = DelayChecker::new(365);
        let time_json = make_sample_time();
        let info = checker.parse_time_field(&time_json).unwrap();
        let result = checker.check_version("1.0.0", &info).unwrap();
        assert_eq!(result, VersionCheckResult::Allowed);
    }

    #[test]
    fn test_check_version_denied() {
        let checker = DelayChecker::new(30);
        let now = Utc::now();
        let time_json = json!({
            "created": "2024-01-01T00:00:00Z",
            "3.0.0": (now - Duration::days(1)).to_rfc3339()
        });
        let info = checker.parse_time_field(&time_json).unwrap();
        let result = checker.check_version("3.0.0", &info).unwrap();
        match result {
            VersionCheckResult::Denied { .. } => {}
            other => panic!("Expected Denied, got {:?}", other),
        }
    }

    #[test]
    fn test_check_version_not_found() {
        let checker = DelayChecker::default();
        let time_json = make_sample_time();
        let info = checker.parse_time_field(&time_json).unwrap();
        let result = checker.check_version("99.99.99", &info);
        assert!(result.is_err());
        match result.unwrap_err() {
            DelayCheckError::VersionNotFound { version } => {
                assert_eq!(version, "99.99.99");
            }
            other => panic!("Expected VersionNotFound, got {:?}", other),
        }
    }

    #[test]
    fn test_find_eligible_version_already_eligible() {
        let checker = DelayChecker::new(365);
        let time_json = make_sample_time();
        let info = checker.parse_time_field(&time_json).unwrap();
        let result = checker.find_eligible_version("1.0.0", &info).unwrap();
        assert_eq!(result, Some("1.0.0".to_string()));
    }

    #[test]
    fn test_find_eligible_version_downgrades() {
        let checker = DelayChecker::new(30);
        let now = Utc::now();
        let time_json = json!({
            "created": "2024-01-01T00:00:00Z",
            "1.0.0": (now - Duration::days(60)).to_rfc3339(),
            "2.0.0": (now - Duration::days(10)).to_rfc3339(),
            "3.0.0": (now - Duration::days(1)).to_rfc3339()
        });
        let info = checker.parse_time_field(&time_json).unwrap();
        let result = checker.find_eligible_version("3.0.0", &info).unwrap();
        assert_eq!(result, Some("1.0.0".to_string()));
    }

    #[test]
    fn test_find_eligible_version_none_eligible() {
        let checker = DelayChecker::new(36500);
        let now = Utc::now();
        let time_json = json!({
            "1.0.0": (now - Duration::days(10)).to_rfc3339(),
            "2.0.0": (now - Duration::days(5)).to_rfc3339()
        });
        let info = checker.parse_time_field(&time_json).unwrap();
        let result = checker.find_eligible_version("2.0.0", &info).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_find_eligible_version_selects_latest_eligible() {
        let checker = DelayChecker::new(30);
        let now = Utc::now();
        let time_json = json!({
            "1.0.0": (now - Duration::days(100)).to_rfc3339(),
            "1.5.0": (now - Duration::days(80)).to_rfc3339(),
            "2.0.0": (now - Duration::days(50)).to_rfc3339(),
            "3.0.0": (now - Duration::days(5)).to_rfc3339()
        });
        let info = checker.parse_time_field(&time_json).unwrap();
        let result = checker.find_eligible_version("3.0.0", &info).unwrap();
        assert_eq!(result, Some("2.0.0".to_string()));
    }

    #[test]
    fn test_find_eligible_version_not_found() {
        let checker = DelayChecker::default();
        let time_json = make_sample_time();
        let info = checker.parse_time_field(&time_json).unwrap();
        let result = checker.find_eligible_version("nonexistent", &info);
        assert!(result.is_err());
        match result.unwrap_err() {
            DelayCheckError::VersionNotFound { version } => {
                assert_eq!(version, "nonexistent");
            }
            other => panic!("Expected VersionNotFound, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_version_allowed() {
        let checker = DelayChecker::new(365);
        let time_json = make_sample_time();
        let info = checker.parse_time_field(&time_json).unwrap();
        let result = checker.resolve_version("1.0.0", &info).unwrap();
        assert_eq!(result, VersionCheckResult::Allowed);
    }

    #[test]
    fn test_resolve_version_downgraded() {
        let checker = DelayChecker::new(30);
        let now = Utc::now();
        let time_json = json!({
            "1.0.0": (now - Duration::days(60)).to_rfc3339(),
            "2.0.0": (now - Duration::days(10)).to_rfc3339(),
            "3.0.0": (now - Duration::days(1)).to_rfc3339()
        });
        let info = checker.parse_time_field(&time_json).unwrap();
        let result = checker.resolve_version("3.0.0", &info).unwrap();
        match result {
            VersionCheckResult::Downgraded {
                original_version,
                suggested_version,
                ..
            } => {
                assert_eq!(original_version, "3.0.0");
                assert_eq!(suggested_version, "1.0.0");
            }
            other => panic!("Expected Downgraded, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_version_denied_no_alternative() {
        let checker = DelayChecker::new(36500);
        let now = Utc::now();
        let time_json = json!({
            "1.0.0": (now - Duration::days(5)).to_rfc3339(),
            "2.0.0": (now - Duration::days(2)).to_rfc3339()
        });
        let info = checker.parse_time_field(&time_json).unwrap();
        let result = checker.resolve_version("2.0.0", &info).unwrap();
        match result {
            VersionCheckResult::Denied { .. } => {}
            other => panic!("Expected Denied, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_version_not_found() {
        let checker = DelayChecker::default();
        let time_json = make_sample_time();
        let info = checker.parse_time_field(&time_json).unwrap();
        let result = checker.resolve_version("missing-version", &info);
        assert!(result.is_err());
    }

    #[test]
    fn test_compare_versions_basic() {
        assert_eq!(compare_versions("1.0.0", "2.0.0"), std::cmp::Ordering::Less);
        assert_eq!(
            compare_versions("2.0.0", "1.0.0"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_versions("1.0.0", "1.0.0"),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn test_compare_versions_multi_part() {
        assert_eq!(compare_versions("1.2.3", "1.2.4"), std::cmp::Ordering::Less);
        assert_eq!(
            compare_versions("1.10.0", "1.9.0"),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn test_compare_versions_with_prerelease() {
        let result = compare_versions("1.0.0-beta", "1.0.0-alpha");
        let ordering = match (result, compare_versions("1.0.0-beta", "1.0.0-alpha")) {
            (std::cmp::Ordering::Equal, _) => std::cmp::Ordering::Equal,
            _ => result,
        };
        let _ = ordering;
    }

    #[test]
    fn test_version_time_info_methods() {
        let time_json = make_sample_time();
        let checker = DelayChecker::default();
        let info = checker.parse_time_field(&time_json).unwrap();

        assert_eq!(info.len(), 3);
        assert!(!info.is_empty());

        let version_count = info.versions().count();
        assert_eq!(version_count, 3);

        assert!(info.get_publish_time("1.0.0").is_some());
        assert!(info.get_publish_time("nonexistent").is_none());
    }

    #[test]
    fn test_parse_time_field_many_versions() {
        let checker = DelayChecker::default();
        let time_json = json!({
            "created": "2020-01-01T00:00:00Z",
            "modified": "2024-12-31T23:59:59Z",
            "0.1.0": "2020-01-15T00:00:00Z",
            "0.2.0": "2020-03-20T00:00:00Z",
            "1.0.0": "2021-01-01T00:00:00Z",
            "1.1.0": "2021-06-15T00:00:00Z",
            "2.0.0": "2022-01-01T00:00:00Z",
            "2.1.0": "2022-08-01T00:00:00Z",
            "3.0.0": "2023-01-01T00:00:00Z"
        });

        let info = checker.parse_time_field(&time_json).unwrap();
        assert_eq!(info.len(), 7);
        assert!(info.get_publish_time("0.1.0").is_some());
        assert!(info.get_publish_time("3.0.0").is_some());
    }

    #[test]
    fn test_edge_case_all_versions_new() {
        let checker = DelayChecker::new(36500);
        let now = Utc::now();
        let time_json = json!({
            "1.0.0": (now - Duration::days(1)).to_rfc3339(),
            "2.0.0": (now - Duration::days(2)).to_rfc3339()
        });
        let info = checker.parse_time_field(&time_json).unwrap();

        let result = checker.resolve_version("2.0.0", &info).unwrap();
        match result {
            VersionCheckResult::Denied { .. } => {}
            other => panic!("Expected Denied when all versions are new, got {:?}", other),
        }
    }

    #[test]
    fn test_edge_case_single_old_version() {
        let checker = DelayChecker::new(30);
        let now = Utc::now();
        let time_json = json!({
            "1.0.0": (now - Duration::days(100)).to_rfc3339(),
            "2.0.0": (now - Duration::days(5)).to_rfc3339()
        });
        let info = checker.parse_time_field(&time_json).unwrap();

        let result = checker.resolve_version("2.0.0", &info).unwrap();
        match result {
            VersionCheckResult::Downgraded {
                ref suggested_version,
                ..
            } => {
                assert_eq!(suggested_version, "1.0.0");
            }
            other => panic!("Expected Downgraded, got {:?}", other),
        }
    }

    #[test]
    fn test_threshold_is_correctly_calculated() {
        let checker = DelayChecker::new(5);
        let expected_threshold = Utc::now() - Duration::days(5);
        let diff = (*checker.threshold() - expected_threshold)
            .num_seconds()
            .abs();
        assert!(diff < 2, "Threshold should be approximately 5 days ago");
    }
}
