/// Incremental `text/event-stream` decoder: raw bytes in — chunk boundaries
/// may fall anywhere, including inside multi-byte UTF-8 characters — and
/// events out on line boundaries. Lines are terminated by `\n` (optionally
/// preceded by `\r`), which is what every OpenAI-compatible server emits;
/// the spec's bare-`\r` form is not recognized and would buffer until EOF.
#[derive(Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
    data: Vec<String>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume raw bytes; returns every event completed by a line break.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut events = Vec::new();
        let mut start = 0;
        // Scan each incoming byte once. Only the unfinished line is buffered;
        // neither large partial lines nor many short lines cause rescanning.
        for (pos, byte) in bytes.iter().enumerate() {
            if *byte == b'\n' {
                self.buf.extend_from_slice(&bytes[start..pos]);
                let line = std::mem::take(&mut self.buf);
                if let Some(event) = self.line(&line) {
                    events.push(event);
                }
                self.buf = line;
                self.buf.clear();
                start = pos + 1;
            }
        }
        self.buf.extend_from_slice(&bytes[start..]);
        events
    }

    /// Flush at end of stream: a final line without a newline, plus any
    /// event whose blank-line terminator never arrived.
    pub fn finish(&mut self) -> Vec<String> {
        let mut events = Vec::new();
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            if let Some(ev) = self.line(&line) {
                events.push(ev);
            }
        }
        if !self.data.is_empty() {
            let joined = std::mem::take(&mut self.data).join("\n");
            events.push(joined);
        }
        events
    }

    /// One complete line, its newline already stripped; may still end in CR.
    fn line(&mut self, line: &[u8]) -> Option<String> {
        let line = match line.split_last() {
            Some((&b'\r', rest)) => rest,
            _ => line,
        };
        // A blank line terminates the event; `:`-leading lines are comments
        // (keep-alives); the other fields (`event:`, `id:`, `retry:`) carry
        // no content.
        if line.is_empty() {
            return if self.data.is_empty() {
                None
            } else {
                let joined = std::mem::take(&mut self.data).join("\n");
                Some(joined)
            };
        }
        if line[0] == b':' {
            return None;
        }
        if let Some(value) = line.strip_prefix(b"data:".as_slice()) {
            let value = value.strip_prefix(b" ".as_slice()).unwrap_or(value);
            // Lines are complete here, and \n never appears inside a
            // multi-byte sequence, so lossy decoding never replaces bytes.
            self.data.push(String::from_utf8_lossy(value).into_owned());
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn large_event_in_small_chunks_and_many_events_in_one_chunk() {
        let payload = "a".repeat(8 * 1024 * 1024);
        let wire = format!("data: {payload}\n\ndata: tail\n\n");
        let mut decoder = SseDecoder::new();
        let mut events = Vec::new();
        for chunk in wire.as_bytes().chunks(1024) {
            events.extend(decoder.feed(chunk));
        }
        assert_eq!(events, vec![payload, "tail".to_string()]);
        assert_eq!(
            decoder.feed("data: x\n\n".repeat(10000).as_bytes()),
            vec!["x"; 10000]
        );
        assert!(decoder.finish().is_empty());
    }
    #[test]
    fn reassembles_utf8_at_every_byte_boundary() {
        let data = "data: {\"text\":\"你好\"}\r\n\r\n";
        for split in 0..data.len() {
            let mut decoder = SseDecoder::new();
            let mut frames = decoder.feed(&data.as_bytes()[..split]);
            frames.extend(decoder.feed(&data.as_bytes()[split..]));
            frames.extend(decoder.finish());
            assert_eq!(frames, vec!["{\"text\":\"你好\"}"]);
        }
    }
    #[test]
    fn framing_leaves_protocol_sentinels_to_adapters() {
        let mut decoder = SseDecoder::new();
        assert_eq!(
            decoder.feed(b": ping\nevent: ignored\ndata: first\ndata: second\n\ndata: [DONE]\n\n"),
            vec!["first\nsecond", "[DONE]"]
        );
        assert!(decoder.feed(b"data: last").is_empty());
        assert_eq!(decoder.finish(), vec!["last"]);
    }
}
