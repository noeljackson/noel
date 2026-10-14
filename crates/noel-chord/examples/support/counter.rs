//! The counter the examples serve and hold: the contract upstream's TypeBox
//! makes for `noel.test.counter` in `scripts/ts-parity/chord-link-fixture.ts`
//! (and the interop scripts), byte for byte, so both sides share one
//! fingerprint.

use noel_chord::delta::{Op, Seg};
use noel_chord::host::Host;
use noel_chord::object::{Cell, Implementation};
use noel_chord::{ChordError, Description};
use serde_json::{json, Value};

pub const COUNTER: &str = r#"{
  "name": "noel.test.counter",
  "scope": "remote",
  "version": 1,
  "input": {"type": "object", "required": ["name"], "properties": {"name": {"type": "string"}}},
  "variants": {
    "owner": {
      "count": {"kind": "state", "schema": {"type": "object", "required": ["n"], "properties": {"n": {"type": "integer"}}}},
      "add": {"kind": "method", "args": {"type": "integer"}, "result": {"type": "integer"}},
      "set": {"kind": "method", "args": {"type": "integer"}},
      "wait": {"kind": "method"},
      "fail": {"kind": "method", "result": {"type": "integer"}}
    },
    "viewer": {
      "count": {"kind": "state", "schema": {"type": "object", "required": ["n"], "properties": {"n": {"type": "integer"}}}}
    }
  }
}"#;

pub fn contract() -> Description {
    serde_json::from_str(COUNTER).expect("the counter contract parses")
}

/// Owners get `owner` (state and methods), guests `viewer`, anyone else is
/// refused.
pub fn serve(host: &Host) {
    host.serve(contract(), |input, principal| {
        let name = input["name"].as_str().unwrap_or("").to_owned();
        let kind = principal.get("kind").and_then(Value::as_str).unwrap_or("");
        if kind != "owner" && kind != "guest" {
            return Err(ChordError::forbidden(format!(
                "{name} is not for this principal"
            )));
        }
        let count = Cell::new(
            json!({"type": "object", "required": ["n"], "properties": {"n": {"type": "integer"}}}),
            json!({"n": 0}),
        )?;
        if kind != "owner" {
            return Ok(Implementation::new("viewer").state("count", &count));
        }
        let (add, set) = (count.clone(), count.clone());
        Ok(Implementation::new("owner")
            .state("count", &count)
            .method("add", move |args, _ctx| {
                let count = add.clone();
                async move {
                    let delta = args.as_ref().and_then(Value::as_i64).unwrap_or(0);
                    let value = count.change(|value| {
                        let n = value["n"].as_i64().unwrap_or(0) + delta;
                        vec![Op::Set(vec![Seg::Key("n".to_owned())], json!(n))]
                    })?;
                    Ok(value["n"].clone())
                }
            })
            .method("set", move |args, _ctx| {
                let count = set.clone();
                async move {
                    count.replace(json!({"n": args.unwrap_or(Value::Null)}))?;
                    Ok(Value::Null)
                }
            })
            .method("wait", |_args, ctx| async move {
                Err(ctx.signal.aborted().await)
            })
            .method("fail", |_args, _ctx| async {
                Err(ChordError::unavailable("boom"))
            }))
    });
}
