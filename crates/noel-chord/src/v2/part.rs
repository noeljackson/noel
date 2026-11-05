//! Parts on links (upstream `src/link/part.ts`): limits per part,
//! compression at the origin and opening a part where it is owned, its size
//! checked before anything is parsed or inflated and a deflated part
//! inflated within the decoded limit.
//!
//! Rust has a synchronous compressor (flate2), so a deflated part is always
//! inflated when its owner opens it; upstream's asynchronous inflate ahead
//! of the owner, for hosts without one, has no counterpart.

use std::borrow::Cow;
use std::io::{Read, Write};

use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use flate2::Compression;

use super::error::{ChordError, Code};
use super::message::Part;

const MIB: u64 = 1024 * 1024;

/// JSON parts above this many UTF-8 bytes are compressed at their origin,
/// when it compresses (a protocol constant).
pub const COMPRESS_ABOVE: usize = 16 * 1024;

/// A link's limits per part as a link is made with them, in bytes; `None`
/// takes the default of the link's trust.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PartLimits {
    /// A part's size on the wire.
    pub part: Option<u64>,
    /// A part's size after inflating it.
    pub decoded: Option<u64>,
}

/// A link's limits in force; `u64::MAX` for none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub part: u64,
    pub decoded: u64,
}

impl Limits {
    /// The limits of untrusted links unless set: 16 MiB on the wire, 64 MiB
    /// decoded.
    pub const UNTRUSTED: Limits = Limits {
        part: 16 * MIB,
        decoded: 64 * MIB,
    };

    /// No limits (trusted links).
    pub const NONE: Limits = Limits {
        part: u64::MAX,
        decoded: u64::MAX,
    };

    /// The limits of a link: the ones set, the defaults of its trust for
    /// the others.
    pub fn of_link(trusted: bool, limits: PartLimits) -> Limits {
        let defaults = if trusted {
            Limits::NONE
        } else {
            Limits::UNTRUSTED
        };
        Limits {
            part: limits.part.unwrap_or(defaults.part),
            decoded: limits.decoded.unwrap_or(defaults.decoded),
        }
    }
}

/// A JSON part at its origin: deflated (raw deflate, level 1) when
/// `compress` is set, its UTF-8 bytes are above [`COMPRESS_ABOVE`] and
/// deflating made it smaller; otherwise the text.
pub fn origin_part(json: String, compress: bool) -> Part {
    if !compress || json.len() <= COMPRESS_ABOVE {
        return Part::Text(json);
    }
    let deflated = deflate(json.as_bytes());
    if deflated.len() < json.len() {
        Part::Deflated(deflated)
    } else {
        Part::Text(json)
    }
}

/// Raw deflate at level 1.
pub fn deflate(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(1));
    encoder
        .write_all(bytes)
        .and_then(|()| encoder.finish())
        .expect("deflate into memory")
}

/// What inflating a deflated part gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inflated {
    Bytes(Vec<u8>),
    /// More than the limit.
    TooLarge,
    /// No raw deflate stream.
    Corrupt(String),
}

/// Inflates raw deflate `bytes`, giving at most `limit` bytes.
pub fn inflate(bytes: &[u8], limit: u64) -> Inflated {
    let mut output = Vec::new();
    let mut decoder = DeflateDecoder::new(bytes).take(limit.saturating_add(1));
    match decoder.read_to_end(&mut output) {
        Err(error) => Inflated::Corrupt(error.to_string()),
        Ok(_) if output.len() as u64 > limit => Inflated::TooLarge,
        Ok(_) => Inflated::Bytes(output),
    }
}

/// The `too_large/part` error of a part whose size on the wire is above
/// `limit`; `None` within it. Nothing is parsed or inflated.
pub fn oversized(part: &Part, limit: u64) -> Option<ChordError> {
    let size = part.wire_bytes().0.len() as u64;
    (size > limit).then(|| too_large(Some(size), limit, "wire"))
}

/// Why a part could not be opened.
#[derive(Debug, Clone, PartialEq)]
pub enum OpenError {
    /// Past a limit: `too_large/part`.
    TooLarge(ChordError),
    /// A deflated part that is no raw deflate stream (a protocol error).
    Corrupt(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::TooLarge(error) => write!(f, "{error}"),
            OpenError::Corrupt(error) => write!(f, "part: no raw deflate stream ({error})"),
        }
    }
}

impl std::error::Error for OpenError {}

/// The JSON bytes of a part its owner is about to parse: its size checked
/// against `limits.part` first, then its decoded size against
/// `limits.decoded`, a deflated part inflated within it.
pub fn open_part(part: &Part, limits: Limits) -> Result<Cow<'_, [u8]>, OpenError> {
    if let Some(error) = oversized(part, limits.part) {
        return Err(OpenError::TooLarge(error));
    }
    match part {
        Part::Text(_) | Part::Bytes(_) => {
            let bytes = part.wire_bytes().0;
            let size = bytes.len() as u64;
            if size > limits.decoded {
                return Err(OpenError::TooLarge(too_large(
                    Some(size),
                    limits.decoded,
                    "decoded",
                )));
            }
            Ok(Cow::Borrowed(bytes))
        }
        Part::Deflated(bytes) => match inflate(bytes, limits.decoded) {
            Inflated::Bytes(bytes) => Ok(Cow::Owned(bytes)),
            Inflated::TooLarge => Err(OpenError::TooLarge(too_large(
                None,
                limits.decoded,
                "decoded",
            ))),
            Inflated::Corrupt(error) => Err(OpenError::Corrupt(error)),
        },
    }
}

/// The error of a part past a limit (`size` unknown while inflating).
fn too_large(size: Option<u64>, limit: u64, measure: &str) -> ChordError {
    let what = match size {
        Some(size) => format!("a part of {size} bytes"),
        None => "a part".to_owned(),
    };
    ChordError::with_reason(
        Code::TooLarge,
        "part",
        format!("{what} is over the {measure} limit of {limit} bytes"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_parts_and_parts_that_do_not_shrink_stay_text() {
        assert_eq!(
            origin_part("[1]".to_owned(), true),
            Part::Text("[1]".to_owned())
        );
        let long = format!("\"{}\"", "a".repeat(COMPRESS_ABOVE));
        assert_eq!(origin_part(long.clone(), false), Part::Text(long.clone()));
        let Part::Deflated(deflated) = origin_part(long.clone(), true) else {
            panic!("not deflated");
        };
        assert_eq!(
            inflate(&deflated, u64::MAX),
            Inflated::Bytes(long.into_bytes())
        );
    }

    #[test]
    fn opening_checks_the_wire_size_then_the_decoded_size() {
        let text = Part::Text("[1,2,3]".to_owned());
        let limits = Limits {
            part: 7,
            decoded: 7,
        };
        assert_eq!(open_part(&text, limits).unwrap().as_ref(), b"[1,2,3]");
        let error = open_part(&text, Limits { part: 6, ..limits }).unwrap_err();
        assert_eq!(
            error.to_string(),
            "a part of 7 bytes is over the wire limit of 6 bytes"
        );
        let error = open_part(
            &text,
            Limits {
                part: 7,
                decoded: 6,
            },
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "a part of 7 bytes is over the decoded limit of 6 bytes"
        );

        let deflated = Part::Deflated(deflate(&[b'0'; 1000]));
        assert_eq!(open_part(&deflated, Limits::UNTRUSTED).unwrap().len(), 1000);
        let error = open_part(
            &deflated,
            Limits {
                part: 1000,
                decoded: 999,
            },
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "a part is over the decoded limit of 999 bytes"
        );
        assert!(matches!(
            open_part(&Part::Deflated(vec![0xff, 0xff]), Limits::NONE),
            Err(OpenError::Corrupt(_))
        ));
    }

    #[test]
    fn link_limits_default_by_trust() {
        assert_eq!(Limits::of_link(true, PartLimits::default()), Limits::NONE);
        assert_eq!(
            Limits::of_link(
                false,
                PartLimits {
                    part: Some(5),
                    decoded: None
                }
            ),
            Limits {
                part: 5,
                decoded: 64 * MIB
            }
        );
    }
}
