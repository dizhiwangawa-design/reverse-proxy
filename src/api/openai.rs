use crate::error::{ApiError, ApiResult};
use crate::inband::{to_openai_tool_call, TextToolCall, TextToolCallScanner};
use crate::sanitize::sanitize_chat_body;
use crate::sse::SseBuffer;
use crate::state::{now_ms, SharedState};
use crate::store::{Account, RequestRecord};
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::time::Instant;

pub async fn list_models(State(state): State<SharedState>) -> Response {
    let (models, _err) = state.models().await;
    let created = state.started_at / 1000;
    let data: Vec<Value> = models
        .iter()
        .filter_map(|m| {
            let id = m.get("id").and_then(Value::as_str)?;
            Some(json!({
                "id": id,
                "object": "model",
                "created": created,
                "owned_by": "workbuddy",
                "name": m.get("name").cloned().unwrap_or(Value::String(id.to_string())),
                "context_window": m.get("maxInputTokens").cloned().unwrap_or(Value::Null),
                "max_output_tokens": m.get("maxOutputTokens").cloned().unwrap_or(Value::Null),
                "capabilities": {
                    "tools": m.get("supportsToolCall").cloned().unwrap_or(Value::Bool(true)),
                    "reasoning": m.get("supportsReasoning").cloned().unwrap_or(Value::Bool(true)),
                    "images": m.get("supportsImages").cloned().unwrap_or(Value::Bool(true)),
                }
            }))
        })
        .collect();
    Json(json!({"object": "list", "data": data})).into_response()
}

pub async fn get_model(
    State(state): State<SharedState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let (models, _) = state.models().await;
    if let Some(m) = models
        .iter()
        .find(|m| m.get("id").and_then(Value::as_str) == Some(&id))
    {
        let created = state.started_at / 1000;
        return Json(json!({
            "id": id,
            "object": "model",
            "created": created,
            "owned_by": "workbuddy",
            "name": m.get("name").cloned().unwrap_or(Value::String(id.clone())),
            "context_window": m.get("maxInputTokens").cloned().unwrap_or(Value::Null),
            "max_output_tokens": m.get("maxOutputTokens").cloned().unwrap_or(Value::Null),
        }))
        .into_response();
    }
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": {"message": format!("model {id} not found"), "type": "invalid_request_error"}})),
    )
        .into_response()
}

pub async fn chat_completions(
    State(state): State<SharedState>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Response {
    let Json(body) = match payload {
        Ok(payload) => payload,
        Err(e) => {
            let status = e.status();
            return ApiError::new(
                status,
                "invalid_request_error",
                format!("invalid JSON body: {}", e.body_text()),
            )
            .into_response();
        }
    };
    let started = Instant::now();
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string);
    let model = state.resolve_model(requested.as_deref()).await;
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    match chat_impl(state.clone(), body, model.clone()).await {
        Ok(r) => r,
        Err(e) => {
            state
                .record_request(RequestRecord {
                    at: now_ms(),
                    model,
                    account_id: String::new(),
                    account_name: String::new(),
                    stream,
                    ok: false,
                    status: e.status.as_u16(),
                    latency_ms: started.elapsed().as_millis() as u64,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    error: Some(e.message.clone()),
                })
                .await;
            e.into_response()
        }
    }
}

async fn chat_impl(state: SharedState, body: Value, model: String) -> ApiResult<Response> {
    let obj = body
        .as_object()
        .ok_or_else(|| ApiError::bad_request("request body must be a JSON object"))?;
    let messages = obj
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::bad_request("missing messages array"))?;
    if messages.is_empty() {
        return Err(ApiError::bad_request("messages must not be empty"));
    }
    if obj.get("n").and_then(Value::as_u64).unwrap_or(1) > 1 {
        return Err(ApiError::bad_request("only n=1 is supported"));
    }
    let client_stream = obj.get("stream").and_then(Value::as_bool).unwrap_or(false);

    let mut upstream_body = body.clone();
    {
        let uobj = upstream_body
            .as_object_mut()
            .ok_or_else(|| ApiError::bad_request("invalid body"))?;
        uobj.insert("model".into(), Value::String(model.clone()));
        uobj.insert("stream".into(), Value::Bool(true));
        // The upstream gateway only implements the streaming chat endpoint.
        uobj.remove("stream_options");
    }

    // Neutralize payload shapes the gateway refuses with a generic 11-128
    // "unapproved channel" rejection. Applied after the protocol fixes above so
    // the report only covers compatibility rewrites, and only ever mutates
    // requests that would otherwise be refused.
    let sanitized = sanitize_chat_body(&mut upstream_body);
    if !sanitized.is_noop() {
        tracing::debug!(
            developer_roles = sanitized.developer_roles,
            scrubbed_strings = sanitized.scrubbed_strings,
            model = %model,
            "rewrote request for upstream channel compatibility"
        );
    }

    let started = Instant::now();

    let (account, resp) = state.chat_with_rotation(&upstream_body, &model).await?;

    if client_stream {
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if !content_type.contains("text/event-stream") {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let (msg, code) = crate::upstream::parse_upstream_error(status, &text);
            return Err(ApiError::upstream(
                if status.is_success() {
                    StatusCode::BAD_GATEWAY
                } else {
                    status
                },
                msg,
                code,
            ));
        }
        let body = openai_stream_body(state.clone(), account, resp, model, started);
        let mut res = Response::new(body);
        res.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream; charset=utf-8"),
        );
        res.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        res.headers_mut()
            .insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
        res.headers_mut()
            .insert("x-accel-buffering", HeaderValue::from_static("no"));
        return Ok(res);
    }

    let status = resp.status();
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let (msg, code) = crate::upstream::parse_upstream_error(status, &text);
        return Err(ApiError::upstream(status, msg, code));
    }
    let mut agg = ChatAggregate::default();
    if content_type.contains("text/event-stream") {
        let mut upstream_error = None;
        for line in text.lines() {
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                break;
            }
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                if let Some(msg) = stream_error_message(&v) {
                    upstream_error = Some(msg);
                    break;
                }
                agg.apply(&v);
            }
        }
        if let Some(message) = upstream_error {
            return Err(ApiError::upstream(StatusCode::BAD_GATEWAY, message, None));
        }
    } else if let Ok(v) = serde_json::from_str::<Value>(&text) {
        // Defensive: tolerate gateways that ignore `stream: true`.
        if v.get("choices").is_some() {
            agg.apply(&v);
        } else {
            let (msg, code) = crate::upstream::parse_upstream_error(StatusCode::BAD_GATEWAY, &text);
            return Err(ApiError::upstream(StatusCode::BAD_GATEWAY, msg, code));
        }
    } else {
        let (msg, code) = crate::upstream::parse_upstream_error(StatusCode::BAD_GATEWAY, &text);
        return Err(ApiError::upstream(StatusCode::BAD_GATEWAY, msg, code));
    }
    // Recover tool calls the upstream left as in-band text; otherwise the
    // client sees prose and never acts.
    let recovered = agg.recover_inband_tool_calls();
    if recovered {
        tracing::debug!(model = %model, "recovered in-band tool calls from upstream text");
    }
    let usage = agg.usage.clone();
    let out = agg.into_response(&model);
    let latency = started.elapsed().as_millis() as u64;
    state
        .record_request(RequestRecord {
            at: now_ms(),
            model: model.clone(),
            account_id: account.id.clone(),
            account_name: account.name.clone(),
            stream: false,
            ok: true,
            status: 200,
            latency_ms: latency,
            prompt_tokens: usage
                .as_ref()
                .and_then(|u| u.get("prompt_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
            completion_tokens: usage
                .as_ref()
                .and_then(|u| u.get("completion_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
            error: None,
        })
        .await;
    Ok(Json(out).into_response())
}

/// Build one OpenAI stream chunk from the original frame's envelope.
///
/// Only the first frame's `id`/`model`/`created` are reused; the delta is
/// replaced wholesale so no upstream-specific fields leak through.
fn frame(shell: &Value, delta: &Value) -> Value {
    json!({
        "id": shell.get("id").cloned().unwrap_or(Value::Null),
        "object": "chat.completion.chunk",
        "created": shell.get("created").cloned().unwrap_or_else(|| json!(now_ms() / 1000)),
        "model": shell.get("model").cloned().unwrap_or(Value::Null),
        "choices": [{ "index": 0, "delta": delta, "finish_reason": Value::Null, "logprobs": Value::Null }],
        "usage": Value::Null,
    })
}

/// Build the terminating chunk that carries `finish_reason`.
fn finish_frame(shell: &Value, reason: &str) -> Value {
    json!({
        "id": shell.get("id").cloned().unwrap_or(Value::Null),
        "object": "chat.completion.chunk",
        "created": shell.get("created").cloned().unwrap_or_else(|| json!(now_ms() / 1000)),
        "model": shell.get("model").cloned().unwrap_or(Value::Null),
        "choices": [{ "index": 0, "delta": json!({}), "finish_reason": reason, "logprobs": Value::Null }],
        "usage": Value::Null,
    })
}

/// Build a terminal chunk carrying the usage report.
fn usage_frame(shell: &Value, usage: &Value) -> Value {
    json!({
        "id": shell.get("id").cloned().unwrap_or(Value::Null),
        "object": "chat.completion.chunk",
        "created": shell.get("created").cloned().unwrap_or_else(|| json!(now_ms() / 1000)),
        "model": shell.get("model").cloned().unwrap_or(Value::Null),
        "choices": [],
        "usage": usage,
    })
}

fn openai_stream_body(
    state: SharedState,
    account: Account,
    resp: reqwest::Response,
    model: String,
    started: Instant,
) -> Body {
    let stream = async_stream::stream! {
        let mut upstream = resp.bytes_stream();
        let mut parser = SseBuffer::new();
        let mut usage: Option<Value> = None;
        let mut done = false;
        let mut failed: Option<String> = None;
        // The gateway emits no structured tool calls: it hides the invocation in
        // `content` as DSML text. Frames are therefore rewritten rather than
        // forwarded verbatim, so markup never reaches the client and the calls
        // are re-emitted in the shape an OpenAI client accumulates.
        let mut scanner = TextToolCallScanner::new();
        let mut shell: Option<Value> = None;
        let mut recovered: Vec<TextToolCall> = Vec::new();
        let mut upstream_finish: Option<String> = None;
        let mut emitted_any_call = false;
        while let Some(chunk) = upstream.next().await {
            match chunk {
                Ok(bytes) => {
                    for data in parser.push(&bytes) {
                        if data == "[DONE]" {
                            done = true;
                            continue;
                        }
                        let Ok(v) = serde_json::from_str::<Value>(&data) else {
                            continue;
                        };
                        if let Some(msg) = stream_error_message(&v) {
                            let err = json!({"error": {"message": msg, "type": "upstream_error"}});
                            failed = Some(msg);
                            yield Ok::<Bytes, std::io::Error>(Bytes::from(format!("data: {err}\n\n")));
                            continue;
                        }
                        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                            usage = Some(u.clone());
                        }
                        if shell.is_none() {
                            shell = Some(v.clone());
                        }
                        let choice = v.get("choices").and_then(|c| c.get(0));
                        let delta = choice.and_then(|c| c.get("delta"));
                        if let Some(fr) = choice
                            .and_then(|c| c.get("finish_reason"))
                            .and_then(Value::as_str)
                            .filter(|f| !f.is_empty())
                        {
                            upstream_finish = Some(fr.to_string());
                        }
                        let Some(delta) = delta else { continue };
                        // Pass a structured call through untouched; only the
                        // text form needs recovery.
                        if delta
                            .get("tool_calls")
                            .and_then(Value::as_array)
                            .is_some_and(|a| !a.is_empty())
                        {
                            yield Ok(Bytes::from(format!("data: {}\n\n", frame(&v, delta))));
                            continue;
                        }
                        // Reasoning is ordinary streamed text and is forwarded;
                        // only `content` can carry markup.
                        if let Some(r) = delta
                            .get("reasoning_content")
                            .and_then(Value::as_str)
                            .filter(|r| !r.is_empty())
                        {
                            let f = json!({"reasoning_content": r});
                            yield Ok(Bytes::from(format!("data: {}\n\n", frame(&v, &f))));
                        }
                        let Some(content) = delta.get("content").and_then(Value::as_str) else {
                            continue;
                        };
                        if content.is_empty() {
                            continue;
                        }
                        let (visible, calls) = scanner.push(content);
                        recovered.extend(calls);
                        if !visible.is_empty() {
                            let f = json!({"content": visible});
                            yield Ok(Bytes::from(format!("data: {}\n\n", frame(&v, &f))));
                        }
                    }
                }
                Err(e) => {
                    failed = Some(e.to_string());
                    let err = json!({"error": {"message": format!("upstream stream error: {e}"), "type": "upstream_error"}});
                    yield Ok(Bytes::from(format!("data: {err}\n\n")));
                    yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
                    done = true;
                    break;
                }
            }
        }
        // Flush whatever the scanner held back, then emit calls followed by the
        // real finish reason: `stop` would tell the client the turn is over and
        // the recovered calls would be ignored.
        let (tail_text, tail_calls) = scanner.finish();
        recovered.extend(tail_calls);
        if let Some(shell) = shell.as_ref() {
            if !tail_text.is_empty() {
                let f = json!({"content": tail_text});
                yield Ok(Bytes::from(format!("data: {}\n\n", frame(shell, &f))));
            }
            for (i, call) in recovered.iter().enumerate() {
                let tc = to_openai_tool_call(i, call);
                let f = json!({"tool_calls": [tc]});
                yield Ok(Bytes::from(format!("data: {}\n\n", frame(shell, &f))));
                emitted_any_call = true;
            }
            let finish = if emitted_any_call {
                "tool_calls"
            } else {
                upstream_finish.as_deref().unwrap_or("stop")
            };
            yield Ok(Bytes::from(format!(
                "data: {}\n\n",
                finish_frame(shell, finish)
            )));
            if let Some(u) = usage.clone() {
                yield Ok(Bytes::from(format!(
                    "data: {}\n\n",
                    usage_frame(shell, &u)
                )));
            }
        }
        // Some gateways close the stream without a terminal [DONE] event.
        if !done {
            yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
        }
        let latency = started.elapsed().as_millis() as u64;
        state.record_request(RequestRecord {
            at: now_ms(),
            model: model.clone(),
            account_id: account.id.clone(),
            account_name: account.name.clone(),
            stream: true,
            ok: failed.is_none(),
            status: 200,
            latency_ms: latency,
            prompt_tokens: usage.as_ref().and_then(|u| u.get("prompt_tokens")).and_then(Value::as_u64).unwrap_or(0),
            completion_tokens: usage.as_ref().and_then(|u| u.get("completion_tokens")).and_then(Value::as_u64).unwrap_or(0),
            error: failed,
        }).await;
    };
    Body::from_stream(stream)
}

/// Extract an error message from an upstream SSE payload, if it contains one.
pub fn stream_error_message(v: &Value) -> Option<String> {
    let err = v.get("error")?;
    if err.is_null() {
        return None;
    }
    if let Some(text) = err.as_str() {
        return Some(text.to_string());
    }
    if let Some(message) = err.get("message").and_then(Value::as_str) {
        return Some(message.to_string());
    }
    Some(err.to_string())
}

#[derive(Default)]
pub struct ChatAggregate {
    pub id: Option<String>,
    pub model: Option<String>,
    pub created: Option<i64>,
    pub content: String,
    pub reasoning: String,
    pub refusal: Option<String>,
    pub tool_calls: BTreeMap<usize, ToolCallAcc>,
    pub finish_reason: Option<String>,
    pub usage: Option<Value>,
}

#[derive(Default, Clone)]
pub struct ToolCallAcc {
    pub id: Option<String>,
    pub name: String,
    pub arguments: String,
}

impl ChatAggregate {
    pub fn apply(&mut self, v: &Value) {
        if self.id.is_none() {
            self.id = v.get("id").and_then(Value::as_str).map(ToString::to_string);
        }
        if self.model.is_none() {
            self.model = v
                .get("model")
                .and_then(Value::as_str)
                .map(ToString::to_string);
        }
        if self.created.is_none() {
            self.created = v.get("created").and_then(Value::as_i64);
        }
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            self.usage = Some(u.clone());
        }
        let Some(choice) = v.get("choices").and_then(|c| c.get(0)) else {
            return;
        };
        if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
            if !fr.is_empty() {
                self.finish_reason = Some(fr.to_string());
            }
        }
        if let Some(msg) = choice.get("message") {
            self.apply_delta(msg);
        }
        if let Some(delta) = choice.get("delta") {
            self.apply_delta(delta);
        }
    }

    fn apply_delta(&mut self, d: &Value) {
        if let Some(c) = d.get("content") {
            if let Some(s) = c.as_str() {
                self.content.push_str(s);
            } else if let Some(arr) = c.as_array() {
                for part in arr {
                    if let Some(s) = part.get("text").and_then(Value::as_str) {
                        self.content.push_str(s);
                    }
                }
            }
        }
        if let Some(s) = d.get("reasoning_content").and_then(Value::as_str) {
            self.reasoning.push_str(s);
        } else if let Some(s) = d.get("reasoning").and_then(Value::as_str) {
            self.reasoning.push_str(s);
        }
        if let Some(s) = d.get("refusal").and_then(Value::as_str) {
            if !s.is_empty() {
                self.refusal = Some(s.to_string());
            }
        }
        if let Some(tcs) = d.get("tool_calls").and_then(Value::as_array) {
            for (fallback_idx, tc) in tcs.iter().enumerate() {
                let idx = tc
                    .get("index")
                    .and_then(Value::as_u64)
                    .map(|v| v as usize)
                    .unwrap_or(fallback_idx);
                let slot = self.tool_calls.entry(idx).or_default();
                if let Some(id) = tc.get("id").and_then(Value::as_str) {
                    if !id.is_empty() {
                        slot.id = Some(id.to_string());
                    }
                }
                if let Some(f) = tc.get("function") {
                    if let Some(n) = f.get("name").and_then(Value::as_str) {
                        if !n.is_empty() {
                            slot.name.push_str(n);
                        }
                    }
                    if let Some(a) = f.get("arguments").and_then(Value::as_str) {
                        slot.arguments.push_str(a);
                    }
                }
            }
        }
        if let Some(f) = d.get("function_call") {
            let slot = self.tool_calls.entry(0).or_default();
            if let Some(n) = f.get("name").and_then(Value::as_str) {
                slot.name.push_str(n);
            }
            if let Some(a) = f.get("arguments").and_then(Value::as_str) {
                slot.arguments.push_str(a);
            }
        }
    }

    pub fn has_tool_calls(&self) -> bool {
        self.tool_calls.values().any(|t| !t.name.is_empty())
    }

    /// Recover tool calls the upstream left as in-band text.
    ///
    /// The gateway answers `200` but emits no structured `tool_calls`; the
    /// invocation sits inside `content` as DSML markup. Without this the client
    /// receives prose and never acts. Text that is not markup is preserved.
    ///
    /// Returns whether anything was recovered, so the caller can tell a real
    /// tool turn from a plain answer.
    pub fn recover_inband_tool_calls(&mut self) -> bool {
        if self.has_tool_calls() || self.content.is_empty() {
            return false;
        }
        // The whole answer is already in hand, so one push plus a finish drains
        // it without holding anything back.
        let mut scanner = TextToolCallScanner::new();
        let (mut visible, mut calls) = scanner.push(&self.content);
        let (tail_text, mut tail_calls) = scanner.finish();
        visible.push_str(&tail_text);
        calls.append(&mut tail_calls);
        if calls.is_empty() {
            return false;
        }
        let base = self.tool_calls.len();
        for (i, call) in calls.iter().enumerate() {
            let slot = self.tool_calls.entry(base + i).or_default();
            slot.id = Some(format!("call_dsml_{i}"));
            slot.name = call.name.clone();
            slot.arguments = call.arguments.to_string();
        }
        self.content = visible;
        if self.finish_reason.as_deref().is_none_or(|f| f == "stop") {
            self.finish_reason = Some("tool_calls".into());
        }
        true
    }

    pub fn into_response(self, requested_model: &str) -> Value {
        let has_tools = self.has_tool_calls();
        let mut message = Map::new();
        message.insert("role".into(), Value::String("assistant".into()));
        if self.content.is_empty() && has_tools {
            message.insert("content".into(), Value::Null);
        } else {
            message.insert("content".into(), Value::String(self.content));
        }
        if !self.reasoning.is_empty() {
            message.insert("reasoning_content".into(), Value::String(self.reasoning));
        }
        if let Some(r) = self.refusal {
            message.insert("refusal".into(), Value::String(r));
        }
        if has_tools {
            let calls: Vec<Value> = self
                .tool_calls
                .into_values()
                .filter(|t| !t.name.is_empty())
                .map(|t| {
                    json!({
                        "id": t.id.unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4().simple())),
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "arguments": if t.arguments.is_empty() { "{}".to_string() } else { t.arguments },
                        }
                    })
                })
                .collect();
            if !calls.is_empty() {
                message.insert("tool_calls".into(), Value::Array(calls));
            }
        }
        let finish = self.finish_reason.unwrap_or_else(|| {
            if has_tools {
                "tool_calls".into()
            } else {
                "stop".into()
            }
        });
        json!({
            "id": self.id.unwrap_or_else(|| format!("chatcmpl-{}", uuid::Uuid::new_v4().simple())),
            "object": "chat.completion",
            "created": self.created.unwrap_or_else(|| now_ms() / 1000),
            "model": requested_model,
            "choices": [{
                "index": 0,
                "message": Value::Object(message),
                "logprobs": Value::Null,
                "finish_reason": finish,
            }],
            "usage": self.usage.unwrap_or_else(|| json!({
                "prompt_tokens": 0,
                "completion_tokens": 0,
                "total_tokens": 0,
            })),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_streaming_text_and_reasoning() {
        let mut agg = ChatAggregate::default();
        agg.apply(&json!({
            "id": "cmb-1",
            "created": 123,
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": "Hello"}}]
        }));
        agg.apply(&json!({
            "choices": [{"index": 0, "delta": {"content": " world", "reasoning_content": "think"}}]
        }));
        agg.apply(&json!({
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
        }));
        let out = agg.into_response("auto");
        assert_eq!(out["id"], "cmb-1");
        assert_eq!(out["model"], "auto");
        assert_eq!(out["choices"][0]["message"]["content"], "Hello world");
        assert_eq!(out["choices"][0]["message"]["reasoning_content"], "think");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(out["usage"]["prompt_tokens"], 5);
    }

    #[test]
    fn aggregates_incremental_tool_calls() {
        let mut agg = ChatAggregate::default();
        agg.apply(&json!({
            "choices": [{"index": 0, "delta": {"tool_calls": [{
                "index": 0,
                "id": "call-1",
                "function": {"name": "get_weather", "arguments": "{\"city\":"}
            }]}}]
        }));
        agg.apply(&json!({
            "choices": [{"index": 0, "delta": {"tool_calls": [{
                "index": 0,
                "function": {"arguments": "\"Beijing\"}"}
            }]}, "finish_reason": "tool_calls"}]
        }));
        // Apply a correctly shaped final chunk as well.
        agg.apply(&json!({
            "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
        }));
        let out = agg.into_response("auto");
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
        let call = &out["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(call["id"], "call-1");
        assert_eq!(call["function"]["name"], "get_weather");
        assert_eq!(call["function"]["arguments"], "{\"city\":\"Beijing\"}");
    }

    #[test]
    fn uses_null_content_when_only_tool_calls() {
        let mut agg = ChatAggregate::default();
        agg.apply(&json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0,
                "id": "call-1",
                "function": {"name": "ping", "arguments": "{}"}
            }]}}]
        }));
        let out = agg.into_response("auto");
        assert!(out["choices"][0]["message"]["content"].is_null());
    }

    #[test]
    fn detects_stream_errors() {
        assert_eq!(
            stream_error_message(&json!({"error": {"message": "boom"}})),
            Some("boom".to_string())
        );
        assert_eq!(
            stream_error_message(&json!({"error": "plain"})),
            Some("plain".to_string())
        );
        assert_eq!(stream_error_message(&json!({"error": null})), None);
        assert_eq!(stream_error_message(&json!({"choices": []})), None);
    }
}
