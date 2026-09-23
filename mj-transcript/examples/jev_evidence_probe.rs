//! Offline Jev experiment adapter. Never opens an mj instance or changes history.
use std::io::Read;

use anyhow::{Result, ensure};
use mj_core::archive::CanonicalSessionSnapshot;
use mj_transcript::summary::{SummaryRole, TranscriptSummary};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    snapshot: CanonicalSessionSnapshot,
    mode: String,
    #[serde(default)]
    replay_events: Vec<mj_core::relay::RelayEvent>,
    #[serde(default)]
    selected_ids: Vec<String>,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    48 * 1024
}

fn project(mut request: Request) -> Result<Value> {
    if !request.replay_events.is_empty() {
        use mj_transcript::projection::{
            ProjectionIndex, apply_committed_projection_event_indexed,
            canonical_session_from_materialized, materialized_session_from_canonical,
            project_relay_event_indexed,
        };
        let mut state =
            materialized_session_from_canonical("offline-experiment", &request.snapshot)?;
        let mut index = ProjectionIndex::new(&state);
        for event in &request.replay_events {
            let projected = project_relay_event_indexed(&state, &index, event)?;
            apply_committed_projection_event_indexed(
                &mut state,
                &mut index,
                event,
                projected.mutation,
            )?;
        }
        request.snapshot = canonical_session_from_materialized(&state)?;
    }
    if request.mode == "replay" {
        return Ok(serde_json::to_value(&request.snapshot)?);
    }
    ensure!(
        request.limit <= default_limit(),
        "summary limit exceeds 48 KiB"
    );
    let mut summary = TranscriptSummary::from_snapshot(&request.snapshot);
    let user = summary
        .entries
        .iter()
        .rposition(|entry| entry.role == SummaryRole::User)
        .ok_or_else(|| anyhow::anyhow!("no delivered user message in snapshot"))?;
    let user_position = summary.entries[user].position;
    let user_text = summary.entries[user].text.clone();
    let assistant = summary.entries[user..]
        .iter()
        .rev()
        .find(|entry| entry.role == SummaryRole::Assistant)
        .map(|entry| entry.text.clone())
        .unwrap_or_default();
    let candidates: Vec<_> = summary.entries[user..]
        .iter()
        .filter(|entry| entry.role == SummaryRole::Tool)
        .map(|entry| json!({"id":entry.id, "name":entry.text, "call_id":entry.tool.as_ref().and_then(|t| t.get("toolCallId")), "position":entry.position}))
        .collect();
    match request.mode.as_str() {
        "baseline" => {}
        "no_tools" => summary = summary.latest_user_messages(),
        "scoped" | "selected" => {
            summary.entries.drain(..user);
            if request.mode == "selected" {
                for id in &request.selected_ids {
                    ensure!(
                        candidates.iter().any(|c| c["id"] == *id),
                        "unknown selected tool ID: {id}"
                    );
                }
                summary.entries.retain(|entry| {
                    entry.role != SummaryRole::Tool || request.selected_ids.contains(&entry.id)
                });
            }
        }
        other => anyhow::bail!("unknown mode: {other}"),
    }
    let (text, retained_ids) = summary.render_with_ids(request.limit);
    let visible_candidates: Vec<_> = candidates
        .into_iter()
        .filter(|candidate| {
            candidate["id"]
                .as_str()
                .is_some_and(|id| retained_ids.iter().any(|retained| retained == id))
        })
        .collect();
    Ok(json!({
        "text": text,
        "candidates": visible_candidates,
        "latest_user_position":user_position,
        "user_text": user_text,
        "assistant_text": assistant,
    }))
}

fn main() -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    println!("{}", project(serde_json::from_str(&input)?)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(mode: &str) -> Request {
        let mut items = Vec::new();
        for (position, body) in [
            json!({"kind":"user", "content":[{"type":"text","text":"OLD_USER"}]}),
            json!({"kind":"tool", "call":{"toolCallId":"old","title":"old tool","status":"completed"}}),
            json!({"kind":"user", "content":[{"type":"text","text":"NEW_USER"}]}),
            json!({"kind":"tool", "call":{"toolCallId":"new","title":"command","status":"completed","rawInput":{"command":"NOISE".repeat(30000)}}}),
            json!({"kind":"agent", "chunks":[{"content":{"type":"text","text":"DONE"}}],"streaming":false}),
        ].into_iter().enumerate() {
            items.push(json!({"stable_id":format!("item-{position}"),"position":position+1,
                "latest_content_event_ordinal":null,"created_at_ms":0,"last_changed_at_ms":0,"body":body}));
        }
        serde_json::from_value(json!({"snapshot":{"event_frontier":5,"event_frontier_digest":"",
            "session":{"execution":{"state":"idle"},"last_activity_at_ms":0,"session_title":null,"configuration":{}},
            "transcript":items,"queued_prompts":[]},"mode":mode})).unwrap()
    }

    #[test]
    fn scopes_before_budgeting_and_removes_large_tool_bodies() {
        let baseline = project(request("baseline")).unwrap();
        assert_eq!(baseline["latest_user_position"], 3);
        let scoped = project(request("scoped")).unwrap();
        assert!(!scoped["text"].as_str().unwrap().contains("OLD_USER"));
        let no_tools = project(request("no_tools")).unwrap();
        let text = no_tools["text"].as_str().unwrap();
        assert!(text.contains("NEW_USER") && text.contains("DONE"));
        assert!(!text.contains("NOISE") && !text.contains("bytes omitted"));
        assert_eq!(scoped["candidates"].as_array().unwrap().len(), 1);
        assert!(no_tools["candidates"].as_array().unwrap().is_empty());
    }

    #[test]
    fn selected_calls_must_belong_to_current_turn() {
        let mut input = request("selected");
        input.selected_ids.push("missing".into());
        assert!(project(input).is_err());
        let selected = project(request("selected")).unwrap();
        assert!(!selected["text"].as_str().unwrap().contains("<tool"));
    }
}
