//! OpenAI Responses API (`POST /v1/responses`) compatibility layer.
//!
//! Codex CLI, the OpenAI Agents SDK and other newer clients speak the Responses
//! protocol (`input` items in, `output` items out, plus the `response.*` SSE
//! event family). The upstream gateway only implements streaming chat
//! completions, so this module translates in both directions:
//!
//! * request: `instructions` + `input[]` → `messages[]`, flat `tools[]` →
//!   chat `tools[]`, `max_output_tokens` → `max_tokens`, `text.format` →
//!   `response_format`;
//! * response: aggregated chat completion → `output[]` items, streamed as
//!   `response.output_item.*` / `response.output_text.delta` events.
//!
//! Tool invocations the upstream leaves as DSML text are recovered with
//! [`TextToolCallScanner`], exactly as the chat endpoint does, so clients that
//! only read `function_call` items still see their tool calls.

use crate::api::openai::{stream_error_message, ChatAggregate};
use crate::error::{ApiError, ApiResult};
use crate::inband::{TextToolCall, TextToolCallScanner};
use crate::sanitize::sanitize_chat_body;
use crate::sse::{sse_event, SseBuffer};
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
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Responses kept for `previous_response_id` replay. Clients that send the full
/// `input` every turn (Codex does) never touch this.
const HISTORY_LIMIT: usize = 64;

type History = VecDeque<(String, Vec<Value>)>;

fn history_store() -> &'static Mutex<History> {
    static STORE: OnceLock<Mutex<History>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn load_history(id: &str) -> Option<Vec<Value>> {
    let guard = history_store().lock().ok()?;
    guard
        .iter()
        .find(|(key, _)| key == id)
        .map(|(_, messages)| messages.clone())
}

fn save_history(id: String, messages: Vec<Value>) {
    if let Ok(mut guard) = history_store().lock() {
        guard.push_back((id, messages));
        while guard.len() > HISTORY_LIMIT {
            guard.pop_front();
        }
    }
}

pub async fn create(
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
    match response_impl(state.clone(), body, model.clone()).await {
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

/// Responses are ephemeral here: without an upstream store there is nothing to
/// fetch back, and clients that need one send the history inline.
pub async fn get(axum::extract::Path(id): axum::extract::Path<String>) -> Response {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "invalid_request_error",
        format!("response {id} not found; this proxy does not persist responses"),
    )
    .into_response()
}

pub async fn delete(axum::extract::Path(id): axum::extract::Path<String>) -> Response {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "invalid_request_error",
        format!("response {id} not found; this proxy does not persist responses"),
    )
    .into_response()
}

async fn response_impl(state: SharedState, body: Value, model: String) -> ApiResult<Response> {
    let obj = body
        .as_object()
        .ok_or_else(|| ApiError::bad_request("request body must be a JSON object"))?;
    let client_stream = obj.get("stream").and_then(Value::as_bool).unwrap_or(false);

    let previous = obj
        .get("previous_response_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut messages = build_messages(obj, previous.as_deref())?;
    let history_prefix: Vec<Value> = messages
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) != Some("system"))
        .cloned()
        .collect();

    let mut upstream_body = Map::new();
    upstream_body.insert("model".into(), Value::String(model.clone()));
    upstream_body.insert("messages".into(), Value::Array(std::mem::take(&mut messages)));
    // The upstream gateway only implements the streaming chat endpoint.
    upstream_body.insert("stream".into(), Value::Bool(true));
    copy_param(obj, &mut upstream_body, "temperature", "temperature");
    copy_param(obj, &mut upstream_body, "top_p", "top_p");
    copy_param(obj, &mut upstream_body, "user", "user");
    copy_param(
        obj,
        &mut upstream_body,
        "max_output_tokens",
        "max_tokens",
    );
    copy_param(
        obj,
        &mut upstream_body,
        "parallel_tool_calls",
        "parallel_tool_calls",
    );
    // `custom` tools (Codex's apply_patch is one) and `namespace` bundles do not
    // exist in the chat protocol, so custom ones are exposed as single-string
    // functions and their calls are turned back into custom_tool_call items.
    let (chat_tools, custom_tools) = obj.get("tools").map(convert_tools).unwrap_or_default();
    if !chat_tools.is_empty() {
        upstream_body.insert("tools".into(), Value::Array(chat_tools));
    }
    if let Some(choice) = obj.get("tool_choice").and_then(convert_tool_choice) {
        upstream_body.insert("tool_choice".into(), choice);
    }
    if let Some(format) = obj
        .get("text")
        .and_then(|t| t.get("format"))
        .and_then(convert_text_format)
    {
        upstream_body.insert("response_format".into(), format);
    }

    let mut upstream_body = Value::Object(upstream_body);
    let sanitized = sanitize_chat_body(&mut upstream_body);
    if !sanitized.is_noop() {
        tracing::debug!(
            developer_roles = sanitized.developer_roles,
            scrubbed_strings = sanitized.scrubbed_strings,
            model = %model,
            "rewrote responses request for upstream channel compatibility"
        );
    }

    let started = Instant::now();
    let (account, resp) = state.chat_with_rotation(&upstream_body, &model).await?;

    let response_id = format!("resp_{}", uuid::Uuid::new_v4().simple());
    let created = now_ms() / 1000;

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
        let body = stream_body(
            state.clone(),
            account,
            resp,
            model,
            response_id,
            created,
            started,
            history_prefix,
            custom_tools,
        );
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
    agg.recover_inband_tool_calls();

    let calls = calls_of(&agg, &custom_tools);
    let usage = map_usage(agg.usage.as_ref());
    let output = output_items(&agg.reasoning, &agg.content, &calls);
    let mut history = history_prefix;
    history.extend(assistant_messages(&agg.content, &calls));
    save_history(response_id.clone(), history);

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
            prompt_tokens: prompt_tokens(agg.usage.as_ref()),
            completion_tokens: completion_tokens(agg.usage.as_ref()),
            error: None,
        })
        .await;

    let body = base_response(&response_id, &model, created, "completed", output, usage);
    Ok(Json(body).into_response())
}

fn copy_param(obj: &Map<String, Value>, out: &mut Map<String, Value>, from: &str, to: &str) {
    if let Some(v) = obj.get(from).filter(|v| !v.is_null()) {
        out.insert(to.to_string(), v.clone());
    }
}

/// Flatten `instructions` + `input[]` into chat `messages[]`.
fn build_messages(obj: &Map<String, Value>, previous: Option<&str>) -> ApiResult<Vec<Value>> {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(text) = obj
        .get("instructions")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        messages.push(json!({"role": "system", "content": text}));
    }
    match obj.get("input") {
        Some(Value::String(text)) => messages.push(json!({"role": "user", "content": text})),
        Some(Value::Array(items)) => {
            for item in items {
                push_input_item(item, &mut messages);
            }
        }
        Some(Value::Null) | None => {}
        _ => return Err(ApiError::bad_request("input must be a string or an array")),
    }
    if messages.is_empty() {
        if let Some(prev) = previous {
            if let Some(history) = load_history(prev) {
                messages.extend(history);
            }
        }
    } else if let Some(prev) = previous {
        if let Some(history) = load_history(prev) {
            let split = usize::from(
                messages
                    .first()
                    .and_then(|m| m.get("role"))
                    .and_then(Value::as_str)
                    == Some("system"),
            );
            let mut merged: Vec<Value> = messages[..split].to_vec();
            merged.extend(history);
            merged.extend(messages[split..].iter().cloned());
            messages = merged;
        }
    }
    if messages.is_empty() {
        return Err(ApiError::bad_request(
            "input must contain at least one message",
        ));
    }
    // Coding agents drive a tool loop: a turn that carries only prose ends the
    // client's turn and forces the user to type "继续". Models that hand control
    // back after every step need to be told the loop is theirs to finish.
    if has_tools(obj) && nudge_enabled() {
        let last_user = messages
            .iter()
            .rposition(|m| m.get("role").and_then(Value::as_str) == Some("user"));
        let nudge = json!({"role": "system", "content": CONTINUATION_NUDGE});
        match last_user {
            Some(idx) => messages.insert(idx + 1, nudge),
            None => messages.push(nudge),
        }
    }
    Ok(messages)
}

/// Whether the client advertised any tool at all.
fn has_tools(obj: &Map<String, Value>) -> bool {
    obj.get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty())
}

/// `WB_RESPONSES_NUDGE=0` turns the continuation prompt off.
fn nudge_enabled() -> bool {
    !matches!(
        std::env::var("WB_RESPONSES_NUDGE").as_deref(),
        Ok("0") | Ok("false") | Ok("no") | Ok("off")
    )
}

const CONTINUATION_NUDGE: &str = "\
运行约束（自动化编码循环，务必遵守）：
1. 这是一次连续的代理回合。任务未完成时，你的回复必须包含至少一个工具调用；只输出文字而不调用工具会被运行环境当作任务结束，用户必须手动催促才能继续。
2. 不要在回复里预告或复述“我正在…”“接下来我会…”。要做的动作直接作为工具调用发出。
3. 只有任务全部完成、并且已经用工具验证过结果之后，才可以输出不带工具调用的最终总结。
4. 需要修改文件时使用文件编辑工具；一次改不完就连续调用多次，不要中途停下来征求意见。
5. 补丁应用失败（上下文不匹配、范围过大）时，把改动拆成更小的 hunk 重新提交，不要放弃，也不要用文字解释失败原因代替重试。";

fn push_input_item(item: &Value, messages: &mut Vec<Value>) {
    let Some(obj) = item.as_object() else {
        if let Some(text) = item.as_str() {
            messages.push(json!({"role": "user", "content": text}));
        }
        return;
    };
    let kind = obj
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message");
    match kind {
        "message" => {
            let role = match obj.get("role").and_then(Value::as_str).unwrap_or("user") {
                "developer" => "system",
                other => other,
            };
            let content = convert_content(obj.get("content"));
            messages.push(json!({"role": role, "content": content}));
        }
        "custom_tool_call" => {
            // Custom tools travel as a free-form `input` string; the chat side
            // sees them through the single-string function wrapper.
            let name = obj
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();
            let arguments = json!({
                "input": obj.get("input").cloned().unwrap_or_else(|| Value::String(String::new())),
            })
            .to_string();
            let call_id = call_id_of(obj).unwrap_or_else(|| "call_0".to_string());
            push_assistant_call(messages, name, arguments, call_id);
        }
        "function_call" | "local_shell_call" => {
            let name = obj
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();
            let arguments = match obj.get("arguments") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Null) | None => match obj.get("action") {
                    Some(action) => action.to_string(),
                    None => "{}".to_string(),
                },
                Some(other) => other.to_string(),
            };
            let call_id = call_id_of(obj).unwrap_or_else(|| "call_0".to_string());
            push_assistant_call(messages, name, arguments, call_id);
        }
        "function_call_output" | "custom_tool_call_output" | "local_shell_call_output" => {
            let call_id = call_id_of(obj).unwrap_or_default();
            let output = obj.get("output").map(output_text).unwrap_or_default();
            messages.push(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": output,
            }));
        }
        // Reasoning items are upstream-specific state; nothing to replay.
        "reasoning" => {}
        _ => {
            if let Some(text) = obj.get("text").and_then(Value::as_str) {
                messages.push(json!({"role": "user", "content": text}));
            }
        }
    }
}

/// Append one tool invocation to the assistant turn being rebuilt from history.
/// Consecutive calls merge into a single assistant message: chat requires one
/// message carrying every call of a turn.
fn push_assistant_call(
    messages: &mut Vec<Value>,
    name: String,
    arguments: String,
    call_id: String,
) {
    let call = json!({
        "id": call_id,
        "type": "function",
        "function": {"name": name, "arguments": arguments},
    });
    let merged = match messages.last_mut() {
        Some(last)
            if last.get("role").and_then(Value::as_str) == Some("assistant")
                && last.get("content").is_none_or(Value::is_null)
                && last.get("tool_calls").and_then(Value::as_array).is_some() =>
        {
            if let Some(list) = last.get_mut("tool_calls").and_then(Value::as_array_mut) {
                list.push(call.clone());
            }
            true
        }
        _ => false,
    };
    if !merged {
        messages.push(json!({
            "role": "assistant",
            "content": Value::Null,
            "tool_calls": [call],
        }));
    }
}

fn call_id_of(obj: &Map<String, Value>) -> Option<String> {
    obj.get("call_id")
        .and_then(Value::as_str)
        .or_else(|| obj.get("id").and_then(Value::as_str))
        .map(str::to_string)
}

fn output_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(parts) => {
            let mut out = String::new();
            for part in parts {
                let text = part
                    .get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.as_str())
                    .unwrap_or_default();
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
            out
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn image_url_of(part: &Value) -> Option<String> {
    match part.get("image_url") {
        Some(Value::String(url)) => Some(url.clone()),
        Some(Value::Object(obj)) => obj
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => part
            .get("file_id")
            .and_then(Value::as_str)
            .map(|id| format!("file:{id}")),
    }
}

fn convert_content(value: Option<&Value>) -> Value {
    match value {
        None | Some(Value::Null) => Value::String(String::new()),
        Some(Value::String(s)) => Value::String(s.clone()),
        Some(Value::Array(parts)) => {
            let mut has_image = false;
            let mut pieces: Vec<Value> = Vec::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str).unwrap_or("") {
                    "input_image" | "image_url" => {
                        if let Some(url) = image_url_of(part) {
                            has_image = true;
                            pieces.push(json!({"type": "image_url", "image_url": {"url": url}}));
                        }
                    }
                    "refusal" => {
                        if let Some(text) = part.get("refusal").and_then(Value::as_str) {
                            pieces.push(json!({"type": "text", "text": text}));
                        }
                    }
                    _ => {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            pieces.push(json!({"type": "text", "text": text}));
                        }
                    }
                }
            }
            if has_image {
                Value::Array(pieces)
            } else {
                let mut flat = String::new();
                for piece in &pieces {
                    if let Some(text) = piece.get("text").and_then(Value::as_str) {
                        if !flat.is_empty() {
                            flat.push('\n');
                        }
                        flat.push_str(text);
                    }
                }
                Value::String(flat)
            }
        }
        Some(other) => other.clone(),
    }
}

/// Responses tools are flat (`{type, name, parameters}`); chat wants them nested
/// under `function`. Unknown tool kinds are dropped rather than rejected so a
/// client advertising built-ins still works for the tools we can serve.
///
/// `custom` tools (Codex's `apply_patch` among them) carry a free-form payload
/// described by a grammar. Chat has no equivalent, so they are exposed as a
/// function taking one `input` string and the returned call is mapped back to a
/// `custom_tool_call` item — otherwise the client silently drops the call.
fn convert_tools(tools: &Value) -> (Vec<Value>, Vec<String>) {
    let Some(array) = tools.as_array() else {
        return (Vec::new(), Vec::new());
    };
    let mut out = Vec::new();
    let mut custom = Vec::new();
    for tool in array {
        let kind = tool.get("type").and_then(Value::as_str).unwrap_or("function");
        let Some(name) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };
        if kind == "custom" {
            custom.push(name.to_string());
            out.push(custom_tool_as_function(tool, name));
            continue;
        }
        if kind != "function" {
            continue;
        }
        let mut function = Map::new();
        function.insert("name".into(), Value::String(name.to_string()));
        if let Some(desc) = tool.get("description").and_then(Value::as_str) {
            function.insert("description".into(), Value::String(desc.to_string()));
        }
        function.insert(
            "parameters".into(),
            tool.get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
        );
        if let Some(strict) = tool.get("strict").and_then(Value::as_bool) {
            function.insert("strict".into(), Value::Bool(strict));
        }
        out.push(json!({"type": "function", "function": Value::Object(function)}));
    }
    (out, custom)
}

fn custom_tool_as_function(tool: &Value, name: &str) -> Value {
    let mut description = tool
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    description.push_str(
        "\n\n把完整的调用载荷原样放进 `input` 字段（字符串），不要改写成 JSON 结构或加代码围栏。",
    );
    if let Some(format) = tool.get("format") {
        if let Some(definition) = format.get("definition").and_then(Value::as_str) {
            let syntax = format
                .get("syntax")
                .and_then(Value::as_str)
                .unwrap_or("grammar");
            description.push_str("\n\n`input` 必须匹配以下 ");
            description.push_str(syntax);
            description.push_str(" 语法：\n");
            description.push_str(definition);
        }
    }
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": {
                "type": "object",
                "properties": {
                    "input": {
                        "type": "string",
                        "description": "工具载荷原文（free-form）"
                    }
                },
                "required": ["input"],
                "additionalProperties": false
            }
        }
    })
}

fn convert_tool_choice(choice: &Value) -> Option<Value> {
    match choice {
        Value::String(s) => Some(Value::String(s.clone())),
        Value::Object(obj) => {
            if let Some(name) = obj.get("name").and_then(Value::as_str) {
                return Some(json!({"type": "function", "function": {"name": name}}));
            }
            if let Some(name) = obj
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
            {
                return Some(json!({"type": "function", "function": {"name": name}}));
            }
            None
        }
        _ => None,
    }
}

fn convert_text_format(format: &Value) -> Option<Value> {
    match format.get("type").and_then(Value::as_str) {
        Some("json_schema") => {
            let mut schema = Map::new();
            if let Some(name) = format.get("name") {
                schema.insert("name".into(), name.clone());
            }
            if let Some(strict) = format.get("strict") {
                schema.insert("strict".into(), strict.clone());
            }
            schema.insert(
                "schema".into(),
                format.get("schema").cloned().unwrap_or(Value::Null),
            );
            Some(json!({"type": "json_schema", "json_schema": Value::Object(schema)}))
        }
        Some("json_object") => Some(json!({"type": "json_object"})),
        Some("text") | None => None,
        _ => None,
    }
}

fn map_usage(usage: Option<&Value>) -> Value {
    let Some(usage) = usage else {
        return json!({
            "input_tokens": 0,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": 0,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": 0,
        });
    };
    let input = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| usage.get("cached_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let reasoning = usage
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| usage.get("reasoning_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    json!({
        "input_tokens": input,
        "input_tokens_details": {"cached_tokens": cached},
        "output_tokens": output,
        "output_tokens_details": {"reasoning_tokens": reasoning},
        "total_tokens": input + output,
    })
}

fn prompt_tokens(usage: Option<&Value>) -> u64 {
    usage
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn completion_tokens(usage: Option<&Value>) -> u64 {
    usage
        .and_then(|u| u.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

struct Call {
    id: String,
    name: String,
    arguments: String,
    /// Declared as a `custom` tool, so it goes back as `custom_tool_call`.
    custom: bool,
}

impl Call {
    /// Free-form payload of a custom tool: `{"input": "..."}` flattened back to
    /// the raw text the client expects.
    fn payload(&self) -> String {
        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&self.arguments) {
            if let Some(Value::String(input)) = map.get("input") {
                return input.clone();
            }
        }
        self.arguments.clone()
    }
}

fn calls_of(agg: &ChatAggregate, custom: &[String]) -> Vec<Call> {
    agg.tool_calls
        .values()
        .filter(|c| !c.name.is_empty())
        .enumerate()
        .map(|(i, c)| Call {
            id: c.id.clone().unwrap_or_else(|| format!("call_{i}")),
            custom: custom.iter().any(|n| n == &c.name),
            name: c.name.clone(),
            arguments: if c.arguments.is_empty() {
                "{}".to_string()
            } else {
                c.arguments.clone()
            },
        })
        .collect()
}

fn assistant_messages(content: &str, calls: &[Call]) -> Vec<Value> {
    let mut out = Vec::new();
    if !content.is_empty() {
        out.push(json!({"role": "assistant", "content": content}));
    }
    if !calls.is_empty() {
        let tool_calls: Vec<Value> = calls
            .iter()
            .map(|c| {
                json!({
                    "id": c.id,
                    "type": "function",
                    "function": {"name": c.name, "arguments": c.arguments},
                })
            })
            .collect();
        out.push(json!({
            "role": "assistant",
            "content": Value::Null,
            "tool_calls": tool_calls,
        }));
    }
    out
}

fn message_item(item_id: &str, text: &str, status: &str) -> Value {
    json!({
        "id": item_id,
        "type": "message",
        "status": status,
        "role": "assistant",
        "content": [{"type": "output_text", "text": text, "annotations": []}],
    })
}

/// A `reasoning` output item carrying only the plain-text summary the upstream
/// actually produced. `encrypted_content` is deliberately absent rather than
/// empty: fabricating an opaque blob the client will echo back would promise a
/// round-trip we cannot honour (chat has nowhere to put it).
fn reasoning_item(item_id: &str, text: &str, status: &str) -> Value {
    let summary = if text.is_empty() {
        json!([])
    } else {
        json!([{"type": "summary_text", "text": text}])
    };
    json!({
        "id": item_id,
        "type": "reasoning",
        "status": status,
        "summary": summary,
    })
}

fn function_call_item(item_id: &str, call: &Call, status: &str) -> Value {
    json!({
        "id": item_id,
        "type": "function_call",
        "status": status,
        "call_id": call.id,
        "name": call.name,
        "arguments": call.arguments,
    })
}

fn custom_tool_call_item(item_id: &str, call: &Call, status: &str) -> Value {
    json!({
        "id": item_id,
        "type": "custom_tool_call",
        "status": status,
        "call_id": call.id,
        "name": call.name,
        "input": call.payload(),
    })
}

/// Pick the item shape the client declared for this tool.
fn tool_item(item_id: &str, call: &Call, status: &str) -> Value {
    if call.custom {
        custom_tool_call_item(item_id, call, status)
    } else {
        function_call_item(item_id, call, status)
    }
}

fn output_items(reasoning: &str, content: &str, calls: &[Call]) -> Value {
    let mut items = Vec::new();
    if !reasoning.is_empty() {
        items.push(reasoning_item(
            &format!("rs_{}", uuid::Uuid::new_v4().simple()),
            reasoning,
            "completed",
        ));
    }
    if !content.is_empty() {
        items.push(message_item(
            &format!("msg_{}", uuid::Uuid::new_v4().simple()),
            content,
            "completed",
        ));
    }
    for call in calls {
        items.push(tool_item(
            &format!("fc_{}", uuid::Uuid::new_v4().simple()),
            call,
            "completed",
        ));
    }
    if items.is_empty() {
        items.push(message_item(
            &format!("msg_{}", uuid::Uuid::new_v4().simple()),
            "",
            "completed",
        ));
    }
    Value::Array(items)
}

fn base_response(
    id: &str,
    model: &str,
    created: i64,
    status: &str,
    output: Value,
    usage: Value,
) -> Value {
    json!({
        "id": id,
        "object": "response",
        "created_at": created,
        "status": status,
        "model": model,
        "output": output,
        "usage": usage,
        "error": Value::Null,
        "incomplete_details": Value::Null,
        "instructions": Value::Null,
        "max_output_tokens": Value::Null,
        "parallel_tool_calls": true,
        "previous_response_id": Value::Null,
        "store": false,
        "temperature": 1.0,
        "top_p": 1.0,
        "tool_choice": "auto",
        "tools": [],
        "truncation": "disabled",
        "user": Value::Null,
        "metadata": {},
        "reasoning": {"effort": Value::Null, "summary": Value::Null},
        "text": {"format": {"type": "text"}},
    })
}

fn event(name: &str, data: Value) -> Bytes {
    Bytes::from(sse_event(name, &data.to_string()))
}

#[allow(clippy::too_many_arguments)]
fn stream_body(
    state: SharedState,
    account: Account,
    resp: reqwest::Response,
    model: String,
    response_id: String,
    created: i64,
    started: Instant,
    history_prefix: Vec<Value>,
    custom_tools: Vec<String>,
) -> Body {
    let stream = async_stream::stream! {
        let empty_usage = map_usage(None);
        let created_response = base_response(
            &response_id,
            &model,
            created,
            "in_progress",
            json!([]),
            Value::Null,
        );
        let mut created_event = created_response.clone();
        if let Some(obj) = created_event.as_object_mut() {
            obj.insert("usage".into(), Value::Null);
        }
        yield Ok::<Bytes, std::io::Error>(event(
            "response.created",
            json!({"type": "response.created", "response": created_event}),
        ));
        yield Ok(event(
            "response.in_progress",
            json!({"type": "response.in_progress", "response": created_response}),
        ));

        let message_item_id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        let reasoning_item_id = format!("rs_{}", uuid::Uuid::new_v4().simple());
        let mut reasoning_started = false;
        let mut reasoning_done = false;
        let mut reasoning_text = String::new();
        let mut text_started = false;
        // Item order is reasoning → message → tool calls; the indexes have to
        // match the final `output[]` array or clients mis-parent their events.
        let mut text_index = 0usize;
        let mut output: Vec<Value> = Vec::new();
        // Closing events for the reasoning item, emitted as soon as the model
        // moves on to visible text so each item's events stay contiguous.
        let reasoning_done_events = |item_id: &str, text: &str| -> [Bytes; 3] {
            [
                event(
                    "response.reasoning_summary_text.done",
                    json!({
                        "type": "response.reasoning_summary_text.done",
                        "item_id": item_id,
                        "output_index": 0,
                        "summary_index": 0,
                        "text": text,
                    }),
                ),
                event(
                    "response.reasoning_summary_part.done",
                    json!({
                        "type": "response.reasoning_summary_part.done",
                        "item_id": item_id,
                        "output_index": 0,
                        "summary_index": 0,
                        "part": {"type": "summary_text", "text": text},
                    }),
                ),
                event(
                    "response.output_item.done",
                    json!({
                        "type": "response.output_item.done",
                        "output_index": 0,
                        "item": reasoning_item(item_id, text, "completed"),
                    }),
                ),
            ]
        };
        let mut full_text = String::new();
        let mut parser = SseBuffer::new();
        let mut upstream = resp.bytes_stream();
        let mut scanner = TextToolCallScanner::new();
        let mut recovered: Vec<TextToolCall> = Vec::new();
        let mut agg = ChatAggregate::default();
        let mut usage: Option<Value> = None;
        let mut failed: Option<String> = None;
        let mut completed = false;

        while let Some(chunk) = upstream.next().await {
            match chunk {
                Ok(bytes) => {
                    for data in parser.push(&bytes) {
                        if data == "[DONE]" {
                            continue;
                        }
                        let Ok(v) = serde_json::from_str::<Value>(&data) else {
                            continue;
                        };
                        if let Some(message) = stream_error_message(&v) {
                            let payload = json!({
                                "type": "response.failed",
                                "response": base_response(
                                    &response_id,
                                    &model,
                                    created,
                                    "failed",
                                    json!([]),
                                    map_usage(usage.as_ref()),
                                ),
                                "error": {"code": "upstream_error", "message": message.clone()},
                            });
                            failed = Some(message);
                            yield Ok::<Bytes, std::io::Error>(event("response.failed", payload));
                            continue;
                        }
                        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                            usage = Some(u.clone());
                        }
                        agg.apply(&v);
                        let delta = v
                            .get("choices")
                            .and_then(|c| c.get(0))
                            .and_then(|c| c.get("delta"));
                        let Some(delta) = delta else { continue };
                        if let Some(reasoning) = delta
                            .get("reasoning_content")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                        {
                            // A real `reasoning` item, without `encrypted_content`:
                            // the upstream only hands back plain text, so claiming
                            // a decryptable blob would be a lie the client echoes
                            // back to us next turn.
                            if !reasoning_started && !text_started {
                                reasoning_started = true;
                                yield Ok::<Bytes, std::io::Error>(event(
                                    "response.output_item.added",
                                    json!({
                                        "type": "response.output_item.added",
                                        "output_index": 0,
                                        "item": reasoning_item(&reasoning_item_id, "", "in_progress"),
                                    }),
                                ));
                                yield Ok(event(
                                    "response.reasoning_summary_part.added",
                                    json!({
                                        "type": "response.reasoning_summary_part.added",
                                        "item_id": reasoning_item_id,
                                        "output_index": 0,
                                        "summary_index": 0,
                                        "part": {"type": "summary_text", "text": ""},
                                    }),
                                ));
                            }
                            if reasoning_started {
                                reasoning_text.push_str(reasoning);
                                yield Ok(event(
                                    "response.reasoning_summary_text.delta",
                                    json!({
                                        "type": "response.reasoning_summary_text.delta",
                                        "item_id": reasoning_item_id,
                                        "output_index": 0,
                                        "summary_index": 0,
                                        "delta": reasoning,
                                    }),
                                ));
                            }
                        }
                        let Some(content) = delta.get("content").and_then(Value::as_str) else {
                            continue;
                        };
                        if content.is_empty() {
                            continue;
                        }
                        let (visible, calls) = scanner.push(content);
                        recovered.extend(calls);
                        if visible.is_empty() {
                            continue;
                        }
                        // Visible text means the thinking phase ended: close the
                        // reasoning item before opening the message item.
                        if reasoning_started && !reasoning_done {
                            reasoning_done = true;
                            for e in reasoning_done_events(&reasoning_item_id, &reasoning_text) {
                                yield Ok::<Bytes, std::io::Error>(e);
                            }
                            output.push(reasoning_item(
                                &reasoning_item_id,
                                &reasoning_text,
                                "completed",
                            ));
                        }
                        if !text_started {
                            text_started = true;
                            text_index = output.len();
                            yield Ok::<Bytes, std::io::Error>(event(
                                "response.output_item.added",
                                json!({
                                    "type": "response.output_item.added",
                                    "output_index": text_index,
                                    "item": message_item(&message_item_id, "", "in_progress"),
                                }),
                            ));
                            yield Ok(event(
                                "response.content_part.added",
                                json!({
                                    "type": "response.content_part.added",
                                    "item_id": message_item_id,
                                    "output_index": text_index,
                                    "content_index": 0,
                                    "part": {"type": "output_text", "text": "", "annotations": []},
                                }),
                            ));
                        }
                        full_text.push_str(&visible);
                        yield Ok::<Bytes, std::io::Error>(event(
                            "response.output_text.delta",
                            json!({
                                "type": "response.output_text.delta",
                                "item_id": message_item_id,
                                "output_index": text_index,
                                "content_index": 0,
                                "delta": visible,
                            }),
                        ));
                    }
                }
                Err(e) => {
                    let message = format!("upstream stream error: {e}");
                    failed = Some(message.clone());
                    yield Ok::<Bytes, std::io::Error>(event(
                        "response.failed",
                        json!({
                            "type": "response.failed",
                            "response": base_response(
                                &response_id,
                                &model,
                                created,
                                "failed",
                                json!([]),
                                map_usage(usage.as_ref()),
                            ),
                            "error": {"code": "upstream_error", "message": message},
                        }),
                    ));
                    completed = true;
                    break;
                }
            }
        }

        if !completed {
            let (tail_text, tail_calls) = scanner.finish();
            recovered.extend(tail_calls);
            if !tail_text.is_empty() {
                if reasoning_started && !reasoning_done {
                    reasoning_done = true;
                    for e in reasoning_done_events(&reasoning_item_id, &reasoning_text) {
                        yield Ok::<Bytes, std::io::Error>(e);
                    }
                    output.push(reasoning_item(&reasoning_item_id, &reasoning_text, "completed"));
                }
                if !text_started {
                    text_started = true;
                    text_index = output.len();
                    yield Ok::<Bytes, std::io::Error>(event(
                        "response.output_item.added",
                        json!({
                            "type": "response.output_item.added",
                            "output_index": text_index,
                            "item": message_item(&message_item_id, "", "in_progress"),
                        }),
                    ));
                    yield Ok(event(
                        "response.content_part.added",
                        json!({
                            "type": "response.content_part.added",
                            "item_id": message_item_id,
                            "output_index": text_index,
                            "content_index": 0,
                            "part": {"type": "output_text", "text": "", "annotations": []},
                        }),
                    ));
                }
                full_text.push_str(&tail_text);
                yield Ok::<Bytes, std::io::Error>(event(
                    "response.output_text.delta",
                    json!({
                        "type": "response.output_text.delta",
                        "item_id": message_item_id,
                        "output_index": text_index,
                        "content_index": 0,
                        "delta": tail_text,
                    }),
                ));
            }

            // Reasoning-only turn: no visible text ever arrived, so the item was
            // never closed above.
            if reasoning_started && !reasoning_done {
                for e in reasoning_done_events(&reasoning_item_id, &reasoning_text) {
                    yield Ok::<Bytes, std::io::Error>(e);
                }
                output.push(reasoning_item(&reasoning_item_id, &reasoning_text, "completed"));
            }

            if text_started {
                yield Ok::<Bytes, std::io::Error>(event(
                    "response.output_text.done",
                    json!({
                        "type": "response.output_text.done",
                        "item_id": message_item_id,
                        "output_index": text_index,
                        "content_index": 0,
                        "text": full_text,
                    }),
                ));
                let part = json!({"type": "output_text", "text": full_text, "annotations": []});
                yield Ok(event(
                    "response.content_part.done",
                    json!({
                        "type": "response.content_part.done",
                        "item_id": message_item_id,
                        "output_index": text_index,
                        "content_index": 0,
                        "part": part,
                    }),
                ));
                let item = message_item(&message_item_id, &full_text, "completed");
                yield Ok(event(
                    "response.output_item.done",
                    json!({
                        "type": "response.output_item.done",
                        "output_index": text_index,
                        "item": item,
                    }),
                ));
                output.push(item);
            }

            let calls = recover_calls(&recovered, &agg, &custom_tools);
            let base_index = output.len();
            for (i, call) in calls.iter().enumerate() {
                let output_index = base_index + i;
                let item_id = format!("fc_{}", uuid::Uuid::new_v4().simple());
                let mut added = tool_item(&item_id, call, "in_progress");
                if let Some(obj) = added.as_object_mut() {
                    // Announce the call with an empty payload, then stream it.
                    if call.custom {
                        obj.insert("input".into(), Value::String(String::new()));
                    } else {
                        obj.insert("arguments".into(), Value::String(String::new()));
                    }
                }
                yield Ok::<Bytes, std::io::Error>(event(
                    "response.output_item.added",
                    json!({
                        "type": "response.output_item.added",
                        "output_index": output_index,
                        "item": added,
                    }),
                ));
                if call.custom {
                    let payload = call.payload();
                    yield Ok(event(
                        "response.custom_tool_call_input.delta",
                        json!({
                            "type": "response.custom_tool_call_input.delta",
                            "item_id": item_id,
                            "output_index": output_index,
                            "delta": payload,
                        }),
                    ));
                    yield Ok(event(
                        "response.custom_tool_call_input.done",
                        json!({
                            "type": "response.custom_tool_call_input.done",
                            "item_id": item_id,
                            "output_index": output_index,
                            "input": payload,
                        }),
                    ));
                } else {
                    yield Ok(event(
                        "response.function_call_arguments.delta",
                        json!({
                            "type": "response.function_call_arguments.delta",
                            "item_id": item_id,
                            "output_index": output_index,
                            "delta": call.arguments,
                        }),
                    ));
                    yield Ok(event(
                        "response.function_call_arguments.done",
                        json!({
                            "type": "response.function_call_arguments.done",
                            "item_id": item_id,
                            "output_index": output_index,
                            "arguments": call.arguments,
                        }),
                    ));
                }
                let item = tool_item(&item_id, call, "completed");
                yield Ok(event(
                    "response.output_item.done",
                    json!({
                        "type": "response.output_item.done",
                        "output_index": output_index,
                        "item": item,
                    }),
                ));
                output.push(item);
            }

            if output.is_empty() {
                // Never complete with an empty output: some clients index
                // `output[0]` unconditionally.
                let item = message_item(&message_item_id, "", "completed");
                yield Ok::<Bytes, std::io::Error>(event(
                    "response.output_item.added",
                    json!({
                        "type": "response.output_item.added",
                        "output_index": 0,
                        "item": message_item(&message_item_id, "", "in_progress"),
                    }),
                ));
                yield Ok(event(
                    "response.output_item.done",
                    json!({
                        "type": "response.output_item.done",
                        "output_index": 0,
                        "item": item,
                    }),
                ));
                output.push(item);
            }

            let response_usage = if usage.is_some() {
                map_usage(usage.as_ref())
            } else {
                empty_usage
            };
            let response = base_response(
                &response_id,
                &model,
                created,
                "completed",
                Value::Array(output.clone()),
                response_usage,
            );
            yield Ok::<Bytes, std::io::Error>(event(
                "response.completed",
                json!({"type": "response.completed", "response": response}),
            ));

            let mut history = history_prefix;
            history.extend(assistant_messages(&full_text, &calls));
            save_history(response_id.clone(), history);
        }

        let latency = started.elapsed().as_millis() as u64;
        state
            .record_request(RequestRecord {
                at: now_ms(),
                model: model.clone(),
                account_id: account.id.clone(),
                account_name: account.name.clone(),
                stream: true,
                ok: failed.is_none(),
                status: 200,
                latency_ms: latency,
                prompt_tokens: prompt_tokens(usage.as_ref()),
                completion_tokens: completion_tokens(usage.as_ref()),
                error: failed,
            })
            .await;
    };
    Body::from_stream(stream)
}

/// Merge the calls the upstream emitted structurally with the ones recovered
/// from in-band DSML markup, de-duplicated by (name, arguments) so a chatty turn
/// is never replayed twice. Each call gets a fresh id because clients echo it
/// back in `function_call_output`.
fn recover_calls(
    recovered: &[TextToolCall],
    agg: &ChatAggregate,
    custom: &[String],
) -> Vec<Call> {
    let mut out: Vec<Call> = Vec::new();
    let mut seen: BTreeMap<(String, String), ()> = BTreeMap::new();
    let mut push = |name: String, arguments: String, out: &mut Vec<Call>| {
        if seen.insert((name.clone(), arguments.clone()), ()).is_none() {
            out.push(Call {
                id: format!("call_{}", uuid::Uuid::new_v4().simple()),
                custom: custom.iter().any(|n| n == &name),
                name,
                arguments,
            });
        }
    };
    for call in recovered {
        push(
            call.name.clone(),
            call.arguments.to_string(),
            &mut out,
        );
    }
    for call in calls_of(agg, custom) {
        push(call.name, call.arguments, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_string_input_with_instructions() {
        let body = json!({
            "instructions": "be brief",
            "input": "hi",
            "max_output_tokens": 64,
            "temperature": 0.2,
        });
        let messages = build_messages(body.as_object().unwrap(), None).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["content"], "hi");
    }

    #[test]
    fn continuation_nudge_sits_right_after_the_last_user_turn() {
        let body = json!({
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "fix the bug"}]},
                {"type": "function_call", "name": "shell", "arguments": "{}", "call_id": "call_1"},
                {"type": "function_call_output", "call_id": "call_1", "output": "ok"},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "继续"}]},
            ],
            "tools": [{"type": "function", "name": "shell", "parameters": {"type": "object"}}],
        });
        let messages = build_messages(body.as_object().unwrap(), None).unwrap();
        let nudge_at = messages
            .iter()
            .position(|m| m.get("content").and_then(Value::as_str) == Some(CONTINUATION_NUDGE))
            .expect("nudge message missing");
        assert_eq!(messages[nudge_at]["role"], "system");
        assert_eq!(messages[nudge_at - 1]["content"], "继续");
        assert_eq!(messages.len(), 5);
    }

    #[test]
    fn no_nudge_without_tools() {
        let body = json!({
            "input": "hi",
            "tools": [],
        });
        let messages = build_messages(body.as_object().unwrap(), None).unwrap();
        assert!(!messages
            .iter()
            .any(|m| m.get("content").and_then(Value::as_str) == Some(CONTINUATION_NUDGE)));
    }

    #[test]
    fn converts_function_call_round_trip_into_tool_messages() {
        let body = json!({
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "list files"}]},
                {"type": "function_call", "name": "shell", "arguments": "{\"cmd\":\"ls\"}", "call_id": "call_1"},
                {"type": "function_call_output", "call_id": "call_1", "output": "a.txt"}
            ]
        });
        let messages = build_messages(body.as_object().unwrap(), None).unwrap();
        assert_eq!(messages[0]["content"], "list files");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["tool_calls"][0]["function"]["name"], "shell");
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call_1");
        assert_eq!(messages[2]["content"], "a.txt");
    }

    #[test]
    fn merges_parallel_function_calls_into_one_assistant_message() {
        let body = json!({
            "input": [
                {"type": "function_call", "name": "a", "arguments": "{}", "call_id": "c1"},
                {"type": "function_call", "name": "b", "arguments": "{}", "call_id": "c2"}
            ]
        });
        let messages = build_messages(body.as_object().unwrap(), None).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["tool_calls"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn converts_flat_tools_to_chat_shape() {
        let tools = json!([
            {"type": "function", "name": "shell", "description": "run", "parameters": {"type": "object"}},
            {"type": "local_shell"}
        ]);
        let (converted, custom) = convert_tools(&tools);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["function"]["name"], "shell");
        assert_eq!(converted[0]["type"], "function");
        assert!(custom.is_empty());
    }

    #[test]
    fn wraps_custom_tools_as_single_string_functions() {
        let tools = json!([{
            "type": "custom",
            "name": "apply_patch",
            "description": "Edit files. This is a FREEFORM tool.",
            "format": {"type": "grammar", "syntax": "lark", "definition": "start: patch"}
        }]);
        let (converted, custom) = convert_tools(&tools);
        assert_eq!(custom, vec!["apply_patch".to_string()]);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["type"], "function");
        assert_eq!(converted[0]["function"]["name"], "apply_patch");
        assert_eq!(
            converted[0]["function"]["parameters"]["required"][0],
            "input"
        );
        let description = converted[0]["function"]["description"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(description.contains("lark"), "{description}");
    }

    #[test]
    fn reasoning_becomes_the_first_output_item_without_encrypted_content() {
        let items = output_items("先看调用链", "改好了", &[]);
        let array = items.as_array().unwrap();
        assert_eq!(array[0]["type"], "reasoning");
        assert_eq!(array[0]["summary"][0]["type"], "summary_text");
        assert_eq!(array[0]["summary"][0]["text"], "先看调用链");
        // Absent, not empty: an opaque blob we cannot decrypt would be echoed
        // back on the next turn and silently dropped.
        assert!(array[0].get("encrypted_content").is_none());
        assert_eq!(array[1]["type"], "message");
    }

    #[test]
    fn reasoning_item_is_skipped_when_the_upstream_sent_none() {
        let items = output_items("", "只有正文", &[]);
        let array = items.as_array().unwrap();
        assert_eq!(array.len(), 1);
        assert_eq!(array[0]["type"], "message");
    }

    #[test]
    fn maps_custom_calls_back_to_custom_tool_call_items() {
        let calls = vec![Call {
            id: "call_1".into(),
            name: "apply_patch".into(),
            arguments: json!({"input": "*** Begin Patch\n*** End Patch"}).to_string(),
            custom: true,
        }];
        let items = output_items("", "", &calls);
        let array = items.as_array().unwrap();
        assert_eq!(array[0]["type"], "custom_tool_call");
        assert_eq!(array[0]["input"], "*** Begin Patch\n*** End Patch");
        assert_eq!(array[0]["call_id"], "call_1");
        assert!(array[0].get("arguments").is_none());
    }

    #[test]
    fn custom_call_without_wrapper_still_yields_raw_payload() {
        let call = Call {
            id: "c".into(),
            name: "apply_patch".into(),
            arguments: "*** Begin Patch".into(),
            custom: true,
        };
        assert_eq!(call.payload(), "*** Begin Patch");
    }

    #[test]
    fn replays_custom_tool_call_history_as_function_arguments() {
        let body = json!({
            "input": [
                {"type": "custom_tool_call", "name": "apply_patch", "input": "*** Begin Patch", "call_id": "c1"}
            ]
        });
        let messages = build_messages(body.as_object().unwrap(), None).unwrap();
        assert_eq!(messages[0]["tool_calls"][0]["function"]["name"], "apply_patch");
        let args = messages[0]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        assert_eq!(serde_json::from_str::<Value>(args).unwrap()["input"], "*** Begin Patch");
    }

    #[test]
    fn maps_usage_details() {
        let usage = json!({
            "prompt_tokens": 10,
            "completion_tokens": 4,
            "prompt_tokens_details": {"cached_tokens": 3},
            "completion_tokens_details": {"reasoning_tokens": 2}
        });
        let mapped = map_usage(Some(&usage));
        assert_eq!(mapped["input_tokens"], 10);
        assert_eq!(mapped["output_tokens"], 4);
        assert_eq!(mapped["total_tokens"], 14);
        assert_eq!(mapped["input_tokens_details"]["cached_tokens"], 3);
        assert_eq!(mapped["output_tokens_details"]["reasoning_tokens"], 2);
    }

    #[test]
    fn builds_text_and_function_call_items() {
        let calls = vec![Call {
            id: "call_1".into(),
            name: "shell".into(),
            arguments: "{}".into(),
            custom: false,
        }];
        let items = output_items("", "hello", &calls);
        let array = items.as_array().unwrap();
        assert_eq!(array.len(), 2);
        assert_eq!(array[0]["type"], "message");
        assert_eq!(array[0]["content"][0]["text"], "hello");
        assert_eq!(array[1]["type"], "function_call");
        assert_eq!(array[1]["call_id"], "call_1");
    }

    #[test]
    fn empty_output_still_has_one_item() {
        let items = output_items("", "", &[]);
        assert_eq!(items.as_array().unwrap().len(), 1);
    }

    #[test]
    fn replays_previous_response_history() {
        let id = "resp_test_history".to_string();
        save_history(
            id.clone(),
            vec![json!({"role": "user", "content": "first"})],
        );
        let body = json!({"input": "second"});
        let messages = build_messages(body.as_object().unwrap(), Some(&id)).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["content"], "first");
        assert_eq!(messages[1]["content"], "second");
    }

    #[test]
    fn keeps_system_prompt_ahead_of_replayed_history() {
        let id = "resp_test_history_sys".to_string();
        save_history(id.clone(), vec![json!({"role": "user", "content": "first"})]);
        let body = json!({"instructions": "sys", "input": "second"});
        let messages = build_messages(body.as_object().unwrap(), Some(&id)).unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["content"], "first");
        assert_eq!(messages[2]["content"], "second");
    }
}
