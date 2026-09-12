//! Transcript conversion and presentation rules shared by terminal and web clients.
use agent_client_protocol::schema::v1::{Plan, ToolCall, ToolCallStatus};
use mj_core::state::{MaterializedSession, TerminalOutputRecord, TranscriptBody, TranscriptItem};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::web::{BrowserDiffStat, BrowserTranscript, BrowserTranscriptEntry};
use mj_core::transcript::*;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptRenderMode {
    Rich,
    Raw,
}

impl TranscriptRenderMode {
    pub fn toggled(self) -> Self {
        match self {
            Self::Rich => Self::Raw,
            Self::Raw => Self::Rich,
        }
    }
}

/// Whether an entry renders on its own, renders nothing, or heads a collapsed
/// streak of completed tools and thoughts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryCollapse {
    None,
    /// A completed tool the user explicitly opened. It is a streak boundary
    /// and renders its provider title plus all available details in Rich mode.
    Expanded,
    /// Member of a collapsed run whose summary renders on the run head.
    Hidden,
    /// Detail the decluttered feed leaves out.
    Omitted,
    /// Head of a collapsed streak spanning `self..end` (end exclusive).
    Summary {
        end: usize,
        fingerprint: u64,
    },
}

pub const BROWSER_TRANSCRIPT_LINES: usize = 1_000;
pub const BROWSER_LINE_BYTES: usize = 4 * 1024;

pub fn browser_transcript(
    source_entries: &[ChatEntry],
    latest_seq: u64,
    last_compaction_seq: u64,
    after_seq: Option<u64>,
) -> BrowserTranscript {
    let collapsed_restarts =
        collapsed_session_restart_states(source_entries, TranscriptRenderMode::Rich);
    let collapse =
        entry_collapse_states(source_entries, TranscriptRenderMode::Rich, &BTreeSet::new());
    let restart_collapse_window_start = restart_collapse_window_start_seq(
        source_entries,
        &collapsed_restarts,
        TranscriptRenderMode::Rich,
    );
    let mut entries = browser_projection_entries(source_entries, &collapse, last_compaction_seq)
        .into_iter()
        .map(|entry| browser_entry(&entry))
        .collect::<Vec<_>>();
    let mut remaining = BROWSER_TRANSCRIPT_LINES;
    for entry in entries.iter_mut().rev() {
        if entry.lines.len() > remaining {
            let omitted = entry
                .lines
                .len()
                .saturating_sub(remaining.saturating_sub(1));
            if remaining == 0 {
                entry.lines.clear();
            } else {
                entry.lines.drain(..omitted);
                entry
                    .lines
                    .insert(0, format!("[… {omitted} earlier lines omitted …]"));
                entry.lines.truncate(remaining);
            }
        }
        remaining = remaining.saturating_sub(entry.lines.len());
    }
    entries.retain(|entry| !entry.lines.is_empty());
    if remaining == 0 {
        while entries.first().is_some_and(|entry| entry.lines.is_empty()) {
            entries.remove(0);
        }
    }
    // A collapse can remove an entry a viewer already received even when
    // older, unrelated entries remain in the window. Move the reset
    // boundary to the newest replacement marker so the append-only web
    // feed is rebuilt in that case; the server applies this same boundary
    // to cached projections when serving `after_seq` deltas.
    let window_start_seq = entries
        .iter()
        .map(|entry| entry.id)
        .min()
        .unwrap_or(latest_seq)
        .max(restart_collapse_window_start.unwrap_or_default());
    let reset = after_seq.is_some_and(|after| after < window_start_seq);
    if let Some(after) = after_seq.filter(|_| !reset) {
        entries.retain(|entry| entry.updated_seq > after);
    }
    BrowserTranscript {
        latest_seq,
        presentation_key: rich_presentation_key(source_entries, &collapse, last_compaction_seq),
        window_start_seq,
        reset,
        entries,
    }
}

pub fn materialized_chat_entries(session: &MaterializedSession) -> Vec<ChatEntry> {
    materialized_chat_entries_with_diffstats(session, &BTreeMap::new())
}

pub fn materialized_chat_entries_with_diffstats(
    session: &MaterializedSession,
    diffstats: &BTreeMap<String, Vec<String>>,
) -> Vec<ChatEntry> {
    let mut entries = session
        .transcript
        .iter()
        .map(|item| {
            materialized_chat_entry_with_diffstats(
                item,
                session.applied_event_ordinal,
                diffstats.get(&item.stable_id),
            )
        })
        .collect::<Vec<_>>();
    suppress_duplicate_standalone_terminal_output(&mut entries);
    entries
}

/// How many transcript items a freshly opened chat converts on the caller's
/// thread. Several screens of scrollback are ready in the first frame, and the
/// rest of the history is converted off the event loop; a long session has
/// thousands of items, and converting them all inline costs seconds.
pub const TAIL_SEED_ITEMS: usize = 256;

/// Entries for the transcript items in `items`, which is a prefix of a
/// session's transcript. Runs off the event loop, so it takes the items by
/// slice rather than the session.
pub fn materialized_prefix_entries(items: &[Arc<TranscriptItem>], frontier: u64) -> Vec<ChatEntry> {
    let mut entries = items
        .iter()
        .map(|item| materialized_chat_entry(item, frontier))
        .collect::<Vec<_>>();
    suppress_duplicate_standalone_terminal_output(&mut entries);
    entries
}

/// Rebuilds the entry list, keeping the entries whose transcript item did not
/// change. An unchanged item is the same `Arc` as in the previous projection,
/// so pointer identity settles reuse without reading a single field. The
/// field comparison is the fallback for items that were rebuilt with equal
/// content, which is what a restore from the canonical log produces.
///
/// `skip` is the number of leading transcript items that are not converted
/// yet, so `previous` lines up with `session.transcript[skip..]`. It is zero
/// for a complete projection.
pub fn materialized_chat_entries_reusing(
    session: &MaterializedSession,
    skip: usize,
    previous: Vec<ChatEntry>,
) -> Vec<ChatEntry> {
    let mut previous = previous.into_iter();
    let mut entries = session
        .transcript
        .iter()
        .skip(skip)
        .map(|item| {
            let Some(mut entry) = previous.next() else {
                return materialized_chat_entry(item, session.applied_event_ordinal);
            };
            if entry.source.is(item) {
                entry.seq = item_update_ordinal(item, session.applied_event_ordinal);
                return entry;
            }
            if entry_matches_transcript_item(&entry, item) {
                entry.seq = item_update_ordinal(item, session.applied_event_ordinal);
                entry.source = TranscriptSource(Some(item.clone()));
                return entry;
            }
            materialized_chat_entry(item, session.applied_event_ordinal)
        })
        .collect::<Vec<_>>();
    suppress_duplicate_standalone_terminal_output(&mut entries);
    entries
}

/// Hide a legacy standalone result from Rich surfaces when a completed tool
/// already carries the exact same raw result. New projections attach and
/// remove this duplicate at the source; this pass keeps transcripts projected
/// by an older Hel release equally quiet after an upgrade.
pub fn suppress_duplicate_standalone_terminal_output(entries: &mut [ChatEntry]) {
    if !entries.iter().any(|entry| {
        entry
            .source
            .0
            .as_ref()
            .is_some_and(|item| matches!(&item.body, TranscriptBody::TerminalOutput { .. }))
    }) {
        return;
    }
    let tools = entries
        .iter()
        .filter_map(|entry| entry.source.0.as_ref())
        .filter(|item| matches!(&item.body, TranscriptBody::Tool { .. }))
        .cloned()
        .collect::<Vec<_>>();
    for entry in entries {
        let Some(item) = entry.source.0.as_ref() else {
            continue;
        };
        let TranscriptBody::TerminalOutput { record } = &item.body else {
            continue;
        };
        entry.raw_only = record.exited_cleanly()
            || tools.iter().any(|tool| {
                let TranscriptBody::Tool { call, .. } = &tool.body else {
                    unreachable!("filtered to tool transcript items above");
                };
                record.matches_tool_raw_result(call)
            });
    }
}

pub fn entry_matches_transcript_item(entry: &ChatEntry, item: &TranscriptItem) -> bool {
    entry.start_seq == item.position
        && entry.recorded_at_ms == Some(item.created_at_ms)
        && entry.revision == u64::try_from(item.last_changed_at_ms).unwrap_or_default()
        && entry.role == entry_role(item)
        && match &item.body {
            TranscriptBody::Agent { .. } | TranscriptBody::Thought { .. } => {
                entry.message_id.as_deref() == Some(item.stable_id.as_str())
            }
            TranscriptBody::Tool { .. } => {
                entry.tool_call_id.as_deref() == Some(item.stable_id.as_str())
            }
            _ => true,
        }
}

/// The role one transcript item renders under.
///
/// The matcher and the builder both read it, so an entry is reused only when
/// it would be rebuilt the same way.
pub fn entry_role(item: &TranscriptItem) -> ChatRole {
    match &item.body {
        // A prompt Hel generated for a review is a control-origin record.
        // Rendering it as the user's would put words in their mouth, so it
        // reads as Hel's own line instead.
        TranscriptBody::User { content } => {
            if mj_core::second_opinion::is_control_origin_prompt(&materialized_content_text(
                content,
            )) {
                ChatRole::System
            } else {
                ChatRole::User
            }
        }
        TranscriptBody::Agent { .. } => ChatRole::Agent,
        TranscriptBody::Thought { .. } => ChatRole::Thought,
        TranscriptBody::Tool { .. } => ChatRole::Tool,
        TranscriptBody::Plan { .. } => ChatRole::Plan,
        TranscriptBody::PlanProposal { .. } => ChatRole::PlanProposal,
        TranscriptBody::System { .. } | TranscriptBody::TerminalOutput { .. } => ChatRole::System,
    }
}

pub fn materialized_chat_entry(item: &Arc<TranscriptItem>, frontier: u64) -> ChatEntry {
    materialized_chat_entry_with_diffstats(item, frontier, None)
}

/// The latest relay ordinal at which this item's rendered form can have
/// changed. It is the entry's update cursor, so a remote viewer polling with
/// `after_seq` receives an entry again only when the projection says the entry
/// moved: stamping every entry with the frontier retransmits the whole window
/// on every event.
///
/// Overshooting is safe and undershooting is not, so an item whose changes the
/// projection records no ordinal for keeps the frontier. Those are the bodies
/// the projection edits in place — thoughts, tool calls, plans, and loose
/// terminal output — and they stay conservative until the projection records a
/// change ordinal for them the way it already does for agent messages.
pub fn item_update_ordinal(item: &TranscriptItem, frontier: u64) -> u64 {
    let latest = match &item.body {
        // Created once and never revisited, so the creating event is exact.
        TranscriptBody::User { .. }
        | TranscriptBody::System { .. }
        | TranscriptBody::PlanProposal { .. } => item.position,
        // Every appended chunk records the ordinal that appended it. Closing
        // the stream is the only other edit and nothing rendered reads it.
        TranscriptBody::Agent { .. } => item.latest_content_event_ordinal.unwrap_or(frontier),
        TranscriptBody::Thought { .. }
        | TranscriptBody::Tool { .. }
        | TranscriptBody::Plan { .. }
        | TranscriptBody::TerminalOutput { .. } => frontier,
    };
    latest.max(item.position)
}

pub fn materialized_chat_entry_with_diffstats(
    item: &Arc<TranscriptItem>,
    frontier: u64,
    exact_diffstats: Option<&Vec<String>>,
) -> ChatEntry {
    let mut entry = match &item.body {
        TranscriptBody::User { content } => ChatEntry::plain(
            item.position,
            entry_role(item),
            materialized_content_text(content),
        ),
        TranscriptBody::Agent { chunks, .. } => ChatEntry::plain(
            item.position,
            ChatRole::Agent,
            materialized_chunks_text(chunks),
        ),
        TranscriptBody::Thought { chunks, .. } => ChatEntry::plain(
            item.position,
            ChatRole::Thought,
            materialized_chunks_text(chunks),
        ),
        TranscriptBody::Tool {
            call,
            terminal_outputs,
            presentation,
            ..
        } => {
            let call = match ToolCall::deserialize(call) {
                Ok(call) => Some(call),
                Err(error) => {
                    tracing::warn!(
                        stable_id = %item.stable_id,
                        %error,
                        "could not decode a stored tool call; rendering it as invalid"
                    );
                    None
                }
            };
            let mut entry = ChatEntry::tool(
                item.position,
                call.as_ref()
                    .map_or("[invalid tool call]", |call| call.title.as_str()),
                Some(item.stable_id.clone()),
                call.as_ref()
                    .map_or(ToolStatus::Pending, |call| tool_status(&call.status)),
            );
            if let Some(call) = call {
                let presentation =
                    materialized_tool_call_presentation(presentation.as_deref(), &call);
                entry.tool_summary = Some(presentation.summary.clone());
                entry.tool_presentation = Some(presentation);
                let fallback_terminal = mj_core::acp::is_fallback_terminal_tool_call(&call);
                entry.tool_content =
                    tool_content_details(&call.content, terminal_outputs, call.raw_output.as_ref());
                entry.tool_diffstats = exact_diffstats
                    .cloned()
                    .unwrap_or_else(|| tool_diff_paths(&call.content));
                entry.tool_locations = tool_location_details(&call.locations);
                // Successful fallback calls replace the successful standalone
                // terminal blocks Rich already omitted. Raw mode still keeps
                // every command, while failures remain visible everywhere.
                entry.raw_only = fallback_terminal
                    && !terminal_outputs.is_empty()
                    && terminal_outputs
                        .iter()
                        .all(TerminalOutputRecord::exited_cleanly);
                if fallback_terminal && call.status == ToolCallStatus::Failed {
                    let details = std::mem::take(&mut entry.tool_content);
                    if !details.is_empty() {
                        entry.text.push('\n');
                        entry.text.push_str(&details.join("\n"));
                        // Raw keeps the provider title plus its captured
                        // output in `text`. Rich/browser need the same
                        // failed-call detail while reading the compact
                        // presentation field, so append it only to this
                        // display value; the cached parser metadata remains
                        // the clean command summary above.
                        if let Some(summary) = &mut entry.tool_summary {
                            summary.push('\n');
                            summary.push_str(&details.join("\n"));
                        }
                    }
                }
            }
            entry
        }
        TranscriptBody::TerminalOutput { record } => {
            let mut entry = ChatEntry::plain(
                item.position,
                ChatRole::System,
                sanitize_terminal_text(&terminal_output_detail(record)),
            );
            // Output no tool call refers to is a whole block per command. A
            // command that ended cleanly says nothing the decluttered feed
            // needs, so only the raw transcript carries it; anything abnormal
            // stays visible everywhere.
            entry.raw_only = record.exited_cleanly();
            entry
        }
        TranscriptBody::Plan { plan } => ChatEntry::plan(
            item.position,
            Plan::deserialize(plan)
                .map(|plan| plan.entries)
                .unwrap_or_else(|error| {
                    tracing::warn!(
                        stable_id = %item.stable_id,
                        %error,
                        "could not decode a stored plan; rendering it empty"
                    );
                    Vec::new()
                })
                .into_iter()
                .map(|line| PlanLine {
                    text: sanitize_terminal_text(&line.content),
                    status: plan_status(&line.status),
                })
                .collect(),
        ),
        TranscriptBody::PlanProposal { plan, .. } => {
            ChatEntry::plain(item.position, ChatRole::PlanProposal, plan)
        }
        TranscriptBody::System { text } => ChatEntry::plain(item.position, ChatRole::System, text),
    };
    entry.seq = item_update_ordinal(item, frontier);
    entry.recorded_at_ms = Some(item.created_at_ms);
    entry.revision = u64::try_from(item.last_changed_at_ms).unwrap_or_default();
    if matches!(
        &item.body,
        TranscriptBody::Agent { .. } | TranscriptBody::Thought { .. }
    ) {
        entry.message_id = Some(item.stable_id.clone());
    }
    entry.source = TranscriptSource(Some(item.clone()));
    entry
}

pub fn user_label(entry: &ChatEntry) -> &'static str {
    if entry
        .source
        .0
        .as_ref()
        .and_then(|item| item.stable_id.strip_prefix("user:"))
        .is_some_and(mj_core::relay::is_capacity_retry_command)
    {
        "Automatic · capacity retry"
    } else {
        "You"
    }
}

pub fn browser_entry(entry: &ChatEntry) -> BrowserTranscriptEntry {
    let (role, label) = match entry.role {
        ChatRole::User => ("user", user_label(entry).to_owned()),
        ChatRole::Agent => ("agent", "Agent".to_owned()),
        ChatRole::Thought => ("thought", "Thinking".to_owned()),
        ChatRole::Tool => (
            "tool",
            format!(
                "Tool · {}",
                tool_status_name(entry.tool_status.unwrap_or(ToolStatus::Pending))
            ),
        ),
        ChatRole::Plan => ("plan", "Plan".to_owned()),
        ChatRole::PlanProposal => ("plan-proposal", "Proposed plan".to_owned()),
        ChatRole::System => ("system", "Mjolnir".to_owned()),
    };
    let source = if entry.role == ChatRole::Plan {
        entry
            .plan
            .iter()
            .map(|line| {
                let marker = match line.status {
                    PlanStatus::Pending => "○",
                    PlanStatus::Running => "●",
                    PlanStatus::Completed => "✓",
                };
                format!("{marker} {}", line.text)
            })
            .collect::<Vec<_>>()
    } else if entry.role == ChatRole::Tool {
        // The remote viewer mirrors the TUI's Rich feed: the parser-derived
        // summary plus any diffstat, not the full Raw detail. Summaries are
        // present while a call is pending too, so the title never changes
        // shape merely because the call completed.
        std::iter::once(
            entry
                .tool_summary
                .as_deref()
                .unwrap_or(&entry.text)
                .to_owned(),
        )
        .chain(entry.tool_diffstats.clone())
        .collect()
    } else {
        entry.text.lines().map(str::to_owned).collect()
    };
    BrowserTranscriptEntry {
        id: entry.start_seq,
        updated_seq: entry.seq,
        role,
        label,
        recorded_at_ms: entry.recorded_at_ms,
        lines: source
            .into_iter()
            .map(|line| truncate_browser_line(&line))
            .collect(),
        glyph: entry_glyph(entry),
        tone: entry_tone(entry),
        tool_status: (entry.role == ChatRole::Tool)
            .then(|| tool_status_name(entry.tool_status.unwrap_or(ToolStatus::Pending))),
        diffstats: entry
            .tool_diffstats
            .iter()
            .filter_map(|line| parse_diffstat(line))
            .collect(),
    }
}

/// Build the rows the browser receives from the same collapse decisions the
/// Rich terminal renderer uses. A synthetic row is a normal `ChatEntry` so it
/// can share labels, timestamps, tool state, and the update cursor with the
/// rest of the projection.
pub fn browser_projection_entries(
    entries: &[ChatEntry],
    collapse: &[EntryCollapse],
    last_compaction_seq: u64,
) -> Vec<ChatEntry> {
    let mut projected = Vec::new();
    let mut index = 0;
    while index < entries.len() {
        match collapse[index] {
            EntryCollapse::None => {
                let entry = &entries[index];
                if entry.start_seq > last_compaction_seq && !entry.raw_only {
                    projected.push(entry.clone());
                }
                index += 1;
            }
            EntryCollapse::Expanded => {
                let entry = &entries[index];
                if entry.start_seq > last_compaction_seq && !entry.raw_only {
                    projected.push(entry.clone());
                }
                index += 1;
            }
            EntryCollapse::Omitted | EntryCollapse::Hidden => index += 1,
            EntryCollapse::Summary { end, .. } => {
                let members = entries[index..end]
                    .iter()
                    .filter(|entry| entry.start_seq > last_compaction_seq && !entry.raw_only)
                    .cloned()
                    .collect::<Vec<_>>();
                projected.extend(collapsed_streak_entries(&members));
                index = end;
            }
        }
    }
    projected
}

/// The stable topology signature for Rich's collapsed tool groups. Ordinary
/// appends and content revisions deliberately do not participate: they can
/// use the normal append/update cursor. A changed group membership produces a
/// new key, which tells an append-only browser to rebuild its DOM.
pub fn rich_presentation_key(
    entries: &[ChatEntry],
    collapse: &[EntryCollapse],
    last_compaction_seq: u64,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"rich-presentation-v2");
    let mut index = 0;
    while index < entries.len() {
        match collapse[index] {
            EntryCollapse::Summary { end, .. } => {
                // Include the complete visible membership of the synthetic
                // row, including an omitted/raw-only member. That member is
                // structurally transparent to the browser rows, but its
                // insertion can change which durable entries are represented
                // by a collapsed run.
                if entries[index..end]
                    .iter()
                    .any(|entry| entry.start_seq > last_compaction_seq)
                {
                    digest.update(b"S");
                    for entry in &entries[index..end] {
                        if entry.start_seq <= last_compaction_seq {
                            continue;
                        }
                        digest.update(if entry.raw_only { b"O" } else { b"M" });
                        digest.update(role_tag(entry.role));
                        digest.update(entry.start_seq.to_le_bytes());
                    }
                }
                index = end;
            }
            EntryCollapse::Hidden => {
                if entries[index].start_seq <= last_compaction_seq {
                    index += 1;
                    continue;
                }
                digest.update(b"H");
                digest.update(role_tag(entries[index].role));
                digest.update(entries[index].start_seq.to_le_bytes());
                index += 1;
            }
            EntryCollapse::Omitted => {
                if entries[index].start_seq <= last_compaction_seq {
                    index += 1;
                    continue;
                }
                digest.update(b"O");
                digest.update(role_tag(entries[index].role));
                digest.update(entries[index].start_seq.to_le_bytes());
                index += 1;
            }
            EntryCollapse::None | EntryCollapse::Expanded => index += 1,
        }
    }
    format!("{:x}", digest.finalize())
}

pub fn role_tag(role: ChatRole) -> &'static [u8] {
    match role {
        ChatRole::User => b"user",
        ChatRole::Agent => b"agent",
        ChatRole::Thought => b"thought",
        ChatRole::Tool => b"tool",
        ChatRole::Plan => b"plan",
        ChatRole::PlanProposal => b"plan-proposal",
        ChatRole::System => b"system",
    }
}

/// The semantic colour name for one entry.
///
/// A tool takes its tone from its state, because a failed tool call is the one
/// thing in a transcript a person most needs to find.
pub fn entry_tone(entry: &ChatEntry) -> &'static str {
    match entry.role {
        ChatRole::User => "user",
        ChatRole::Agent => "agent",
        ChatRole::Thought => "thinking",
        ChatRole::Tool => match entry.tool_status.unwrap_or(ToolStatus::Pending) {
            ToolStatus::Pending => "system",
            ToolStatus::Running => "running",
            ToolStatus::Completed => "done",
            ToolStatus::Failed => "failed",
        },
        ChatRole::Plan => "plan",
        ChatRole::PlanProposal => "plan-proposal",
        ChatRole::System => "system",
    }
}

/// Read back what `format_diffstat` wrote.
///
/// It emits the path, two spaces, `+{insertions}`, a space, and `-{deletions}`
/// using a Unicode MINUS SIGN. Parsing it here rather than in the browser
/// keeps one definition of the format beside the code that produces it, and
/// means a line that does not match is dropped rather than mangled.
pub fn parse_diffstat(line: &str) -> Option<BrowserDiffStat> {
    let (path, counts) = line.rsplit_once("  ")?;
    let (added, removed) = counts.split_once(' ')?;
    Some(BrowserDiffStat {
        path: path.trim().to_owned(),
        insertions: added.strip_prefix('+')?.parse().ok()?,
        deletions: removed
            .strip_prefix('\u{2212}')
            .or_else(|| removed.strip_prefix('-'))?
            .parse()
            .ok()?,
    })
}

pub fn truncate_browser_line(line: &str) -> String {
    if line.len() <= BROWSER_LINE_BYTES {
        return line.to_owned();
    }
    const SUFFIX: &str = "… [truncated]";
    let mut end = BROWSER_LINE_BYTES - SUFFIX.len();
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{SUFFIX}", &line[..end])
}

pub const fn tool_status_name(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Pending => "waiting",
        ToolStatus::Running => "running",
        ToolStatus::Completed => "done",
        ToolStatus::Failed => "failed",
    }
}
pub fn is_completed_tool(entry: &ChatEntry) -> bool {
    entry.role == ChatRole::Tool && entry.tool_status == Some(ToolStatus::Completed)
}

pub fn collapse_revision_fingerprint(entries: &[ChatEntry]) -> u64 {
    entries.iter().fold(0u64, |accumulated, entry| {
        accumulated.wrapping_mul(31).wrapping_add(entry.revision)
    })
}

/// Mark all but the newest restart in each run of restart markers. Entries
/// omitted from the selected feed do not interrupt a run, because they cannot
/// be visible between the markers. Raw mode has no omitted entries, while
/// Rich omits clean terminal output. The returned positions let browser and
/// TUI presentation use the same adjacency rule without mutating the durable
/// transcript or its cursors.
pub fn collapsed_session_restart_states(
    entries: &[ChatEntry],
    mode: TranscriptRenderMode,
) -> Vec<bool> {
    let mut hidden = vec![false; entries.len()];
    let mut latest_restart = None;
    for (index, entry) in entries.iter().enumerate() {
        if mode == TranscriptRenderMode::Rich && entry.raw_only {
            continue;
        }
        if entry.is_session_restart() {
            if let Some(previous) = latest_restart {
                hidden[previous] = true;
            }
            latest_restart = Some(index);
        } else {
            latest_restart = None;
        }
    }
    hidden
}

/// The conservative reset boundary for a browser feed whose previous restart
/// marker may have been removed by a later projection. The wire shape only
/// carries one `window_start_seq`, so use the newest replacement marker across
/// all collapsed runs. This can resend a little history, but it guarantees a
/// client never retains a hidden entry in its append-only DOM.
pub fn restart_collapse_window_start_seq(
    entries: &[ChatEntry],
    collapsed_restarts: &[bool],
    mode: TranscriptRenderMode,
) -> Option<u64> {
    let mut latest_restart = None;
    let mut boundary = None;
    for (index, entry) in entries.iter().enumerate() {
        if mode == TranscriptRenderMode::Rich && entry.raw_only {
            continue;
        }
        if entry.is_session_restart() {
            if latest_restart.is_some_and(|previous| collapsed_restarts[previous]) {
                boundary = Some(boundary.map_or(entry.seq, |current: u64| current.max(entry.seq)));
            }
            latest_restart = Some(index);
        } else {
            latest_restart = None;
        }
    }
    boundary
}

/// What every entry renders as, which is the one place the decluttered feed is
/// decided. In rich mode a maximal streak of completed tools and thoughts
/// renders its newest thought followed by one synthetic tool summary. Earlier
/// thoughts are hidden. Multiple thoughts, multiple completed tools, or a
/// thought following a completed tool use the summary layout. A singleton tool
/// uses its parser-derived summary in place, including while it is pending,
/// running, or failed. Every other entry, including an active or failed tool,
/// breaks the streak.
/// A `raw_only` entry renders nothing at all and is transparent to a streak
/// rather than breaking it, since nothing of it is on screen to separate the
/// surrounding entries. Raw mode does not tool-collapse or omit entries, but
/// it still coalesces adjacent restart markers as a presentation rule.
pub fn entry_collapse_states(
    entries: &[ChatEntry],
    mode: TranscriptRenderMode,
    expanded_tool_calls: &BTreeSet<u64>,
) -> Vec<EntryCollapse> {
    let mut states = vec![EntryCollapse::None; entries.len()];
    if mode == TranscriptRenderMode::Rich {
        for (index, entry) in entries.iter().enumerate() {
            if entry.raw_only {
                states[index] = EntryCollapse::Omitted;
            } else if is_completed_tool(entry) && expanded_tool_calls.contains(&entry.start_seq) {
                states[index] = EntryCollapse::Expanded;
            }
        }
    }
    if mode != TranscriptRenderMode::Rich {
        for (index, hidden) in collapsed_session_restart_states(entries, mode)
            .into_iter()
            .enumerate()
        {
            if hidden {
                states[index] = EntryCollapse::Hidden;
            }
        }
        return states;
    }
    let streak_member = |index: usize| {
        entries[index].role == ChatRole::Thought
            || (is_completed_tool(&entries[index])
                && !expanded_tool_calls.contains(&entries[index].start_seq))
    };
    let mut start = 0;
    while start < entries.len() {
        if !streak_member(start) {
            start += 1;
            continue;
        }
        // `end` stops at the last visible member, so an omitted entry the
        // streak reached across is only inside it when another member follows.
        let mut end = start + 1;
        let mut cursor = start + 1;
        while cursor < entries.len() {
            if streak_member(cursor) {
                cursor += 1;
                end = cursor;
            } else if entries[cursor].raw_only {
                cursor += 1;
            } else {
                break;
            }
        }
        let members = &entries[start..end];
        let thoughts = members
            .iter()
            .filter(|entry| entry.role == ChatRole::Thought)
            .count();
        let tools = members
            .iter()
            .filter(|entry| is_completed_tool(entry))
            .count();
        let tool_precedes_thought = members
            .iter()
            .find(|entry| !entry.raw_only)
            .is_some_and(|entry| entry.role == ChatRole::Tool)
            && thoughts > 0;
        if thoughts > 1 || tools > 1 || tool_precedes_thought {
            // A member's update does not bump the head's revision, so fold the
            // streak's revisions into the head's state: its cached rows then
            // drop whenever any member changes.
            let fingerprint = collapse_revision_fingerprint(members);
            states[start] = EntryCollapse::Summary { end, fingerprint };
            // Omitted members render nothing either way, so one state covers
            // everything the head speaks for.
            states[start + 1..end].fill(EntryCollapse::Hidden);
        }
        start = end;
    }
    // Apply restart coalescing last: unlike tool streaks, restart markers are
    // never streak members, but keeping this pass last makes that invariant
    // explicit if more Rich collapse rules are added later.
    for (index, hidden) in collapsed_session_restart_states(entries, mode)
        .into_iter()
        .enumerate()
    {
        if hidden {
            states[index] = EntryCollapse::Hidden;
        }
    }
    states
}

/// The single cell that stands in for a streak of completed tools: each
/// member's compact summary in order. Non-tool entries contribute none.
pub fn collapsed_tool_entry(members: &[ChatEntry]) -> ChatEntry {
    let tools = members
        .iter()
        .filter(|member| is_completed_tool(member))
        .collect::<Vec<_>>();
    let summaries = tools
        .iter()
        .map(|member| member.tool_summary.as_deref().unwrap_or(&member.text))
        .collect::<Vec<_>>()
        .join(", ");
    let first = tools[0];
    let mut summary = ChatEntry::tool(
        first.start_seq,
        summaries.clone(),
        None,
        ToolStatus::Completed,
    );
    summary.seq = members
        .iter()
        .map(|member| member.seq)
        .max()
        .unwrap_or(first.seq);
    summary.revision = members
        .iter()
        .map(|member| member.revision)
        .max()
        .unwrap_or(first.revision);
    summary.recorded_at_ms = tools.iter().rev().find_map(|member| member.recorded_at_ms);
    summary.tool_summary = Some(summaries);
    summary
}

/// Materialize the Rich order for a collapsed streak: newest thought first,
/// followed by the synthetic tool row. This is shared by the terminal and
/// browser projections so an interleaved thought cannot disappear on one
/// surface or move behind the tool summary on the other.
pub fn collapsed_streak_entries(members: &[ChatEntry]) -> Vec<ChatEntry> {
    let mut projected = Vec::new();
    if let Some(thought) = members
        .iter()
        .rev()
        .find(|member| member.role == ChatRole::Thought)
    {
        projected.push(thought.clone());
    }
    let tools = members
        .iter()
        .filter(|member| is_completed_tool(member))
        .collect::<Vec<_>>();
    if tools.len() >= 2 {
        projected.push(collapsed_tool_entry(members));
    } else {
        projected.extend(tools.into_iter().cloned());
    }
    projected
}

pub fn entry_glyph(entry: &ChatEntry) -> &'static str {
    match entry.role {
        ChatRole::User => "❯",
        ChatRole::Agent => "●",
        ChatRole::Thought => "○",
        ChatRole::Plan => "◇",
        ChatRole::PlanProposal => "◈",
        ChatRole::System => "─",
        ChatRole::Tool => match entry.tool_status.unwrap_or(ToolStatus::Pending) {
            ToolStatus::Pending => "•",
            ToolStatus::Running => "●",
            ToolStatus::Completed => "✓",
            ToolStatus::Failed => "×",
        },
    }
}

pub fn materialized_browser_transcript(session: &MaterializedSession) -> BrowserTranscript {
    browser_transcript(
        &materialized_chat_entries(session),
        session.applied_event_ordinal,
        0,
        None,
    )
}
