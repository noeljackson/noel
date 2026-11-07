//! Chord v2 messages (upstream `src/link/message.ts`): the one message type
//! every link carries. Parts (params, state, ops, call arguments and
//! results) are opaque here: JSON text at their origin, its bytes once
//! received, or those bytes deflated by an origin that compresses; only
//! their owner opens them. Errors, addresses' params, principals and
//! revocations are plain JSON.

use serde_json::{json, Map, Value};

/// A value as it crosses a link (upstream `Part`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    /// JSON text at its origin.
    Text(String),
    /// Its UTF-8 bytes, as a binary codec received them.
    Bytes(Vec<u8>),
    /// Its UTF-8 bytes, deflated (raw deflate) by its origin.
    Deflated(Vec<u8>),
}

impl Part {
    /// The part's bytes on the wire and whether they are deflated.
    pub fn wire_bytes(&self) -> (&[u8], bool) {
        match self {
            Part::Text(text) => (text.as_bytes(), false),
            Part::Bytes(bytes) => (bytes, false),
            Part::Deflated(bytes) => (bytes, true),
        }
    }

    /// The same part as its origin wrote it, whatever its form: text and
    /// received bytes of the same JSON compare equal.
    pub fn same_as(&self, other: &Part) -> bool {
        self.wire_bytes() == other.wire_bytes()
    }
}

/// The kinds of a `failed` message's cause, in their codes' order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CauseKind {
    Transient,
    Broken,
    Waiting,
}

impl CauseKind {
    pub const ALL: [CauseKind; 3] = [CauseKind::Transient, CauseKind::Broken, CauseKind::Waiting];

    pub fn as_str(self) -> &'static str {
        match self {
            CauseKind::Transient => "transient",
            CauseKind::Broken => "broken",
            CauseKind::Waiting => "waiting",
        }
    }

    pub fn code(self) -> u8 {
        self as u8
    }
}

/// An address as a hold names it: a host kind and its params.
#[derive(Debug, Clone, PartialEq)]
pub struct Address {
    pub kind: String,
    pub params: Value,
}

/// A member of a hold that failed, by index.
#[derive(Debug, Clone, PartialEq)]
pub struct FailedMember {
    pub member: u64,
    pub error: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundEntry {
    pub hold: u64,
    pub variant: u64,
    pub members: Vec<(u64, Part)>,
    pub failed: Vec<FailedMember>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StateEntry {
    pub hold: u64,
    pub members: Vec<(u64, Part)>,
    pub failed: Vec<FailedMember>,
    pub retired: Vec<u64>,
}

/// A protocol message. `origin` is a relay's number for whose work it is;
/// absent for the sender's own.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Hello {
        protocol: u64,
        incarnation: String,
        calls: u64,
    },
    Hold {
        origin: Option<u64>,
        hold: u64,
        type_name: String,
        version: u64,
        fingerprint: String,
        params: Part,
        address: Address,
        principal: Option<Value>,
        incarnation: Option<String>,
        chain: Option<Vec<String>>,
        limit: Option<u64>,
        /// The caller's trace carrier (upstream slice 9): an opaque record of
        /// at most 8 printable-ASCII strings, forwarded by relays as it came.
        trace: Option<Value>,
    },
    Release {
        origin: Option<u64>,
        hold: u64,
    },
    Call {
        origin: Option<u64>,
        call: u64,
        hold: u64,
        method: u64,
        args: Option<Part>,
        segments: Option<Vec<Vec<u8>>>,
        /// The caller's trace carrier, as on `Hold`.
        trace: Option<Value>,
        /// Old work of a reload upstream (`call.old`): honoured only from a
        /// trusted link. noel's providers never reload, so they ignore it.
        old: bool,
    },
    Abort {
        origin: Option<u64>,
        call: u64,
    },
    Retry {
        origin: Option<u64>,
        hold: u64,
        hops: u64,
    },
    Bound {
        origin: Option<u64>,
        holds: Vec<BoundEntry>,
    },
    State {
        origin: Option<u64>,
        holds: Vec<StateEntry>,
    },
    Ack {
        origin: Option<u64>,
    },
    Closed {
        origin: u64,
    },
    Revoke {
        revocation: Value,
    },
    Ping,
    Pong,
    Failed {
        origin: Option<u64>,
        hold: u64,
        cause: CauseKind,
        error: Value,
        retry_in: Option<u64>,
    },
    Loading {
        origin: Option<u64>,
        hold: u64,
    },
    Result {
        origin: Option<u64>,
        call: u64,
        value: Option<Part>,
        segments: Option<Vec<Vec<u8>>>,
    },
    Error {
        origin: Option<u64>,
        call: u64,
        error: Value,
    },
    Gone {
        origin: Option<u64>,
        hold: u64,
        error: Value,
    },
}

impl Message {
    /// The message's kind, as upstream names it.
    pub fn kind(&self) -> &'static str {
        match self {
            Message::Hello { .. } => "hello",
            Message::Hold { .. } => "hold",
            Message::Release { .. } => "release",
            Message::Call { .. } => "call",
            Message::Abort { .. } => "abort",
            Message::Retry { .. } => "retry",
            Message::Bound { .. } => "bound",
            Message::State { .. } => "state",
            Message::Ack { .. } => "ack",
            Message::Closed { .. } => "closed",
            Message::Revoke { .. } => "revoke",
            Message::Ping => "ping",
            Message::Pong => "pong",
            Message::Failed { .. } => "failed",
            Message::Loading { .. } => "loading",
            Message::Result { .. } => "result",
            Message::Error { .. } => "error",
            Message::Gone { .. } => "gone",
        }
    }

    /// The origin of a message that may carry one.
    pub fn origin(&self) -> Option<u64> {
        match self {
            Message::Hold { origin, .. }
            | Message::Release { origin, .. }
            | Message::Call { origin, .. }
            | Message::Abort { origin, .. }
            | Message::Retry { origin, .. }
            | Message::Bound { origin, .. }
            | Message::State { origin, .. }
            | Message::Ack { origin }
            | Message::Failed { origin, .. }
            | Message::Loading { origin, .. }
            | Message::Result { origin, .. }
            | Message::Error { origin, .. }
            | Message::Gone { origin, .. } => *origin,
            Message::Closed { origin } => Some(*origin),
            Message::Hello { .. } | Message::Revoke { .. } | Message::Ping | Message::Pong => None,
        }
    }

    /// The message with each part received as UTF-8 bytes as text again
    /// (deflated parts stay deflated).
    pub fn with_text_parts(mut self) -> Message {
        let text = |part: &mut Part| {
            if let Part::Bytes(bytes) = part
                && let Ok(text) = String::from_utf8(std::mem::take(bytes))
            {
                *part = Part::Text(text);
            }
        };
        match &mut self {
            Message::Hold { params, .. } => text(params),
            Message::Call {
                args: Some(part), ..
            }
            | Message::Result {
                value: Some(part), ..
            } => text(part),
            Message::Bound { holds, .. } => {
                for entry in holds {
                    entry.members.iter_mut().for_each(|(_, part)| text(part));
                }
            }
            Message::State { holds, .. } => {
                for entry in holds {
                    entry.members.iter_mut().for_each(|(_, part)| text(part));
                }
            }
            _ => {}
        }
        self
    }

    /// The message as JSON, upstream's object with parts as `{text}`,
    /// `{bytes: hex}` or `{deflated: hex}` and segments as `{bytes: hex}`.
    pub fn to_json(&self) -> Value {
        let part = |part: &Part| match part {
            Part::Text(text) => json!(text),
            Part::Bytes(bytes) => json!({ "bytes": hex(bytes) }),
            Part::Deflated(bytes) => json!({ "deflated": hex(bytes) }),
        };
        let segments = |segments: &[Vec<u8>]| {
            Value::Array(
                segments
                    .iter()
                    .map(|bytes| json!({ "bytes": hex(bytes) }))
                    .collect(),
            )
        };
        let failed = |failed: &[FailedMember]| {
            Value::Array(
                failed
                    .iter()
                    .map(|failed| json!({ "member": failed.member, "error": failed.error }))
                    .collect(),
            )
        };
        let mut object = Map::new();
        object.insert("kind".into(), json!(self.kind()));
        let mut put = |key: &str, value: Value| {
            object.insert(key.into(), value);
        };
        if let Some(origin) = self.origin() {
            put("origin", json!(origin));
        }
        match self {
            Message::Hello {
                protocol,
                incarnation,
                calls,
            } => {
                put("protocol", json!(protocol));
                put("incarnation", json!(incarnation));
                put("calls", json!(calls));
            }
            Message::Hold {
                hold,
                type_name,
                version,
                fingerprint,
                params,
                address,
                principal,
                incarnation,
                chain,
                limit,
                trace,
                ..
            } => {
                put("hold", json!(hold));
                put("type", json!(type_name));
                put("version", json!(version));
                put("fingerprint", json!(fingerprint));
                put("params", part(params));
                put("address", json!({ "kind": address.kind, "params": address.params }));
                if let Some(principal) = principal {
                    put("principal", principal.clone());
                }
                if let Some(incarnation) = incarnation {
                    put("incarnation", json!(incarnation));
                }
                if let Some(chain) = chain {
                    put("chain", json!(chain));
                }
                if let Some(limit) = limit {
                    put("limit", json!(limit));
                }
                if let Some(trace) = trace {
                    put("trace", trace.clone());
                }
            }
            Message::Release { hold, .. } | Message::Loading { hold, .. } => put("hold", json!(hold)),
            Message::Retry { hold, hops, .. } => {
                put("hold", json!(hold));
                put("hops", json!(hops));
            }
            Message::Call {
                call,
                hold,
                method,
                args,
                segments: parts,
                trace,
                old,
                ..
            } => {
                put("call", json!(call));
                put("hold", json!(hold));
                put("method", json!(method));
                if let Some(args) = args {
                    put("args", part(args));
                }
                if let Some(parts) = parts {
                    put("segments", segments(parts));
                }
                if let Some(trace) = trace {
                    put("trace", trace.clone());
                }
                if *old {
                    put("old", json!(true));
                }
            }
            Message::Abort { call, .. } => put("call", json!(call)),
            Message::Bound { holds, .. } => put(
                "holds",
                Value::Array(
                    holds
                        .iter()
                        .map(|entry| {
                            json!({
                                "hold": entry.hold,
                                "variant": entry.variant,
                                "members": entry.members.iter().map(|(member, value)| json!({"member": member, "value": part(value)})).collect::<Vec<_>>(),
                                "failed": failed(&entry.failed),
                            })
                        })
                        .collect(),
                ),
            ),
            Message::State { holds, .. } => put(
                "holds",
                Value::Array(
                    holds
                        .iter()
                        .map(|entry| {
                            json!({
                                "hold": entry.hold,
                                "members": entry.members.iter().map(|(member, ops)| json!({"member": member, "ops": part(ops)})).collect::<Vec<_>>(),
                                "failed": failed(&entry.failed),
                                "retired": entry.retired,
                            })
                        })
                        .collect(),
                ),
            ),
            Message::Revoke { revocation } => put("revocation", revocation.clone()),
            Message::Failed {
                hold,
                cause,
                error,
                retry_in,
                ..
            } => {
                put("hold", json!(hold));
                put("cause", json!({ "kind": cause.as_str(), "error": error }));
                if let Some(retry_in) = retry_in {
                    put("retryIn", json!(retry_in));
                }
            }
            Message::Result {
                call,
                value,
                segments: parts,
                ..
            } => {
                put("call", json!(call));
                if let Some(value) = value {
                    put("value", part(value));
                }
                if let Some(parts) = parts {
                    put("segments", segments(parts));
                }
            }
            Message::Error { call, error, .. } => {
                put("call", json!(call));
                put("error", error.clone());
            }
            Message::Gone { hold, error, .. } => {
                put("hold", json!(hold));
                put("error", error.clone());
            }
            Message::Ack { .. } | Message::Closed { .. } | Message::Ping | Message::Pong => {}
        }
        Value::Object(object)
    }

    /// A message from its JSON form (as [`Message::to_json`] writes it, a
    /// part's text as a string).
    pub fn from_json(value: &Value) -> Result<Message, String> {
        let field = |key: &str| value.get(key).ok_or_else(|| format!("message: no `{key}`"));
        let int = |key: &str| -> Result<u64, String> {
            field(key)?
                .as_u64()
                .ok_or_else(|| format!("message: `{key}` is no integer"))
        };
        let text = |key: &str| -> Result<String, String> {
            field(key)?
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("message: `{key}` is no string"))
        };
        let origin = value.get("origin").and_then(Value::as_u64);
        let part = |value: &Value| -> Result<Part, String> {
            match value {
                Value::String(text) => Ok(Part::Text(text.clone())),
                Value::Object(object) => match (object.get("bytes"), object.get("deflated")) {
                    (Some(Value::String(bytes)), None) => Ok(Part::Bytes(unhex(bytes)?)),
                    (None, Some(Value::String(bytes))) => Ok(Part::Deflated(unhex(bytes)?)),
                    _ => Err("message: a malformed part".to_owned()),
                },
                _ => Err("message: a malformed part".to_owned()),
            }
        };
        let optional_part = |key: &str| value.get(key).map(part).transpose();
        let segments = || -> Result<Option<Vec<Vec<u8>>>, String> {
            value
                .get("segments")
                .map(|segments| {
                    segments
                        .as_array()
                        .ok_or("message: `segments` is no list")?
                        .iter()
                        .map(|segment| {
                            unhex(
                                segment["bytes"]
                                    .as_str()
                                    .ok_or("message: a malformed segment")?,
                            )
                        })
                        .collect()
                })
                .transpose()
        };
        let list = |key: &str| -> Result<Vec<Value>, String> {
            field(key)?
                .as_array()
                .cloned()
                .ok_or_else(|| format!("message: `{key}` is no list"))
        };
        let failed = |entry: &Value| -> Result<Vec<FailedMember>, String> {
            entry["failed"]
                .as_array()
                .ok_or("message: `failed` is no list")?
                .iter()
                .map(|failed| {
                    Ok(FailedMember {
                        member: failed["member"]
                            .as_u64()
                            .ok_or("message: a malformed failure")?,
                        error: failed["error"].clone(),
                    })
                })
                .collect()
        };
        let members = |entry: &Value, key: &str| -> Result<Vec<(u64, Part)>, String> {
            entry["members"]
                .as_array()
                .ok_or("message: `members` is no list")?
                .iter()
                .map(|member| {
                    Ok((
                        member["member"]
                            .as_u64()
                            .ok_or("message: a malformed member")?,
                        part(&member[key])?,
                    ))
                })
                .collect()
        };
        Ok(match text("kind")?.as_str() {
            "hello" => Message::Hello {
                protocol: int("protocol")?,
                incarnation: text("incarnation")?,
                calls: int("calls")?,
            },
            "hold" => Message::Hold {
                origin,
                hold: int("hold")?,
                type_name: text("type")?,
                version: int("version")?,
                fingerprint: text("fingerprint")?,
                params: part(field("params")?)?,
                address: Address {
                    kind: field("address")?["kind"]
                        .as_str()
                        .ok_or("message: a malformed address")?
                        .to_owned(),
                    params: field("address")?["params"].clone(),
                },
                principal: value.get("principal").cloned(),
                incarnation: value
                    .get("incarnation")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                chain: value.get("chain").map(|chain| {
                    chain
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|id| id.as_str().map(str::to_owned))
                        .collect()
                }),
                limit: value.get("limit").and_then(Value::as_u64),
                trace: value.get("trace").cloned(),
            },
            "release" => Message::Release {
                origin,
                hold: int("hold")?,
            },
            "loading" => Message::Loading {
                origin,
                hold: int("hold")?,
            },
            "retry" => Message::Retry {
                origin,
                hold: int("hold")?,
                hops: int("hops")?,
            },
            "call" => Message::Call {
                origin,
                call: int("call")?,
                hold: int("hold")?,
                method: int("method")?,
                args: optional_part("args")?,
                segments: segments()?,
                trace: value.get("trace").cloned(),
                old: value.get("old") == Some(&Value::Bool(true)),
            },
            "abort" => Message::Abort {
                origin,
                call: int("call")?,
            },
            "bound" => Message::Bound {
                origin,
                holds: list("holds")?
                    .iter()
                    .map(|entry| {
                        Ok(BoundEntry {
                            hold: entry["hold"].as_u64().ok_or("message: a malformed hold")?,
                            variant: entry["variant"]
                                .as_u64()
                                .ok_or("message: a malformed variant")?,
                            members: members(entry, "value")?,
                            failed: failed(entry)?,
                        })
                    })
                    .collect::<Result<_, String>>()?,
            },
            "state" => Message::State {
                origin,
                holds: list("holds")?
                    .iter()
                    .map(|entry| {
                        Ok(StateEntry {
                            hold: entry["hold"].as_u64().ok_or("message: a malformed hold")?,
                            members: members(entry, "ops")?,
                            failed: failed(entry)?,
                            retired: entry["retired"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_u64)
                                .collect(),
                        })
                    })
                    .collect::<Result<_, String>>()?,
            },
            "ack" => Message::Ack { origin },
            "closed" => Message::Closed {
                origin: origin.ok_or("message: `closed` without an origin")?,
            },
            "revoke" => Message::Revoke {
                revocation: field("revocation")?.clone(),
            },
            "ping" => Message::Ping,
            "pong" => Message::Pong,
            "failed" => Message::Failed {
                origin,
                hold: int("hold")?,
                cause: CauseKind::ALL
                    .into_iter()
                    .find(|kind| {
                        field("cause").ok().and_then(|cause| cause["kind"].as_str())
                            == Some(kind.as_str())
                    })
                    .ok_or("message: an unknown failure kind")?,
                error: field("cause")?["error"].clone(),
                retry_in: value.get("retryIn").and_then(Value::as_u64),
            },
            "result" => Message::Result {
                origin,
                call: int("call")?,
                value: optional_part("value")?,
                segments: segments()?,
            },
            "error" => Message::Error {
                origin,
                call: int("call")?,
                error: field("error")?.clone(),
            },
            "gone" => Message::Gone {
                origin,
                hold: int("hold")?,
                error: field("error")?.clone(),
            },
            other => return Err(format!("message: an unknown kind {other:?}")),
        })
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn unhex(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("not hex: {text:?}"));
    }
    (0..text.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&text[index..index + 2], 16).map_err(|error| error.to_string())
        })
        .collect()
}
