//! Where lines of a file are, from its bytes fed in order (upstream
//! `durable/src/env/line-scan.ts`), so any size scans in bounded memory.

use crate::decode::RangeDecoder;
use crate::{LineRange, LineScan, MAX_SAFE_INTEGER};

/// UTF-8 byte length of decoded text.
fn decoded_bytes(text: &str) -> u64 {
    text.len() as u64
}

/// Computes a [`LineScan`]; decoded sizes are those of decoding the whole
/// file at once with a default `TextDecoder`.
#[derive(Debug)]
pub struct LineScanner {
    start_line: u64,
    end_line: Option<u64>,
    position: u64,
    newlines: u64,
    line_start: u64,
    start: Option<u64>,
    end: Option<u64>,
    first_line_end: Option<u64>,
    last_line_start: Option<u64>,
    selected_bytes: u64,
    first_line_bytes: u64,
    selection: Option<RangeDecoder>,
    first_line: Option<RangeDecoder>,
    /// The first bytes, held until it is known whether they are a byte-order
    /// mark.
    head: Option<Vec<u8>>,
    bom: bool,
}

impl LineScanner {
    /// Lines `[start_line, end_line)`, 0-based; `end_line` absent: to the
    /// end. `None` for an invalid range.
    pub fn new(range: LineRange) -> Option<Self> {
        let LineRange {
            start_line,
            end_line,
        } = range;
        if start_line > MAX_SAFE_INTEGER
            || end_line.is_some_and(|end| end <= start_line || end > MAX_SAFE_INTEGER)
        {
            return None;
        }
        let mut scanner = Self {
            start_line,
            end_line,
            position: 0,
            newlines: 0,
            line_start: 0,
            start: None,
            end: None,
            first_line_end: None,
            last_line_start: None,
            selected_bytes: 0,
            first_line_bytes: 0,
            selection: None,
            first_line: None,
            head: Some(Vec::new()),
            bom: false,
        };
        if start_line == 0 {
            scanner.begin(0);
        }
        Some(scanner)
    }

    /// Whether `line` is the last selected one.
    fn is_last(&self, line: u64) -> bool {
        self.end_line == Some(line + 1)
    }

    pub fn push(&mut self, mut chunk: &[u8]) {
        if let Some(head) = &mut self.head {
            let take = (3 - head.len()).min(chunk.len());
            head.extend_from_slice(&chunk[..take]);
            if head.len() < 3 {
                return;
            }
            self.release_head();
            chunk = &chunk[take..];
        }
        self.process(chunk);
    }

    fn release_head(&mut self) {
        let head = self.head.take().unwrap_or_default();
        self.bom = head.starts_with(b"\xef\xbb\xbf");
        self.process(&head);
    }

    fn process(&mut self, chunk: &[u8]) {
        let base = self.position;
        let mut from = 0;
        for index in chunk
            .iter()
            .enumerate()
            .filter_map(|(index, byte)| (*byte == b'\n').then_some(index))
        {
            // The newline ends line `newlines`. It belongs to the selection
            // between selected lines only.
            self.feed(chunk, base, from, index);
            let line = self.newlines;
            let position = base + index as u64;
            if line == self.start_line {
                self.end_first_line(position);
            }
            if self.is_last(line) {
                self.end_selection(position);
            }
            self.feed(chunk, base, index, index + 1);
            from = index + 1;
            self.newlines += 1;
            self.line_start = position + 1;
            if self.newlines == self.start_line {
                self.begin(self.line_start);
            }
            if self.is_last(self.newlines) {
                self.last_line_start = Some(self.line_start);
            }
        }
        self.feed(chunk, base, from, chunk.len());
        self.position += chunk.len() as u64;
    }

    pub fn finish(mut self) -> LineScan {
        if self.head.is_some() {
            self.release_head();
        }
        let size = self.position;
        let Some(start) = self.start else {
            return LineScan {
                newlines: self.newlines,
                start: size,
                end: size,
                first_line_end: size,
                last_line_start: size,
                selected_bytes: 0,
                first_line_bytes: 0,
            };
        };
        if self.first_line_end.is_none() {
            self.end_first_line(size);
        }
        if self.end.is_none() {
            self.end_selection(size);
        }
        LineScan {
            newlines: self.newlines,
            start,
            end: self.end.unwrap_or(size),
            first_line_end: self.first_line_end.unwrap_or(size),
            // A selection that reaches past the last line ends with it.
            last_line_start: self.last_line_start.unwrap_or(self.line_start),
            selected_bytes: self.selected_bytes,
            first_line_bytes: self.first_line_bytes,
        }
    }

    fn begin(&mut self, start: u64) {
        self.start = Some(start);
        if self.is_last(self.start_line) {
            self.last_line_start = Some(start);
        }
        self.selection = Some(RangeDecoder::new());
        self.first_line = Some(RangeDecoder::new());
    }

    fn end_first_line(&mut self, position: u64) {
        self.first_line_end = Some(position);
        if let Some(mut decoder) = self.first_line.take() {
            self.first_line_bytes += decoded_bytes(&decoder.decode(&[], false));
        }
    }

    fn end_selection(&mut self, position: u64) {
        self.end = Some(position);
        if let Some(mut decoder) = self.selection.take() {
            self.selected_bytes += decoded_bytes(&decoder.decode(&[], false));
        }
    }

    /// Feeds `chunk[from..to]`, which starts at file offset `base`, to the
    /// decoders of the ranges still open.
    fn feed(&mut self, chunk: &[u8], base: u64, mut from: usize, to: usize) {
        // Decoding the whole file drops a leading byte-order mark.
        if self.bom && base + (from as u64) < 3 {
            from = to.min((3 - base) as usize);
        }
        if to <= from {
            return;
        }
        let bytes = &chunk[from..to];
        if let Some(decoder) = &mut self.selection {
            self.selected_bytes += decoded_bytes(&decoder.decode(bytes, true));
        }
        if let Some(decoder) = &mut self.first_line {
            self.first_line_bytes += decoded_bytes(&decoder.decode(bytes, true));
        }
    }
}
