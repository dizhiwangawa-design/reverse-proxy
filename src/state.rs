use crate::config::{expand_tilde, Config};
use crate::error::ApiError;
pub use crate::store::now_ms;
use crate::store::{
    decode_jwt_claims, jwt_expires_at_ms, normalize_epoch_ms, Account, ApiKey, RequestRecord,
    Settings, Store,
};
use crate::upstream::{is_retryable, parse_upstream_error, UpstreamClient};
use anyhow::{Context, Result};
use axum::http::StatusCode;
use futures_util::StreamExt;
use reqwest::Response;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

pub struct AppState {
    pub config: Config,
    pub data_dir: PathBuf,
    pub admin_token: String,
    pub store: RwLock<Store>,
    pub http: UpstreamClient,
    pub rr: AtomicUsize,
    pub models: RwLock<ModelCache>,
    refresh_lock: Mutex<()>,
    save_lock: Mutex<()>,
    dirty: AtomicBool,
    pub started_at: i64,
}

#[derive(Debug, Clone, Default)]
pub struct ModelCache {
    pub fetched_at: i64,
    pub models: Vec<Value>,
    pub error: Option<String>,
}

pub type SharedState = Arc<AppState>;

impl AppState {
    pub async fn new(config: Config, data_dir: PathBuf, admin_token: String) -> Result<Self> {
        let mut store = Store::load(&data_dir)?;
        // Only values that were explicitly present in config.toml / env override
        // the settings persisted by the admin UI.
        if let Some(v) = config.rotation.strategy.clone() {
            store.settings.rotation_strategy = v;
        }
        if let Some(v) = config.rotation.max_retries {
            store.settings.max_retries = v;
        }
        if let Some(v) = config.rotation.cooldown_secs {
            store.settings.cooldown_secs = v;
        }
        if let Some(v) = config.rotation.failure_cooldown_secs {
            store.settings.failure_cooldown_secs = v;
        }
        if let Some(v) = config.rotation.refresh_before_secs {
            store.settings.refresh_before_secs = v;
        }
        if let Some(v) = config.rotation.sync_local_auth {
            store.settings.sync_local_auth = v;
        }
        if let Some(v) = config.rotation.local_auth_path.clone() {
            store.settings.local_auth_path = v;
        }
        if let Some(v) = config.server.require_api_key {
            store.settings.require_api_key = v;
        }
        store.ensure_default_key();
        store.save(&data_dir)?;

        let http = UpstreamClient::new(config.upstream.clone())?;
        let state = Self {
            config,
            data_dir,
            admin_token,
            store: RwLock::new(store),
            http,
            rr: AtomicUsize::new(0),
            models: RwLock::new(ModelCache::default()),
            refresh_lock: Mutex::new(()),
            save_lock: Mutex::new(()),
            dirty: AtomicBool::new(false),
            started_at: now_ms(),
        };

        // First-run convenience: import the local WorkBuddy session if present.
        {
            let has_accounts = state.store.read().await.accounts.is_empty();
            if has_accounts {
                if let Err(e) = state.import_local_auth().await {
                    tracing::info!("local WorkBuddy auth not imported: {e:#}");
                }
            }
        }

        Ok(state)
    }

    pub async fn save_now(&self) {
        let _guard = self.save_lock.lock().await;
        let store = self.store.read().await;
        if let Err(e) = store.save(&self.data_dir) {
            tracing::error!("failed to persist state: {e:#}");
            self.dirty.store(true, Ordering::Relaxed);
            return;
        }
        self.dirty.store(false, Ordering::Relaxed);
    }

    pub async fn save_if_dirty(&self) {
        let _guard = self.save_lock.lock().await;
        if self.dirty.swap(false, Ordering::Relaxed) {
            let store = self.store.read().await;
            if let Err(e) = store.save(&self.data_dir) {
                tracing::error!("failed to persist state: {e:#}");
                self.dirty.store(true, Ordering::Relaxed);
            }
        }
    }

    pub async fn settings(&self) -> Settings {
        self.store.read().await.settings.clone()
    }

    /// Resolve the model to send upstream. Requests may omit `model`; in that
    /// case the value configured in the admin UI is used (falling back to auto).
    pub async fn resolve_model(&self, requested: Option<&str>) -> String {
        if let Some(model) = requested.map(str::trim).filter(|m| !m.is_empty()) {
            return model.to_string();
        }
        let fallback = self.settings().await.default_model;
        let fallback = fallback.trim();
        if fallback.is_empty() {
            "auto".to_string()
        } else {
            fallback.to_string()
        }
    }

    // ---------------------------------------------------------------- accounts

    pub async fn accounts(&self) -> Vec<Account> {
        self.store.read().await.accounts.clone()
    }

    pub async fn account(&self, id: &str) -> Option<Account> {
        self.store
            .read()
            .await
            .accounts
            .iter()
            .find(|a| a.id == id || a.uid == id)
            .cloned()
    }

    pub async fn add_account(
        &self,
        name: String,
        access_token: String,
        refresh_token: Option<String>,
        uid: Option<String>,
        endpoint: Option<String>,
        weight: Option<u32>,
    ) -> Account {
        let mut acc = Account::new(name, access_token);
        if let Some(uid) = uid.filter(|v| !v.trim().is_empty()) {
            acc.uid = uid;
        }
        if let Some(rt) = refresh_token.filter(|v| !v.trim().is_empty()) {
            acc.refresh_token = Some(rt);
        }
        if let Some(ep) = endpoint.filter(|v| !v.trim().is_empty()) {
            acc.endpoint = Some(ep);
        }
        if let Some(w) = weight {
            acc.weight = w.max(1);
        }
        // Re-adding the same WorkBuddy account (same uid) should refresh its
        // credentials instead of creating a second, competing account entry.
        let mut store = self.store.write().await;
        let existing = if acc.uid.is_empty() {
            store
                .accounts
                .iter_mut()
                .find(|a| a.uid.is_empty() && a.access_token == acc.access_token)
        } else {
            store
                .accounts
                .iter_mut()
                .find(|a| !a.uid.is_empty() && a.uid == acc.uid)
        };
        if let Some(existing) = existing {
            existing.access_token = acc.access_token;
            if acc.refresh_token.is_some() {
                existing.refresh_token = acc.refresh_token;
            }
            existing.expires_at = acc.expires_at.or(existing.expires_at);
            if acc.endpoint.is_some() {
                existing.endpoint = acc.endpoint;
            }
            if weight.is_some() {
                existing.weight = acc.weight;
            }
            existing.invalid = false;
            existing.cooldown_until = None;
            existing.last_error = None;
            let out = existing.clone();
            drop(store);
            self.save_now().await;
            return out;
        }
        store.accounts.push(acc.clone());
        drop(store);
        self.save_now().await;
        acc
    }

    pub async fn delete_account(&self, id: &str) -> bool {
        let mut store = self.store.write().await;
        let before = store.accounts.len();
        store.accounts.retain(|a| a.id != id && a.uid != id);
        let removed = store.accounts.len() != before;
        drop(store);
        if removed {
            self.save_now().await;
        }
        removed
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn update_account(
        &self,
        id: &str,
        name: Option<String>,
        enabled: Option<bool>,
        weight: Option<u32>,
        endpoint: Option<Option<String>>,
        access_token: Option<String>,
        refresh_token: Option<String>,
    ) -> Option<Account> {
        let mut store = self.store.write().await;
        let acc = store
            .accounts
            .iter_mut()
            .find(|a| a.id == id || a.uid == id)?;
        if let Some(v) = name {
            acc.name = v;
        }
        if let Some(v) = enabled {
            acc.enabled = v;
            if v {
                acc.invalid = false;
                acc.cooldown_until = None;
            }
        }
        if let Some(v) = weight {
            acc.weight = v.max(1);
        }
        if let Some(v) = endpoint {
            acc.endpoint = v.filter(|s| !s.trim().is_empty());
        }
        if let Some(raw) = access_token {
            let token = raw.trim().trim_start_matches("Bearer ").trim().to_string();
            if !token.is_empty() {
                let claims = decode_jwt_claims(&token);
                acc.access_token = token;
                if let Some(exp) = claims.as_ref().and_then(|c| c.exp) {
                    acc.expires_at = Some(normalize_epoch_ms(exp));
                }
                if acc.uid.is_empty() {
                    acc.uid = claims
                        .as_ref()
                        .and_then(|c| c.sub.clone())
                        .unwrap_or_default();
                }
                acc.invalid = false;
                acc.cooldown_until = None;
                acc.last_error = None;
            }
        }
        if let Some(raw) = refresh_token {
            let token = raw.trim().to_string();
            if !token.is_empty() {
                acc.refresh_token = Some(token);
            }
        }
        let out = acc.clone();
        drop(store);
        self.save_now().await;
        Some(out)
    }

    pub async fn refresh_account(&self, id: &str, force: bool) -> Result<Account, ApiError> {
        let _guard = self.refresh_lock.lock().await;
        let acc = self.account(id).await.ok_or_else(|| {
            ApiError::new(StatusCode::NOT_FOUND, "not_found", "account not found")
        })?;
        let Some(rt) = acc.refresh_token.clone() else {
            return Err(ApiError::bad_request("account has no refresh token"));
        };
        if !force {
            if let Some(exp) = acc.expires_at {
                if exp - now_ms() > 60_000 {
                    return Ok(acc);
                }
            }
        }
        let endpoint = acc.effective_endpoint(&self.config.upstream.endpoint);
        let result = self.http.refresh(endpoint, &rt).await;
        match result {
            Ok(r) => {
                let claims = decode_jwt_claims(&r.access_token);
                let expires_at = r
                    .expires_in
                    .map(|s| now_ms() + s * 1000)
                    .or_else(|| claims.as_ref().and_then(|c| c.exp).map(normalize_epoch_ms));
                let refresh_expires_at = r.refresh_expires_in.map(|s| now_ms() + s * 1000);
                let mut store = self.store.write().await;
                let Some(a) = store
                    .accounts
                    .iter_mut()
                    .find(|a| a.id == acc.id || a.uid == acc.uid)
                else {
                    return Err(ApiError::new(
                        StatusCode::NOT_FOUND,
                        "not_found",
                        "account disappeared",
                    ));
                };
                a.access_token = r.access_token;
                if let Some(rt) = r.refresh_token {
                    a.refresh_token = Some(rt);
                }
                a.expires_at = expires_at;
                a.refresh_expires_at = refresh_expires_at;
                a.last_refresh_at = Some(now_ms());
                a.last_error = None;
                a.invalid = false;
                a.cooldown_until = None;
                let out = a.clone();
                drop(store);
                self.save_now().await;
                tracing::info!(account = %out.name, "token refreshed");
                Ok(out)
            }
            Err(e) => {
                let msg = format!("{e:#}");
                let invalid = msg.contains("HTTP 401") || msg.contains("HTTP 403");
                let mut store = self.store.write().await;
                if let Some(a) = store.accounts.iter_mut().find(|a| a.id == acc.id) {
                    a.last_error = Some(crate::upstream::truncate(&msg, 300));
                    if invalid {
                        a.invalid = true;
                    }
                }
                drop(store);
                self.save_now().await;
                Err(ApiError::upstream(
                    StatusCode::UNAUTHORIZED,
                    format!("token refresh failed: {msg}"),
                    None,
                ))
            }
        }
    }

    async fn ensure_token(&self, acc: &Account) -> Result<Account, ApiError> {
        let s = self.settings().await;
        let soon = acc
            .expires_at
            .map(|exp| exp - now_ms() <= s.refresh_before_secs * 1000)
            .unwrap_or(false);
        if soon && acc.refresh_token.is_some() {
            return self.refresh_account(&acc.id, true).await;
        }
        if acc.expires_at.map(|exp| exp <= now_ms()).unwrap_or(false) && acc.refresh_token.is_none()
        {
            return Err(ApiError::upstream(
                StatusCode::UNAUTHORIZED,
                "access token expired and no refresh token is available",
                None,
            ));
        }
        Ok(acc.clone())
    }

    async fn mark_failure(&self, acc: &Account, status: StatusCode, message: &str) {
        let s = self.settings().await;
        let now = now_ms();
        let cooldown = if status == StatusCode::TOO_MANY_REQUESTS {
            s.cooldown_secs
        } else {
            s.failure_cooldown_secs
        };
        let mut store = self.store.write().await;
        if let Some(a) = store.accounts.iter_mut().find(|a| a.id == acc.id) {
            a.last_error = Some(crate::upstream::truncate(message, 400));
            a.cooldown_until = Some(now + cooldown as i64 * 1000);
            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                a.invalid = true;
            }
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    async fn mark_success(&self, acc: &Account, latency_ms: u64, usage: Option<&Value>) {
        let mut store = self.store.write().await;
        if let Some(a) = store.accounts.iter_mut().find(|a| a.id == acc.id) {
            a.last_used_at = Some(now_ms());
            a.last_error = None;
            let _ = (latency_ms, usage);
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub async fn record_request(&self, rec: RequestRecord) {
        let mut store = self.store.write().await;
        store.record(rec);
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub async fn select_accounts(&self, exclude: &HashSet<String>) -> Vec<Account> {
        let store = self.store.read().await;
        let s = &store.settings;
        let now = now_ms();
        let mut list: Vec<Account> = store
            .accounts
            .iter()
            .filter(|a| !exclude.contains(&a.id))
            .filter(|a| a.is_available(now))
            .filter(|a| {
                !(a.expires_at.map(|e| e <= now).unwrap_or(false) && a.refresh_token.is_none())
            })
            .cloned()
            .collect();
        if list.is_empty() {
            return list;
        }
        match s.rotation_strategy.as_str() {
            "least_recent" | "least-recent" => {
                list.sort_by_key(|a| (a.last_used_at.unwrap_or(0), a.stats.requests));
            }
            "weighted" => {
                let mut out = Vec::with_capacity(list.len());
                let mut pool = list;
                while !pool.is_empty() {
                    let total: u64 = pool.iter().map(|a| a.weight.max(1) as u64).sum();
                    let mut pick = rand::random_range(0..total);
                    let mut idx = 0;
                    for (i, a) in pool.iter().enumerate() {
                        let w = a.weight.max(1) as u64;
                        if pick < w {
                            idx = i;
                            break;
                        }
                        pick -= w;
                    }
                    out.push(pool.remove(idx));
                }
                list = out;
            }
            _ => {
                let start = self.rr.fetch_add(1, Ordering::Relaxed) % list.len();
                list.rotate_left(start);
            }
        }
        list.truncate(s.max_retries.max(1));
        list
    }

    // ------------------------------------------------------------------ chat

    pub async fn chat_with_rotation(
        &self,
        body: &Value,
        model: &str,
    ) -> Result<(Account, Response), ApiError> {
        let attempts = self.select_accounts(&HashSet::new()).await;
        if attempts.is_empty() {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "no_available_account",
                "no WorkBuddy account is currently available; add one in the admin UI",
            ));
        }
        let mut last_error = String::from("all accounts failed");
        for acc in attempts {
            let acc = match self.ensure_token(&acc).await {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!(account = %acc.name, "token unavailable: {}", e.message);
                    last_error = e.message;
                    continue;
                }
            };
            let endpoint = acc
                .effective_endpoint(&self.config.upstream.endpoint)
                .to_string();
            let started = std::time::Instant::now();
            let resp = match self
                .http
                .chat(&endpoint, &acc.id, &acc.access_token, &acc.uid, model, body)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let msg = format!("{e:#}");
                    tracing::warn!(account = %acc.name, "upstream transport error: {msg}");
                    self.mark_failure(&acc, StatusCode::BAD_GATEWAY, &msg).await;
                    last_error = msg;
                    continue;
                }
            };
            let status = resp.status();
            if status.is_success() {
                let latency = started.elapsed().as_millis() as u64;
                self.mark_success(&acc, latency, None).await;
                return Ok((acc, resp));
            }

            let text = resp.text().await.unwrap_or_default();
            let (msg, code) = parse_upstream_error(status, &text);
            last_error = msg.clone();
            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                // A 401 can be caused by a token rotated elsewhere; try once.
                match self.refresh_account(&acc.id, true).await {
                    Ok(fresh) => {
                        let endpoint = fresh
                            .effective_endpoint(&self.config.upstream.endpoint)
                            .to_string();
                        if let Ok(r) = self
                            .http
                            .chat(
                                &endpoint,
                                &fresh.id,
                                &fresh.access_token,
                                &fresh.uid,
                                model,
                                body,
                            )
                            .await
                        {
                            if r.status().is_success() {
                                self.mark_success(
                                    &fresh,
                                    started.elapsed().as_millis() as u64,
                                    None,
                                )
                                .await;
                                return Ok((fresh, r));
                            }
                            let st = r.status();
                            let t = r.text().await.unwrap_or_default();
                            let (m, _) = parse_upstream_error(st, &t);
                            last_error = m;
                        }
                    }
                    Err(e) => {
                        last_error = e.message;
                    }
                }
                self.mark_failure(&acc, StatusCode::UNAUTHORIZED, &last_error)
                    .await;
                continue;
            }
            if is_retryable(status) {
                self.mark_failure(&acc, status, &msg).await;
                continue;
            }
            return Err(ApiError {
                status,
                message: msg,
                kind: "upstream_error".into(),
                code,
                request_id: None,
            });
        }
        Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "all_accounts_failed",
            last_error,
        ))
    }

    // ----------------------------------------------------------------- models

    pub async fn refresh_models(&self) -> Result<Vec<Value>, ApiError> {
        let accounts = self.accounts().await;
        let candidates: Vec<Account> = accounts
            .into_iter()
            .filter(|a| a.is_available(now_ms()))
            .collect();
        if candidates.is_empty() {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "no_available_account",
                "no account available to fetch models",
            ));
        }
        let mut last_error = String::from("no account could fetch models");
        for acc in candidates {
            let acc = match self.ensure_token(&acc).await {
                Ok(acc) => acc,
                Err(e) => {
                    last_error = e.message;
                    continue;
                }
            };
            let endpoint = acc
                .effective_endpoint(&self.config.upstream.endpoint)
                .to_string();
            match self
                .http
                .fetch_models(&endpoint, &acc.access_token, &acc.uid)
                .await
            {
                Ok(v) => {
                    let models = v
                        .get("data")
                        .and_then(|d| d.get("models"))
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    if models.is_empty() {
                        last_error = "upstream returned an empty model catalogue".into();
                        continue;
                    }
                    let mut cache = self.models.write().await;
                    cache.fetched_at = now_ms();
                    cache.models = models.clone();
                    cache.error = None;
                    return Ok(models);
                }
                Err(e) => {
                    last_error = format!("{e:#}");
                }
            }
        }
        let mut cache = self.models.write().await;
        cache.error = Some(last_error.clone());
        Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "models_error",
            last_error,
        ))
    }

    pub async fn models(&self) -> (Vec<Value>, Option<String>) {
        let cache = self.models.read().await.clone();
        if !cache.models.is_empty() {
            return (cache.models, cache.error);
        }
        (fallback_models(), cache.error)
    }

    // ------------------------------------------------------------- local auth

    pub async fn local_auth_path(&self) -> Option<PathBuf> {
        let s = self.settings().await.local_auth_path;
        let path = expand_tilde(&s);
        if path.exists() {
            Some(path)
        } else {
            None
        }
    }

    pub fn read_local_auth(path: &Path) -> Result<LocalAuth> {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let v: Value =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        let account = v.get("account").cloned().unwrap_or(Value::Null);
        let auth = v.get("auth").cloned().unwrap_or(Value::Null);
        let access = auth
            .get("accessToken")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("no auth.accessToken in {}", path.display()))?;
        let uid = account
            .get("uid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let name = account
            .get("nickname")
            .and_then(Value::as_str)
            .or_else(|| account.get("phoneNumber").and_then(Value::as_str))
            .unwrap_or("WorkBuddy")
            .to_string();
        let refresh = auth
            .get("refreshToken")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let expires_at = auth
            .get("expiresAt")
            .and_then(Value::as_i64)
            .map(normalize_epoch_ms)
            .or_else(|| jwt_expires_at_ms(access));
        let refresh_expires_at = auth
            .get("refreshExpiresAt")
            .and_then(Value::as_i64)
            .map(normalize_epoch_ms);
        Ok(LocalAuth {
            uid,
            name,
            access_token: access.to_string(),
            refresh_token: refresh,
            expires_at,
            refresh_expires_at,
        })
    }

    pub async fn import_local_auth(&self) -> Result<Account, ApiError> {
        let configured_path = self.settings().await.local_auth_path;
        let path = self.local_auth_path().await.ok_or_else(|| {
            ApiError::bad_request(format!(
                "local WorkBuddy auth file not found: {configured_path}"
            ))
        })?;
        let local = Self::read_local_auth(&path)
            .map_err(|e| ApiError::bad_request(format!("failed to read local auth: {e:#}")))?;
        let mut store = self.store.write().await;
        if let Some(a) = store.accounts.iter_mut().find(|a| a.uid == local.uid) {
            a.access_token = local.access_token;
            a.refresh_token = local.refresh_token;
            a.expires_at = local.expires_at;
            a.refresh_expires_at = local.refresh_expires_at;
            a.invalid = false;
            a.cooldown_until = None;
            let out = a.clone();
            drop(store);
            self.save_now().await;
            return Ok(out);
        }
        let mut acc = Account::new(local.name, local.access_token);
        acc.uid = local.uid;
        acc.refresh_token = local.refresh_token;
        acc.expires_at = local.expires_at.or(acc.expires_at);
        acc.refresh_expires_at = local.refresh_expires_at;
        store.accounts.push(acc.clone());
        drop(store);
        self.save_now().await;
        Ok(acc)
    }

    /// Pull fresh tokens written by the desktop app, but never overwrite a token
    /// that is newer than the one on disk.
    pub async fn sync_local_auth(&self) {
        let Some(path) = self.local_auth_path().await else {
            return;
        };
        let Ok(local) = Self::read_local_auth(&path) else {
            return;
        };
        let mut store = self.store.write().await;
        let Some(a) = store.accounts.iter_mut().find(|a| a.uid == local.uid) else {
            return;
        };
        let file_newer = match (local.expires_at, a.expires_at) {
            (Some(f), Some(cur)) => f > cur + 1000,
            (Some(_), None) => true,
            _ => false,
        };
        if local.access_token != a.access_token && file_newer {
            a.access_token = local.access_token;
            if local.refresh_token.is_some() {
                a.refresh_token = local.refresh_token;
            }
            a.expires_at = local.expires_at;
            a.refresh_expires_at = local.refresh_expires_at;
            a.invalid = false;
            a.cooldown_until = None;
            drop(store);
            self.save_now().await;
            tracing::info!("synced rotated token from local WorkBuddy auth file");
        }
    }

    // ------------------------------------------------------------------- keys

    pub async fn create_key(&self, name: String) -> ApiKey {
        let key = ApiKey {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            key: format!("sk-wb-{}", uuid::Uuid::new_v4().simple()),
            enabled: true,
            created_at: now_ms(),
            last_used_at: None,
            request_count: 0,
        };
        self.store.write().await.api_keys.push(key.clone());
        self.save_now().await;
        key
    }

    pub async fn delete_key(&self, id: &str) -> bool {
        let mut store = self.store.write().await;
        let before = store.api_keys.len();
        store.api_keys.retain(|k| k.id != id);
        let removed = store.api_keys.len() != before;
        drop(store);
        if removed {
            self.save_now().await;
        }
        removed
    }

    pub async fn validate_api_key(&self, key: &str) -> bool {
        let mut store = self.store.write().await;
        if let Some(k) = store.find_key(key) {
            k.last_used_at = Some(now_ms());
            k.request_count += 1;
            drop(store);
            self.dirty.store(true, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    pub async fn update_settings(&self, new: Settings) {
        let mut store = self.store.write().await;
        store.settings = new;
        drop(store);
        self.save_now().await;
    }

    pub async fn overview(&self) -> Value {
        let store = self.store.read().await;
        let now = now_ms();
        let accounts: Vec<Value> = store
            .accounts
            .iter()
            .map(|a| {
                json!({
                    "id": a.id,
                    "name": a.name,
                    "uid": a.uid,
                    "status": a.public_status(now),
                    "enabled": a.enabled,
                    "expires_at": a.expires_at,
                    "last_used_at": a.last_used_at,
                    "last_error": a.last_error,
                    "stats": a.stats,
                })
            })
            .collect();
        json!({
            "started_at": self.started_at,
            "now": now,
            "totals": store.totals,
            "accounts": accounts,
            "recent": store.recent,
            "settings": store.settings,
            "upstream": {
                "endpoint": self.config.upstream.endpoint,
            },
            "models_cached": self.models.read().await.models.len(),
        })
    }
}

pub struct LocalAuth {
    pub uid: String,
    pub name: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>,
    pub refresh_expires_at: Option<i64>,
}

pub fn fallback_models() -> Vec<Value> {
    // Snapshot of the upstream catalogue, used only until the first successful
    // `/v2/enterprises/personal/models` refresh.
    const MODELS: &[(&str, &str, i64, i64, bool, bool, bool)] = &[
        ("auto", "Auto", 256000, 32000, true, true, true),
        ("hy4free", "Hy4", 960000, 64000, true, true, true),
        ("hy4std", "Hy4", 960000, 64000, true, true, true),
        ("fast-model", "快速", 300000, 48000, true, true, true),
        ("balanced-model", "均衡", 300000, 48000, true, true, true),
        ("deep-model", "极致", 300000, 48000, true, true, true),
        ("hy3", "Hy3", 192000, 64000, true, true, true),
        ("hy3-x", "Hy3", 192000, 64000, true, true, true),
        (
            "hy4-preview-x",
            "Hy4 preview",
            960000,
            64000,
            true,
            true,
            true,
        ),
        ("default", "Default", 200000, 24000, true, true, false),
        ("glm-5.3", "GLM-5.3", 1000000, 64000, true, true, true),
        (
            "glm-5.3-flash",
            "GLM-5.3-Flash",
            1000000,
            131072,
            true,
            true,
            true,
        ),
        ("glm-5.2", "GLM-5.2", 1000000, 64000, true, true, true),
        ("glm-5.0", "GLM-5.0", 200000, 48000, true, true, false),
        ("glm-4.7", "GLM-4.7", 200000, 48000, true, true, false),
        ("glm-4.6", "GLM-4.6", 168000, 32000, true, true, false),
        ("glm-4.6v", "GLM-4.6V", 128000, 32000, true, true, true),
        (
            "minimax-m2.5",
            "MiniMax-M2.5",
            200000,
            48000,
            true,
            true,
            false,
        ),
        ("kimi-k3-1", "Kimi-K3", 1000000, 32000, true, true, true),
        (
            "kimi-k2.8-preview",
            "Kimi-K2.8-Preview",
            1000000,
            64000,
            true,
            true,
            true,
        ),
        ("kimi-k2.5", "Kimi-K2.5", 164000, 32000, true, true, true),
        (
            "kimi-k2-thinking",
            "Kimi-K2-Thinking",
            164000,
            32000,
            true,
            true,
            false,
        ),
        (
            "deepseek-v4-flash",
            "Deepseek-V4-Flash",
            1000000,
            50000,
            true,
            true,
            true,
        ),
        (
            "deepseek-v4.1-flash",
            "DeepSeek-V4.1-Flash",
            1000000,
            128000,
            true,
            true,
            true,
        ),
        (
            "deepseek-v4-pro",
            "DeepSeek-V4-Pro",
            1000000,
            128000,
            true,
            true,
            true,
        ),
        (
            "deepseek-v3-2-volc",
            "DeepSeek-V3.2",
            96000,
            32000,
            true,
            true,
            false,
        ),
        (
            "hunyuan-2.0-thinking",
            "Hunyuan-2.0-Thinking",
            128000,
            24000,
            true,
            true,
            false,
        ),
        (
            "hunyuan-chat",
            "Hunyuan-Turbos",
            200000,
            8192,
            true,
            true,
            false,
        ),
        (
            "hunyuan-image-alpha",
            "Hunyuan Image Alpha",
            0,
            0,
            true,
            true,
            true,
        ),
        (
            "hunyuan-image-alpha-edit",
            "Hunyuan Image Alpha Edit",
            0,
            0,
            true,
            true,
            true,
        ),
        (
            "space-bunny",
            "Space-Bunny",
            1000000,
            128000,
            true,
            true,
            true,
        ),
        (
            "step-5-preview",
            "Step-5-Preview",
            1000000,
            64000,
            true,
            true,
            true,
        ),
        (
            "mimo-v2.6-pro",
            "Xiaomi MiMo-V2.6-Pro",
            1000000,
            128000,
            true,
            true,
            true,
        ),
        (
            "minimax-m3.1-flash",
            "MiniMax-M3.1-Flash",
            1000000,
            128000,
            true,
            true,
            true,
        ),
        (
            "kimi-k3.1-pro",
            "Kimi-K3.1-Pro",
            1000000,
            32000,
            true,
            true,
            true,
        ),
        ("kling-video-v3", "Kling-Video-V3", 0, 0, true, true, true),
        ("minimax-video-h3", "MiniMax-H3", 0, 0, true, true, true),
    ];
    MODELS
        .iter()
        .map(|(id, name, input, output, tools, reasoning, images)| {
            json!({
                "id": id,
                "name": name,
                "maxInputTokens": input,
                "maxOutputTokens": output,
                "supportsToolCall": tools,
                "supportsReasoning": reasoning,
                "supportsImages": images,
            })
        })
        .collect()
}

impl AppState {
    pub async fn test_account(&self, id: &str) -> Result<(u64, String), ApiError> {
        let acc = self.account(id).await.ok_or_else(|| {
            ApiError::new(StatusCode::NOT_FOUND, "not_found", "account not found")
        })?;
        let acc = self.ensure_token(&acc).await?;
        let endpoint = acc
            .effective_endpoint(&self.config.upstream.endpoint)
            .to_string();
        let body = json!({
            "model": "auto",
            "messages": [{"role": "user", "content": "Reply with the single word: pong"}],
            "max_tokens": 16,
            "stream": true
        });
        let started = std::time::Instant::now();
        let resp = self
            .http
            .chat(
                &endpoint,
                &acc.id,
                &acc.access_token,
                &acc.uid,
                "auto",
                &body,
            )
            .await
            .map_err(|e| ApiError::upstream(StatusCode::BAD_GATEWAY, format!("{e:#}"), None))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let (msg, code) = parse_upstream_error(status, &text);
            self.mark_failure(&acc, status, &msg).await;
            return Err(ApiError::upstream(status, msg, code));
        }
        let mut stream = resp.bytes_stream();
        let first = stream.next().await;
        let latency = started.elapsed().as_millis() as u64;
        match first {
            Some(Ok(b)) => Ok((
                latency,
                String::from_utf8_lossy(&b).chars().take(200).collect(),
            )),
            Some(Err(e)) => Err(ApiError::upstream(
                StatusCode::BAD_GATEWAY,
                format!("stream error: {e}"),
                None,
            )),
            None => Err(ApiError::upstream(
                StatusCode::BAD_GATEWAY,
                "empty stream",
                None,
            )),
        }
    }
}
