pub mod admin;
pub mod anthropic;
pub mod openai;
pub mod responses;

use crate::error::ApiError;
use crate::state::SharedState;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, patch, post};
use axum::{Json, Router};
use rust_embed::RustEmbed;
use serde_json::json;

#[derive(RustEmbed)]
#[folder = "web/dist/"]
struct Assets;

pub fn router(state: SharedState) -> Router {
    let cors = if state.config.server.cors_origins.iter().any(|o| o == "*") {
        tower_http::cors::CorsLayer::new()
            .allow_origin(tower_http::cors::Any)
            .allow_methods(tower_http::cors::Any)
            .allow_headers(tower_http::cors::Any)
    } else {
        let mut layer = tower_http::cors::CorsLayer::new()
            .allow_methods(tower_http::cors::Any)
            .allow_headers(tower_http::cors::Any);
        for origin in &state.config.server.cors_origins {
            if let Ok(v) = origin.parse::<HeaderValue>() {
                layer = layer.allow_origin(v);
            }
        }
        layer
    };

    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(openai::list_models))
        .route("/v1/models/{id}", get(openai::get_model))
        .route("/v1/chat/completions", post(openai::chat_completions))
        .route("/v1/responses", post(responses::create))
        .route(
            "/v1/responses/{id}",
            get(responses::get).delete(responses::delete),
        )
        .route("/v1/messages", post(anthropic::messages))
        .route("/v1/messages/count_tokens", post(anthropic::count_tokens))
        .route("/admin/api/login", post(login))
        .route("/admin/api/overview", get(admin::overview))
        .route(
            "/admin/api/accounts",
            get(admin::list_accounts).post(admin::add_account),
        )
        .route(
            "/admin/api/accounts/{id}",
            patch(admin::patch_account).delete(admin::delete_account),
        )
        .route(
            "/admin/api/accounts/{id}/refresh",
            post(admin::refresh_account),
        )
        .route("/admin/api/accounts/{id}/test", post(admin::test_account))
        .route(
            "/admin/api/accounts/import-local",
            post(admin::import_local),
        )
        .route("/admin/api/models", get(admin::list_models))
        .route(
            "/admin/api/keys",
            get(admin::list_keys).post(admin::create_key),
        )
        .route(
            "/admin/api/keys/{id}",
            axum::routing::delete(admin::delete_key),
        )
        .route(
            "/admin/api/settings",
            get(admin::get_settings).put(admin::put_settings),
        )
        .route("/admin/api/logs", get(admin::logs))
        .route("/admin/api/info", get(admin::info))
        .route("/v1/{*rest}", any(api_not_found))
        .route("/admin/api/{*rest}", any(api_not_found))
        .fallback(spa_or_api_not_found)
        // Screenshots and other multimodal payloads can exceed axum's 2 MiB default.
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .layer(cors)
        .with_state(state)
}

async fn health() -> Response {
    Json(json!({"status": "ok"})).into_response()
}

async fn api_not_found(uri: Uri) -> Response {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("route not found: {uri}"),
    )
    .into_response()
}

#[derive(serde::Deserialize)]
struct LoginRequest {
    token: String,
}

async fn login(State(state): State<SharedState>, Json(req): Json<LoginRequest>) -> Response {
    if token_eq(&req.token, &state.admin_token) {
        Json(json!({"ok": true})).into_response()
    } else {
        ApiError::unauthorized("invalid admin token").into_response()
    }
}

pub async fn auth(State(state): State<SharedState>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if is_public(path) {
        return next.run(req).await;
    }
    let bearer = bearer_token(req.headers());
    let api_key = req
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string)
        .or_else(|| bearer.clone());
    let admin_key = req
        .headers()
        .get("x-admin-token")
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string)
        .or_else(|| bearer.clone());

    let is_admin = [admin_key.as_deref(), api_key.as_deref()]
        .into_iter()
        .flatten()
        .any(|k| token_eq(k, &state.admin_token));

    if path.starts_with("/admin/api") {
        if is_admin {
            return next.run(req).await;
        }
        return ApiError::unauthorized("admin token required").into_response();
    }

    if path.starts_with("/v1") || path.starts_with("/api") {
        // The admin token doubles as a privileged API key (used by the
        // playground and by Claude Code, which sends `x-api-key`).
        if is_admin {
            return next.run(req).await;
        }
        let require = state.settings().await.require_api_key;
        if !require {
            return next.run(req).await;
        }
        if let Some(k) = api_key {
            if state.validate_api_key(&k).await {
                return next.run(req).await;
            }
        }
        return ApiError::unauthorized("invalid API key").into_response();
    }

    next.run(req).await
}

fn is_public(path: &str) -> bool {
    path == "/"
        || path == "/health"
        || path == "/favicon.ico"
        || path.starts_with("/assets/")
        || path.starts_with("/docs/")
        || path == "/admin/api/login"
        || path == "/manifest.webmanifest"
}

fn bearer_token(headers: &header::HeaderMap) -> Option<String> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = raw
        .strip_prefix("Bearer ")
        .unwrap_or_else(|| raw.strip_prefix("bearer ").unwrap_or(raw));
    Some(token.trim().to_string())
}

fn token_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

async fn spa(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    if let Some(file) = Assets::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime.as_ref())
            .header(
                header::CACHE_CONTROL,
                if path.starts_with("assets/") {
                    "public, max-age=31536000, immutable"
                } else {
                    "no-cache"
                },
            )
            .body(Body::from(file.data))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }
    match Assets::get("index.html") {
        Some(file) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from(file.data))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        None => (
            StatusCode::NOT_FOUND,
            "WorkBuddy Proxy UI is not built. Run `cd web && npm install && npm run build`.",
        )
            .into_response(),
    }
}

async fn spa_or_api_not_found(uri: Uri) -> Response {
    if uri.path().starts_with("/v1/") || uri.path().starts_with("/admin/") {
        return api_not_found(uri).await;
    }
    spa(uri).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn constant_time_compare_matches_exactly() {
        assert!(token_eq("wbadmin_abc", "wbadmin_abc"));
        assert!(!token_eq("wbadmin_abc", "wbadmin_abd"));
        assert!(!token_eq("short", "longer"));
        assert!(token_eq("", ""));
    }

    #[test]
    fn public_paths_do_not_require_auth() {
        assert!(is_public("/"));
        assert!(is_public("/health"));
        assert!(is_public("/assets/index.js"));
        assert!(is_public("/admin/api/login"));
        assert!(!is_public("/admin/api/overview"));
        assert!(!is_public("/v1/messages"));
    }

    #[test]
    fn frontend_assets_are_embedded() {
        let index = Assets::get("index.html").expect("index.html must be embedded");
        let html = String::from_utf8_lossy(&index.data);
        assert!(html.contains("<div id=\"root\"></div>"), "{html}");
        assert!(
            html.contains("/assets/") || html.contains("src=\"/src/main.tsx\""),
            "{html}"
        );
    }

    #[tokio::test]
    async fn root_page_does_not_depend_on_working_directory() {
        let response = spa(Uri::from_static("/")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/html")
        );
    }

    #[tokio::test]
    async fn unknown_api_paths_return_json_404() {
        let response = spa_or_api_not_found(Uri::from_static("/v1/unknown")).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
    }

    #[test]
    fn extracts_bearer_tokens() {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer secret"),
        );
        assert_eq!(bearer_token(&headers).as_deref(), Some("secret"));

        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("bearer lower"),
        );
        assert_eq!(bearer_token(&headers).as_deref(), Some("lower"));

        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("raw"));
        assert_eq!(bearer_token(&headers).as_deref(), Some("raw"));
    }
}
