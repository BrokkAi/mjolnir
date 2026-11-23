// Port of mj-store/src/database/native_agents.rs.
//
// Native children are projections of their owner's relay, not worker
// sessions.

package store

import (
	"database/sql"
	"encoding/json"
	"errors"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// LoadNativeAgentView reads identity and tail from one snapshot so replay
// cannot mix generations. It is nil when the owner has no such child.
func LoadNativeAgentView(owner string, child string, limit int) (*core.NativeAgentView, error) {
	panic("unported: mj-store/src/database/native_agents.rs load_native_agent_view")
}

func loadNativeAgentViewFrom(connection Queryer, owner string, child string, limit int) (*core.NativeAgentView, error) {
	panic("unported: mj-store/src/database/native_agents.rs load_native_agent_view_from")
}

func LoadNativeAgents(owner string, limit int) ([]core.NativeAgentView, error) {
	panic("unported: mj-store/src/database/native_agents.rs load_native_agents")
}

func loadNativeAgentsFrom(connection Queryer, owner string, limit int) ([]core.NativeAgentView, error) {
	return loadNativeAgentsAt(connection, owner, limit, 0)
}

func loadNativeAgentsAt(connection Queryer, owner string, limit int, staging int64) ([]core.NativeAgentView, error) {
	panic("unported: mj-store/src/database/native_agents.rs load_native_agents_at")
}

// nativeAgentTranscript is Rust's transcript (renamed: a package-level
// transcript would collide with the transcript package other files import).
// It is the newest limit items of a child's transcript, oldest first.
func nativeAgentTranscript(connection Queryer, owner string, child string, staging int64, limit int) ([]*core.TranscriptItem, error) {
	panic("unported: mj-store/src/database/native_agents.rs transcript")
}

// seedAdoptedNativeAgents: the worker owns native identities even when their
// spawn precedes retained history. Replayed spawns are idempotent against
// this base, using the ordinary writer.
func seedAdoptedNativeAgents(connection Queryer, owner string, agents []core.NativeAgent, cursor *core.RelayCursor, staging int64) error {
	panic("unported: mj-store/src/database/native_agents.rs seed_adopted_native_agents")
}

func applyNativeAgentEvent(connection Queryer, owner string, relay *core.RelayEvent) error {
	panic("unported: mj-store/src/database/native_agents.rs apply_native_agent_event")
}

func finishStreaming(connection Queryer, owner string, child string, staging int64) error {
	_, err := connection.Exec("UPDATE native_agent_transcript SET body=json_set(body,'$.body.streaming',json('false')) WHERE owner=?1 AND child=?2 AND staging=?3 AND json_extract(body,'$.body.kind') IN ('agent','thought')",
		owner, child, staging)
	return err
}

func save(connection Queryer, view *core.NativeAgentView, staging int64) error {
	body, err := json.Marshal(view)
	if err != nil {
		return err
	}
	_, err = connection.Exec("INSERT INTO native_agents(owner,child,staging,body) VALUES (?1,?2,?3,?4) ON CONFLICT(owner,child,staging) DO UPDATE SET body=excluded.body",
		view.Agent.OwnerSessionID, view.Agent.SessionID, staging, string(body))
	return err
}

// nativeAgentBody is the stored view of a committed child; false when there
// is none.
func nativeAgentBody(connection Queryer, owner string, child string) (string, bool, error) {
	var body string
	err := connection.QueryRow("SELECT body FROM native_agents WHERE owner=?1 AND child=?2 AND staging=0",
		owner, child).Scan(&body)
	if errors.Is(err, sql.ErrNoRows) {
		return "", false, nil
	}
	if err != nil {
		return "", false, err
	}
	return body, true, nil
}

// LoadNativeAgentToolCallDetail is a child's tool call detail; nil when the
// child or the item is unknown.
func LoadNativeAgentToolCallDetail(owner string, child string, stableID string) (core.ToolCallDetailResult, error) {
	panic("unported: mj-store/src/database/native_agents.rs load_native_agent_tool_call_detail")
}

func loadNativeAgentToolCallDetailFrom(connection Queryer, owner string, child string, stableID string) (core.ToolCallDetailResult, error) {
	panic("unported: mj-store/src/database/native_agents.rs load_native_agent_tool_call_detail_from")
}

// NativeAgentHistory pages a child's transcript backwards. before is Rust's
// `Option<(u64, String)>`, the exclusive (position, stable id) cursor.
func NativeAgentHistory(owner string, child string, before *core.TranscriptCursor) (core.NativeAgentHistoryPage, error) {
	panic("unported: mj-store/src/database/native_agents.rs native_agent_history")
}

func nativeAgentHistoryFrom(connection Queryer, owner string, child string, before *core.TranscriptCursor) (core.NativeAgentHistoryPage, error) {
	panic("unported: mj-store/src/database/native_agents.rs native_agent_history_from")
}
