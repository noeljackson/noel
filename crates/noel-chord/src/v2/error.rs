//! Chord v2 errors (upstream `src/data/errors.ts` and `src/link/errors.ts`):
//! the closed table of codes and reasons, `ChordError` and its envelope,
//! `PluginError`, and both in the wire form `{kind: "chord", error}` or
//! `{kind: "plugin", code, message, details?}`.

use std::fmt;

use serde_json::{json, Map, Value};

/// The version of the error table on links, which is also the link
/// protocol number `hello` carries.
pub const PROTOCOL_VERSION: u64 = 1;

/// Every code. `invalid` requires a reason; every other code may omit it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Code {
    Invalid,
    ContractMismatch,
    Forbidden,
    Gone,
    NotReady,
    Released,
    TooLarge,
    Unavailable,
    Aborted,
}

impl Code {
    pub const ALL: [Code; 9] = [
        Code::Invalid,
        Code::ContractMismatch,
        Code::Forbidden,
        Code::Gone,
        Code::NotReady,
        Code::Released,
        Code::TooLarge,
        Code::Unavailable,
        Code::Aborted,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Code::Invalid => "invalid",
            Code::ContractMismatch => "contract_mismatch",
            Code::Forbidden => "forbidden",
            Code::Gone => "gone",
            Code::NotReady => "not_ready",
            Code::Released => "released",
            Code::TooLarge => "too_large",
            Code::Unavailable => "unavailable",
            Code::Aborted => "aborted",
        }
    }

    pub fn parse(text: &str) -> Option<Code> {
        Code::ALL.into_iter().find(|code| code.as_str() == text)
    }

    /// The reasons the code may carry.
    pub fn reasons(self) -> &'static [&'static str] {
        match self {
            Code::Invalid => &[
                "args",
                "params",
                "member",
                "host_address",
                "reentrant",
                "read_after_bulk",
                "cycle",
            ],
            Code::ContractMismatch => &["updating"],
            Code::Forbidden => &["revoked"],
            Code::Gone | Code::NotReady | Code::Aborted => &[],
            Code::Released => &["ended", "closed", "settled", "version", "discarded"],
            Code::TooLarge => &["part"],
            Code::Unavailable => &[
                "locked",
                "park_timeout",
                "busy",
                "storage_failed",
                "undeclared",
                // A dependency refused for its version, and old work waiting
                // on its own drain during an upstream reload (a5d209af).
                "version",
                "reload_cycle",
            ],
        }
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A Chord failure: a code, a reason the code allows, a message and JSON
/// details. Only these cross a link.
#[derive(Debug, Clone, PartialEq)]
pub struct ChordError {
    pub code: Code,
    pub reason: Option<&'static str>,
    pub message: String,
    pub details: Option<Value>,
}

impl ChordError {
    /// An error without a reason (not for `invalid`).
    pub fn new(code: Code, message: impl Into<String>) -> ChordError {
        debug_assert!(code != Code::Invalid, "invalid requires a reason");
        ChordError {
            code,
            reason: None,
            message: message.into(),
            details: None,
        }
    }

    /// An error with `reason`, which must be one `code` allows.
    pub fn with_reason(code: Code, reason: &str, message: impl Into<String>) -> ChordError {
        let reason = code
            .reasons()
            .iter()
            .copied()
            .find(|allowed| *allowed == reason);
        debug_assert!(reason.is_some(), "a reason {code} does not allow");
        ChordError {
            code,
            reason,
            message: message.into(),
            details: None,
        }
    }

    /// An `invalid` error with `reason`.
    pub fn invalid(reason: &str, message: impl Into<String>) -> ChordError {
        ChordError::with_reason(Code::Invalid, reason, message)
    }

    pub fn details(mut self, details: Value) -> ChordError {
        self.details = Some(details);
        self
    }

    /// The envelope: `{code, reason?, message, details?}`.
    pub fn to_json(&self) -> Value {
        let mut json = Map::new();
        json.insert("code".to_owned(), json!(self.code.as_str()));
        if let Some(reason) = self.reason {
            json.insert("reason".to_owned(), json!(reason));
        }
        json.insert("message".to_owned(), json!(self.message));
        if let Some(details) = &self.details {
            json.insert("details".to_owned(), details.clone());
        }
        Value::Object(json)
    }

    /// Rebuilds an error from a link, checking the envelope only: a known
    /// code, a reason it allows (required for `invalid`), a string message,
    /// no other keys.
    pub fn from_json(raw: &Value) -> Result<ChordError, String> {
        let problem = |problem: &str| format!("Invalid ChordError JSON: {problem}");
        let Value::Object(json) = raw else {
            return Err(problem("not an object"));
        };
        for key in json.keys() {
            if !matches!(key.as_str(), "code" | "reason" | "message" | "details") {
                return Err(problem(&format!("unknown key \"{key}\"")));
            }
        }
        let code = json
            .get("code")
            .and_then(Value::as_str)
            .and_then(Code::parse)
            .ok_or_else(|| problem("unknown code"))?;
        let reason = match json.get("reason") {
            None => {
                if code == Code::Invalid {
                    return Err(problem(&format!("reason not allowed for {code}")));
                }
                None
            }
            Some(reason) => Some(
                reason
                    .as_str()
                    .and_then(|reason| code.reasons().iter().copied().find(|r| *r == reason))
                    .ok_or_else(|| problem(&format!("reason not allowed for {code}")))?,
            ),
        };
        let message = json
            .get("message")
            .and_then(Value::as_str)
            .ok_or_else(|| problem("message is not a string"))?;
        Ok(ChordError {
            code,
            reason,
            message: message.to_owned(),
            details: json.get("details").cloned(),
        })
    }
}

impl fmt::Display for ChordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ChordError {}

/// An error a plugin gives its callers: its own code, a message and JSON
/// details.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginError {
    pub code: String,
    pub message: String,
    pub details: Option<Value>,
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PluginError {}

/// What crosses a link for a failure.
#[derive(Debug, Clone, PartialEq)]
pub enum WireError {
    Chord(ChordError),
    Plugin(PluginError),
}

impl WireError {
    /// Anything else a provider fails with: the plugin code `internal`.
    pub fn internal(message: impl Into<String>) -> WireError {
        WireError::Plugin(PluginError {
            code: "internal".to_owned(),
            message: message.into(),
            details: None,
        })
    }

    pub fn to_json(&self) -> Value {
        match self {
            WireError::Chord(error) => json!({"kind": "chord", "error": error.to_json()}),
            WireError::Plugin(error) => {
                let mut json = json!({
                    "kind": "plugin",
                    "code": error.code,
                    "message": error.message,
                });
                if let Some(details) = &error.details {
                    json["details"] = details.clone();
                }
                json
            }
        }
    }

    /// The error a consumer rethrows. A wire error of the wrong shape or a
    /// malformed ChordError envelope is an error (a protocol error).
    pub fn from_json(value: &Value) -> Result<WireError, String> {
        check_wire_error(value)?;
        if value["kind"] == "chord" {
            return ChordError::from_json(&value["error"]).map(WireError::Chord);
        }
        Ok(WireError::Plugin(PluginError {
            code: value["code"].as_str().unwrap_or_default().to_owned(),
            message: value["message"].as_str().unwrap_or_default().to_owned(),
            details: value.get("details").cloned(),
        }))
    }
}

/// `code/reason: message` for a Chord error, `code: message` for a
/// plugin's, as noel's front ends print them.
impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::Chord(error) => match error.reason {
                Some(reason) => write!(f, "{}/{}: {}", error.code, reason, error.message),
                None => write!(f, "{}: {}", error.code, error.message),
            },
            WireError::Plugin(error) => write!(f, "{}: {}", error.code, error.message),
        }
    }
}

impl std::error::Error for WireError {}

impl WireError {
    /// A plugin error with `code` and `message`.
    pub fn plugin(code: impl Into<String>, message: impl Into<String>) -> WireError {
        WireError::Plugin(PluginError {
            code: code.into(),
            message: message.into(),
            details: None,
        })
    }

    /// Whether it is a Chord error with `code`.
    pub fn is(&self, code: Code) -> bool {
        matches!(self, WireError::Chord(error) if error.code == code)
    }

    /// Whether it is a plugin error with `code`.
    pub fn is_plugin(&self, code: &str) -> bool {
        matches!(self, WireError::Plugin(error) if error.code == code)
    }
}

impl From<ChordError> for WireError {
    fn from(error: ChordError) -> WireError {
        WireError::Chord(error)
    }
}

impl From<PluginError> for WireError {
    fn from(error: PluginError) -> WireError {
        WireError::Plugin(error)
    }
}

/// Whether `value` has a wire error's shape (upstream's `wireError`
/// schema): `{kind: "chord", error}` or `{kind: "plugin", code, message,
/// details?}`, closed. A chord error's envelope is checked where it is
/// opened.
pub(crate) fn check_wire_error(value: &Value) -> Result<(), String> {
    let malformed = || Err("message: a malformed error".to_owned());
    let Value::Object(fields) = value else {
        return malformed();
    };
    match fields.get("kind").and_then(Value::as_str) {
        Some("chord") if fields.len() == 2 && fields.contains_key("error") => Ok(()),
        Some("plugin")
            if fields.get("code").is_some_and(Value::is_string)
                && fields.get("message").is_some_and(Value::is_string)
                && fields
                    .keys()
                    .all(|key| matches!(key.as_str(), "kind" | "code" | "message" | "details")) =>
        {
            Ok(())
        }
        _ => malformed(),
    }
}
