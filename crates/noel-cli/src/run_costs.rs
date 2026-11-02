//! `/runs` and `/cost` (spec §9.8): each run's tokens and what they cost
//! at the active model's catalog prices, and a forecast of the next work.

use super::*;

/// A run's cost from the model's prices (per million tokens), USD: its
/// input and output tokens at the list prices. Cache reads and writes and
/// price tiers are not counted, so it is an estimate. `None` when the
/// model has no prices in the catalog.
fn run_cost(
    definition: &noel_config::ModelDefinition,
    usage: &noel_core::SessionUsage,
) -> Option<f64> {
    let cost = definition.cost.as_ref()?;
    let per_million = |tokens: u64, price: f64| tokens as f64 * price / 1_000_000.0;
    Some(
        per_million(usage.input_tokens, cost.input.as_f64().unwrap_or(0.0))
            + per_million(usage.output_tokens, cost.output.as_f64().unwrap_or(0.0)),
    )
}

fn format_usd(cost: f64) -> String {
    if cost == 0.0 {
        "free".to_string()
    } else if cost >= 1.0 {
        format!("${cost:.2}")
    } else if cost >= 0.01 {
        format!("${cost:.3}")
    } else {
        format!("${cost:.5}")
    }
}

/// `/runs`: every finished or interrupted run with its tokens, request
/// count and cost at the session's active model's prices, newest first.
pub(crate) fn format_run_costs(runtime: &Runtime, config: &LoadedConfig) -> String {
    let session = runtime.session();
    let model = session.active_model.as_ref();
    let definition = model.and_then(|model| model_definition(config, model));
    let Some(definition) = definition else {
        return "runs: no active model to price against".to_string();
    };
    if session.runs.is_empty() {
        return format!(
            "runs: none yet; costs at {}/{}",
            definition.provider, definition.id
        );
    }
    let mut lines = vec![format!(
        "runs (estimated at {}/{} list prices):",
        definition.provider, definition.id
    )];
    let mut total_input = 0u64;
    let mut total_output = 0u64;
    for run in session.runs.iter().rev() {
        let usage = session.run_usage.get(&run.id);
        match usage {
            Some(usage) => {
                total_input += usage.input_tokens;
                total_output += usage.output_tokens;
                let cost = run_cost(&definition, usage)
                    .map(format_usd)
                    .unwrap_or_else(|| "n/a".to_string());
                lines.push(format!(
                    "  {} · {} in / {} out · {} request{} · {} · {}",
                    match run.status {
                        noel_core::RunStatus::Running => "running",
                        noel_core::RunStatus::Interrupted => "interrupted",
                        noel_core::RunStatus::Completed => "completed",
                        noel_core::RunStatus::Failed => "failed",
                        noel_core::RunStatus::Aborted => "aborted",
                    },
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.requests,
                    if usage.requests == 1 { "" } else { "s" },
                    cost,
                    &run.id[..run.id.len().min(8)],
                ));
            }
            None => lines.push(format!(
                "  (no usage recorded) · {}",
                &run.id[..run.id.len().min(8)]
            )),
        }
    }
    let total = noel_core::SessionUsage {
        input_tokens: total_input,
        output_tokens: total_output,
        requests: 0,
    };
    if let Some(cost) = run_cost(&definition, &total) {
        lines.push(format!(
            "  total: {total_input} in / {total_output} out · {}",
            format_usd(cost)
        ));
    }
    lines.join("\n")
}

/// `/cost`: what the queued work plus this context will approximately
/// cost, from the average cost of the recent runs and the session's
/// context estimate.
pub(crate) fn format_cost_forecast(runtime: &Runtime, config: &LoadedConfig) -> String {
    let session = runtime.session();
    let model = session.active_model.as_ref();
    let definition = model.and_then(|model| model_definition(config, model));
    let Some(definition) = definition else {
        return "cost: no active model to price against".to_string();
    };
    let queue = session.queue.len();
    let recent: Vec<&noel_core::SessionUsage> = session
        .runs
        .iter()
        .rev()
        .filter_map(|run| session.run_usage.get(&run.id))
        .take(5)
        .collect();
    let mut lines = vec![format!(
        "cost forecast (estimated at {}/{} list prices):",
        definition.provider, definition.id
    )];
    lines.push(format!(
        "  context: ~{} tokens in play",
        session
            .messages
            .iter()
            .map(|message| (message.content.len() + message.thinking.len()) / 4)
            .sum::<usize>()
    ));
    match recent.len() {
        0 => {
            lines.push("  no finished runs to average; the first run sets the baseline".to_string())
        }
        count => {
            let average: f64 = recent
                .iter()
                .filter_map(|usage| run_cost(&definition, usage))
                .sum::<f64>()
                / count as f64;
            lines.push(format!(
                "  recent runs: {} averaged {}",
                count,
                format_usd(average)
            ));
            if queue == 0 {
                lines.push("  queue empty; the next prompt costs about the average".to_string());
            } else {
                lines.push(format!(
                    "  queue: {} follow-up{} ≈ {}",
                    queue,
                    if queue == 1 { "" } else { "s" },
                    format_usd(average * queue as f64)
                ));
            }
        }
    }
    lines.join("\n")
}
