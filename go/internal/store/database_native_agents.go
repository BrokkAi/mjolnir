// Port of mj-store/src/database/native_agents.rs.
//
// Native children are projections of their owner's relay, not worker
// sessions.

package store

import (
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"math"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// LoadNativeAgentView reads identity and tail from one snapshot so replay
// cannot mix generations. It is nil when the owner has no such child.
func LoadNativeAgentView(owner string, child string, limit int) (*core.NativeAgentView, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	transaction, err := connection.Begin()
	if err != nil {
		return nil, err
	}
	defer transaction.Rollback()
	return loadNativeAgentViewFrom(transaction, owner, child, limit)
}

func loadNativeAgentViewFrom(connection Queryer, owner string, child string, limit int) (*core.NativeAgentView, error) {
	body, ok, err := nativeAgentBody(connection, owner, child)
	if err != nil || !ok {
		return nil, err
	}
	var view core.NativeAgentView
	if err := json.Unmarshal([]byte(body), &view); err != nil {
		return nil, err
	}
	view.Projection.Transcript, err = nativeAgentTranscript(connection, owner, child, 0, limit)
	if err != nil {
		return nil, err
	}
	return &view, nil
}

func LoadNativeAgents(owner string, limit int) ([]core.NativeAgentView, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	transaction, err := connection.Begin()
	if err != nil {
		return nil, err
	}
	defer transaction.Rollback()
	return loadNativeAgentsFrom(transaction, owner, limit)
}

func loadNativeAgentsFrom(connection Queryer, owner string, limit int) ([]core.NativeAgentView, error) {
	return loadNativeAgentsAt(connection, owner, limit, 0)
}

func loadNativeAgentsAt(connection Queryer, owner string, limit int, staging int64) ([]core.NativeAgentView, error) {
	bodies, err := queryTextColumn(connection,
		"SELECT body FROM native_agents WHERE owner=?1 AND staging=?2 ORDER BY child", owner, staging)
	if err != nil {
		return nil, err
	}
	views := make([]core.NativeAgentView, 0, len(bodies))
	for _, body := range bodies {
		var view core.NativeAgentView
		if err := json.Unmarshal([]byte(body), &view); err != nil {
			return nil, err
		}
		view.Projection.Transcript, err = nativeAgentTranscript(connection, owner, view.Agent.SessionID, staging, limit)
		if err != nil {
			return nil, err
		}
		views = append(views, view)
	}
	return views, nil
}

// nativeAgentTranscript is Rust's transcript (renamed: a package-level
// transcript would collide with the transcript package other files import).
// It is the newest limit items of a child's transcript, oldest first.
func nativeAgentTranscript(connection Queryer, owner string, child string, staging int64, limit int) ([]*core.TranscriptItem, error) {
	bodies, err := queryTextColumn(connection,
		"SELECT body FROM (SELECT position, stable_id, body FROM native_agent_transcript WHERE owner=?1 AND child=?2 AND staging=?3 ORDER BY position DESC, stable_id DESC LIMIT ?4) ORDER BY position, stable_id",
		owner, child, staging, int64(limit))
	if err != nil {
		return nil, err
	}
	return decodeStoredTranscriptItems(bodies)
}

// decodeStoredTranscriptItems decodes stored transcript item JSON, in order.
func decodeStoredTranscriptItems(bodies []string) ([]*core.TranscriptItem, error) {
	items := make([]*core.TranscriptItem, 0, len(bodies))
	for _, body := range bodies {
		item := new(core.TranscriptItem)
		if err := json.Unmarshal([]byte(body), item); err != nil {
			return nil, err
		}
		items = append(items, item)
	}
	return items, nil
}

// queryTextColumn collects the single text column of every row of query.
func queryTextColumn(connection Queryer, query string, args ...any) ([]string, error) {
	rows, err := connection.Query(query, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var values []string
	for rows.Next() {
		var value string
		if err := rows.Scan(&value); err != nil {
			return nil, err
		}
		values = append(values, value)
	}
	return values, rows.Err()
}

// seedAdoptedNativeAgents: the worker owns native identities even when their
// spawn precedes retained history. Replayed spawns are idempotent against
// this base, using the ordinary writer.
func seedAdoptedNativeAgents(connection Queryer, owner string, agents []core.NativeAgent, cursor *core.RelayCursor, staging int64) error {
	for _, agent := range agents {
		if agent.OwnerSessionID != owner {
			return errors.New("adopted native child belongs to another owner")
		}
		projection := core.MaterializedSessionEmpty(agent.ViewID())
		projection.AppliedEventOrdinal = cursor.Ordinal
		projection.AppliedEventDigest = cursor.Digest
		name := agent.Name
		projection.SessionTitle = &name
		if agent.State == core.NativeAgentStateRunning {
			projection.Execution = core.MaterializedExecutionStateRunning{StartedAtMs: core.EpochMillis()}
		} else {
			projection.Execution = core.MaterializedExecutionStateIdle{}
		}
		if err := save(connection, &core.NativeAgentView{
			GenerationOrdinal: cursor.Ordinal,
			Agent:             agent,
			Projection:        projection,
		}, staging); err != nil {
			return err
		}
	}
	return nil
}

// nativeAgentReplayStaging is whether owner has an open native replay, as
// the staging generation (1) or the committed one (0).
func nativeAgentReplayStaging(connection Queryer, owner string) (int64, error) {
	var staging int64
	err := connection.QueryRow("SELECT EXISTS(SELECT 1 FROM native_agent_replay WHERE owner=?1)", owner).Scan(&staging)
	return staging, err
}

func applyNativeAgentEvent(connection Queryer, owner string, relay *core.RelayEvent) error {
	observation, ok := relay.Observation.(core.RelayObservationNativeAgent)
	if !ok {
		return errors.New("expected native agent observation")
	}
	event := observation.Event
	switch typed := event.(type) {
	case core.NativeAgentEventAvailability:
		views, err := loadNativeAgentsFrom(connection, owner, 0)
		if err != nil {
			return err
		}
		for i := range views {
			views[i].Agent.ApplyAvailability(typed.Reports, typed.Complete)
			if err := save(connection, &views[i], 0); err != nil {
				return err
			}
		}
		return nil
	case core.NativeAgentEventReplayBegin:
		views, err := loadNativeAgentsFrom(connection, owner, 0)
		if err != nil {
			return err
		}
		for i := range views {
			views[i].Agent.InvalidateAvailability()
			if views[i].Agent.State == core.NativeAgentStateRunning {
				views[i].Agent.State = core.NativeAgentStateDisconnected
			}
			if err := save(connection, &views[i], 0); err != nil {
				return err
			}
		}
		if _, err := connection.Exec("DELETE FROM native_agents WHERE owner=?1 AND staging=1", owner); err != nil {
			return err
		}
		_, err = connection.Exec("INSERT OR IGNORE INTO native_agent_replay(owner) VALUES (?1)", owner)
		return err
	case core.NativeAgentEventReplayCommit:
		staging, err := nativeAgentReplayStaging(connection, owner)
		if err != nil {
			return err
		}
		if staging != 0 {
			views, err := loadNativeAgentsAt(connection, owner, 0, 1)
			if err != nil {
				return err
			}
			for i := range views {
				views[i].Agent.FinishReplay()
				views[i].Projection.Execution = core.MaterializedExecutionStateIdle{}
				if err := finishStreaming(connection, owner, views[i].Agent.SessionID, 1); err != nil {
					return err
				}
				if err := save(connection, &views[i], 1); err != nil {
					return err
				}
			}
			if _, err := connection.Exec("DELETE FROM native_agents WHERE owner=?1 AND staging=0 AND child IN (SELECT child FROM native_agents WHERE owner=?1 AND staging=1)", owner); err != nil {
				return err
			}
			if _, err := connection.Exec("UPDATE native_agents SET staging=0 WHERE owner=?1 AND staging=1", owner); err != nil {
				return err
			}
			if _, err := connection.Exec("DELETE FROM native_agent_replay WHERE owner=?1", owner); err != nil {
				return err
			}
		}
		return nil
	case core.NativeAgentEventDisconnected:
		if _, err := connection.Exec("DELETE FROM native_agents WHERE owner=?1 AND staging=1", owner); err != nil {
			return err
		}
		if _, err := connection.Exec("DELETE FROM native_agent_replay WHERE owner=?1", owner); err != nil {
			return err
		}
		views, err := loadNativeAgentsFrom(connection, owner, 0)
		if err != nil {
			return err
		}
		for i := range views {
			view := &views[i]
			view.Agent.InvalidateAvailability()
			if view.Agent.State == core.NativeAgentStateRunning {
				view.Agent.State = core.NativeAgentStateDisconnected
			}
			view.Projection.Execution = core.MaterializedExecutionStateIdle{}
			if err := finishStreaming(connection, owner, view.Agent.SessionID, 0); err != nil {
				return err
			}
			view.Projection.AppliedEventOrdinal = relay.Ordinal
			if err := save(connection, view, 0); err != nil {
				return err
			}
		}
		return nil
	}
	staging, err := nativeAgentReplayStaging(connection, owner)
	if err != nil {
		return err
	}
	var child string
	switch typed := event.(type) {
	case core.NativeAgentEventSpawned:
		child = typed.SessionID
	case core.NativeAgentEventState:
		child = typed.SessionID
	case core.NativeAgentEventUpdate:
		child = typed.SessionID
	default:
		panic("unreachable native agent event")
	}
	var stored *string
	err = connection.QueryRow("SELECT body FROM native_agents WHERE owner=?1 AND child=?2 AND staging=?3",
		owner, child, staging).Scan(&stored)
	if err != nil && !errors.Is(err, sql.ErrNoRows) {
		return err
	}
	var view core.NativeAgentView
	if stored != nil {
		if err := json.Unmarshal([]byte(*stored), &view); err != nil {
			return err
		}
	} else {
		spawned, ok := event.(core.NativeAgentEventSpawned)
		if !ok {
			return fmt.Errorf("native child %s update precedes spawn for owner %s", child, owner)
		}
		agent := core.NativeAgent{
			OwnerSessionID:  owner,
			SessionID:       spawned.SessionID,
			ParentSessionID: spawned.ParentSessionID,
			Name:            spawned.Name,
			Task:            spawned.Task,
			Capabilities:    spawned.Capabilities,
			State:           core.NativeAgentStateRunning,
		}
		projection := core.MaterializedSessionEmpty(agent.ViewID())
		name := spawned.Name
		projection.SessionTitle = &name
		projection.Execution = core.MaterializedExecutionStateRunning{StartedAtMs: relay.RecordedAtMs}
		view = core.NativeAgentView{
			GenerationOrdinal: relay.Ordinal,
			Agent:             agent,
			Projection:        projection,
		}
	}
	var pendingElicitations *[]core.ElicitationRequest
	switch typed := event.(type) {
	case core.NativeAgentEventState:
		view.Agent.State = typed.State
		if typed.State != core.NativeAgentStateRunning {
			if err := finishStreaming(connection, owner, child, staging); err != nil {
				return err
			}
		}
		if typed.State == core.NativeAgentStateRunning {
			view.Projection.Execution = core.MaterializedExecutionStateRunning{StartedAtMs: relay.RecordedAtMs}
		} else {
			view.Projection.Execution = core.MaterializedExecutionStateIdle{}
		}
	case core.NativeAgentEventUpdate:
		// Streaming text needs the tail; an old tool update additionally needs
		// its named row, even when many newer messages have followed it.
		view.Projection.Transcript, err = nativeAgentTranscript(connection, owner, child, staging, 64)
		if err != nil {
			return err
		}
		var toolID *string
		if call := typed.Update.ToolCall; call != nil {
			id := string(call.ToolCallID)
			toolID = &id
		} else if call := typed.Update.ToolCallUpdate; call != nil {
			id := string(call.ToolCallID)
			toolID = &id
		}
		if toolID != nil {
			stableID := "tool:" + *toolID
			present := false
			for _, item := range view.Projection.Transcript {
				if item.StableID == stableID {
					present = true
					break
				}
			}
			if !present {
				var body *string
				err := connection.QueryRow("SELECT body FROM native_agent_transcript WHERE owner=?1 AND child=?2 AND staging=?3 AND stable_id=?4",
					owner, child, staging, stableID).Scan(&body)
				if err != nil && !errors.Is(err, sql.ErrNoRows) {
					return err
				}
				if body != nil {
					item := new(core.TranscriptItem)
					if err := json.Unmarshal([]byte(*body), item); err != nil {
						return err
					}
					view.Projection.Transcript = append(view.Projection.Transcript, item)
				}
			}
		}
		update := typed.Update
		mutation, err := transcript.ProjectNativeUpdate(&view.Projection, relay, &update)
		if err != nil {
			return err
		}
		pendingElicitations = mutation.PendingElicitations
		for _, change := range mutation.Transcript {
			switch change := change.(type) {
			case core.TranscriptMutationUpsert:
				item := change.Item
				body, err := databaseMarshalJSON(&item)
				if err != nil {
					return err
				}
				if _, err := connection.Exec("INSERT INTO native_agent_transcript(owner,child,staging,stable_id,position,body) VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(owner,child,staging,stable_id) DO UPDATE SET body=excluded.body",
					owner, child, staging, item.StableID, item.Position, string(body)); err != nil {
					return err
				}
			case core.TranscriptMutationRemove:
				if _, err := connection.Exec("DELETE FROM native_agent_transcript WHERE owner=?1 AND child=?2 AND staging=?3 AND stable_id=?4",
					owner, child, staging, change.StableID); err != nil {
					return err
				}
			}
		}
		view.Projection.Transcript = nil
	}
	view.Projection.AppliedEventOrdinal = relay.Ordinal
	view.Projection.AppliedEventDigest = relay.Digest
	view.ApplyActivity(relay.RecordedAtMs, pendingElicitations)
	return save(connection, &view, staging)
}

func finishStreaming(connection Queryer, owner string, child string, staging int64) error {
	_, err := connection.Exec("UPDATE native_agent_transcript SET body=json_set(body,'$.body.streaming',json('false')) WHERE owner=?1 AND child=?2 AND staging=?3 AND json_extract(body,'$.body.kind') IN ('agent','thought')",
		owner, child, staging)
	return err
}

func save(connection Queryer, view *core.NativeAgentView, staging int64) error {
	body, err := databaseMarshalJSON(view)
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
	reader, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer reader.Close()
	connection, err := reader.Begin()
	if err != nil {
		return nil, err
	}
	defer connection.Rollback()
	return loadNativeAgentToolCallDetailFrom(connection, owner, child, stableID)
}

func loadNativeAgentToolCallDetailFrom(connection Queryer, owner string, child string, stableID string) (core.ToolCallDetailResult, error) {
	if _, ok, err := nativeAgentBody(connection, owner, child); err != nil || !ok {
		return nil, err
	}
	var item string
	err := connection.QueryRow(`SELECT body FROM native_agent_transcript
         WHERE owner=?1 AND child=?2 AND staging=0 AND stable_id=?3`,
		owner, child, stableID).Scan(&item)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	var decoded core.TranscriptItem
	if err := json.Unmarshal([]byte(item), &decoded); err != nil {
		return nil, err
	}
	if core.TranscriptBodyDetailTrimmed(decoded.Body) {
		return core.ToolCallDetailResultUnavailable{
			Summary: decoded.Body,
			Reason:  "full tool detail is not stored locally",
		}, nil
	}
	return core.ToolCallDetailResultFull{Body: decoded.Body}, nil
}

// NativeAgentHistory pages a child's transcript backwards. before is Rust's
// `Option<(u64, String)>`, the exclusive (position, stable id) cursor.
func NativeAgentHistory(owner string, child string, before *core.TranscriptCursor) (core.NativeAgentHistoryPage, error) {
	reader, err := openReader(DatabasePath())
	if err != nil {
		return core.NativeAgentHistoryPage{}, err
	}
	defer reader.Close()
	connection, err := reader.Begin()
	if err != nil {
		return core.NativeAgentHistoryPage{}, err
	}
	defer connection.Rollback()
	return nativeAgentHistoryFrom(connection, owner, child, before)
}

func nativeAgentHistoryFrom(connection Queryer, owner string, child string, before *core.TranscriptCursor) (core.NativeAgentHistoryPage, error) {
	body, ok, err := nativeAgentBody(connection, owner, child)
	if err != nil {
		return core.NativeAgentHistoryPage{}, err
	}
	if !ok {
		return core.NativeAgentHistoryPage{}, sql.ErrNoRows
	}
	var view core.NativeAgentView
	if err := json.Unmarshal([]byte(body), &view); err != nil {
		return core.NativeAgentHistoryPage{}, err
	}
	position, stableID := uint64(math.MaxInt64), ""
	if before != nil {
		position, stableID = before.Position, before.StableID
	}
	bodies, err := queryTextColumn(connection,
		"SELECT body FROM native_agent_transcript WHERE owner=?1 AND child=?2 AND staging=0 AND (position, stable_id) < (?3, ?4) ORDER BY position DESC, stable_id DESC LIMIT 201",
		owner, child, position, stableID)
	if err != nil {
		return core.NativeAgentHistoryPage{}, err
	}
	hasMore := len(bodies) > 200
	if hasMore {
		bodies = bodies[:200]
	}
	items, err := decodeStoredTranscriptItems(bodies)
	if err != nil {
		return core.NativeAgentHistoryPage{}, err
	}
	for i, j := 0, len(items)-1; i < j; i, j = i+1, j-1 {
		items[i], items[j] = items[j], items[i]
	}
	return core.NativeAgentHistoryPage{
		GenerationOrdinal: view.GenerationOrdinal,
		Items:             items,
		HasMore:           hasMore,
	}, nil
}
