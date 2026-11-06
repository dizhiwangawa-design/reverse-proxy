//! Recovery of structured tool calls from the upstream's text protocol.
//!
//! The copilot gateway accepts `tools` and answers with HTTP 200, but it does
//! not emit OpenAI's structured `tool_calls` field: every delta carries an
//! empty `tool_calls` array and the invocation arrives as text inside
//! `content`, using DeepSeek's in-band markup:
//!
//! ```text
//! <｜｜DSML｜｜ calls>
//! <｜｜DSML｜｜ invoke name="read">
//! <｜｜DSML｜｜ parameter name="path" string="true">/etc/hostname</｜｜DSML｜｜ parameter>
//! </｜｜DSML｜｜ invoke>
//! </｜｜DSML｜｜ calls>
//! ```
//!
//! A standard OpenAI client (DSH included) only reads `tool_calls`, so without
//! translation every tool appears broken: the model believes it acted, the
//! runtime sees prose.
//!
//! The `string` attribute carries the value's type. `string="true"` means the
//! body is literal text (`echo hi`, including `<` and `"` unchanged);
//! `string="false"` means the body is already JSON and is spliced in verbatim,
//! which is how arrays, objects, numbers and booleans travel.
//!
//! Parsing is incremental. SSE deltas split the markup at arbitrary boundaries
//! — mid-tag, mid-attribute, mid-payload — so the scanner works on a buffer,
//! holds back any trailing bytes that could still grow into a marker, and
//! emits a call only once its enclosing block is complete.

use serde_json::{json, Map, Value};

/// The full-width vertical bars (U+FF5C) the upstream wraps its markers in.
const BAR: &str = "\u{FF5C}\u{FF5C}";

/// Opening and closing delimiters around the in-band markup.
fn open_delim() -> String {
    format!("<{BAR}DSML{BAR}")
}

fn close_delim() -> String {
    format!("</{BAR}DSML{BAR}")
}

/// One recovered invocation, before it becomes an OpenAI `tool_calls` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct TextToolCall {
    /// Tool name, from `invoke name="..."`.
    pub name: String,
    /// Parsed arguments object.
    pub arguments: Value,
}

/// Incremental extractor for the in-band tool-call markup.
///
/// Feed content deltas with [`push`](Self::push); it returns the assistant text
/// that should actually be shown (markup removed) plus any tool calls that
/// completed. Call [`finish`](Self::finish) at end of stream to flush a call
/// whose closing tag never arrived.
#[derive(Debug, Default)]
pub struct TextToolCallScanner {
    /// Bytes seen but not yet emitted: either a partial marker, or an
    /// incomplete block being accumulated.
    pending: String,
    /// Whether we are inside a `<DSML calls>` block.
    in_calls: bool,
}

impl TextToolCallScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one content delta.
    ///
    /// Returns the assistant text to forward and the calls that completed. The
    /// text never contains the markup; surrounding prose is preserved.
    pub fn push(&mut self, delta: &str) -> (String, Vec<TextToolCall>) {
        self.pending.push_str(delta);
        let mut visible = String::new();
        let mut calls = Vec::new();
        loop {
            if !self.in_calls {
                let Some(start) = find_calls_open(&self.pending) else {
                    // No opener yet: emit everything that cannot be the start
                    // of one, keeping a short tail that still could be.
                    let keep = partial_marker_tail(&self.pending);
                    let cut = self.pending.len() - keep;
                    visible.push_str(&self.pending[..cut]);
                    self.pending.drain(..cut);
                    break;
                };
                // Prose before the block is ordinary assistant text.
                visible.push_str(&self.pending[..start]);
                self.pending.drain(..start);
                self.in_calls = true;
                // Drop the opener itself.
                let opener_len = calls_open_len(&self.pending);
                self.pending.drain(..opener_len);
            }
            // Inside a block: wait for the closing `</DSML calls>`.
            let Some(end) = find_calls_close(&self.pending) else {
                break;
            };
            let block: String = self.pending[..end].to_string();
            // The closer sits at `end`, not at the start of the buffer, so its
            // length must be measured from that offset.
            let close_len = closer_len_at(&self.pending[end..]);
            self.pending.drain(..end + close_len);
            self.in_calls = false;
            calls.extend(parse_block(&block));
            // Continue: another block may follow in the same buffer.
        }
        (visible, calls)
    }

    /// Flush at end of stream.
    ///
    /// A block left open by a truncated response is still recovered when it
    /// holds a complete `<invoke>`: that is a real call the model intended, and
    /// dropping it would silently strand the turn. Anything else is returned as
    /// text so no bytes are lost.
    pub fn finish(&mut self) -> (String, Vec<TextToolCall>) {
        if self.pending.is_empty() && !self.in_calls {
            return (String::new(), Vec::new());
        }
        let buffered = std::mem::take(&mut self.pending);
        self.in_calls = false;
        if buffered.trim().is_empty() {
            return (String::new(), Vec::new());
        }
        let calls = parse_block(&buffered);
        if calls.is_empty() {
            (buffered, Vec::new())
        } else {
            (String::new(), calls)
        }
    }
}

/// Length of the opener `<DSML calls>` at the start of `s`, if present.
fn calls_open_len(s: &str) -> usize {
    marker_len_at(s, &open_delim())
}

/// Byte offset where a complete `calls>` opener begins, if any.
fn find_calls_open(s: &str) -> Option<usize> {
    find_marker(s, &open_delim())
}

/// Length of the closer `</DSML calls>` at the start of `s`, if present.
fn closer_len_at(s: &str) -> usize {
    marker_len_at(s, &close_delim())
}

/// Byte offset where a complete `</DSML calls>` closer begins, if any.
fn find_calls_close(s: &str) -> Option<usize> {
    find_marker(s, &close_delim())
}

/// Consume one `<...DSML...>` delimiter plus its `calls>` tail, spaced or not.
///
/// Returns the delimiter's own length when the tail is absent, so a caller that
/// already located a complete marker never double-counts.
fn marker_len_at(s: &str, delim: &str) -> usize {
    if !s.starts_with(delim) {
        return 0;
    }
    let rest = &s[delim.len()..];
    let trimmed = rest.trim_start();
    let ws = rest.len() - trimmed.len();
    if trimmed.starts_with("calls>") {
        return delim.len() + ws + "calls>".len();
    }
    delim.len()
}

/// Find the first offset where `delim` is followed by a `calls>` tail.
///
/// The tail may be separated by whitespace or abut the delimiter directly, so a
/// bare delimiter that belongs to some other construct (`invoke`, `parameter`)
/// is skipped rather than mistaken for a block boundary.
fn find_marker(s: &str, delim: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(rel) = s[from..].find(delim) {
        let idx = from + rel;
        let after = &s[idx + delim.len()..];
        if after.trim_start().starts_with("calls>") {
            return Some(idx);
        }
        from = idx + delim.len();
    }
    None
}

/// How many trailing bytes of `s` could still become a marker.
///
/// A marker only becomes actionable once its trailing `calls>` has arrived, and
/// SSE deltas split anywhere — after `<`, inside `DSML`, or between the marker
/// and `calls>`. So the unit held back is the *whole* opener or closer,
/// `calls>` included; holding only the delimiter would let a complete
/// delimiter with a missing `calls>` leak out as prose, and the block would
/// then never open.
///
/// Both a spaced and an unspaced spelling are accepted, since the upstream uses
/// `<DSML calls>` while a tighter variant is equally plausible.
fn partial_marker_tail(s: &str) -> usize {
    let open = open_delim();
    let close = close_delim();
    let tokens = [
        format!("{open} calls>"),
        format!("{close} calls>"),
        format!("{open}calls>"),
        format!("{close}calls>"),
    ];
    let max = tokens.iter().map(String::len).max().unwrap_or(0);
    let bytes = s.as_bytes();
    let limit = max.min(bytes.len());
    for keep in (1..=limit).rev() {
        let cut = bytes.len() - keep;
        if !s.is_char_boundary(cut) {
            continue;
        }
        let tail = &s[cut..];
        if tokens.iter().any(|t| t.starts_with(tail)) {
            return keep;
        }
    }
    0
}

/// Parse every `<invoke>` inside one block body.
fn parse_block(block: &str) -> Vec<TextToolCall> {
    let open = open_delim();
    let close = close_delim();
    let invoke_open = format!("{open} invoke");
    let invoke_close = format!("{close} invoke>");
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = block[from..].find(&invoke_open) {
        let start = from + rel;
        let Some(end_rel) = block[start..].find(&invoke_close) else {
            break;
        };
        let end = start + end_rel;
        let body = &block[start + invoke_open.len()..end];
        if let Some(call) = parse_invoke(body) {
            out.push(call);
        }
        from = end + invoke_close.len();
    }
    // A truncated block may still hold a closed invoke whose block closer never
    // arrived; the loop above already covers that case by scanning `block`.
    out
}

/// Parse one `<invoke>` body: its `name` attribute and `<parameter>` children.
fn parse_invoke(body: &str) -> Option<TextToolCall> {
    let name = attr_value(body, "name")?;
    let name = name.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let mut args = Map::new();
    let open = open_delim();
    let close = close_delim();
    let param_open = format!("{open} parameter");
    let param_close = format!("{close} parameter>");
    let mut from = 0;
    while let Some(rel) = body[from..].find(&param_open) {
        let start = from + rel;
        let tag_end = match body[start..].find('>') {
            Some(t) => start + t + 1,
            None => break,
        };
        let header = &body[start..tag_end];
        let Some(pname) = attr_value(header, "name") else {
            from = tag_end;
            continue;
        };
        let as_string = attr_value(header, "string")
            .map(|v| v.trim().eq_ignore_ascii_case("true"))
            .unwrap_or(true);
        let Some(end_rel) = body[tag_end..].find(&param_close) else {
            break;
        };
        let end = tag_end + end_rel;
        let raw = &body[tag_end..end];
        args.insert(pname.trim().to_string(), coerce(raw, as_string));
        from = end + param_close.len();
    }
    Some(TextToolCall {
        name,
        arguments: Value::Object(args),
    })
}

/// Turn a raw parameter body into JSON according to its `string` attribute.
///
/// `string="false"` bodies are already JSON (arrays, objects, numbers,
/// booleans). A body that looks like JSON but fails to parse falls back to a
/// string rather than failing the whole call: the model still meant to pass a
/// value, and losing it would break the invocation entirely.
fn coerce(raw: &str, as_string: bool) -> Value {
    if as_string {
        return Value::String(strip_leading_newline(raw).to_string());
    }
    let trimmed = raw.trim();
    match serde_json::from_str::<Value>(trimmed) {
        Ok(v) => v,
        Err(_) => Value::String(strip_leading_newline(raw).to_string()),
    }
}

/// Drop the single newline the markup places between a tag and its payload.
fn strip_leading_newline(s: &str) -> &str {
    s.strip_prefix("\r\n")
        .or_else(|| s.strip_prefix('\n'))
        .unwrap_or(s)
}

/// Read `key="value"` from a tag header.
fn attr_value(header: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=\"");
    let start = header.find(&needle)? + needle.len();
    let rest = &header[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Build an OpenAI `tool_calls` array entry.
pub fn to_openai_tool_call(index: usize, call: &TextToolCall) -> Value {
    json!({
        "index": index,
        "id": format!("call_dsml_{index}"),
        "type": "function",
        "function": {
            "name": call.name,
            "arguments": call.arguments.to_string(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_all(parts: &[&str]) -> (String, Vec<TextToolCall>) {
        let mut s = TextToolCallScanner::new();
        let mut text = String::new();
        let mut calls = Vec::new();
        for p in parts {
            let (t, c) = s.push(p);
            text.push_str(&t);
            calls.extend(c);
        }
        let (t, c) = s.finish();
        text.push_str(&t);
        calls.extend(c);
        (text, calls)
    }

    #[test]
    fn parses_single_string_parameter() {
        let raw = "\n\n<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} calls>\n<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} invoke name=\"read\">\n<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} parameter name=\"path\" string=\"true\">/etc/hostname</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} parameter>\n</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} invoke>\n</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} calls>";
        let (text, calls) = scan_all(&[raw]);
        assert_eq!(text, "\n\n");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[0].arguments, json!({"path": "/etc/hostname"}));
    }

    #[test]
    fn parses_json_typed_parameters() {
        let raw = "<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} calls>\n<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} invoke name=\"read\">\n<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} parameter name=\"limit\" string=\"false\">5</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} parameter>\n<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} parameter name=\"todos\" string=\"false\">[{\"id\": \"a\"}]</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} parameter>\n</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} invoke>\n</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} calls>";
        let (_, calls) = scan_all(&[raw]);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments["limit"], json!(5));
        assert_eq!(calls[0].arguments["todos"], json!([{"id": "a"}]));
    }

    #[test]
    fn parses_multiple_invocations() {
        let raw = "<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} calls>\n<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} invoke name=\"read\">\n<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} parameter name=\"path\" string=\"true\">/a</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} parameter>\n</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} invoke>\n<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} invoke name=\"bash\">\n<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} parameter name=\"command\" string=\"true\">echo hi</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} parameter>\n</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} invoke>\n</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} calls>";
        let (_, calls) = scan_all(&[raw]);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[1].name, "bash");
        assert_eq!(calls[1].arguments["command"], json!("echo hi"));
    }

    #[test]
    fn survives_arbitrary_delta_splits() {
        let raw = "lead<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} calls>\n<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} invoke name=\"bash\">\n<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} parameter name=\"command\" string=\"true\">echo hi</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} parameter>\n</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} invoke>\n</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} calls>tail";
        // Split at every single byte offset: the scanner must be boundary safe.
        for cut in 0..=raw.len() {
            if !raw.is_char_boundary(cut) {
                continue;
            }
            let (a, b) = raw.split_at(cut);
            let (text, calls) = scan_all(&[a, b]);
            assert_eq!(calls.len(), 1, "cut at {cut} lost the call");
            assert_eq!(calls[0].name, "bash", "cut at {cut}");
            assert_eq!(calls[0].arguments["command"], json!("echo hi"), "cut at {cut}");
            assert_eq!(text, "leadtail", "cut at {cut} text");
        }
    }

    #[test]
    fn survives_byte_at_a_time_streaming() {
        let raw = "<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} calls>\n<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} invoke name=\"read\">\n<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} parameter name=\"path\" string=\"true\">/x</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} parameter>\n</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} invoke>\n</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} calls>";
        let mut s = TextToolCallScanner::new();
        let mut calls = Vec::new();
        for ch in raw.chars() {
            let (_, c) = s.push(&ch.to_string());
            calls.extend(c);
        }
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments["path"], json!("/x"));
    }

    #[test]
    fn preserves_special_characters_in_string_body() {
        let raw = "<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} calls>\n<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} invoke name=\"bash\">\n<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} parameter name=\"command\" string=\"true\">printf 'a\"b<c>d&e'</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} parameter>\n</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} invoke>\n</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} calls>";
        let (_, calls) = scan_all(&[raw]);
        assert_eq!(calls[0].arguments["command"], json!("printf 'a\"b<c>d&e'"));
    }

    #[test]
    fn plain_text_without_markup_passes_through() {
        let (text, calls) = scan_all(&["Hello! ", "How can I help?"]);
        assert_eq!(text, "Hello! How can I help?");
        assert!(calls.is_empty());
    }

    #[test]
    fn lone_angle_brackets_are_not_held_forever() {
        // `<` and `</｜｜` are marker prefixes, but ordinary `<b>` must not stall.
        let (text, calls) = scan_all(&["a < b and c > d"]);
        assert!(calls.is_empty());
        assert_eq!(text, "a < b and c > d");
    }

    #[test]
    fn truncated_block_still_yields_complete_invoke() {
        let raw = "<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} calls>\n<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} invoke name=\"glob\">\n<\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} parameter name=\"pattern\" string=\"true\">*.rs</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} parameter>\n</\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C} invoke>";
        let (_, calls) = scan_all(&[raw]);
        assert_eq!(calls.len(), 1, "a closed invoke must survive a missing block closer");
        assert_eq!(calls[0].name, "glob");
        assert_eq!(calls[0].arguments["pattern"], json!("*.rs"));
    }

    #[test]
    fn builds_openai_shaped_entries() {
        let call = TextToolCall {
            name: "read".into(),
            arguments: json!({"path": "/x"}),
        };
        let v = to_openai_tool_call(0, &call);
        assert_eq!(v["index"], 0);
        assert_eq!(v["type"], "function");
        assert_eq!(v["function"]["name"], "read");
        assert_eq!(v["function"]["arguments"], "{\"path\":\"/x\"}");
    }
}
