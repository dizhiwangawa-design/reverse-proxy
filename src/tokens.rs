//! Best-effort token estimation.
//!
//! WorkBuddy does not expose a public tokenizer, so the proxy uses a
//! conservative character-based approximation. The result is only used by
//! `/v1/messages/count_tokens`, which clients such as Claude Code use to keep
//! track of their context window. Over-estimating slightly is safer than
//! under-estimating, because it makes clients compact a little earlier.

use serde_json::Value;

/// Approximate number of tokens in a plain string.
pub fn estimate_text_tokens(text: &str) -> u64 {
    let mut ascii = 0u64;
    let mut wide = 0u64;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii += 1;
        } else {
            wide += 1;
        }
    }
    // Latin text is roughly 4 characters per token; CJK/emoji are closer to
    // 1-2 tokens per character.
    let estimate = (ascii as f64 / 4.0) + (wide as f64 * 1.5);
    estimate.ceil() as u64
}

/// Approximate prompt tokens for an OpenAI-style chat request body.
pub fn estimate_chat_tokens(body: &Value) -> u64 {
    let mut total = 3u64; // reply priming / role boilerplate
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for message in messages {
            total += 4; // per-message framing
            total += estimate_value_tokens(message.get("content"));
            if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
                for call in tool_calls {
                    total += 4;
                    if let Some(name) = call
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                    {
                        total += estimate_text_tokens(name);
                    }
                    total += estimate_value_tokens(
                        call.get("function").and_then(|f| f.get("arguments")),
                    );
                }
            }
        }
    }
    if let Some(tools) = body.get("tools") {
        // Tool schemas are serialized JSON; estimating the serialized form is
        // close enough for context accounting.
        if let Ok(raw) = serde_json::to_string(tools) {
            total += estimate_text_tokens(&raw) + 8;
        }
    }
    total.max(1)
}

fn estimate_value_tokens(value: Option<&Value>) -> u64 {
    let Some(value) = value else { return 0 };
    match value {
        Value::Null => 0,
        Value::String(s) => estimate_text_tokens(s),
        Value::Array(parts) => parts
            .iter()
            .map(|part| {
                let kind = part.get("type").and_then(Value::as_str).unwrap_or("");
                match kind {
                    // Vision inputs cost roughly a fixed number of tokens for
                    // a typical screenshot; use a mid-range approximation.
                    "image_url" | "image" => 800,
                    "text" => {
                        estimate_text_tokens(part.get("text").and_then(Value::as_str).unwrap_or(""))
                    }
                    _ => serde_json::to_string(part)
                        .map(|s| estimate_text_tokens(&s))
                        .unwrap_or(0),
                }
            })
            .sum(),
        other => serde_json::to_string(other)
            .map(|s| estimate_text_tokens(&s))
            .unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_text_is_zero() {
        assert_eq!(estimate_text_tokens(""), 0);
    }

    #[test]
    fn latin_text_uses_four_chars_per_token() {
        assert_eq!(estimate_text_tokens("abcdefgh"), 2);
        assert_eq!(estimate_text_tokens("hello world"), 3);
    }

    #[test]
    fn cjk_text_is_not_underestimated() {
        let estimate = estimate_text_tokens("你好，世界");
        assert!(estimate >= 5, "unexpected estimate: {estimate}");
    }

    #[test]
    fn chat_body_counts_messages_and_tools() {
        let body = json!({
            "model": "auto",
            "messages": [
                {"role": "system", "content": "You are a helpful assistant."},
                {"role": "user", "content": "hello"}
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
                }
            }]
        });
        let estimate = estimate_chat_tokens(&body);
        assert!(estimate > 10, "unexpected estimate: {estimate}");
    }

    #[test]
    fn multimodal_content_counts_images() {
        let body = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "what is this?"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
                ]
            }]
        });
        assert!(estimate_chat_tokens(&body) > 800);
    }

    #[test]
    fn always_at_least_one_token() {
        assert_eq!(estimate_chat_tokens(&json!({})), 3);
    }
}
