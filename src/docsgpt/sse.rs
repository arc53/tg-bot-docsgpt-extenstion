//! Minimal server-sent-events parser (`data:` lines, comments, blank-line dispatch).

#[derive(Default, Debug)]
pub struct SseParser {
    buf: String,
    data: Vec<String>,
}

impl SseParser {
    /// Feed raw bytes; returns complete `data` payloads in order.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.push_str(&String::from_utf8_lossy(chunk));
        let mut out = Vec::new();
        while let Some(pos) = self.buf.find('\n') {
            let line = self.buf[..pos].trim_end_matches('\r').to_string();
            self.buf.drain(..=pos);
            if line.is_empty() {
                if !self.data.is_empty() {
                    out.push(self.data.join("\n"));
                    self.data.clear();
                }
                continue;
            }
            if line.starts_with(':') {
                continue; // comment / keepalive
            }
            let (field, value) = match line.find(':') {
                Some(i) => (
                    &line[..i],
                    line[i + 1..].strip_prefix(' ').unwrap_or(&line[i + 1..]),
                ),
                None => (line.as_str(), ""),
            };
            if field == "data" {
                self.data.push(value.to_string());
            }
            // `id`, `event`, `retry` are ignored.
        }
        out
    }

    /// Flush a trailing event without a final blank line.
    pub fn finish(&mut self) -> Option<String> {
        if !self.buf.is_empty() {
            let rest = std::mem::take(&mut self.buf);
            let _ = self.push(format!("{rest}\n").as_bytes());
        }
        if self.data.is_empty() {
            None
        } else {
            let d = self.data.join("\n");
            self.data.clear();
            Some(d)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events_across_chunks() {
        let mut p = SseParser::default();
        let mut got = p.push(b"id: 0\ndata: {\"a\":1}\n\n: keepalive\nid: 1\ndata: {\"b\"");
        assert_eq!(got, vec!["{\"a\":1}"]);
        got = p.push(b":2}\n\ndata: tail");
        assert_eq!(got, vec!["{\"b\":2}"]);
        assert_eq!(p.finish().as_deref(), Some("tail"));
    }

    #[test]
    fn joins_multiline_data() {
        let mut p = SseParser::default();
        let got = p.push(b"data: a\r\ndata: b\r\n\r\n");
        assert_eq!(got, vec!["a\nb"]);
    }
}
