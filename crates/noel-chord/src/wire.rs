//! Link protocol 4 frames, as upstream's `chord/src/wire.ts`.
//!
//! Every frame type has one direction, so ids never collide: a consumer sends
//! `hold`, `release`, `call`, `cancel`, `sub`, `unsub` and `resync`; a
//! provider sends `shape`, `gone`, `result` and `state`; both send `hello`,
//! `ping`, `pong`, `bye`, `root_end` and `ack`. State is addressed by
//! `(hold id, member)`: a hold implies the state of its variant, sent as a
//! unit (a shape followed by a snapshot of each member the consumer lacks).
//! Reload and revocation control (`invalidate`, `prepare`, `commit`,
//! `abort`, `retire`) goes from parent to child, `vote` from child to
//! parent.
//!
//! [`Frame::parse`] checks a received frame as upstream's frame schema does
//! (`isFrame`): anything it refuses is a protocol violation. Payloads (hold
//! inputs, call arguments, results, snapshots) stay [`Value`]s, validated where
//! they are used, against the contract.

use std::sync::Arc;

use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::contract::Description;
use crate::error::{ErrorCode, WireError};
use crate::js;

/// The link protocol version announced in `hello`. Peers of another version
/// do not talk: the side that notices closes the link with the reason
/// `protocol-version 4`.
pub const PROTOCOL_VERSION: u64 = 4;

/// What a peer is: a host (addressable, carries roots) or a client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Host,
    Client,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Host => "host",
            Kind::Client => "client",
        }
    }
}

/// The root a call runs in, carried on host-to-host calls.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootRef {
    #[serde(rename = "rootId")]
    pub root_id: String,
    #[serde(rename = "codeVersion")]
    pub code_version: u64,
}

/// A contract token: the consumer's description, sent in its own member
/// order, or one received and forwarded as it came.
#[derive(Clone, Debug, PartialEq)]
pub enum Token {
    Description(Arc<Description>),
    Value(Value),
}

impl Token {
    pub fn to_value(&self) -> Value {
        match self {
            Token::Description(description) => description.to_value(),
            Token::Value(value) => value.clone(),
        }
    }
}

impl Serialize for Token {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Token::Description(description) => description.serialize(serializer),
            Token::Value(value) => value.serialize(serializer),
        }
    }
}

/// One frame. Field order follows upstream's, so the JSON text matches what
/// upstream sends up to the order of keys inside payloads.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Frame {
    Hello {
        /// Absent means version 1.
        #[serde(skip_serializing_if = "Option::is_none")]
        protocol: Option<u64>,
        name: String,
        kind: Kind,
        #[serde(rename = "codeVersion")]
        code_version: u64,
        /// The sender acknowledges what it receives.
        #[serde(skip_serializing_if = "Option::is_none")]
        acks: Option<bool>,
    },
    /// Every frame received so far except `ack` frames.
    Ack {
        n: u64,
    },
    Hold {
        id: u64,
        /// Target host name, resolved by the consumer from the token's
        /// `host` and `input`. Relays route on it after checking it against
        /// both; the target refuses an input its own contract places
        /// elsewhere.
        at: String,
        token: Token,
        input: Value,
        /// A forwarded principal, honoured only from the parent link.
        #[serde(skip_serializing_if = "Option::is_none")]
        principal: Option<Value>,
        /// The client connection the hold comes from: an opaque id the
        /// server assigned to it, forwarded by relays. Honoured only from
        /// the parent link; present means the hold is a client's.
        #[serde(skip_serializing_if = "Option::is_none")]
        origin: Option<String>,
        /// `Some(false)`: no state for this hold until `sub` (a method-only
        /// binding).
        #[serde(skip_serializing_if = "Option::is_none")]
        state: Option<bool>,
        /// The consumer already has values (a re-hold after a link loss):
        /// its unit is not urgent.
        #[serde(skip_serializing_if = "Option::is_none")]
        again: Option<bool>,
    },
    Release {
        id: u64,
    },
    Shape {
        id: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        variant: Option<String>,
        members: Vec<String>,
        contract: String,
        /// The state members whose snapshots follow this frame; the
        /// consumer applies the shape once they are here.
        state: Vec<String>,
    },
    Gone {
        id: u64,
        error: WireError,
    },
    Call {
        id: u64,
        hold: u64,
        member: String,
        /// Absent for methods without arguments.
        #[serde(skip_serializing_if = "Option::is_none")]
        args: Option<Value>,
        /// Absent: the receiving host starts a root.
        #[serde(skip_serializing_if = "Option::is_none")]
        root: Option<RootRef>,
    },
    Result {
        id: u64,
        /// Absent for void results.
        #[serde(skip_serializing_if = "Option::is_none")]
        value: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<WireError>,
    },
    Cancel {
        id: u64,
    },
    /// Starts the state of a hold opened with `state: false` (answered
    /// with a unit).
    Sub {
        hold: u64,
    },
    /// Stops the state of a hold.
    Unsub {
        hold: u64,
    },
    /// Sends member `m` again as a reset (a relay that had to drop what
    /// waited for it).
    Resync {
        hold: u64,
        m: String,
    },
    State(StateFrame),
    Ping {
        n: i64,
    },
    Pong {
        n: i64,
    },
    Bye {
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    RootEnd {
        #[serde(rename = "rootId")]
        root_id: String,
    },
    Invalidate {
        pattern: Map<String, Value>,
    },
    Prepare {
        reload: u64,
        #[serde(rename = "codeVersion")]
        code_version: u64,
        plugins: Vec<String>,
        options: Value,
    },
    Vote {
        reload: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<WireError>,
        #[serde(skip_serializing_if = "Option::is_none")]
        report: Option<Value>,
    },
    Commit {
        reload: u64,
    },
    Retire {
        #[serde(rename = "codeVersion")]
        code_version: u64,
    },
    Abort {
        reload: u64,
    },
}

/// A `state` frame, addressed by hold and member: exactly one of `snap`,
/// `reset`, `ops` and `error` in practice; the schema does not insist.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StateFrame {
    pub hold: u64,
    pub m: String,
    pub as_kind: StateAs,
    pub value: Option<Value>,
    /// Wire tuples from the subscription's delta encoder.
    pub ops: Option<Vec<Value>>,
    pub error: Option<WireError>,
}

/// What a `state` frame carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StateAs {
    #[default]
    Snap,
    Reset,
    Ops,
    Error,
}

impl StateAs {
    fn as_str(self) -> &'static str {
        match self {
            StateAs::Snap => "snap",
            StateAs::Reset => "reset",
            StateAs::Ops => "ops",
            StateAs::Error => "error",
        }
    }
}

impl Serialize for StateFrame {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Inside the tagged enum: `t` is written by the enum.
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("hold", &self.hold)?;
        map.serialize_entry("m", &self.m)?;
        map.serialize_entry("as", self.as_kind.as_str())?;
        if let Some(value) = &self.value {
            map.serialize_entry("value", value)?;
        }
        if let Some(ops) = &self.ops {
            map.serialize_entry("ops", ops)?;
        }
        if let Some(error) = &self.error {
            map.serialize_entry("error", error)?;
        }
        map.end()
    }
}

impl Frame {
    /// The frame's `t`.
    pub fn kind(&self) -> &'static str {
        match self {
            Frame::Hello { .. } => "hello",
            Frame::Ack { .. } => "ack",
            Frame::Hold { .. } => "hold",
            Frame::Release { .. } => "release",
            Frame::Shape { .. } => "shape",
            Frame::Gone { .. } => "gone",
            Frame::Call { .. } => "call",
            Frame::Result { .. } => "result",
            Frame::Cancel { .. } => "cancel",
            Frame::Sub { .. } => "sub",
            Frame::Unsub { .. } => "unsub",
            Frame::Resync { .. } => "resync",
            Frame::State(_) => "state",
            Frame::Ping { .. } => "ping",
            Frame::Pong { .. } => "pong",
            Frame::Bye { .. } => "bye",
            Frame::RootEnd { .. } => "root_end",
            Frame::Invalidate { .. } => "invalidate",
            Frame::Prepare { .. } => "prepare",
            Frame::Vote { .. } => "vote",
            Frame::Commit { .. } => "commit",
            Frame::Retire { .. } => "retire",
            Frame::Abort { .. } => "abort",
        }
    }

    /// The JSON text, as one WebSocket text message or IPC payload carries it.
    pub fn to_text(&self) -> String {
        serde_json::to_string(self).expect("a frame is JSON")
    }

    /// The frame as a JSON value.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("a frame is JSON")
    }

    /// Checks a received frame as upstream's frame schema does. The error
    /// says what is wrong; upstream reports every such frame as `malformed
    /// frame`.
    pub fn parse(value: &Value) -> Result<Frame, String> {
        let object = value.as_object().ok_or("not an object")?;
        let fields = Fields(object);
        let t = fields.string("t")?;
        let frame = match t {
            "hello" => {
                let kind = match fields.string("kind")? {
                    "host" => Kind::Host,
                    "client" => Kind::Client,
                    other => return Err(format!("kind {other}")),
                };
                Frame::Hello {
                    protocol: fields
                        .optional_integer("protocol", Some(1))?
                        .map(|n| n as u64),
                    name: fields.name("name")?,
                    kind,
                    code_version: fields.integer("codeVersion", Some(0))? as u64,
                    acks: fields.optional_bool("acks")?,
                }
            }
            "ack" => Frame::Ack {
                n: fields.integer("n", Some(1))? as u64,
            },
            "hold" => {
                let token = fields.required("token")?;
                check_description(token).map_err(|error| format!("token: {error}"))?;
                Frame::Hold {
                    id: fields.id("id")?,
                    at: fields.name("at")?,
                    token: Token::Value(token.clone()),
                    input: fields.required("input")?.clone(),
                    principal: fields.optional("principal")?.cloned(),
                    origin: fields.optional_string("origin")?,
                    state: match fields.optional("state")? {
                        None | Some(Value::Null) => None,
                        Some(Value::Bool(false)) => Some(false),
                        Some(Value::Bool(true)) => return Err("state must be false".to_owned()),
                        Some(_) => return Err("state must be a boolean".to_owned()),
                    },
                    again: match fields.optional("again")? {
                        None | Some(Value::Null) => None,
                        Some(Value::Bool(true)) => Some(true),
                        Some(Value::Bool(false)) => return Err("again must be true".to_owned()),
                        Some(_) => return Err("again must be a boolean".to_owned()),
                    },
                }
            }
            "release" => Frame::Release {
                id: fields.id("id")?,
            },
            "shape" => Frame::Shape {
                id: fields.id("id")?,
                variant: fields.optional_string("variant")?,
                members: fields.strings("members")?,
                contract: fields.string("contract")?.to_owned(),
                state: fields.strings("state")?,
            },
            "gone" => Frame::Gone {
                id: fields.id("id")?,
                error: wire_error(fields.required("error")?)?,
            },
            "call" => Frame::Call {
                id: fields.id("id")?,
                hold: fields.id("hold")?,
                member: fields.name("member")?,
                args: fields.optional("args")?.cloned(),
                root: match fields.optional("root")? {
                    None => None,
                    Some(root) => Some(root_ref(root)?),
                },
            },
            "result" => Frame::Result {
                id: fields.id("id")?,
                value: fields.optional("value")?.cloned(),
                error: fields.optional("error")?.map(wire_error).transpose()?,
            },
            "cancel" => Frame::Cancel {
                id: fields.id("id")?,
            },
            "sub" => Frame::Sub {
                hold: fields.id("hold")?,
            },
            "unsub" => Frame::Unsub {
                hold: fields.id("hold")?,
            },
            "resync" => Frame::Resync {
                hold: fields.id("hold")?,
                m: fields.name("m")?,
            },
            "state" => {
                let as_kind = match fields.string("as")? {
                    "snap" => StateAs::Snap,
                    "reset" => StateAs::Reset,
                    "ops" => StateAs::Ops,
                    "error" => StateAs::Error,
                    other => return Err(format!("as {other}")),
                };
                Frame::State(StateFrame {
                    hold: fields.id("hold")?,
                    m: fields.name("m")?,
                    as_kind,
                    value: fields.optional("value")?.cloned(),
                    ops: match fields.optional("ops")? {
                        None => None,
                        Some(Value::Array(ops)) if ops.iter().all(Value::is_array) => {
                            Some(ops.clone())
                        }
                        Some(_) => return Err("ops must be a list of lists".to_owned()),
                    },
                    error: fields.optional("error")?.map(wire_error).transpose()?,
                })
            }
            "ping" => Frame::Ping {
                n: fields.integer("n", None)?,
            },
            "pong" => Frame::Pong {
                n: fields.integer("n", None)?,
            },
            "bye" => Frame::Bye {
                reason: fields.optional_string("reason")?,
            },
            "root_end" => Frame::RootEnd {
                root_id: fields.name("rootId")?,
            },
            "invalidate" => match fields.required("pattern")? {
                Value::Object(pattern) => Frame::Invalidate {
                    pattern: pattern.clone(),
                },
                _ => return Err("pattern must be an object".to_owned()),
            },
            "prepare" => {
                let options = fields.required("options")?;
                check_reload_options(options)?;
                Frame::Prepare {
                    reload: fields.integer("reload", Some(1))? as u64,
                    code_version: fields.integer("codeVersion", Some(1))? as u64,
                    plugins: fields.strings("plugins")?,
                    options: options.clone(),
                }
            }
            "vote" => {
                let report = fields.optional("report")?;
                if let Some(report) = report {
                    check_reload_report(report)?;
                }
                Frame::Vote {
                    reload: fields.integer("reload", Some(1))? as u64,
                    error: fields.optional("error")?.map(wire_error).transpose()?,
                    report: report.cloned(),
                }
            }
            "commit" => Frame::Commit {
                reload: fields.integer("reload", Some(1))? as u64,
            },
            "retire" => Frame::Retire {
                code_version: fields.integer("codeVersion", Some(1))? as u64,
            },
            "abort" => Frame::Abort {
                reload: fields.integer("reload", Some(1))? as u64,
            },
            other => return Err(format!("unknown frame type {other}")),
        };
        Ok(frame)
    }
}

/// The size a link counts for flow control: the JSON text's length in UTF-16
/// code units, as JavaScript's `string.length`.
pub fn text_size(text: &str) -> usize {
    js::utf16_len(text)
}

/// Field access with TypeBox's rules: an optional field may be absent but
/// not `null` (unless its type admits `null`); a required `Unknown` must be
/// present.
struct Fields<'a>(&'a Map<String, Value>);

impl<'a> Fields<'a> {
    fn required(&self, key: &str) -> Result<&'a Value, String> {
        self.0.get(key).ok_or_else(|| format!("{key} is missing"))
    }

    fn optional(&self, key: &str) -> Result<Option<&'a Value>, String> {
        Ok(self.0.get(key))
    }

    fn string(&self, key: &str) -> Result<&'a str, String> {
        self.required(key)?
            .as_str()
            .ok_or_else(|| format!("{key} must be a string"))
    }

    /// A string of at least one UTF-16 code unit.
    fn name(&self, key: &str) -> Result<String, String> {
        let text = self.string(key)?;
        if text.is_empty() {
            return Err(format!("{key} must not be empty"));
        }
        Ok(text.to_owned())
    }

    fn optional_string(&self, key: &str) -> Result<Option<String>, String> {
        match self.0.get(key) {
            None => Ok(None),
            Some(Value::String(text)) => Ok(Some(text.clone())),
            Some(_) => Err(format!("{key} must be a string")),
        }
    }

    fn optional_bool(&self, key: &str) -> Result<Option<bool>, String> {
        match self.0.get(key) {
            None => Ok(None),
            Some(Value::Bool(flag)) => Ok(Some(*flag)),
            Some(_) => Err(format!("{key} must be a boolean")),
        }
    }

    fn integer(&self, key: &str, minimum: Option<i64>) -> Result<i64, String> {
        integer(self.required(key)?, minimum).ok_or_else(|| match minimum {
            Some(minimum) => format!("{key} must be an integer of at least {minimum}"),
            None => format!("{key} must be an integer"),
        })
    }

    fn optional_integer(&self, key: &str, minimum: Option<i64>) -> Result<Option<i64>, String> {
        match self.0.get(key) {
            None => Ok(None),
            Some(_) => self.integer(key, minimum).map(Some),
        }
    }

    fn id(&self, key: &str) -> Result<u64, String> {
        Ok(self.integer(key, Some(1))? as u64)
    }

    fn strings(&self, key: &str) -> Result<Vec<String>, String> {
        strings(self.required(key)?).ok_or_else(|| format!("{key} must be a list of strings"))
    }
}

/// `Number.isInteger(value)` and at least `minimum`, within the range a
/// double holds exactly.
fn integer(value: &Value, minimum: Option<i64>) -> Option<i64> {
    let Value::Number(number) = value else {
        return None;
    };
    let float = js::number_value(number);
    if !float.is_finite() || float.fract() != 0.0 || float.abs() > 9_007_199_254_740_991.0 {
        return None;
    }
    let integer = float as i64;
    match minimum {
        Some(minimum) if integer < minimum => None,
        _ => Some(integer),
    }
}

fn strings(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|item| item.as_str().map(str::to_owned))
        .collect()
}

fn wire_error(value: &Value) -> Result<WireError, String> {
    let object = value.as_object().ok_or("error must be an object")?;
    let fields = Fields(object);
    let code = ErrorCode::parse(fields.string("code")?).ok_or("unknown error code")?;
    Ok(WireError {
        code,
        message: fields.string("message")?.to_owned(),
        reason: fields.optional_string("reason")?,
    })
}

fn root_ref(value: &Value) -> Result<RootRef, String> {
    let object = value.as_object().ok_or("root must be an object")?;
    let fields = Fields(object);
    Ok(RootRef {
        root_id: fields.name("rootId")?,
        code_version: fields.integer("codeVersion", Some(1))? as u64,
    })
}

fn is_object(value: &Value) -> bool {
    value.is_object()
}

/// Upstream's `DescriptionSchema`.
fn check_description(value: &Value) -> Result<(), String> {
    let object = value.as_object().ok_or("must be an object")?;
    let fields = Fields(object);
    fields.name("name")?;
    match fields.string("scope")? {
        "remote" | "local" => {}
        _ => return Err("scope must be remote or local".to_owned()),
    }
    fields.integer("version", None)?;
    if !is_object(fields.required("input")?) {
        return Err("input must be an object".to_owned());
    }
    if let Some(host) = fields.optional("host")? {
        check_placement(host)?;
    }
    let variants = fields
        .required("variants")?
        .as_object()
        .ok_or("variants must be an object")?;
    for members in variants.values() {
        let members = members.as_object().ok_or("a variant must be an object")?;
        for member in members.values() {
            check_member(member)?;
        }
    }
    Ok(())
}

/// A placement: `{name}` or `{prefix, key}`, both non-empty strings.
fn check_placement(value: &Value) -> Result<(), String> {
    let object = value.as_object().ok_or("host must be an object")?;
    let fields = Fields(object);
    match (fields.optional_string("name")?, object.get("prefix")) {
        (Some(_), None) => Ok(()),
        (None, Some(_)) => {
            fields.name("prefix")?;
            fields.name("key")?;
            Ok(())
        }
        _ => Err("host must be a name or a prefix and key".to_owned()),
    }
}

fn check_member(value: &Value) -> Result<(), String> {
    let object = value.as_object().ok_or("a member must be an object")?;
    let fields = Fields(object);
    match fields.string("kind")? {
        "method" => {
            for key in ["args", "result"] {
                if let Some(schema) = fields.optional(key)? {
                    if !is_object(schema) {
                        return Err(format!("{key} must be a schema object"));
                    }
                }
            }
        }
        "state" => {
            if !is_object(fields.required("schema")?) {
                return Err("schema must be a schema object".to_owned());
            }
        }
        "opaque" => {
            fields.string("id")?;
        }
        other => return Err(format!("member kind {other}")),
    }
    Ok(())
}

fn check_reload_options(value: &Value) -> Result<(), String> {
    let object = value.as_object().ok_or("options must be an object")?;
    let fields = Fields(object);
    if let Some(remove) = fields.optional("remove")? {
        strings(remove).ok_or("remove must be a list of strings")?;
    }
    for key in ["gateDeadline", "retireDeadline"] {
        if let Some(number) = fields.optional(key)? {
            if !number.is_number() {
                return Err(format!("{key} must be a number"));
            }
        }
    }
    Ok(())
}

fn check_reload_report(value: &Value) -> Result<(), String> {
    let object = value.as_object().ok_or("report must be an object")?;
    let fields = Fields(object);
    fields.integer("codeVersion", None)?;
    fields.integer("previous", None)?;
    for key in ["changed", "removed", "prepared", "gated"] {
        fields.strings(key)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frames_round_trip_through_their_text() {
        let frames = [
            json!({"t": "hello", "protocol": 4, "name": "server", "kind": "host", "codeVersion": 1, "acks": true}),
            json!({"t": "ack", "n": 3}),
            json!({"t": "release", "id": 1}),
            json!({"t": "shape", "id": 1, "variant": "owner", "members": ["count"], "contract": "93ba9f7ef8c79f2b", "state": ["count"]}),
            json!({"t": "gone", "id": 2, "error": {"code": "forbidden", "message": "forbidden: no"}}),
            json!({"t": "call", "id": 3, "hold": 1, "member": "add", "args": 1}),
            json!({"t": "call", "id": 3, "hold": 1, "member": "say", "args": "hi", "root": {"rootId": "server#x:1", "codeVersion": 1}}),
            json!({"t": "result", "id": 3}),
            json!({"t": "result", "id": 3, "value": null}),
            json!({"t": "state", "hold": 2, "m": "count", "as": "ops", "ops": [["s", ["n"], 1]]}),
            json!({"t": "state", "hold": 2, "m": "count", "as": "snap", "value": {"n": 0}}),
            json!({"t": "state", "hold": 2, "m": "count", "as": "reset"}),
            json!({"t": "sub", "hold": 2}),
            json!({"t": "unsub", "hold": 2}),
            json!({"t": "resync", "hold": 2, "m": "count"}),
            json!({"t": "ping", "n": -4}),
            json!({"t": "bye", "reason": "host closed"}),
            json!({"t": "root_end", "rootId": "server#x:1"}),
            json!({"t": "invalidate", "pattern": {"kind": "guest"}}),
            json!({"t": "vote", "reload": 1, "error": {"code": "unavailable", "message": "unavailable: no"}}),
        ];
        for value in frames {
            let frame = Frame::parse(&value).unwrap_or_else(|error| panic!("{value}: {error}"));
            assert_eq!(frame.to_value(), value);
        }
    }

    #[test]
    fn malformed_frames_are_refused_as_the_schema_refuses_them() {
        for value in [
            json!({"t": "call", "hold": 1}),
            json!({"t": "hold", "id": 0, "at": "s", "token": {}, "input": {}}),
            json!({"t": "hold", "id": 1, "at": "s", "token": {"name": "x", "scope": "remote", "version": 1, "input": {}, "variants": {}}}),
            json!({"t": "bye", "reason": null}),
            json!({"t": "hello", "name": "", "kind": "host", "codeVersion": 1}),
            json!({"t": "hello", "name": "a", "kind": "browser", "codeVersion": 1}),
            json!({"t": "ack", "n": 0}),
            json!({"t": "ping", "n": 1.5}),
            json!({"t": "state", "hold": 1, "m": "count", "as": "ops", "ops": [1]}),
            json!({"t": "state", "hold": 1, "m": "count", "as": "nope"}),
            json!({"t": "hold", "id": 1, "at": "s", "token": {"name": "x", "scope": "remote", "version": 1, "input": {}, "host": {}, "variants": {"o": {}}}, "input": {}}),
            json!({"t": "gone", "id": 1, "error": {"code": "nope", "message": ""}}),
            json!({"t": "nope"}),
            json!([1]),
        ] {
            assert!(Frame::parse(&value).is_err(), "{value} should be refused");
        }
        // Extra fields are allowed, as in a TypeBox object.
        assert!(Frame::parse(&json!({"t": "ping", "n": 1, "extra": true})).is_ok());
        // 1.0 is an integer to JavaScript.
        assert!(Frame::parse(&json!({"t": "release", "id": 1.0})).is_ok());
    }
}
