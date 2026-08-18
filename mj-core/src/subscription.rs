//! Subscription tier detection for the two vendor-native ACP adapters.
//!
//! `auto` model selection ranks on DeepSWE quality alone, which sends the
//! primary seat at the best model even when the account paying for it is the
//! cheapest plan on offer: a Claude Pro seat running Opus is exhausted in a
//! session or two next to a ChatGPT Pro seat that could carry the whole day.
//! Both vendors record the active plan locally, so this reads their own files
//! instead of paying a subprocess round trip during roster resolution. Claude
//! Code keeps the organization tier in `.claude.json`; Codex carries
//! `chatgpt_plan_type` inside the ID token it persists in `auth.json`, so an
//! upgrade shows up here once Codex next refreshes that token.

use std::path::PathBuf;

use base64::Engine;
use serde_json::Value;

pub use crate::roster_types::{Subscription, Subscriptions};

/// Detect subscriptions recorded by vendor-native clients.
pub fn detect() -> Subscriptions {
    Subscriptions {
        claude: detect_claude(),
        codex: detect_codex(),
    }
}

fn detect_claude() -> Option<Subscription> {
    let document = read_json(claude_config_path()?)?;
    let account = document.get("oauthAccount")?;
    let organization_type = account.get("organizationType").and_then(Value::as_str)?;
    let rate_limit_tier = account
        .get("organizationRateLimitTier")
        .and_then(Value::as_str);
    Some(claude_plan(organization_type, rate_limit_tier))
}

fn detect_codex() -> Option<Subscription> {
    let document = read_json(codex_auth_path()?)?;
    let id_token = document.pointer("/tokens/id_token")?.as_str()?;
    let claims = decode_jwt_claims(id_token)?;
    let plan_type = claims
        .get("https://api.openai.com/auth")?
        .get("chatgpt_plan_type")?
        .as_str()?;
    Some(codex_plan(plan_type))
}

/// Claude Code reads `<config dir>/.config.json` when it exists and otherwise
/// `.claude.json` beside it, where the config dir is `$CLAUDE_CONFIG_DIR` or
/// the home directory. Follow the same order so a relocated config is honored.
fn claude_config_path() -> Option<PathBuf> {
    let configured = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);
    let scoped = configured
        .clone()
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))?
        .join(".config.json");
    if scoped.is_file() {
        return Some(scoped);
    }
    Some(match configured {
        Some(configured) => configured.join(".claude.json"),
        None => dirs::home_dir()?.join(".claude.json"),
    })
}

fn codex_auth_path() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .map(|root| root.join("auth.json"))
}

fn read_json(path: PathBuf) -> Option<Value> {
    let contents = std::fs::read(path).ok()?;
    serde_json::from_slice(&contents).ok()
}

/// A JWT is `header.payload.signature` with each part base64url-encoded and
/// unpadded; only the payload's claims are of interest here.
fn decode_jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn claude_plan(organization_type: &str, rate_limit_tier: Option<&str>) -> Subscription {
    match (organization_type, rate_limit_tier) {
        ("claude_max", Some("default_claude_max_20x")) => Subscription::new("Claude Max 20x", 20.0),
        ("claude_max", Some("default_claude_max_5x")) => Subscription::new("Claude Max 5x", 5.0),
        // An unrecognized Max tier is read as the smaller one. Understating
        // capacity only forgoes a routing preference; overstating it parks the
        // primary seat on a plan that cannot carry the session.
        ("claude_max", _) => Subscription::new("Claude Max", 5.0),
        ("claude_team", _) => Subscription::new("Claude Team", 1.0),
        ("claude_enterprise", _) => Subscription::new("Claude Enterprise", 1.0),
        ("claude_pro", _) => Subscription::new("Claude Pro", 1.0),
        (other, _) => Subscription::new(format!("Claude ({other})"), 1.0),
    }
}

fn codex_plan(plan_type: &str) -> Subscription {
    match plan_type {
        "free" => Subscription::new("ChatGPT Free", 0.0),
        "go" => Subscription::new("ChatGPT Go", 0.25),
        "plus" => Subscription::new("ChatGPT Plus", 1.0),
        "prolite" => Subscription::new("ChatGPT Pro Lite", 5.0),
        "pro" => Subscription::new("ChatGPT Pro", 20.0),
        // Seat-priced plans bill per member, so one seat is the entry rung
        // however large the organization behind it is.
        "team" | "business" | "self_serve_business_prolite" | "self_serve_business_usage_based" => {
            Subscription::new("ChatGPT Business", 1.0)
        }
        "enterprise" | "ent26" | "enterprise_cbp_automation" | "enterprise_cbp_usage_based" => {
            Subscription::new("ChatGPT Enterprise", 1.0)
        }
        "edu" => Subscription::new("ChatGPT Edu", 1.0),
        other => Subscription::new(format!("ChatGPT ({other})"), 1.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roster_types::AdapterKind;

    fn subscriptions(claude: Option<Subscription>, codex: Option<Subscription>) -> Subscriptions {
        Subscriptions { claude, codex }
    }

    #[test]
    fn claude_max_tiers_carry_anthropics_published_multipliers() {
        assert_eq!(
            claude_plan("claude_max", Some("default_claude_max_20x")),
            Subscription::new("Claude Max 20x", 20.0)
        );
        assert_eq!(
            claude_plan("claude_max", Some("default_claude_max_5x")),
            Subscription::new("Claude Max 5x", 5.0)
        );
        assert_eq!(
            claude_plan("claude_pro", Some("default_claude_ai")),
            Subscription::new("Claude Pro", 1.0)
        );
    }

    #[test]
    fn an_unrecognized_max_tier_is_read_as_the_smaller_one() {
        assert_eq!(
            claude_plan("claude_max", Some("default_claude_max_50x")),
            Subscription::new("Claude Max", 5.0)
        );
        assert_eq!(
            claude_plan("claude_forge", None),
            Subscription::new("Claude (claude_forge)", 1.0)
        );
    }

    #[test]
    fn chatgpt_plans_land_on_the_same_price_rungs_as_claude_plans() {
        assert_eq!(
            codex_plan("plus").capacity,
            claude_plan("claude_pro", None).capacity
        );
        assert_eq!(
            codex_plan("pro").capacity,
            claude_plan("claude_max", Some("default_claude_max_20x")).capacity
        );
        assert_eq!(codex_plan("free"), Subscription::new("ChatGPT Free", 0.0));
        assert_eq!(
            codex_plan("self_serve_business_usage_based"),
            Subscription::new("ChatGPT Business", 1.0)
        );
        assert_eq!(
            codex_plan("moonshot"),
            Subscription::new("ChatGPT (moonshot)", 1.0)
        );
    }

    #[test]
    fn the_larger_subscription_is_favored() {
        let claude_pro = claude_plan("claude_pro", None);
        let chatgpt_pro = codex_plan("pro");
        assert_eq!(
            subscriptions(Some(claude_pro.clone()), Some(chatgpt_pro.clone())).favored(),
            Some(AdapterKind::Codex)
        );
        assert_eq!(
            subscriptions(
                Some(claude_plan("claude_max", Some("default_claude_max_20x"))),
                Some(codex_plan("plus"))
            )
            .favored(),
            Some(AdapterKind::Claude)
        );
    }

    #[test]
    fn equal_or_one_sided_tiers_express_no_preference() {
        // Both $200 rungs: nothing to gain, so ranking decides.
        assert_eq!(
            subscriptions(
                Some(claude_plan("claude_max", Some("default_claude_max_20x"))),
                Some(codex_plan("pro"))
            )
            .favored(),
            None
        );
        assert_eq!(
            subscriptions(Some(claude_plan("claude_pro", None)), None).favored(),
            None
        );
        assert_eq!(subscriptions(None, Some(codex_plan("pro"))).favored(), None);
    }

    #[test]
    fn subscriptions_are_only_reported_for_the_vendor_native_adapters() {
        let detected = subscriptions(
            Some(claude_plan("claude_pro", None)),
            Some(codex_plan("pro")),
        );
        assert_eq!(
            detected
                .for_adapter(AdapterKind::Claude)
                .map(|plan| plan.capacity),
            Some(1.0)
        );
        assert_eq!(
            detected
                .for_adapter(AdapterKind::Codex)
                .map(|plan| plan.capacity),
            Some(20.0)
        );
    }

    #[test]
    fn codex_plan_type_is_read_from_the_persisted_id_token() {
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&serde_json::json!({
                "https://api.openai.com/auth": { "chatgpt_plan_type": "pro" }
            }))
            .unwrap(),
        );
        let token = format!("header.{claims}.signature");
        assert_eq!(
            decode_jwt_claims(&token).and_then(|claims| claims
                .get("https://api.openai.com/auth")?
                .get("chatgpt_plan_type")?
                .as_str()
                .map(str::to_string)),
            Some("pro".to_string())
        );
        assert!(decode_jwt_claims("not-a-jwt").is_none());
    }
}
