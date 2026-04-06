use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

const DELAY_EVENT_TYPE: &str = "version_check";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PackageType {
    Npm,
    GoMod,
    PyPI,
}

impl fmt::Display for PackageType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Npm => write!(f, "npm"),
            Self::GoMod => write!(f, "gomod"),
            Self::PyPI => write!(f, "pypi"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DelayAction {
    Allowed,
    Denied,
    Downgraded,
}

impl fmt::Display for DelayAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Allowed => write!(f, "allowed"),
            Self::Denied => write!(f, "denied"),
            Self::Downgraded => write!(f, "downgraded"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelayLogEntry {
    pub timestamp: DateTime<Utc>,
    #[serde(rename = "event")]
    pub event: String,
    pub package_type: PackageType,
    #[serde(rename = "package")]
    pub package_name: String,
    pub original_version: String,
    pub actual_version: Option<String>,
    pub action: DelayAction,
    pub reason: String,
    pub client_ip: Option<String>,
}

impl DelayLogEntry {
    pub fn new(
        package_type: PackageType,
        package_name: String,
        original_version: String,
        actual_version: Option<String>,
        action: DelayAction,
        reason: String,
        client_ip: Option<String>,
    ) -> Self {
        Self {
            timestamp: Utc::now(),
            event: DELAY_EVENT_TYPE.to_string(),
            package_type,
            package_name,
            original_version,
            actual_version,
            action,
            reason,
            client_ip,
        }
    }
}

pub struct DelayLogger;

impl DelayLogger {
    pub fn new() -> Self {
        Self
    }

    pub fn log(&self, entry: &DelayLogEntry) {
        if let Ok(json_line) = serde_json::to_string(entry) {
            #[cfg(all(not(test), feature = "workers"))]
            worker::console_log!("{}", json_line);
            #[cfg(any(test, not(feature = "workers")))]
            eprintln!("{}", json_line);
        }
    }

    pub fn log_blocked(
        &self,
        package_type: PackageType,
        package_name: &str,
        original_version: &str,
        reason: &str,
        client_ip: Option<&str>,
    ) {
        let entry = DelayLogEntry::new(
            package_type,
            package_name.to_string(),
            original_version.to_string(),
            None,
            DelayAction::Denied,
            reason.to_string(),
            client_ip.map(|s| s.to_string()),
        );
        self.log(&entry);
    }

    pub fn log_downgraded(
        &self,
        package_type: PackageType,
        package_name: &str,
        original_version: &str,
        actual_version: &str,
        reason: &str,
        client_ip: Option<&str>,
    ) {
        let entry = DelayLogEntry::new(
            package_type,
            package_name.to_string(),
            original_version.to_string(),
            Some(actual_version.to_string()),
            DelayAction::Downgraded,
            reason.to_string(),
            client_ip.map(|s| s.to_string()),
        );
        self.log(&entry);
    }
}

impl Default for DelayLogger {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_package_type_display() {
        assert_eq!(PackageType::Npm.to_string(), "npm");
        assert_eq!(PackageType::GoMod.to_string(), "gomod");
        assert_eq!(PackageType::PyPI.to_string(), "pypi");
    }

    #[test]
    fn test_delay_action_display() {
        assert_eq!(DelayAction::Allowed.to_string(), "allowed");
        assert_eq!(DelayAction::Denied.to_string(), "denied");
        assert_eq!(DelayAction::Downgraded.to_string(), "downgraded");
    }

    #[test]
    fn test_delay_log_entry_new_denied() {
        let entry = DelayLogEntry::new(
            PackageType::Npm,
            "axios".to_string(),
            "1.6.0".to_string(),
            None,
            DelayAction::Denied,
            "Version too recent".to_string(),
            Some("192.168.1.100".to_string()),
        );

        assert_eq!(entry.package_type, PackageType::Npm);
        assert_eq!(entry.package_name, "axios");
        assert_eq!(entry.original_version, "1.6.0");
        assert!(entry.actual_version.is_none());
        assert_eq!(entry.action, DelayAction::Denied);
        assert_eq!(entry.reason, "Version too recent");
        assert_eq!(entry.client_ip, Some("192.168.1.100".to_string()));
        assert_eq!(entry.event, "version_check");
    }

    #[test]
    fn test_delay_log_entry_new_downgraded() {
        let entry = DelayLogEntry::new(
            PackageType::GoMod,
            "github.com/gin-gonic/gin".to_string(),
            "v1.10.0".to_string(),
            Some("v1.9.1".to_string()),
            DelayAction::Downgraded,
            "Auto-downgraded for security".to_string(),
            None,
        );

        assert_eq!(entry.package_type, PackageType::GoMod);
        assert_eq!(entry.original_version, "v1.10.0");
        assert_eq!(entry.actual_version, Some("v1.9.1".to_string()));
        assert_eq!(entry.action, DelayAction::Downgraded);
        assert!(entry.client_ip.is_none());
    }

    #[test]
    fn test_delay_log_entry_serialization_denied() {
        let entry = DelayLogEntry::new(
            PackageType::Npm,
            "lodash".to_string(),
            "4.18.0".to_string(),
            None,
            DelayAction::Denied,
            "Published 1 day ago, below 3-day threshold".to_string(),
            Some("10.0.0.1".to_string()),
        );

        let json = serde_json::to_string(&entry).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["event"], "version_check");
        assert_eq!(parsed["package_type"], "npm");
        assert_eq!(parsed["package"], "lodash");
        assert_eq!(parsed["original_version"], "4.18.0");
        assert!(parsed.get("actual_version").is_none() || parsed["actual_version"].is_null());
        assert_eq!(parsed["action"], "denied");
        assert_eq!(
            parsed["reason"],
            "Published 1 day ago, below 3-day threshold"
        );
        assert_eq!(parsed["client_ip"], "10.0.0.1");
        assert!(parsed.get("timestamp").is_some());
    }

    #[test]
    fn test_delay_log_entry_serialization_downgraded() {
        let entry = DelayLogEntry::new(
            PackageType::PyPI,
            "numpy".to_string(),
            "2.0.0".to_string(),
            Some("1.26.4".to_string()),
            DelayAction::Downgraded,
            "Version published 2 days ago, below 3-day threshold".to_string(),
            Some("172.16.0.5".to_string()),
        );

        let json = serde_json::to_string(&entry).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["event"], "version_check");
        assert_eq!(parsed["package_type"], "pypi");
        assert_eq!(parsed["package"], "numpy");
        assert_eq!(parsed["original_version"], "2.0.0");
        assert_eq!(parsed["actual_version"], "1.26.4");
        assert_eq!(parsed["action"], "downgraded");
        assert_eq!(
            parsed["reason"],
            "Version published 2 days ago, below 3-day threshold"
        );
        assert_eq!(parsed["client_ip"], "172.16.0.5");
    }

    #[test]
    fn test_delay_log_entry_deserialization_roundtrip() {
        let original = DelayLogEntry::new(
            PackageType::GoMod,
            "github.com/google/uuid".to_string(),
            "v2.0.0".to_string(),
            Some("v1.6.0".to_string()),
            DelayAction::Downgraded,
            "Security delay policy".to_string(),
            None,
        );

        let json = serde_json::to_string(&original).unwrap();
        let restored: DelayLogEntry = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.package_type, original.package_type);
        assert_eq!(restored.package_name, original.package_name);
        assert_eq!(restored.original_version, original.original_version);
        assert_eq!(restored.actual_version, original.actual_version);
        assert_eq!(restored.action, original.action);
        assert_eq!(restored.reason, original.reason);
        assert_eq!(restored.client_ip, original.client_ip);
    }

    #[test]
    fn test_default_impl() {
        let logger = DelayLogger;
        logger.log_blocked(PackageType::Npm, "default-test", "1.0.0", "test", None);
    }

    #[test]
    fn test_package_type_equality() {
        assert_eq!(PackageType::Npm, PackageType::Npm);
        assert_ne!(PackageType::Npm, PackageType::GoMod);
        assert_ne!(PackageType::GoMod, PackageType::PyPI);
    }

    #[test]
    fn test_delay_action_equality() {
        assert_eq!(DelayAction::Allowed, DelayAction::Allowed);
        assert_ne!(DelayAction::Allowed, DelayAction::Denied);
        assert_ne!(DelayAction::Denied, DelayAction::Downgraded);
    }

    #[test]
    fn test_delay_log_entry_with_empty_client_ip() {
        let entry = DelayLogEntry::new(
            PackageType::Npm,
            "test-pkg".to_string(),
            "1.0.0".to_string(),
            None,
            DelayAction::Allowed,
            "Test with no IP".to_string(),
            None,
        );

        assert!(entry.client_ip.is_none());
        assert_eq!(entry.action, DelayAction::Allowed);

        let json = serde_json::to_string(&entry).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("client_ip").is_none() || parsed["client_ip"].is_null());
    }

    #[test]
    fn test_delay_log_entry_long_reason() {
        let long_reason = "A".repeat(500);
        let entry = DelayLogEntry::new(
            PackageType::PyPI,
            "long-reason-pkg".to_string(),
            "1.0.0".to_string(),
            None,
            DelayAction::Denied,
            long_reason.clone(),
            None,
        );

        assert_eq!(entry.reason.len(), 500);

        let json = serde_json::to_string(&entry).unwrap();
        let restored: DelayLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.reason, long_reason);
    }

    #[test]
    fn test_delay_log_entry_special_characters_in_package_name() {
        let entry = DelayLogEntry::new(
            PackageType::Npm,
            "@scope/package-name".to_string(),
            "1.0.0".to_string(),
            Some("0.9.0".to_string()),
            DelayAction::Downgraded,
            "Special chars test".to_string(),
            None,
        );

        assert_eq!(entry.package_name, "@scope/package-name");

        let json = serde_json::to_string(&entry).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["package"], "@scope/package-name");
    }

    #[test]
    fn test_multiple_entries_different_types() {
        let npm_entry = DelayLogEntry::new(
            PackageType::Npm,
            "npm-pkg".to_string(),
            "2.0.0".to_string(),
            None,
            DelayAction::Denied,
            "NPM test".to_string(),
            Some("192.168.1.1".to_string()),
        );

        let gomod_entry = DelayLogEntry::new(
            PackageType::GoMod,
            "gomod-pkg".to_string(),
            "v3.0.0".to_string(),
            Some("v2.0.0".to_string()),
            DelayAction::Downgraded,
            "GoMod test".to_string(),
            Some("10.0.0.1".to_string()),
        );

        let pypi_entry = DelayLogEntry::new(
            PackageType::PyPI,
            "pypi-pkg".to_string(),
            "1.5.0".to_string(),
            None,
            DelayAction::Allowed,
            "PyPI test".to_string(),
            None,
        );

        assert_eq!(npm_entry.package_type, PackageType::Npm);
        assert_eq!(gomod_entry.package_type, PackageType::GoMod);
        assert_eq!(pypi_entry.package_type, PackageType::PyPI);

        assert_eq!(npm_entry.action, DelayAction::Denied);
        assert_eq!(gomod_entry.action, DelayAction::Downgraded);
        assert_eq!(pypi_entry.action, DelayAction::Allowed);
    }

    #[test]
    fn test_timestamp_is_auto_generated() {
        let before = Utc::now();
        let entry = DelayLogEntry::new(
            PackageType::Npm,
            "timestamp-test".to_string(),
            "1.0.0".to_string(),
            None,
            DelayAction::Allowed,
            "Timestamp test".to_string(),
            None,
        );
        let after = Utc::now();

        assert!(entry.timestamp >= before);
        assert!(entry.timestamp <= after);
    }

    #[test]
    fn test_event_type_is_constant() {
        let entry1 = DelayLogEntry::new(
            PackageType::Npm,
            "pkg1".to_string(),
            "1.0.0".to_string(),
            None,
            DelayAction::Allowed,
            "Test".to_string(),
            None,
        );

        let entry2 = DelayLogEntry::new(
            PackageType::GoMod,
            "pkg2".to_string(),
            "2.0.0".to_string(),
            Some("1.0.0".to_string()),
            DelayAction::Denied,
            "Test 2".to_string(),
            Some("127.0.0.1".to_string()),
        );

        assert_eq!(entry1.event, "version_check");
        assert_eq!(entry2.event, "version_check");
    }
}
