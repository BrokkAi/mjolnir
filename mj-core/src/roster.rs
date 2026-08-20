//! Model-first resolution of the primary agent and the default subagent
//! model. ACP adapters are an implementation detail selected from local
//! capabilities.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, OnceLock};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use futures::{StreamExt, stream};

use crate::config::{AcpServerPolicy, Config};
use crate::deepswe;
use crate::model_resolve;
use crate::probe;
use crate::subscription::{self, Subscriptions};

const PROBE_TIMEOUT: Duration = Duration::from_secs(120);

pub use crate::roster_types::{
    AcpInventory, AcpServerInfo, AdapterKind, AdapterLaunch, Availability, ClaudeAuthStatus,
    ModelChoice, ModelRow as Row, ResolvedAgent, Roster, configure_permissions,
};

/// An ACP adapter contributed by the embedding binary (e.g. a platform-only
/// sidecar). Registered once at startup, before the first roster resolution.
#[derive(Debug, Clone)]
pub struct ExternalAdapter {
    pub id: String,
    pub label: String,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    /// Shown in the adapter inventory, e.g. the path the binary was found at.
    pub evidence: String,
}

static EXTERNAL_ADAPTER: OnceLock<ExternalAdapter> = OnceLock::new();

/// Register an adapter the embedding binary discovered. Only the first
/// registration wins; call before the first roster resolution.
pub fn register_external_adapter(adapter: ExternalAdapter) {
    let _ = EXTERNAL_ADAPTER.set(adapter);
}

pub fn external_adapter() -> Option<&'static ExternalAdapter> {
    EXTERNAL_ADAPTER.get()
}

fn external_launch(external: &ExternalAdapter) -> AdapterLaunch {
    AdapterLaunch {
        kind: AdapterKind::External,
        source_id: external.id.clone(),
        command: external.command.clone(),
        args: external.args.clone(),
        env: external.env.clone(),
    }
}

fn external_server_info(external: &ExternalAdapter, _config: &Config) -> AcpServerInfo {
    AcpServerInfo {
        id: external.id.clone(),
        label: external.label.clone(),
        // The platform adapter cannot be disabled: it is the only route on
        // this build, and normalize() strips any stale policy for it.
        policy: AcpServerPolicy::Auto,
        // The platform supplies this route by construction; launcher
        // availability is handled at probe and spawn time.
        detected: true,
        selected: true,
        evidence: external.evidence.clone(),
        launch: external_launch(external),
        model_count: 0,
        error: None,
        session_config: Vec::new(),
        subscription: None,
    }
}

pub fn subagent_failover_roles(roster: &Roster) -> Vec<ResolvedAgent> {
    let Some(initial) = roster.subagent_default.clone() else {
        return Vec::new();
    };
    let available = source_candidates(&roster.available, roster.subagent_acp_source.as_deref());
    failover_roles(initial, &available, false, &roster.subagent_acp_priority)
}

pub fn rebind_auto_review_for_primary(roster: &mut Roster, config: &Config) {
    if !config.agent.discrete_review {
        roster.review_supervisor = None;
        return;
    }
    if config.review.model != "auto" {
        return;
    }
    let available = source_candidates(&roster.available, config.review.acp_source.as_deref());
    let rows = roster
        .choices
        .iter()
        .filter(|choice| choice.ranked)
        .map(|choice| Row {
            model: choice.model.clone(),
            reasoning_effort: None,
            pass_at_1: choice.pass_at_1,
            mean_cost_usd: choice.mean_cost_usd,
        })
        .collect::<Vec<_>>();
    roster.review_supervisor = choose_review_auto(
        &roster.primary,
        &rows,
        &available,
        &config.review.acp_priority,
    );
    if let Some(review_supervisor) = roster.review_supervisor.as_mut() {
        review_supervisor.reasoning_effort = config.review.reasoning_effort.clone();
    }
}

fn source_candidates(available: &[ResolvedAgent], source: Option<&str>) -> Vec<ResolvedAgent> {
    available
        .iter()
        .filter(|candidate| source.is_none_or(|source| candidate.launch.source_id == source))
        .cloned()
        .collect()
}

/// Team routes constrain automatic selection only. A concrete saved model
/// always resolves through the adapter that advertises that model.
fn candidates_for_selector(
    available: &[ResolvedAgent],
    selector: &str,
    source: Option<&str>,
) -> Vec<ResolvedAgent> {
    source_candidates(available, (selector == "auto").then_some(source).flatten())
}

fn failover_roles(
    initial: ResolvedAgent,
    available: &[ResolvedAgent],
    prefer_other_provider: bool,
    acp_priority: &[String],
) -> Vec<ResolvedAgent> {
    let mut roles = vec![initial.clone()];
    let mut alternatives = available
        .iter()
        .filter(|candidate| candidate.ranked)
        .filter(|candidate| {
            candidate.model.model != initial.model.model
                || candidate.launch.source_id != initial.launch.source_id
        })
        .cloned()
        .collect::<Vec<_>>();
    if prefer_other_provider {
        alternatives
            .sort_by_key(|candidate| candidate.launch.source_id == initial.launch.source_id);
    } else {
        let mut model_order = HashMap::new();
        for (index, candidate) in available.iter().enumerate() {
            model_order
                .entry(candidate.model.model.as_str())
                .or_insert(index);
        }
        alternatives.sort_by_key(|candidate| {
            (
                model_order
                    .get(candidate.model.model.as_str())
                    .copied()
                    .unwrap_or(usize::MAX),
                source_priority(&candidate.launch.source_id, acp_priority),
            )
        });
    }
    for candidate in alternatives {
        if !roles.iter().any(|role| {
            role.model.model == candidate.model.model
                && role.launch.source_id == candidate.launch.source_id
        }) {
            roles.push(candidate);
        }
    }
    roles
}

fn source_priority(source_id: &str, priority: &[String]) -> usize {
    priority
        .iter()
        .position(|candidate| candidate == source_id)
        .unwrap_or(priority.len())
}

fn detect_availability() -> Availability {
    Availability {
        codex_credentials: codex_credentials_available(),
        claude_status: claude_auth_status(),
        subscriptions: subscription::detect(),
    }
}

fn nonempty_env(names: &[&str]) -> bool {
    names.iter().any(|name| {
        std::env::var_os(name).is_some_and(|value| !value.to_string_lossy().trim().is_empty())
    })
}

fn credential_file_has_any(path: &Path, pointers: &[&str]) -> bool {
    let Ok(contents) = std::fs::read(path) else {
        return false;
    };
    let Ok(document) = serde_json::from_slice::<serde_json::Value>(&contents) else {
        return false;
    };
    pointers.iter().any(|pointer| {
        document
            .pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    })
}

fn credential_file_evidence(path: &Path, pointers: &[&str]) -> Option<String> {
    credential_file_has_any(path, pointers).then(|| path.display().to_string())
}

fn codex_credentials_available() -> bool {
    crate::auth::detect(crate::auth::AuthVendor::OpenAi).available()
}

fn claude_auth_status() -> ClaudeAuthStatus {
    if claude_detection().is_some() {
        ClaudeAuthStatus::LoggedIn
    } else {
        ClaudeAuthStatus::NotLoggedIn
    }
}

fn claude_detection() -> Option<String> {
    match crate::auth::detect(crate::auth::AuthVendor::Anthropic) {
        crate::auth::CredentialSource::Environment(name) => Some(format!("{name} is set")),
        crate::auth::CredentialSource::File(path) => Some(path.display().to_string()),
        crate::auth::CredentialSource::Missing => None,
    }
}

fn codex_detection() -> Option<String> {
    for name in ["CODEX_API_KEY", "OPENAI_API_KEY"] {
        if nonempty_env(&[name]) {
            return Some(format!("{name} is set"));
        }
    }
    let root = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))?;
    credential_file_evidence(
        &root.join("auth.json"),
        &[
            "/OPENAI_API_KEY",
            "/tokens/access_token",
            "/tokens/refresh_token",
        ],
    )
}

/// The built-in adapter whose vendor serves this model. `None` for providers
/// no built-in adapter speaks; such ranked rows have no launchable route.
fn adapter_kind(model: &str) -> Option<AdapterKind> {
    match deepswe::model_provider(model) {
        "openai" => Some(AdapterKind::Codex),
        "anthropic" => Some(AdapterKind::Claude),
        _ => None,
    }
}

/// Drop leaderboard rows whose provider no built-in adapter serves. Ranking
/// them would only ever produce unlaunchable choices.
fn natively_served(rows: Vec<Row>) -> Vec<Row> {
    rows.into_iter()
        .filter(|row| adapter_kind(&row.model).is_some())
        .collect()
}

/// The built-in ACP source that natively serves a model, by provider. Lets
/// settings judge a pinned model's route even when the model-choice catalog
/// has no entry for it (e.g. the catalog was resolved while that vendor was
/// disabled). `None` when no built-in adapter serves the model's provider.
pub fn native_source_id(model: &str) -> Option<String> {
    Some(launch_for(adapter_kind(model)?).source_id)
}

/// The built-in ACP servers this machine has usable credentials for. Decides
/// which team a config with no team of its own adopts.
pub(crate) fn signed_in_sources() -> Vec<String> {
    [
        (AdapterKind::Claude, claude_detection()),
        (AdapterKind::Codex, codex_detection()),
    ]
    .into_iter()
    .filter_map(|(kind, evidence)| evidence.map(|_| launch_for(kind).source_id))
    .collect()
}

/// A config that explicitly enables one built-in server for tests that need a
/// selected route regardless of the host's credentials.
#[doc(hidden)]
pub fn config_with_a_visible_builtin() -> Config {
    let mut config = Config::default();
    config.set_acp_server_policy("codex-acp", AcpServerPolicy::Enabled);
    config
}

fn adapter_accepts_model(kind: AdapterKind, model: &str) -> bool {
    match kind {
        AdapterKind::Codex => deepswe::model_provider(model) == "openai",
        AdapterKind::Claude => deepswe::model_provider(model) == "anthropic",
        // External adapters never claim ranked leaderboard rows; everything
        // they advertise surfaces as unranked entries instead.
        AdapterKind::External => false,
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
        // Only reachable when an external adapter is registered: nothing maps
        // a model or source id to External without one.
        AdapterKind::External => {
            external_launch(external_adapter().expect("an external adapter is registered"))
        }
    }
}

pub fn discover_inventory(config: &Config) -> AcpInventory {
    if let Some(external) = external_adapter() {
        return AcpInventory {
            servers: vec![external_server_info(external, config)],
        };
    }
    let availability = detect_availability();
    let detections = [
        (
            AdapterKind::Codex,
            codex_detection(),
            "Codex credentials not found".to_string(),
        ),
        (
            AdapterKind::Claude,
            claude_detection(),
            availability.claude_status.unavailable_reason().to_string(),
        ),
    ];
    let mut servers = detections
        .into_iter()
        .map(|(kind, evidence, missing)| {
            let launch = launch_for(kind);
            let policy = config.acp.policy(&launch.source_id);
            let detected = evidence.is_some();
            AcpServerInfo {
                id: launch.source_id.clone(),
                label: kind.display_name().to_string(),
                policy,
                detected,
                selected: policy == AcpServerPolicy::Enabled
                    || (policy == AcpServerPolicy::Auto && detected),
                evidence: evidence.unwrap_or(missing),
                launch,
                model_count: 0,
                error: None,
                session_config: Vec::new(),
                subscription: availability
                    .subscriptions
                    .for_adapter(kind)
                    .map(|plan| plan.label.clone()),
            }
        })
        .collect::<Vec<_>>();
    servers.retain(inventory_server_is_visible);
    AcpInventory { servers }
}

/// Re-run local ACP discovery without discarding capabilities learned from
/// background probes during this process.
pub fn rediscover_inventory(config: &Config, previous: &AcpInventory) -> AcpInventory {
    let mut refreshed = discover_inventory(config);
    for server in &mut refreshed.servers {
        if let Some(previous) = previous
            .servers
            .iter()
            .find(|previous| previous.id == server.id)
        {
            server.model_count = previous.model_count;
            server.session_config.clone_from(&previous.session_config);
            server.error.clone_from(&previous.error);
        }
    }
    refreshed
}

fn inventory_server_is_visible(server: &AcpServerInfo) -> bool {
    AdapterKind::from_source_id(&server.id).is_some()
        || server.detected
        || server.error.is_some()
        || server.policy != AcpServerPolicy::Auto
}

type ProbeResult = std::result::Result<probe::AdapterCapabilities, String>;
static WARNED_ADAPTERS: LazyLock<std::sync::Mutex<HashSet<String>>> =
    LazyLock::new(|| std::sync::Mutex::new(HashSet::new()));

async fn probe_launch(
    launch: &AdapterLaunch,
    cwd: &Path,
) -> std::result::Result<probe::AdapterCapabilities, String> {
    probe::adapter_capabilities(
        launch.command.clone(),
        launch.args.clone(),
        launch.env.clone(),
        cwd.to_path_buf(),
        PROBE_TIMEOUT,
    )
    .await
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
    available: Vec<ResolvedAgent>,
    adapter_errors: HashMap<String, String>,
    session_config: HashMap<String, Vec<agent_client_protocol::schema::v1::SessionConfigOption>>,
}

fn resolve_probes(rows: &[Row], mut probes: Vec<(usize, AdapterLaunch, ProbeResult)>) -> Discovery {
    probes.sort_by_key(|(priority, _, _)| *priority);
    let mut resolved = Vec::new();
    let mut adapter_errors = HashMap::new();
    let mut session_config = HashMap::new();
    for (_, launch, capabilities) in probes {
        let capabilities = match capabilities {
            Ok(capabilities) => capabilities,
            Err(reason) => {
                adapter_errors.insert(launch.source_id.clone(), reason);
                tracing::warn!(adapter = %launch.source_id, "roster adapter probe failed");
                continue;
            }
        };
        session_config.insert(
            launch.source_id.clone(),
            capabilities.session_config.clone(),
        );
        let options = capabilities.models;
        let mut matched_values = HashSet::new();
        for row in rows
            .iter()
            .filter(|row| adapter_accepts_model(launch.kind, &row.model))
        {
            if let Some(option) = options
                .iter()
                .find(|option| option_matches(&launch, option, row))
            {
                matched_values.insert(option.value.clone());
                resolved.push(ResolvedAgent {
                    model: row.clone(),
                    model_value: option.value.clone(),
                    launch: launch.clone(),
                    ranked: true,
                    reasoning_effort: None,
                });
            }
        }
        // Adapters advertise models the leaderboard doesn't rank (e.g.
        // claude-acp's `haiku`). Surface them unranked under their advertised
        // value so they stay selectable.
        for option in options
            .iter()
            .filter(|option| !matched_values.contains(&option.value))
        {
            resolved.push(ResolvedAgent {
                model: Row {
                    model: option.value.clone(),
                    reasoning_effort: None,
                    pass_at_1: 0.0,
                    mean_cost_usd: 0.0,
                },
                model_value: option.value.clone(),
                launch: launch.clone(),
                ranked: false,
                reasoning_effort: None,
            });
        }
    }
    resolved.sort_by(|a, b| {
        b.ranked
            .cmp(&a.ranked)
            .then_with(|| b.model.pass_at_1.total_cmp(&a.model.pass_at_1))
            .then_with(|| a.model.mean_cost_usd.total_cmp(&b.model.mean_cost_usd))
            .then_with(|| {
                if a.ranked {
                    a.model.model.cmp(&b.model.model)
                } else {
                    std::cmp::Ordering::Equal
                }
            })
    });
    Discovery {
        available: resolved,
        adapter_errors,
        session_config,
    }
}

fn configured_launches(inventory: &AcpInventory) -> Vec<AdapterLaunch> {
    inventory
        .servers
        .iter()
        .filter(|server| server.selected)
        .map(|server| server.launch.clone())
        .collect()
}

async fn discover_available(rows: &[Row], inventory: &AcpInventory, cwd: &Path) -> Discovery {
    discover_available_with_probe(rows, inventory, cwd, |launch, cwd| async move {
        probe_launch(&launch, &cwd).await
    })
    .await
}

async fn discover_available_with_probe<F, Fut>(
    rows: &[Row],
    inventory: &AcpInventory,
    cwd: &Path,
    probe: F,
) -> Discovery
where
    F: Fn(AdapterLaunch, PathBuf) -> Fut + Clone,
    Fut: std::future::Future<Output = ProbeResult>,
{
    let launches = configured_launches(inventory);
    let probes = stream::iter(launches.into_iter().enumerate().map(|(priority, launch)| {
        let cwd = cwd.to_path_buf();
        let probe = probe.clone();
        async move {
            let capabilities = probe(launch.clone(), cwd).await;
            (priority, launch, capabilities)
        }
    }))
    .buffer_unordered(probe::PROBE_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    resolve_probes(rows, probes)
}

fn explicit<'a>(
    seat: &str,
    selector: &str,
    rows: &[Row],
    available: &'a [ResolvedAgent],
    acp_priority: &[String],
) -> Result<&'a ResolvedAgent> {
    if let Some(candidate) = preferred_route(selector, available, acp_priority) {
        return Ok(candidate);
    }
    if !rows.iter().any(|row| row.model == selector) {
        bail!(
            "{seat} model '{selector}' is not a ranked DeepSWE model and no connected ACP adapter advertised it"
        );
    }
    bail!("{seat} model '{selector}' is unavailable: no connected ACP adapter advertised it")
}

fn preferred_route<'a>(
    model: &str,
    available: &'a [ResolvedAgent],
    acp_priority: &[String],
) -> Option<&'a ResolvedAgent> {
    acp_priority
        .iter()
        .find_map(|source| {
            available.iter().find(|candidate| {
                candidate.model.model == model && candidate.launch.source_id == *source
            })
        })
        .or_else(|| {
            available
                .iter()
                .find(|candidate| candidate.model.model == model)
        })
}

/// Apply primary-only product ranking policy within an optional adapter.
fn preferred_primary_candidate(
    available: &[ResolvedAgent],
    adapter: Option<AdapterKind>,
) -> Option<&ResolvedAgent> {
    let eligible = |candidate: &&ResolvedAgent| {
        candidate.ranked && adapter.is_none_or(|kind| candidate.launch.kind == kind)
    };
    let strongest_opus = available
        .iter()
        .filter(eligible)
        .filter(|candidate| is_claude_opus(&candidate.model.model))
        .map(|candidate| candidate.model.pass_at_1)
        .max_by(f64::total_cmp);
    available.iter().filter(eligible).max_by(|a, b| {
        primary_ranking_score(a, strongest_opus)
            .total_cmp(&primary_ranking_score(b, strongest_opus))
            .then_with(|| b.model.mean_cost_usd.total_cmp(&a.model.mean_cost_usd))
            .then_with(|| b.model.model.cmp(&a.model.model))
    })
}

fn primary_ranking_score(candidate: &ResolvedAgent, strongest_opus: Option<f64>) -> f64 {
    if is_claude_fable_5(&candidate.model.model) {
        // Fable 5 is the Claude flagship for the primary seat. Lift it only
        // enough to outrank Opus without changing review or subagent scoring.
        strongest_opus.map_or(candidate.model.pass_at_1, |opus| {
            candidate.model.pass_at_1.max(opus + f64::EPSILON)
        })
    } else {
        candidate.model.pass_at_1
    }
}

fn is_claude_fable_5(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model == "claude-fable-5" || model.starts_with("claude-fable-5-")
}

fn is_claude_opus(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model == "claude-opus" || model.starts_with("claude-opus-")
}

/// `auto` takes the best-ranked launchable model, except that a strictly
/// larger subscription on the other vendor wins the seat. Quality is worth
/// less than being able to finish: the marginally better model on an entry
/// plan runs the account dry, while the larger plan carries the whole day.
fn choose_primary_auto<'a>(
    available: &'a [ResolvedAgent],
    subscriptions: &Subscriptions,
    acp_priority: &[String],
) -> Option<&'a ResolvedAgent> {
    let best_model = preferred_primary_candidate(available, None)?;
    let best = preferred_route(&best_model.model.model, available, acp_priority)
        .expect("ranked model has a launchable route");
    let Some(favored) = subscriptions.favored() else {
        return Some(best);
    };
    let Some(preferred) = preferred_primary_candidate(available, Some(favored)) else {
        return Some(best);
    };
    if best.model.model != preferred.model.model || best.launch.kind != preferred.launch.kind {
        tracing::info!(
            model = %preferred.model.model,
            adapter = %preferred.launch.source_id,
            subscription = ?subscriptions.for_adapter(favored).map(|plan| &plan.label),
            "auto primary routed to the larger subscription instead of the highest-ranked model"
        );
    }
    Some(preferred)
}

/// Shared automatic selection for review and subagent seats. Prefer the
/// cheapest Pareto-efficient distinct model at the Sonnet quality floor; when
/// none clears it, keep costs below the primary before falling back to it.
fn choose_secondary_auto(
    primary: &ResolvedAgent,
    rows: &[Row],
    available: &[ResolvedAgent],
    acp_priority: &[String],
) -> Option<ResolvedAgent> {
    let distinct = available
        .iter()
        .filter(|role| role.ranked)
        .filter(|role| role.model.model != primary.model.model)
        .cloned()
        .collect::<Vec<_>>();
    let launchable_rows: Vec<Row> = distinct.iter().map(|role| role.model.clone()).collect();
    let candidate = deepswe::sonnet_anchor(rows)
        .and_then(|anchor| {
            deepswe::pareto_frontier(&launchable_rows)
                .into_iter()
                .filter(|row| row.pass_at_1 >= anchor.pass_at_1)
                .min_by(|a, b| {
                    a.mean_cost_usd
                        .total_cmp(&b.mean_cost_usd)
                        .then_with(|| b.pass_at_1.total_cmp(&a.pass_at_1))
                })
        })
        .or_else(|| {
            deepswe::pareto_frontier(&launchable_rows)
                .into_iter()
                .filter(|row| row.mean_cost_usd < primary.model.mean_cost_usd)
                .max_by(|a, b| {
                    a.pass_at_1
                        .total_cmp(&b.pass_at_1)
                        .then_with(|| b.mean_cost_usd.total_cmp(&a.mean_cost_usd))
                })
        });
    candidate
        .and_then(|row| preferred_route(&row.model, &distinct, acp_priority).cloned())
        .or_else(|| Some(primary.clone()))
}

fn choose_review_auto(
    primary: &ResolvedAgent,
    rows: &[Row],
    available: &[ResolvedAgent],
    acp_priority: &[String],
) -> Option<ResolvedAgent> {
    choose_secondary_auto(primary, rows, available, acp_priority)
}

fn resolve_review_supervisor(
    selector: &str,
    primary: &ResolvedAgent,
    rows: &[Row],
    available: &[ResolvedAgent],
    acp_priority: &[String],
    review_enabled: bool,
) -> Result<Option<ResolvedAgent>> {
    if !review_enabled {
        return Ok(None);
    }
    if matches!(selector, crate::config::DISABLED_MODEL | "none") {
        bail!("Review model cannot be disabled; use agent.discrete_review = false");
    }
    Ok(if selector == "auto" {
        choose_review_auto(primary, rows, available, acp_priority)
    } else {
        Some(explicit("Review", selector, rows, available, acp_priority)?.clone())
    })
}

fn resolve_subagent_default(
    selector: &str,
    rows: &[Row],
    available: &[ResolvedAgent],
    primary: &ResolvedAgent,
    acp_priority: &[String],
) -> Result<Option<ResolvedAgent>> {
    if selector == crate::config::DISABLED_MODEL || selector == "none" {
        Ok(None)
    } else if selector == "auto" {
        Ok(choose_secondary_auto(
            primary,
            rows,
            available,
            acp_priority,
        ))
    } else {
        explicit("Subagent", selector, rows, available, acp_priority).map(|role| Some(role.clone()))
    }
}

fn unavailable_reason(
    row: &Row,
    config: &Config,
    availability: &Availability,
    adapter_errors: &HashMap<String, String>,
) -> String {
    let mut reasons = Vec::new();
    match adapter_kind(&row.model) {
        None => reasons.push("no built-in ACP adapter serves this model's provider".to_string()),
        Some(native) => {
            let native_source = launch_for(native).source_id;
            let native_detected = match native {
                AdapterKind::Codex => {
                    availability.codex_credentials
                        || config.acp.policy("codex-acp") == AcpServerPolicy::Enabled
                }
                AdapterKind::Claude => {
                    availability.claude_status.logged_in()
                        || config.acp.policy("claude-acp") == AcpServerPolicy::Enabled
                }
                // Unreachable via adapter_kind(); a registered external
                // adapter is detected by definition.
                AdapterKind::External => true,
            };
            let native_enabled = config.acp.policy(&native_source) != AcpServerPolicy::Disabled;
            if !native_enabled {
                reasons.push(format!("{native_source} is disabled in config"));
            } else if native_detected {
                reasons.push(adapter_errors.get(&native_source).map_or_else(
                    || format!("{native_source} did not advertise this model"),
                    |reason| format!("{native_source}: {reason}"),
                ));
            } else if let Some(reason) = availability.missing_reason(&row.model) {
                reasons.push(reason.to_string());
            }
        }
    }
    reasons.sort();
    reasons.dedup();
    reasons.join("; ")
}

pub async fn resolve(config: &Config, cwd: &Path) -> Result<Roster> {
    let mut config = config.clone();
    resolve_recovering(&mut config, cwd)
        .await
        .map(|(roster, _)| roster)
}

/// Resolve the roster and reset persisted explicit model choices only when a
/// successful adapter probe proves they are no longer offered. Callers that
/// own the config file should save `config` when notices are returned.
pub async fn resolve_recovering(config: &mut Config, cwd: &Path) -> Result<(Roster, Vec<String>)> {
    // Callers that never run setup — headless runs, remote hosts — still get
    // the default team rather than falling back to bare model ranking.
    config.apply_default_team();
    let leaderboard = deepswe::load(
        &deepswe::default_cache_path(),
        deepswe::CACHE_TTL,
        deepswe::DEFAULT_URL,
    )
    .await;
    let rows = natively_served(deepswe::eligible_high(&leaderboard.rows));
    let availability = detect_availability();
    let inventory = discover_inventory(config);
    let discovery = discover_available(&rows, &inventory, cwd).await;
    let notices = recover_unavailable_explicit_models(config, &inventory, &discovery);
    let mut roster = assemble_roster(config, &rows, &availability, inventory, discovery)?;
    roster.warnings.extend(notices.iter().cloned());
    roster.warnings.sort();
    Ok((roster, notices))
}

fn recover_unavailable_explicit_models(
    config: &mut Config,
    inventory: &AcpInventory,
    discovery: &Discovery,
) -> Vec<String> {
    recover_unavailable_explicit_models_with_external(
        config,
        inventory,
        discovery,
        external_adapter().is_some(),
    )
}

fn recover_unavailable_explicit_models_with_external(
    config: &mut Config,
    inventory: &AcpInventory,
    discovery: &Discovery,
    external_registered: bool,
) -> Vec<String> {
    let mut notices = Vec::new();
    let source_was_probed = |source: &str| {
        inventory
            .servers
            .iter()
            .any(|server| server.selected && server.id == source)
            && !discovery.adapter_errors.contains_key(source)
    };
    let all_selected_sources_were_probed = inventory
        .servers
        .iter()
        .filter(|server| server.selected)
        .all(|server| !discovery.adapter_errors.contains_key(&server.id));
    let model_is_missing = |model: &str| {
        !discovery
            .available
            .iter()
            .any(|candidate| candidate.model.model == model)
    };
    // With a platform adapter registered it is the only route, so a model's
    // native source (codex-acp/claude-acp) is never in the inventory and
    // "was that source probed?" would always answer no — leaving a stale pin
    // to fail resolution on every launch with no way to reset it in-app.
    // Judge such pins against the sources that actually were probed.
    let conclusively_missing = |model: &str| match adapter_kind(model) {
        Some(kind) if !external_registered => {
            model_is_missing(model) && source_was_probed(&launch_for(kind).source_id)
        }
        _ => model_is_missing(model) && all_selected_sources_were_probed,
    };

    for (label, model) in [
        ("Agent", &mut config.agent.model),
        ("Review", &mut config.review.model),
        ("Subagent", &mut config.subagents.model),
    ] {
        if matches!(
            model.as_str(),
            "auto" | crate::config::DISABLED_MODEL | "none"
        ) || !conclusively_missing(model)
        {
            continue;
        }
        let unavailable = model.clone();
        *model = "auto".to_string();
        notices.push(format!(
            "{label} model '{unavailable}' is no longer offered; switched to automatic selection"
        ));
    }
    notices
}

/// Bind the primary agent and the default subagent model plus the model
/// catalog from the completed adapter probes.
fn assemble_roster(
    config: &Config,
    rows: &[Row],
    availability: &Availability,
    mut inventory: AcpInventory,
    discovery: Discovery,
) -> Result<Roster> {
    for server in &mut inventory.servers {
        server.model_count = discovery
            .available
            .iter()
            .filter(|role| role.launch.source_id == server.id)
            .count();
        server.error = discovery.adapter_errors.get(&server.id).cloned();
        server.session_config = discovery
            .session_config
            .get(&server.id)
            .cloned()
            .unwrap_or_default();
    }
    let available = discovery.available;
    let mut choices = rows
        .iter()
        .map(|row| {
            let candidate = available
                .iter()
                .find(|candidate| candidate.model.model == row.model);
            let launchable = candidate.is_some();
            let disabled_reason = (!launchable)
                .then(|| unavailable_reason(row, config, availability, &discovery.adapter_errors));
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
        if let Some(external) = inventory
            .servers
            .iter()
            .find(|server| server.launch.kind == AdapterKind::External)
        {
            bail!(
                "no model is launchable{diagnostic}: {} did not advertise a usable model",
                external.label
            );
        }
        bail!("no model is launchable{diagnostic}: install or authenticate Codex or Claude Code");
    }

    if matches!(
        config.agent.model.as_str(),
        crate::config::DISABLED_MODEL | "none"
    ) {
        bail!("the primary agent cannot be disabled");
    }
    let primary_available = candidates_for_selector(
        &available,
        &config.agent.model,
        config.agent.acp_source.as_deref(),
    );
    if config.agent.model == "auto"
        && primary_available.is_empty()
        && let Some(source) = &config.agent.acp_source
    {
        bail!("Agent ACP source '{source}' has no launchable models");
    }
    let primary = if config.agent.model == "auto" {
        choose_primary_auto(
            &primary_available,
            &availability.subscriptions,
            &config.agent.acp_priority,
        )
        .or_else(|| {
            // When only an external adapter is connected (e.g. the Android
            // sidecar), no ranked DeepSWE row is launchable; fall back to its
            // first advertised model instead of failing Auto outright.
            primary_available
                .iter()
                .find(|candidate| candidate.launch.kind == AdapterKind::External)
        })
        .ok_or_else(|| anyhow!("Agent Auto requires at least one ranked DeepSWE model"))?
    } else {
        explicit(
            "Agent",
            &config.agent.model,
            rows,
            &primary_available,
            &config.agent.acp_priority,
        )?
    };
    let review_available = candidates_for_selector(
        &available,
        &config.review.model,
        config.review.acp_source.as_deref(),
    );
    let mut review_supervisor = resolve_review_supervisor(
        &config.review.model,
        primary,
        rows,
        &review_available,
        &config.review.acp_priority,
        config.agent.discrete_review,
    )?;
    let subagent_available = candidates_for_selector(
        &available,
        &config.subagents.model,
        config.subagents.acp_source.as_deref(),
    );
    let mut subagent_default = resolve_subagent_default(
        &config.subagents.model,
        rows,
        &subagent_available,
        primary,
        &config.subagents.acp_priority,
    )?;

    // Attach each seat's per-invocation reasoning-effort override (from
    // `--model`/`--subagent-model MODEL+effort`, threaded via `Config`). This
    // only touches the exact agent selected for the seat; failover
    // alternates discovered elsewhere in `available` are unaffected.
    let mut primary = primary.clone();
    primary.reasoning_effort = config.agent.reasoning_effort.clone();
    if let Some(review_supervisor) = review_supervisor.as_mut() {
        review_supervisor.reasoning_effort = config.review.reasoning_effort.clone();
    }
    if let Some(subagent_default) = subagent_default.as_mut() {
        subagent_default.reasoning_effort = config.subagents.reasoning_effort.clone();
    }

    let mut warned = WARNED_ADAPTERS
        .lock()
        .expect("adapter warning set poisoned");
    let mut warnings = discovery
        .adapter_errors
        .iter()
        .filter(|(adapter, _)| warned.insert((*adapter).clone()))
        .map(|(adapter, reason)| format!("{adapter} unavailable: {reason}"))
        .collect::<Vec<_>>();
    drop(warned);
    if subagent_default.is_none()
        && !matches!(
            config.subagents.model.as_str(),
            crate::config::DISABLED_MODEL | "none"
        )
    {
        warnings.push(
            "subagent delegation is disabled: no launchable subagent model is available. \
             Authenticate a built-in Codex or Claude ACP route, then restart and review the route \
             in /mjconfig."
                .to_string(),
        );
    }
    if config.agent.discrete_review && review_supervisor.is_none() {
        warnings.push(
            "agentic review supervisor is disabled: no distinct launchable review model is available. \
             Install or authenticate another supported ACP adapter, select an explicit review model, \
             or turn off discrete review."
                .to_string(),
        );
    }
    if let Some(notice) = config.newer_build_notice() {
        warnings.push(notice);
    }
    warnings.sort();
    Ok(Roster {
        primary,
        review_supervisor,
        subagent_default,
        available,
        choices,
        warnings,
        inventory,
        subagent_acp_priority: config.subagents.acp_priority.clone(),
        subagent_acp_source: (config.subagents.model == "auto")
            .then(|| config.subagents.acp_source.clone())
            .flatten(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PermissionPreset;
    use agent_client_protocol::schema::v1::{SessionConfigOption, SessionConfigSelectOption};

    #[test]
    fn rediscovery_preserves_probe_only_inventory_fields() {
        let config = config_with_a_visible_builtin();
        let mut previous = discover_inventory(&config);
        let server = previous.servers.first_mut().expect("visible ACP server");
        let server_id = server.id.clone();
        server.model_count = 3;
        server.session_config = vec![SessionConfigOption::select(
            "service_tier",
            "Service tier",
            "default",
            vec![SessionConfigSelectOption::new("default", "Default")],
        )];

        let refreshed = rediscover_inventory(&config, &previous);
        let server = refreshed
            .servers
            .iter()
            .find(|server| server.id == server_id)
            .expect("same server");
        assert_eq!(server.model_count, 3);
        assert_eq!(server.session_config[0].id.to_string(), "service_tier");
    }

    #[test]
    fn claude_auth_status_reports_missing_credentials_without_requiring_a_cli() {
        assert_eq!(
            ClaudeAuthStatus::NotLoggedIn.unavailable_reason(),
            "Claude credentials not found"
        );
    }

    #[test]
    fn permission_presets_map_to_provider_controls() {
        let mut env = HashMap::new();
        let codex = configure_permissions(AdapterKind::Codex, PermissionPreset::Auto, &mut env)
            .expect("Codex preset");
        assert_eq!(codex.config_id, "mode");
        assert_eq!(codex.value, "agent");
        assert_eq!(codex.manual_fallback.as_deref(), Some("read-only"));

        let claude = configure_permissions(AdapterKind::Claude, PermissionPreset::Manual, &mut env)
            .expect("Claude preset");
        assert_eq!(claude.value, "default");
    }

    #[test]
    fn omitted_permission_mode_sends_no_config() {
        let mut env = HashMap::new();
        let omitted: Option<PermissionPreset> = None;
        let sent =
            omitted.and_then(|mode| configure_permissions(AdapterKind::Claude, mode, &mut env));

        assert_eq!(sent, None);
    }

    fn option(value: &str) -> probe::ModelOption {
        probe::ModelOption {
            value: value.to_string(),
            name: value.to_string(),
            description: None,
        }
    }

    fn capabilities(values: &[&str]) -> ProbeResult {
        Ok(probe::AdapterCapabilities {
            models: values.iter().map(|value| option(value)).collect(),
            session_config: Vec::new(),
        })
    }

    fn role(model: &str, pass_at_1: f64) -> ResolvedAgent {
        role_at(model, pass_at_1, 1.0)
    }

    fn role_at(model: &str, pass_at_1: f64, mean_cost_usd: f64) -> ResolvedAgent {
        ResolvedAgent {
            model: Row {
                model: model.to_string(),
                reasoning_effort: Some("high".to_string()),
                pass_at_1,
                mean_cost_usd,
            },
            model_value: model.to_string(),
            launch: launch_for(adapter_kind(model).expect("model has a built-in adapter")),
            ranked: true,
            reasoning_effort: None,
        }
    }

    fn plans(claude: &str, codex: &str) -> Subscriptions {
        Subscriptions {
            claude: Some(crate::subscription::Subscription {
                label: claude.to_string(),
                capacity: match claude {
                    "max20" => 20.0,
                    "max5" => 5.0,
                    _ => 1.0,
                },
            }),
            codex: Some(crate::subscription::Subscription {
                label: codex.to_string(),
                capacity: if codex == "pro" { 20.0 } else { 1.0 },
            }),
        }
    }

    fn sidecar_adapter() -> ExternalAdapter {
        ExternalAdapter {
            id: "sidecar".to_string(),
            label: "Sidecar".to_string(),
            command: PathBuf::from("/opt/sidecar/acp"),
            args: vec!["--serve".to_string()],
            env: HashMap::new(),
            evidence: "bundled sibling /opt/sidecar/acp".to_string(),
        }
    }

    fn external_role(model: &str) -> ResolvedAgent {
        ResolvedAgent {
            model: Row {
                model: model.to_string(),
                reasoning_effort: None,
                pass_at_1: 0.0,
                mean_cost_usd: 0.0,
            },
            model_value: model.to_string(),
            launch: external_launch(&sidecar_adapter()),
            ranked: false,
            reasoning_effort: None,
        }
    }

    #[test]
    fn external_server_is_always_selected() {
        let external = sidecar_adapter();
        let mut config = Config::default();

        let info = external_server_info(&external, &config);
        assert!(info.detected);
        assert!(info.selected);
        assert_eq!(info.launch.kind, AdapterKind::External);
        assert_eq!(info.launch.command, PathBuf::from("/opt/sidecar/acp"));

        // The platform adapter is the only route on its build; even a stale
        // Disabled policy in the config must not deselect it, or nothing is
        // launchable and every start fails.
        config
            .acp
            .policies
            .insert("sidecar".to_string(), AcpServerPolicy::Disabled);
        assert!(external_server_info(&external, &config).selected);
    }

    #[test]
    fn external_probe_results_surface_every_model_unranked() {
        let rows = vec![Row {
            model: "gpt-5-5".to_string(),
            reasoning_effort: None,
            pass_at_1: 0.9,
            mean_cost_usd: 1.0,
        }];
        let launch = external_launch(&sidecar_adapter());

        let discovery = resolve_probes(&rows, vec![(0, launch, capabilities(&["local-coder"]))]);

        let candidate = discovery.available.first().expect("advertised model");
        assert_eq!(discovery.available.len(), 1);
        assert!(
            !candidate.ranked,
            "external adapters never claim ranked rows"
        );
        assert_eq!(candidate.model.model, "local-coder");
        assert_eq!(candidate.launch.kind, AdapterKind::External);
    }

    #[test]
    fn auto_primary_falls_back_to_an_external_model_when_nothing_is_ranked() {
        let discovery = Discovery {
            available: vec![external_role("anvil-coder"), external_role("anvil-mini")],
            adapter_errors: HashMap::new(),
            session_config: HashMap::new(),
        };
        let availability = Availability {
            codex_credentials: false,
            claude_status: ClaudeAuthStatus::NotLoggedIn,
            subscriptions: Subscriptions::default(),
        };

        let roster = assemble_roster(
            &Config::default(),
            &[],
            &availability,
            AcpInventory::default(),
            discovery,
        )
        .expect("external models keep Auto launchable");

        assert_eq!(roster.primary.launch.kind, AdapterKind::External);
        assert_eq!(roster.primary.model.model, "anvil-coder");
    }

    #[test]
    fn external_auto_preserves_the_adapters_default_model_order() {
        let launch = external_launch(&sidecar_adapter());
        let discovery = resolve_probes(
            &[],
            vec![(0, launch, capabilities(&["zeta-default", "alpha-other"]))],
        );
        let availability = Availability {
            codex_credentials: false,
            claude_status: ClaudeAuthStatus::NotLoggedIn,
            subscriptions: Subscriptions::default(),
        };

        let roster = assemble_roster(
            &Config::default(),
            &[],
            &availability,
            AcpInventory::default(),
            discovery,
        )
        .expect("external models keep Auto launchable");

        assert_eq!(roster.primary.model.model, "zeta-default");
    }

    #[test]
    fn external_probe_failure_names_the_platform_team() {
        let external = sidecar_adapter();
        let inventory = AcpInventory {
            servers: vec![external_server_info(&external, &Config::default())],
        };
        let discovery = Discovery {
            available: Vec::new(),
            adapter_errors: HashMap::from([(external.id.clone(), "probe timed out".to_string())]),
            session_config: HashMap::new(),
        };

        let error = assemble_roster(
            &Config::default(),
            &[],
            &Availability {
                codex_credentials: false,
                claude_status: ClaudeAuthStatus::NotLoggedIn,
                subscriptions: Subscriptions::default(),
            },
            inventory,
            discovery,
        )
        .expect_err("missing external models must fail");

        assert!(
            error
                .to_string()
                .contains("Sidecar did not advertise a usable model")
        );
        assert!(error.to_string().contains("probe timed out"));
    }

    #[test]
    fn auto_primary_takes_the_top_ranked_model_without_a_subscription_gap() {
        let available = vec![
            role("claude-fable-5", 0.70),
            role("gpt-5-6-sol", 0.68),
            role("gpt-5-5", 0.65),
        ];

        assert_eq!(
            choose_primary_auto(&available, &plans("pro", "plus"), &[])
                .expect("ranked model")
                .model
                .model,
            "claude-fable-5"
        );
        assert_eq!(
            choose_primary_auto(&available, &Subscriptions::default(), &[])
                .expect("ranked model")
                .model
                .model,
            "claude-fable-5"
        );
    }

    #[test]
    fn auto_primary_prefers_fable_5_when_opus_would_otherwise_win() {
        let available = vec![
            role("claude-opus-5", 0.75),
            role("claude-fable-5", 0.65),
            role("gpt-5-6-sol", 0.60),
        ];

        for subscriptions in [Subscriptions::default(), plans("max20", "plus")] {
            assert_eq!(
                choose_primary_auto(&available, &subscriptions, &[])
                    .expect("primary model")
                    .model
                    .model,
                "claude-fable-5"
            );
        }
    }

    #[test]
    fn auto_primary_keeps_a_higher_ranked_non_opus_model() {
        let available = vec![
            role("gpt-5-6-sol", 0.80),
            role("claude-opus-5", 0.75),
            role("claude-fable-5", 0.65),
        ];

        assert_eq!(
            choose_primary_auto(&available, &Subscriptions::default(), &[])
                .expect("primary model")
                .model
                .model,
            "gpt-5-6-sol"
        );
    }

    #[test]
    fn source_constraint_keeps_auto_selection_within_codex() {
        let available = vec![
            role("claude-fable-5", 0.70),
            role("gpt-5-6-sol", 0.68),
            role("gpt-5-5", 0.65),
        ];
        let codex = source_candidates(&available, Some("codex-acp"));

        let chosen =
            choose_primary_auto(&codex, &plans("max20", "plus"), &[]).expect("Codex model");

        assert_eq!(chosen.model.model, "gpt-5-6-sol");
        assert!(
            codex
                .iter()
                .all(|candidate| candidate.launch.source_id == "codex-acp")
        );
    }

    #[test]
    fn explicit_model_ignores_a_runtime_team_route() {
        let primary = role("gpt-5-6-sol", 0.70);
        let reviewer = role("claude-fable-5", 0.64);
        let rows = vec![primary.model.clone(), reviewer.model.clone()];
        let discovery = Discovery {
            available: vec![primary.clone(), reviewer],
            adapter_errors: HashMap::new(),
            session_config: HashMap::new(),
        };
        let availability = Availability {
            codex_credentials: false,
            claude_status: ClaudeAuthStatus::NotLoggedIn,
            subscriptions: Subscriptions::default(),
        };
        let mut config = Config::default();
        config.agent.model = primary.model.model.clone();
        config.agent.acp_source = Some("claude-acp".to_string());
        config.subagents.model = crate::config::DISABLED_MODEL.to_string();

        let roster = assemble_roster(
            &config,
            &rows,
            &availability,
            AcpInventory::default(),
            discovery,
        )
        .expect("explicit model resolves through its own adapter");

        assert_eq!(roster.primary.model.model, "gpt-5-6-sol");
        assert_eq!(roster.primary.launch.source_id, "codex-acp");
    }

    #[test]
    fn auto_primary_moves_to_the_larger_subscription() {
        let available = vec![
            role("claude-fable-5", 0.70),
            role("gpt-5-6-sol", 0.68),
            role("gpt-5-5", 0.65),
        ];

        // Claude Pro against ChatGPT Pro: the best Codex model wins the seat
        // even though the Claude model ranks higher.
        let chosen =
            choose_primary_auto(&available, &plans("pro", "pro"), &[]).expect("favored model");
        assert_eq!(chosen.model.model, "gpt-5-6-sol");
        assert_eq!(chosen.launch.kind, AdapterKind::Codex);

        let chosen =
            choose_primary_auto(&available, &plans("max20", "plus"), &[]).expect("favored model");
        assert_eq!(chosen.model.model, "claude-fable-5");
    }

    #[test]
    fn auto_primary_falls_back_when_the_larger_subscription_has_no_model() {
        let available = vec![role("claude-fable-5", 0.70), role("claude-sonnet-5", 0.60)];

        assert_eq!(
            choose_primary_auto(&available, &plans("pro", "pro"), &[])
                .expect("ranked fallback")
                .model
                .model,
            "claude-fable-5"
        );
    }

    #[test]
    fn auto_review_uses_the_same_cost_quality_frontier_as_subagents() {
        let available = vec![
            role_at("gpt-5-6-sol", 0.70, 3.5),
            role_at("gpt-5-5", 0.65, 1.2),
            role_at("claude-sonnet-5", 0.60, 7.0),
        ];
        let rows = available
            .iter()
            .map(|role| role.model.clone())
            .collect::<Vec<_>>();

        assert_eq!(
            choose_review_auto(&available[0], &rows, &available, &[])
                .expect("cost-efficient review model")
                .model
                .model,
            "gpt-5-5"
        );
    }

    #[test]
    fn auto_review_uses_the_sonnet_quality_floor() {
        let available = vec![
            role_at("gpt-5-6-sol", 0.80, 3.0),
            role_at("claude-opus-5", 0.75, 6.0),
            role_at("claude-sonnet-5", 0.65, 7.0),
        ];
        let rows = available
            .iter()
            .map(|role| role.model.clone())
            .collect::<Vec<_>>();

        assert_eq!(
            choose_review_auto(&available[0], &rows, &available, &[])
                .expect("review model")
                .model
                .model,
            "claude-opus-5"
        );
    }

    #[test]
    fn auto_review_falls_back_to_the_strongest_distinct_model() {
        let available = vec![
            role_at("gpt-5-6-sol", 0.70, 3.0),
            role_at("gpt-5-5", 0.45, 1.0),
        ];
        let rows = vec![
            available[0].model.clone(),
            available[1].model.clone(),
            role_at("claude-sonnet-5", 0.50, 7.0).model,
        ];

        assert_eq!(
            choose_review_auto(&available[0], &rows, &available, &[])
                .expect("strongest fallback")
                .model
                .model,
            "gpt-5-5"
        );
    }

    #[test]
    fn auto_review_reuses_primary_when_no_distinct_model_exists() {
        let available = vec![role("gpt-5-6-sol", 0.70)];
        let rows = available
            .iter()
            .map(|role| role.model.clone())
            .collect::<Vec<_>>();

        assert_eq!(
            choose_review_auto(&available[0], &rows, &available, &[])
                .expect("primary fallback")
                .model
                .model,
            "gpt-5-6-sol"
        );
    }

    #[test]
    fn explicit_review_can_match_primary_model() {
        let available = vec![role("gpt-5-6-sol", 0.70), role("gpt-5-5", 0.65)];
        let rows = available
            .iter()
            .map(|candidate| candidate.model.clone())
            .collect::<Vec<_>>();

        let review =
            resolve_review_supervisor("gpt-5-6-sol", &available[0], &rows, &available, &[], true)
                .expect("explicit review is user-forced")
                .expect("review role");

        assert_eq!(review.model.model, "gpt-5-6-sol");
    }

    #[test]
    fn auto_review_rebinds_after_primary_is_pinned() {
        let mut config = Config::default();
        config.review.reasoning_effort = Some("high".to_string());
        config.review.acp_source = Some("codex-acp".to_string());
        let gpt = role("gpt-5-6-sol", 0.70);
        let claude = role("claude-fable-5", 0.64);
        let mut roster = Roster {
            primary: gpt.clone(),
            review_supervisor: Some(claude.clone()),
            subagent_default: None,
            available: vec![gpt, claude.clone()],
            choices: vec![
                ModelChoice {
                    model: "gpt-5-6-sol".to_string(),
                    pass_at_1: 0.70,
                    mean_cost_usd: 1.0,
                    available: true,
                    disabled_reason: None,
                    adapter: Some("codex-acp".to_string()),
                    ranked: true,
                },
                ModelChoice {
                    model: "claude-fable-5".to_string(),
                    pass_at_1: 0.64,
                    mean_cost_usd: 1.0,
                    available: true,
                    disabled_reason: None,
                    adapter: Some("claude-acp".to_string()),
                    ranked: true,
                },
                ModelChoice {
                    model: "claude-sonnet-5".to_string(),
                    pass_at_1: 0.60,
                    mean_cost_usd: 7.0,
                    available: false,
                    disabled_reason: None,
                    adapter: None,
                    ranked: true,
                },
            ],
            warnings: Vec::new(),
            inventory: AcpInventory::default(),
            subagent_acp_priority: Vec::new(),
            subagent_acp_source: None,
        };

        roster.primary = claude;
        rebind_auto_review_for_primary(&mut roster, &config);

        let review = roster.review_supervisor.expect("review rebound");
        assert_eq!(review.model.model, "gpt-5-6-sol");
        assert_eq!(review.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn explicit_review_is_not_rebound_after_primary_is_pinned() {
        let mut config = Config::default();
        config.review.model = "claude-fable-5".to_string();
        let gpt = role("gpt-5-6-sol", 0.70);
        let claude = role("claude-fable-5", 0.64);
        let mut roster = Roster {
            primary: gpt.clone(),
            review_supervisor: Some(claude.clone()),
            subagent_default: None,
            available: vec![gpt, claude.clone()],
            choices: Vec::new(),
            warnings: Vec::new(),
            inventory: AcpInventory::default(),
            subagent_acp_priority: Vec::new(),
            subagent_acp_source: None,
        };

        roster.primary = claude;
        rebind_auto_review_for_primary(&mut roster, &config);

        assert_eq!(
            roster
                .review_supervisor
                .expect("explicit review kept")
                .model
                .model,
            "claude-fable-5"
        );
    }

    #[test]
    fn subagent_source_constraint_applies_to_failover_pool() {
        let primary = role("gpt-5-6-sol", 0.70);
        let worker = role("gpt-5-5", 0.65);
        let claude = role("claude-fable-5", 0.64);
        let roster = Roster {
            primary: primary.clone(),
            review_supervisor: None,
            subagent_default: Some(worker.clone()),
            available: vec![primary, worker, claude],
            choices: Vec::new(),
            warnings: Vec::new(),
            inventory: AcpInventory::default(),
            subagent_acp_priority: Vec::new(),
            subagent_acp_source: Some("codex-acp".to_string()),
        };

        assert!(
            subagent_failover_roles(&roster)
                .iter()
                .all(|candidate| candidate.launch.source_id == "codex-acp")
        );
    }

    #[test]
    fn provider_routes_are_model_first() {
        assert_eq!(adapter_kind("gpt-5-6-sol"), Some(AdapterKind::Codex));
        assert_eq!(adapter_kind("claude-sonnet-5"), Some(AdapterKind::Claude));
        assert_eq!(adapter_kind("kimi-k2-7-code"), None);
        assert_eq!(adapter_kind("gemini-3-5-flash"), None);
        assert_eq!(adapter_kind("glm-5-2"), None);
    }

    #[test]
    fn unserved_providers_are_dropped_from_the_ranked_catalog() {
        let rows = vec![
            role_at("gpt-5-6-sol", 0.7, 1.0).model,
            role_at("claude-sonnet-5", 0.6, 1.0).model,
            Row {
                model: "gemini-3-5-flash".to_string(),
                reasoning_effort: Some("high".to_string()),
                pass_at_1: 0.9,
                mean_cost_usd: 0.1,
            },
            Row {
                model: "glm-5-2".to_string(),
                reasoning_effort: Some("high".to_string()),
                pass_at_1: 0.8,
                mean_cost_usd: 0.1,
            },
        ];

        let served = natively_served(rows);

        assert_eq!(
            served
                .iter()
                .map(|row| row.model.as_str())
                .collect::<Vec<_>>(),
            ["gpt-5-6-sol", "claude-sonnet-5"]
        );
    }

    #[test]
    fn credential_files_require_a_nonempty_supported_token() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("credentials.json");
        let pointers = ["/oauth/accessToken", "/apiKey"];

        std::fs::write(&path, r#"{"oauth":{"accessToken":"token"}}"#).expect("write");
        assert!(credential_file_has_any(&path, &pointers));

        std::fs::write(&path, r#"{"oauth":{"accessToken":"  "}}"#).expect("write");
        assert!(!credential_file_has_any(&path, &pointers));

        std::fs::write(&path, "not json").expect("write");
        assert!(!credential_file_has_any(&path, &pointers));
    }

    #[test]
    fn adapter_display_names_match_the_primary_acp_products() {
        assert_eq!(AdapterKind::Codex.display_name(), "Codex");
        assert_eq!(AdapterKind::Claude.display_name(), "Claude Code");
    }

    #[test]
    fn configured_launches_exclude_disabled_adapters() {
        let mut config = Config::default();
        config.set_acp_server_policy("codex-acp", AcpServerPolicy::Disabled);
        let mut inventory = discover_inventory(&config);
        for server in &mut inventory.servers {
            if server.id == "claude-acp" {
                server.detected = true;
                server.selected = true;
            }
        }
        let ids = configured_launches(&inventory)
            .into_iter()
            .map(|launch| launch.source_id)
            .collect::<Vec<_>>();
        assert!(ids.contains(&"claude-acp".to_string()));
        assert!(!ids.contains(&"codex-acp".to_string()));
    }

    #[tokio::test]
    async fn discovery_probes_every_selected_adapter_on_each_resolution() {
        let mut config = Config::default();
        for id in ["codex-acp", "claude-acp"] {
            config.set_acp_server_policy(id, AcpServerPolicy::Enabled);
        }
        let inventory = discover_inventory(&config);
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let probe = |launch: AdapterLaunch, _cwd: PathBuf| {
            let calls = std::sync::Arc::clone(&calls);
            async move {
                calls
                    .lock()
                    .expect("probe call lock")
                    .push(launch.source_id);
                Ok(probe::AdapterCapabilities {
                    models: Vec::new(),
                    session_config: Vec::new(),
                })
            }
        };

        discover_available_with_probe(&[], &inventory, Path::new("."), probe).await;
        discover_available_with_probe(&[], &inventory, Path::new("."), probe).await;

        let mut calls = calls.lock().expect("probe call lock").clone();
        calls.sort();
        assert_eq!(
            calls,
            vec![
                "claude-acp".to_string(),
                "claude-acp".to_string(),
                "codex-acp".to_string(),
                "codex-acp".to_string(),
            ]
        );
    }

    #[test]
    fn explicit_policy_selects_a_builtin_without_detection() {
        let mut config = Config::default();
        config.set_acp_server_policy("claude-acp", AcpServerPolicy::Enabled);
        let inventory = discover_inventory(&config);
        let claude = inventory
            .servers
            .iter()
            .find(|server| server.id == "claude-acp")
            .expect("claude-acp");
        assert!(claude.selected);
        assert_eq!(claude.policy, AcpServerPolicy::Enabled);
    }

    #[test]
    fn builtins_remain_visible_without_detection() {
        let launch = launch_for(AdapterKind::Codex);
        let mut server = AcpServerInfo {
            id: launch.source_id.clone(),
            label: "Codex".to_string(),
            policy: AcpServerPolicy::Enabled,
            detected: false,
            selected: true,
            evidence: "Codex credentials not found".to_string(),
            launch,
            model_count: 0,
            error: None,
            session_config: Vec::new(),
            subscription: None,
        };

        assert!(inventory_server_is_visible(&server));
        server.policy = AcpServerPolicy::Disabled;
        assert!(inventory_server_is_visible(&server));
        server.policy = AcpServerPolicy::Auto;
        assert!(inventory_server_is_visible(&server));
    }

    #[test]
    fn auto_misses_are_inventory_state_not_probe_errors() {
        let mut config = Config::default();
        for id in ["codex-acp", "claude-acp"] {
            config.set_acp_server_policy(id, AcpServerPolicy::Disabled);
        }
        let inventory = discover_inventory(&config);
        assert!(
            inventory
                .servers
                .iter()
                .all(|server| server.error.is_none())
        );
    }

    #[test]
    fn failed_adapters_are_excluded_with_sanitized_reasons() {
        let rows = vec![
            role_at("gpt-5-5", 0.6, 5.0).model,
            role_at("claude-opus-4-8", 0.5, 4.0).model,
        ];
        let discovery = resolve_probes(
            &rows,
            vec![
                (
                    0,
                    launch_for(AdapterKind::Codex),
                    Err("probe timed out".to_string()),
                ),
                (
                    1,
                    launch_for(AdapterKind::Claude),
                    Err("needs auth".to_string()),
                ),
            ],
        );
        assert!(discovery.available.is_empty());
        assert_eq!(discovery.adapter_errors["codex-acp"], "probe timed out");
        assert_eq!(discovery.adapter_errors["claude-acp"], "needs auth");
    }

    #[test]
    fn builtin_unranked_models_surface_by_advertised_value() {
        // claude-acp advertises `haiku`, which has no leaderboard row: it must
        // stay selectable, unranked, under its plain advertised value.
        let rows = vec![role_at("claude-opus-4-8", 0.5, 4.0).model];
        let discovery = resolve_probes(
            &rows,
            vec![(
                0,
                launch_for(AdapterKind::Claude),
                capabilities(&["claude-opus-4-8", "haiku"]),
            )],
        );

        let haiku = discovery
            .available
            .iter()
            .find(|role| role.model.model == "haiku")
            .expect("unranked haiku entry");
        assert!(!haiku.ranked);
        assert_eq!(haiku.model_value, "haiku");
        assert_eq!(haiku.launch.source_id, "claude-acp");

        let resolved =
            explicit("Agent", "haiku", &rows, &discovery.available, &[]).expect("haiku pin");
        assert_eq!(resolved.model_value, "haiku");
    }

    #[test]
    fn missing_reasons_are_based_on_adapter_presence() {
        let availability = Availability {
            codex_credentials: false,
            claude_status: ClaudeAuthStatus::NotLoggedIn,
            subscriptions: Subscriptions::default(),
        };
        assert_eq!(
            availability.missing_reason("gpt-5-6-sol"),
            Some("Codex credentials not found")
        );
        // No built-in adapter serves these providers at all, so there is no
        // per-adapter reason to report.
        assert_eq!(availability.missing_reason("kimi-k2-7-code"), None);
        assert_eq!(availability.missing_reason("glm-5-2"), None);
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
            codex_credentials: false,
            claude_status: ClaudeAuthStatus::NotLoggedIn,
            subscriptions: Subscriptions::default(),
        };
        let error = explicit("Agent", "gpt-5-6-sol", &rows, &[], &[])
            .expect_err("must reject unavailable explicit model");
        assert!(error.to_string().contains("no connected ACP adapter"));
        let _ = availability;
    }

    #[test]
    fn confirmed_missing_explicit_model_switches_the_saved_seat_to_auto() {
        let mut config = Config::default();
        config.agent.model = "gpt-5-6-terra".to_string();
        config.set_acp_server_policy("codex-acp", AcpServerPolicy::Enabled);
        let inventory = discover_inventory(&config);
        let discovery = Discovery {
            available: vec![role("gpt-5-6-sol", 0.7)],
            adapter_errors: HashMap::new(),
            session_config: HashMap::new(),
        };

        let notices = recover_unavailable_explicit_models(&mut config, &inventory, &discovery);

        assert_eq!(config.agent.model, "auto");
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("gpt-5-6-terra"));
    }

    #[test]
    fn platform_adapter_resets_stale_pins_from_other_builds() {
        // On a platform build (e.g. Android/Anvil) the external server is
        // the only probed route. A pin whose name maps to a built-in
        // adapter (here openai -> codex-acp) must still reset to auto when
        // the probe didn't offer it — codex-acp is never in this inventory,
        // so waiting for it to be probed would keep the pin forever and
        // fail every launch.
        let mut config = Config::default();
        config.agent.model = "gpt-5-6-terra".to_string();
        let external = ExternalAdapter {
            id: "sidecar".to_string(),
            label: "Sidecar".to_string(),
            command: PathBuf::from("/opt/sidecar"),
            args: Vec::new(),
            env: HashMap::new(),
            evidence: "bundled".to_string(),
        };
        let inventory = AcpInventory {
            servers: vec![external_server_info(&external, &config)],
        };
        let discovery = Discovery {
            available: vec![role("gpt-5-6-sol", 0.7)],
            adapter_errors: HashMap::new(),
            session_config: HashMap::new(),
        };

        let notices = recover_unavailable_explicit_models_with_external(
            &mut config,
            &inventory,
            &discovery,
            true,
        );
        assert_eq!(config.agent.model, "auto");
        assert_eq!(notices.len(), 1);

        // Control: without a platform adapter the same shape keeps the pin,
        // because codex-acp itself was never probed.
        config.agent.model = "gpt-5-6-terra".to_string();
        let notices = recover_unavailable_explicit_models_with_external(
            &mut config,
            &inventory,
            &discovery,
            false,
        );
        assert_eq!(config.agent.model, "gpt-5-6-terra");
        assert!(notices.is_empty());
    }

    #[test]
    fn failed_adapter_probe_keeps_an_explicit_model_selection() {
        let mut config = Config::default();
        config.agent.model = "gpt-5-6-terra".to_string();
        config.set_acp_server_policy("codex-acp", AcpServerPolicy::Enabled);
        let inventory = discover_inventory(&config);
        let discovery = Discovery {
            available: vec![role("gpt-5-6-sol", 0.7)],
            adapter_errors: HashMap::from([("codex-acp".to_string(), "timed out".to_string())]),
            session_config: HashMap::new(),
        };

        let notices = recover_unavailable_explicit_models(&mut config, &inventory, &discovery);

        assert_eq!(config.agent.model, "gpt-5-6-terra");
        assert!(notices.is_empty());
    }

    #[test]
    fn auto_subagent_default_uses_sonnet_quality_floor_and_selects_terra() {
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
            choose_secondary_auto(&available[0], &rows, &available, &[])
                .expect("subagent default choice")
                .model
                .model,
            "gpt-5-6-terra"
        );
    }

    #[test]
    fn unavailable_explicit_subagent_model_fails_resolution() {
        let rows = vec![role_at("gpt-5-6-sol", 0.694, 3.47).model];
        let available = vec![role_at("claude-fable-5", 0.64, 4.0)];

        let error = resolve_subagent_default("gpt-5-6-sol", &rows, &available, &available[0], &[])
            .expect_err("explicit unavailable subagent model must fail");
        assert!(
            error
                .to_string()
                .contains("Subagent model 'gpt-5-6-sol' is unavailable"),
            "{error:#}"
        );
    }

    #[test]
    fn optional_roles_accept_disabled_and_none() {
        let primary = role("gpt-5-6-sol", 0.70);
        let rows = vec![primary.model.clone()];
        let available = vec![primary.clone()];

        let _ = &primary;
        assert!(
            resolve_subagent_default("none", &rows, &available, &primary, &[])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn assemble_roster_threads_reasoning_effort_onto_the_selected_agents_only() {
        let primary_role = role_at("gpt-5-6-sol", 0.70, 3.0);
        let other_role = role_at("claude-sonnet-5", 0.60, 4.0);
        let rows = vec![primary_role.model.clone(), other_role.model.clone()];
        let available = vec![primary_role.clone(), other_role.clone()];
        let discovery = Discovery {
            available,
            adapter_errors: HashMap::new(),
            session_config: HashMap::new(),
        };
        let availability = Availability {
            codex_credentials: false,
            claude_status: ClaudeAuthStatus::NotLoggedIn,
            subscriptions: Subscriptions::default(),
        };

        let mut config = Config::default();
        config.agent.model = "gpt-5-6-sol".to_string();
        config.agent.reasoning_effort = Some("high".to_string());
        config.subagents.model = crate::config::DISABLED_MODEL.to_string();

        let resolved = assemble_roster(
            &config,
            &rows,
            &availability,
            AcpInventory::default(),
            discovery,
        )
        .expect("assemble roster");

        assert_eq!(resolved.primary.reasoning_effort.as_deref(), Some("high"));
        assert!(resolved.subagent_default.is_none());
    }

    #[test]
    fn auto_subagent_default_reuses_an_excluded_model_when_needed() {
        let subagent = role_at("gpt-5-6-terra", 0.538, 1.13);
        let rows = vec![
            role_at("claude-sonnet-5", 0.482, 7.43).model,
            subagent.model.clone(),
        ];
        let available = vec![subagent];

        assert_eq!(
            resolve_subagent_default("auto", &rows, &available, &available[0], &[])
                .unwrap()
                .unwrap()
                .model
                .model,
            "gpt-5-6-terra"
        );
    }

    #[test]
    fn auto_subagent_default_prefers_a_model_distinct_from_the_occupied_seats() {
        let rows = vec![
            role_at("claude-sonnet-5", 0.482, 7.43).model,
            role_at("gpt-5-6-sol", 0.694, 3.47).model,
            role_at("gpt-5-6-terra", 0.538, 1.13).model,
            role_at("claude-fable-5", 0.640, 4.0).model,
        ];
        let available = vec![
            role_at("gpt-5-6-sol", 0.694, 3.47),
            role_at("gpt-5-6-terra", 0.538, 1.13),
            role_at("claude-fable-5", 0.640, 4.0),
        ];

        assert_eq!(
            resolve_subagent_default("auto", &rows, &available, &available[0], &[],)
                .unwrap()
                .unwrap()
                .model
                .model,
            "gpt-5-6-terra"
        );
    }
}
