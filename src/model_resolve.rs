//! Resolve agent-selectable model options — and LMArena leaderboard rows — to a
//! common normalized *match key*, so a strength score can be joined onto a model
//! in the picker.
//!
//! Matching is deliberately **exact** on the normalized key: we never
//! fuzzy-match or substring-match. An option that produces no key present in the
//! score catalog yields no score (the picker shows `—`), per the design's "if we
//! don't know, show no score" rule. A wrong number is worse than none.
//!
//! ## The shape of the problem
//!
//! Agents name models in wildly different ways, and the identifying detail isn't
//! always in the id:
//!
//! - **Anvil** embeds a backend + provider path + AWS suffix:
//!   `bedrock::us.anthropic.claude-opus-4-8` → `anthropic` / `claude-opus-4-8`.
//! - **claude-acp** uses bare aliases whose version lives in the *description*:
//!   value `opus`, name `Opus`, description `Opus 4.8 with 1M context · …`.
//! - **Aggregators** (`opencode`, …) use `provider/model`.
//! - **Closed** agents (`devin`, …) use opaque labels that resolve to nothing.
//!
//! So every signal we have — the option id, the display name, and the leading
//! phrase of the description — is fed through the resolver.
//!
//! ## How a key is formed
//!
//! Every candidate is reduced by [`alnum_lower`] (lowercased, non-alphanumerics
//! dropped), so `"Claude Opus 4.8"`, `claude-opus-4-8` and `claude_opus_4.8` all
//! collapse to `claudeopus48`. Each (provider, model) pair emits a
//! provider-qualified key and a bare key. Two extra tricks bridge the naming gap:
//!
//! - **Progressive suffix stripping** expands a model into candidates
//!   (`…-20251101-v1` → `…-20251101` → `claude-opus-4-5`), tried most-specific
//!   first, because version/date suffixes differ between agents and LMArena.
//! - **Brand stripping** also emits a provider-qualified key with the brand word
//!   removed (`claude-opus-4-8` → `anthropic/opus48`), so an agent that names a
//!   model just `Opus 4.8` still joins the row LMArena calls `claude-opus-4-8`.

use std::collections::HashMap;

/// A normalized join key (see module docs). Compared by exact equality only.
pub type MatchKey = String;

/// How directly a leaderboard row produced a key. Exact keys are generated from
/// the row's published model name; aliases come from progressively stripped
/// suffixes such as `-high`, `-preview`, or a date. Exact rows must beat aliases
/// so a high-vote variant cannot overwrite a base model's own score.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchSpecificity {
    Alias,
    Exact,
}

/// Single-vendor agents whose model option ids are bare vendor-native names
/// (provider implied by the agent). Aggregators are absent (their ids carry
/// `provider/model`); closed agents are absent (no public provider).
const SINGLE_VENDOR_MAP: &[(&str, &str)] = &[
    ("claude-acp", "anthropic"),
    ("codex-acp", "openai"),
    ("gemini", "google"),
    ("qwen-code", "alibaba"),
    ("kimi", "moonshotai"),
    ("glm-acp-agent", "zhipuai"),
    ("grok-build", "xai"),
    ("mistral-vibe", "mistralai"),
];

/// Reconcile differing vendor slugs (agent ids / models.dev / LMArena's
/// `organization` column) to one canonical provider token. Keys are themselves
/// alnum-normalized, so `"Google DeepMind"` matches `googledeepmind`.
const PROVIDER_ALIAS: &[(&str, &str)] = &[
    ("googledeepmind", "google"),
    ("deepmind", "google"),
    ("qwen", "alibaba"),
    ("alibabacloud", "alibaba"),
    ("moonshot", "moonshotai"),
    ("zhipu", "zhipuai"),
    ("zai", "zhipuai"),
    ("mistral", "mistralai"),
    ("metallama", "meta"),
];

/// Canonical provider tokens we recognize as namespace segments inside an id
/// (e.g. the `anthropic` in `bedrock::us.anthropic.claude-opus-4-8`). Region
/// tokens like `us`/`eu` are deliberately absent, so the scan skips them.
const KNOWN_PROVIDERS: &[&str] = &[
    "anthropic",
    "openai",
    "google",
    "alibaba",
    "deepseek",
    "moonshotai",
    "xai",
    "baidu",
    "amazon",
    "meta",
    "mistralai",
    "bytedance",
    "xiaomi",
    "minimax",
    "writer",
    "cohere",
    "nvidia",
    "microsoft",
    "zhipuai",
];

/// The brand word a provider prefixes its models with. Used to bridge agents
/// that drop the brand (claude-acp's `Opus 4.8`) and rows that keep it
/// (`claude-opus-4-8`).
fn brand_words(provider: &str) -> &'static [&'static str] {
    match provider {
        "anthropic" => &["claude"],
        "openai" => &["gpt"],
        "google" => &["gemini", "gemma"],
        "alibaba" => &["qwen"],
        "zhipuai" => &["glm"],
        "xai" => &["grok"],
        "moonshotai" => &["kimi"],
        "deepseek" => &["deepseek"],
        "baidu" => &["ernie"],
        "meta" => &["llama"],
        "mistralai" => &["mistral"],
        _ => &[],
    }
}

/// Keep only ASCII alphanumerics, lowercased — collapses separator/case
/// differences so equivalent names compare equal.
fn alnum_lower(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Canonical provider token for a raw vendor/segment string, or `None` if it
/// isn't a recognized provider (e.g. a region or a model word).
fn canonical_provider(seg: &str) -> Option<String> {
    let key = alnum_lower(seg);
    if key.is_empty() {
        return None;
    }
    for (alias, canon) in PROVIDER_ALIAS {
        if key == *alias {
            return Some((*canon).to_string());
        }
    }
    KNOWN_PROVIDERS.contains(&key.as_str()).then_some(key)
}

/// Normalize a provider for a key: alias-folded, alnum. Unknown providers pass
/// through as their alnum form (still useful as a qualified key).
fn provider_norm(p: &str) -> String {
    canonical_provider(p).unwrap_or_else(|| alnum_lower(p))
}

/// Drop a context/variant decoration: a bracketed suffix like `[1m]`, a
/// parenthetical like `(latest)`/`(Fast)`, and anything after a `:` (e.g.
/// `:thinking`, the AWS `:0`, an ollama `:4b` tag).
fn strip_variant(s: &str) -> &str {
    let s = s.split('[').next().unwrap_or(s);
    let s = s.split('(').next().unwrap_or(s);
    s.split(':').next().unwrap_or(s).trim()
}

/// Remove one trailing non-identity token: an AWS version (`-v1`), a compact
/// date (`-20251101`), a reasoning-effort word (`-high`/`-low`/…), or a release
/// qualifier (`-preview`). Lets an agent's `gpt-5.4-mini` reach LMArena's
/// `gpt-5.4-mini-high`, and `qwen3.7-max` reach `qwen3.7-max-preview`. Returns
/// the input unchanged when none applies.
fn strip_one_suffix(s: &str) -> &str {
    if let Some((head, tail)) = s.rsplit_once('-') {
        let is_version = tail
            .strip_prefix('v')
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        let is_date = (6..=8).contains(&tail.len()) && tail.chars().all(|c| c.is_ascii_digit());
        let is_droppable = matches!(
            tail.to_ascii_lowercase().as_str(),
            // reasoning effort      | release qualifier
            "high" | "low" | "medium" | "minimal" | "xhigh" | "preview"
        );
        if is_version || is_date || is_droppable {
            return head;
        }
    }
    s
}

/// Progressively stripped model-name candidates, most-specific first:
/// `claude-opus-4-5-20251101-v1:0` → `[claude-opus-4-5-20251101-v1,
/// claude-opus-4-5-20251101, claude-opus-4-5]`. Tried in order so a dated
/// LMArena row matches before the bare family name.
fn model_candidates(model: &str) -> Vec<String> {
    let base = strip_variant(model).trim_end_matches('-');
    if base.is_empty() {
        return Vec::new();
    }
    let mut out = vec![base.to_string()];
    let mut cur = base;
    loop {
        let next = strip_one_suffix(cur);
        if next == cur || next.is_empty() {
            break;
        }
        cur = next;
        out.push(cur.to_string());
    }
    out
}

/// The leading model phrase of a human description, e.g.
/// `"Opus 4.8 with 1M context · Best for everyday…"` → `"Opus 4.8"`. The version
/// agents bury in the description is often the only place it appears.
fn leading_phrase(description: &str) -> Option<String> {
    let mut s = description.trim();
    for sep in ["·", "(", "—", ",", " with ", " for ", " - ", "·"] {
        if let Some(idx) = s.find(sep) {
            s = s[..idx].trim();
        }
    }
    (!s.is_empty()).then(|| s.to_string())
}

fn single_vendor_provider(agent_id: &str) -> Option<&'static str> {
    SINGLE_VENDOR_MAP
        .iter()
        .find(|(a, _)| *a == agent_id)
        .map(|(_, p)| *p)
}

/// The keys for one (provider, model) pair: provider-qualified, bare, and a
/// provider-qualified brand-stripped alias. Bare brand-stripped keys are
/// intentionally omitted (too collision-prone) and short remainders skipped.
fn keys_for(provider: Option<&str>, model: &str) -> Vec<MatchKey> {
    let m = alnum_lower(model);
    if m.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let pn = provider.map(provider_norm).filter(|p| !p.is_empty());
    if let Some(p) = &pn {
        out.push(format!("{p}/{m}"));
    }
    out.push(m.clone());
    if let Some(p) = &pn {
        for brand in brand_words(p) {
            if let Some(rest) = m.strip_prefix(brand)
                && rest.len() >= 4
            {
                out.push(format!("{p}/{rest}"));
            }
        }
    }
    out
}

fn dedup_preserving(keys: Vec<MatchKey>) -> Vec<MatchKey> {
    let mut seen = std::collections::HashSet::new();
    keys.into_iter()
        .filter(|k| seen.insert(k.clone()))
        .collect()
}

fn dedup_ranked_preserving(
    keys: Vec<(MatchKey, MatchSpecificity)>,
) -> Vec<(MatchKey, MatchSpecificity)> {
    let mut out: Vec<(MatchKey, MatchSpecificity)> = Vec::new();
    for (key, specificity) in keys {
        if let Some((_, existing)) = out
            .iter_mut()
            .find(|(existing_key, _)| existing_key == &key)
        {
            *existing = (*existing).max(specificity);
        } else {
            out.push((key, specificity));
        }
    }
    out
}

/// Split a model id into (provider, model-name), stripping any `backend::`
/// prefix and detecting an embedded provider among `.`-separated segments.
fn parse_id(agent_id: &str, option_id: &str) -> (Option<String>, String) {
    // Strip a `backend::` prefix (Anvil: `bedrock::…`, `codex::…`, `ollama::…`).
    let after_backend = option_id.rsplit("::").next().unwrap_or(option_id);

    // Slash-separated namespace (aggregators: `provider/model`, or a router
    // path like `openrouter/anthropic/claude-opus-4.8`). Prefer the last
    // `/`-segment that is a known provider; the model is everything after it.
    // Otherwise treat the first segment as the provider.
    if after_backend.contains('/') {
        let segments: Vec<&str> = after_backend.split('/').collect();
        if let Some(idx) = segments
            .iter()
            .rposition(|seg| canonical_provider(seg).is_some())
        {
            let model = segments[idx + 1..].join("/");
            if !model.is_empty() {
                return (canonical_provider(segments[idx]), model);
            }
        }
        if let Some((prov, model)) = after_backend.split_once('/') {
            return (Some(provider_norm(prov)), model.to_string());
        }
    }

    // Dotted namespace (`us.anthropic.claude-opus-4-8`): the provider is the last
    // segment that is a known provider; everything after it is the model. Falls
    // back to the whole string when no provider segment is present (e.g.
    // `gpt-5.5`, where the dot is a version separator).
    let segments: Vec<&str> = after_backend.split('.').collect();
    if segments.len() > 1
        && let Some(idx) = segments
            .iter()
            .rposition(|seg| canonical_provider(seg).is_some())
    {
        let model = segments[idx + 1..].join(".");
        if !model.is_empty() {
            return (canonical_provider(segments[idx]), model);
        }
    }

    // Bare id: provider implied by the agent (single-vendor) or unknown.
    (
        single_vendor_provider(agent_id).map(str::to_string),
        after_backend.to_string(),
    )
}

/// Ordered candidate match keys for an agent's selectable model option, drawn
/// from the option id (`value`), the display `name`, and the leading phrase of
/// the `description`. The caller tries them against the score catalog in order;
/// the first hit wins. Empty when nothing principled can be formed (→ no score).
///
/// `overrides` maps `"{agent_id}/{value}"` → canonical `"provider/model"` and
/// takes precedence over all heuristics.
pub fn agent_keys(
    agent_id: &str,
    value: &str,
    name: &str,
    description: &str,
    overrides: &HashMap<String, String>,
) -> Vec<MatchKey> {
    // 1. Explicit user override wins outright.
    let override_key = format!("{agent_id}/{value}");
    if let Some(canon) = overrides.get(&override_key) {
        let (provider, model) = match canon.split_once('/') {
            Some((p, m)) => (Some(provider_norm(p)), m.to_string()),
            None => (None, canon.clone()),
        };
        return dedup_preserving(
            model_candidates(&model)
                .iter()
                .flat_map(|m| keys_for(provider.as_deref(), m))
                .collect(),
        );
    }

    let mut out = Vec::new();

    // 2. The option id and display name — either may carry the model id (Anvil)
    //    or a bare alias (claude-acp).
    for raw in [value, name] {
        if raw.is_empty() {
            continue;
        }
        let (provider, model) = parse_id(agent_id, raw);
        for cand in model_candidates(&model) {
            out.extend(keys_for(provider.as_deref(), &cand));
        }
    }

    // 3. The description's leading phrase carries the version a bare alias omits
    //    (`Opus 4.8 with 1M context` → `Opus 4.8`), qualified by the agent vendor.
    if let Some(phrase) = leading_phrase(description) {
        let provider = single_vendor_provider(agent_id);
        for cand in model_candidates(&phrase) {
            out.extend(keys_for(provider, &cand));
        }
    }

    dedup_preserving(out)
}

/// Match keys for an LMArena leaderboard row (`model_name` + `organization`).
/// Used to index the score catalog.
#[cfg(test)]
fn lmarena_keys(name: &str, vendor: &str) -> Vec<MatchKey> {
    lmarena_keys_ranked(name, vendor)
        .into_iter()
        .map(|(key, _)| key)
        .collect()
}

/// Match keys for a leaderboard row, annotated with whether they came from the
/// exact published name or from a stripped fallback alias.
pub fn lmarena_keys_ranked(name: &str, vendor: &str) -> Vec<(MatchKey, MatchSpecificity)> {
    let provider = (!vendor.trim().is_empty()).then_some(vendor);
    dedup_ranked_preserving(
        model_candidates(name)
            .iter()
            .enumerate()
            .flat_map(|(idx, m)| {
                let specificity = if idx == 0 {
                    MatchSpecificity::Exact
                } else {
                    MatchSpecificity::Alias
                };
                keys_for(provider, m)
                    .into_iter()
                    .map(move |key| (key, specificity))
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_overrides() -> HashMap<String, String> {
        HashMap::new()
    }

    /// Does the agent option resolve to a key the LMArena row also produces?
    #[allow(clippy::too_many_arguments)]
    fn joins(
        agent_id: &str,
        value: &str,
        name: &str,
        description: &str,
        arena_name: &str,
        vendor: &str,
    ) -> bool {
        let agent = agent_keys(agent_id, value, name, description, &no_overrides());
        let arena = lmarena_keys(arena_name, vendor);
        agent.iter().any(|k| arena.contains(k))
    }

    #[test]
    fn alnum_lower_collapses_separators_and_case() {
        assert_eq!(alnum_lower("Claude Opus 4.8"), "claudeopus48");
        assert_eq!(alnum_lower("claude-opus-4-8"), "claudeopus48");
    }

    #[test]
    fn provider_alias_and_known_set() {
        assert_eq!(canonical_provider("zai").as_deref(), Some("zhipuai"));
        assert_eq!(canonical_provider("Qwen").as_deref(), Some("alibaba"));
        assert_eq!(canonical_provider("us"), None);
    }

    #[test]
    fn leading_phrase_extracts_model_from_description() {
        assert_eq!(
            leading_phrase("Opus 4.8 with 1M context · Best for everyday, complex tasks")
                .as_deref(),
            Some("Opus 4.8")
        );
        assert_eq!(
            leading_phrase("Sonnet 4.6 · Efficient for routine tasks").as_deref(),
            Some("Sonnet 4.6")
        );
        assert_eq!(
            leading_phrase("Haiku 4.5 · Fastest").as_deref(),
            Some("Haiku 4.5")
        );
    }

    // ---- claude-acp: alias name + version only in the description ----

    #[test]
    fn claude_acp_alias_joins_via_description_and_brand_strip() {
        assert!(joins(
            "claude-acp",
            "opus",
            "Opus",
            "Opus 4.8 with 1M context · Best for everyday, complex tasks",
            "claude-opus-4-8",
            "anthropic",
        ));
        assert!(joins(
            "claude-acp",
            "sonnet",
            "Sonnet",
            "Sonnet 4.6 · Efficient for routine tasks",
            "claude-sonnet-4-6",
            "anthropic",
        ));
        // Haiku is dated on LMArena (claude-haiku-4-5-20251001) — date-strip +
        // brand-strip still bridges it.
        assert!(joins(
            "claude-acp",
            "haiku",
            "Haiku",
            "Haiku 4.5 · Fastest for quick answers",
            "claude-haiku-4-5-20251001",
            "anthropic",
        ));
    }

    #[test]
    fn claude_acp_default_uses_description_version() {
        assert!(joins(
            "claude-acp",
            "default",
            "Default (recommended)",
            "Opus 4.8 with 1M context · Best for everyday, complex tasks",
            "claude-opus-4-8",
            "anthropic",
        ));
    }

    // ---- Anvil bedrock/codex ids ----

    #[test]
    fn anvil_bedrock_and_codex_ids_join() {
        assert!(joins(
            "anvil",
            "bedrock::us.anthropic.claude-opus-4-8",
            "bedrock::us.anthropic.claude-opus-4-8",
            "",
            "claude-opus-4-8",
            "anthropic",
        ));
        assert!(joins(
            "anvil",
            "bedrock::us.anthropic.claude-opus-4-5-20251101-v1:0",
            "bedrock::us.anthropic.claude-opus-4-5-20251101-v1:0",
            "",
            "claude-opus-4-5-20251101",
            "anthropic",
        ));
        assert!(joins(
            "anvil",
            "codex::gpt-5.5",
            "codex::gpt-5.5",
            "",
            "gpt-5.5",
            "openai"
        ));
        assert!(joins(
            "anvil",
            "bedrock::zai.glm-5",
            "bedrock::zai.glm-5",
            "",
            "glm-5",
            "zai"
        ));
    }

    #[test]
    fn unknown_and_closed_models_yield_no_score() {
        assert!(!joins(
            "anvil",
            "bedrock::xai.grok-4.3",
            "bedrock::xai.grok-4.3",
            "",
            "claude-opus-4-8",
            "anthropic",
        ));
        let devin = agent_keys("devin", "devin-1", "Devin 1", "", &no_overrides());
        let arena = lmarena_keys("claude-opus-4-8", "anthropic");
        assert!(!devin.iter().any(|k| arena.contains(k)));
    }

    #[test]
    fn aggregator_provider_model_id_is_used_directly() {
        assert!(joins(
            "opencode",
            "anthropic/claude-opus-4-8",
            "Claude Opus 4.8",
            "",
            "claude-opus-4-8",
            "anthropic",
        ));
    }

    #[test]
    fn opencode_router_path_resolves_via_value() {
        // openrouter/anthropic/claude-opus-4.8 → provider anthropic / claude-opus-4.8
        assert!(joins(
            "opencode",
            "openrouter/anthropic/claude-opus-4.8",
            "OpenRouter/Claude Opus 4.8",
            "",
            "claude-opus-4-8",
            "anthropic",
        ));
        // A `(latest)` parenthetical in the name is stripped; the dated board row
        // is reached via date-stripping.
        assert!(joins(
            "opencode",
            "openrouter/anthropic/claude-opus-4.5",
            "OpenRouter/Claude Opus 4.5 (latest)",
            "",
            "claude-opus-4-5-20251101",
            "anthropic",
        ));
    }

    #[test]
    fn effort_suffix_stripped_so_mini_matches_mini_high() {
        // codex exposes `gpt-5.4-mini`; LMArena only has `gpt-5.4-mini-high`.
        assert!(joins(
            "codex-acp",
            "gpt-5.4-mini",
            "GPT-5.4-Mini",
            "",
            "gpt-5.4-mini-high",
            "openai",
        ));
    }

    #[test]
    fn preview_suffix_stripped_so_qwen_max_matches() {
        // opencode exposes `qwen/qwen3.7-max`; LMArena lists `qwen3.7-max-preview`.
        assert!(joins(
            "opencode",
            "openrouter/qwen/qwen3.7-max",
            "OpenRouter/Qwen3.7 Max",
            "",
            "qwen3.7-max-preview",
            "alibaba",
        ));
        // And distinct versions must not collapse onto each other.
        assert!(!joins(
            "opencode",
            "openrouter/qwen/qwen3.6-max",
            "Qwen3.6 Max",
            "",
            "qwen3.7-max-preview",
            "alibaba",
        ));
    }

    #[test]
    fn brand_strip_does_not_emit_dangerous_short_bare_keys() {
        // `gpt-5` brand-strips to `5`, which must NOT become a bare key.
        let keys = keys_for(Some("openai"), "gpt-5");
        assert!(keys.contains(&"openai/gpt5".to_string()));
        assert!(keys.contains(&"gpt5".to_string()));
        assert!(!keys.contains(&"5".to_string()));
        assert!(!keys.contains(&"openai/5".to_string())); // remainder too short
    }

    #[test]
    fn override_redirects_agent_specific_tune_to_base() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "codex-acp/gpt-5-codex".to_string(),
            "openai/gpt-5.5".to_string(),
        );
        let agent = agent_keys("codex-acp", "gpt-5-codex", "GPT-5 Codex", "", &overrides);
        let arena = lmarena_keys("gpt-5.5", "openai");
        assert!(agent.iter().any(|k| arena.contains(k)));
    }
}
