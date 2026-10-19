//! Incremental Server-Sent-Events parser.
//!
//! Bytes are accumulated and frames are cut on blank-line boundaries *before* UTF-8
//! decoding, so multi-byte characters fragmented across HTTP chunks are handled (UTF-8
//! continuation bytes never collide with the 0x0A frame delimiter). Field parsing follows
//! the SSE convention: `data:`, `event:`, `id:`, retry:, and `:`-prefixed comments; a
//! missing single space after the colon is tolerated. Multiple `data:` lines in one frame
//! are joined with `\n`.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub id: Option<String>,
    pub data: String,
}

impl SseFrame {
    pub fn is_dispatch(&self) -> bool {
        !self.data.is_empty()
    }
}

#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    /// Last event id seen (for `Last-Event-ID` semantics and duplicate detection by callers).
    pub last_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedError {
    /// A frame contained invalid UTF-8; the frame is skipped and reported.
    InvalidUtf8,
}

impl SseParser {
    pub fn new() -> SseParser {
        SseParser::default()
    }

    /// Feed raw bytes; returns every complete frame. Treats both `\n\n` and `\r\n\r\n`
    /// (and stray `\r`) as line terminators.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Result<SseFrame, FeedError>> {
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some(end) = find_frame_end(&self.buf) {
            let frame_bytes: Vec<u8> = self.buf.drain(..end.total).collect();
            let raw = &frame_bytes[..end.payload_len];
            frames.push(self.parse_frame(raw));
        }
        frames
    }

    /// Flush any unterminated trailing frame (end of stream). An incomplete frame at EOF
    /// is parsed if non-empty; a protocol-clean stream has nothing here.
    pub fn finish(&mut self) -> Vec<Result<SseFrame, FeedError>> {
        if self.buf.is_empty() {
            return Vec::new();
        }
        let rest = std::mem::take(&mut self.buf);
        vec![self.parse_frame(&rest)]
    }

    fn parse_frame(&mut self, raw: &[u8]) -> Result<SseFrame, FeedError> {
        let text = match std::str::from_utf8(raw) {
            Ok(t) => t,
            Err(_) => {
                return Err(FeedError::InvalidUtf8);
            }
        };
        let mut event = None;
        let mut id = None;
        let mut data_lines: Vec<&str> = Vec::new();
        for line in text.split(['\n', '\r']) {
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("data:") {
                let rest = rest.strip_prefix(' ').unwrap_or(rest);
                data_lines.push(rest);
            } else if let Some(rest) = line.strip_prefix("event:") {
                event = Some(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            } else if let Some(rest) = line.strip_prefix("id:") {
                let v = rest.strip_prefix(' ').unwrap_or(rest).to_string();
                self.last_id = Some(v.clone());
                id = Some(v);
            }
            // `retry:` and unknown fields are ignored per spec.
        }
        Ok(SseFrame {
            event,
            id,
            data: data_lines.join("\n"),
        })
    }
}

struct FrameEnd {
    payload_len: usize,
    total: usize,
}

/// Find the first blank line (\n\n, \r\n\r\n, \r\r, or mixed). Returns the payload length
/// and total bytes to consume.
fn find_frame_end(buf: &[u8]) -> Option<FrameEnd> {
    let mut i = 0;
    while i < buf.len() {
        let b = buf[i];
        if b == b'\n' || b == b'\r' {
            // count consecutive terminators
            let mut j = i;
            let mut newlines = 0;
            while j < buf.len() && (buf[j] == b'\n' || buf[j] == b'\r') {
                newlines += 1;
                j += 1;
            }
            if newlines >= 2 {
                return Some(FrameEnd {
                    payload_len: i,
                    total: j,
                });
            }
            // A single terminator followed by content is a normal line ending; skip it.
            i = j;
        } else {
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(mut p: SseParser, chunks: &[&[u8]]) -> Vec<SseFrame> {
        let mut out = Vec::new();
        for c in chunks {
            for r in p.feed(c) {
                out.push(r.unwrap());
            }
        }
        for r in p.finish() {
            out.push(r.unwrap());
        }
        out
    }

    #[test]
    fn basic_frames_with_event_and_data() {
        let p = SseParser::new();
        let fs = frames(
            p,
            &[b"event: message\ndata: {\"a\":1}\n\nevent: done\ndata: [DONE]\n\n"],
        );
        assert_eq!(fs.len(), 2);
        assert_eq!(fs[0].event.as_deref(), Some("message"));
        assert_eq!(fs[0].data, "{\"a\":1}");
        assert_eq!(fs[1].data, "[DONE]");
    }

    #[test]
    fn frames_split_across_chunks() {
        let p = SseParser::new();
        let fs = frames(p, &[b"data: hel", b"lo\n\ndata: wor", b"ld\n\n"]);
        assert_eq!(fs.len(), 2);
        assert_eq!(fs[0].data, "hello");
        assert_eq!(fs[1].data, "world");
    }

    #[test]
    fn multiline_data_joins_with_newline() {
        let p = SseParser::new();
        let fs = frames(p, &[b"data: line1\ndata: line2\n\n"]);
        assert_eq!(fs[0].data, "line1\nline2");
    }

    #[test]
    fn crlf_framing() {
        let p = SseParser::new();
        let fs = frames(p, &[b"data: a\r\n\r\ndata: b\r\n\r\n"]);
        assert_eq!(fs.len(), 2);
        assert_eq!(fs[0].data, "a");
    }

    #[test]
    fn comments_and_missing_space_tolerated() {
        let p = SseParser::new();
        let fs = frames(p, &[b": keepalive\ndata:no-space\n\n"]);
        assert_eq!(fs[0].data, "no-space");
    }

    #[test]
    fn partial_frame_is_held_back() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: incomplete-no-terminator").is_empty());
        assert!(p.feed(b"more bytes").is_empty());
        let fs = frames(p, &[]);
        assert_eq!(fs.len(), 1, "finish() flushes the trailing frame");
        assert_eq!(fs[0].data, "incomplete-no-terminatormore bytes");
    }

    #[test]
    fn invalid_utf8_in_frame_is_reported_not_panic() {
        let mut p = SseParser::new();
        let rs = p.feed(b"data: \xff\xfe bad\n\n");
        assert!(matches!(rs[0], Err(FeedError::InvalidUtf8)));
    }

    #[test]
    fn multibyte_utf8_split_across_chunks_survives() {
        // "é" = 0xC3 0xA9, split across chunks; frame terminator is a clean \n later.
        let p = SseParser::new();
        let fs = frames(p, &[b"data: \xc3", b"\xa9 ok\n\n"]);
        assert_eq!(fs[0].data, "é ok");
    }

    proptest::proptest! {
        #[test]
        fn no_panic_on_arbitrary_bytes(input in proptest::prelude::prop::collection::vec(0u8..255, 0..2000)) {
            let mut p = SseParser::new();
            let _ = p.feed(&input);
            let _ = p.finish();
        }
    }
}
