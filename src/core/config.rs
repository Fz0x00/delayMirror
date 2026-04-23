pub struct Config {
    pub delay_days: i64,
    pub npm_registry: String,
    pub npm_download_registry: String,
    pub gomod_registry: String,
    pub gomod_download_registry: String,
    pub pypi_registry: String,
    pub pypi_simple_index: String,
    pub pypi_json_api_base: String,
    pub pypi_download_base: String,
    pub pypi_download_mirror: String,
    pub allowlist_enabled: bool,
    pub debug_mode: bool,
    pub pypi_filter_mode: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            delay_days: 3,
            npm_registry: "https://registry.npmmirror.com".to_string(),
            npm_download_registry: "https://registry.npmjs.org".to_string(),
            gomod_registry: "https://mirrors.aliyun.com/goproxy".to_string(),
            gomod_download_registry: "https://proxy.golang.org".to_string(),
            pypi_registry: "https://pypi.org/simple/".to_string(),
            pypi_simple_index: "https://pypi.org/simple".to_string(),
            pypi_json_api_base: "https://pypi.org/pypi".to_string(),
            pypi_download_base: "https://files.pythonhosted.org/packages".to_string(),
            pypi_download_mirror: "https://mirrors.aliyun.com/pypi/packages".to_string(),
            allowlist_enabled: false,
            debug_mode: false,
            pypi_filter_mode: None,
        }
    }
}

impl Config {
    pub fn from_env_vars(get_var: impl Fn(&str) -> Option<String>) -> Self {
        let default = Self::default();
        Self {
            delay_days: get_var("DELAY_DAYS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(default.delay_days),
            npm_registry: get_var("NPM_REGISTRY").unwrap_or(default.npm_registry),
            npm_download_registry: get_var("NPM_DOWNLOAD_REGISTRY")
                .unwrap_or_else(|| default.npm_download_registry.clone()),
            gomod_registry: get_var("GOMOD_REGISTRY").unwrap_or(default.gomod_registry),
            gomod_download_registry: get_var("GOMOD_DOWNLOAD_REGISTRY")
                .unwrap_or_else(|| default.gomod_download_registry.clone()),
            pypi_simple_index: get_var("PYPI_SIMPLE_INDEX")
                .or_else(|| get_var("PYPI_REGISTRY"))
                .unwrap_or(default.pypi_simple_index),
            pypi_registry: get_var("PYPI_REGISTRY").unwrap_or(default.pypi_registry),
            pypi_json_api_base: get_var("PYPI_JSON_API_BASE").unwrap_or(default.pypi_json_api_base),
            pypi_download_base: get_var("PYPI_DOWNLOAD_BASE")
                .unwrap_or_else(|| default.pypi_download_base.clone()),
            pypi_download_mirror: get_var("PYPI_DOWNLOAD_MIRROR")
                .unwrap_or(default.pypi_download_mirror),
            allowlist_enabled: get_var("ALLOWLIST_ENABLED")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(default.allowlist_enabled),
            debug_mode: get_var("DEBUG_MODE")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(default.debug_mode),
            pypi_filter_mode: get_var("PYPI_FILTER_MODE").filter(|s| !s.is_empty()),
        }
    }

    pub fn gomod_base_url(&self) -> &str {
        self.gomod_registry.trim_end_matches('/')
    }

    pub fn gomod_download_base_url(&self) -> &str {
        self.gomod_download_registry.trim_end_matches('/')
    }

    pub fn gomod_meta_url(&self, escaped_module: &str, path: &str) -> String {
        format!("{}/{}{}", self.gomod_base_url(), escaped_module, path)
    }

    pub fn gomod_download_url(&self, escaped_module: &str, path: &str) -> String {
        format!(
            "{}/{}{}",
            self.gomod_download_base_url(),
            escaped_module,
            path
        )
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Config {
    pub fn from_std_env() -> Self {
        Self::from_env_vars(|key| std::env::var(key).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_values() {
        let config = Config::default();
        assert_eq!(config.delay_days, 3);
        assert_eq!(config.npm_registry, "https://registry.npmmirror.com");
        assert_eq!(
            config.npm_download_registry,
            "https://registry.npmjs.org"
        );
        assert_eq!(
            config.gomod_registry,
            "https://mirrors.aliyun.com/goproxy"
        );
        assert_eq!(
            config.gomod_download_registry,
            "https://proxy.golang.org"
        );
        assert_eq!(config.pypi_registry, "https://pypi.org/simple/");
        assert_eq!(config.pypi_simple_index, "https://pypi.org/simple");
        assert_eq!(config.pypi_json_api_base, "https://pypi.org/pypi");
        assert_eq!(
            config.pypi_download_base,
            "https://files.pythonhosted.org/packages"
        );
        assert_eq!(
            config.pypi_download_mirror,
            "https://mirrors.aliyun.com/pypi/packages"
        );
        assert!(!config.allowlist_enabled);
        assert!(!config.debug_mode);
        assert!(config.pypi_filter_mode.is_none());
    }

    #[test]
    fn test_from_env_vars_empty() {
        let config = Config::from_env_vars(|_| None);
        // Should match defaults when no env vars set
        assert_eq!(config.delay_days, 3);
        assert_eq!(config.npm_registry, "https://registry.npmmirror.com");
        assert!(!config.allowlist_enabled);
        assert!(!config.debug_mode);
    }

    #[test]
    fn test_from_env_vars_delay_days() {
        let config = Config::from_env_vars(|key| match key {
            "DELAY_DAYS" => Some("7".to_string()),
            _ => None,
        });
        assert_eq!(config.delay_days, 7);
    }

    #[test]
    fn test_from_env_vars_delay_days_invalid() {
        let config = Config::from_env_vars(|key| match key {
            "DELAY_DAYS" => Some("not_a_number".to_string()),
            _ => None,
        });
        // Falls back to default
        assert_eq!(config.delay_days, 3);
    }

    #[test]
    fn test_from_env_vars_npm_registry() {
        let config = Config::from_env_vars(|key| match key {
            "NPM_REGISTRY" => Some("https://custom.registry.com".to_string()),
            _ => None,
        });
        assert_eq!(config.npm_registry, "https://custom.registry.com");
    }

    #[test]
    fn test_from_env_vars_npm_download_registry() {
        let config = Config::from_env_vars(|key| match key {
            "NPM_DOWNLOAD_REGISTRY" => Some("https://download.registry.com".to_string()),
            _ => None,
        });
        assert_eq!(
            config.npm_download_registry,
            "https://download.registry.com"
        );
    }

    #[test]
    fn test_from_env_vars_gomod_registries() {
        let config = Config::from_env_vars(|key| match key {
            "GOMOD_REGISTRY" => Some("https://custom.goproxy.com".to_string()),
            "GOMOD_DOWNLOAD_REGISTRY" => Some("https://custom.download.com".to_string()),
            _ => None,
        });
        assert_eq!(config.gomod_registry, "https://custom.goproxy.com");
        assert_eq!(
            config.gomod_download_registry,
            "https://custom.download.com"
        );
    }

    #[test]
    fn test_from_env_vars_pypi_settings() {
        let config = Config::from_env_vars(|key| match key {
            "PYPI_SIMPLE_INDEX" => Some("https://custom.pypi.org/simple".to_string()),
            "PYPI_JSON_API_BASE" => Some("https://custom.pypi.org/pypi".to_string()),
            "PYPI_DOWNLOAD_BASE" => Some("https://custom.files.org".to_string()),
            "PYPI_DOWNLOAD_MIRROR" => Some("https://custom.mirror.org".to_string()),
            _ => None,
        });
        assert_eq!(
            config.pypi_simple_index,
            "https://custom.pypi.org/simple"
        );
        assert_eq!(
            config.pypi_json_api_base,
            "https://custom.pypi.org/pypi"
        );
        assert_eq!(config.pypi_download_base, "https://custom.files.org");
        assert_eq!(
            config.pypi_download_mirror,
            "https://custom.mirror.org"
        );
    }

    #[test]
    fn test_from_env_vars_pypi_registry_fallback() {
        // PYPI_REGISTRY should also set pypi_simple_index if PYPI_SIMPLE_INDEX not set
        let config = Config::from_env_vars(|key| match key {
            "PYPI_REGISTRY" => Some("https://fallback.pypi.org/simple/".to_string()),
            _ => None,
        });
        assert_eq!(
            config.pypi_simple_index,
            "https://fallback.pypi.org/simple/"
        );
    }

    #[test]
    fn test_from_env_vars_pypi_simple_index_priority() {
        // PYPI_SIMPLE_INDEX takes priority over PYPI_REGISTRY
        let config = Config::from_env_vars(|key| match key {
            "PYPI_SIMPLE_INDEX" => Some("https://primary.pypi.org/simple".to_string()),
            "PYPI_REGISTRY" => Some("https://fallback.pypi.org/simple/".to_string()),
            _ => None,
        });
        assert_eq!(
            config.pypi_simple_index,
            "https://primary.pypi.org/simple"
        );
    }

    #[test]
    fn test_from_env_vars_allowlist_enabled_true() {
        let config = Config::from_env_vars(|key| match key {
            "ALLOWLIST_ENABLED" => Some("true".to_string()),
            _ => None,
        });
        assert!(config.allowlist_enabled);
    }

    #[test]
    fn test_from_env_vars_allowlist_enabled_one() {
        let config = Config::from_env_vars(|key| match key {
            "ALLOWLIST_ENABLED" => Some("1".to_string()),
            _ => None,
        });
        assert!(config.allowlist_enabled);
    }

    #[test]
    fn test_from_env_vars_allowlist_enabled_false() {
        let config = Config::from_env_vars(|key| match key {
            "ALLOWLIST_ENABLED" => Some("false".to_string()),
            _ => None,
        });
        assert!(!config.allowlist_enabled);
    }

    #[test]
    fn test_from_env_vars_allowlist_enabled_random() {
        let config = Config::from_env_vars(|key| match key {
            "ALLOWLIST_ENABLED" => Some("random".to_string()),
            _ => None,
        });
        assert!(!config.allowlist_enabled);
    }

    #[test]
    fn test_from_env_vars_debug_mode() {
        let config = Config::from_env_vars(|key| match key {
            "DEBUG_MODE" => Some("true".to_string()),
            _ => None,
        });
        assert!(config.debug_mode);
    }

    #[test]
    fn test_from_env_vars_pypi_filter_mode() {
        let config = Config::from_env_vars(|key| match key {
            "PYPI_FILTER_MODE" => Some("strict".to_string()),
            _ => None,
        });
        assert_eq!(config.pypi_filter_mode, Some("strict".to_string()));
    }

    #[test]
    fn test_from_env_vars_pypi_filter_mode_empty() {
        let config = Config::from_env_vars(|key| match key {
            "PYPI_FILTER_MODE" => Some("".to_string()),
            _ => None,
        });
        assert!(config.pypi_filter_mode.is_none());
    }

    #[test]
    fn test_gomod_base_url_trailing_slash() {
        let config = Config::from_env_vars(|key| match key {
            "GOMOD_REGISTRY" => Some("https://proxy.golang.org/".to_string()),
            _ => None,
        });
        assert_eq!(config.gomod_base_url(), "https://proxy.golang.org");
    }

    #[test]
    fn test_gomod_base_url_no_trailing_slash() {
        let config = Config::from_env_vars(|key| match key {
            "GOMOD_REGISTRY" => Some("https://proxy.golang.org".to_string()),
            _ => None,
        });
        assert_eq!(config.gomod_base_url(), "https://proxy.golang.org");
    }

    #[test]
    fn test_gomod_download_base_url_trailing_slash() {
        let config = Config::from_env_vars(|key| match key {
            "GOMOD_DOWNLOAD_REGISTRY" => Some("https://download.golang.org/".to_string()),
            _ => None,
        });
        assert_eq!(
            config.gomod_download_base_url(),
            "https://download.golang.org"
        );
    }

    #[test]
    fn test_gomod_meta_url() {
        let config = Config::from_env_vars(|key| match key {
            "GOMOD_REGISTRY" => Some("https://proxy.golang.org".to_string()),
            _ => None,
        });
        let url = config.gomod_meta_url("github.com/gin-gonic/gin", "/@v/list");
        assert_eq!(
            url,
            "https://proxy.golang.org/github.com/gin-gonic/gin/@v/list"
        );
    }

    #[test]
    fn test_gomod_download_url() {
        let config = Config::from_env_vars(|key| match key {
            "GOMOD_DOWNLOAD_REGISTRY" => Some("https://dl.golang.org".to_string()),
            _ => None,
        });
        let url = config.gomod_download_url(
            "github.com/gin-gonic/!gin",
            "/@v/v1.9.1.zip",
        );
        assert_eq!(
            url,
            "https://dl.golang.org/github.com/gin-gonic/!gin/@v/v1.9.1.zip"
        );
    }

    #[test]
    fn test_from_env_vars_multiple_overrides() {
        let config = Config::from_env_vars(|key| match key {
            "DELAY_DAYS" => Some("14".to_string()),
            "NPM_REGISTRY" => Some("https://npm.example.com".to_string()),
            "DEBUG_MODE" => Some("1".to_string()),
            "ALLOWLIST_ENABLED" => Some("true".to_string()),
            "PYPI_FILTER_MODE" => Some("strict".to_string()),
            _ => None,
        });
        assert_eq!(config.delay_days, 14);
        assert_eq!(config.npm_registry, "https://npm.example.com");
        assert!(config.debug_mode);
        assert!(config.allowlist_enabled);
        assert_eq!(config.pypi_filter_mode, Some("strict".to_string()));
        // Non-overridden values keep defaults
        assert_eq!(
            config.gomod_registry,
            "https://mirrors.aliyun.com/goproxy"
        );
    }
}
