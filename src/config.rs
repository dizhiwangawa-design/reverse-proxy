use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub upstream: UpstreamConfig,
    pub rotation: RotationConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ServerConfig {
    pub bind: String,
    /// Empty means: load from data/admin_token.txt, or generate a new one.
    pub admin_token: String,
    /// `None` keeps whatever the admin UI persisted in `data/state.json`.
    pub require_api_key: Option<bool>,
    pub cors_origins: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct UpstreamConfig {
    pub endpoint: String,
    pub timeout_secs: u64,
    pub connect_timeout_secs: u64,
    pub user_agent: String,
    pub product_version: String,
    pub product_type: String,
    pub ide_type: String,
    pub ide_name: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct RotationConfig {
    // All fields are optional so that a value changed in the admin UI is not
    // silently overwritten by the built-in defaults on the next start.
    pub strategy: Option<String>,
    pub max_retries: Option<usize>,
    pub cooldown_secs: Option<u64>,
    pub failure_cooldown_secs: Option<u64>,
    pub refresh_before_secs: Option<i64>,
    pub sync_local_auth: Option<bool>,
    pub local_auth_path: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8317".into(),
            admin_token: String::new(),
            require_api_key: None,
            cors_origins: vec!["*".into()],
        }
    }
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            endpoint: "https://copilot.tencent.com".into(),
            timeout_secs: 600,
            connect_timeout_secs: 20,
            user_agent: "WorkBuddy/5.5.6 (CLI/2.137.1)".into(),
            product_version: "5.5.6".into(),
            product_type: "WorkBuddy".into(),
            ide_type: "WorkBuddy".into(),
            ide_name: "WorkBuddy".into(),
        }
    }
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}

impl Config {
    pub fn load() -> Result<(Self, PathBuf)> {
        let data_dir = std::env::var("WB_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("data"));
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("creating data dir {}", data_dir.display()))?;

        let config_path = std::env::var("WB_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("config.toml"));

        let mut cfg = if config_path.exists() {
            let raw = std::fs::read_to_string(&config_path)
                .with_context(|| format!("reading {}", config_path.display()))?;
            toml::from_str(&raw).with_context(|| format!("parsing {}", config_path.display()))?
        } else {
            Config::default()
        };

        if let Ok(v) = std::env::var("WB_PROXY_BIND") {
            cfg.server.bind = v;
        }
        if let Ok(v) = std::env::var("WB_ADMIN_TOKEN") {
            cfg.server.admin_token = v;
        }
        if let Ok(v) = std::env::var("WB_UPSTREAM_ENDPOINT") {
            cfg.upstream.endpoint = v;
        }
        if let Ok(v) = std::env::var("WB_REQUIRE_API_KEY") {
            cfg.server.require_api_key = Some(matches!(v.as_str(), "1" | "true" | "yes"));
        }
        if let Ok(v) = std::env::var("WB_LOCAL_AUTH_PATH") {
            cfg.rotation.local_auth_path = Some(v);
        }

        Ok((cfg, data_dir))
    }

    pub fn admin_token(&self, data_dir: &Path) -> Result<String> {
        if !self.server.admin_token.trim().is_empty() {
            return Ok(self.server.admin_token.trim().to_string());
        }
        let path = data_dir.join("admin_token.txt");
        if let Ok(token) = std::fs::read_to_string(&path) {
            let token = token.trim().to_string();
            if !token.is_empty() {
                return Ok(token);
            }
        }
        let token = format!("wbadmin_{}", uuid::Uuid::new_v4().simple());
        std::fs::write(&path, &token)?;
        set_private_permissions(&path);
        Ok(token)
    }
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_does_not_override_ui_settings() {
        let cfg = Config::default();
        assert!(cfg.server.require_api_key.is_none());
        assert!(cfg.rotation.strategy.is_none());
        assert!(cfg.rotation.sync_local_auth.is_none());
        assert_eq!(cfg.server.bind, "127.0.0.1:8317");
    }

    #[test]
    fn explicit_toml_values_are_optional_overrides() {
        let cfg: Config = toml::from_str(
            r#"
            [server]
            require_api_key = false
            [rotation]
            strategy = "weighted"
            max_retries = 7
            "#,
        )
        .expect("parse config");
        assert_eq!(cfg.server.require_api_key, Some(false));
        assert_eq!(cfg.rotation.strategy.as_deref(), Some("weighted"));
        assert_eq!(cfg.rotation.max_retries, Some(7));
        assert!(cfg.rotation.cooldown_secs.is_none());
    }

    #[test]
    fn expands_tilde_for_home_paths() {
        let home = std::env::var("HOME").unwrap_or_default();
        let expanded = expand_tilde("~/auth.info");
        if home.is_empty() {
            assert_eq!(expanded, PathBuf::from("~/auth.info"));
        } else {
            assert_eq!(expanded, PathBuf::from(home).join("auth.info"));
        }
        assert_eq!(
            expand_tilde("/tmp/auth.info"),
            PathBuf::from("/tmp/auth.info")
        );
    }
}
