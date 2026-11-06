use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Normalize epoch timestamps from JWT/local-auth files to milliseconds.
/// Older state files could contain JWT `exp` values in seconds.
pub fn normalize_epoch_ms(value: i64) -> i64 {
    const MILLISECOND_THRESHOLD: i64 = 100_000_000_000;
    if value > 0 && value < MILLISECOND_THRESHOLD {
        value.saturating_mul(1000)
    } else {
        value
    }
}

pub fn jwt_expires_at_ms(token: &str) -> Option<i64> {
    decode_jwt_claims(token)
        .and_then(|claims| claims.exp)
        .map(normalize_epoch_ms)
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AccountStats {
    pub requests: u64,
    pub successes: u64,
    pub failures: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_latency_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub uid: String,
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub refresh_expires_at: Option<i64>,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub invalid: bool,
    #[serde(default)]
    pub cooldown_until: Option<i64>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_used_at: Option<i64>,
    #[serde(default)]
    pub last_refresh_at: Option<i64>,
    #[serde(default = "default_weight")]
    pub weight: u32,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub stats: AccountStats,
}

fn default_true() -> bool {
    true
}
fn default_weight() -> u32 {
    1
}

impl Account {
    pub fn new(name: String, access_token: String) -> Self {
        let claims = decode_jwt_claims(&access_token);
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            uid: claims
                .as_ref()
                .and_then(|c| c.sub.clone())
                .unwrap_or_default(),
            access_token,
            refresh_token: None,
            expires_at: claims.as_ref().and_then(|c| c.exp).map(normalize_epoch_ms),
            refresh_expires_at: None,
            endpoint: None,
            enabled: true,
            invalid: false,
            cooldown_until: None,
            last_error: None,
            last_used_at: None,
            last_refresh_at: None,
            weight: 1,
            created_at: now_ms(),
            stats: AccountStats::default(),
        }
    }

    pub fn public_status(&self, now: i64) -> &'static str {
        if !self.enabled {
            "disabled"
        } else if self.invalid {
            "invalid"
        } else if self.cooldown_until.unwrap_or(0) > now {
            "cooldown"
        } else {
            "active"
        }
    }

    pub fn is_available(&self, now: i64) -> bool {
        self.enabled && !self.invalid && self.cooldown_until.unwrap_or(0) <= now
    }

    pub fn effective_endpoint<'a>(&'a self, default_endpoint: &'a str) -> &'a str {
        self.endpoint.as_deref().unwrap_or(default_endpoint)
    }

    pub fn masked_token(&self) -> String {
        let t = &self.access_token;
        if t.len() <= 16 {
            return "****".into();
        }
        format!("{}...{}", &t[..10], &t[t.len() - 6..])
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: String,
    pub name: String,
    pub key: String,
    pub enabled: bool,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub request_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub rotation_strategy: String,
    pub max_retries: usize,
    pub cooldown_secs: u64,
    pub failure_cooldown_secs: u64,
    pub refresh_before_secs: i64,
    pub sync_local_auth: bool,
    pub local_auth_path: String,
    pub require_api_key: bool,
    pub default_model: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            rotation_strategy: "round_robin".into(),
            max_retries: 3,
            cooldown_secs: 60,
            failure_cooldown_secs: 15,
            refresh_before_secs: 300,
            sync_local_auth: true,
            local_auth_path:
                "~/.local/share/CodeBuddyExtension/Data/Public/auth/workbuddy-desktop.info".into(),
            require_api_key: true,
            default_model: "auto".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Totals {
    pub requests: u64,
    pub successes: u64,
    pub failures: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_latency_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRecord {
    pub at: i64,
    pub model: String,
    pub account_id: String,
    pub account_name: String,
    pub stream: bool,
    pub ok: bool,
    pub status: u16,
    pub latency_ms: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Store {
    pub accounts: Vec<Account>,
    pub api_keys: Vec<ApiKey>,
    pub settings: Settings,
    pub totals: Totals,
    /// Bounded request log, persisted so the dashboard survives restarts.
    #[serde(default)]
    pub recent: VecDeque<RequestRecord>,
}

impl Store {
    pub fn load(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join("state.json");
        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let mut store: Store = serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", path.display()))?;
            for account in &mut store.accounts {
                if let Some(expires_at) = account.expires_at {
                    account.expires_at = Some(normalize_epoch_ms(expires_at));
                }
                if let Some(expires_at) = account.refresh_expires_at {
                    account.refresh_expires_at = Some(normalize_epoch_ms(expires_at));
                }
            }
            // Older state files did not persist the request log.
            if store.recent.is_empty() {
                store.recent = VecDeque::with_capacity(100);
            }
            Ok(store)
        } else {
            let mut store = Store {
                accounts: Vec::new(),
                api_keys: Vec::new(),
                settings: Settings::default(),
                totals: Totals::default(),
                recent: VecDeque::with_capacity(100),
            };
            store.ensure_default_key();
            Ok(store)
        }
    }

    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("state.json")
    }

    pub fn ensure_default_key(&mut self) -> String {
        if let Some(k) = self.api_keys.iter().find(|k| k.enabled) {
            return k.key.clone();
        }
        let key = format!("sk-wb-{}", uuid::Uuid::new_v4().simple());
        self.api_keys.push(ApiKey {
            id: uuid::Uuid::new_v4().to_string(),
            name: "default".into(),
            key: key.clone(),
            enabled: true,
            created_at: now_ms(),
            last_used_at: None,
            request_count: 0,
        });
        key
    }

    pub fn find_key(&mut self, key: &str) -> Option<&mut ApiKey> {
        self.api_keys.iter_mut().find(|k| k.enabled && k.key == key)
    }

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        let path = Self::path(data_dir);
        // Unique temp names prevent concurrent writers from truncating each
        // other's file before the atomic rename.
        let tmp = data_dir.join(format!(
            "state.json.tmp.{}.{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let raw = serde_json::to_vec_pretty(self)?;
        let result = (|| {
            std::fs::write(&tmp, raw).with_context(|| format!("writing {}", tmp.display()))?;
            set_private_permissions(&tmp);
            std::fs::rename(&tmp, &path).with_context(|| format!("renaming {}", path.display()))
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result?;
        Ok(())
    }

    pub fn record(&mut self, rec: RequestRecord) {
        self.totals.requests += 1;
        if rec.ok {
            self.totals.successes += 1;
        } else {
            self.totals.failures += 1;
        }
        self.totals.prompt_tokens += rec.prompt_tokens;
        self.totals.completion_tokens += rec.completion_tokens;
        self.totals.total_latency_ms += rec.latency_ms;
        if let Some(acc) = self.accounts.iter_mut().find(|a| a.id == rec.account_id) {
            acc.stats.requests += 1;
            if rec.ok {
                acc.stats.successes += 1;
            } else {
                acc.stats.failures += 1;
            }
            acc.stats.prompt_tokens += rec.prompt_tokens;
            acc.stats.completion_tokens += rec.completion_tokens;
            acc.stats.total_latency_ms += rec.latency_ms;
        }
        if self.recent.len() >= 100 {
            self.recent.pop_front();
        }
        self.recent.push_back(rec);
    }
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) {}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct JwtClaims {
    pub sub: Option<String>,
    pub exp: Option<i64>,
    pub iat: Option<i64>,
    pub preferred_username: Option<String>,
    pub email: Option<String>,
}

pub fn decode_jwt_claims(token: &str) -> Option<JwtClaims> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| {
            base64::engine::general_purpose::URL_SAFE.decode(payload.trim_end_matches('='))
        })
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn jwt_with_payload(payload: &str) -> String {
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
        format!("header.{encoded}.signature")
    }

    fn test_store() -> Store {
        let mut account = Account::new("primary".into(), "token".into());
        account.id = "account-1".into();
        Store {
            accounts: vec![account],
            api_keys: Vec::new(),
            settings: Settings::default(),
            totals: Totals::default(),
            recent: VecDeque::with_capacity(100),
        }
    }

    fn record(ok: bool) -> RequestRecord {
        RequestRecord {
            at: now_ms(),
            model: "auto".into(),
            account_id: "account-1".into(),
            account_name: "primary".into(),
            stream: false,
            ok,
            status: if ok { 200 } else { 502 },
            latency_ms: 100,
            prompt_tokens: 10,
            completion_tokens: 5,
            error: if ok { None } else { Some("boom".into()) },
        }
    }

    #[test]
    fn decodes_jwt_claims() {
        let token = jwt_with_payload(r#"{"sub":"user-1","exp":1234567890}"#);
        let claims = decode_jwt_claims(&token).expect("claims");
        assert_eq!(claims.sub.as_deref(), Some("user-1"));
        assert_eq!(claims.exp, Some(1_234_567_890));
    }

    #[test]
    fn normalizes_jwt_expiry_to_milliseconds() {
        let token = jwt_with_payload(r#"{"exp":1234567890}"#);
        let account = Account::new("primary".into(), token.clone());
        assert_eq!(account.expires_at, Some(1_234_567_890_000));
        assert_eq!(jwt_expires_at_ms(&token), Some(1_234_567_890_000));
    }

    #[test]
    fn normalizes_legacy_second_timestamps_on_load() {
        let dir = std::env::temp_dir().join(format!(
            "wb-proxy-store-timestamp-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let mut store = test_store();
        store.accounts[0].expires_at = Some(1_234_567_890);
        store.save(&dir).expect("save");
        let loaded = Store::load(&dir).expect("load");
        assert_eq!(loaded.accounts[0].expires_at, Some(1_234_567_890_000));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn simulated_shutdown_flushes_before_reload() {
        let dir = std::env::temp_dir().join(format!(
            "wb-proxy-store-shutdown-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let mut store = test_store();
        store.record(record(true));
        store.save(&dir).expect("save before exit");
        drop(store);

        let loaded = Store::load(&dir).expect("reload after exit");
        assert_eq!(loaded.totals.requests, 1);
        assert_eq!(loaded.recent.len(), 1);
        assert_eq!(loaded.accounts[0].stats.requests, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_invalid_jwt() {
        assert!(decode_jwt_claims("not-a-jwt").is_none());
        assert!(decode_jwt_claims("a.b.c").is_none());
    }

    #[test]
    fn account_status_follows_flags() {
        let mut account = Account::new("primary".into(), "token".into());
        assert_eq!(account.public_status(now_ms()), "active");
        account.cooldown_until = Some(now_ms() + 60_000);
        assert_eq!(account.public_status(now_ms()), "cooldown");
        assert!(!account.is_available(now_ms()));
        account.invalid = true;
        assert_eq!(account.public_status(now_ms()), "invalid");
        account.invalid = false;
        account.enabled = false;
        assert_eq!(account.public_status(now_ms()), "disabled");
    }

    #[test]
    fn record_updates_totals_and_account_stats() {
        let mut store = test_store();
        store.record(record(true));
        store.record(record(false));
        assert_eq!(store.totals.requests, 2);
        assert_eq!(store.totals.successes, 1);
        assert_eq!(store.totals.failures, 1);
        assert_eq!(store.totals.prompt_tokens, 20);
        assert_eq!(store.totals.completion_tokens, 10);
        let stats = &store.accounts[0].stats;
        assert_eq!(stats.requests, 2);
        assert_eq!(stats.successes, 1);
        assert_eq!(stats.failures, 1);
        assert_eq!(store.recent.len(), 2);
    }

    #[test]
    fn record_keeps_recent_bounded() {
        let mut store = test_store();
        for _ in 0..105 {
            store.record(record(true));
        }
        assert_eq!(store.recent.len(), 100);
    }

    #[test]
    fn state_roundtrips_with_request_log() {
        let dir = std::env::temp_dir().join(format!(
            "wb-proxy-store-test-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let mut store = test_store();
        store.record(record(true));
        store.save(&dir).expect("save");
        let loaded = Store::load(&dir).expect("load");
        assert_eq!(loaded.totals.requests, 1);
        assert_eq!(loaded.recent.len(), 1);
        assert_eq!(loaded.accounts[0].name, "primary");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
