//! Streamed command output with bounded memory (upstream
//! `output-accumulator.ts`): the end of the output is kept for the model, and
//! once the output exceeds the limits all of it is saved to a temp file.

use std::collections::hash_map::RandomState;
use std::fs::{File, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use crate::truncate::{truncate_tail, Truncation, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

/// Creates `<temp dir>/<prefix>-<16 hex digits>.log` for output the model
/// was not shown in full. Output can hold private data, so only the user
/// may read it, and the file never replaces an existing path.
pub(crate) fn create_output_file(prefix: &str) -> io::Result<(PathBuf, File)> {
    loop {
        // A new `RandomState` carries fresh random keys.
        let random = RandomState::new().build_hasher().finish();
        let path = std::env::temp_dir().join(format!("{prefix}-{random:016x}.log"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

pub(crate) struct OutputSnapshot {
    pub truncation: Truncation,
    /// Bytes of the last line, which may be longer than what was kept.
    pub last_line_bytes: usize,
    pub full_output_path: Option<PathBuf>,
}

pub(crate) struct OutputAccumulator {
    file_prefix: &'static str,
    /// An incomplete UTF-8 sequence at the end of the last chunk.
    undecoded: Vec<u8>,
    decoded_any: bool,
    /// Everything received, until the temp file takes over.
    raw: Vec<u8>,
    tail: String,
    tail_starts_at_line_boundary: bool,
    total_raw_bytes: usize,
    total_decoded_bytes: usize,
    completed_lines: usize,
    current_line_bytes: usize,
    has_open_line: bool,
    file: Option<(PathBuf, File)>,
}

/// The decoded tail is trimmed back to this size once it doubles it.
const MAX_ROLLING_BYTES: usize = DEFAULT_MAX_BYTES * 2;

impl OutputAccumulator {
    pub fn new(file_prefix: &'static str) -> Self {
        Self {
            file_prefix,
            undecoded: Vec::new(),
            decoded_any: false,
            raw: Vec::new(),
            tail: String::new(),
            tail_starts_at_line_boundary: true,
            total_raw_bytes: 0,
            total_decoded_bytes: 0,
            completed_lines: 0,
            current_line_bytes: 0,
            has_open_line: false,
            file: None,
        }
    }

    pub fn append(&mut self, data: &[u8]) -> io::Result<()> {
        self.total_raw_bytes += data.len();
        self.undecoded.extend_from_slice(data);
        let pending = std::mem::take(&mut self.undecoded);
        let mut rest = pending.as_slice();
        // Invalid bytes become U+FFFD; a sequence cut off by the end of the
        // chunk waits for the next one.
        loop {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    self.append_text(text);
                    break;
                }
                Err(error) => {
                    let (valid, after) = rest.split_at(error.valid_up_to());
                    self.append_text(std::str::from_utf8(valid).unwrap_or_default());
                    let Some(invalid) = error.error_len() else {
                        self.undecoded = after.to_vec();
                        break;
                    };
                    self.append_text("\u{fffd}");
                    rest = &after[invalid..];
                }
            }
        }
        if self.file.is_some() || self.exceeds_limits() {
            self.file_mut()?.write_all(data)
        } else {
            self.raw.extend_from_slice(data);
            Ok(())
        }
    }

    /// The end of the output so far, within the limits: what
    /// [`OutputAccumulator::finish`] would show if the output ended here.
    pub fn tail(&self) -> String {
        self.tail_truncation().content
    }

    /// Whether the output so far exceeds the limits, so [`tail`] leaves its
    /// beginning out.
    ///
    /// [`tail`]: OutputAccumulator::tail
    pub fn truncated(&self) -> bool {
        self.completed_lines + usize::from(self.has_open_line) > DEFAULT_MAX_LINES
            || self.total_decoded_bytes > DEFAULT_MAX_BYTES
    }

    fn tail_truncation(&self) -> Truncation {
        let text = match self.tail.find('\n') {
            Some(newline) if !self.tail_starts_at_line_boundary => &self.tail[newline + 1..],
            _ => self.tail.as_str(),
        };
        truncate_tail(text, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES)
    }

    /// Ends the output and returns what the model is shown.
    pub fn finish(mut self) -> io::Result<OutputSnapshot> {
        if !std::mem::take(&mut self.undecoded).is_empty() {
            self.append_text("\u{fffd}");
        }
        let mut truncation = self.tail_truncation();
        let total_lines = self.completed_lines + usize::from(self.has_open_line);
        let truncated =
            total_lines > DEFAULT_MAX_LINES || self.total_decoded_bytes > DEFAULT_MAX_BYTES;
        if truncated && !truncation.truncated {
            truncation.by_lines = self.total_decoded_bytes <= DEFAULT_MAX_BYTES;
        }
        truncation.truncated = truncated;
        truncation.total_lines = total_lines;
        truncation.total_bytes = self.total_decoded_bytes;
        if truncated {
            self.file_mut()?.flush()?;
        }
        Ok(OutputSnapshot {
            truncation,
            last_line_bytes: self.current_line_bytes,
            full_output_path: self.file.map(|(path, _)| path),
        })
    }

    fn append_text(&mut self, mut text: &str) {
        // Like a `TextDecoder`, drop a byte order mark that starts the output.
        if !self.decoded_any && !text.is_empty() {
            self.decoded_any = true;
            text = text.strip_prefix('\u{feff}').unwrap_or(text);
        }
        if text.is_empty() {
            return;
        }
        self.total_decoded_bytes += text.len();
        self.tail.push_str(text);
        if self.tail.len() > MAX_ROLLING_BYTES * 2 {
            let mut start = self.tail.len() - MAX_ROLLING_BYTES;
            while !self.tail.is_char_boundary(start) {
                start += 1;
            }
            self.tail_starts_at_line_boundary = self.tail.as_bytes()[start - 1] == b'\n';
            self.tail.drain(..start);
        }
        match text.rfind('\n') {
            Some(last_newline) => {
                self.completed_lines += text.matches('\n').count();
                self.current_line_bytes = text.len() - last_newline - 1;
                self.has_open_line = self.current_line_bytes > 0;
            }
            None => {
                self.current_line_bytes += text.len();
                self.has_open_line = true;
            }
        }
    }

    fn exceeds_limits(&self) -> bool {
        self.total_raw_bytes > DEFAULT_MAX_BYTES
            || self.total_decoded_bytes > DEFAULT_MAX_BYTES
            || self.completed_lines + usize::from(self.has_open_line) > DEFAULT_MAX_LINES
    }

    /// The temp file, created on first use with everything received so far.
    fn file_mut(&mut self) -> io::Result<&mut File> {
        if self.file.is_none() {
            let (path, mut file) = create_output_file(self.file_prefix)?;
            file.write_all(&std::mem::take(&mut self.raw))?;
            self.file = Some((path, file));
        }
        Ok(&mut self.file.as_mut().expect("output file").1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_output_is_kept_whole_without_a_file() {
        let mut output = OutputAccumulator::new("noel-test-output");
        output.append(b"one\ntwo\n").unwrap();
        let snapshot = output.finish().unwrap();
        assert!(!snapshot.truncation.truncated);
        assert_eq!(snapshot.truncation.content, "one\ntwo\n");
        assert_eq!(snapshot.full_output_path, None);
    }

    #[test]
    fn characters_split_across_chunks_decode_and_invalid_bytes_are_replaced() {
        let mut output = OutputAccumulator::new("noel-test-output");
        let text = "h\u{e9}llo".as_bytes();
        output.append(&text[..2]).unwrap();
        output.append(&text[2..]).unwrap();
        output.append(b" \xff end \xe2\x82").unwrap();
        let snapshot = output.finish().unwrap();
        assert_eq!(
            snapshot.truncation.content,
            "h\u{e9}llo \u{fffd} end \u{fffd}"
        );
    }

    #[test]
    fn long_output_keeps_its_tail_and_saves_everything() {
        let mut output = OutputAccumulator::new("noel-test-output");
        let mut all = Vec::new();
        // Chunks that end mid-line, far more than the rolling tail holds.
        for chunk in 0..400 {
            let text = (0..25)
                .map(|line| format!("chunk {chunk} line {line} {}\n", "x".repeat(40)))
                .collect::<String>();
            let (first, second) = text.as_bytes().split_at(text.len() / 3);
            output.append(first).unwrap();
            output.append(second).unwrap();
            all.extend_from_slice(text.as_bytes());
        }
        let snapshot = output.finish().unwrap();
        let truncation = &snapshot.truncation;
        assert!(truncation.truncated && !truncation.by_lines);
        assert_eq!(truncation.total_lines, 10_000);
        assert_eq!(truncation.total_bytes, all.len());
        assert!(truncation.output_bytes <= DEFAULT_MAX_BYTES);
        // Whole lines only, ending with the last line of the output.
        assert!(truncation.content.starts_with("chunk "));
        assert!(truncation
            .content
            .ends_with(&format!("chunk 399 line 24 {}", "x".repeat(40))));
        let path = snapshot.full_output_path.expect("saved output");
        assert_eq!(std::fs::read(&path).unwrap(), all);
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&path).unwrap().permissions(),
        );
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_file(path).unwrap();
    }
}
