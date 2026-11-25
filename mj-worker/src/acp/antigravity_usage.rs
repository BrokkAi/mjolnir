//! Antigravity's prompt-response usage, verified as whole-turn accounting.
use anyhow::{Context, Result, ensure};
use mj_core::usage::{TokenUsage, UsageScope};

const USAGE_SCOPE_KEY: &str = "mjolnir.dev/usage-scope";

/// The Antigravity (`agy acp`) versions whose prompt responses were verified
/// live against their per-step transcripts to report the whole turn (every
/// model step of the prompt). Another version, for example one found on PATH,
/// keeps the adapter's undeclared scope until checked.
const WHOLE_TURN_VERSIONS: [&str; 2] = ["1.5.11", "1.5.15"];

/// Whether this Antigravity ACP server, as it named itself in `initialize`,
/// reports whole turns.
pub fn reports_whole_turns(agent_version: Option<&str>) -> bool {
    agent_version.is_some_and(|version| WHOLE_TURN_VERSIONS.contains(&version))
}

/// Antigravity 1.5.11 and 1.5.15 sum every model step of a prompt into one per-turn
/// `Usage` report without declaring `mjolnir.dev/usage-scope`. Its counters
/// match Claude's convention: `input_tokens` is uncached input, cache tokens
/// are separate tallies, `output_tokens` includes `thought_tokens`, and
/// `input + output + cached_read + cached_write == total`. Verify those
/// invariants before marking the report as `UsageScope::Turn`; a mismatch
/// stays unspecified and unchanged so it never enters totals or pricing.
/// A report that declares any scope, even `unspecified`, is left as declared.
pub fn normalize(
    usage: &mut TokenUsage,
    usage_meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Result<()> {
    if usage_meta.is_some_and(|meta| meta.contains_key(USAGE_SCOPE_KEY)) {
        return Ok(());
    }
    ensure!(
        usage
            .thought_tokens
            .is_none_or(|thought| thought <= usage.output_tokens),
        "Antigravity thought tokens ({}) exceed output tokens ({})",
        usage.thought_tokens.unwrap_or(0),
        usage.output_tokens,
    );
    let cache = usage
        .cached_read_tokens
        .unwrap_or(0)
        .checked_add(usage.cached_write_tokens.unwrap_or(0))
        .context("Antigravity cache token overflow")?;
    let total = usage
        .input_tokens
        .checked_add(usage.output_tokens)
        .and_then(|v| v.checked_add(cache))
        .context("Antigravity usage total overflow")?;
    ensure!(
        total == usage.total_tokens,
        "Antigravity reported {} input, {} output, and {} cache tokens ({total}), \
         but {} in total",
        usage.input_tokens,
        usage.output_tokens,
        cache,
        usage.total_tokens,
    );
    usage.scope = UsageScope::Turn;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::v1::schema::Usage;
    use mj_core::config::HarnessKind;
    use mj_core::storage::UsageCounterTotal;
    use mj_core::usage::pricing;
    use std::collections::BTreeMap;

    /// `usage` of a recorded Antigravity 1.5.11 `session/prompt` response
    /// (a four-step Gemini 4 Neon turn verified against `transcript.jsonl`).
    const RECORDED: &str = r#"{"totalTokens":144764,"inputTokens":40053,"outputTokens":3500,
        "thoughtTokens":1830,"cachedReadTokens":101211,"cachedWriteTokens":0}"#;

    fn parse(
        report: &str,
    ) -> (
        TokenUsage,
        Option<serde_json::Map<String, serde_json::Value>>,
    ) {
        let usage: Usage = serde_json::from_str(report).expect("ACP usage");
        let meta = usage.meta.clone();
        (TokenUsage::from_acp(HarnessKind::Antigravity, usage), meta)
    }

    // Moving the pin must come with a fresh live check of the prompt
    // response's usage scope; then add the new version to `WHOLE_TURN_VERSIONS`.
    #[test]
    fn the_pinned_antigravity_is_a_whole_turn_version() {
        assert!(WHOLE_TURN_VERSIONS.contains(&mj_core::harness_runtime::ANTIGRAVITY_VERSION));
        assert!(reports_whole_turns(Some(
            mj_core::harness_runtime::ANTIGRAVITY_VERSION
        )));
        assert!(!reports_whole_turns(Some("1.5.10")));
        assert!(!reports_whole_turns(None));
    }

    #[test]
    fn the_recorded_antigravity_report_becomes_a_priceable_turn_and_a_mismatch_stays_out() {
        let (mut usage, meta) = parse(RECORDED);
        assert_eq!(usage.scope, UsageScope::Unspecified);
        normalize(&mut usage, meta.as_ref()).expect("recorded report adds up");
        assert_eq!(usage.scope, UsageScope::Turn);
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, usage.total_tokens),
            (40_053, 3_500, 144_764)
        );
        assert_eq!(
            (
                usage.thought_tokens,
                usage.cached_read_tokens,
                usage.cached_write_tokens
            ),
            (Some(1_830), Some(101_211), Some(0))
        );

        let totals: BTreeMap<_, _> = [
            ("input_tokens", usage.input_tokens),
            ("output_tokens", usage.output_tokens),
            ("total_tokens", usage.total_tokens),
            ("cached_read_tokens", 101_211),
            ("cached_write_tokens", 0),
        ]
        .into_iter()
        .map(|(name, tokens)| {
            (
                name.to_string(),
                UsageCounterTotal {
                    tokens,
                    reported_turns: 1,
                },
            )
        })
        .collect();
        // 40,053 * $3/M + 3,500 * $15/M + 101,211 * $0.15/M = $0.18784065
        assert_eq!(
            pricing::estimate(Some("gemini-4-neon"), &totals).expect("estimate"),
            Some(1_878_406_500)
        );

        for bad in [
            RECORDED.replace("144764", "144765"),
            RECORDED.replace("1830", "3501"),
        ] {
            let (mut mismatched, meta) = parse(&bad);
            let before = mismatched.clone();
            assert!(normalize(&mut mismatched, meta.as_ref()).is_err());
            assert_eq!(mismatched, before);
            assert_eq!(mismatched.scope, UsageScope::Unspecified);
        }

        // An adapter that declares the report incomplete is believed, even
        // when its counters add up.
        let declared = RECORDED.replace(
            "\"cachedWriteTokens\":0}",
            "\"cachedWriteTokens\":0,\"_meta\":{\"mjolnir.dev/usage-scope\":\"unspecified\"}}",
        );
        let (mut incomplete, meta) = parse(&declared);
        let before = incomplete.clone();
        normalize(&mut incomplete, meta.as_ref()).expect("declared scope is kept");
        assert_eq!(incomplete, before);
        assert_eq!(incomplete.scope, UsageScope::Unspecified);
    }
}
