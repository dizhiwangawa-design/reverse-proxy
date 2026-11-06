use crate::error::{ApiError, ApiResult};
use crate::state::{now_ms, SharedState};
use crate::store::{Account, Settings};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

fn account_dto(a: &Account, now: i64) -> Value {
    json!({
        "id": a.id,
        "name": a.name,
        "uid": a.uid,
        "status": a.public_status(now),
        "enabled": a.enabled,
        "invalid": a.invalid,
        "masked_token": a.masked_token(),
        "has_refresh_token": a.refresh_token.is_some(),
        "expires_at": a.expires_at,
        "refresh_expires_at": a.refresh_expires_at,
        "endpoint": a.endpoint,
        "weight": a.weight,
        "created_at": a.created_at,
        "last_used_at": a.last_used_at,
        "last_refresh_at": a.last_refresh_at,
        "last_error": a.last_error,
        "stats": a.stats,
    })
}

pub async fn overview(State(state): State<SharedState>) -> Response {
    Json(state.overview().await).into_response()
}

pub async fn list_accounts(State(state): State<SharedState>) -> Response {
    let now = now_ms();
    let accounts: Vec<Value> = state
        .accounts()
        .await
        .iter()
        .map(|a| account_dto(a, now))
        .collect();
    Json(json!({"accounts": accounts})).into_response()
}

#[derive(Deserialize)]
pub struct AddAccount {
    #[serde(default)]
    pub name: Option<String>,
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub uid: Option<String>,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub weight: Option<u32>,
}

pub async fn add_account(
    State(state): State<SharedState>,
    Json(req): Json<AddAccount>,
) -> Response {
    let token = req
        .access_token
        .trim()
        .trim_start_matches("Bearer ")
        .to_string();
    if token.is_empty() {
        return ApiError::bad_request("access_token is required").into_response();
    }
    let name = req
        .name
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "WorkBuddy".into());
    let acc = state
        .add_account(
            name,
            token,
            req.refresh_token,
            req.uid,
            req.endpoint,
            req.weight,
        )
        .await;
    Json(json!({"account": account_dto(&acc, now_ms())})).into_response()
}

#[derive(Deserialize)]
pub struct PatchAccount {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub weight: Option<u32>,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
}

pub async fn patch_account(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(req): Json<PatchAccount>,
) -> Response {
    let endpoint = req.endpoint.map(Some);
    match state
        .update_account(
            &id,
            req.name,
            req.enabled,
            req.weight,
            endpoint,
            req.access_token,
            req.refresh_token,
        )
        .await
    {
        Some(acc) => Json(json!({"account": account_dto(&acc, now_ms())})).into_response(),
        None => {
            ApiError::new(StatusCode::NOT_FOUND, "not_found", "account not found").into_response()
        }
    }
}

pub async fn delete_account(State(state): State<SharedState>, Path(id): Path<String>) -> Response {
    if state.delete_account(&id).await {
        Json(json!({"ok": true})).into_response()
    } else {
        ApiError::new(StatusCode::NOT_FOUND, "not_found", "account not found").into_response()
    }
}

pub async fn refresh_account(State(state): State<SharedState>, Path(id): Path<String>) -> Response {
    match state.refresh_account(&id, true).await {
        Ok(acc) => Json(json!({"account": account_dto(&acc, now_ms())})).into_response(),
        Err(e) => e.into_response(),
    }
}

pub async fn test_account(State(state): State<SharedState>, Path(id): Path<String>) -> Response {
    match state.test_account(&id).await {
        Ok((latency, preview)) => Json(json!({
            "ok": true,
            "latency_ms": latency,
            "preview": preview,
        }))
        .into_response(),
        Err(e) => e.into_response(),
    }
}

pub async fn import_local(State(state): State<SharedState>) -> Response {
    match state.import_local_auth().await {
        Ok(acc) => Json(json!({"account": account_dto(&acc, now_ms())})).into_response(),
        Err(e) => e.into_response(),
    }
}

#[derive(Deserialize)]
pub struct ModelsQuery {
    #[serde(default)]
    pub refresh: bool,
}

pub async fn list_models(
    State(state): State<SharedState>,
    Query(q): Query<ModelsQuery>,
) -> Response {
    if q.refresh {
        if let Err(e) = state.refresh_models().await {
            tracing::warn!("model refresh failed: {}", e.message);
        }
    }
    let (models, err) = state.models().await;
    let data: Vec<Value> = models
        .iter()
        .map(|m| {
            json!({
                "id": m.get("id").cloned().unwrap_or(Value::Null),
                "name": m.get("name").cloned().unwrap_or(Value::Null),
                "context_window": m.get("maxInputTokens").cloned().unwrap_or(Value::Null),
                "max_output_tokens": m.get("maxOutputTokens").cloned().unwrap_or(Value::Null),
                "supports_tools": m.get("supportsToolCall").cloned().unwrap_or(Value::Bool(true)),
                "supports_reasoning": m.get("supportsReasoning").cloned().unwrap_or(Value::Bool(true)),
                "supports_images": m.get("supportsImages").cloned().unwrap_or(Value::Bool(true)),
                "tags": m.get("tags").cloned().unwrap_or(Value::Array(vec![])),
                "description_zh": m.get("descriptionZh").cloned().unwrap_or(Value::Null),
                "description_en": m.get("descriptionEn").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    Json(json!({"models": data, "error": err})).into_response()
}

pub async fn list_keys(State(state): State<SharedState>) -> Response {
    let store = state.store.read().await;
    Json(json!({"keys": store.api_keys})).into_response()
}

#[derive(Deserialize)]
pub struct NewKey {
    #[serde(default)]
    pub name: Option<String>,
}

pub async fn create_key(State(state): State<SharedState>, Json(req): Json<NewKey>) -> Response {
    let name = req.name.unwrap_or_else(|| "api-key".into());
    let key = state.create_key(name).await;
    Json(json!({"key": key})).into_response()
}

pub async fn delete_key(State(state): State<SharedState>, Path(id): Path<String>) -> Response {
    if state.delete_key(&id).await {
        Json(json!({"ok": true})).into_response()
    } else {
        ApiError::new(StatusCode::NOT_FOUND, "not_found", "key not found").into_response()
    }
}

pub async fn get_settings(State(state): State<SharedState>) -> Response {
    Json(json!({"settings": state.settings().await})).into_response()
}

pub async fn put_settings(
    State(state): State<SharedState>,
    Json(mut s): Json<Settings>,
) -> ApiResult<Response> {
    if !matches!(
        s.rotation_strategy.as_str(),
        "round_robin" | "least_recent" | "weighted"
    ) {
        return Err(ApiError::bad_request(
            "rotation_strategy must be round_robin, least_recent or weighted",
        ));
    }
    s.max_retries = s.max_retries.clamp(1, 20);
    s.cooldown_secs = s.cooldown_secs.clamp(1, 3600);
    s.failure_cooldown_secs = s.failure_cooldown_secs.clamp(1, 3600);
    s.refresh_before_secs = s.refresh_before_secs.clamp(0, 86_400);
    state.update_settings(s.clone()).await;
    Ok(Json(json!({"settings": s})).into_response())
}

pub async fn logs(State(state): State<SharedState>) -> Response {
    let store = state.store.read().await;
    Json(json!({"recent": store.recent, "totals": store.totals})).into_response()
}

pub async fn info(State(state): State<SharedState>) -> Response {
    Json(json!({
        "name": "workbuddy-proxy",
        "version": env!("CARGO_PKG_VERSION"),
        "bind": state.config.server.bind,
        "upstream": state.config.upstream.endpoint,
        "started_at": state.started_at,
        "now": now_ms(),
        "admin_token_required": true,
        "endpoints": {
            "openai": ["/v1/models", "/v1/chat/completions"],
            "anthropic": ["/v1/messages", "/v1/messages/count_tokens"],
        }
    }))
    .into_response()
}
