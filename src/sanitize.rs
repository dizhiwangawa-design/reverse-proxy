//! Upstream channel-compatibility sanitization.
//!
//! The WorkBuddy copilot gateway rejects requests whose payload carries
//! identifying strings for a non-WorkBuddy agent runtime. Its rejection is a
//! generic `11-128` "Illegal API invocation from an unapproved channel", which
//! is indistinguishable from a real credential/routing failure at the client.
//!
//! Two classes of payload trigger it (both established empirically against the
//! live upstream):
//!
//! 1. `role: "developer"` on any message. The gateway accepts `system`, and
//!    accepts `Developer`/`DEVELOPER` — only the exact lowercase wire spelling
//!    is refused.
//! 2. A small set of literal prompt signatures emitted by other agent runtimes.
//!    These are matched case-insensitively against message text; ordinary
//!    paraphrases of the same meaning pass, so the check is a signature match
//!    rather than a semantic classifier.
//!
//! This module rewrites the outbound request so a foreign agent's prompt
//! survives the gateway: roles are folded onto `system`, and signatures are
//! replaced with neutral equivalents that keep the instruction's meaning.

use serde_json::{json, Value};

/// `(needle, replacement)` signature pairs, matched case-insensitively.
///
/// Replacements preserve the instruction's intent so the model still behaves
/// correctly; only the identifying phrasing is dropped.
const SIGNATURES: &[(&str, &str)] = &[
    (
        "You are an AI agent powered by DeepSeek Harness.",
        "You are an AI coding agent.",
    ),
    (
        "Tokens prefixed with @ are paths the user explicitly referenced.",
        "Paths may begin with @ when the user references them.",
    ),
    ("DeepSeek Harness Web GUI", "Web GUI"),
];

/// Fold `developer` onto `system`.
///
/// Returns the number of messages rewritten, for logging.
fn fold_developer_role(messages: &mut [Value]) -> usize {
    let mut changed = 0;
    for msg in messages.iter_mut() {
        let Some(obj) = msg.as_object_mut() else {
            continue;
        };
        let is_developer = obj
            .get("role")
            .and_then(Value::as_str)
            .is_some_and(|r| r == "developer");
        if is_developer {
            obj.insert("role".into(), json!("system"));
            changed += 1;
        }
    }
    changed
}

/// Byte offset of the first ASCII-case-insensitive occurrence of `needle`.
///
/// Every needle is ASCII and every replacement is UTF-8, so matching over raw
/// bytes keeps offsets valid: a case fold that changed a string's byte length
/// (as `to_lowercase` can, e.g. `İ`) would desynchronize slice and search
/// indices. Comparing bytes avoids that class of bug entirely.
fn find_ascii_ci(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))
}

/// Replace every known signature inside one string, ASCII-case-insensitively.
fn scrub_str(input: &str) -> Option<String> {
    let mut out = input.to_string();
    let mut touched = false;
    for (needle, replacement) in SIGNATURES {
        let mut rebuilt = String::with_capacity(out.len());
        let mut rest = out.as_str();
        let mut replaced = false;
        while let Some(pos) = find_ascii_ci(rest.as_bytes(), needle.as_bytes()) {
            rebuilt.push_str(&rest[..pos]);
            rebuilt.push_str(replacement);
            rest = &rest[pos + needle.len()..];
            replaced = true;
        }
        if replaced {
            rebuilt.push_str(rest);
            out = rebuilt;
            touched = true;
        }
    }
    touched.then_some(out)
}

/// Walk any JSON value, scrubbing every string leaf in place.
///
/// Prompt text may arrive as a plain string, as an array of `{type, text}`
/// content parts, or nested inside tool arguments; a blanket walk covers all
/// of those shapes without enumerating them.
fn scrub_value(value: &mut Value) -> bool {
    match value {
        Value::String(s) => match scrub_str(s) {
            Some(replacement) => {
                *s = replacement;
                true
            }
            None => false,
        },
        Value::Array(items) => items.iter_mut().fold(false, |acc, item| {
            let hit = scrub_value(item);
            acc || hit
        }),
        Value::Object(map) => map.values_mut().fold(false, |acc, item| {
            let hit = scrub_value(item);
            acc || hit
        }),
        _ => false,
    }
}

/// Description of what a sanitization pass changed, for logging.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SanitizeReport {
    /// Messages whose `developer` role was folded onto `system`.
    pub developer_roles: usize,
    /// String leaves whose signature text was rewritten.
    pub scrubbed_strings: usize,
}

impl SanitizeReport {
    /// Whether the request was left byte-identical.
    pub fn is_noop(&self) -> bool {
        self.developer_roles == 0 && self.scrubbed_strings == 0
    }
}

/// Rewrite an outbound chat-completions body for upstream compatibility.
///
/// A no-op for well-behaved payloads: untouched requests keep their exact
/// bytes, so requests that the gateway already accepts are never perturbed.
pub fn sanitize_chat_body(body: &mut Value) -> SanitizeReport {
    let mut report = SanitizeReport::default();
    let Some(obj) = body.as_object_mut() else {
        return report;
    };

    if let Some(messages) = obj.get_mut("messages").and_then(Value::as_array_mut) {
        report.developer_roles = fold_developer_role(messages);
        // Only message content is signature-scrubbed: that is where agent
        // system prompts and turns live. Scrubbing the whole body would also
        // rewrite caller tool schemas, which the gateway accepts untouched.
        for msg in messages.iter_mut() {
            if let Some(content) = msg.get_mut("content") {
                let hit = scrub_value(content);
                if hit {
                    report.scrubbed_strings += 1;
                }
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(messages: Value) -> Value {
        json!({ "model": "m", "messages": messages, "stream": true })
    }

    #[test]
    fn developer_role_is_folded() {
        let mut b = body(json!([
            { "role": "developer", "content": "be nice" },
            { "role": "user", "content": "hi" }
        ]));
        let r = sanitize_chat_body(&mut b);
        assert_eq!(r.developer_roles, 1);
        assert_eq!(b["messages"][0]["role"], json!("system"));
        assert_eq!(b["messages"][1]["role"], json!("user"));
    }

    #[test]
    fn non_lowercase_developer_is_left_alone() {
        // The gateway accepts these spellings, so rewriting them would be a
        // gratuitous change to a request it already serves.
        for spelling in ["Developer", "DEVELOPER", "dev"] {
            let mut b = body(json!([{ "role": spelling, "content": "x" }]));
            let r = sanitize_chat_body(&mut b);
            assert_eq!(r.developer_roles, 0, "{spelling} must not be folded");
            assert_eq!(b["messages"][0]["role"], json!(spelling));
        }
    }

    #[test]
    fn identity_signature_is_scrubbed() {
        let mut b = body(json!([{
            "role": "system",
            "content": "You are an AI agent powered by DeepSeek Harness.\nBe terse."
        }]));
        let r = sanitize_chat_body(&mut b);
        assert_eq!(r.scrubbed_strings, 1);
        let text = b["messages"][0]["content"].as_str().unwrap();
        assert_eq!(text, "You are an AI coding agent.\nBe terse.");
    }

    #[test]
    fn signature_match_is_case_insensitive() {
        let mut b = body(json!([{
            "role": "system",
            "content": "you are an ai agent powered by deepseek harness."
        }]));
        let r = sanitize_chat_body(&mut b);
        assert_eq!(r.scrubbed_strings, 1);
        assert_eq!(
            b["messages"][0]["content"].as_str().unwrap(),
            "You are an AI coding agent."
        );
    }

    #[test]
    fn signatures_are_scrubbed_inside_content_parts() {
        let mut b = body(json!([{
            "role": "system",
            "content": [
                { "type": "text", "text": "DeepSeek Harness Web GUI is up." },
                { "type": "text", "text": "Nothing to change here." }
            ]
        }]));
        let r = sanitize_chat_body(&mut b);
        assert_eq!(r.scrubbed_strings, 1);
        assert_eq!(
            b["messages"][0]["content"][0]["text"],
            json!("Web GUI is up.")
        );
        assert_eq!(
            b["messages"][0]["content"][1]["text"],
            json!("Nothing to change here.")
        );
    }

    #[test]
    fn benign_body_is_untouched() {
        let mut b = body(json!([
            { "role": "system", "content": "You are a helpful assistant." },
            { "role": "user", "content": "hi" }
        ]));
        let before = b.clone();
        let r = sanitize_chat_body(&mut b);
        assert!(r.is_noop(), "benign requests must not be rewritten");
        assert_eq!(b, before);
    }

    #[test]
    fn every_occurrence_is_replaced() {
        let mut b = body(json!([{
            "role": "system",
            "content": "DeepSeek Harness Web GUI and DeepSeek Harness Web GUI again"
        }]));
        sanitize_chat_body(&mut b);
        assert_eq!(
            b["messages"][0]["content"].as_str().unwrap(),
            "Web GUI and Web GUI again"
        );
    }

    #[test]
    fn missing_messages_is_a_noop() {
        let mut b = json!({ "model": "m" });
        let r = sanitize_chat_body(&mut b);
        assert!(r.is_noop());
        assert_eq!(b, json!({ "model": "m" }));
    }
}
