//! Incremental SSE data decoder. Line endings may be LF, CRLF or CR, including
//! mixed endings and CRLF split between HTTP chunks. The caller bounds wire bytes.
#[derive(Default)]
pub(super) struct Decoder {
    line: Vec<u8>,
    data: Vec<u8>,
    skip_lf: bool,
}

impl Decoder {
    pub(super) fn feed(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut events = Vec::new();
        for &byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if matches!(byte, b'\r' | b'\n') {
                self.skip_lf = byte == b'\r';
                if self.line.is_empty() {
                    if !self.data.is_empty() {
                        self.data.pop(); // Remove the last SSE data newline.
                        if !self.data.is_empty() {
                            events.push(std::mem::take(&mut self.data));
                        }
                    }
                } else if self.line == b"data" || self.line.starts_with(b"data:") {
                    let value = self.line.get(5..).unwrap_or_default();
                    self.data
                        .extend_from_slice(value.strip_prefix(b" ").unwrap_or(value));
                    self.data.push(b'\n');
                }
                self.line.clear();
            } else {
                self.line.push(byte);
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::Decoder;

    #[test]
    fn mixed_line_endings_and_multiline_utf8_are_independent_of_chunk_boundaries() {
        let wire = "event: message\r\ndata: {\"note\":\"你好\"}\r\n\r\ndata: {\"a\":\n data: ignored\ndata: 1}\n\ndata: last\r\r".as_bytes();
        let expected = vec![
            "{\"note\":\"你好\"}".as_bytes().to_vec(),
            b"{\"a\":\n1}".to_vec(),
            b"last".to_vec(),
        ];
        for width in 1..=wire.len() {
            let mut decoder = Decoder::default();
            let events: Vec<_> = wire
                .chunks(width)
                .flat_map(|chunk| decoder.feed(chunk))
                .collect();
            assert_eq!(events, expected, "chunk width {width}");
        }
    }
}
