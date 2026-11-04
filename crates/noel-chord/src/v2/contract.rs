//! Chord v2 contracts (upstream `src/contracts`): a contract's params, its
//! variants' state and methods as JSON Schema, checked against Chord's
//! vocabulary, and its fingerprint: `hash64` of
//! `JSON.stringify({normalFormVersion: 2, value})` over the normal form
//! `{kind: "remote", params, variants}`. The contract's name, version and
//! host address are not in it.
//!
//! The normal form sorts object keys, `required` and `enum`, and drops
//! annotations on schema nodes, so `serde_json`'s sorted maps serialize it
//! as `JSON.stringify` does upstream's.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::message::Address;

/// Version of the normal form (upstream `NORMAL_FORM_VERSION`).
pub const NORMAL_FORM_VERSION: u64 = 2;

/// 16 hex characters from two 32-bit FNV-1a lanes (different offset bases
/// and primes) over the UTF-8 bytes (upstream v2 `hash64`; not v1's, which
/// hashes UTF-16 code units).
pub fn hash64(text: &str) -> String {
    let mut low: u32 = 0x811c_9dc5;
    let mut high: u32 = 0xcbf2_9ce4;
    for byte in text.bytes() {
        low = (low ^ u32::from(byte)).wrapping_mul(0x0100_0193);
        high = (high ^ u32::from(byte)).wrapping_mul(0x0100_0197);
    }
    format!("{high:08x}{low:08x}")
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ContractError(pub String);

fn error(message: impl Into<String>) -> ContractError {
    ContractError(message.into())
}

/// The kind of host a contract's objects live on (upstream `hostKind`):
/// noel's server, or a session's worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostKind {
    Server,
    Session,
}

/// A method's arguments and result schemas.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MethodDeclaration {
    pub args: Value,
    pub result: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct VariantDeclaration {
    #[serde(default)]
    pub state: BTreeMap<String, Value>,
    #[serde(default)]
    pub methods: BTreeMap<String, MethodDeclaration>,
}

/// A remote contract as noel writes it (`noel-client/contracts/v2/*.json`):
/// params in identity order, and per variant its state and methods.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Declaration {
    pub name: String,
    pub version: u64,
    pub host: HostKind,
    pub params: Vec<(String, Value)>,
    pub variants: BTreeMap<String, VariantDeclaration>,
}

/// A member's kind across variants, and a method's signature.
type MemberKind<'a> = (&'static str, Option<(&'a Value, &'a Value)>);

/// A checked contract with its normal form and fingerprint.
#[derive(Debug, Clone, PartialEq)]
pub struct Contract {
    pub declaration: Declaration,
    /// The schemas as Chord keeps them (annotations kept, keys sorted).
    pub normal: Value,
    pub fingerprint: String,
}

impl Contract {
    /// Checks `declaration` as upstream's `remote()` does and computes its
    /// fingerprint.
    pub fn declare(declaration: Declaration) -> Result<Contract, ContractError> {
        check_name(&declaration.name)?;
        if declaration.variants.is_empty() {
            return Err(error(format!(
                "{}: a remote contract needs a variant",
                declaration.name
            )));
        }
        let mut params = Vec::new();
        for (field, schema) in &declaration.params {
            let path = format!("{}.params.{field}", declaration.name);
            let normal = snapshot(schema, &path, Objects::Open)?;
            if !is_param(&normal) {
                return Err(error(format!(
                    "{path}: a param is a required str(), int(), bool() or oneOf(...) of strings"
                )));
            }
            params.push(json!([field, normal]));
        }
        // A member has one kind across variants, and a method one signature.
        let mut kinds: BTreeMap<&str, MemberKind<'_>> = BTreeMap::new();
        let mut variants = Map::new();
        for (variant, members) in &declaration.variants {
            let path = format!("{}.variants.{variant}", declaration.name);
            let mut normal = Map::new();
            for (member, schema) in &members.state {
                check_member(member, &format!("{path}.{member}"))?;
                if let Some((kind, _)) = kinds.insert(member, ("state", None)) {
                    if kind != "state" {
                        return Err(error(format!("{path}.{member}: a member has one kind")));
                    }
                }
                let state = snapshot(schema, &format!("{path}.state.{member}"), Objects::Open)?;
                normal.insert(member.clone(), json!({ "state": state }));
            }
            for (member, method) in &members.methods {
                let at = format!("{path}.methods.{member}");
                check_member(member, &at)?;
                let signature = Some((&method.args, &method.result));
                match kinds.insert(member, ("method", signature)) {
                    Some(("method", previous)) if previous != signature => {
                        return Err(error(format!(
                            "{at}: a method has one signature across variants"
                        )));
                    }
                    Some(("state", _)) => {
                        return Err(error(format!("{at}: a member has one kind")))
                    }
                    _ => {}
                }
                let args = snapshot(&method.args, &format!("{at}.args"), Objects::Closed)?;
                let result = snapshot(&method.result, &format!("{at}.result"), Objects::Open)?;
                normal.insert(member.clone(), json!({ "args": args, "result": result }));
            }
            variants.insert(variant.clone(), Value::Object(normal));
        }
        let normal = json!({
            "kind": "remote",
            "params": params,
            "variants": variants,
        });
        let fingerprint = hash64(
            &json!({ "normalFormVersion": NORMAL_FORM_VERSION, "value": normal }).to_string(),
        );
        Ok(Contract {
            declaration,
            normal,
            fingerprint,
        })
    }

    /// Parses and checks a declaration in noel's JSON form.
    pub fn from_json(text: &str) -> Result<Contract, ContractError> {
        let declaration: Declaration =
            serde_json::from_str(text).map_err(|parse| error(format!("contract: {parse}")))?;
        Contract::declare(declaration)
    }

    /// The variants in the order links index them: sorted.
    pub fn variant_order(&self) -> Vec<&str> {
        self.declaration
            .variants
            .keys()
            .map(String::as_str)
            .collect()
    }

    /// A variant's state members and methods in the order links index them:
    /// each sorted.
    pub fn variant_names(&self, variant: &str) -> Option<(Vec<&str>, Vec<&str>)> {
        let variant = self.declaration.variants.get(variant)?;
        Some((
            variant.state.keys().map(String::as_str).collect(),
            variant.methods.keys().map(String::as_str).collect(),
        ))
    }

    /// The method `name` of any variant (a method has one signature across
    /// variants).
    pub fn method(&self, name: &str) -> Option<&MethodDeclaration> {
        self.declaration
            .variants
            .values()
            .find_map(|variant| variant.methods.get(name))
    }

    /// The address of the object with `params` (upstream `hostAddress`):
    /// `Server()` for server contracts, `Session({id: params.session})` for
    /// session contracts.
    pub fn address(&self, params: &Value) -> Address {
        match self.declaration.host {
            HostKind::Server => Address {
                kind: "server".to_owned(),
                params: json!({}),
            },
            HostKind::Session => Address {
                kind: "session".to_owned(),
                params: json!({"id": params.get("session").cloned().unwrap_or(Value::Null)}),
            },
        }
    }

    /// Checks `params` against the declared fields: an object with exactly
    /// those fields, each valid.
    pub fn check_params(&self, params: &Value) -> Result<(), String> {
        let Value::Object(fields) = params else {
            return Err("params: not an object".to_owned());
        };
        if let Some(extra) = fields
            .keys()
            .find(|key| !self.declaration.params.iter().any(|(name, _)| name == *key))
        {
            return Err(format!("params: an unknown field {extra:?}"));
        }
        for (name, schema) in &self.declaration.params {
            let value = fields
                .get(name)
                .ok_or_else(|| format!("params: {name:?} is missing"))?;
            crate::validate::check(schema, value)
                .map_err(|error| format!("params.{name}: {error}"))?;
        }
        Ok(())
    }
}

/// Names use `[a-z0-9@/._~-]` (upstream `checkName`).
fn check_name(name: &str) -> Result<(), ContractError> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "@/._~-".contains(c));
    if valid {
        Ok(())
    } else {
        Err(error(format!(
            "A contract's name \"{name}\" must use a-z, 0-9 and @/._~-"
        )))
    }
}

/// Member names that a reference's own properties use (upstream).
const RESERVED_MEMBERS: [&str; 5] = ["then", "release", "state", "retry", "variant"];

fn check_member(member: &str, path: &str) -> Result<(), ContractError> {
    if member.is_empty() || RESERVED_MEMBERS.contains(&member) {
        return Err(error(format!("{path}: \"{member}\" is not a member name")));
    }
    Ok(())
}

/// Whether object schemas must be closed (method arguments).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Objects {
    Open,
    Closed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Keyword {
    Schema,
    Schemas,
    Items,
    SchemaMap,
    Type,
    Required,
    Enum,
    Data,
    Annotation,
}

fn keyword(name: &str) -> Option<Keyword> {
    Some(match name {
        "additionalProperties" | "additionalItems" => Keyword::Schema,
        "anyOf" | "allOf" | "prefixItems" => Keyword::Schemas,
        "items" => Keyword::Items,
        "properties" | "patternProperties" | "$defs" => Keyword::SchemaMap,
        "type" => Keyword::Type,
        "required" => Keyword::Required,
        "enum" => Keyword::Enum,
        "const" | "default" | "$ref" | "$id" | "pattern" | "format" | "minLength" | "maxLength"
        | "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum" | "multipleOf"
        | "minItems" | "maxItems" | "uniqueItems" | "minProperties" | "maxProperties"
        | "readOnly" | "writeOnly" => Keyword::Data,
        "title" | "description" | "examples" | "$comment" | "deprecated" => Keyword::Annotation,
        _ => return None,
    })
}

const TYPES: [&str; 8] = [
    "null", "boolean", "number", "integer", "string", "array", "object", "bytes",
];

/// A schema's normal form (upstream `snapshotSchema`/`snapshotArguments`):
/// checked against the vocabulary, annotations dropped, `required` and
/// `enum` sorted; keys sort as `serde_json` keeps them.
fn snapshot(schema: &Value, path: &str, objects: Objects) -> Result<Value, ContractError> {
    let node = match schema {
        Value::Bool(_) => return Ok(schema.clone()),
        Value::Object(node) => node,
        _ => return Err(error(format!("{path}: expected a schema"))),
    };
    let is_bytes = node.get("type") == Some(&json!("bytes"));
    let mut normal = Map::new();
    for (key, value) in node {
        let at = format!("{path}.{key}");
        let kind = keyword(key)
            .filter(|kind| {
                !is_bytes
                    || *kind == Keyword::Annotation
                    || matches!(key.as_str(), "type" | "minLength" | "maxLength")
            })
            .ok_or_else(|| error(format!("{path}: unsupported keyword \"{key}\"")))?;
        let value = match kind {
            Keyword::Annotation => continue,
            Keyword::Schema => snapshot(value, &at, objects)?,
            Keyword::Schemas => schemas(value, &at, objects)?,
            Keyword::Items => match value {
                Value::Array(_) => schemas(value, &at, objects)?,
                _ => snapshot(value, &at, objects)?,
            },
            Keyword::SchemaMap => {
                let Value::Object(entries) = value else {
                    return Err(error(format!("{at}: expected an object of schemas")));
                };
                let mut map = Map::new();
                for (name, item) in entries {
                    map.insert(
                        name.clone(),
                        snapshot(item, &format!("{at}.{name}"), objects)?,
                    );
                }
                Value::Object(map)
            }
            Keyword::Type => match value.as_str() {
                Some(name) if TYPES.contains(&name) => value.clone(),
                _ => return Err(error(format!("{at}: unsupported type {value}"))),
            },
            Keyword::Required => {
                let mut names: Vec<String> = value
                    .as_array()
                    .and_then(|names| {
                        names
                            .iter()
                            .map(|name| name.as_str().map(str::to_owned))
                            .collect()
                    })
                    .ok_or_else(|| error(format!("{at}: expected an array of strings")))?;
                names.sort();
                json!(names)
            }
            Keyword::Enum => {
                let mut values = value
                    .as_array()
                    .cloned()
                    .ok_or_else(|| error(format!("{at}: expected an array")))?;
                values.sort_by_key(|value| value.to_string());
                Value::Array(values)
            }
            Keyword::Data => value.clone(),
        };
        normal.insert(key.clone(), value);
    }
    if objects == Objects::Closed && is_open_object(&normal) {
        return Err(error(format!(
            "{path}: argument objects must be closed (additionalProperties: false)"
        )));
    }
    Ok(Value::Object(normal))
}

fn schemas(value: &Value, path: &str, objects: Objects) -> Result<Value, ContractError> {
    let Value::Array(items) = value else {
        return Err(error(format!("{path}: expected an array of schemas")));
    };
    items
        .iter()
        .enumerate()
        .map(|(index, item)| snapshot(item, &format!("{path}[{index}]"), objects))
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Array)
}

/// An object schema that admits unknown fields; a record (pattern
/// properties only) is allowed.
fn is_open_object(node: &Map<String, Value>) -> bool {
    let is_record = node.contains_key("patternProperties") && !node.contains_key("properties");
    let is_object = node.get("type") == Some(&json!("object")) || node.contains_key("properties");
    is_object && !is_record && node.get("additionalProperties") != Some(&json!(false))
}

/// A params field: a string, integer or boolean, or an enum of strings.
fn is_param(normal: &Value) -> bool {
    match normal.get("type").and_then(Value::as_str) {
        Some("string" | "integer" | "boolean") => normal.get("default").is_none(),
        None => normal
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|values| values.iter().all(Value::is_string)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash64_is_two_fnv_lanes_over_utf8() {
        assert_eq!(hash64(""), "cbf29ce4811c9dc5");
        assert_eq!(hash64("é€😀"), "6c275e1104a2e806");
    }

    #[test]
    fn arguments_must_be_closed_and_names_checked() {
        let open = r#"{"name":"x","version":1,"host":"server","params":[],"variants":{"a":{"methods":{"go":{"args":{"type":"object","properties":{}},"result":{"type":"null"}}}}}}"#;
        let refused = Contract::from_json(open).unwrap_err().0;
        assert!(
            refused.contains("argument objects must be closed"),
            "{refused}"
        );
        let named =
            r#"{"name":"Bad Name","version":1,"host":"server","params":[],"variants":{"a":{}}}"#;
        assert!(Contract::from_json(named).is_err());
        let reserved = r#"{"name":"x","version":1,"host":"server","params":[],"variants":{"a":{"state":{"then":true}}}}"#;
        assert!(Contract::from_json(reserved).is_err());
    }
}
