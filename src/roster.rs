//! Model-first resolution of the primary agent and the default subagent
//! model. ACP adapters are an implementation detail selected from local
//! capabilities.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures::{StreamExt, stream};

use crate::config::{AcpServerOrigin, AcpServerPolicy, Config, PermissionPreset};
use crate::deepswe::{self, Row};
use crate::{model_resolve, probe};

const PROBE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterKind {
    Codex,
    Claude,
    Kimi,
    Anvil,
    Custom,
}

impl AdapterKind {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
            Self::Kimi => "Kimi Code",
            Self::Anvil => "Anvil",
            Self::Custom => "Custom",
        }
    }

    pub fn from_source_id(source_id: &str) -> Option<Self> {
        match source_id {
            "codex-acp" => Some(Self::Codex),
            "claude-acp" => Some(Self::Claude),
            "kimi" => Some(Self::Kimi),
            "anvil" => Some(Self::Anvil),
            _ => None,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePermissionConfig {
    pub config_id: String,
    pub value: String,
    pub manual_fallback: Option<String>,
    pub mode: PermissionPreset,
}

pub fn configure_permissions(
    kind: AdapterKind,
    mode: PermissionPreset,
    _env: &mut HashMap<String, String>,
) -> Option<RuntimePermissionConfig> {
    let (config_id, value, manual_fallback) = match (kind, mode) {
        (AdapterKind::Codex, PermissionPreset::Manual) => ("mode", "read-only", None),
        (AdapterKind::Codex, PermissionPreset::Auto) => ("mode", "agent", Some("read-only")),
        (AdapterKind::Codex, PermissionPreset::Yolo) => ("mode", "agent-full-access", None),
        (AdapterKind::Claude, PermissionPreset::Manual) => ("mode", "default", None),
        (AdapterKind::Claude, PermissionPreset::Auto) => ("mode", "auto", Some("default")),
        (AdapterKind::Claude, PermissionPreset::Yolo) => ("mode", "bypassPermissions", None),
        (AdapterKind::Kimi, _) => return None,
        (AdapterKind::Anvil | AdapterKind::Custom, PermissionPreset::Manual) => {
            ("permission_mode", "default", None)
        }
        (AdapterKind::Anvil | AdapterKind::Custom, PermissionPreset::Auto) => {
            ("permission_mode", "auto", Some("default"))
        }
        (AdapterKind::Anvil | AdapterKind::Custom, PermissionPreset::Yolo) => {
            ("permission_mode", "bypassPermissions", None)
        }
    };
    Some(RuntimePermissionConfig {
        config_id: config_id.to_string(),
        value: value.to_string(),
        manual_fallback: manual_fallback.map(str::to_string),
        mode,
    })
}

#[derive(Debug, Clone)]
pub struct ResolvedAgent {
    pub model: Row,
    pub model_value: String,
    pub launch: AdapterLaunch,
    pub ranked: bool,
    /// Per-seat reasoning-effort override applied to this agent's ACP
    /// session (e.g. from `--model MODEL+high`). `None` leaves the
    /// adapter's own default effort untouched.
    pub reasoning_effort: Option<String>,
}

/// Everything one resolution pass bound: the seats in use plus the catalog the
/// UI and the subagent MCP surface offer.
#[derive(Debug, Clone)]
pub struct Roster {
    /// The agent that owns the conversation.
    pub primary: ResolvedAgent,
    /// Default model for `create_subagent` delegations. `None` disables them.
    pub subagent_default: Option<ResolvedAgent>,
    pub available: Vec<ResolvedAgent>,
    pub choices: Vec<ModelChoice>,
    pub warnings: Vec<String>,
    pub inventory: AcpInventory,
}

impl Roster {
    /// The subagent pool's model preference order: its bound default first,
    /// then every other ranked model as quota failover.
    pub fn subagent_failover_roles(&self) -> Vec<ResolvedAgent> {
        let Some(initial) = self.subagent_default.clone() else {
            return Vec::new();
        };
        failover_roles(initial, &self.available, false)
    }
}

fn failover_roles(
    initial: ResolvedAgent,
    available: &[ResolvedAgent],
    prefer_other_provider: bool,
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

#[derive(Debug, Clone, Default)]
pub struct AcpInventory {
    pub servers: Vec<AcpServerInfo>,
}

#[derive(Debug, Clone)]
pub struct AcpServerInfo {
    pub id: String,
    pub label: String,
    pub policy: AcpServerPolicy,
    pub detected: bool,
    pub selected: bool,
    pub evidence: String,
    pub launch: AdapterLaunch,
    pub model_count: usize,
    pub error: Option<String>,
    pub installing: bool,
    pub origin: Option<AcpServerOrigin>,
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
    pub codex_credentials: bool,
    pub claude_status: ClaudeAuthStatus,
    pub kimi_credentials: bool,
    pub kimi: Option<PathBuf>,
    pub anvil: Option<PathBuf>,
}

impl Availability {
    pub fn detect() -> Self {
        Self {
            codex_credentials: codex_credentials_available(),
            claude_status: claude_auth_status(),
            kimi_credentials: kimi_credentials_available(),
            kimi: crate::kimi::detect().path,
            anvil: crate::anvil::detect().path,
        }
    }

    pub fn missing_reason(&self, model: &str) -> Option<&'static str> {
        match adapter_kind(model) {
            AdapterKind::Codex if !self.codex_credentials => Some("Codex credentials not found"),
            AdapterKind::Claude if !self.claude_status.logged_in() => {
                Some(self.claude_status.unavailable_reason())
            }
            AdapterKind::Kimi if !self.kimi_credentials => Some("Kimi credentials not found"),
            AdapterKind::Kimi if self.kimi.is_none() => Some("Kimi Code is not installed"),
            AdapterKind::Anvil if self.anvil.is_none() => Some("managed Anvil is not ready"),
            _ => None,
        }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeAuthStatus {
    LoggedIn,
    NotLoggedIn,
    NotInstalled,
    Unavailable,
}

impl ClaudeAuthStatus {
    fn logged_in(self) -> bool {
        self == Self::LoggedIn
    }

    fn unavailable_reason(self) -> &'static str {
        match self {
            Self::LoggedIn => "Claude Code is logged in",
            Self::NotLoggedIn => "Claude Code is not logged in",
            Self::NotInstalled => "Claude Code is not installed",
            Self::Unavailable => "Claude Code login status is unavailable",
        }
    }
}

fn claude_auth_status() -> ClaudeAuthStatus {
    let output = match std::process::Command::new("claude")
        .args(["auth", "status"])
        .stdin(std::process::Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ClaudeAuthStatus::NotInstalled;
        }
        Err(_) => return ClaudeAuthStatus::Unavailable,
    };
    if claude_auth_status_logged_in(&output.stdout) {
        ClaudeAuthStatus::LoggedIn
    } else {
        ClaudeAuthStatus::NotLoggedIn
    }
}

fn kimi_credentials_available() -> bool {
    crate::auth::detect(crate::auth::AuthVendor::Kimi).available()
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

fn claude_auth_status_logged_in(output: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(output)
        .ok()
        .and_then(|status| status.get("loggedIn").and_then(serde_json::Value::as_bool))
        .unwrap_or(false)
}

fn adapter_kind(model: &str) -> AdapterKind {
    match deepswe::model_provider(model) {
        "openai" => AdapterKind::Codex,
        "anthropic" => AdapterKind::Claude,
        "moonshotai" => AdapterKind::Kimi,
        _ => AdapterKind::Anvil,
    }
}

fn adapter_accepts_model(kind: AdapterKind, model: &str) -> bool {
    match kind {
        AdapterKind::Codex => deepswe::model_provider(model) == "openai",
        AdapterKind::Claude => deepswe::model_provider(model) == "anthropic",
        AdapterKind::Kimi => deepswe::model_provider(model) == "moonshotai",
        AdapterKind::Anvil | AdapterKind::Custom => true,
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
        AdapterKind::Kimi => {
            let detection = crate::kimi::detect();
            AdapterLaunch {
                kind,
                source_id: "kimi".to_string(),
                command: detection.path.unwrap_or_else(|| PathBuf::from("kimi")),
                args: detection.args,
                env: detection.env,
            }
        }
        AdapterKind::Anvil => AdapterLaunch {
            kind,
            source_id: "anvil".to_string(),
            command: PathBuf::from("anvil"),
            args: Vec::new(),
            env: HashMap::new(),
        },
        AdapterKind::Custom => unreachable!("custom launches come from configuration"),
    }
}

pub fn discover_inventory(config: &Config) -> AcpInventory {
    let availability = Availability::detect();
    let anvil = crate::anvil::detect();
    let kimi = crate::kimi::detect();
    let detections = [
        (
            AdapterKind::Codex,
            codex_detection(),
            "Codex credentials not found".to_string(),
        ),
        (
            AdapterKind::Claude,
            availability
                .claude_status
                .logged_in()
                .then(|| "authenticated via `claude auth status`".to_string()),
            availability.claude_status.unavailable_reason().to_string(),
        ),
        (
            AdapterKind::Kimi,
            (availability.kimi_credentials && availability.kimi.is_some())
                .then(|| kimi.evidence.clone()),
            kimi.evidence.clone(),
        ),
        (
            AdapterKind::Anvil,
            availability.anvil.as_ref().map(|_| anvil.evidence.clone()),
            anvil.evidence.clone(),
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
                installing: (kind == AdapterKind::Anvil && anvil.installing)
                    || (kind == AdapterKind::Kimi && kimi.installing),
                origin: None,
            }
        })
        .collect::<Vec<_>>();
    let configured_ids = config
        .acp
        .servers
        .iter()
        .map(|server| server.id.as_str())
        .collect::<HashSet<_>>();
    servers.retain(|server| !configured_ids.contains(server.id.as_str()));
    servers.extend(config.acp.servers.iter().map(|server| {
        let selected = server.policy == AcpServerPolicy::Enabled;
        AcpServerInfo {
            id: server.id.clone(),
            label: server.label.clone(),
            policy: server.policy,
            detected: true,
            selected,
            evidence: match server.origin {
                AcpServerOrigin::Registry => "installed from ACP registry".to_string(),
                AcpServerOrigin::Custom => "custom command".to_string(),
            },
            launch: AdapterLaunch {
                kind: AdapterKind::Custom,
                source_id: server.id.clone(),
                command: server.command.clone(),
                args: server.args.clone(),
                env: server.env.clone(),
            },
            model_count: 0,
            error: None,
            installing: false,
            origin: Some(server.origin),
        }
    }));
    if let Some(server) = servers.iter_mut().find(|server| server.id == "anvil") {
        server.error = anvil.error;
        if let Some(path) = availability.anvil {
            server.launch.command = path;
        } else if let Some(path) = crate::anvil::managed_path() {
            server.launch.command = path;
        }
    }
    if let Some(server) = servers.iter_mut().find(|server| server.id == "kimi") {
        server.error = kimi.error;
        if let Some(path) = availability.kimi {
            server.launch.command = path;
        }
    }
    servers.retain(inventory_server_is_visible);
    AcpInventory { servers }
}

fn inventory_server_is_visible(server: &AcpServerInfo) -> bool {
    server.detected
        || server.installing
        || server.error.is_some()
        || server.origin.is_some()
        || server.id == "anvil"
        || server.policy != AcpServerPolicy::Auto
}

type ProbeResult = std::result::Result<probe::AdapterCapabilities, String>;
type ProbeCell = Arc<tokio::sync::OnceCell<ProbeResult>>;

static PROBE_CACHE: LazyLock<tokio::sync::Mutex<HashMap<String, ProbeCell>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));
static WARNED_ADAPTERS: LazyLock<std::sync::Mutex<HashSet<String>>> =
    LazyLock::new(|| std::sync::Mutex::new(HashSet::new()));

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
        cache.entry(key.clone()).or_default().clone()
    };
    cell.get_or_init(|| async {
        let result = probe::adapter_capabilities(
            launch.command.clone(),
            launch.args.clone(),
            launch.env.clone(),
            cwd.to_path_buf(),
            PROBE_TIMEOUT,
        )
        .await;
        if let Ok(capabilities) = &result {
            crate::probe_cache::store(
                &crate::probe_cache::default_cache_path(),
                &key,
                &launch.command,
                capabilities,
            );
        }
        result
    })
    .await
    .clone()
}

/// Capabilities available without launching the adapter: an already-completed
/// in-process probe, or a fresh disk cache entry (which then seeds the
/// in-process cache so this resolution and later ones agree).
async fn cached_probe_result(launch: &AdapterLaunch) -> Option<ProbeResult> {
    let key = probe_key(launch);
    let cell = {
        let mut cache = PROBE_CACHE.lock().await;
        cache.entry(key.clone()).or_default().clone()
    };
    if let Some(result) = cell.get() {
        return Some(result.clone());
    }
    let cached = crate::probe_cache::load(
        &crate::probe_cache::default_cache_path(),
        &key,
        &launch.command,
        crate::probe_cache::CACHE_TTL,
    )?;
    let result: ProbeResult = Ok(cached);
    let _ = cell.set(result.clone());
    Some(result)
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
                tracing::warn!(adapter = %launch.source_id, "roster adapter probe failed");
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
                "roster adapter excluded because HTTP MCP is unavailable"
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
                resolved.push(ResolvedAgent {
                    model: row.clone(),
                    model_value: option.value.clone(),
                    launch: launch.clone(),
                    ranked: true,
                    reasoning_effort: None,
                });
            }
        }
        if launch.kind == AdapterKind::Custom {
            for option in options
                .iter()
                .filter(|option| !matched_values.contains(&option.value))
            {
                let id = custom_model_id(&launch.source_id, &option.value);
                resolved.push(ResolvedAgent {
                    model: Row {
                        model: id,
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

fn configured_launches(inventory: &AcpInventory) -> Vec<AdapterLaunch> {
    inventory
        .servers
        .iter()
        .filter(|server| server.selected)
        .map(|server| server.launch.clone())
        .collect()
}

fn custom_model_id(source_id: &str, model_value: &str) -> String {
    let name = source_id.strip_prefix("custom:").unwrap_or(source_id);
    format!("custom/{name}/{model_value}")
}

fn credentialed_provider_capabilities(
    launch: &AdapterLaunch,
    rows: &[Row],
) -> Option<probe::AdapterCapabilities> {
    matches!(
        launch.kind,
        AdapterKind::Codex | AdapterKind::Claude | AdapterKind::Kimi
    )
    .then(|| probe::AdapterCapabilities {
        http_mcp: true,
        models: rows
            .iter()
            .filter(|row| adapter_accepts_model(launch.kind, &row.model))
            .map(|row| probe::ModelOption {
                value: row.model.clone(),
                name: row.model.clone(),
                description: None,
            })
            .collect(),
    })
}

async fn discover_available(rows: &[Row], inventory: &AcpInventory, cwd: &Path) -> Discovery {
    let launches = configured_launches(inventory);
    let probes = stream::iter(launches.into_iter().enumerate().map(|(priority, launch)| {
        let cwd = cwd.to_path_buf();
        let credentialed = credentialed_provider_capabilities(&launch, rows);
        async move {
            // Built-in Codex and Claude discovery is intentionally only a
            // credential check. Launching their npx bridges here can download
            // npm packages before the UI has rendered anything.
            let capabilities = match credentialed {
                Some(capabilities) => Ok(capabilities),
                None => match cached_probe_result(&launch).await {
                    Some(result) => result,
                    None => probe_launch(&launch, &cwd).await,
                },
            };
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
) -> Result<&'a ResolvedAgent> {
    if let Some(candidate) = available
        .iter()
        .find(|candidate| candidate.model.model == selector)
    {
        return Ok(candidate);
    }
    if selector.starts_with("custom/") {
        if let Some(candidate) = available.iter().find(|candidate| {
            candidate.launch.kind == AdapterKind::Custom
                && custom_model_id(&candidate.launch.source_id, &candidate.model_value) == selector
        }) {
            return Ok(candidate);
        }
        bail!("{seat} model '{selector}' is unavailable from its configured custom ACP server");
    }
    if !rows.iter().any(|row| row.model == selector) {
        bail!("{seat} model '{selector}' is not an eligible DeepSWE High/default model");
    }
    bail!("{seat} model '{selector}' is unavailable: no HTTP-MCP-capable ACP adapter advertised it")
}

fn choose_subagent_default<'a>(
    rows: &[Row],
    available: &'a [ResolvedAgent],
) -> Option<&'a ResolvedAgent> {
    let anchor = deepswe::sonnet_anchor(rows)?;
    let launchable_rows: Vec<Row> = available
        .iter()
        .filter(|role| role.ranked)
        .map(|role| role.model.clone())
        .collect();
    deepswe::subagent_frontier_choice(&launchable_rows, anchor.pass_at_1).and_then(|row| {
        available
            .iter()
            .find(|candidate| candidate.model.model == row.model)
    })
}

fn resolve_subagent_default(
    selector: &str,
    rows: &[Row],
    available: &[ResolvedAgent],
    excluded_models: &[&str],
) -> Result<Option<ResolvedAgent>> {
    if selector == crate::config::DISABLED_MODEL || selector == "none" {
        Ok(None)
    } else if selector == "auto" {
        let distinct = available
            .iter()
            .filter(|role| !excluded_models.contains(&role.model.model.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        Ok(choose_subagent_default(rows, &distinct)
            .or_else(|| choose_subagent_default(rows, available))
            .cloned())
    } else {
        explicit("Subagent", selector, rows, available).map(|role| Some(role.clone()))
    }
}

fn unavailable_reason(
    row: &Row,
    config: &Config,
    availability: &Availability,
    adapter_errors: &HashMap<String, String>,
) -> String {
    let mut reasons = Vec::new();
    for server in config
        .acp
        .servers
        .iter()
        .filter(|server| server.policy == AcpServerPolicy::Enabled)
    {
        let source = server.id.clone();
        reasons.push(match adapter_errors.get(&source) {
            Some(reason) => format!("{}: {reason}", server.label),
            None => format!("{} did not advertise this model", server.label),
        });
    }
    let native = adapter_kind(&row.model);
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
        AdapterKind::Kimi => {
            (availability.kimi_credentials && availability.kimi.is_some())
                || config.acp.policy("kimi") == AcpServerPolicy::Enabled
        }
        AdapterKind::Anvil => {
            availability.anvil.is_some() || config.acp.policy("anvil") == AcpServerPolicy::Enabled
        }
        AdapterKind::Custom => false,
    };
    let native_enabled = match native {
        AdapterKind::Codex => config.acp.policy("codex-acp") != AcpServerPolicy::Disabled,
        AdapterKind::Claude => config.acp.policy("claude-acp") != AcpServerPolicy::Disabled,
        AdapterKind::Kimi => config.acp.policy("kimi") != AcpServerPolicy::Disabled,
        AdapterKind::Anvil => config.acp.policy("anvil") != AcpServerPolicy::Disabled,
        AdapterKind::Custom => true,
    };
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
    if native != AdapterKind::Anvil && config.acp.policy("anvil") != AcpServerPolicy::Disabled {
        reasons.push(adapter_errors.get("anvil").map_or_else(
            || "anvil did not advertise this model".to_string(),
            |reason| format!("anvil: {reason}"),
        ));
    }
    reasons.sort();
    reasons.dedup();
    reasons.join("; ")
}

pub async fn resolve(config: &Config, cwd: &Path) -> Result<Roster> {
    resolve_inner(config, cwd).await
}

pub async fn resolve_waiting_for_installs(config: &Config, cwd: &Path) -> Result<Roster> {
    if config.acp.policy("anvil") != AcpServerPolicy::Disabled
        && crate::anvil::detect().path.is_none()
    {
        crate::anvil::wait_until_ready()
            .await
            .context("install managed Anvil")?;
    }
    if config.acp.policy("kimi") != AcpServerPolicy::Disabled
        && kimi_credentials_available()
        && crate::kimi::detect().path.is_none()
    {
        crate::kimi::wait_until_ready()
            .await
            .context("install managed Kimi Code")?;
    }
    resolve_inner(config, cwd).await
}

/// A roster bound from instantly-known adapters, plus a stream of refreshed
/// rosters as the remaining adapters finish probing in the background.
pub struct StreamingResolution {
    pub roster: Roster,
    /// New roster snapshots as background probes land. `None` when every
    /// adapter resolved instantly. Snapshots never rebind the running
    /// session's seats; they refresh choices, inventory, and warnings.
    pub updates: Option<tokio::sync::watch::Receiver<Roster>>,
    /// Adapters still probing when the initial roster was returned.
    pub pending_servers: Vec<String>,
}

/// Resolve the roster without waiting on adapter launches when possible.
///
/// Adapters whose capabilities are known instantly (credentialed built-ins,
/// completed in-process probes, fresh disk cache entries) bind immediately;
/// the rest are probed in the background and delivered as update snapshots.
/// Only when the initial set cannot bind the configured roster does this
/// wait, and then only until the earliest set of probe results that can.
pub async fn resolve_streaming(config: &Config, cwd: &Path) -> Result<StreamingResolution> {
    let leaderboard = deepswe::load(
        &deepswe::default_cache_path(),
        deepswe::CACHE_TTL,
        deepswe::DEFAULT_URL,
    )
    .await;
    let rows = deepswe::eligible_high(&leaderboard.rows);
    let availability = Availability::detect();
    let inventory = discover_inventory(config);
    let anvil_installing = inventory
        .servers
        .iter()
        .any(|server| server.id == "anvil" && server.selected && server.installing);

    let mut results: Vec<(usize, AdapterLaunch, ProbeResult)> = Vec::new();
    let mut pending: Vec<(usize, AdapterLaunch)> = Vec::new();
    for (priority, launch) in configured_launches(&inventory).into_iter().enumerate() {
        let instant = match credentialed_provider_capabilities(&launch, &rows) {
            Some(capabilities) => Some(Ok(capabilities)),
            None => cached_probe_result(&launch).await,
        };
        match instant {
            Some(result) => results.push((priority, launch, result)),
            None => pending.push((priority, launch)),
        }
    }

    let assemble = |results: Vec<(usize, AdapterLaunch, ProbeResult)>,
                    config: &Config,
                    rows: &[Row],
                    availability: &Availability,
                    inventory: &AcpInventory| {
        let discovery = resolve_probes(rows, results);
        assemble_roster(config, rows, availability, inventory.clone(), discovery)
    };

    if pending.is_empty() {
        let roster = assemble(results, config, &rows, &availability, &inventory)?;
        return Ok(StreamingResolution {
            roster,
            updates: None,
            pending_servers: Vec::new(),
        });
    }

    let pending_servers = pending
        .iter()
        .map(|(_, launch)| launch.source_id.clone())
        .collect::<Vec<_>>();
    let (probe_tx, mut probe_rx) = tokio::sync::mpsc::unbounded_channel();
    {
        let cwd = cwd.to_path_buf();
        let jobs = pending
            .into_iter()
            .map(|(priority, launch)| {
                let cwd = cwd.clone();
                async move {
                    // A managed Anvil install already running in the
                    // background becomes probe-able once it lands.
                    if launch.kind == AdapterKind::Anvil && anvil_installing {
                        let _ = crate::anvil::wait_until_ready().await;
                    }
                    let result = probe_launch(&launch, &cwd).await;
                    (priority, launch, result)
                }
            })
            .collect::<Vec<_>>();
        tokio::spawn(async move {
            let mut probes = stream::iter(jobs).buffer_unordered(probe::PROBE_CONCURRENCY);
            while let Some(item) = probes.next().await {
                if probe_tx.send(item).is_err() {
                    break;
                }
            }
        });
    }

    // Wait only while the instantly-known adapters cannot bind the roster.
    let mut roster = assemble(results.clone(), config, &rows, &availability, &inventory);
    while roster.is_err() {
        let Some(item) = probe_rx.recv().await else {
            return roster.map(|roster| StreamingResolution {
                roster,
                updates: None,
                pending_servers: Vec::new(),
            });
        };
        results.push(item);
        roster = assemble(results.clone(), config, &rows, &availability, &inventory);
    }
    let roster = roster.expect("roster bound");

    let (snapshot_tx, snapshot_rx) = tokio::sync::watch::channel(roster.clone());
    {
        let config = config.clone();
        let inventory = inventory.clone();
        let availability = availability.clone();
        let rows = rows.clone();
        let mut results = results;
        tokio::spawn(async move {
            while let Some(item) = probe_rx.recv().await {
                results.push(item);
                let discovery = resolve_probes(&rows, results.clone());
                if let Ok(snapshot) =
                    assemble_roster(&config, &rows, &availability, inventory.clone(), discovery)
                    && snapshot_tx.send(snapshot).is_err()
                {
                    break;
                }
            }
        });
    }
    Ok(StreamingResolution {
        roster,
        updates: Some(snapshot_rx),
        pending_servers,
    })
}

async fn resolve_inner(config: &Config, cwd: &Path) -> Result<Roster> {
    let leaderboard = deepswe::load(
        &deepswe::default_cache_path(),
        deepswe::CACHE_TTL,
        deepswe::DEFAULT_URL,
    )
    .await;
    let rows = deepswe::eligible_high(&leaderboard.rows);
    let availability = Availability::detect();
    let inventory = discover_inventory(config);
    let discovery = discover_available(&rows, &inventory, cwd).await;
    assemble_roster(config, &rows, &availability, inventory, discovery)
}

/// Bind the primary agent and the default subagent model plus the model
/// catalog from one set of probe results. Pure with respect to probing:
/// callable repeatedly as additional adapters finish probing in the background.
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
        bail!(
            "no model is launchable{diagnostic}: install or authenticate an ACP adapter, or configure a custom ACP server"
        );
    }

    if matches!(
        config.agent.model.as_str(),
        crate::config::DISABLED_MODEL | "none"
    ) {
        bail!("the primary agent cannot be disabled");
    }
    let primary = if config.agent.model == "auto" {
        available
            .iter()
            .find(|candidate| candidate.ranked)
            .ok_or_else(|| anyhow!("Agent Auto requires at least one ranked DeepSWE model"))?
    } else {
        explicit("Agent", &config.agent.model, rows, &available)?
    };
    let occupied = vec![primary.model.model.as_str()];
    let mut subagent_default =
        resolve_subagent_default(&config.subagents.model, rows, &available, &occupied)?;

    // Attach each seat's per-invocation reasoning-effort override (from
    // `--model`/`--subagent-model MODEL+effort`, threaded via `Config`). This
    // only touches the exact agent selected for the seat; failover
    // alternates discovered elsewhere in `available` are unaffected.
    let mut primary = primary.clone();
    primary.reasoning_effort = config.agent.reasoning_effort.clone();
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
             Install and authenticate a supported ACP adapter (for Codex: install `@openai/codex` \
             and run `codex login`), then restart or retry with /models."
                .to_string(),
        );
    }
    warnings.sort();
    Ok(Roster {
        primary,
        subagent_default,
        available,
        choices,
        warnings,
        inventory,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_auth_status_requires_explicit_logged_in_true() {
        assert!(claude_auth_status_logged_in(
            br#"{"loggedIn":true,"authMethod":"claude.ai"}"#
        ));
        assert!(!claude_auth_status_logged_in(
            br#"{"loggedIn":false,"authMethod":"none"}"#
        ));
        assert!(!claude_auth_status_logged_in(
            br#"{"authMethod":"claude.ai"}"#
        ));
        assert!(!claude_auth_status_logged_in(b"not json"));
        assert_eq!(
            ClaudeAuthStatus::NotInstalled.unavailable_reason(),
            "Claude Code is not installed"
        );
        assert_eq!(
            ClaudeAuthStatus::NotLoggedIn.unavailable_reason(),
            "Claude Code is not logged in"
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

        let anvil = configure_permissions(AdapterKind::Anvil, PermissionPreset::Yolo, &mut env)
            .expect("Anvil preset");
        assert_eq!(anvil.config_id, "permission_mode");
        assert_eq!(anvil.value, "bypassPermissions");
    }

    #[test]
    fn custom_explicit_yolo_permission_uses_anvil_config_option() {
        let mut env = HashMap::new();
        let custom = configure_permissions(AdapterKind::Custom, PermissionPreset::Yolo, &mut env)
            .expect("Custom preset");

        assert_eq!(custom.config_id, "permission_mode");
        assert_eq!(custom.value, "bypassPermissions");
        assert_eq!(custom.manual_fallback, None);
        assert_eq!(custom.mode, PermissionPreset::Yolo);
    }

    #[test]
    fn custom_without_explicit_permission_mode_sends_no_config() {
        let mut env = HashMap::new();
        let omitted: Option<PermissionPreset> = None;
        let custom =
            omitted.and_then(|mode| configure_permissions(AdapterKind::Custom, mode, &mut env));

        assert_eq!(custom, None);
    }

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
            launch: launch_for(adapter_kind(model)),
            ranked: true,
            reasoning_effort: None,
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
    fn credentialed_codex_and_claude_use_catalog_without_startup_probe() {
        let rows = vec![
            role_at("gpt-5-6-sol", 0.7, 1.0).model,
            role_at("claude-sonnet-5", 0.6, 1.0).model,
            role_at("gemini-3-5-flash", 0.5, 1.0).model,
        ];

        let codex = credentialed_provider_capabilities(&launch_for(AdapterKind::Codex), &rows)
            .expect("Codex credential discovery");
        assert_eq!(codex.models.len(), 1);
        assert_eq!(codex.models[0].value, "gpt-5-6-sol");

        let claude = credentialed_provider_capabilities(&launch_for(AdapterKind::Claude), &rows)
            .expect("Claude credential discovery");
        assert_eq!(claude.models.len(), 1);
        assert_eq!(claude.models[0].value, "claude-sonnet-5");

        assert!(
            credentialed_provider_capabilities(&launch_for(AdapterKind::Anvil), &rows).is_none()
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
    fn anvil_launch_uses_inventory_resolved_binary() {
        let launch = launch_for(AdapterKind::Anvil);
        assert_eq!(launch.command, PathBuf::from("anvil"));
        assert!(launch.args.is_empty());
    }

    #[test]
    fn adapter_display_names_match_the_primary_acp_products() {
        assert_eq!(AdapterKind::Codex.display_name(), "Codex");
        assert_eq!(AdapterKind::Claude.display_name(), "Claude Code");
        assert_eq!(AdapterKind::Kimi.display_name(), "Kimi Code");
        assert_eq!(AdapterKind::Anvil.display_name(), "Anvil");
    }

    #[test]
    fn opencode_normalizes_provider_and_openrouter_model_values() {
        let row = role_at("claude-opus-4-8", 0.5, 4.0).model;
        let launch = AdapterLaunch {
            kind: AdapterKind::Custom,
            source_id: "opencode".to_string(),
            command: PathBuf::from("opencode"),
            args: vec!["acp".to_string()],
            env: HashMap::new(),
        };
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
    fn configured_launches_exclude_disabled_adapters() {
        let mut config = Config::default();
        config.set_acp_server_policy("codex-acp", AcpServerPolicy::Disabled);
        config.set_acp_server_policy("opencode-acp", AcpServerPolicy::Disabled);
        config.acp.servers.push(crate::config::ConfiguredAcpServer {
            id: "custom:disabled".to_string(),
            label: "disabled".to_string(),
            command: PathBuf::from("disabled-acp"),
            args: Vec::new(),
            env: HashMap::new(),
            origin: AcpServerOrigin::Custom,
            policy: AcpServerPolicy::Disabled,
        });
        config.acp.servers.push(crate::config::ConfiguredAcpServer {
            id: "custom:enabled".to_string(),
            label: "enabled".to_string(),
            command: PathBuf::from("enabled-acp"),
            args: Vec::new(),
            env: HashMap::new(),
            origin: AcpServerOrigin::Custom,
            policy: AcpServerPolicy::Enabled,
        });
        let mut inventory = discover_inventory(&config);
        for server in &mut inventory.servers {
            if matches!(server.id.as_str(), "claude-acp" | "anvil") {
                server.detected = true;
                server.selected = true;
            }
        }
        let ids = configured_launches(&inventory)
            .into_iter()
            .map(|launch| launch.source_id)
            .collect::<Vec<_>>();
        assert!(ids.contains(&"custom:enabled".to_string()));
        assert!(!ids.contains(&"custom:disabled".to_string()));
        assert!(!ids.contains(&"codex-acp".to_string()));
    }

    #[test]
    fn explicit_policy_selects_a_builtin_without_detection() {
        let mut config = Config::default();
        config.set_acp_server_policy("anvil", AcpServerPolicy::Enabled);
        let inventory = discover_inventory(&config);
        let anvil = inventory
            .servers
            .iter()
            .find(|server| server.id == "anvil")
            .expect("anvil");
        assert!(anvil.selected);
        assert_eq!(anvil.policy, AcpServerPolicy::Enabled);
    }

    #[test]
    fn explicit_policy_keeps_undetected_builtin_visible() {
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
            installing: false,
            origin: None,
        };

        assert!(inventory_server_is_visible(&server));
        server.policy = AcpServerPolicy::Disabled;
        assert!(inventory_server_is_visible(&server));
        server.policy = AcpServerPolicy::Auto;
        assert!(!inventory_server_is_visible(&server));
    }

    #[test]
    fn auto_misses_are_inventory_state_not_probe_errors() {
        let mut config = Config::default();
        for id in ["codex-acp", "claude-acp", "kimi", "anvil"] {
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
    fn route_precedence_is_custom_then_native_then_anvil() {
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
    fn explicit_custom_selector_resolves_ranked_model_by_exact_advertised_value() {
        let rows = vec![Row {
            model: "kimi-k2-7-code".to_string(),
            reasoning_effort: None,
            pass_at_1: 0.3,
            mean_cost_usd: 2.8,
        }];
        let discovery = resolve_probes(
            &rows,
            vec![(
                0,
                custom_launch("bpr-agent"),
                capabilities(true, &["openrouter::moonshotai/kimi-k2.7-code"]),
            )],
        );

        let resolved = explicit(
            "Agent",
            "custom/bpr-agent/openrouter::moonshotai/kimi-k2.7-code",
            &rows,
            &discovery.available,
        )
        .expect("exact custom selector");

        assert_eq!(resolved.model.model, "kimi-k2-7-code");
        assert_eq!(
            resolved.model_value,
            "openrouter::moonshotai/kimi-k2.7-code"
        );
        assert_eq!(resolved.launch.source_id, "custom:bpr-agent");
    }

    #[test]
    fn missing_reasons_are_based_on_adapter_presence() {
        let availability = Availability {
            codex_credentials: false,
            claude_status: ClaudeAuthStatus::NotLoggedIn,
            kimi_credentials: false,
            kimi: None,
            anvil: None,
        };
        assert_eq!(
            availability.missing_reason("gpt-5-6-sol"),
            Some("Codex credentials not found")
        );
        assert_eq!(
            availability.missing_reason("glm-5-2"),
            Some("managed Anvil is not ready")
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
            codex_credentials: false,
            claude_status: ClaudeAuthStatus::NotLoggedIn,
            kimi_credentials: false,
            kimi: None,
            anvil: None,
        };
        let error = explicit("Agent", "gpt-5-6-sol", &rows, &[])
            .expect_err("must reject unavailable explicit model");
        assert!(
            error
                .to_string()
                .contains("no HTTP-MCP-capable ACP adapter")
        );
        let _ = availability;
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
            choose_subagent_default(&rows, &available)
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

        let error = resolve_subagent_default("gpt-5-6-sol", &rows, &available, &[])
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
            resolve_subagent_default("none", &rows, &available, &[])
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
        };
        let availability = Availability {
            codex_credentials: false,
            claude_status: ClaudeAuthStatus::NotLoggedIn,
            kimi_credentials: false,
            kimi: None,
            anvil: None,
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
            resolve_subagent_default("auto", &rows, &available, &["gpt-5-6-terra"])
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
            resolve_subagent_default("auto", &rows, &available, &["gpt-5-6-sol", "gpt-5-6-terra"])
                .unwrap()
                .unwrap()
                .model
                .model,
            "claude-fable-5"
        );
    }
}
