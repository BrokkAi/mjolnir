//! Model-first resolution for Thor (primary), Loki (reviewer), and Eitri
//! (builder). ACP adapters are an implementation detail selected from local
//! capabilities.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use futures::{StreamExt, stream};

use crate::config::Config;
use crate::deepswe::{self, Row};
use crate::{model_resolve, probe};

const PROBE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterKind {
    Codex,
    Claude,
    Anvil,
    OpenCode,
    Custom,
}

impl AdapterKind {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
            Self::Anvil => "Anvil",
            Self::OpenCode => "OpenCode",
            Self::Custom => "Custom",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AdapterLaunch {
    pub kind: AdapterKind,
    pub source_id: String,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedRole {
    pub model: Row,
    pub model_value: String,
    pub launch: AdapterLaunch,
    pub ranked: bool,
}

#[derive(Debug, Clone)]
pub struct ResolvedCouncil {
    pub thor: ResolvedRole,
    pub loki: Option<ResolvedRole>,
    pub eitri: ResolvedRole,
    pub available: Vec<ResolvedRole>,
    pub choices: Vec<ModelChoice>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ModelChoice {
    pub model: String,
    pub pass_at_1: f64,
    pub mean_cost_usd: f64,
    pub available: bool,
    pub disabled_reason: Option<String>,
    pub adapter: Option<String>,
    pub ranked: bool,
}

#[derive(Debug, Clone)]
pub struct Availability {
    pub codex: Option<PathBuf>,
    pub claude: Option<PathBuf>,
    pub opencode: Option<PathBuf>,
    pub openrouter: bool,
}

impl Availability {
    pub fn detect() -> Self {
        Self {
            codex: find_on_path("codex"),
            claude: find_on_path("claude"),
            opencode: find_on_path("opencode"),
            openrouter: std::env::var_os("OPENROUTER_API_KEY")
                .is_some_and(|value| !value.is_empty()),
        }
    }

    pub fn missing_reason(&self, model: &str) -> Option<&'static str> {
        match adapter_kind(model) {
            AdapterKind::Codex if self.codex.is_none() => {
                Some("codex executable not found on PATH")
            }
            AdapterKind::Claude if self.claude.is_none() => {
                Some("claude executable not found on PATH")
            }
            AdapterKind::Anvil if !self.openrouter => Some("OPENROUTER_API_KEY is not set"),
            AdapterKind::OpenCode if self.opencode.is_none() => {
                Some("opencode executable not found on PATH")
            }
            _ => None,
        }
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe = directory.join(format!("{name}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

fn adapter_kind(model: &str) -> AdapterKind {
    match deepswe::model_provider(model) {
        "openai" => AdapterKind::Codex,
        "anthropic" => AdapterKind::Claude,
        _ => AdapterKind::Anvil,
    }
}

fn adapter_accepts_model(kind: AdapterKind, model: &str) -> bool {
    match kind {
        AdapterKind::Codex => deepswe::model_provider(model) == "openai",
        AdapterKind::Claude => deepswe::model_provider(model) == "anthropic",
        AdapterKind::Anvil | AdapterKind::OpenCode | AdapterKind::Custom => true,
    }
}

fn launch_for(kind: AdapterKind) -> AdapterLaunch {
    match kind {
        AdapterKind::Codex => AdapterLaunch {
            kind,
            source_id: "codex-acp".to_string(),
            command: PathBuf::from("npx"),
            args: vec![
                "-y".to_string(),
                "@agentclientprotocol/codex-acp".to_string(),
            ],
            env: HashMap::new(),
        },
        AdapterKind::Claude => AdapterLaunch {
            kind,
            source_id: "claude-acp".to_string(),
            command: PathBuf::from("npx"),
            args: vec![
                "-y".to_string(),
                "@agentclientprotocol/claude-agent-acp".to_string(),
            ],
            env: HashMap::new(),
        },
        AdapterKind::Anvil => AdapterLaunch {
            kind,
            source_id: "anvil".to_string(),
            command: PathBuf::from("uvx"),
            args: vec!["brokk".to_string(), "acp".to_string()],
            env: HashMap::new(),
        },
        AdapterKind::OpenCode => AdapterLaunch {
            kind,
            source_id: "opencode-acp".to_string(),
            command: PathBuf::from("opencode"),
            args: vec!["acp".to_string()],
            env: HashMap::new(),
        },
        AdapterKind::Custom => unreachable!("custom launches come from configuration"),
    }
}

type ProbeResult = std::result::Result<probe::AdapterCapabilities, String>;
type ProbeCell = Arc<tokio::sync::OnceCell<ProbeResult>>;

static PROBE_CACHE: LazyLock<tokio::sync::Mutex<HashMap<String, ProbeCell>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));
static WARNED_ADAPTERS: LazyLock<tokio::sync::Mutex<HashSet<String>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashSet::new()));

fn probe_key(launch: &AdapterLaunch) -> String {
    format!(
        "{}\u{0}{}\u{0}{}",
        launch.source_id,
        launch.command.display(),
        launch.args.join("\u{0}")
    )
}

async fn probe_launch(
    launch: &AdapterLaunch,
    cwd: &Path,
) -> std::result::Result<probe::AdapterCapabilities, String> {
    let key = probe_key(launch);
    let cell = {
        let mut cache = PROBE_CACHE.lock().await;
        cache.entry(key).or_default().clone()
    };
    cell.get_or_init(|| async {
        probe::adapter_capabilities(
            launch.command.clone(),
            launch.args.clone(),
            launch.env.clone(),
            cwd.to_path_buf(),
            PROBE_TIMEOUT,
        )
        .await
    })
    .await
    .clone()
}

fn row_keys(row: &Row) -> HashSet<String> {
    model_resolve::catalog_keys_ranked(&row.model, deepswe::model_provider(&row.model))
        .into_iter()
        .map(|(key, _)| key)
        .collect()
}

fn option_matches(launch: &AdapterLaunch, option: &probe::ModelOption, row: &Row) -> bool {
    let wanted = row_keys(row);
    let description = option.description.as_deref().unwrap_or_default();
    model_resolve::agent_keys(
        &launch.source_id,
        &option.value,
        &option.name,
        description,
        &HashMap::new(),
    )
    .into_iter()
    .any(|key| wanted.contains(&key))
}

struct Discovery {
    available: Vec<ResolvedRole>,
    adapter_errors: HashMap<String, String>,
}

fn resolve_probes(rows: &[Row], mut probes: Vec<(usize, AdapterLaunch, ProbeResult)>) -> Discovery {
    probes.sort_by_key(|(priority, _, _)| *priority);
    let mut resolved = Vec::new();
    let mut adapter_errors = HashMap::new();
    let mut claimed_ranked = HashSet::new();
    for (_, launch, capabilities) in probes {
        let capabilities = match capabilities {
            Ok(capabilities) => capabilities,
            Err(reason) => {
                adapter_errors.insert(launch.source_id.clone(), reason);
                tracing::warn!(adapter = %launch.source_id, "council adapter probe failed");
                continue;
            }
        };
        if !capabilities.http_mcp {
            adapter_errors.insert(
                launch.source_id.clone(),
                "ACP server does not advertise mcpCapabilities.http".to_string(),
            );
            tracing::warn!(
                adapter = %launch.source_id,
                "Council adapter excluded because HTTP MCP is unavailable"
            );
            continue;
        }
        let options = capabilities.models;
        let matched_values = options
            .iter()
            .filter(|option| {
                rows.iter().any(|row| {
                    adapter_accepts_model(launch.kind, &row.model)
                        && option_matches(&launch, option, row)
                })
            })
            .map(|option| option.value.clone())
            .collect::<HashSet<_>>();
        for row in rows
            .iter()
            .filter(|row| adapter_accepts_model(launch.kind, &row.model))
        {
            if claimed_ranked.contains(&row.model) {
                continue;
            }
            if let Some(option) = options
                .iter()
                .find(|option| option_matches(&launch, option, row))
            {
                claimed_ranked.insert(row.model.clone());
                resolved.push(ResolvedRole {
                    model: row.clone(),
                    model_value: option.value.clone(),
                    launch: launch.clone(),
                    ranked: true,
                });
            }
        }
        if launch.kind == AdapterKind::Custom {
            for option in options
                .iter()
                .filter(|option| !matched_values.contains(&option.value))
            {
                let id = custom_model_id(&launch.source_id, &option.value);
                resolved.push(ResolvedRole {
                    model: Row {
                        model: id,
                        reasoning_effort: None,
                        pass_at_1: 0.0,
                        mean_cost_usd: 0.0,
                    },
                    model_value: option.value.clone(),
                    launch: launch.clone(),
                    ranked: false,
                });
            }
        }
    }
    resolved.sort_by(|a, b| {
        b.ranked
            .cmp(&a.ranked)
            .then_with(|| b.model.pass_at_1.total_cmp(&a.model.pass_at_1))
            .then_with(|| a.model.mean_cost_usd.total_cmp(&b.model.mean_cost_usd))
            .then_with(|| a.model.model.cmp(&b.model.model))
    });
    Discovery {
        available: resolved,
        adapter_errors,
    }
}

fn configured_launches(config: &Config, availability: &Availability) -> Vec<AdapterLaunch> {
    let mut launches = config
        .acp
        .servers
        .iter()
        .map(|server| AdapterLaunch {
            kind: AdapterKind::Custom,
            source_id: format!("custom:{}", server.name),
            command: server.command.clone(),
            args: server.args.clone(),
            env: HashMap::new(),
        })
        .collect::<Vec<_>>();
    if availability.codex.is_some() {
        launches.push(launch_for(AdapterKind::Codex));
    }
    if availability.claude.is_some() {
        launches.push(launch_for(AdapterKind::Claude));
    }
    if availability.openrouter {
        launches.push(launch_for(AdapterKind::Anvil));
    }
    if availability.opencode.is_some() {
        launches.push(launch_for(AdapterKind::OpenCode));
    }
    launches
}

fn custom_model_id(source_id: &str, model_value: &str) -> String {
    let name = source_id.strip_prefix("custom:").unwrap_or(source_id);
    format!("custom/{name}/{model_value}")
}

async fn discover_available(
    config: &Config,
    rows: &[Row],
    availability: &Availability,
    cwd: &Path,
) -> Discovery {
    let launches = configured_launches(config, availability);
    let probes = stream::iter(launches.into_iter().enumerate().map(|(priority, launch)| {
        let cwd = cwd.to_path_buf();
        async move {
            let capabilities = probe_launch(&launch, &cwd).await;
            (priority, launch, capabilities)
        }
    }))
    .buffer_unordered(probe::PROBE_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    resolve_probes(rows, probes)
}

fn explicit<'a>(
    role: &str,
    selector: &str,
    rows: &[Row],
    available: &'a [ResolvedRole],
) -> Result<&'a ResolvedRole> {
    if let Some(candidate) = available
        .iter()
        .find(|candidate| candidate.model.model == selector)
    {
        return Ok(candidate);
    }
    if selector.starts_with("custom/") {
        bail!("{role} model '{selector}' is unavailable from its configured custom ACP server");
    }
    if !rows.iter().any(|row| row.model == selector) {
        bail!("{role} model '{selector}' is not an eligible DeepSWE High/default model");
    }
    bail!("{role} model '{selector}' is unavailable: no HTTP-MCP-capable ACP adapter advertised it")
}

fn choose_eitri<'a>(rows: &[Row], available: &'a [ResolvedRole]) -> Option<&'a ResolvedRole> {
    let anchor = deepswe::sonnet_anchor(rows)?;
    let launchable_rows: Vec<Row> = available
        .iter()
        .filter(|role| role.ranked)
        .map(|role| role.model.clone())
        .collect();
    deepswe::eitri_frontier_choice(&launchable_rows, anchor.pass_at_1).and_then(|row| {
        available
            .iter()
            .find(|candidate| candidate.model.model == row.model)
    })
}

fn provider_key(model: &str) -> &str {
    let provider = deepswe::model_provider(model);
    if provider.is_empty() {
        model.split_once('-').map_or(model, |(prefix, _)| prefix)
    } else {
        provider
    }
}

fn choose_loki<'a>(thor: &ResolvedRole, available: &'a [ResolvedRole]) -> Option<&'a ResolvedRole> {
    let thor_provider = provider_key(&thor.model.model);
    available
        .iter()
        .filter(|candidate| candidate.ranked)
        .find(|candidate| provider_key(&candidate.model.model) != thor_provider)
}

fn unavailable_reason(
    row: &Row,
    config: &Config,
    availability: &Availability,
    adapter_errors: &HashMap<String, String>,
) -> String {
    let mut reasons = Vec::new();
    for server in &config.acp.servers {
        let source = format!("custom:{}", server.name);
        reasons.push(match adapter_errors.get(&source) {
            Some(reason) => format!("{}: {reason}", server.name),
            None => format!("{} did not advertise this model", server.name),
        });
    }
    let native = adapter_kind(&row.model);
    let native_source = launch_for(native).source_id;
    let native_detected = match native {
        AdapterKind::Codex => availability.codex.is_some(),
        AdapterKind::Claude => availability.claude.is_some(),
        AdapterKind::Anvil => availability.openrouter,
        _ => false,
    };
    if native_detected {
        reasons.push(adapter_errors.get(&native_source).map_or_else(
            || format!("{native_source} did not advertise this model"),
            |reason| format!("{native_source}: {reason}"),
        ));
    } else if let Some(reason) = availability.missing_reason(&row.model) {
        reasons.push(reason.to_string());
    }
    if native != AdapterKind::Anvil {
        if availability.openrouter {
            reasons.push(adapter_errors.get("anvil").map_or_else(
                || "anvil did not advertise this model".to_string(),
                |reason| format!("anvil: {reason}"),
            ));
        } else {
            reasons.push("OPENROUTER_API_KEY is not set".to_string());
        }
    }
    if availability.opencode.is_some() {
        reasons.push(adapter_errors.get("opencode-acp").map_or_else(
            || "opencode-acp did not advertise this model".to_string(),
            |reason| format!("opencode-acp: {reason}"),
        ));
    } else {
        reasons.push("opencode executable not found on PATH".to_string());
    }
    reasons.sort();
    reasons.dedup();
    reasons.join("; ")
}

pub async fn resolve(config: &Config, cwd: &Path) -> Result<ResolvedCouncil> {
    let leaderboard = deepswe::load(
        &deepswe::default_cache_path(),
        deepswe::CACHE_TTL,
        deepswe::DEFAULT_URL,
    )
    .await;
    let rows = deepswe::eligible_high(&leaderboard.rows);
    let availability = Availability::detect();
    let discovery = discover_available(config, &rows, &availability, cwd).await;
    let available = discovery.available;
    let mut choices = rows
        .iter()
        .map(|row| {
            let candidate = available
                .iter()
                .find(|candidate| candidate.model.model == row.model);
            let launchable = candidate.is_some();
            let disabled_reason = (!launchable)
                .then(|| unavailable_reason(row, config, &availability, &discovery.adapter_errors));
            ModelChoice {
                model: row.model.clone(),
                pass_at_1: row.pass_at_1,
                mean_cost_usd: row.mean_cost_usd,
                available: launchable,
                disabled_reason,
                adapter: candidate.map(|candidate| candidate.launch.source_id.clone()),
                ranked: true,
            }
        })
        .collect::<Vec<_>>();
    choices.extend(
        available
            .iter()
            .filter(|candidate| !candidate.ranked)
            .map(|candidate| ModelChoice {
                model: candidate.model.model.clone(),
                pass_at_1: 0.0,
                mean_cost_usd: 0.0,
                available: true,
                disabled_reason: None,
                adapter: Some(candidate.launch.source_id.clone()),
                ranked: false,
            }),
    );
    if available.is_empty() {
        let diagnostic = discovery
            .adapter_errors
            .values()
            .next()
            .map(|reason| format!(" ({reason})"))
            .unwrap_or_default();
        bail!(
            "no Council model is launchable{diagnostic}: install codex, claude, or opencode; set OPENROUTER_API_KEY; or configure an ACP server"
        );
    }

    let thor = if config.thor.model == "auto" {
        available
            .iter()
            .find(|candidate| candidate.ranked)
            .ok_or_else(|| anyhow!("Thor Auto requires at least one ranked DeepSWE model"))?
    } else {
        explicit("Thor", &config.thor.model, &rows, &available)?
    };
    let loki = if config.loki.model == "auto" {
        choose_loki(thor, &available)
    } else {
        Some(explicit("Loki", &config.loki.model, &rows, &available)?)
    };
    if loki.is_some_and(|candidate| candidate.model.model == thor.model.model) {
        bail!("Loki must use a model distinct from Thor");
    }
    let eitri = if config.eitri.model == "auto" {
        choose_eitri(&rows, &available)
            .ok_or_else(|| anyhow!("no launchable Eitri model lies on the DeepSWE frontier"))?
    } else {
        explicit("Eitri", &config.eitri.model, &rows, &available)?
    };

    let mut warned = WARNED_ADAPTERS.lock().await;
    let mut warnings = discovery
        .adapter_errors
        .iter()
        .filter(|(adapter, _)| warned.insert((*adapter).clone()))
        .map(|(adapter, reason)| format!("{adapter} unavailable: {reason}"))
        .collect::<Vec<_>>();
    warnings.sort();
    Ok(ResolvedCouncil {
        thor: thor.clone(),
        loki: loki.cloned(),
        eitri: eitri.clone(),
        available,
        choices,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(value: &str) -> probe::ModelOption {
        probe::ModelOption {
            value: value.to_string(),
            name: value.to_string(),
            description: None,
        }
    }

    fn capabilities(http_mcp: bool, values: &[&str]) -> ProbeResult {
        Ok(probe::AdapterCapabilities {
            http_mcp,
            models: values.iter().map(|value| option(value)).collect(),
        })
    }

    fn custom_launch(name: &str) -> AdapterLaunch {
        AdapterLaunch {
            kind: AdapterKind::Custom,
            source_id: format!("custom:{name}"),
            command: PathBuf::from(name),
            args: Vec::new(),
            env: HashMap::new(),
        }
    }

    fn role(model: &str, pass_at_1: f64) -> ResolvedRole {
        role_at(model, pass_at_1, 1.0)
    }

    fn role_at(model: &str, pass_at_1: f64, mean_cost_usd: f64) -> ResolvedRole {
        ResolvedRole {
            model: Row {
                model: model.to_string(),
                reasoning_effort: Some("high".to_string()),
                pass_at_1,
                mean_cost_usd,
            },
            model_value: model.to_string(),
            launch: launch_for(adapter_kind(model)),
            ranked: true,
        }
    }

    #[test]
    fn provider_routes_are_model_first() {
        assert_eq!(adapter_kind("gpt-5-6-sol"), AdapterKind::Codex);
        assert_eq!(adapter_kind("claude-sonnet-5"), AdapterKind::Claude);
        assert_eq!(adapter_kind("gemini-3-5-flash"), AdapterKind::Anvil);
        assert_eq!(adapter_kind("glm-5-2"), AdapterKind::Anvil);
    }

    #[test]
    fn adapter_display_names_match_the_primary_acp_products() {
        assert_eq!(AdapterKind::Codex.display_name(), "Codex");
        assert_eq!(AdapterKind::Claude.display_name(), "Claude Code");
        assert_eq!(AdapterKind::Anvil.display_name(), "Anvil");
    }

    #[test]
    fn opencode_normalizes_provider_and_openrouter_model_values() {
        let row = role_at("claude-opus-4-8", 0.5, 4.0).model;
        let launch = launch_for(AdapterKind::OpenCode);
        assert!(option_matches(
            &launch,
            &option("anthropic/claude-opus-4-8"),
            &row
        ));
        assert!(option_matches(
            &launch,
            &option("openrouter/anthropic/claude-opus-4-8"),
            &row
        ));
    }

    #[test]
    fn route_precedence_is_custom_then_native_then_anvil_then_opencode() {
        let rows = vec![
            role_at("gpt-5-5", 0.6, 5.0).model,
            role_at("claude-opus-4-8", 0.5, 4.0).model,
            role_at("gemini-3-1-pro-preview", 0.1, 9.0).model,
        ];
        let custom = custom_launch("company");
        let discovery = resolve_probes(
            &rows,
            vec![
                (0, custom.clone(), capabilities(true, &["gpt-5-5"])),
                (
                    1,
                    launch_for(AdapterKind::Codex),
                    capabilities(true, &["gpt-5-5"]),
                ),
                (
                    2,
                    launch_for(AdapterKind::Claude),
                    capabilities(true, &["claude-opus-4-8"]),
                ),
                (
                    3,
                    launch_for(AdapterKind::Anvil),
                    capabilities(
                        true,
                        &[
                            "openai/gpt-5-5",
                            "anthropic/claude-opus-4-8",
                            "google/gemini-3-1-pro-preview",
                        ],
                    ),
                ),
                (
                    4,
                    launch_for(AdapterKind::OpenCode),
                    capabilities(true, &["openrouter/google/gemini-3-1-pro-preview"]),
                ),
            ],
        );
        let route = |model: &str| {
            discovery
                .available
                .iter()
                .find(|role| role.model.model == model)
                .expect("resolved route")
                .launch
                .source_id
                .as_str()
        };
        assert_eq!(route("gpt-5-5"), "custom:company");
        assert_eq!(route("claude-opus-4-8"), "claude-acp");
        assert_eq!(route("gemini-3-1-pro-preview"), "anvil");
    }

    #[test]
    fn incompatible_and_failed_adapters_are_excluded_with_sanitized_reasons() {
        let rows = vec![role_at("gpt-5-5", 0.6, 5.0).model];
        let discovery = resolve_probes(
            &rows,
            vec![
                (
                    0,
                    custom_launch("no-http"),
                    capabilities(false, &["gpt-5-5"]),
                ),
                (
                    1,
                    custom_launch("needs-auth"),
                    Err("needs auth".to_string()),
                ),
            ],
        );
        assert!(discovery.available.is_empty());
        assert_eq!(
            discovery.adapter_errors["custom:no-http"],
            "ACP server does not advertise mcpCapabilities.http"
        );
        assert_eq!(discovery.adapter_errors["custom:needs-auth"], "needs auth");
    }

    #[test]
    fn custom_unmatched_models_are_unranked_and_preserve_exact_values() {
        let rows = vec![role_at("gpt-5-5", 0.6, 5.0).model];
        let discovery = resolve_probes(
            &rows,
            vec![(
                0,
                custom_launch("company"),
                capabilities(true, &["company/private-model"]),
            )],
        );
        let role = discovery.available.first().expect("unranked model");
        assert!(!role.ranked);
        assert_eq!(role.model.model, "custom/company/company/private-model");
        assert_eq!(role.model_value, "company/private-model");
    }

    #[test]
    fn missing_reasons_never_include_secret_values() {
        let availability = Availability {
            codex: None,
            claude: None,
            opencode: None,
            openrouter: false,
        };
        assert_eq!(
            availability.missing_reason("gpt-5-6-sol"),
            Some("codex executable not found on PATH")
        );
        assert_eq!(
            availability.missing_reason("glm-5-2"),
            Some("OPENROUTER_API_KEY is not set")
        );
    }

    #[test]
    fn explicit_unavailable_model_has_actionable_provider_reason() {
        let rows = vec![Row {
            model: "gpt-5-6-sol".to_string(),
            reasoning_effort: Some("high".to_string()),
            pass_at_1: 0.7,
            mean_cost_usd: 3.0,
        }];
        let availability = Availability {
            codex: None,
            claude: None,
            opencode: None,
            openrouter: false,
        };
        let error = explicit("Thor", "gpt-5-6-sol", &rows, &[])
            .expect_err("must reject unavailable explicit model");
        assert!(
            error
                .to_string()
                .contains("no HTTP-MCP-capable ACP adapter")
        );
        let _ = availability;
    }

    #[test]
    fn auto_loki_chooses_best_model_from_a_different_provider() {
        let available = vec![
            role("gpt-5-6-sol", 0.70),
            role("gpt-5-5", 0.65),
            role("claude-fable-5", 0.64),
            role("gemini-3-1-pro-preview", 0.60),
        ];

        assert_eq!(
            choose_loki(&available[0], &available)
                .expect("cross-provider Loki")
                .model
                .model,
            "claude-fable-5"
        );
    }

    #[test]
    fn auto_loki_is_unavailable_when_only_thors_provider_is_launchable() {
        let available = vec![role("gpt-5-6-sol", 0.70), role("gpt-5-5", 0.65)];

        assert!(choose_loki(&available[0], &available).is_none());
    }

    #[test]
    fn auto_eitri_uses_sonnet_quality_floor_and_selects_terra() {
        let rows = vec![
            role_at("claude-sonnet-5", 0.482, 7.43).model,
            role_at("gpt-5-6-sol", 0.694, 3.47).model,
            role_at("gpt-5-6-terra", 0.538, 1.13).model,
            role_at("gpt-5-6-luna", 0.442, 0.78).model,
        ];
        let available = vec![
            role_at("gpt-5-6-sol", 0.694, 3.47),
            role_at("gpt-5-6-terra", 0.538, 1.13),
            role_at("gpt-5-6-luna", 0.442, 0.78),
        ];

        assert_eq!(
            choose_eitri(&rows, &available)
                .expect("Eitri choice")
                .model
                .model,
            "gpt-5-6-terra"
        );
    }
}
