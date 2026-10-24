//! Standard base token API rates checked 2026-10-14, USD per million tokens.
//! Sources: https://developers.openai.com/api/docs/pricing
//! https://developers.openai.com/api/docs/models/gpt-6-sol
//! https://platform.claude.com/docs/en/about-claude/pricing
//! https://docs.z.ai/guides/overview/pricing
//! https://api-docs.deepseek.com/quick_start/pricing/
//! https://docs.x.ai/developers/models/grok-4.7
//! https://docs.x.ai/developers/pricing
//! https://platform.kimi.ai/docs/pricing/chat
//! Default 5-minute writes; excludes tools, speed/region/context premiums.
use anyhow::{Context, Result};
use mj_core::storage::UsageCounterTotal;
use std::collections::BTreeMap;

// One rate unit is 0.001 USD per million, or ten USD ticks per token.
fn rates(model: &str) -> Option<[u64; 4]> {
    let lowercase = model.to_ascii_lowercase();
    let model = lowercase.as_str();
    // Known provider-qualified ids and dated snapshots share a base rate.
    let model = model
        .strip_prefix("global.")
        .or_else(|| model.strip_prefix("us."))
        .or_else(|| model.strip_prefix("eu."))
        .unwrap_or(model);
    let model = model
        .strip_prefix("openai.")
        .or_else(|| model.strip_prefix("anthropic."))
        .unwrap_or(model);
    let model = model
        .strip_prefix("openai/")
        .or_else(|| model.strip_prefix("anthropic/"))
        .or_else(|| model.strip_prefix("z-ai/"))
        .or_else(|| model.strip_prefix("deepseek/"))
        .or_else(|| model.strip_prefix("x-ai/"))
        .or_else(|| model.strip_prefix("moonshotai/"))
        .unwrap_or(model);
    let model = model.strip_suffix(":0").unwrap_or(model);
    // Claude's context hint (`[1m]`); context premiums are excluded.
    let model = match model.strip_suffix(']').and_then(|m| m.rsplit_once('[')) {
        Some((base, _)) => base,
        None => model,
    };
    let base = model.strip_suffix("-latest").unwrap_or(model);
    let base = if let Some((name, date)) = base.rsplit_once('-') {
        if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) {
            name
        } else {
            base
        }
    } else {
        base
    };
    // [ordinary input, output, cache read, cache write]. Exact aliases only.
    Some(match base {
        "gpt-6.1-sol" => [2000, 10000, 100, 2500],
        "gpt-6-sol" => [2000, 10000, 200, 2500],
        "gpt-6-astra" => [10000, 50000, 1000, 12500],
        "gpt-6-luna" => [100, 500, 10, 125],
        "gpt-5.6-sol" => [4000, 20000, 400, 5000],
        "claude-fable-5-1" | "claude-mythos-5-1" => [10000, 50000, 250, 12500],
        "claude-opus-5-5" => [4000, 20000, 200, 5000],
        "claude-sonnet-5-5" => [2000, 10000, 100, 2500],
        "claude-haiku-5-5" => [100, 500, 10, 125],
        "claude-fable-5" | "claude-mythos-5" => [10000, 50000, 1000, 12500],
        "claude-opus-5" | "claude-opus-4-8" | "claude-opus-4-7" | "claude-opus-4-6"
        | "claude-opus-4-5" => [5000, 25000, 500, 6250],
        "claude-opus-4" | "claude-opus-4-1" => [15000, 75000, 1500, 18750],
        "claude-sonnet-5" => [2000, 10000, 200, 2500],
        "claude-sonnet-4" | "claude-sonnet-4-5" | "claude-sonnet-4-6" => [3000, 15000, 300, 3750],
        "claude-haiku-4-5" => [1000, 5000, 100, 1250],
        "claude-3-5-haiku" => [800, 4000, 80, 1000],
        "glm-5.3" | "glm-5.2" | "glm-5.1" => [1400, 4400, 260, 0],
        "glm-5" => [1000, 3200, 200, 0],
        "glm-4.7" | "glm-4.6" | "glm-4.5" => [600, 2200, 110, 0],
        "glm-5.3-flash" => [150, 500, 30, 0],
        "glm-5.3-flashx" => [370, 1250, 75, 0],
        "glm-4.7-flash" | "glm-4.5-flash" => [0, 0, 0, 0],
        // Peak rates: a stable estimate without reconstructing holidays/request times.
        "deepseek-flash" | "deepseek-v4-flash" | "deepseek-v4-flash-vision-exp" => {
            [300, 1200, 6, 300]
        }
        "deepseek-v4-pro" => [1320, 3960, 44, 1320],
        "grok-4.7" | "grok-4.6" => [2000, 6000, 500, 2000],
        "grok-4.5" => [2000, 6000, 300, 2000],
        "grok-4.3"
        | "grok-4.20-multi-agent-0309"
        | "grok-4.20-0309-reasoning"
        | "grok-4.20-0309-non-reasoning" => [1250, 2500, 200, 1250],
        "grok-build-0.1" => [1000, 2000, 200, 1000],
        "kimi-k3" | "kimi-code/k3" => [3000, 15000, 300, 3000],
        "kimi-k2.7-code" => [950, 4000, 190, 950],
        "kimi-k2.7-code-highspeed" => [1900, 8000, 380, 1900],
        "kimi-k2.6" => [950, 4000, 160, 950],
        _ => return None,
    })
}

pub(super) fn is_priced(model: &str) -> bool {
    rates(model).is_some()
}

pub(super) fn estimate(
    model: Option<&str>,
    totals: &BTreeMap<String, UsageCounterTotal>,
) -> Result<Option<u64>> {
    let Some(rates) = model.and_then(rates) else {
        return Ok(None);
    };
    let count = |name: &str| totals.get(name).map(|counter| counter.tokens);
    let (Some(input), Some(output), Some(total)) = (
        count("input_tokens"),
        count("output_tokens"),
        count("total_tokens"),
    ) else {
        return Ok(None);
    };
    let read = count("cached_read_tokens").unwrap_or(0);
    let write = count("cached_write_tokens").unwrap_or(0);
    let caches = read.checked_add(write).context("cache token overflow")?;
    let ordinary = if input.checked_add(output) == Some(total) {
        // Codex, Muse, Kimi and Grok include cache tokens in input.
        let Some(ordinary) = input.checked_sub(caches) else {
            return Ok(None);
        };
        ordinary
    } else if input
        .checked_add(output)
        .and_then(|v| v.checked_add(caches))
        == Some(total)
    {
        // Claude's input is the uncached count; caches are separate tallies.
        input
    } else {
        return Ok(None);
    };
    let cost = [ordinary, output, read, write]
        .into_iter()
        .zip(rates)
        .try_fold(0u64, |sum, (tokens, rate)| {
            tokens
                .checked_mul(rate)
                .and_then(|v| v.checked_mul(10))
                .and_then(|v| sum.checked_add(v))
                .context("API cost overflow")
        })?;
    Ok(Some(cost))
}
