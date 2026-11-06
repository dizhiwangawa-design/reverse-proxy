use crate::config::UpstreamConfig;
use anyhow::{Context, Result};
use reqwest::{Response, StatusCode};
use serde_json::Value;
use std::time::Duration;

#[derive(Clone)]
pub struct UpstreamClient {
    http: reqwest::Client,
    pub cfg: UpstreamConfig,
}

#[derive(Debug, Clone)]
pub struct RefreshResult {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Option<i64>,
    pub refresh_expires_in: Option<i64>,
}

impl UpstreamClient {
    pub fn new(cfg: UpstreamConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .connect_timeout(Duration::from_secs(cfg.connect_timeout_secs))
            .pool_idle_timeout(Duration::from_secs(90))
            // reqwest enables transparent decompression only when the matching
            // compression feature is selected.
            .gzip(true)
            .brotli(true)
            .deflate(true)
            .build()
            .context("building http client")?;
        Ok(Self { http, cfg })
    }

    fn base(endpoint: &str) -> String {
        endpoint.trim_end_matches('/').to_string()
    }

    pub async fn chat(
        &self,
        endpoint: &str,
        account_id: &str,
        token: &str,
        uid: &str,
        model: &str,
        body: &Value,
    ) -> Result<Response> {
        let url = format!("{}/v2/chat/completions", Self::base(endpoint));
        let request_id = uuid::Uuid::new_v4().simple().to_string();
        let mut req = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .header("X-Requested-With", "XMLHttpRequest")
            .header("X-Model-ID", model)
            .header("X-Request-ID", &request_id)
            .header("X-Product", &self.cfg.product_type)
            .header("X-Agent-Intent", "craft")
            .header("X-Agent-Type", "main")
            .header("X-CodeBuddy-Request", "1")
            .header("X-Product-Version", &self.cfg.product_version)
            .header("X-IDE-Type", &self.cfg.ide_type)
            .header("X-IDE-Name", &self.cfg.ide_name)
            .header("X-IDE-Version", &self.cfg.product_version)
            .header("User-Agent", &self.cfg.user_agent)
            .json(body);
        if !uid.is_empty() {
            req = req.header("X-User-Id", uid);
        }
        req = req
            .header("X-Conversation-ID", request_id_with_prefix(account_id))
            .header("X-Conversation-Request-ID", &request_id)
            .header("X-Conversation-Message-ID", &request_id);
        req.send().await.context("sending upstream chat request")
    }

    pub async fn refresh(&self, endpoint: &str, refresh_token: &str) -> Result<RefreshResult> {
        let url = format!("{}/v2/plugin/auth/token/refresh", Self::base(endpoint));
        let resp = self
            .http
            .post(&url)
            .header("X-Refresh-Token", refresh_token)
            .header("X-Auth-Refresh-Source", "plugin")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("User-Agent", &self.cfg.user_agent)
            .json(&serde_json::json!({}))
            .send()
            .await
            .context("sending token refresh request")?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("refresh HTTP {}: {}", status.as_u16(), truncate(&text, 500));
        }
        let v: Value = serde_json::from_str(&text)
            .with_context(|| format!("refresh response is not json: {}", truncate(&text, 300)))?;
        let data = v.get("data").cloned().unwrap_or(Value::Null);
        let access = data
            .get("accessToken")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "refresh response missing accessToken: {}",
                    truncate(&text, 300)
                )
            })?;
        Ok(RefreshResult {
            access_token: access.to_string(),
            refresh_token: data
                .get("refreshToken")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            expires_in: data.get("expiresIn").and_then(Value::as_i64),
            refresh_expires_in: data.get("refreshExpiresIn").and_then(Value::as_i64),
        })
    }

    pub async fn fetch_models(&self, endpoint: &str, token: &str, uid: &str) -> Result<Value> {
        let url = format!("{}/v2/enterprises/personal/models", Self::base(endpoint));
        let mut req = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/json")
            .header("X-Product-Version", &self.cfg.product_version)
            .header("X-Product", &self.cfg.product_type)
            .header("X-IDE-Type", &self.cfg.ide_type)
            .header("X-IDE-Name", &self.cfg.ide_name)
            .header("X-IDE-Version", &self.cfg.product_version)
            .header("User-Agent", &self.cfg.user_agent);
        if !uid.is_empty() {
            req = req.header("X-User-Id", uid);
        }
        let resp = req.send().await.context("fetching models")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("models HTTP {}: {}", status.as_u16(), truncate(&text, 400));
        }
        serde_json::from_str(&text)
            .with_context(|| format!("models response is not json: {}", truncate(&text, 300)))
    }
}

fn request_id_with_prefix(account_id: &str) -> String {
    let suffix = account_id.replace('-', "");
    let suffix = suffix.get(..12).unwrap_or(&suffix);
    format!("wbproxy-{suffix}")
}

pub fn parse_upstream_error(status: StatusCode, body: &str) -> (String, Option<String>) {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        let code = v.get("code").and_then(value_to_code).or_else(|| {
            v.get("error")
                .and_then(|e| e.get("code"))
                .and_then(value_to_code)
        });
        let msg = v
            .get("msg")
            .and_then(Value::as_str)
            .or_else(|| v.get("message").and_then(Value::as_str))
            .or_else(|| {
                v.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
            })
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("upstream returned HTTP {}", status.as_u16()));
        return (msg, code);
    }
    (
        format!(
            "upstream returned HTTP {}: {}",
            status.as_u16(),
            truncate(body, 300)
        ),
        None,
    )
}

fn value_to_code(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(ToString::to_string)
        .or_else(|| value.as_i64().map(|v| v.to_string()))
        .or_else(|| value.as_u64().map(|v| v.to_string()))
}

pub fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::UNAUTHORIZED
        || status == StatusCode::FORBIDDEN
        || status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::BAD_GATEWAY
        || status == StatusCode::SERVICE_UNAVAILABLE
        || status == StatusCode::GATEWAY_TIMEOUT
        || status == StatusCode::PAYMENT_REQUIRED
        || status.is_server_error()
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_json_error_message_and_code() {
        let body = r#"{"code":"rate_limited","msg":"too many requests"}"#;
        let (message, code) = parse_upstream_error(StatusCode::TOO_MANY_REQUESTS, body);
        assert_eq!(message, "too many requests");
        assert_eq!(code.as_deref(), Some("rate_limited"));
    }

    #[test]
    fn parses_openai_style_error() {
        let body = r#"{"error":{"message":"invalid model","code":"model_not_found"}}"#;
        let (message, code) = parse_upstream_error(StatusCode::BAD_REQUEST, body);
        assert_eq!(message, "invalid model");
        assert_eq!(code.as_deref(), Some("model_not_found"));
    }

    #[test]
    fn falls_back_to_http_status() {
        let (message, code) = parse_upstream_error(StatusCode::BAD_GATEWAY, "<html>oops</html>");
        assert!(message.contains("502"));
        assert!(message.contains("oops"));
        assert!(code.is_none());
    }

    #[test]
    fn classifies_retryable_statuses() {
        assert!(is_retryable(StatusCode::UNAUTHORIZED));
        assert!(is_retryable(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(!is_retryable(StatusCode::BAD_REQUEST));
        assert!(!is_retryable(StatusCode::NOT_FOUND));
    }

    #[test]
    fn truncates_on_char_boundaries() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("你好世界", 2), "你好...");
    }

    #[test]
    fn makes_proxy_conversation_id_safe() {
        assert_eq!(
            request_id_with_prefix("bca2adce-ce3c-4e3a-a3bc-06d4b8d8d01f"),
            "wbproxy-bca2adcece3c"
        );
        assert_eq!(request_id_with_prefix("short"), "wbproxy-short");
    }
}
