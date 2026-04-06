use crate::core::delay_logger::PackageType;
use serde::{Deserialize, Serialize};
use worker::Env;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AllowlistConfig {
    pub npm: Option<Vec<String>>,
    pub gomod: Option<Vec<String>>,
    pub pypi: Option<Vec<String>>,
}

pub struct AllowlistManager {
    config: AllowlistConfig,
    enabled: bool,
}

impl AllowlistManager {
    pub fn from_env(env: &Env) -> Self {
        let config = match env.var("ALLOWLIST_JSON") {
            Ok(var) => serde_json::from_str(&var.to_string()).unwrap_or_default(),
            Err(_) => AllowlistConfig::default(),
        };
        let enabled = env.var("ALLOWLIST_JSON").is_ok();
        Self { config, enabled }
    }

    pub fn from_config(config: AllowlistConfig) -> Self {
        let has_entries = config.npm.as_ref().is_some_and(|v| !v.is_empty())
            || config.gomod.as_ref().is_some_and(|v| !v.is_empty())
            || config.pypi.as_ref().is_some_and(|v| !v.is_empty());
        Self {
            config,
            enabled: has_entries,
        }
    }

    pub fn is_allowed(&self, package_type: &PackageType, name: &str) -> bool {
        if !self.enabled {
            return true;
        }
        let list = match package_type {
            PackageType::Npm => &self.config.npm,
            PackageType::GoMod => &self.config.gomod,
            PackageType::PyPI => &self.config.pypi,
        };
        match list {
            Some(entries) => entries.iter().any(|entry| Self::match_name(entry, name)),
            None => true,
        }
    }

    fn match_name(pattern: &str, name: &str) -> bool {
        if pattern.contains('*') {
            let re_pattern = pattern.replace('*', ".*");
            if let Ok(re) = regex::Regex::new(&format!("^{}$", re_pattern)) {
                return re.is_match(name);
            }
        }
        pattern == name
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn config(&self) -> &AllowlistConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_allowlist_allows_all() {
        let manager = AllowlistManager::from_config(AllowlistConfig::default());
        assert!(!manager.is_enabled());
        assert!(manager.is_allowed(&PackageType::Npm, "any-package"));
        assert!(manager.is_allowed(&PackageType::GoMod, "any-module"));
        assert!(manager.is_allowed(&PackageType::PyPI, "any-lib"));
    }

    #[test]
    fn test_exact_match() {
        let config = AllowlistConfig {
            npm: Some(vec!["lodash".to_string(), "axios".to_string()]),
            gomod: None,
            pypi: None,
        };
        let manager = AllowlistManager::from_config(config);
        assert!(manager.is_enabled());
        assert!(manager.is_allowed(&PackageType::Npm, "lodash"));
        assert!(manager.is_allowed(&PackageType::Npm, "axios"));
        assert!(!manager.is_allowed(&PackageType::Npm, "react"));
        assert!(manager.is_allowed(&PackageType::GoMod, "anything"));
    }

    #[test]
    fn test_wildcard_match() {
        let config = AllowlistConfig {
            npm: Some(vec!["react-*".to_string(), "@types/*".to_string()]),
            gomod: None,
            pypi: None,
        };
        let manager = AllowlistManager::from_config(config);
        assert!(manager.is_allowed(&PackageType::Npm, "react-dom"));
        assert!(manager.is_allowed(&PackageType::Npm, "react-router"));
        assert!(!manager.is_allowed(&PackageType::Npm, "react"));
        assert!(manager.is_allowed(&PackageType::Npm, "@types/node"));
        assert!(manager.is_allowed(&PackageType::Npm, "@types/react"));
    }

    #[test]
    fn test_all_package_types() {
        let config = AllowlistConfig {
            npm: Some(vec!["lodash".to_string()]),
            gomod: Some(vec!["github.com/gin-gonic/gin".to_string()]),
            pypi: Some(vec!["requests".to_string()]),
        };
        let manager = AllowlistManager::from_config(config);
        assert!(manager.is_allowed(&PackageType::Npm, "lodash"));
        assert!(!manager.is_allowed(&PackageType::Npm, "axios"));
        assert!(manager.is_allowed(&PackageType::GoMod, "github.com/gin-gonic/gin"));
        assert!(!manager.is_allowed(&PackageType::GoMod, "github.com/foo/bar"));
        assert!(manager.is_allowed(&PackageType::PyPI, "requests"));
        assert!(!manager.is_allowed(&PackageType::PyPI, "numpy"));
    }

    #[test]
    fn test_none_type_allows_all() {
        let config = AllowlistConfig {
            npm: Some(vec!["lodash".to_string()]),
            gomod: None,
            pypi: None,
        };
        let manager = AllowlistManager::from_config(config);
        assert!(manager.is_enabled());
        assert!(manager.is_allowed(&PackageType::GoMod, "anything-goes"));
        assert!(manager.is_allowed(&PackageType::PyPI, "anything-goes"));
    }

    #[test]
    fn test_empty_list_same_as_none() {
        let config_with_empty = AllowlistConfig {
            npm: Some(vec![]),
            gomod: None,
            pypi: None,
        };
        let manager = AllowlistManager::from_config(config_with_empty);
        assert!(!manager.is_enabled());
    }

    #[test]
    fn test_deserialize_from_json() {
        let json = r#"{"npm":["lodash","axios"],"gomod":["github.com/gin-gonic/gin"],"pypi":["requests"]}"#;
        let config: AllowlistConfig = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(config.npm.unwrap(), vec!["lodash", "axios"]);
        assert_eq!(config.gomod.unwrap(), vec!["github.com/gin-gonic/gin"]);
        assert_eq!(config.pypi.unwrap(), vec!["requests"]);
    }
}
