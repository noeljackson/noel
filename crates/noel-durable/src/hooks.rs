//! Durable tasks requested at run boundaries.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The run boundary asking for durable work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPoint {
    AfterTools,
    OnYield,
    AfterRun,
}

/// Stable input to hook selectors. The task itself can read documents for
/// richer state; selectors stay synchronous and side-effect free.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    pub run_id: String,
    pub completed: bool,
    pub had_tools: bool,
}

/// One registered durable task a hook asks the harness to run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookTask {
    pub kind: String,
    pub input: Value,
}

impl HookTask {
    pub fn new(kind: impl Into<String>, input: Value) -> Self {
        Self {
            kind: kind.into(),
            input,
        }
    }
}

type Selector = Arc<dyn Fn(&RunSummary) -> Vec<HookTask> + Send + Sync>;

/// Ordered selectors for generation hook tasks. Requested tasks are
/// created and joined before the submission's wait resolves.
#[derive(Clone, Default)]
pub struct HookRegistry {
    pub(crate) after_tools: Vec<Selector>,
    pub(crate) on_yield: Vec<Selector>,
    pub(crate) after_run: Vec<Selector>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn after_tools(
        mut self,
        selector: impl Fn(&RunSummary) -> Vec<HookTask> + Send + Sync + 'static,
    ) -> Self {
        self.after_tools.push(Arc::new(selector));
        self
    }

    pub fn on_yield(
        mut self,
        selector: impl Fn(&RunSummary) -> Vec<HookTask> + Send + Sync + 'static,
    ) -> Self {
        self.on_yield.push(Arc::new(selector));
        self
    }

    pub fn after_run(
        mut self,
        selector: impl Fn(&RunSummary) -> Vec<HookTask> + Send + Sync + 'static,
    ) -> Self {
        self.after_run.push(Arc::new(selector));
        self
    }

    pub(crate) fn select(&self, point: HookPoint, summary: &RunSummary) -> Vec<HookTask> {
        let selectors = match point {
            HookPoint::AfterTools => &self.after_tools,
            HookPoint::OnYield => &self.on_yield,
            HookPoint::AfterRun => &self.after_run,
        };
        selectors
            .iter()
            .flat_map(|selector| selector(summary))
            .collect()
    }
}
