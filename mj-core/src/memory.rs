//! Shared project knowledge for Claude and Codex primary sessions.
//!
//! Durable facts are stored globally or per project and synchronized into
//! Claude Code and Codex's native memory files. The `memory_save` and
//! `memory_forget` MCP tools manage the common store; memory text is never
//! prepended to an ACP user prompt.

use std::fmt::Write as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_client_protocol::schema::v1::McpServer;
use anyhow::{Context, Result, anyhow};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::{
        CallToolRequestParams, CallToolResult, Content, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
    },
    service::RequestContext,
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MCP_SERVER_NAME: &str = "mj-memory";
const STORE_VERSION: u32 = 1;

/// Longest accepted memory text in bytes. Memories are single short facts;
/// the cap also guarantees one entry can never dominate the prompt budget.
pub const MAX_TEXT_BYTES: usize = 2_000;

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryEntry {
    pub id: u64,
    pub text: String,
    /// Project root this memory is scoped to; `None` means global.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<PathBuf>,
    pub created_at_ms: u64,
    /// Stable importer-owned identity. Interactive and MCP entries have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoreData {
    version: u32,
    #[serde(default = "default_next_id")]
    next_id: u64,
    #[serde(default)]
    entries: Vec<MemoryEntry>,
}

impl Default for StoreData {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            next_id: 1,
            entries: Vec::new(),
        }
    }
}

fn default_next_id() -> u64 {
    1
}

/// Default store path: `$XDG_CONFIG_HOME/mj/memories.json` (or
/// `~/.config/mj/memories.json` when `XDG_CONFIG_HOME` is unset).
pub fn default_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("mj")
        .join("memories.json")
}

/// The project a working directory's memories attach to: the enclosing
/// project when `cwd` is inside a `.mjolnir` worktree, otherwise `cwd`
/// itself. Worktree sessions therefore share the parent project's memories.
pub fn project_key(cwd: &Path) -> PathBuf {
    crate::paths::parent_above_mjolnir(cwd).unwrap_or_else(|| cwd.to_path_buf())
}

pub fn add(path: &Path, text: &str, project: Option<PathBuf>) -> Result<MemoryEntry> {
    let text = text.trim();
    if text.is_empty() {
        return Err(anyhow!("memory text must not be empty"));
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(anyhow!(
            "memory text is {} bytes; keep each memory one short fact under {MAX_TEXT_BYTES} bytes",
            text.len()
        ));
    }
    let _guard = lock_store(path)?;
    // A malformed store is a hard error: silently replacing it would destroy
    // every memory the user has saved.
    let mut store = load(path)?;
    let entry = MemoryEntry {
        id: store.next_id,
        text: text.to_string(),
        project,
        created_at_ms: now_ms(),
        source: None,
    };
    store.next_id = store.next_id.saturating_add(1);
    store.entries.push(entry.clone());
    save(path, &store)?;
    Ok(entry)
}

pub fn forget(path: &Path, id: u64) -> Result<Option<MemoryEntry>> {
    let _guard = lock_store(path)?;
    let mut store = load(path)?;
    let Some(position) = store.entries.iter().position(|entry| entry.id == id) else {
        return Ok(None);
    };
    // Imported entries are a projection of a file this process does not own.
    // Deleting the row here would only be undone by the next import, so point
    // at the authority instead of recording a delete marker.
    if store.entries[position].source.is_some() {
        return Err(anyhow!(
            "memory m{id} is imported from Claude auto-memory; edit MEMORY.md to remove it"
        ));
    }
    let removed = store.entries.remove(position);
    save(path, &store)?;
    Ok(Some(removed))
}

/// Delete every memory and reset the store. Returns how many entries were
/// removed. A malformed store is preserved and returned as an error.
pub fn clear(path: &Path) -> Result<usize> {
    let _guard = lock_store(path)?;
    let removed = load(path)?.entries.len();
    save(path, &StoreData::default())?;
    Ok(removed)
}

pub fn entries(path: &Path) -> Result<Vec<MemoryEntry>> {
    Ok(load(path)?.entries)
}

/// Global entries plus entries scoped to `project`, in insertion order.
pub fn entries_for_project(path: &Path, project: &Path) -> Result<Vec<MemoryEntry>> {
    Ok(load(path)?
        .entries
        .into_iter()
        .filter(|entry| {
            entry
                .project
                .as_deref()
                .is_none_or(|scoped| scoped == project)
        })
        .collect())
}

/// Advisory file lock shared by every mjolnir process using this store. The
/// lock lives beside the store rather than on it: atomically replacing the
/// JSON file would otherwise replace the inode carrying the lock.
fn lock_store(path: &Path) -> Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create memory store directory {}", parent.display()))?;
    }
    let lock_path = store_lock_path(path);
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("open memory store lock {}", lock_path.display()))?;
    lock.lock()
        .with_context(|| format!("lock memory store {}", lock_path.display()))?;
    Ok(lock)
}

fn store_lock_path(path: &Path) -> PathBuf {
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    PathBuf::from(lock_path)
}

fn load(path: &Path) -> Result<StoreData> {
    if !path.exists() {
        return Ok(StoreData::default());
    }
    let body = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&body).with_context(|| format!("parse {}", path.display()))
}

fn save(path: &Path, store: &StoreData) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    // A unique staging file per writer: the TUI, the MCP server, and `mj
    // memory` CLI processes may save concurrently and must not truncate one
    // another's staging file mid-write.
    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary memories file in {}", parent.display()))?;
    std::io::Write::write_all(&mut tmp, &serde_json::to_vec_pretty(store)?)?;
    tmp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Per-session behavior

const CLAUDE_AUTO_SOURCE: &str = "claude-auto";
const CLAUDE_AUTO_READ_LIMIT: usize = 25_000;
const CLAUDE_AUTO_CHUNK_LIMIT: usize = 1_800;
const MANAGED_BLOCK_PREFIX: &str = "<!-- mjolnir:shared-memory:start";
const MANAGED_BLOCK_END: &str = "<!-- mjolnir:shared-memory:end -->";

#[derive(Clone, Copy)]
enum ClaudeImportVisibility {
    Project,
    Global,
    Disabled,
    Unresolved,
}

/// Import Claude Code's project auto-memory into the shared store. Imported
/// chunks have stable source identities, so repeated turn-boundary refreshes
/// are cheap and edits replace earlier content instead of duplicating it.
fn remove_project_claude_imports(store_path: &Path, project: &Path) -> Result<()> {
    mutate_claude_imports(store_path, |entry| {
        entry.project.as_deref() == Some(project)
            && entry
                .source
                .as_deref()
                .is_some_and(|source| source.starts_with("claude-auto:"))
    })
}

fn remove_global_claude_imports(store_path: &Path) -> Result<()> {
    mutate_claude_imports(store_path, |entry| {
        entry.project.is_none()
            && entry
                .source
                .as_deref()
                .is_some_and(|source| source.starts_with("claude-auto-global:"))
    })
}

fn mutate_claude_imports(
    store_path: &Path,
    mut remove: impl FnMut(&MemoryEntry) -> bool,
) -> Result<()> {
    let _guard = lock_store(store_path)?;
    let mut store = load(store_path)?;
    let before = store.entries.len();
    store.entries.retain(|entry| !remove(entry));
    if store.entries.len() != before {
        save(store_path, &store)?;
    }
    Ok(())
}

fn sync_claude_auto_memory_from(
    store_path: &Path,
    project: &Path,
    memory_path: Option<&Path>,
    global: bool,
) -> Result<()> {
    // `None` means path resolution failed, not that Claude's file was deleted.
    // Only a positively resolved, absent path may remove imported chunks.
    let Some(memory_path) = memory_path else {
        return Ok(());
    };
    let content = Some(memory_path)
        .filter(|path| path.is_file())
        .map(read_claude_auto_memory)
        .transpose()?
        .unwrap_or_default();
    let chunks = chunk_text(&content, CLAUDE_AUTO_CHUNK_LIMIT);
    let entry_project = (!global).then(|| project.to_path_buf());
    let source_prefix = if global {
        "claude-auto-global"
    } else {
        CLAUDE_AUTO_SOURCE
    };

    let _guard = lock_store(store_path)?;
    let mut store = load(store_path)?;
    let mut changed = false;
    for (index, text) in chunks.iter().enumerate() {
        let source = format!("{source_prefix}:{index}");
        match store.entries.iter_mut().find(|entry| {
            entry.project == entry_project && entry.source.as_deref() == Some(source.as_str())
        }) {
            Some(entry) if entry.text != *text => {
                entry.text.clone_from(text);
                entry.created_at_ms = now_ms();
                changed = true;
            }
            Some(_) => {}
            None => {
                store.entries.push(MemoryEntry {
                    id: store.next_id,
                    text: text.clone(),
                    project: entry_project.clone(),
                    created_at_ms: now_ms(),
                    source: Some(source),
                });
                store.next_id = store.next_id.saturating_add(1);
                changed = true;
            }
        }
    }
    let chunk_count = chunks.len();
    store.entries.retain(|entry| {
        let stale_scope = global
            && entry.project.as_deref() == Some(project)
            && entry
                .source
                .as_deref()
                .is_some_and(|source| source.starts_with("claude-auto:"));
        let stale = entry.project == entry_project
            && entry.source.as_deref().is_some_and(|source| {
                source
                    .strip_prefix(&format!("{source_prefix}:"))
                    .and_then(|index| index.parse::<usize>().ok())
                    .is_some_and(|index| index >= chunk_count)
            });
        changed |= stale || stale_scope;
        !(stale || stale_scope)
    });
    if changed {
        save(store_path, &store)?;
    }
    Ok(())
}

fn read_claude_auto_memory(entrypoint: &Path) -> Result<String> {
    let content = std::fs::read_to_string(entrypoint)
        .with_context(|| format!("read Claude auto-memory {}", entrypoint.display()))?;
    let content = if content.len() <= CLAUDE_AUTO_READ_LIMIT {
        content
    } else {
        let mut end = CLAUDE_AUTO_READ_LIMIT;
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        content[..end].to_string()
    };
    Ok(strip_managed_blocks(&content))
}

struct ClaudeMemoryLocation {
    path: PathBuf,
    global: bool,
}

enum ClaudeMemoryResolution {
    Location(ClaudeMemoryLocation),
    Disabled,
    Unresolved,
}

fn claude_auto_memory_path(project: &Path) -> ClaudeMemoryResolution {
    if std::env::var_os("CLAUDE_CODE_DISABLE_AUTO_MEMORY")
        .is_some_and(|value| claude_env_true(&value.to_string_lossy()))
    {
        return ClaudeMemoryResolution::Disabled;
    }
    let Some(home) = dirs::home_dir() else {
        return ClaudeMemoryResolution::Unresolved;
    };
    let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"));
    let managed = read_managed_claude_settings();
    let local = read_claude_settings(&project.join(".claude/settings.local.json"));
    let project_settings = read_claude_settings(&project.join(".claude/settings.json"));
    let user = read_claude_settings(&config_dir.join("settings.json"));
    for settings in [
        managed.as_ref(),
        local.as_ref(),
        project_settings.as_ref(),
        user.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(enabled) = settings
            .get("autoMemoryEnabled")
            .and_then(|value| value.as_bool())
        {
            if !enabled {
                return ClaudeMemoryResolution::Disabled;
            }
            break;
        }
    }
    for settings in [managed.as_ref(), user.as_ref()].into_iter().flatten() {
        if let Some(configured) = settings
            .get("autoMemoryDirectory")
            .and_then(|value| value.as_str())
        {
            let configured = configured
                .strip_prefix("~/")
                .map(|suffix| home.join(suffix))
                .unwrap_or_else(|| PathBuf::from(configured));
            if !configured.is_absolute() {
                tracing::warn!(
                    "ignoring relative Claude autoMemoryDirectory {}",
                    configured.display()
                );
                return ClaudeMemoryResolution::Unresolved;
            }
            return ClaudeMemoryResolution::Location(ClaudeMemoryLocation {
                path: configured.join("MEMORY.md"),
                global: true,
            });
        }
    }
    ClaudeMemoryResolution::Location(ClaudeMemoryLocation {
        path: config_dir
            .join("projects")
            .join(claude_project_directory_name(project))
            .join("memory/MEMORY.md"),
        global: false,
    })
}

fn claude_env_true(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn managed_claude_settings_dir() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        return Some(PathBuf::from("/etc/claude-code"));
    }
    #[cfg(target_os = "macos")]
    {
        return Some(PathBuf::from("/Library/Application Support/ClaudeCode"));
    }
    #[cfg(target_os = "windows")]
    {
        return Some(PathBuf::from(r"C:\Program Files\ClaudeCode"));
    }
    #[allow(unreachable_code)]
    None
}

fn read_managed_claude_settings() -> Option<serde_json::Value> {
    let directory = managed_claude_settings_dir()?;
    let mut merged = read_claude_settings(&directory.join("managed-settings.json"))
        .unwrap_or_else(|| serde_json::json!({}));
    let dropins = directory.join("managed-settings.d");
    let mut paths = std::fs::read_dir(dropins)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        if let Some(settings) = read_claude_settings(&path) {
            merge_json(&mut merged, settings);
        }
    }
    (!merged.as_object().is_some_and(|object| object.is_empty())).then_some(merged)
}

fn merge_json(base: &mut serde_json::Value, overlay: serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(base), serde_json::Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge_json(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn read_claude_settings(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn claude_project_directory_name(project: &Path) -> String {
    let original = project.to_string_lossy();
    let encoded: String = original
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect();
    if encoded.len() <= 200 {
        return encoded;
    }
    // Claude uses JavaScript's 32-bit string hash over UTF-16 code units.
    let mut hash = 0_i32;
    for unit in original.encode_utf16() {
        hash = hash.wrapping_mul(31).wrapping_add(i32::from(unit));
    }
    format!("{}-{}", &encoded[..200], base36(hash.unsigned_abs()))
}

fn base36(mut value: u32) -> String {
    if value == 0 {
        return "0".into();
    }
    let mut digits = Vec::new();
    while value > 0 {
        digits.push(char::from_digit(value % 36, 36).expect("base-36 digit"));
        value /= 36;
    }
    digits.iter().rev().collect()
}

/// Synchronize the shared store into the native Claude Code and Codex memory
/// locations for `project`. This is deliberately file-based: neither provider
/// receives a synthetic user prompt from Mjolnir.
pub fn synchronize_native_memories(store_path: &Path, project: &Path) -> Result<()> {
    let (claude, visibility) = match claude_auto_memory_path(project) {
        ClaudeMemoryResolution::Location(location) => {
            // `autoMemoryDirectory` is configured above the project level. If
            // it is no longer selected, its global import must not remain
            // visible to Codex through the shared store.
            if !location.global {
                remove_global_claude_imports(store_path)?;
            }
            let visibility = if location.global {
                ClaudeImportVisibility::Global
            } else {
                ClaudeImportVisibility::Project
            };
            (Some(location), visibility)
        }
        ClaudeMemoryResolution::Disabled => {
            // A project-level setting must not erase imports still used by
            // another project. Its native Codex block omits all such imports.
            remove_project_claude_imports(store_path, project)?;
            (None, ClaudeImportVisibility::Disabled)
        }
        ClaudeMemoryResolution::Unresolved => (None, ClaudeImportVisibility::Unresolved),
    };
    synchronize_native_memories_at(
        store_path,
        project,
        claude
            .as_ref()
            .map(|location| (location.path.as_path(), location.global)),
        codex_memories_root().as_deref(),
        visibility,
    )
}

/// Remove Mjolnir-owned native memory blocks when native memory use is
/// disabled. Provider-owned content and the shared store are left untouched.
pub fn remove_managed_native_memories(project: &Path) -> Result<()> {
    let claude = match claude_auto_memory_path(project) {
        ClaudeMemoryResolution::Location(location) => Some(location),
        ClaudeMemoryResolution::Disabled | ClaudeMemoryResolution::Unresolved => None,
    };
    remove_managed_native_memories_at(
        project,
        claude
            .as_ref()
            .map(|location| (location.path.as_path(), location.global)),
        codex_memories_root().as_deref(),
    )
}

fn synchronize_native_memories_at(
    store_path: &Path,
    project: &Path,
    claude_memory: Option<(&Path, bool)>,
    codex_root: Option<&Path>,
    visibility: ClaudeImportVisibility,
) -> Result<()> {
    if let Some((path, global)) = claude_memory {
        sync_claude_auto_memory_from(store_path, project, Some(path), global)?;
    }
    let visible = native_visible_entries(store_path, project, visibility)?;

    if let Some((path, _global)) = claude_memory {
        let native_entries: Vec<_> = visible
            .iter()
            .filter(|entry| {
                !entry
                    .source
                    .as_deref()
                    .is_some_and(|source| source.starts_with(CLAUDE_AUTO_SOURCE))
            })
            .cloned()
            .collect();
        replace_managed_project_block(path, project, &native_entries)?;
    }

    if let Some(root) = codex_root {
        replace_managed_project_block(&root.join("MEMORY.md"), project, &visible)?;
        // Codex loads memory_summary.md as native developer policy. Keep the
        // same project-owned block there, rather than relying on the registry
        // file being read during a turn.
        replace_managed_project_block(&root.join("memory_summary.md"), project, &visible)?;
    }
    Ok(())
}

fn native_visible_entries(
    store_path: &Path,
    project: &Path,
    visibility: ClaudeImportVisibility,
) -> Result<Vec<MemoryEntry>> {
    let mut entries = entries_for_project(store_path, project)?;
    entries.retain(|entry| {
        let source = entry.source.as_deref();
        match visibility {
            ClaudeImportVisibility::Global => {
                !source.is_some_and(|source| source.starts_with("claude-auto:"))
            }
            ClaudeImportVisibility::Project => {
                !source.is_some_and(|source| source.starts_with("claude-auto-global:"))
            }
            ClaudeImportVisibility::Disabled => {
                !source.is_some_and(|source| source.starts_with(CLAUDE_AUTO_SOURCE))
            }
            ClaudeImportVisibility::Unresolved => true,
        }
    });
    Ok(entries)
}

fn remove_managed_native_memories_at(
    project: &Path,
    claude_memory: Option<(&Path, bool)>,
    codex_root: Option<&Path>,
) -> Result<()> {
    if let Some((path, global)) = claude_memory {
        if global {
            remove_all_managed_blocks(path)?;
        } else {
            replace_managed_project_block(path, project, &[])?;
        }
    }
    if let Some(root) = codex_root {
        // Codex's memory files are global to its installation, so disabling
        // native memory use must remove blocks from every Mjolnir project.
        remove_all_managed_blocks(&root.join("MEMORY.md"))?;
        remove_all_managed_blocks(&root.join("memory_summary.md"))?;
    }
    Ok(())
}

fn codex_memories_root() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .map(|home| home.join("memories"))
}

fn replace_managed_project_block(
    path: &Path,
    project: &Path,
    entries: &[MemoryEntry],
) -> Result<()> {
    let start = format!(
        "{MANAGED_BLOCK_PREFIX} project={} -->",
        project_marker(project)
    );
    if entries.is_empty() {
        return remove_managed_block(path, &start);
    }
    let replacement = render_project_block(&start, project, entries);
    replace_managed_block(path, &start, Some(&replacement))
}

fn render_project_block(start: &str, project: &Path, entries: &[MemoryEntry]) -> String {
    let mut block = String::from(start);
    let _ = write!(
        block,
        "\n## Mjolnir shared project knowledge ({})\n",
        project.display()
    );
    for entry in entries {
        push_memory_entry(&mut block, entry);
    }
    block.push('\n');
    block.push_str(MANAGED_BLOCK_END);
    block
}

fn push_memory_entry(out: &mut String, entry: &MemoryEntry) {
    let text = entry
        .text
        .replace(MANAGED_BLOCK_PREFIX, "mjolnir shared-memory marker")
        .replace(MANAGED_BLOCK_END, "mjolnir shared-memory end marker");
    let mut lines = text.lines();
    if let Some(first) = lines.next() {
        let _ = write!(out, "\n- {first}");
    }
    for line in lines {
        let _ = write!(out, "\n  {line}");
    }
}

fn project_marker(project: &Path) -> String {
    project
        .as_os_str()
        .to_string_lossy()
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn replace_managed_block(path: &Path, start: &str, replacement: Option<&str>) -> Result<()> {
    let _guard = lock_native_memory(path)?;
    let existing = if path.exists() {
        std::fs::read_to_string(path)
            .with_context(|| format!("read native memory {}", path.display()))?
    } else {
        String::new()
    };
    let updated = match existing.find(start) {
        Some(begin) => {
            let after_start = begin + start.len();
            let Some(end_offset) = existing[after_start..].find(MANAGED_BLOCK_END) else {
                return Err(anyhow!(
                    "managed memory block in {} has no closing marker",
                    path.display()
                ));
            };
            let end = after_start + end_offset + MANAGED_BLOCK_END.len();
            let mut updated = String::with_capacity(existing.len());
            updated.push_str(&existing[..begin]);
            if let Some(replacement) = replacement {
                updated.push_str(replacement);
            }
            updated.push_str(&existing[end..]);
            updated
        }
        None => match replacement {
            Some(replacement) if existing.is_empty() => replacement.to_string(),
            Some(replacement) => format!("{existing}\n\n{replacement}\n"),
            None => return Ok(()),
        },
    };
    write_native_memory(path, &updated)
}

fn remove_managed_block(path: &Path, start: &str) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    replace_managed_block(path, start, None)
}

fn remove_all_managed_blocks(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let _guard = lock_native_memory(path)?;
    let existing = if path.exists() {
        std::fs::read_to_string(path)
            .with_context(|| format!("read native memory {}", path.display()))?
    } else {
        return Ok(());
    };
    let mut remaining = existing.as_str();
    let mut updated = String::with_capacity(existing.len());
    let mut removed = false;
    while let Some(begin) = remaining.find(MANAGED_BLOCK_PREFIX) {
        updated.push_str(&remaining[..begin]);
        let after_start = &remaining[begin..];
        let Some(end) = after_start.find(MANAGED_BLOCK_END) else {
            return Err(anyhow!(
                "managed memory block in {} has no closing marker",
                path.display()
            ));
        };
        remaining = &after_start[end + MANAGED_BLOCK_END.len()..];
        removed = true;
    }
    if !removed {
        return Ok(());
    }
    updated.push_str(remaining);
    write_native_memory(path, &updated)
}

fn lock_native_memory(path: &Path) -> Result<std::fs::File> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("native memory path {} has no parent", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create native memory directory {}", parent.display()))?;
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".mjolnir.lock");
    let lock_path = PathBuf::from(lock_path);
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("open native memory lock {}", lock_path.display()))?;
    lock.lock()
        .with_context(|| format!("lock native memory {}", lock_path.display()))?;
    Ok(lock)
}

fn write_native_memory(path: &Path, content: &str) -> Result<()> {
    if path.is_file() && std::fs::read_to_string(path).ok().as_deref() == Some(content) {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("native memory path {} has no parent", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create native memory directory {}", parent.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create native memory file in {}", parent.display()))?;
    std::io::Write::write_all(&mut temporary, content.as_bytes())?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("replace native memory {}", path.display()))?;
    Ok(())
}

fn strip_managed_blocks(content: &str) -> String {
    let mut remaining = content;
    let mut stripped = String::with_capacity(content.len());
    while let Some(begin) = remaining.find(MANAGED_BLOCK_PREFIX) {
        stripped.push_str(&remaining[..begin]);
        let after_start = &remaining[begin..];
        let Some(end) = after_start.find(MANAGED_BLOCK_END) else {
            // Preserve malformed user content; a later sync reports the block
            // clearly instead of silently discarding it.
            stripped.push_str(after_start);
            return stripped;
        };
        remaining = &after_start[end + MANAGED_BLOCK_END.len()..];
    }
    stripped.push_str(remaining);
    stripped
}

fn chunk_text(text: &str, max_bytes: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut heading = String::new();
    let mut in_fence = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let is_fence = trimmed.starts_with("```") || trimmed.starts_with("~~~");
        let is_heading = !in_fence
            && trimmed.starts_with('#')
            && trimmed.chars().find(|character| *character != '#') == Some(' ');
        if is_heading {
            heading = line.to_string();
        }
        if !current.is_empty() && current.len() + line.len() > max_bytes {
            chunks.push(std::mem::take(&mut current));
            if !is_heading && heading.len() + line.len() <= max_bytes {
                current.push_str(&heading);
            }
        }
        for character in line.chars() {
            if current.len() + character.len_utf8() > max_bytes {
                if !current.is_empty() {
                    chunks.push(std::mem::take(&mut current));
                }
                if !is_heading && heading.len() + character.len_utf8() <= max_bytes {
                    current.push_str(&heading);
                }
            }
            current.push(character);
        }
        if is_fence {
            in_fence = !in_fence;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

// ---------------------------------------------------------------------------
/// Memory behavior for one ACP runtime, carried on `AcpRuntimeConfig`.
/// `None` there disables the feature entirely for that runtime.
#[derive(Debug, Clone)]
pub struct SessionMemory {
    pub store_path: PathBuf,
    /// Config path to re-check before a fresh provider session starts. This
    /// lets `/memory off` take effect on an already-running ACP runtime.
    pub config_path: Option<PathBuf>,
    /// Project scope used for recall and for project-scoped saves.
    pub project: PathBuf,
    /// Synchronize stored knowledge into the provider's native memory files.
    pub inject: bool,
    /// Remove Mjolnir-managed native blocks before the next provider session.
    /// This is set when native memory use is disabled, so old blocks cannot
    /// remain active in a provider after the toggle changes.
    pub cleanup: bool,
    /// Expose the `memory_save` / `memory_forget` MCP tools.
    pub tools: bool,
}

impl SessionMemory {
    /// Shared project knowledge applies to built-in Claude and Codex primary
    /// sessions. Custom adapters remain opt-in until their behavior is known.
    pub fn from_config(
        config: &crate::config::MemoryConfig,
        cwd: &Path,
        adapter: Option<crate::roster::AdapterKind>,
    ) -> Option<Self> {
        if !matches!(
            adapter,
            Some(crate::roster::AdapterKind::Codex | crate::roster::AdapterKind::Claude)
        ) {
            return None;
        }
        let inject = config.enabled && config.use_memories;
        Some(Self {
            store_path: default_path(),
            config_path: Some(crate::config::default_config_path()),
            project: project_key(cwd),
            inject,
            cleanup: !inject,
            tools: config.enabled && config.generate_memories,
        })
    }

    /// Native-memory synchronization for worker lanes (subagents, review
    /// lanes, ragnarok combatants, fresh side sessions). The save tools stay
    /// with primary sessions so workers cannot write to the common store.
    pub fn inject_only(
        config: &crate::config::MemoryConfig,
        cwd: &Path,
        adapter: Option<crate::roster::AdapterKind>,
    ) -> Option<Self> {
        if !matches!(
            adapter,
            Some(crate::roster::AdapterKind::Codex | crate::roster::AdapterKind::Claude)
        ) {
            return None;
        }
        let inject = config.enabled && config.use_memories;
        Some(Self {
            store_path: default_path(),
            config_path: Some(crate::config::default_config_path()),
            project: project_key(cwd),
            inject,
            cleanup: !inject,
            tools: false,
        })
    }

    /// Synchronize provider-native memory before a session is created. Errors
    /// are non-fatal: a provider's private memory store must never block an
    /// agent session.
    pub(crate) fn synchronize_native(&self) {
        if native_memory_enabled(self.config_path.as_deref(), self.inject) {
            if let Err(error) = synchronize_native_memories(&self.store_path, &self.project) {
                tracing::warn!("could not synchronize native memories: {error:#}");
            }
        } else if (self.cleanup || self.config_path.is_some())
            && let Err(error) = remove_managed_native_memories(&self.project)
        {
            tracing::warn!("could not remove managed native memories: {error:#}");
        }
    }
}

fn native_memory_enabled(config_path: Option<&Path>, fallback: bool) -> bool {
    config_path
        .and_then(|path| crate::config::Config::load(path).ok())
        .map(|config| config.memory.enabled && config.memory.use_memories)
        .unwrap_or(fallback)
}

/// Native-memory synchronization for a worker lane identified by its adapter
/// source id, reading the user's `[memory]` toggles from the default config
/// path.
pub fn worker_lane_memory(source_id: &str, cwd: &Path) -> Option<SessionMemory> {
    let memory_config = crate::config::Config::load(&crate::config::default_config_path())
        .map(|config| config.memory)
        .unwrap_or_default();
    worker_lane_memory_with(&memory_config, source_id, cwd)
}

fn worker_lane_memory_with(
    config: &crate::config::MemoryConfig,
    source_id: &str,
    cwd: &Path,
) -> Option<SessionMemory> {
    SessionMemory::inject_only(
        config,
        cwd,
        crate::roster::AdapterKind::from_source_id(source_id),
    )
}

// ---------------------------------------------------------------------------
// Listing (shared by the TUI /memory panel and `mj memory` CLI)
// ---------------------------------------------------------------------------

const DISABLED_STATUS_LINE: &str = "memory: DISABLED — run /memory on in the TUI or set \
[memory] enabled = true in config.toml\n";

pub fn render_list(path: &Path, project: &Path, config: &crate::config::MemoryConfig) -> String {
    let use_memories = config.use_memories;
    let generate_memories = config.generate_memories;
    let entries = match entries(path) {
        Ok(entries) => entries,
        Err(error) => return format!("could not read memories: {error:#}"),
    };
    let mut out = format!("Memories — {}\n", path.display());
    if !config.enabled {
        out.push_str(DISABLED_STATUS_LINE);
    }
    out.push_str(&format!(
        "use: {} (synchronize Claude and Codex native memory) · generate: {} (agents share discoveries)\n",
        on_off(use_memories),
        on_off(generate_memories),
    ));
    if entries.is_empty() {
        out.push_str(
            "\nNo memories saved yet. Ask the agent to remember something, or run \
             /memory add <text>.",
        );
        return out;
    }
    let now = now_ms();
    let global: Vec<&MemoryEntry> = entries.iter().filter(|e| e.project.is_none()).collect();
    let scoped: Vec<&MemoryEntry> = entries
        .iter()
        .filter(|e| e.project.as_deref() == Some(project))
        .collect();
    let other = entries.len() - global.len() - scoped.len();
    if !global.is_empty() {
        out.push_str("\nGlobal:\n");
        for entry in &global {
            out.push_str(&entry_line(entry, now));
        }
    }
    if !scoped.is_empty() {
        out.push_str(&format!("\nThis project ({}):\n", project.display()));
        for entry in &scoped {
            out.push_str(&entry_line(entry, now));
        }
    }
    if other > 0 {
        out.push_str(&format!(
            "\nOther projects: {} (run `mj memory list` to see all)\n",
            count_label(other)
        ));
    }
    out
}

/// Full listing for the CLI: every scope, grouped by project.
pub fn render_full_list(path: &Path, config: &crate::config::MemoryConfig) -> String {
    let entries = match entries(path) {
        Ok(entries) => entries,
        Err(error) => return format!("could not read memories: {error:#}"),
    };
    let mut out = format!("Memories — {}\n", path.display());
    if !config.enabled {
        out.push_str(DISABLED_STATUS_LINE);
    }
    out.push_str(&format!(
        "use: {} (synchronize Claude and Codex native memory) · generate: {} (agents share discoveries)\n",
        on_off(config.use_memories),
        on_off(config.generate_memories),
    ));
    if entries.is_empty() {
        out.push_str("\nNo memories saved yet.");
        return out;
    }
    let now = now_ms();
    let global: Vec<&MemoryEntry> = entries.iter().filter(|e| e.project.is_none()).collect();
    if !global.is_empty() {
        out.push_str("\nGlobal:\n");
        for entry in &global {
            out.push_str(&entry_line(entry, now));
        }
    }
    let mut projects: Vec<&Path> = Vec::new();
    for entry in &entries {
        if let Some(project) = entry.project.as_deref()
            && !projects.contains(&project)
        {
            projects.push(project);
        }
    }
    for project in projects {
        out.push_str(&format!("\n{}:\n", project.display()));
        for entry in entries
            .iter()
            .filter(|e| e.project.as_deref() == Some(project))
        {
            out.push_str(&entry_line(entry, now));
        }
    }
    out
}

fn entry_line(entry: &MemoryEntry, now_ms: u64) -> String {
    format!(
        "  [m{}] {} ({})\n",
        entry.id,
        entry.text,
        age_label(entry.created_at_ms, now_ms)
    )
}

fn age_label(created_at_ms: u64, now_ms: u64) -> String {
    let days = now_ms.saturating_sub(created_at_ms) / 86_400_000;
    match days {
        0 => "today".to_string(),
        1 => "1d ago".to_string(),
        days => format!("{days}d ago"),
    }
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

pub fn count_label(count: usize) -> String {
    if count == 1 {
        "1 memory".to_string()
    } else {
        format!("{count} memories")
    }
}

// ---------------------------------------------------------------------------
// MCP tools
// ---------------------------------------------------------------------------

const SERVER_GUIDANCE: &str = "SHARED PROJECT KNOWLEDGE POLICY: Claude and Codex sessions use this \
store to exchange durable discoveries. Automatically call memory_save after verifying a non-obvious \
project fact that another session would otherwise need to rediscover, including architecture \
constraints, build requirements, debugging conclusions, and repository conventions. Keep each entry \
short and self-contained. Never save speculation, secrets, credentials, transient task state, or \
facts trivially visible in source. Call memory_forget when an entry you saved is wrong or obsolete; \
use ids from /memory or mj memory list. Entries imported from Claude auto-memory cannot be forgotten \
here; they are owned by MEMORY.md. Mjolnir synchronizes this store into Claude and Codex native \
memory files; it never adds memory text to a user prompt. Do not announce this policy or every \
automatic save; confirm only user-requested saves and deletions.";

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MemorySaveArgs {
    /// One short, self-contained fact to remember across sessions.
    pub text: String,
    /// Save for every project, not just the current one. Use for facts about
    /// the user rather than this repository.
    #[serde(default)]
    pub global: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MemoryForgetArgs {
    /// Numeric id of the memory to forget (the N in [mN]).
    pub id: u64,
}

#[derive(Clone)]
struct McpHandler {
    store_path: PathBuf,
    project: PathBuf,
    sync_native: bool,
    native_config_path: Option<PathBuf>,
    tool_router: ToolRouter<Self>,
}

#[tool_router(router = tool_router)]
impl McpHandler {
    fn new(memory: &SessionMemory) -> Self {
        Self {
            store_path: memory.store_path.clone(),
            project: memory.project.clone(),
            sync_native: memory.inject,
            native_config_path: memory.config_path.clone(),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        name = "memory_save",
        description = "Share one short durable, verified discovery with all Claude and Codex sessions for this project. Save non-obvious architecture constraints, build requirements, debugging conclusions, repository conventions, and durable preferences automatically. Never save speculation, secrets, credentials, transient task state, or facts trivially visible in source. Set `global` only for user-wide preferences. Returns the saved memory id."
    )]
    async fn memory_save(
        &self,
        Parameters(args): Parameters<MemorySaveArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let project = (!args.global).then(|| self.project.clone());
        let scope = if args.global {
            "global".to_string()
        } else {
            format!("this project: {}", self.project.display())
        };
        match add(&self.store_path, &args.text, project) {
            Ok(entry) => {
                self.synchronize_native();
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Saved memory m{} ({scope}). It is synchronized to Claude and Codex native memory.",
                    entry.id
                ))]))
            }
            Err(error) => Ok(CallToolResult::error(vec![Content::text(format!(
                "could not save memory: {error:#}"
            ))])),
        }
    }

    #[tool(
        name = "memory_forget",
        description = "Delete one stored Mjolnir memory by numeric id from /memory or mj memory list. Use when the user asks you to forget something or a stored memory is wrong or obsolete."
    )]
    async fn memory_forget(
        &self,
        Parameters(args): Parameters<MemoryForgetArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        match forget(&self.store_path, args.id) {
            Ok(Some(entry)) => {
                self.synchronize_native();
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Forgot memory m{}: {}",
                    entry.id, entry.text
                ))]))
            }
            Ok(None) => Ok(CallToolResult::error(vec![Content::text(format!(
                "no memory with id m{}",
                args.id
            ))])),
            Err(error) => Ok(CallToolResult::error(vec![Content::text(format!(
                "could not forget memory: {error:#}"
            ))])),
        }
    }

    fn synchronize_native(&self) {
        if native_memory_enabled(self.native_config_path.as_deref(), self.sync_native) {
            if let Err(error) = synchronize_native_memories(&self.store_path, &self.project) {
                tracing::warn!(
                    "could not synchronize native memories after tool update: {error:#}"
                );
            }
        } else if self.native_config_path.is_some()
            && let Err(error) = remove_managed_native_memories(&self.project)
        {
            tracing::warn!("could not remove managed native memories after tool update: {error:#}");
        }
    }
}

impl ServerHandler for McpHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                MCP_SERVER_NAME,
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(SERVER_GUIDANCE)
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = std::result::Result<ListToolsResult, McpError>> + Send + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(
            self.tool_router.list_all(),
        )))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = std::result::Result<CallToolResult, McpError>> + Send + '_ {
        self.tool_router
            .call(ToolCallContext::new(self, request, context))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tool_router.get(name).cloned()
    }
}

/// In-process MCP endpoint advertised to the primary ACP agent as a stdio
/// server via the MCP bridge. Dropping it closes every open MCP session.
pub struct ToolServer {
    bridge: crate::mcp_bridge::BridgeServer,
}

impl ToolServer {
    pub async fn start(memory: &SessionMemory) -> Result<Self> {
        let handler = McpHandler::new(memory);
        let bridge = crate::mcp_bridge::BridgeServer::start(MCP_SERVER_NAME, handler)
            .await
            .context("start memory MCP bridge")?;
        Ok(Self { bridge })
    }

    pub fn advertised(&self) -> &McpServer {
        self.bridge.advertised()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("memories.json")
    }

    #[test]
    fn add_assigns_sequential_ids_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        let first = add(&path, "prefers rebase merges", None).unwrap();
        let second = add(&path, "uses pnpm", Some(PathBuf::from("/tmp/proj"))).unwrap();
        assert_eq!(first.id, 1);
        assert_eq!(second.id, 2);
        let all = entries(&path).unwrap();
        assert_eq!(all, vec![first, second]);
    }

    #[test]
    fn add_rejects_empty_or_whitespace_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        assert!(add(&path, "   ", None).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn add_refuses_to_clobber_a_malformed_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        std::fs::write(&path, "not json").unwrap();
        let error = add(&path, "fact", None).unwrap_err();
        assert!(format!("{error:#}").contains("parse"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
    }

    #[test]
    fn forget_removes_only_the_requested_id_and_keeps_ids_stable() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        add(&path, "one", None).unwrap();
        add(&path, "two", None).unwrap();
        let removed = forget(&path, 1).unwrap().expect("entry removed");
        assert_eq!(removed.text, "one");
        assert!(forget(&path, 99).unwrap().is_none());
        let ids: Vec<u64> = entries(&path).unwrap().iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![2]);
        // New entries never reuse a forgotten id.
        assert_eq!(add(&path, "three", None).unwrap().id, 3);
    }

    #[test]
    fn clear_reports_removed_count_and_resets_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        add(&path, "one", None).unwrap();
        add(&path, "two", None).unwrap();
        assert_eq!(clear(&path).unwrap(), 2);
        assert!(entries(&path).unwrap().is_empty());
        assert_eq!(clear(&path).unwrap(), 0);
    }

    #[test]
    fn clear_preserves_a_malformed_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        std::fs::write(&path, "not json").unwrap();

        let error = clear(&path).unwrap_err();

        assert!(format!("{error:#}").contains("parse"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
    }

    #[test]
    fn store_lock_is_held_on_a_stable_sibling_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        let _first = lock_store(&path).unwrap();
        let lock_path = store_lock_path(&path);
        let second = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();

        assert!(second.try_lock().is_err());
    }

    #[test]
    fn entries_for_project_includes_global_and_matching_scope_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        add(&path, "global fact", None).unwrap();
        add(&path, "proj fact", Some(PathBuf::from("/tmp/proj"))).unwrap();
        add(&path, "other fact", Some(PathBuf::from("/tmp/other"))).unwrap();
        let visible = entries_for_project(&path, Path::new("/tmp/proj")).unwrap();
        let texts: Vec<&str> = visible.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["global fact", "proj fact"]);
    }

    #[test]
    fn project_key_resolves_worktrees_to_the_enclosing_project() {
        assert_eq!(
            project_key(Path::new("/home/me/proj/.mjolnir/worktrees/bold-fox")),
            PathBuf::from("/home/me/proj")
        );
        assert_eq!(
            project_key(Path::new("/home/me/proj")),
            PathBuf::from("/home/me/proj")
        );
    }

    #[test]
    fn claude_project_directory_name_matches_claude_code_layout() {
        assert_eq!(
            claude_project_directory_name(Path::new("/home/parallels/code/mjolnir")),
            "-home-parallels-code-mjolnir"
        );
    }

    #[test]
    fn claude_auto_memory_syncs_updates_and_deletions_into_shared_project_knowledge() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = store(&dir);
        let project = dir.path().join("project");
        let claude_memory = dir.path().join("MEMORY.md");
        std::fs::write(
            &claude_memory,
            "# Project memory\n- Tests require Redis\n- Parser paths are normalized",
        )
        .unwrap();

        std::fs::write(dir.path().join("debugging.md"), "Use RUST_LOG=mj=debug").unwrap();
        sync_claude_auto_memory_from(&store_path, &project, Some(&claude_memory), false).unwrap();
        let imported = entries_for_project(&store_path, &project).unwrap();
        assert_eq!(imported.len(), 1);
        assert!(imported[0].text.contains("Tests require Redis"));
        assert!(!imported[0].text.contains("RUST_LOG=mj=debug"));
        assert_eq!(imported[0].source.as_deref(), Some("claude-auto:0"));
        let imported_id = imported[0].id;

        std::fs::write(&claude_memory, "- Tests use an embedded Redis fixture").unwrap();
        sync_claude_auto_memory_from(&store_path, &project, Some(&claude_memory), false).unwrap();
        let updated = entries_for_project(&store_path, &project).unwrap();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].id, imported_id);
        assert!(updated[0].text.contains("embedded Redis"));

        std::fs::remove_file(&claude_memory).unwrap();
        sync_claude_auto_memory_from(&store_path, &project, Some(&claude_memory), false).unwrap();
        assert!(
            entries_for_project(&store_path, &project)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn native_sync_preserves_provider_content_and_uses_managed_sections() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = store(&dir);
        let project = dir.path().join("project");
        let claude_memory = dir.path().join("claude/MEMORY.md");
        let codex_root = dir.path().join("codex/memories");
        std::fs::create_dir_all(claude_memory.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&codex_root).unwrap();
        std::fs::write(
            &claude_memory,
            "# Claude's own memory\n- native Claude fact\n",
        )
        .unwrap();
        std::fs::write(codex_root.join("MEMORY.md"), "# Codex's own memory\n").unwrap();
        std::fs::write(codex_root.join("memory_summary.md"), "# Existing summary\n").unwrap();
        add(&store_path, "global shared fact", None).unwrap();
        add(&store_path, "this-project fact", Some(project.clone())).unwrap();

        synchronize_native_memories_at(
            &store_path,
            &project,
            Some((&claude_memory, false)),
            Some(&codex_root),
            ClaudeImportVisibility::Project,
        )
        .unwrap();

        let claude = std::fs::read_to_string(&claude_memory).unwrap();
        assert!(claude.contains("native Claude fact"));
        assert!(claude.contains("global shared fact"));
        assert!(claude.contains("this-project fact"));
        let codex = std::fs::read_to_string(codex_root.join("MEMORY.md")).unwrap();
        assert!(codex.contains("Codex's own memory"));
        assert!(codex.contains("native Claude fact"));
        assert!(codex.contains("global shared fact"));
        assert!(codex.contains("this-project fact"));
        let summary = std::fs::read_to_string(codex_root.join("memory_summary.md")).unwrap();
        assert!(summary.contains("Existing summary"));
        assert!(summary.contains("global shared fact"));
        assert!(summary.contains("this-project fact"));
        assert!(!summary.contains("<mj-memory>"));

        let other_project = dir.path().join("other-project");
        add(
            &store_path,
            "other-project fact",
            Some(other_project.clone()),
        )
        .unwrap();
        synchronize_native_memories_at(
            &store_path,
            &other_project,
            None,
            Some(&codex_root),
            ClaudeImportVisibility::Unresolved,
        )
        .unwrap();
        let summary = std::fs::read_to_string(codex_root.join("memory_summary.md")).unwrap();
        assert!(summary.contains("this-project fact"));
        assert!(summary.contains("other-project fact"));

        forget(&store_path, 2).unwrap();
        synchronize_native_memories_at(
            &store_path,
            &project,
            Some((&claude_memory, false)),
            Some(&codex_root),
            ClaudeImportVisibility::Project,
        )
        .unwrap();
        let codex = std::fs::read_to_string(codex_root.join("MEMORY.md")).unwrap();
        assert!(!codex.contains("this-project fact"));
        assert!(codex.contains("native Claude fact"));
        assert!(codex.contains("other-project fact"));
        let summary = std::fs::read_to_string(codex_root.join("memory_summary.md")).unwrap();
        assert!(!summary.contains("this-project fact"));
        assert!(summary.contains("native Claude fact"));
        assert!(summary.contains("other-project fact"));

        remove_managed_native_memories_at(
            &project,
            Some((&claude_memory, false)),
            Some(&codex_root),
        )
        .unwrap();
        let claude = std::fs::read_to_string(&claude_memory).unwrap();
        assert!(claude.contains("native Claude fact"));
        assert!(!claude.contains("global shared fact"));
        let codex = std::fs::read_to_string(codex_root.join("MEMORY.md")).unwrap();
        assert!(codex.contains("Codex's own memory"));
        assert!(!codex.contains("other-project fact"));
        let summary = std::fs::read_to_string(codex_root.join("memory_summary.md")).unwrap();
        assert!(summary.contains("Existing summary"));
        assert!(!summary.contains("other-project fact"));
    }

    #[test]
    fn native_sync_keeps_project_knowledge_in_global_claude_memory() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = store(&dir);
        let project = dir.path().join("project");
        let claude_memory = dir.path().join("claude/MEMORY.md");
        std::fs::create_dir_all(claude_memory.parent().unwrap()).unwrap();
        add(&store_path, "global fact", None).unwrap();
        add(&store_path, "project fact", Some(project.clone())).unwrap();

        synchronize_native_memories_at(
            &store_path,
            &project,
            Some((&claude_memory, true)),
            None,
            ClaudeImportVisibility::Global,
        )
        .unwrap();

        let claude = std::fs::read_to_string(&claude_memory).unwrap();
        assert!(claude.contains("global fact"));
        assert!(claude.contains("project fact"));
    }

    #[test]
    fn native_sync_serializes_concurrent_project_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = store(&dir);
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        let codex_root = dir.path().join("codex/memories");
        add(&store_path, "first project fact", Some(first.clone())).unwrap();
        add(&store_path, "second project fact", Some(second.clone())).unwrap();

        std::thread::scope(|scope| {
            scope.spawn(|| {
                synchronize_native_memories_at(
                    &store_path,
                    &first,
                    None,
                    Some(&codex_root),
                    ClaudeImportVisibility::Unresolved,
                )
                .unwrap();
            });
            scope.spawn(|| {
                synchronize_native_memories_at(
                    &store_path,
                    &second,
                    None,
                    Some(&codex_root),
                    ClaudeImportVisibility::Unresolved,
                )
                .unwrap();
            });
        });

        for path in [
            codex_root.join("MEMORY.md"),
            codex_root.join("memory_summary.md"),
        ] {
            let content = std::fs::read_to_string(path).unwrap();
            assert!(content.contains("first project fact"));
            assert!(content.contains("second project fact"));
        }
    }

    #[test]
    fn native_memory_lock_is_held_on_a_stable_sibling_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("codex/memories/MEMORY.md");
        let _first = lock_native_memory(&path).unwrap();
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".mjolnir.lock");
        let second = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(PathBuf::from(lock_path))
            .unwrap();

        assert!(second.try_lock().is_err());
    }

    #[test]
    fn claude_import_ignores_mjolnir_managed_memory() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = store(&dir);
        let project = dir.path().join("project");
        let memory = dir.path().join("MEMORY.md");
        let managed = render_project_block(
            &format!("{MANAGED_BLOCK_PREFIX} project=test -->"),
            &project,
            &[MemoryEntry {
                id: 1,
                text: "shared fact".to_string(),
                project: Some(project.clone()),
                created_at_ms: 1,
                source: None,
            }],
        );
        std::fs::write(&memory, format!("native fact\n\n{managed}\n")).unwrap();

        sync_claude_auto_memory_from(&store_path, &project, Some(&memory), false).unwrap();

        let imported = entries_for_project(&store_path, &project).unwrap();
        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].text.trim(), "native fact");
    }

    #[test]
    fn claude_encoding_replaces_underscores_and_truncates_long_paths() {
        assert_eq!(
            claude_project_directory_name(Path::new("/tmp/my_project")),
            "-tmp-my-project"
        );
        let encoded = claude_project_directory_name(Path::new(&format!("/{}", "a".repeat(240))));
        assert!(encoded.starts_with(&format!("-{}", "a".repeat(199))));
        assert!(encoded.len() > 201);
    }

    #[test]
    fn unresolved_claude_lookup_preserves_imported_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = store(&dir);
        let project = dir.path().join("project");
        let memory = dir.path().join("MEMORY.md");
        std::fs::write(&memory, "durable fact").unwrap();
        sync_claude_auto_memory_from(&store_path, &project, Some(&memory), false).unwrap();
        sync_claude_auto_memory_from(&store_path, &project, None, false).unwrap();
        assert_eq!(entries_for_project(&store_path, &project).unwrap().len(), 1);
    }

    #[test]
    fn chunking_preserves_markdown_structure() {
        let text = "## Things that do NOT work\n\n  - nested item\n\n```rust\nlet x = 1;\n```\n";
        assert_eq!(chunk_text(text, 1_800), vec![text]);
        let split = chunk_text(
            "## Rejected\n- first item that is long\n- second item\n",
            38,
        );
        assert!(split.len() > 1);
        assert!(split[1].starts_with("## Rejected\n"));
        let fenced = chunk_text(
            "## Real\n```sh\n# not a heading\nxxxxxxxxxxxxxxxxxxxxxxxx\n```\n",
            34,
        );
        assert!(
            fenced
                .iter()
                .skip(1)
                .all(|chunk| !chunk.starts_with("# not a heading"))
        );
    }

    #[test]
    fn global_claude_import_is_not_duplicated_per_project() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = store(&dir);
        let memory = dir.path().join("MEMORY.md");
        std::fs::write(&memory, "user-wide fact").unwrap();
        sync_claude_auto_memory_from(&store_path, Path::new("/one"), Some(&memory), true).unwrap();
        sync_claude_auto_memory_from(&store_path, Path::new("/two"), Some(&memory), true).unwrap();
        let all = entries(&store_path).unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].project.is_none());
    }

    #[test]
    fn reconfiguring_claude_auto_memory_removes_global_imports() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = store(&dir);
        let memory = dir.path().join("MEMORY.md");
        std::fs::write(&memory, "old global Claude fact").unwrap();
        sync_claude_auto_memory_from(&store_path, Path::new("/one"), Some(&memory), true).unwrap();
        add(&store_path, "locally saved global fact", None).unwrap();

        remove_global_claude_imports(&store_path).unwrap();
        let remaining = entries(&store_path).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].text, "locally saved global fact");
    }

    #[test]
    fn disabling_one_project_claude_auto_memory_preserves_other_project_imports() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = store(&dir);
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        let first_memory = dir.path().join("first/MEMORY.md");
        let second_memory = dir.path().join("second/MEMORY.md");
        std::fs::create_dir_all(first_memory.parent().unwrap()).unwrap();
        std::fs::create_dir_all(second_memory.parent().unwrap()).unwrap();
        std::fs::write(&first_memory, "first Claude fact").unwrap();
        std::fs::write(&second_memory, "second Claude fact").unwrap();
        sync_claude_auto_memory_from(&store_path, &first, Some(&first_memory), false).unwrap();
        sync_claude_auto_memory_from(&store_path, &second, Some(&second_memory), false).unwrap();

        remove_project_claude_imports(&store_path, &first).unwrap();

        assert!(entries_for_project(&store_path, &first).unwrap().is_empty());
        let second_entries = entries_for_project(&store_path, &second).unwrap();
        assert_eq!(second_entries.len(), 1);
        assert!(second_entries[0].text.contains("second Claude fact"));
    }

    #[test]
    fn native_codex_memory_hides_claude_imports_outside_selected_scope() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = store(&dir);
        let project = dir.path().join("project");
        let global_memory = dir.path().join("global/MEMORY.md");
        let project_memory = dir.path().join("project/MEMORY.md");
        let codex_root = dir.path().join("codex/memories");
        std::fs::create_dir_all(global_memory.parent().unwrap()).unwrap();
        std::fs::create_dir_all(project_memory.parent().unwrap()).unwrap();
        std::fs::write(&global_memory, "global Claude fact").unwrap();
        std::fs::write(&project_memory, "project Claude fact").unwrap();
        sync_claude_auto_memory_from(&store_path, &project, Some(&global_memory), true).unwrap();
        sync_claude_auto_memory_from(&store_path, &project, Some(&project_memory), false).unwrap();
        add(&store_path, "local fact", Some(project.clone())).unwrap();

        for (visibility, global, project_import) in [
            (ClaudeImportVisibility::Global, true, false),
            (ClaudeImportVisibility::Project, false, true),
            (ClaudeImportVisibility::Disabled, false, false),
        ] {
            synchronize_native_memories_at(
                &store_path,
                &project,
                None,
                Some(&codex_root),
                visibility,
            )
            .unwrap();
            let codex = std::fs::read_to_string(codex_root.join("MEMORY.md")).unwrap();
            assert!(codex.contains("local fact"));
            assert_eq!(codex.contains("global Claude fact"), global);
            assert_eq!(codex.contains("project Claude fact"), project_import);
        }
    }

    #[test]
    fn imported_entries_are_owned_by_their_source_file() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = store(&dir);
        let project = Path::new("/project");
        let memory = dir.path().join("MEMORY.md");
        std::fs::write(&memory, "imported fact").unwrap();
        sync_claude_auto_memory_from(&store_path, project, Some(&memory), false).unwrap();
        let imported = entries(&store_path).unwrap().pop().unwrap();

        // Forget names the authority instead of deleting a row the importer
        // would immediately recreate.
        let error = forget(&store_path, imported.id).unwrap_err().to_string();
        assert!(error.contains("MEMORY.md"), "{error}");
        assert_eq!(entries(&store_path).unwrap().len(), 1);

        // Removing it at the source removes it here on the next refresh.
        std::fs::remove_file(&memory).unwrap();
        sync_claude_auto_memory_from(&store_path, project, Some(&memory), false).unwrap();
        assert!(entries(&store_path).unwrap().is_empty());
    }

    #[test]
    fn forget_still_removes_locally_authored_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        let entry = add(&path, "locally authored", None).unwrap();
        assert!(forget(&path, entry.id).unwrap().is_some());
        assert!(entries(&path).unwrap().is_empty());
    }

    #[test]
    fn claude_disable_env_uses_claude_truthiness() {
        for value in ["1", "true", "YES", "on"] {
            assert!(claude_env_true(value));
        }
        for value in ["", "0", "false", "no", "off", "anything"] {
            assert!(!claude_env_true(value));
        }
    }

    #[test]
    fn session_memory_supports_claude_and_codex_primaries_and_reflects_toggles() {
        use crate::roster::AdapterKind;

        let defaults = crate::config::MemoryConfig::default();
        let project = Path::new("/tmp/proj");
        // Unknown/custom adapters remain opt-in.
        assert!(SessionMemory::from_config(&defaults, project, None).is_none());
        assert!(
            SessionMemory::from_config(&defaults, project, Some(AdapterKind::Claude)).is_some()
        );
        assert!(SessionMemory::from_config(&defaults, project, Some(AdapterKind::Codex)).is_some());

        // Disabling native memory keeps a cleanup-only runtime so provider
        // files cannot continue loading an old Mjolnir block.
        let config = crate::config::MemoryConfig {
            enabled: false,
            ..Default::default()
        };
        let memory =
            SessionMemory::from_config(&config, project, Some(AdapterKind::Codex)).unwrap();
        assert!(!memory.inject);
        assert!(memory.cleanup);
        assert!(!memory.tools);

        let config = crate::config::MemoryConfig {
            enabled: true,
            use_memories: false,
            generate_memories: false,
        };
        let memory =
            SessionMemory::from_config(&config, project, Some(AdapterKind::Codex)).unwrap();
        assert!(!memory.inject);
        assert!(memory.cleanup);
        let config = crate::config::MemoryConfig {
            enabled: true,
            use_memories: true,
            generate_memories: false,
        };
        let memory =
            SessionMemory::from_config(&config, project, Some(AdapterKind::Codex)).unwrap();
        assert!(memory.inject);
        assert!(!memory.cleanup);
        assert!(!memory.tools);
        assert_eq!(memory.project, PathBuf::from("/tmp/proj"));
    }

    #[test]
    fn native_memory_sync_reloads_the_saved_toggle() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let mut config = crate::config::Config::default();
        config.save(&config_path).unwrap();
        assert!(native_memory_enabled(Some(&config_path), false));

        config.memory.use_memories = false;
        config.save(&config_path).unwrap();
        assert!(!native_memory_enabled(Some(&config_path), true));
    }

    #[test]
    fn inject_only_supplies_knowledge_without_the_save_tools() {
        use crate::roster::AdapterKind;

        let defaults = crate::config::MemoryConfig::default();
        let project = Path::new("/tmp/proj");
        let memory = SessionMemory::inject_only(&defaults, project, Some(AdapterKind::Codex))
            .expect("codex worker lanes receive memory");
        assert!(memory.inject);
        assert!(!memory.cleanup);
        assert!(!memory.tools, "workers never get memory_save");
        let memory = SessionMemory::inject_only(&defaults, project, Some(AdapterKind::Claude))
            .expect("claude worker lanes receive memory");
        assert!(memory.inject);
        assert!(!memory.cleanup);
        // Unknown/custom adapters remain opt-in, as for primaries.
        assert!(SessionMemory::inject_only(&defaults, project, None).is_none());

        let config = crate::config::MemoryConfig {
            enabled: false,
            ..Default::default()
        };
        let memory =
            SessionMemory::inject_only(&config, project, Some(AdapterKind::Codex)).unwrap();
        assert!(!memory.inject);
        assert!(memory.cleanup);
        let config = crate::config::MemoryConfig {
            enabled: true,
            use_memories: false,
            generate_memories: true,
        };
        let memory =
            SessionMemory::inject_only(&config, project, Some(AdapterKind::Codex)).unwrap();
        assert!(!memory.inject);
        assert!(memory.cleanup);
    }

    #[test]
    fn worker_lane_memory_maps_adapter_source_ids() {
        let defaults = crate::config::MemoryConfig::default();
        let project = Path::new("/tmp/proj");
        let memory = worker_lane_memory_with(&defaults, "codex-acp", project)
            .expect("codex lanes receive memory");
        assert!(memory.inject);
        assert!(!memory.tools);
        let memory = worker_lane_memory_with(&defaults, "claude-acp", project)
            .expect("claude lanes receive memory");
        assert!(memory.inject);
        assert!(worker_lane_memory_with(&defaults, "custom:bridge", project).is_none());
    }
    #[test]
    fn render_list_scopes_to_the_current_project() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        add(&path, "global fact", None).unwrap();
        add(&path, "proj fact", Some(PathBuf::from("/tmp/proj"))).unwrap();
        add(&path, "other fact", Some(PathBuf::from("/tmp/other"))).unwrap();
        let config = crate::config::MemoryConfig {
            enabled: true,
            use_memories: true,
            generate_memories: false,
        };
        let listing = render_list(&path, Path::new("/tmp/proj"), &config);
        assert!(!listing.contains("DISABLED"));
        assert!(listing.contains("use: on"));
        assert!(listing.contains("generate: off"));
        assert!(listing.contains("[m1] global fact"));
        assert!(listing.contains("[m2] proj fact"));
        assert!(!listing.contains("other fact"));
        assert!(listing.contains("Other projects: 1 memory"));
    }

    #[test]
    fn render_list_flags_a_disabled_feature_but_keeps_entries_visible() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        add(&path, "fact", None).unwrap();
        let config = crate::config::MemoryConfig {
            enabled: false,
            ..Default::default()
        };
        let listing = render_list(&path, Path::new("/tmp/proj"), &config);
        assert!(listing.contains("memory: DISABLED"));
        assert!(listing.contains("[m1] fact"), "entries stay visible");
        assert!(render_full_list(&path, &config).contains("memory: DISABLED"));
    }

    #[test]
    fn render_full_list_groups_every_project() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        add(&path, "global fact", None).unwrap();
        add(&path, "proj fact", Some(PathBuf::from("/tmp/proj"))).unwrap();
        add(&path, "other fact", Some(PathBuf::from("/tmp/other"))).unwrap();
        let listing = render_full_list(&path, &crate::config::MemoryConfig::default());
        assert!(!listing.contains("DISABLED"));
        assert!(listing.contains("Global:"));
        assert!(listing.contains("/tmp/proj:"));
        assert!(listing.contains("/tmp/other:"));
        assert!(listing.contains("other fact"));
    }

    #[test]
    fn tool_router_lists_both_memory_tools() {
        let router = McpHandler::tool_router();
        let names: Vec<String> = router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert!(names.contains(&"memory_save".to_string()));
        assert!(names.contains(&"memory_forget".to_string()));
    }

    #[tokio::test]
    async fn memory_save_tool_scopes_by_the_global_flag() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        let handler = McpHandler::new(&SessionMemory {
            store_path: path.clone(),
            config_path: None,
            project: PathBuf::from("/tmp/proj"),
            inject: false,
            cleanup: false,
            tools: true,
        });
        handler
            .memory_save(Parameters(MemorySaveArgs {
                text: "proj fact".into(),
                global: false,
            }))
            .await
            .unwrap();
        handler
            .memory_save(Parameters(MemorySaveArgs {
                text: "user fact".into(),
                global: true,
            }))
            .await
            .unwrap();
        let all = entries(&path).unwrap();
        assert_eq!(all[0].project.as_deref(), Some(Path::new("/tmp/proj")));
        assert_eq!(all[1].project, None);
    }

    #[tokio::test]
    async fn memory_forget_tool_reports_unknown_ids_as_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = store(&dir);
        add(&path, "fact", None).unwrap();
        let handler = McpHandler::new(&SessionMemory {
            store_path: path.clone(),
            config_path: None,
            project: PathBuf::from("/tmp/proj"),
            inject: false,
            cleanup: false,
            tools: true,
        });
        let ok = handler
            .memory_forget(Parameters(MemoryForgetArgs { id: 1 }))
            .await
            .unwrap();
        assert_ne!(ok.is_error, Some(true));
        let missing = handler
            .memory_forget(Parameters(MemoryForgetArgs { id: 1 }))
            .await
            .unwrap();
        assert_eq!(missing.is_error, Some(true));
        assert!(entries(&path).unwrap().is_empty());
    }
}
