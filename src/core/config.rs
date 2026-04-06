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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            delay_days: 3,
            npm_registry: "https://registry.npmmirror.com".to_string(),
            npm_download_registry: "https://registry.npmjs.org".to_string(),
            gomod_registry: "https://mirrors.aliyun.com/goproxy/".to_string(),
            gomod_download_registry: "https://proxy.golang.org".to_string(),
            pypi_registry: "https://pypi.org/simple/".to_string(),
            pypi_simple_index: "https://pypi.org/simple".to_string(),
            pypi_json_api_base: "https://pypi.org/pypi".to_string(),
            pypi_download_base: "https://files.pythonhosted.org/packages".to_string(),
            pypi_download_mirror: "https://mirrors.aliyun.com/pypi/packages".to_string(),
            allowlist_enabled: false,
            debug_mode: false,
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
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Config {
    pub fn from_std_env() -> Self {
        Self::from_env_vars(|key| std::env::var(key).ok())
    }
}
