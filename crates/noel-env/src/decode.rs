//! UTF-8 decoding that matches a WHATWG `TextDecoder` (upstream
//! `durable/src/env/decode.ts`): an invalid sequence becomes U+FFFD per
//! maximal subpart, which is Rust's own lossy rule, and input that arrives in
//! pieces decodes as the whole would.

/// A streaming decoder for a byte range that keeps a byte-order mark
/// (upstream `rangeDecoder()`, `TextDecoder("utf-8", {ignoreBOM: true})`).
#[derive(Debug, Default)]
pub struct RangeDecoder {
    /// An incomplete sequence at the end of the last chunk.
    pending: Vec<u8>,
}

impl RangeDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Text for `bytes`. With `stream`, an incomplete character at the end
    /// waits for the next call; without it the input ends here.
    pub fn decode(&mut self, bytes: &[u8], stream: bool) -> String {
        let mut input = std::mem::take(&mut self.pending);
        input.extend_from_slice(bytes);
        let mut text = String::with_capacity(input.len());
        let mut rest = input.as_slice();
        loop {
            match std::str::from_utf8(rest) {
                Ok(valid) => {
                    text.push_str(valid);
                    break;
                }
                Err(error) => {
                    let (valid, after) = rest.split_at(error.valid_up_to());
                    text.push_str(std::str::from_utf8(valid).unwrap_or_default());
                    match error.error_len() {
                        Some(invalid) => {
                            text.push('\u{fffd}');
                            rest = &after[invalid..];
                        }
                        None if stream => {
                            self.pending = after.to_vec();
                            break;
                        }
                        None => {
                            text.push('\u{fffd}');
                            break;
                        }
                    }
                }
            }
        }
        text
    }
}

/// Decodes one stream chunk by chunk exactly like decoding all of it at once
/// with a default `TextDecoder`: only a byte-order mark that starts the
/// stream is dropped (upstream `StreamDecoder`).
#[derive(Debug, Default)]
pub struct StreamDecoder {
    decoder: RangeDecoder,
    started: bool,
}

impl StreamDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Text for `bytes`, holding back an incomplete character.
    pub fn decode(&mut self, bytes: &[u8]) -> String {
        let text = self.decoder.decode(bytes, true);
        self.start(text)
    }

    /// The end of the stream: an incomplete character becomes U+FFFD.
    pub fn finish(&mut self) -> String {
        let text = self.decoder.decode(&[], false);
        self.start(text)
    }

    fn start(&mut self, text: String) -> String {
        if self.started || text.is_empty() {
            return text;
        }
        self.started = true;
        match text.strip_prefix('\u{feff}') {
            Some(rest) => rest.to_string(),
            None => text,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pieces_decode_like_the_whole() {
        let bytes = "\u{feff}h\u{e9}\u{1f600}\u{feff}x".as_bytes();
        for split in 0..bytes.len() {
            let mut decoder = StreamDecoder::new();
            let mut text = decoder.decode(&bytes[..split]);
            text += &decoder.decode(&bytes[split..]);
            text += &decoder.finish();
            assert_eq!(text, "h\u{e9}\u{1f600}\u{feff}x", "split at {split}");
        }
        let mut decoder = RangeDecoder::new();
        assert_eq!(decoder.decode(b"a\xe2\x82", true), "a");
        assert_eq!(decoder.decode(b"\n", true), "\u{fffd}\n");
        assert_eq!(decoder.decode(b"\xff\xc3", false), "\u{fffd}\u{fffd}");
        assert_eq!(
            RangeDecoder::new().decode(b"\xef\xbb\xbfa", false),
            "\u{feff}a"
        );
    }
}
