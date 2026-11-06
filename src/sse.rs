/// Minimal incremental SSE parser: feeds arbitrary byte chunks, yields complete
/// event `data` payloads.
#[derive(Default)]
pub struct SseBuffer {
    buf: Vec<u8>,
}

impl SseBuffer {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((end, sep_len)) = find_boundary(&self.buf) {
            let block = self.buf[..end].to_vec();
            self.buf.drain(..end + sep_len);
            let text = String::from_utf8_lossy(&block);
            let mut data_lines = Vec::new();
            for line in text.lines() {
                let line = line.trim_end_matches('\r');
                if let Some(rest) = line.strip_prefix("data:") {
                    data_lines.push(rest.trim_start().to_string());
                }
            }
            if !data_lines.is_empty() {
                out.push(data_lines.join("\n"));
            }
        }
        out
    }
}

fn find_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some((i, 2));
        }
        if i + 3 < buf.len()
            && buf[i] == b'\r'
            && buf[i + 1] == b'\n'
            && buf[i + 2] == b'\r'
            && buf[i + 3] == b'\n'
        {
            return Some((i, 4));
        }
        i += 1;
    }
    None
}

pub fn sse_event(event: &str, data: &str) -> String {
    if event.is_empty() {
        format!("data: {data}\n\n")
    } else {
        format!("event: {event}\ndata: {data}\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_events_in_one_chunk() {
        let mut parser = SseBuffer::new();
        let events = parser.push(b"data: one\n\ndata: two\n\n");
        assert_eq!(events, vec!["one".to_string(), "two".to_string()]);
    }

    #[test]
    fn buffers_partial_events() {
        let mut parser = SseBuffer::new();
        assert!(parser.push(b"data: hel").is_empty());
        let events = parser.push(b"lo\n\n");
        assert_eq!(events, vec!["hello".to_string()]);
    }

    #[test]
    fn handles_crlf_framing() {
        let mut parser = SseBuffer::new();
        let events = parser.push(b"event: message\r\ndata: {\"ok\":true}\r\n\r\n");
        assert_eq!(events, vec!["{\"ok\":true}".to_string()]);
    }

    #[test]
    fn joins_multiline_data() {
        let mut parser = SseBuffer::new();
        let events = parser.push(b"data: line1\ndata: line2\n\n");
        assert_eq!(events, vec!["line1\nline2".to_string()]);
    }

    #[test]
    fn ignores_comments_and_empty_frames() {
        let mut parser = SseBuffer::new();
        let events = parser.push(b": heartbeat\n\ndata: value\n\n");
        assert_eq!(events, vec!["value".to_string()]);
    }

    #[test]
    fn handles_split_utf8_payload() {
        let payload = "data: 你好\n\n".as_bytes();
        let split = 8; // in the middle of the multi-byte sequence
        let mut parser = SseBuffer::new();
        assert!(parser.push(&payload[..split]).is_empty());
        let events = parser.push(&payload[split..]);
        assert_eq!(events, vec!["你好".to_string()]);
    }

    #[test]
    fn formats_events() {
        assert_eq!(sse_event("", "x"), "data: x\n\n");
        assert_eq!(sse_event("message", "x"), "event: message\ndata: x\n\n");
    }
}
