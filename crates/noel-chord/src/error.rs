//! Chord's errors, as upstream's `chord/src/errors.ts`, and their wire form
//! (`wire.ts`'s `toWireError` and `fromWireError`).

use std::fmt;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// The codes callers see. A finer cause travels as the error's `reason`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Aborted,
    ContractMismatch,
    Forbidden,
    Gone,
    Invalid,
    NotInVariant,
    NotReady,
    Released,
    Superseded,
    Unavailable,
    UpgradeConflict,
    UpgradeRequired,
}

impl ErrorCode {
    pub const ALL: [ErrorCode; 12] = [
        ErrorCode::Aborted,
        ErrorCode::ContractMismatch,
        ErrorCode::Forbidden,
        ErrorCode::Gone,
        ErrorCode::Invalid,
        ErrorCode::NotInVariant,
        ErrorCode::NotReady,
        ErrorCode::Released,
        ErrorCode::Superseded,
        ErrorCode::Unavailable,
        ErrorCode::UpgradeConflict,
        ErrorCode::UpgradeRequired,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Aborted => "aborted",
            ErrorCode::ContractMismatch => "contract_mismatch",
            ErrorCode::Forbidden => "forbidden",
            ErrorCode::Gone => "gone",
            ErrorCode::Invalid => "invalid",
            ErrorCode::NotInVariant => "not_in_variant",
            ErrorCode::NotReady => "not_ready",
            ErrorCode::Released => "released",
            ErrorCode::Superseded => "superseded",
            ErrorCode::Unavailable => "unavailable",
            ErrorCode::UpgradeConflict => "upgrade_conflict",
            ErrorCode::UpgradeRequired => "upgrade_required",
        }
    }

    pub fn parse(text: &str) -> Option<ErrorCode> {
        ErrorCode::ALL
            .into_iter()
            .find(|code| code.as_str() == text)
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An error with a code, a message without its `code/reason: ` prefix, and
/// an optional reason. It displays as upstream's `ChordError.message` does:
/// `code/reason: message`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChordError {
    pub code: ErrorCode,
    pub message: String,
    pub reason: Option<String>,
}

impl ChordError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        ChordError {
            code,
            message: message.into(),
            reason: None,
        }
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    pub fn aborted(message: impl Into<String>) -> Self {
        ChordError::new(ErrorCode::Aborted, message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        ChordError::new(ErrorCode::Forbidden, message)
    }

    pub fn gone(message: impl Into<String>) -> Self {
        ChordError::new(ErrorCode::Gone, message)
    }

    pub fn invalid(reason: impl Into<String>, message: impl Into<String>) -> Self {
        ChordError::new(ErrorCode::Invalid, message).with_reason(reason)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        ChordError::new(ErrorCode::Unavailable, message)
    }

    /// Whether retrying (with a new root) can succeed.
    pub fn retryable(&self) -> bool {
        matches!(
            self.code,
            ErrorCode::Superseded | ErrorCode::UpgradeConflict
        )
    }

    /// The wire form: the message carries the `code/reason: ` prefix.
    pub fn to_wire(&self) -> WireError {
        WireError {
            code: self.code,
            message: self.to_string(),
            reason: self.reason.clone(),
        }
    }

    /// Back from the wire: the prefix is stripped, as upstream does.
    pub fn from_wire(error: &WireError) -> Self {
        static PREFIX: OnceLock<Regex> = OnceLock::new();
        let prefix =
            PREFIX.get_or_init(|| Regex::new(r"^[a-z_]+(/[a-z_]+)?: ").expect("a valid pattern"));
        ChordError {
            code: error.code,
            message: prefix.replace(&error.message, "").into_owned(),
            reason: error.reason.clone(),
        }
    }
}

impl fmt::Display for ChordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.reason {
            Some(reason) if !reason.is_empty() => {
                write!(f, "{}/{}: {}", self.code, reason, self.message)
            }
            _ => write!(f, "{}: {}", self.code, self.message),
        }
    }
}

impl std::error::Error for ChordError {}

/// `{code, message, reason?}` as it travels.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_message_carries_the_prefix_and_loses_it_on_the_way_back() {
        let error = ChordError::invalid("args", "x.add args: / must be integer");
        let wire = error.to_wire();
        assert_eq!(wire.message, "invalid/args: x.add args: / must be integer");
        assert_eq!(ChordError::from_wire(&wire), error);
        let plain = ChordError::gone("principal invalidated");
        assert_eq!(plain.to_wire().message, "gone: principal invalidated");
        assert_eq!(
            serde_json::to_value(plain.to_wire()).unwrap(),
            serde_json::json!({"code": "gone", "message": "gone: principal invalidated"})
        );
    }
}
