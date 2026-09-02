//! Bifrost: the semantic analysis every turn review depends on.
//!
//! Bifrost is a first-party code-analysis tool baked into the session's
//! container image. A review uses it twice. Before the reviewing agents start,
//! `analyze_diff` compares the two captured Git trees and reports which
//! callables the turn introduced, edited, moved, or deleted -- the "change
//! packet" the supervisor and validator prompts embed. During the review, the
//! same binary runs as an MCP server so the agents can navigate the repository
//! and run the slop-cop analyzers.
//!
//! There is no degraded mode. A missing binary, a spawn failure, an analysis
//! error, or a timeout fails the review loudly with a message naming the fix,
//! because a review without its instruments is not the review this feature
//! promises. The analysis types and the packet formatting are ported from
//! mjolnir's `mj-agents/src/discrete_review.rs`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;

use super::{CHANGED_FUNCTIONS_LIMIT, bound_review_section};

/// Wall-clock budget for one repository's semantic diff analysis. Large
/// changesets can require several minutes before review fan-out can begin.
pub const ANALYZE_DIFF_TIMEOUT: Duration = Duration::from_secs(600);

/// The binary baked into the container image. An operator or a test can point
/// this elsewhere, which is also how the worker's own tests drive a fake.
pub const BIFROST_BIN_ENV: &str = "MJ_BIFROST_BIN";
const DEFAULT_BIFROST_BIN: &str = "bifrost";

/// What a review runs Bifrost as.
#[must_use]
pub fn bifrost_binary() -> PathBuf {
    crate::hel_config::env_override_os("BIFROST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_BIFROST_BIN))
}

/// The MCP server command line for one reviewed repository. `toolset` is
/// `core` for the supervisor, validator and quick reviewer, and `core|slopcop`
/// for a specialist lane, whose analyzers live in the `slopcop` set.
#[must_use]
pub fn mcp_server_args(repository: &Path, toolset: &str) -> Vec<String> {
    vec![
        "--root".to_string(),
        repository.display().to_string(),
        "--mcp".to_string(),
        toolset.to_string(),
    ]
}

/// The Bifrost MCP servers one reviewing role gets: one per reviewed
/// repository, named the way the review prompts name them, so an agent told to
/// "use the server whose root contains the changed path" can.
#[must_use]
pub fn review_mcp_servers(
    repositories: &[PathBuf],
    toolset: &str,
) -> Vec<crate::hel_worker_launch::ReviewMcpServer> {
    let binary = bifrost_binary();
    repositories
        .iter()
        .enumerate()
        .map(
            |(index, repository)| crate::hel_worker_launch::ReviewMcpServer {
                name: if index == 0 {
                    "bifrost".to_string()
                } else {
                    format!("bifrost_{}", index + 1)
                },
                command: binary.clone(),
                args: mcp_server_args(repository, toolset),
            },
        )
        .collect()
}

#[derive(Debug, Deserialize)]
struct AnalyzeDiffEnvelope {
    #[serde(rename = "structuredContent")]
    structured_content: AnalyzeDiffResult,
}

#[derive(Debug, Default, Deserialize)]
pub struct AnalyzeDiffResult {
    #[serde(default)]
    file_changes: Vec<FileChange>,
    #[serde(default)]
    patch_symbols: PatchSymbols,
}

#[derive(Debug, Default, Deserialize)]
struct FileChange {
    #[serde(default)]
    old_path: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    status: String,
    #[serde(default)]
    insertions: usize,
    #[serde(default)]
    deletions: usize,
    #[serde(default)]
    is_binary: bool,
    #[serde(default)]
    is_test: bool,
    #[serde(default)]
    is_parseable: bool,
}

#[derive(Debug, Default, Deserialize)]
struct PatchSymbols {
    #[serde(default)]
    edited: Vec<EditedSymbol>,
    #[serde(default)]
    introduced: Vec<IntroducedSymbol>,
    #[serde(default)]
    deleted: Vec<DeletedSymbol>,
    #[serde(default)]
    moved: Vec<SymbolPair>,
    #[serde(default)]
    signature_changes: Vec<SymbolPair>,
}

#[derive(Debug, Deserialize)]
struct EditedSymbol {
    after: PatchSymbol,
}

#[derive(Debug, Deserialize)]
struct IntroducedSymbol {
    after: PatchSymbol,
}

#[derive(Debug, Deserialize)]
struct DeletedSymbol {
    before: PatchSymbol,
}

#[derive(Debug, Deserialize)]
struct SymbolPair {
    before: PatchSymbol,
    after: PatchSymbol,
}

#[derive(Debug, Default, Deserialize)]
struct PatchSymbol {
    #[serde(default)]
    fqn: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    signature: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    start_line: usize,
    #[serde(default)]
    end_line: usize,
    #[serde(default)]
    change_reason: String,
}

impl AnalyzeDiffResult {
    #[must_use]
    pub fn changed_line_count(&self) -> usize {
        self.file_changes.iter().fold(0, |total, change| {
            total.saturating_add(change.insertions.saturating_add(change.deletions))
        })
    }

    #[must_use]
    pub fn diffstat(&self) -> String {
        let mut lines = self
            .file_changes
            .iter()
            .map(format_file_change)
            .collect::<Vec<_>>();
        let insertions = self.file_changes.iter().fold(0usize, |total, change| {
            total.saturating_add(change.insertions)
        });
        let deletions = self.file_changes.iter().fold(0usize, |total, change| {
            total.saturating_add(change.deletions)
        });
        let file_count = self.file_changes.len();
        let mut summary = format!(
            "{file_count} {} changed",
            if file_count == 1 { "file" } else { "files" }
        );
        if insertions > 0 {
            summary.push_str(&format!(
                ", {insertions} {}(+)",
                if insertions == 1 {
                    "insertion"
                } else {
                    "insertions"
                }
            ));
        }
        if deletions > 0 {
            summary.push_str(&format!(
                ", {deletions} {}(-)",
                if deletions == 1 {
                    "deletion"
                } else {
                    "deletions"
                }
            ));
        }
        lines.push(summary);
        lines.join("\n")
    }
}

fn format_file_change(change: &FileChange) -> String {
    // These semantic flags are intentionally retained in the decoded schema;
    // the review packet's stat rendering only needs the path and line totals.
    let _semantic_metadata = (change.status.as_str(), change.is_test, change.is_parseable);
    let path = match (&change.old_path, &change.path) {
        (Some(old), Some(new)) if old != new => format!("{old} => {new}"),
        (Some(old), _) => old.clone(),
        (_, Some(new)) => new.clone(),
        (None, None) => "<unknown path>".to_string(),
    };
    let detail = if change.is_binary {
        "binary".to_string()
    } else {
        let changed = change.insertions.saturating_add(change.deletions);
        format!(
            "{changed} changed ({} insertion{}, {} deletion{})",
            change.insertions,
            if change.insertions == 1 { "" } else { "s" },
            change.deletions,
            if change.deletions == 1 { "" } else { "s" },
        )
    };
    format!(" {path} | {detail}")
}

/// The changed-callable packet the review prompts embed.
#[must_use]
pub fn format_changed_functions(analysis: &AnalyzeDiffResult) -> String {
    let mut entries = Vec::new();
    for symbol in &analysis.patch_symbols.introduced {
        push_changed_function(&mut entries, "introduced", &symbol.after);
    }
    for symbol in &analysis.patch_symbols.edited {
        push_changed_function(&mut entries, "edited", &symbol.after);
    }
    for moved in &analysis.patch_symbols.moved {
        // Bifrost reports ordinary line shifts as moves. Only a path change is
        // strong evidence that the turn actually moved a callable rather than
        // inserting text above it.
        if moved.before.path != moved.after.path && is_callable(&moved.after.kind) {
            entries.push(format!(
                "- moved {} -> {}",
                display_symbol(&moved.before),
                display_symbol(&moved.after)
            ));
        }
    }
    for signature_change in &analysis.patch_symbols.signature_changes {
        if is_callable(&signature_change.after.kind) {
            entries.push(format!(
                "- signature changed {} -> {}",
                display_symbol(&signature_change.before),
                display_symbol(&signature_change.after)
            ));
        }
    }
    for symbol in &analysis.patch_symbols.deleted {
        push_changed_function(&mut entries, "deleted", &symbol.before);
    }
    entries.sort();
    entries.dedup();
    if entries.is_empty() {
        "No callable symbols changed between the captured turn trees.".to_string()
    } else {
        entries.join("\n")
    }
}

fn push_changed_function(entries: &mut Vec<String>, change: &str, symbol: &PatchSymbol) {
    if is_callable(&symbol.kind) {
        let reason = if symbol.change_reason.trim().is_empty() {
            String::new()
        } else {
            format!("; {}", symbol.change_reason.trim())
        };
        entries.push(format!("- {change}: {}{reason}", display_symbol(symbol)));
    }
}

fn display_symbol(symbol: &PatchSymbol) -> String {
    let identity = if !symbol.signature.trim().is_empty() {
        symbol.signature.trim()
    } else if !symbol.fqn.trim().is_empty() {
        symbol.fqn.trim()
    } else {
        symbol.name.trim()
    };
    format!(
        "{}:{}-{} `{identity}` ({})",
        symbol.path, symbol.start_line, symbol.end_line, symbol.kind
    )
}

fn is_callable(kind: &str) -> bool {
    let kind = kind.to_ascii_lowercase();
    ["function", "method", "constructor", "procedure", "closure"]
        .iter()
        .any(|candidate| kind.contains(candidate))
}

/// One repository's analysis request.
#[derive(Debug, Clone)]
pub struct AnalyzeRequest {
    pub repository: PathBuf,
    /// The review baseline tree, or the repository's empty tree when the
    /// baseline is the start of coverage.
    pub base_tree: String,
    pub target_tree: String,
}

/// Runs `analyze_diff` over each repository and renders one packet.
///
/// Every repository must succeed. A partial packet would tell the supervisor
/// that a repository changed nothing when in truth Bifrost could not read it,
/// which is the fabricated-evidence failure the whole design refuses.
pub async fn changed_functions_packet(requests: &[AnalyzeRequest]) -> Result<String, String> {
    let mut sections = Vec::new();
    for request in requests {
        let analysis = analyze_diff(request).await?;
        sections.push(format!(
            "Repository: {}\n{}",
            request.repository.display(),
            format_changed_functions(&analysis)
        ));
    }
    Ok(bound_review_section(
        &sections.join("\n\n"),
        CHANGED_FUNCTIONS_LIMIT,
        "changed functions",
    ))
}

/// Runs Bifrost's one-shot `analyze_diff` for one repository.
pub async fn analyze_diff(request: &AnalyzeRequest) -> Result<AnalyzeDiffResult, String> {
    let binary = bifrost_binary();
    tracing::info!(
        event = "review_analyze_diff_started",
        bifrost = %binary.display(),
        root = %request.repository.display(),
        base_tree = %request.base_tree,
        target_tree = %request.target_tree,
        "running bifrost analyze_diff for the captured turn trees"
    );
    let args = serde_json::json!({
        "base": request.base_tree,
        "target": request.target_tree,
    })
    .to_string();
    let mut command = tokio::process::Command::new(&binary);
    command
        .current_dir(&request.repository)
        .kill_on_drop(true)
        // The capture ran against a scratch index; a review must never inherit
        // it, or Bifrost would read a half-written index as the repository.
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .arg("--root")
        .arg(&request.repository)
        .args(["--tool", "analyze_diff", "--args"])
        .arg(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // `output()` drains stdout and stderr concurrently with the wait, so a
    // child that fills a pipe buffer cannot deadlock this call. stdin is
    // closed, so there is nothing to feed it.
    let output = match tokio::time::timeout(ANALYZE_DIFF_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "the review needs the `{}` binary, which this session's container image does not have; rebuild the image so it includes Bifrost",
                binary.display()
            ));
        }
        Ok(Err(error)) => return Err(format!("could not run bifrost: {error}")),
        Err(_) => {
            return Err(format!(
                "bifrost analysis of {} exceeded its {}s budget",
                request.repository.display(),
                ANALYZE_DIFF_TIMEOUT.as_secs()
            ));
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "bifrost exited with {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    let envelope: AnalyzeDiffEnvelope = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("invalid analyze_diff JSON: {error}"))?;
    Ok(envelope.structured_content)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANALYSIS: &str = r#"{
      "structuredContent": {
        "file_changes": [
          {"path": "src/lib.rs", "status": "modified", "insertions": 12, "deletions": 3,
           "is_binary": false, "is_test": false, "is_parseable": true},
          {"old_path": "src/old.rs", "path": "src/new.rs", "status": "renamed",
           "insertions": 1, "deletions": 1}
        ],
        "patch_symbols": {
          "introduced": [{"after": {"fqn": "app::retry", "name": "retry", "kind": "function",
             "signature": "fn retry(times: usize)", "path": "src/lib.rs",
             "start_line": 10, "end_line": 20, "change_reason": "new callable"}}],
          "edited": [{"after": {"fqn": "app::run", "name": "run", "kind": "method",
             "signature": "fn run(&self)", "path": "src/lib.rs", "start_line": 30, "end_line": 40}}],
          "deleted": [{"before": {"fqn": "app::old", "name": "old", "kind": "function",
             "path": "src/old.rs", "start_line": 1, "end_line": 5}}],
          "moved": [
            {"before": {"name": "moved", "kind": "function", "path": "src/old.rs",
              "start_line": 1, "end_line": 2},
             "after": {"name": "moved", "kind": "function", "path": "src/new.rs",
              "start_line": 1, "end_line": 2}},
            {"before": {"name": "shifted", "kind": "function", "path": "src/lib.rs",
              "start_line": 1, "end_line": 2},
             "after": {"name": "shifted", "kind": "function", "path": "src/lib.rs",
              "start_line": 5, "end_line": 6}}
          ],
          "signature_changes": [
            {"before": {"name": "sig", "kind": "function", "signature": "fn sig()",
              "path": "src/lib.rs", "start_line": 50, "end_line": 51},
             "after": {"name": "sig", "kind": "function", "signature": "fn sig(x: u8)",
              "path": "src/lib.rs", "start_line": 50, "end_line": 52}},
            {"before": {"name": "Config", "kind": "struct", "path": "src/lib.rs",
              "start_line": 60, "end_line": 61},
             "after": {"name": "Config", "kind": "struct", "path": "src/lib.rs",
              "start_line": 60, "end_line": 62}}
          ]
        }
      }
    }"#;

    fn analysis() -> AnalyzeDiffResult {
        serde_json::from_str::<AnalyzeDiffEnvelope>(ANALYSIS)
            .expect("the fixture matches the analyze_diff envelope")
            .structured_content
    }

    #[test]
    fn a_changed_function_packet_names_every_callable_the_turn_touched() {
        let packet = format_changed_functions(&analysis());
        assert!(packet.contains(
            "- introduced: src/lib.rs:10-20 `fn retry(times: usize)` (function); new callable"
        ));
        assert!(packet.contains("- edited: src/lib.rs:30-40 `fn run(&self)` (method)"));
        assert!(packet.contains("- deleted: src/old.rs:1-5 `app::old` (function)"));
        assert!(packet.contains("- moved src/old.rs:1-2 `moved` (function) -> src/new.rs:1-2"));
        assert!(
            !packet.contains("shifted"),
            "a line shift inside one file is not a move"
        );
        assert!(packet.contains("- signature changed src/lib.rs:50-51 `fn sig()`"));
        assert!(
            !packet.contains("Config"),
            "only callables reach the changed-function packet"
        );
    }

    #[test]
    fn an_empty_analysis_says_so_rather_than_rendering_nothing() {
        let packet = format_changed_functions(&AnalyzeDiffResult::default());
        assert_eq!(
            packet,
            "No callable symbols changed between the captured turn trees."
        );
    }

    #[test]
    fn an_analysis_diffstat_lists_files_and_totals() {
        let analysis = analysis();
        assert_eq!(analysis.changed_line_count(), 17);
        let diffstat = analysis.diffstat();
        assert!(diffstat.contains(" src/lib.rs | 15 changed (12 insertions, 3 deletions)"));
        assert!(diffstat.contains(" src/old.rs => src/new.rs | 2 changed"));
        assert!(diffstat.ends_with("2 files changed, 13 insertions(+), 4 deletions(-)"));
    }

    #[test]
    fn the_mcp_server_command_names_the_root_and_toolset() {
        let args = mcp_server_args(Path::new("/w/app"), "core|slopcop");
        assert_eq!(args, vec!["--root", "/w/app", "--mcp", "core|slopcop"]);
    }

    #[test]
    fn one_bifrost_server_is_attached_per_reviewed_repository() {
        let servers =
            review_mcp_servers(&[PathBuf::from("/w/app"), PathBuf::from("/w/lib")], "core");
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "bifrost");
        assert_eq!(servers[1].name, "bifrost_2");
        assert!(servers[1].args.contains(&"/w/lib".to_string()));
        assert!(
            review_mcp_servers(&[], "core").is_empty(),
            "a review with no repositories attaches no analyzer"
        );
    }

    #[tokio::test]
    async fn a_missing_bifrost_binary_fails_the_review_with_the_fix() {
        // Safety: the env var is process-global; this test names a unique
        // path and does not race another test that reads it, because no other
        // test in this module spawns Bifrost.
        unsafe {
            std::env::set_var(BIFROST_BIN_ENV, "/nonexistent/hel-review-bifrost");
        }
        let error = analyze_diff(&AnalyzeRequest {
            repository: std::env::temp_dir(),
            base_tree: "base".to_string(),
            target_tree: "target".to_string(),
        })
        .await
        .expect_err("a missing binary must fail the review");
        unsafe {
            std::env::remove_var(BIFROST_BIN_ENV);
        }
        assert!(error.contains("rebuild the image"), "{error}");
    }
}
