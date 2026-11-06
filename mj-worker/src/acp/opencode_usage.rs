//! OpenCode's prompt-response usage, converted to the convention `mj usage` totals use.
use anyhow::{Context, Result, ensure};
use mj_core::usage::{TokenUsage, UsageScope};

const USAGE_SCOPE_KEY: &str = "mjolnir.dev/usage-scope";

/// The OpenCode version whose prompt response was checked live to report the
/// whole turn (every model step of the prompt). Another version, for example
/// one found on PATH, may report only the last step, so its report keeps the
/// adapter's undeclared scope.
const WHOLE_TURN_VERSION: &str = "2.2.1";

/// Whether this OpenCode, as it named itself in `initialize`, reports whole turns.
pub fn reports_whole_turns(agent_version: Option<&str>) -> bool {
    agent_version == Some(WHOLE_TURN_VERSION)
}

/// OpenCode 2.2.1 sums its model steps into one per-prompt figure but does not
/// declare a scope, and it counts differently from the rest of mj: `input`
/// excludes cache reads and writes, and `output` excludes reasoning. Fold both
/// back in so input and output are the full counts, then mark the report as a
/// whole turn. A report that does not add up to its own total is left
/// unspecified and unchanged, so it never enters the totals or the pricing.
/// A report that declares any scope, even `unspecified`, is left as declared.
pub fn normalize(
    usage: &mut TokenUsage,
    usage_meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Result<()> {
    if usage_meta.is_some_and(|meta| meta.contains_key(USAGE_SCOPE_KEY)) {
        return Ok(());
    }
    let cache = usage
        .cached_read_tokens
        .unwrap_or(0)
        .checked_add(usage.cached_write_tokens.unwrap_or(0))
        .context("OpenCode cache token overflow")?;
    let input = usage
        .input_tokens
        .checked_add(cache)
        .context("OpenCode input token overflow")?;
    let output = usage
        .output_tokens
        .checked_add(usage.thought_tokens.unwrap_or(0))
        .context("OpenCode output token overflow")?;
    ensure!(
        input.checked_add(output) == Some(usage.total_tokens),
        "OpenCode reported {input} input and {output} output tokens including cache and reasoning, \
         but {} in total",
        usage.total_tokens
    );
    usage.input_tokens = input;
    usage.output_tokens = output;
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

    /// `usage` of a recorded OpenCode 2.2.1 `session/prompt` response (a
    /// three-step turn with reasoning and cache reads), as it crosses ACP.
    const RECORDED: &str = r#"{"inputTokens":15434,"outputTokens":468,"totalTokens":43649,
        "thoughtTokens":99,"cachedReadTokens":27648}"#;

    fn parse(
        report: &str,
    ) -> (
        TokenUsage,
        Option<serde_json::Map<String, serde_json::Value>>,
    ) {
        let usage: Usage = serde_json::from_str(report).expect("ACP usage");
        let meta = usage.meta.clone();
        (TokenUsage::from_acp(HarnessKind::OpenCode, usage), meta)
    }

    // Moving the pin must come with a fresh live check of the prompt
    // response's usage scope; then update `WHOLE_TURN_VERSION`.
    #[test]
    fn the_whole_turn_version_is_the_pinned_opencode() {
        assert_eq!(
            WHOLE_TURN_VERSION,
            mj_core::harness_runtime::OPENCODE_VERSION
        );
        assert!(reports_whole_turns(Some(WHOLE_TURN_VERSION)));
        assert!(!reports_whole_turns(Some("1.14.0")));
        assert!(!reports_whole_turns(None));
    }

    #[test]
    fn the_recorded_opencode_report_becomes_a_priceable_turn_and_a_mismatch_stays_out() {
        let (mut usage, meta) = parse(RECORDED);
        assert_eq!(usage.scope, UsageScope::Unspecified);
        normalize(&mut usage, meta.as_ref()).expect("recorded report adds up");
        assert_eq!(usage.scope, UsageScope::Turn);
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, usage.total_tokens),
            (43_082, 567, 43_649)
        );
        assert_eq!(
            (usage.thought_tokens, usage.cached_read_tokens),
            (Some(99), Some(27_648))
        );

        // The shape pricing accepts: full input and output, cache as a subset.
        let totals: BTreeMap<_, _> = [
            ("input_tokens", usage.input_tokens),
            ("output_tokens", usage.output_tokens),
            ("total_tokens", usage.total_tokens),
            ("cached_read_tokens", 27_648),
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
        assert!(
            pricing::estimate(Some("deepseek-v4-flash"), &totals)
                .expect("estimate")
                .is_some()
        );

        let (mut mismatched, meta) = parse(&RECORDED.replace("43649", "43650"));
        let before = mismatched.clone();
        assert!(normalize(&mut mismatched, meta.as_ref()).is_err());
        assert_eq!(mismatched, before);
        assert_eq!(mismatched.scope, UsageScope::Unspecified);

        // An adapter that declares the report incomplete is believed, even
        // when its counters add up.
        let declared = RECORDED.replace(
            "\"cachedReadTokens\":27648}",
            "\"cachedReadTokens\":27648,\"_meta\":{\"mjolnir.dev/usage-scope\":\"unspecified\"}}",
        );
        let (mut incomplete, meta) = parse(&declared);
        let before = incomplete.clone();
        normalize(&mut incomplete, meta.as_ref()).expect("declared scope is kept");
        assert_eq!(incomplete, before);
        assert_eq!(incomplete.scope, UsageScope::Unspecified);
    }
}
