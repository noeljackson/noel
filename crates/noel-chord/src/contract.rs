//! Contract descriptions, their fingerprints and coverage, as upstream's
//! `chord/src/contract.ts`.
//!
//! A description is what travels as a hold's `token`:
//! `{name, scope, version, input, variants: {variant: {member: description}}}`,
//! with JSON Schemas inside. A provider binds a consumer whose fingerprint
//! equals its own, or whose version equals its own when [`coverage`] finds
//! nothing missing.

use std::fmt;
use std::marker::PhantomData;

use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::js;
use crate::schema::{canonical_json, fits_optional, hash64};

/// The variant name of a local contract.
pub const LOCAL_VARIANT: &str = "";

/// Names a reference uses itself, which members may not use.
pub const RESERVED_MEMBERS: [&str; 10] = [
    "variant", "status", "ready", "shape", "error", "onShape", "onStatus", "release", "then",
    "contract",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Remote,
    Local,
}

/// One member of a variant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum MemberDescription {
    /// Absent `args`: the method takes none. Absent `result`: it returns
    /// nothing.
    Method {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
    },
    State {
        schema: Value,
    },
    /// A native in-process object, `name@version`; local contracts only.
    Opaque {
        id: String,
    },
}

impl MemberDescription {
    pub fn method(args: Option<Value>, result: Option<Value>) -> Self {
        MemberDescription::Method { args, result }
    }

    pub fn state(schema: Value) -> Self {
        MemberDescription::State { schema }
    }

    pub fn opaque(id: impl Into<String>) -> Self {
        MemberDescription::Opaque { id: id.into() }
    }

    fn kind(&self) -> &'static str {
        match self {
            MemberDescription::Method { .. } => "method",
            MemberDescription::State { .. } => "state",
            MemberDescription::Opaque { .. } => "opaque",
        }
    }
}

/// A contract description. Variants and members keep JavaScript's property
/// order, so [`coverage`] reports the same first difference upstream does.
///
/// Deserialize it from the JSON text: a [`Value`] has already sorted its
/// keys, so a description converted from one loses the sender's order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Description {
    pub name: String,
    pub scope: Scope,
    pub version: i64,
    pub input: Value,
    pub variants: PropertyMap<PropertyMap<MemberDescription>>,
}

/// Why a description could not be built; the messages are upstream's.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ContractError(String);

impl Description {
    /// A remote contract, as upstream's `remote(name, def)`.
    pub fn remote<V, K, M, N>(
        name: impl Into<String>,
        version: i64,
        input: Value,
        variants: V,
    ) -> Result<Self, ContractError>
    where
        V: IntoIterator<Item = (K, M)>,
        K: Into<String>,
        M: IntoIterator<Item = (N, MemberDescription)>,
        N: Into<String>,
    {
        let name = name.into();
        check_header(&name, version)?;
        let mut described = PropertyMap::new();
        for (kind, members) in variants {
            let kind = kind.into();
            let members = describe(&name, &kind, Scope::Remote, members)?;
            described.insert(kind, members);
        }
        if described.is_empty() {
            return Err(ContractError(format!(
                "remote contract {name} has no variants"
            )));
        }
        Ok(Description {
            name,
            scope: Scope::Remote,
            version,
            input,
            variants: described,
        })
    }

    /// A local contract, as upstream's `local(name, def)`: one unnamed variant.
    pub fn local<M, N>(
        name: impl Into<String>,
        version: i64,
        input: Value,
        members: M,
    ) -> Result<Self, ContractError>
    where
        M: IntoIterator<Item = (N, MemberDescription)>,
        N: Into<String>,
    {
        let name = name.into();
        check_header(&name, version)?;
        let members = describe(&name, LOCAL_VARIANT, Scope::Local, members)?;
        let mut variants = PropertyMap::new();
        variants.insert(LOCAL_VARIANT, members);
        Ok(Description {
            name,
            scope: Scope::Local,
            version,
            input,
            variants,
        })
    }

    /// The description as it travels.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("a description is JSON")
    }

    /// `hash64(canonicalJson(description))`.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.to_value())
    }
}

fn check_header(name: &str, version: i64) -> Result<(), ContractError> {
    if name.is_empty() {
        return Err(ContractError("contract name is empty".to_owned()));
    }
    if version < 1 {
        return Err(ContractError(
            "contract version must be a positive integer".to_owned(),
        ));
    }
    Ok(())
}

fn describe<M, N>(
    contract: &str,
    kind: &str,
    scope: Scope,
    members: M,
) -> Result<PropertyMap<MemberDescription>, ContractError>
where
    M: IntoIterator<Item = (N, MemberDescription)>,
    N: Into<String>,
{
    let mut described = PropertyMap::new();
    for (name, member) in members {
        let name = name.into();
        if RESERVED_MEMBERS.contains(&name.as_str()) {
            return Err(ContractError(format!("member name is reserved: {name}")));
        }
        if let MemberDescription::Opaque { id } = &member {
            if scope == Scope::Remote {
                return Err(ContractError(format!(
                    "remote contracts cannot expose opaque members: {name}"
                )));
            }
            if !opaque_id(id) {
                return Err(ContractError(format!(
                    "opaque id must be name@version: {id}"
                )));
            }
        }
        if described.contains_key(&name) {
            return Err(ContractError(format!(
                "member {name} is declared twice in {contract}.{kind}"
            )));
        }
        described.insert(name, member);
    }
    Ok(described)
}

/// `/^[^@\s]+@\d+$/`.
fn opaque_id(id: &str) -> bool {
    let Some((name, version)) = id.split_once('@') else {
        return false;
    };
    !name.is_empty()
        && !name.chars().any(char::is_whitespace)
        && !version.is_empty()
        && version.bytes().all(|b| b.is_ascii_digit())
}

/// The fingerprint of a description as it was received: `hash64` of its
/// canonical JSON, extra fields included, as upstream's
/// `contractFromDescription` computes it.
pub fn fingerprint(description: &Value) -> String {
    hash64(&canonical_json(description))
}

/// Why `provider` does not cover `consumer`, or `None` when it does, as
/// upstream's `coverage`.
pub fn coverage(provider: &Description, consumer: &Description) -> Option<String> {
    if provider.name != consumer.name || provider.scope != consumer.scope {
        return Some("different type".to_owned());
    }
    if !fits_optional(Some(&consumer.input), Some(&provider.input)) {
        return Some("input".to_owned());
    }
    for (kind, consumer_members) in consumer.variants.iter() {
        let provider_members = match provider.variants.get(kind) {
            Some(members) => Some(members),
            // `provider.variants[kind]` finds `Object.prototype`'s property,
            // which has no members.
            None if js::OBJECT_PROTOTYPE_KEYS.contains(&kind) => None,
            None => return Some(format!("variant {kind} missing")),
        };
        for (name, wanted) in consumer_members.iter() {
            let offered = provider_members.and_then(|members| members.get(name));
            let Some(offered) = offered.filter(|offered| offered.kind() == wanted.kind()) else {
                return Some(format!("{kind}.{name} missing or of another kind"));
            };
            match (offered, wanted) {
                (
                    MemberDescription::Method {
                        args: offered_args,
                        result: offered_result,
                    },
                    MemberDescription::Method {
                        args: wanted_args,
                        result: wanted_result,
                    },
                ) => {
                    // Both take arguments or neither does; a consumer that
                    // expects no result accepts any.
                    if wanted_args.is_none() != offered_args.is_none() {
                        return Some(format!("{kind}.{name} arguments"));
                    }
                    if let (Some(wanted), Some(offered)) = (wanted_args, offered_args) {
                        if !fits_optional(Some(wanted), Some(offered)) {
                            return Some(format!("{kind}.{name} arguments"));
                        }
                    }
                    if let Some(wanted) = wanted_result {
                        if offered_result
                            .as_ref()
                            .is_none_or(|offered| !fits_optional(Some(offered), Some(wanted)))
                        {
                            return Some(format!("{kind}.{name} result"));
                        }
                    }
                }
                (
                    MemberDescription::State { schema: offered },
                    MemberDescription::State { schema: wanted },
                ) => {
                    if !fits_optional(Some(offered), Some(wanted)) {
                        return Some(format!("{kind}.{name} state"));
                    }
                }
                (
                    MemberDescription::Opaque { id: offered },
                    MemberDescription::Opaque { id: wanted },
                ) => {
                    if offered != wanted {
                        return Some(format!("{kind}.{name} opaque {offered} != {wanted}"));
                    }
                }
                _ => unreachable!("kinds compared above"),
            }
        }
    }
    None
}

/// A JSON object whose keys iterate in JavaScript's property order: array
/// indices first, ascending, then other keys in insertion order. Setting an
/// existing key keeps its place.
#[derive(Clone, Debug, PartialEq)]
pub struct PropertyMap<V> {
    entries: Vec<(String, V)>,
}

impl<V> Default for PropertyMap<V> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl<V> PropertyMap<V> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets `key`, returning its previous value.
    pub fn insert(&mut self, key: impl Into<String>, value: V) -> Option<V> {
        let key = key.into();
        if let Some(slot) = self
            .entries
            .iter_mut()
            .find(|(existing, _)| *existing == key)
        {
            return Some(std::mem::replace(&mut slot.1, value));
        }
        match js::property_insert_at(self.entries.iter().map(|(key, _)| key.as_str()), &key) {
            Some(at) => self.entries.insert(at, (key, value)),
            None => self.entries.push((key, value)),
        }
        None
    }

    pub fn get(&self, key: &str) -> Option<&V> {
        self.entries
            .iter()
            .find(|(existing, _)| existing == key)
            .map(|(_, value)| value)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &V)> {
        self.entries
            .iter()
            .map(|(key, value)| (key.as_str(), value))
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(key, _)| key.as_str())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl<K: Into<String>, V> FromIterator<(K, V)> for PropertyMap<V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut map = PropertyMap::new();
        for (key, value) in iter {
            map.insert(key, value);
        }
        map
    }
}

impl<V: Serialize> Serialize for PropertyMap<V> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.entries.len()))?;
        for (key, value) in &self.entries {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for PropertyMap<V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PropertyVisitor<V>(PhantomData<V>);

        impl<'de, V: Deserialize<'de>> Visitor<'de> for PropertyVisitor<V> {
            type Value = PropertyMap<V>;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                // As JSON.parse: a repeated key keeps its first place and its
                // last value.
                let mut map = PropertyMap::new();
                while let Some((key, value)) = access.next_entry::<String, V>()? {
                    map.insert(key, value);
                }
                Ok(map)
            }
        }

        deserializer.deserialize_map(PropertyVisitor(PhantomData))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn property_maps_order_indices_first() {
        let parsed: PropertyMap<i32> =
            serde_json::from_str(r#"{"x": 1, "10": 2, "9": 3, "a": 4, "x": 5}"#).unwrap();
        assert_eq!(parsed.keys().collect::<Vec<_>>(), ["9", "10", "x", "a"]);
        assert_eq!(parsed.get("x"), Some(&5));
    }

    #[test]
    fn builders_check_names_and_members() {
        let input = json!({"type": "object", "properties": {}});
        assert!(Description::remote(
            "",
            1,
            input.clone(),
            Vec::<(&str, Vec<(&str, MemberDescription)>)>::new()
        )
        .is_err());
        let error = Description::remote(
            "fixture.counter",
            1,
            input.clone(),
            [(
                "owner",
                vec![("then", MemberDescription::method(None, None))],
            )],
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "member name is reserved: then");
        let error = Description::remote(
            "fixture.counter",
            1,
            input.clone(),
            [(
                "owner",
                vec![("handle", MemberDescription::opaque("fixture.handle@1"))],
            )],
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "remote contracts cannot expose opaque members: handle"
        );
        let error = Description::local(
            "fixture.native",
            1,
            input,
            [("handle", MemberDescription::opaque("no version"))],
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "opaque id must be name@version: no version"
        );
    }
}
