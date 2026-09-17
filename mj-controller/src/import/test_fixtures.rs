//! Native session fixtures shared by the import tests and by the tests of
//! anything else that reads a native session, such as the SessionWiki
//! adapters. One writer per harness keeps every test writing the same shape.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

pub(crate) const MUSE_ID: &str = "01a08120-6536-7721-8ddf-e0df0e921c2c";

/// Synthetic `chat_history.jsonl`, modeled on the shape Grok Build writes:
/// internally tagged items, user content as typed parts, reasoning
/// summaries beside the assistant turn, and `search_replace` tool calls
/// paired with their results.
pub(crate) fn grok_session(directory: &Path, cwd: &str, history: &str) -> PathBuf {
    let session = directory.join("sessions/%2Fwork%2Fapp/01a00c3a-553f-71e0-95ab-aa04396d3ad7");
    fs::create_dir_all(&session).unwrap();
    fs::write(
        session.join("summary.json"),
        json!({
            "info": {"id": "01a00c3a-553f-71e0-95ab-aa04396d3ad7", "cwd": cwd},
            "session_summary": "",
            "num_chat_messages": 4,
            "current_model_id": "grok-4.6",
            "grok_home": "/home/me/.grok",
        })
        .to_string(),
    )
    .unwrap();
    fs::write(session.join("chat_history.jsonl"), history).unwrap();
    session
}

/// A Kimi Code session directory with the `session_index.jsonl` entry that
/// makes it visible, its `state.json` metadata and one agent wire log.
pub(crate) fn kimi_session(home: &Path, id: &str, cwd: &str, title: &str, wire: &str) -> PathBuf {
    let session = home.join("sessions/project").join(id);
    fs::create_dir_all(session.join("agents/main")).unwrap();
    fs::write(
        session.join("state.json"),
        json!({"workDir": cwd, "customTitle": title}).to_string(),
    )
    .unwrap();
    fs::write(session.join("agents/main/wire.jsonl"), wire).unwrap();
    fs::write(
        home.join("session_index.jsonl"),
        json!({"sessionId": id, "sessionDir": session, "workDir": cwd}).to_string(),
    )
    .unwrap();
    session
}

pub(crate) fn muse_record(id: &str, sequence: u64, payload_type: &str, payload: Value) -> Value {
    json!({
        "schema_version": 1,
        "id": id,
        "stream": {"kind": "session", "id": MUSE_ID},
        "sequence": sequence,
        "recorded_at": 1_788_872_000_000_000_i64 + sequence as i64,
        "record_type": "event",
        "durability": "durable",
        "payload_type": payload_type,
        "payload_schema_version": 1,
        "payload": payload,
    })
}

pub(crate) fn write_muse_session(path: &Path, id: &str, cwd: &Path) {
    let records = [
        muse_record(
            "metadata-1",
            1,
            "runtime.session.metadata",
            json!({"kind":"metadata","record":{"workspace_root":cwd,"provider_id":"test"}}),
        ),
        muse_record(
            "metadata-2",
            2,
            "runtime.session.metadata",
            json!({"kind":"metadata","record":{"workspace_root":cwd,"provider_id":"test"}}),
        ),
        muse_record(
            "intent-1",
            3,
            "runtime.user_intent.accepted",
            json!({"intent_id":"run-1","model_messages":[{"content":[{"kind":"text","text":"remember Muse import"}]}]}),
        ),
        muse_record(
            "started-1",
            4,
            "runtime.session",
            json!({"kind":"run","run_id":"run-1","event":{"kind":"started"}}),
        ),
        muse_record(
            "tools-1",
            5,
            "runtime.session",
            json!({"kind":"run","run_id":"run-1","event":{"kind":"assistant_tool_calls_committed","tool_calls":[{"id":"call-1","name":"write_file","args":"{\"path\":\"native-marker.txt\",\"content\":\"ok\"}"}]}}),
        ),
        muse_record(
            "assistant-1",
            6,
            "runtime.session",
            json!({"kind":"run","run_id":"run-1","event":{"kind":"assistant_message_committed","message_id":"message-1","text":"hello back"}}),
        ),
        muse_record(
            "terminal-1",
            7,
            "runtime.session",
            json!({"kind":"run","run_id":"run-1","event":{"kind":"terminal","terminal":"completed"}}),
        ),
    ];
    let body = records
        .into_iter()
        .map(|record| serde_json::to_string(&record).unwrap() + "\n")
        .collect::<String>();
    assert_eq!(id, MUSE_ID);
    fs::write(path, body).unwrap();
}
