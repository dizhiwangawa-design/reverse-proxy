use axum::body::Body;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
    pub kind: String,
    pub code: Option<String>,
    pub request_id: Option<String>,
}

impl ApiError {
    pub fn new(status: StatusCode, kind: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            kind: kind.into(),
            code: None,
            request_id: None,
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request_error", message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "authentication_error", message)
    }

    pub fn upstream(status: StatusCode, message: impl Into<String>, code: Option<String>) -> Self {
        Self {
            status,
            message: message.into(),
            kind: "upstream_error".into(),
            code,
            request_id: None,
        }
    }

    pub fn openai_body(&self) -> serde_json::Value {
        json!({
            "error": {
                "message": self.message,
                "type": self.kind,
                "code": self.code,
                "request_id": self.request_id,
            }
        })
    }

    pub fn anthropic_body(&self) -> serde_json::Value {
        json!({
            "type": "error",
            "error": {
                "type": match self.kind.as_str() {
                    "invalid_request_error" => "invalid_request_error",
                    "authentication_error" => "authentication_error",
                    "rate_limit_error" => "rate_limit_error",
                    _ => "api_error",
                },
                "message": self.message,
            }
        })
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, axum::Json(self.openai_body())).into_response()
    }
}

/// Convert an error into a unified Anthropic-style HTTP response.
pub fn anthropic_error(e: ApiError) -> Response {
    let mut response = Response::new(Body::from(e.anthropic_body().to_string()));
    *response.status_mut() = e.status;
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    response
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.status, self.message)
    }
}

impl std::error::Error for ApiError {}

pub type ApiResult<T> = Result<T, ApiError>;
