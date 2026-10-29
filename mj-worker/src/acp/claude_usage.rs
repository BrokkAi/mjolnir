//! Claude's per-turn model accounting includes native agents and internal calls.
use anyhow::{Context, Result, ensure};
use mj_core::usage::{ProviderTurnUsage, TokenUsage};
use serde::Deserialize;
use std::collections::BTreeMap;

/// The response's `_meta.quota.model_usage` is fuller than its flat `usage`,
/// which covers only the main agent loop. The bridge already differences the
/// SDK's cumulative model readings, so these rows must never be differenced again.
pub(super) fn attach_provider_details(
    usage: &mut TokenUsage,
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Result<()> {
    let Some(rows) = meta
        .and_then(|meta| meta.get("quota"))
        .and_then(|quota| quota.get("model_usage"))
    else {
        return Ok(());
    };
    let rows: Vec<ModelUsage> =
        serde_json::from_value(rows.clone()).context("decode Claude per-model turn usage")?;
    if rows.is_empty() {
        return Ok(());
    }
    let mut models = BTreeMap::new();
    let mut totals = [0u64; 4];
    for row in rows {
        let count = row.token_count;
        let counters = [
            count.input_tokens,
            count.output_tokens,
            count.cached_input_tokens,
            count.cached_write_tokens,
        ];
        let total = sum(counters)?;
        ensure!(
            total == count.total_tokens,
            "inconsistent Claude model token total"
        );
        for (total, value) in totals.iter_mut().zip(counters) {
            *total = total
                .checked_add(value)
                .context("Claude model usage overflow")?;
        }
        let model = TokenUsage {
            provider_details: None,
            scope: usage.scope,
            total_tokens: total,
            input_tokens: count.input_tokens,
            output_tokens: count.output_tokens,
            thought_tokens: None, // Claude bills thinking within output.
            cached_read_tokens: Some(count.cached_input_tokens),
            cached_write_tokens: Some(count.cached_write_tokens),
        };
        ensure!(
            models.insert(row.model, model).is_none(),
            "duplicate Claude usage model"
        );
    }
    // Replace the main-loop counts with the fuller accounting, never add the
    // main loop twice. If model rows omit known usage, retain that remainder
    // in the totals: the shared attribution reader leaves it unattributed.
    let input = usage.input_tokens.max(totals[0]);
    let output = usage.output_tokens.max(totals[1]);
    let read = usage.cached_read_tokens.unwrap_or(0).max(totals[2]);
    let write = usage.cached_write_tokens.unwrap_or(0).max(totals[3]);
    let total = sum([input, output, read, write])?.max(usage.total_tokens);
    usage.input_tokens = input;
    usage.output_tokens = output;
    usage.cached_read_tokens = Some(read);
    usage.cached_write_tokens = Some(write);
    usage.total_tokens = total;
    usage.provider_details = Some(Box::new(ProviderTurnUsage {
        model_usage: models,
        ..Default::default()
    }));
    Ok(())
}

fn sum(counts: [u64; 4]) -> Result<u64> {
    counts.into_iter().try_fold(0u64, |sum, count| {
        sum.checked_add(count)
            .context("Claude token total overflow")
    })
}

#[derive(Deserialize)]
struct ModelUsage {
    model: String,
    token_count: TokenCount,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenCount {
    total_tokens: u64,
    input_tokens: u64,
    output_tokens: u64,
    cached_input_tokens: u64,
    cached_write_tokens: u64,
}
