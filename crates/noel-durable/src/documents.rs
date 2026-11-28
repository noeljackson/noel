//! Embedder-defined durable documents (slice 8).

use std::marker::PhantomData;

use noel_core::DurableDocumentWrite;
use serde_json::Value;

/// A named typed document. Values are JSON on disk and keys are strings.
pub struct Document<T> {
    pub name: &'static str,
    pub version: u32,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Copy for Document<T> {}
impl<T> Clone for Document<T> {
    fn clone(&self) -> Self {
        *self
    }
}

pub const fn define_doc<T>(name: &'static str, version: u32) -> Document<T> {
    Document {
        name,
        version,
        _marker: PhantomData,
    }
}

/// A document batch is kept in memory until `commit_documents`, so callers
/// can derive several writes and persist them in one journal record.
#[derive(Debug, Clone, Default)]
pub struct DocumentBatch {
    pub(crate) writes: Vec<DurableDocumentWrite>,
}

impl DocumentBatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set<T: serde::Serialize>(
        mut self,
        document: &Document<T>,
        key: impl Into<String>,
        value: &T,
    ) -> Self {
        self.writes.push(DurableDocumentWrite {
            document: document.name.to_owned(),
            key: key.into(),
            value: Some(serde_json::to_value(value).unwrap_or(Value::Null)),
        });
        self
    }

    pub fn delete<T>(mut self, document: &Document<T>, key: impl Into<String>) -> Self {
        self.writes.push(DurableDocumentWrite {
            document: document.name.to_owned(),
            key: key.into(),
            value: None,
        });
        self
    }

    pub fn is_empty(&self) -> bool {
        self.writes.is_empty()
    }
}

pub(crate) fn decode<T: serde::de::DeserializeOwned>(value: Option<Value>) -> Option<T> {
    serde_json::from_value(value?).ok()
}
