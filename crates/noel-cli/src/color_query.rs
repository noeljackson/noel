//! The TUI's terminal color query: write upstream's query batch, collect
//! the replies non-blocking for a bounded time, and hand the reported
//! colors to the system theme. Upstream's TUI does this inside its input
//! pump; here the attach loop drains the collector on its render ticks.

use std::io::{Read, Write};
use std::os::unix::io::FromRawFd;
use std::time::{Duration, Instant};

use noel_tui::{OscColorTarget, Rgb, TerminalColors, TerminalReply};

/// Default colors, palette colors 0-15, and a trailing primary device
/// attributes (DA1) request: every terminal answers DA1 and terminals
/// answer in order, so the DA1 reply marks the end of the color replies,
/// including for terminals that ignore the color queries.
pub(crate) const TERMINAL_COLOR_QUERY: &str = concat!(
    "\x1b]10;?\x07\x1b]11;?\x07",
    "\x1b]4;0;?\x07\x1b]4;1;?\x07\x1b]4;2;?\x07\x1b]4;3;?\x07",
    "\x1b]4;4;?\x07\x1b]4;5;?\x07\x1b]4;6;?\x07\x1b]4;7;?\x07",
    "\x1b]4;8;?\x07\x1b]4;9;?\x07\x1b]4;10;?\x07\x1b]4;11;?\x07",
    "\x1b]4;12;?\x07\x1b]4;13;?\x07\x1b]4;14;?\x07\x1b]4;15;?\x07",
    "\x1b[c",
);

/// How long the collector keeps reading after the query: terminals
/// answer the trailing DA1 request right after the color replies, so
/// this only matters for terminals that answer neither.
const TIMEOUT: Duration = Duration::from_millis(100);

/// A terminal color query in flight: a reply buffer filled without
/// blocking, ending on the DA1 reply, a reply for every target, or the
/// timeout.
pub(crate) struct ColorQuery {
    pending: Vec<u8>,
    started: Instant,
    done: bool,
}

impl ColorQuery {
    /// Write the query batch. None when it cannot be written: a
    /// terminal that cannot be queried reports no colors, and the theme
    /// keeps its defaults.
    pub(crate) fn start() -> Option<Self> {
        let mut out = std::io::stdout();
        if out.write_all(TERMINAL_COLOR_QUERY.as_bytes()).is_err() || out.flush().is_err() {
            return None;
        }
        Some(Self {
            pending: Vec::new(),
            started: Instant::now(),
            done: false,
        })
    }

    /// Drain whatever stdin has now, without blocking, and parse the
    /// complete replies out of the buffer. Returns `Some(colors)` once
    /// the batch ends (the DA1 reply, every target replied, or the
    /// timeout); the colors are None when nothing usable arrived, so
    /// the caller keeps its defaults. Late replies after `done` are
    /// dropped, as the first frame already rendered.
    pub(crate) fn poll(&mut self) -> Option<Option<TerminalColors>> {
        if self.done {
            return None;
        }
        let mut buffer = [0u8; 4096];
        // A dup'd stdin with O_NONBLOCK: reading never blocks the loop,
        // and dropping the dup leaves stdin itself untouched.
        let fd = unsafe { libc::dup(0) };
        if fd < 0 {
            self.done = true;
            return None;
        }
        let previous = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        unsafe { libc::fcntl(fd, libc::F_SETFL, previous | libc::O_NONBLOCK) };
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        loop {
            match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => self.pending.extend_from_slice(&buffer[..read]),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::Interrupted =>
                {
                    break;
                }
                Err(_) => break,
            }
        }
        drop(file);

        let (colors, ended) = self.parse_pending();
        if !ended && self.started.elapsed() < TIMEOUT {
            return None;
        }
        self.done = true;
        Some(colors)
    }

    /// Parse complete replies from the buffer, keeping any tail. The
    /// batch ends on the DA1 reply or once every target replied.
    fn parse_pending(&mut self) -> (Option<TerminalColors>, bool) {
        let mut foreground = None;
        let mut background = None;
        let mut palette: [Option<Rgb>; 16] = [None; 16];
        let mut replied = [false; 18];
        let mut ended = false;
        let mut consumed = 0;
        loop {
            let tail = &self.pending[consumed..];
            let Some(end) = reply_end(tail) else {
                break;
            };
            let chunk = String::from_utf8_lossy(&tail[..end]).into_owned();
            match noel_tui::parse_terminal_reply(&chunk) {
                TerminalReply::DeviceAttributes => {
                    ended = true;
                    consumed += end;
                    break;
                }
                TerminalReply::Color(target, color) => {
                    let seen = match target {
                        OscColorTarget::Foreground => {
                            foreground = color;
                            &mut replied[0]
                        }
                        OscColorTarget::Background => {
                            background = color;
                            &mut replied[1]
                        }
                        OscColorTarget::Palette(index) => match palette.get_mut(index as usize) {
                            Some(slot) => {
                                *slot = color;
                                &mut replied[2 + index as usize]
                            }
                            None => continue,
                        },
                    };
                    *seen = true;
                    consumed += end;
                }
                TerminalReply::Other => consumed += end,
            }
        }
        self.pending.drain(..consumed);
        if ended || replied.iter().all(|seen| *seen) {
            let mut complete = [Rgb::new(0, 0, 0); 16];
            let mut every = true;
            for (slot, color) in palette.iter().enumerate() {
                match color {
                    Some(color) => complete[slot] = *color,
                    None => every = false,
                }
            }
            (
                Some(TerminalColors {
                    foreground,
                    background,
                    palette: every.then_some(complete),
                }),
                true,
            )
        } else {
            (None, false)
        }
    }
}

/// The end offset of the first complete reply in `bytes`: an ESC that
/// starts a known reply shape, up to and including its terminator (BEL,
/// ST) or the DA1 reply's final `c`.
fn reply_end(bytes: &[u8]) -> Option<usize> {
    let esc = bytes.iter().position(|byte| *byte == 0x1b)?;
    let rest = &bytes[esc..];
    let bel = rest.iter().position(|byte| *byte == 0x07);
    let st = rest.windows(2).position(|window| window == [0x1b, b'\\']);
    let osc_end = match (bel, st) {
        (Some(bel), Some(st)) => Some(esc + bel.min(st) + 1),
        (Some(bel), None) => Some(esc + bel + 1),
        (None, Some(st)) => Some(esc + st + 2),
        (None, None) => None,
    };
    if let Some(end) = osc_end {
        // An OSC reply only counts when it starts like one.
        return (rest.starts_with(&[0x1b, b']'])).then_some(end);
    }
    if rest.starts_with(&[0x1b, b'[']) {
        return rest[2..]
            .iter()
            .position(|byte| *byte == b'c')
            .map(|end| esc + end + 3);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reply framing: BEL and ST terminators, DA1, partial tails.
    #[test]
    fn reply_ends_at_the_first_terminator() {
        assert_eq!(reply_end(b"\x1b]10;#ffffff\x07"), Some(13));
        assert_eq!(reply_end(b"\x1b]11;rgb:0/0/0\x1b\\"), Some(16));
        assert_eq!(reply_end(b"\x1b[?62;22c"), Some(9));
        assert_eq!(reply_end(b"\x1b]10;#ff"), None);
        assert_eq!(reply_end(b""), None);
        // A stray BEL before the escape belongs to no reply.
        assert_eq!(reply_end(b"\x07\x1b]10;#ffffff\x07"), Some(14));
    }
}
