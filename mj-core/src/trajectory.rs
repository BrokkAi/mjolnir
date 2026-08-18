//! Neutral trajectory-tracking and rendering infrastructure shared by the
//! orchestrators and code agent: boundary detection over the raw ACP event
//! stream, and the condensed tool-call log rendering built on top
//! of it.

use std::collections::HashMap;

use similar::TextDiff;

use crate::event::UiEvent;

#[derive(Debug, Clone)]
pub struct Checkpoint {
    #[allow(dead_code)]
    pub step: u64,
    pub text: String,
    #[allow(dead_code)]
    pub activities: Vec<String>,
}

#[derive(Default)]
pub struct BoundaryTracker {
    trajectory: String,
    review_trajectory: String,
    final_message: String,
    segment: String,
    lane: Option<SegmentLane>,
    tools: HashMap<String, agent_client_protocol::schema::v1::ToolCall>,
    terminals: HashMap<String, TerminalTool>,
    next_step: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SegmentLane {
    Message,
    Thought,
}

#[derive(Clone)]
struct TerminalTool {
    activity: String,
    head: String,
}

impl BoundaryTracker {
    pub fn observe(&mut self, event: &UiEvent) -> Option<Checkpoint> {
        use agent_client_protocol::schema::v1::{
            SessionUpdate, ToolCall, ToolCallContent, ToolCallStatus,
        };
        let flush = |this: &mut Self| {
            this.lane.take()?;
            if this.segment.trim().is_empty() {
                this.segment.clear();
                return None;
            }
            Some(std::mem::take(&mut this.segment))
        };
        let append = |this: &mut Self, lane: SegmentLane, text: &str| {
            if this.segment.is_empty() {
                this.segment.push_str("**agent**:\n");
            }
            if this.lane != Some(lane) {
                if this.lane.is_some() {
                    this.segment.push('\n');
                }
                if lane == SegmentLane::Thought {
                    this.segment.push_str("_thinking:_ ");
                }
                this.lane = Some(lane);
            }
            this.segment.push_str(text);
        };
        let boundary: Option<(String, String, Vec<String>)> = match event {
            UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(chunk)) => {
                let text = crate::event::content_block_text(&chunk.content);
                append(self, SegmentLane::Message, &text);
                self.final_message.push_str(&text);
                None
            }
            UiEvent::SessionUpdate(SessionUpdate::AgentThoughtChunk(chunk)) => {
                append(
                    self,
                    SegmentLane::Thought,
                    &crate::event::content_block_text(&chunk.content),
                );
                None
            }
            UiEvent::SessionUpdate(SessionUpdate::ToolCall(call)) => {
                for content in &call.content {
                    if let ToolCallContent::Terminal(terminal) = content {
                        self.terminals.insert(
                            terminal.terminal_id.to_string(),
                            TerminalTool {
                                activity: tool_activity(call),
                                head: tool_call_head(call),
                            },
                        );
                    }
                }
                self.tools
                    .insert(call.tool_call_id.to_string(), call.clone());
                tool_completes_agent_message_segment(call).then(|| {
                    self.final_message.clear();
                    let activity = tool_activity(call);
                    let previous = flush(self);
                    (
                        join_agent_boundary(previous.clone(), render_tool_delta(call)),
                        join_agent_boundary(previous, render_review_tool_delta(call)),
                        vec![activity],
                    )
                })
            }
            UiEvent::SessionUpdate(SessionUpdate::ToolCallUpdate(update)) => {
                let id = update.tool_call_id.to_string();
                let completed = matches!(
                    update.fields.status,
                    Some(ToolCallStatus::Completed | ToolCallStatus::Failed)
                );
                let rendered = {
                    let tool = self
                        .tools
                        .entry(id.clone())
                        .or_insert_with(|| ToolCall::new(id.clone(), "tool"));
                    tool.update(update.fields.clone());
                    (completed && tool_completes_agent_message_segment(tool))
                        .then(|| (render_tool_delta(tool), render_review_tool_delta(tool)))
                };
                rendered.map(|(rendered, review_rendered)| {
                    self.final_message.clear();
                    let activity = self
                        .tools
                        .get(&id)
                        .map_or_else(|| "tool".to_string(), tool_activity);
                    let previous = flush(self);
                    (
                        join_agent_boundary(previous.clone(), rendered),
                        join_agent_boundary(previous, review_rendered),
                        vec![activity],
                    )
                })
            }
            UiEvent::SessionUpdate(SessionUpdate::Plan(plan)) => {
                self.final_message.clear();
                let text = join_boundary(flush(self), format!("plan update:\n{plan:?}"));
                Some((text.clone(), text, vec!["plan update".to_string()]))
            }
            UiEvent::TerminalOutput(snapshot)
                if terminal_output_completes_agent_message_segment(snapshot) =>
            {
                self.final_message.clear();
                let terminal = self
                    .terminals
                    .get(&snapshot.terminal_id)
                    .cloned()
                    .unwrap_or_else(|| TerminalTool {
                        activity: "terminal".to_string(),
                        head: "→ terminal()".to_string(),
                    });
                let text = join_agent_boundary(
                    flush(self),
                    render_terminal_result(&terminal.head, snapshot),
                );
                Some((text.clone(), text, vec![terminal.activity]))
            }
            UiEvent::PromptDone { stop_reason, .. } => {
                use agent_client_protocol::schema::v1::StopReason;
                // The concluding message otherwise never reaches a tool
                // boundary, so flush it as its own reviewable checkpoint.
                // Cancelled turns are user aborts; they discard the partial
                // segment through reset_attempt.
                (!matches!(stop_reason, StopReason::Cancelled))
                    .then(|| flush(self))
                    .flatten()
                    .map(|segment| (segment.clone(), segment, vec!["final response".to_string()]))
            }
            UiEvent::PromptFailed { .. } => None,
            _ => None,
        };
        boundary.map(|(text, review_text, activities)| {
            self.next_step += 1;
            self.trajectory.push_str(&text);
            self.trajectory.push('\n');
            self.review_trajectory.push_str(&review_text);
            self.review_trajectory.push('\n');
            Checkpoint {
                step: self.next_step,
                text,
                activities,
            }
        })
    }

    #[cfg(test)]
    pub fn trajectory(&self) -> String {
        self.trajectory.clone()
    }

    pub fn review_trajectory(&self) -> String {
        self.review_trajectory.clone()
    }

    pub fn final_message(&self) -> String {
        self.final_message.clone()
    }

    pub fn reset_attempt(&mut self) {
        self.final_message.clear();
        self.segment.clear();
        self.lane = None;
        self.tools.clear();
        self.terminals.clear();
    }
}

fn tool_has_terminal(tool: &agent_client_protocol::schema::v1::ToolCall) -> bool {
    use agent_client_protocol::schema::v1::ToolCallContent;
    tool.content
        .iter()
        .any(|content| matches!(content, ToolCallContent::Terminal(_)))
}

/// A completed non-terminal tool starts a new candidate final-message segment.
/// Both the trajectory tracker and headless result collector use this boundary
/// so the user-facing answer cannot drift from the orchestrator's definition.
pub fn tool_completes_agent_message_segment(
    tool: &agent_client_protocol::schema::v1::ToolCall,
) -> bool {
    use agent_client_protocol::schema::v1::ToolCallStatus;
    matches!(
        tool.status,
        ToolCallStatus::Completed | ToolCallStatus::Failed
    ) && !tool_has_terminal(tool)
}

pub fn terminal_output_completes_agent_message_segment(
    snapshot: &crate::event::TerminalOutputSnapshot,
) -> bool {
    snapshot.exit_status.is_some()
}

fn render_tool_delta(tool: &agent_client_protocol::schema::v1::ToolCall) -> String {
    render_tool_delta_with_diffs(tool, true)
}

fn render_review_tool_delta(tool: &agent_client_protocol::schema::v1::ToolCall) -> String {
    render_tool_delta_with_diffs(tool, false)
}

fn render_tool_delta_with_diffs(
    tool: &agent_client_protocol::schema::v1::ToolCall,
    include_diffs: bool,
) -> String {
    use agent_client_protocol::schema::v1::{ToolCallContent, ToolCallStatus};
    let output = tool_output_text(tool);
    let lines = line_count(&output);
    let count = line_count_label(lines);
    let mut text = match tool.status {
        ToolCallStatus::Completed => format!("{} ⇒ ok · {count}", tool_call_head(tool)),
        ToolCallStatus::Failed => format!("{} ⇒ error · {count}", tool_call_head(tool)),
        _ => format!("{} ⇒ pending", tool_call_head(tool)),
    };
    if matches!(tool.status, ToolCallStatus::Failed)
        && let Some(first) = tool
            .raw_output
            .as_ref()
            .and_then(first_error_value)
            .or_else(|| {
                output
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .map(str::to_string)
            })
    {
        text.push_str(" — ");
        text.push_str(&one_line(first.trim(), 120));
    }
    if include_diffs {
        for content in &tool.content {
            if let ToolCallContent::Diff(diff) = content {
                let diff = unified_diff(diff);
                if !diff.trim().is_empty() {
                    text.push('\n');
                    text.push_str(&fence_diff(&diff));
                }
            }
        }
    }
    if let Some(intent) = tool
        .raw_input
        .as_ref()
        .and_then(|value| find_json_string(value, &["i"]))
        .filter(|intent| !intent.trim().is_empty())
    {
        format!("// {}\n{text}", one_line(&intent, 80))
    } else {
        text
    }
}

fn tool_call_head(tool: &agent_client_protocol::schema::v1::ToolCall) -> String {
    let activity = tool_activity(tool);
    let primary = tool
        .raw_input
        .as_ref()
        .and_then(|value| primary_arg(&activity, value))
        .unwrap_or_default();
    format!("→ {activity}({primary})")
}

fn render_terminal_result(
    activity: &str,
    snapshot: &crate::event::TerminalOutputSnapshot,
) -> String {
    let lines = line_count(&snapshot.output);
    let count = line_count_label(lines);
    let failed = snapshot.exit_status.as_ref().is_some_and(|status| {
        status.exit_code.is_some_and(|code| code != 0) || status.signal.is_some()
    });
    let mut text = format!(
        "{activity} ⇒ {} · {count}",
        if failed { "error" } else { "ok" }
    );
    if failed && let Some(first) = snapshot.output.lines().find(|line| !line.trim().is_empty()) {
        text.push_str(" — ");
        text.push_str(&one_line(first, 120));
    }
    text
}

fn unified_diff(diff: &agent_client_protocol::schema::v1::Diff) -> String {
    let path = diff.path.display().to_string();
    let relative = path.trim_start_matches('/');
    let old_header = diff
        .old_text
        .as_ref()
        .map_or_else(|| "/dev/null".to_string(), |_| format!("a/{relative}"));
    let new_header = format!("b/{relative}");
    TextDiff::from_lines(diff.old_text.as_deref().unwrap_or(""), &diff.new_text)
        .unified_diff()
        .context_radius(3)
        .header(&old_header, &new_header)
        .to_string()
}

fn fence_diff(diff: &str) -> String {
    let longest = diff.split(|ch| ch != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.saturating_add(1).max(3));
    format!("{fence}diff\n{diff}{fence}")
}

fn line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.split('\n').count()
    }
}

fn line_count_label(lines: usize) -> String {
    format!("{lines} {}", if lines == 1 { "line" } else { "lines" })
}

fn first_error_value(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => ["error", "stderr", "message"]
            .iter()
            .find_map(|key| map.get(*key))
            .and_then(|value| match value {
                serde_json::Value::String(text) => text.lines().next().map(str::to_string),
                value => Some(value.to_string()),
            })
            .or_else(|| map.values().find_map(first_error_value)),
        serde_json::Value::Array(values) => values.iter().find_map(first_error_value),
        _ => None,
    }
}

fn tool_output_text(tool: &agent_client_protocol::schema::v1::ToolCall) -> String {
    use agent_client_protocol::schema::v1::ToolCallContent;
    let mut parts = Vec::new();
    for content in &tool.content {
        if let ToolCallContent::Content(content) = content {
            parts.push(crate::event::content_block_text(&content.content));
        }
    }
    if let Some(output) = tool.raw_output.as_ref() {
        collect_json_text(output, &mut parts);
    }
    parts.join("\n")
}

fn collect_json_text(value: &serde_json::Value, parts: &mut Vec<String>) {
    match value {
        serde_json::Value::String(value) => parts.push(value.clone()),
        serde_json::Value::Array(values) => {
            for value in values {
                collect_json_text(value, parts);
            }
        }
        serde_json::Value::Object(map) => {
            for value in map.values() {
                collect_json_text(value, parts);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn tool_activity(tool: &agent_client_protocol::schema::v1::ToolCall) -> String {
    let metadata = tool
        .meta
        .as_ref()
        .and_then(|meta| serde_json::to_value(meta).ok());
    metadata
        .as_ref()
        .and_then(|value| find_json_string(value, &["toolName", "tool_name", "name"]))
        .or_else(|| {
            tool.raw_input
                .as_ref()
                .and_then(|value| find_json_string(value, &["toolName", "tool_name"]))
        })
        .unwrap_or_else(|| tool.title.clone())
}

fn find_json_string(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            for key in keys {
                if let Some(value) = map.get(*key).and_then(serde_json::Value::as_str) {
                    return Some(value.to_string());
                }
            }
            map.values().find_map(|value| find_json_string(value, keys))
        }
        serde_json::Value::Array(values) => values
            .iter()
            .find_map(|value| find_json_string(value, keys)),
        _ => None,
    }
}

fn primary_arg(activity: &str, value: &serde_json::Value) -> Option<String> {
    if activity == "grep" {
        let pattern = find_json_primary_value(value, "pattern");
        let paths = find_json_primary_value(value, "path")
            .or_else(|| find_json_primary_value(value, "paths"));
        match (pattern, paths) {
            (Some(pattern), Some(paths)) => {
                return Some(one_line(&format!("{pattern} @ {paths}"), 120));
            }
            (Some(pattern), None) => return Some(one_line(&pattern, 120)),
            (None, Some(paths)) => return Some(one_line(&paths, 120)),
            (None, None) => {}
        }
    }
    if activity == "glob"
        && let Some(paths) = find_json_primary_value(value, "path")
            .or_else(|| find_json_primary_value(value, "paths"))
    {
        return Some(one_line(&paths, 120));
    }
    if activity == "ast_grep"
        && let Some(pattern) = find_json_primary_value(value, "pat")
    {
        return Some(one_line(&pattern, 120));
    }
    for key in [
        "path",
        "file_path",
        "filePath",
        "command",
        "cmd",
        "pattern",
        "url",
        "query",
        "prompt",
        "assignment",
        "note",
        "message",
        "op",
        "name",
        "id",
    ] {
        if let Some(primary) = find_json_primary_value(value, key) {
            return Some(one_line(&primary, 120));
        }
    }
    first_non_intent_string(value)
        .map(|value| one_line(&value, 120))
        .or_else(|| {
            (!matches!(value, serde_json::Value::Null)).then(|| one_line(&value.to_string(), 120))
        })
}

fn first_non_intent_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => map.iter().find_map(|(key, value)| {
            if key == "i" {
                None
            } else if let Some(value) = value.as_str().filter(|value| !value.is_empty()) {
                Some(value.to_string())
            } else {
                first_non_intent_string(value)
            }
        }),
        serde_json::Value::Array(values) => values.iter().find_map(first_non_intent_string),
        _ => None,
    }
}

fn find_json_primary_value(value: &serde_json::Value, key: &str) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => map
            .get(key)
            .and_then(|value| match value {
                serde_json::Value::String(value) if !value.is_empty() => Some(value.clone()),
                serde_json::Value::Array(values)
                    if !values.is_empty()
                        && values.iter().all(|value| value.as_str().is_some()) =>
                {
                    Some(
                        values
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", "),
                    )
                }
                _ => None,
            })
            .or_else(|| {
                map.values()
                    .find_map(|value| find_json_primary_value(value, key))
            }),
        serde_json::Value::Array(values) => values
            .iter()
            .find_map(|value| find_json_primary_value(value, key)),
        _ => None,
    }
}

fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut shortened = flat.chars().take(max.saturating_sub(1)).collect::<String>();
    shortened.push('…');
    shortened
}

fn join_boundary(previous: Option<String>, current: String) -> String {
    previous.map_or(current.clone(), |previous| format!("{previous}\n{current}"))
}

fn join_agent_boundary(previous: Option<String>, current: String) -> String {
    previous.map_or_else(
        || format!("**agent**:\n{current}"),
        |previous| format!("{previous}\n{current}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{SessionUpdate, ToolCall, ToolCallStatus};

    #[test]
    fn tool_reviews_only_run_at_completed_or_failed_boundaries() {
        let mut tracker = BoundaryTracker::default();
        let tool = |status| {
            UiEvent::SessionUpdate(SessionUpdate::ToolCall(
                ToolCall::new("tool-1", "build").status(status),
            ))
        };

        assert!(tracker.observe(&tool(ToolCallStatus::Pending)).is_none());
        assert!(tracker.observe(&tool(ToolCallStatus::InProgress)).is_none());
        assert!(tracker.observe(&tool(ToolCallStatus::Completed)).is_some());
        assert!(tracker.observe(&tool(ToolCallStatus::Failed)).is_some());
    }

    #[test]
    fn completed_tool_delta_uses_compact_omp_shape() {
        let mut tracker = BoundaryTracker::default();
        let event = UiEvent::SessionUpdate(SessionUpdate::ToolCall(
            ToolCall::new("tool-1", "run tests")
                .raw_input(serde_json::json!({"command": "cargo test"}))
                .raw_output(serde_json::json!({"exit": 1, "stderr": "boom"}))
                .status(ToolCallStatus::Failed),
        ));
        let delta = tracker.observe(&event).expect("completed tool boundary");
        assert_eq!(
            delta.text,
            "**agent**:\n→ run tests(cargo test) ⇒ error · 1 line — boom"
        );
        assert!(!delta.text.contains("stderr"));
    }

    #[test]
    fn prompt_done_flushes_the_final_message_as_a_checkpoint() {
        use agent_client_protocol::schema::v1::{
            ContentBlock, ContentChunk, StopReason, TextContent,
        };

        let mut tracker = BoundaryTracker::default();
        let message = UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new("done: the fix is in place")),
        )));
        assert!(tracker.observe(&message).is_none());

        let done = UiEvent::PromptDone {
            stop_reason: StopReason::EndTurn,
            usage: None,
        };
        let checkpoint = tracker.observe(&done).expect("final response checkpoint");
        assert_eq!(checkpoint.text, "**agent**:\ndone: the fix is in place");
        assert_eq!(checkpoint.activities, ["final response"]);

        // Nothing pending afterwards: a second completion yields no checkpoint.
        assert!(
            tracker
                .observe(&UiEvent::PromptDone {
                    stop_reason: StopReason::EndTurn,
                    usage: None,
                })
                .is_none()
        );
    }

    #[test]
    fn cancelled_prompt_done_does_not_flush_a_final_checkpoint() {
        use agent_client_protocol::schema::v1::{
            ContentBlock, ContentChunk, StopReason, TextContent,
        };

        let mut tracker = BoundaryTracker::default();
        let message = UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new("partial")),
        )));
        assert!(tracker.observe(&message).is_none());
        assert!(
            tracker
                .observe(&UiEvent::PromptDone {
                    stop_reason: StopReason::Cancelled,
                    usage: None,
                })
                .is_none()
        );
    }

    #[test]
    fn message_and_thought_transitions_wait_for_a_semantic_checkpoint() {
        use agent_client_protocol::schema::v1::{ContentBlock, ContentChunk, TextContent};

        let mut tracker = BoundaryTracker::default();
        let message = UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new("checking")),
        )));
        let thought = UiEvent::SessionUpdate(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(" next")),
        )));
        assert!(tracker.observe(&message).is_none());
        assert!(tracker.observe(&thought).is_none());

        let tool = UiEvent::SessionUpdate(SessionUpdate::ToolCall(
            ToolCall::new("tool", "cargo test").status(ToolCallStatus::Completed),
        ));
        let checkpoint = tracker.observe(&tool).expect("tool checkpoint");
        assert!(checkpoint.text.contains("**agent**:\nchecking"));
        assert!(checkpoint.text.contains("_thinking:_  next"));
        assert_eq!(checkpoint.step, 1);
    }

    #[test]
    fn successful_tool_projects_shape_without_raw_result_body() {
        let tool = ToolCall::new("tool", "search")
            .raw_input(serde_json::json!({
                "action": {"type": "mcpToolCall", "toolName": "explore_agent"},
                "query": "find config"
            }))
            .raw_output(serde_json::json!({"result": "large successful body\nsecond line"}))
            .status(ToolCallStatus::Completed);
        let projected = render_tool_delta(&tool);
        assert_eq!(projected, "→ explore_agent(find config) ⇒ ok · 2 lines");
        assert!(!projected.contains("large successful body"));
    }

    #[test]
    fn tool_projection_includes_omp_intent_and_argument_formatting() {
        let tool = ToolCall::new("tool", "grep")
            .raw_input(serde_json::json!({
                "pattern": "Seat\\s+enum",
                "paths": ["src/roster.rs", "src/agent_usage.rs"],
                "i": "Find   the seat declaration"
            }))
            .status(ToolCallStatus::Completed);

        assert_eq!(
            render_tool_delta(&tool),
            "// Find the seat declaration\n→ grep(Seat\\s+enum @ src/roster.rs, src/agent_usage.rs) ⇒ ok · 0 lines"
        );
    }

    #[test]
    fn edit_projection_renders_a_unified_hunk_instead_of_file_snapshots() {
        use agent_client_protocol::schema::v1::{Diff, ToolCallContent};

        let old = [
            "far-start",
            "one",
            "two",
            "three",
            "old value",
            "five",
            "six",
            "seven",
            "eight",
            "far-end",
        ]
        .join("\n");
        let new = old.replace("old value", "new value");
        let tool = ToolCall::new("tool", "edit")
            .raw_input(serde_json::json!({"path": "src/lib.rs"}))
            .content(vec![ToolCallContent::Diff(
                Diff::new("src/lib.rs", new).old_text(old),
            )])
            .status(ToolCallStatus::Completed);

        let projected = render_tool_delta(&tool);
        assert!(projected.starts_with("→ edit(src/lib.rs) ⇒ ok · 0 lines\n```diff\n"));
        assert!(projected.contains("--- a/src/lib.rs\n+++ b/src/lib.rs"));
        assert!(projected.contains("-old value\n+new value"));
        assert!(!projected.contains("far-start"));
        assert!(!projected.contains("far-end"));
        assert!(projected.ends_with("```"));

        let review_projected = render_review_tool_delta(&tool);
        assert_eq!(review_projected, "→ edit(src/lib.rs) ⇒ ok · 0 lines");

        let mut tracker = BoundaryTracker::default();
        tracker
            .observe(&UiEvent::SessionUpdate(SessionUpdate::ToolCall(tool)))
            .expect("edit checkpoint");
        assert!(tracker.trajectory().contains("```diff"));
        assert!(!tracker.review_trajectory().contains("```diff"));
        assert!(
            tracker
                .review_trajectory()
                .contains("→ edit(src/lib.rs) ⇒ ok")
        );
    }

    #[test]
    fn new_file_and_large_replacement_diffs_are_complete() {
        use agent_client_protocol::schema::v1::Diff;

        let created = unified_diff(&Diff::new("src/new.rs", "fn main() {}\n"));
        assert!(created.contains("--- /dev/null\n+++ b/src/new.rs"));
        assert!(created.contains("+fn main() {}"));

        let old = (0..2_000)
            .map(|line| format!("old line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = (0..2_000)
            .map(|line| format!("new line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let replacement = unified_diff(&Diff::new("large.txt", new).old_text(old));
        assert!(replacement.len() > 16 * 1024);
        assert!(replacement.contains("-old line 1999"));
        assert!(replacement.contains("+new line 1999"));
        assert!(!replacement.contains("item truncated"));
    }

    #[test]
    fn diff_fence_outlasts_backticks_in_edited_content() {
        let fenced = fence_diff("@@ -1 +1 @@\n-```old\n+```new\n");
        assert!(fenced.starts_with("````diff\n"));
        assert!(fenced.ends_with("````"));
    }

    #[test]
    fn trajectory_accumulation_is_not_context_truncated() {
        let text = "x".repeat(100 * 1024);
        let mut tracker = BoundaryTracker {
            trajectory: text.clone(),
            ..BoundaryTracker::default()
        };
        tracker.trajectory.push_str("tail");
        assert!(tracker.trajectory().starts_with(&text));
        assert!(tracker.trajectory().ends_with("tail"));
    }

    #[test]
    fn terminal_backed_tool_emits_only_the_terminal_exit_checkpoint() {
        use crate::event::TerminalOutputSnapshot;
        use agent_client_protocol::schema::v1::{Terminal, TerminalExitStatus, ToolCallContent};

        let mut tracker = BoundaryTracker::default();
        let pending = UiEvent::SessionUpdate(SessionUpdate::ToolCall(
            ToolCall::new("tool", "printf")
                .content(vec![ToolCallContent::Terminal(Terminal::new("term"))])
                .status(ToolCallStatus::InProgress),
        ));
        assert!(tracker.observe(&pending).is_none());
        tracker.observe(&UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
            agent_client_protocol::schema::v1::ContentChunk::new(
                agent_client_protocol::schema::v1::ContentBlock::Text(
                    agent_client_protocol::schema::v1::TextContent::new(
                        "progress before terminal completion",
                    ),
                ),
            ),
        )));
        let terminal = UiEvent::TerminalOutput(TerminalOutputSnapshot {
            terminal_id: "term".into(),
            output: "alpha\nbeta\n".into(),
            truncated: false,
            exit_status: Some(TerminalExitStatus::new().exit_code(0)),
        });
        let checkpoint = tracker.observe(&terminal).expect("terminal checkpoint");
        assert_eq!(checkpoint.step, 1);
        assert_eq!(
            checkpoint.text,
            "**agent**:\nprogress before terminal completion\n→ printf() ⇒ ok · 3 lines"
        );
        assert!(
            tracker.final_message().is_empty(),
            "terminal completion supersedes pre-tool progress"
        );

        let completed = UiEvent::SessionUpdate(SessionUpdate::ToolCall(
            ToolCall::new("tool", "printf")
                .content(vec![ToolCallContent::Terminal(Terminal::new("term"))])
                .status(ToolCallStatus::Completed),
        ));
        assert!(tracker.observe(&completed).is_none());
    }

    #[test]
    fn compaction_signal_is_not_a_semantic_step() {
        let mut tracker = BoundaryTracker::default();

        assert!(tracker.observe(&UiEvent::ContextCompacted).is_none());
    }
}
