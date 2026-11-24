// Port of mj-sessionwiki/src/sessionwiki/provenance.rs.
//
// File evidence from stored ACP calls, shared by live and checkpoint
// indexing.

package sessionwiki

import (
	"encoding/json"
	"fmt"
	"log/slog"
	"slices"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/brokksessionwiki"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
)

// provenanceRevision is Rust's REVISION: the tag that marks a Mjolnir row
// whose file evidence has been indexed.
const provenanceRevision = "mj-provenance:1"

// provenanceEvidence is Rust's Evidence.
type provenanceEvidence struct {
	// paths is a set (Rust's BTreeSet<String>); read it in sorted order.
	paths map[string]struct{}
	edits []brokksessionwiki.EditEvent
}

// addPath records one touched path.
func (e *provenanceEvidence) addPath(path string) {
	if e.paths == nil {
		e.paths = map[string]struct{}{}
	}
	e.paths[path] = struct{}{}
}

// sortedPaths is paths in order, as Rust's BTreeSet iterates it.
func (e *provenanceEvidence) sortedPaths() []string {
	paths := make([]string, 0, len(e.paths))
	for path := range e.paths {
		paths = append(paths, path)
	}
	slices.Sort(paths)
	return paths
}

// observeTool records the file evidence of one stored ACP tool call. Only a
// completed call supplies evidence.
func (e *provenanceEvidence) observeTool(call json.RawMessage, presentation *core.ToolCallPresentation, createdAtMs int64) {
	value, _ := jsonvalue.Parse(call)
	if status, ok := jsonMember(value, "status").(string); !ok || status != "completed" {
		return
	}
	ts := chronoTimestampMillis(createdAtMs)
	if presentation != nil {
		for _, edit := range presentation.EditSnippets {
			e.addPath(edit.Path)
			e.edits = append(e.edits, brokksessionwiki.EditEvent{
				Path:    edit.Path,
				Kind:    provenanceEditKind(edit.Kind),
				Snippet: edit.Snippet,
				Ts:      copyTime(ts),
			})
		}
	}
	useRawPatches := presentation == nil || (len(presentation.EditSnippets) == 0 && !presentation.DetailTrimmed)
	contents, _ := jsonMember(value, "content").([]any)
	for _, content := range contents {
		if kind, ok := jsonMember(content, "type").(string); !ok || kind != "diff" {
			continue
		}
		path, ok := provenanceFilePath(content)
		if !ok {
			continue
		}
		e.addPath(path)
		if !useRawPatches {
			continue
		}
		encoded, err := jsonvalue.Marshal(content)
		if err != nil {
			continue
		}
		var decoded schema.ToolCallContent
		if json.Unmarshal(encoded, &decoded) != nil || decoded.Kind != schema.ToolCallContentKindDiff || decoded.Diff == nil {
			continue
		}
		patch := core.PatchOf(decoded.Diff)
		if patch.Text == "" {
			continue
		}
		kind := brokksessionwiki.EditKindEdit
		if patch.Created {
			kind = brokksessionwiki.EditKindWrite
		}
		e.edits = append(e.edits, brokksessionwiki.EditEvent{
			Path:    path,
			Kind:    kind,
			Snippet: firstChars(patch.Text, 2000),
			Ts:      copyTime(ts),
		})
	}
	if kind, ok := jsonMember(value, "kind").(string); ok && (kind == "edit" || kind == "delete" || kind == "move") {
		locations, _ := jsonMember(value, "locations").([]any)
		for _, location := range locations {
			if path, ok := provenanceFilePath(location); ok {
				e.addPath(path)
			}
		}
	}
}

// provenanceEditKind is the SessionWiki edit kind of a presentation's edit
// snippet.
func provenanceEditKind(kind core.ToolEditKind) brokksessionwiki.EditKind {
	switch kind {
	case core.ToolEditKindEdit:
		return brokksessionwiki.EditKindEdit
	case core.ToolEditKindWrite:
		return brokksessionwiki.EditKindWrite
	case core.ToolEditKindMultiEdit:
		return brokksessionwiki.EditKindMultiEdit
	case core.ToolEditKindNotebookEdit:
		return brokksessionwiki.EditKindNotebookEdit
	}
	panic(fmt.Sprintf("unknown tool edit kind %q", string(kind)))
}

// provenanceFilePath is Rust's `file_path`: the non-empty `path` of a JSON
// value (acp-go's jsonvalue representation), with forward slashes.
func provenanceFilePath(value any) (string, bool) {
	path, ok := jsonMember(value, "path").(string)
	if !ok || path == "" {
		return "", false
	}
	return strings.ReplaceAll(path, `\`, "/"), true
}

// provenanceBackfill is Rust's `backfill`: a compatible data repair, not a
// schema migration. Scope it to this instance's keys and only mark a row
// after reading its source and committing evidence.
func provenanceBackfill(conn *store.Connection, adapter *MjolnirAdapter) error {
	scope, _ := adapter.ReconcileScope()
	rows, err := provenanceUnmarked(conn)
	if err != nil {
		return err
	}
	for _, row := range rows {
		path, id := row[0], row[1]
		if !strings.HasPrefix(path, scope) {
			continue
		}
		session, err := adapter.ParseKey(path)
		if err != nil {
			slog.Debug("file provenance remains incomplete: source unavailable", "session_id", id, "error", err)
			continue
		}
		if err := provenanceReplace(conn, id, session); err != nil {
			return err
		}
	}
	return nil
}

// provenanceUnmarked is the path and session id of every Mjolnir row whose
// evidence has not been indexed.
func provenanceUnmarked(conn *store.Connection) ([][2]string, error) {
	statement, err := conn.Prepare(
		`SELECT path, session_id FROM files WHERE tool = 'mjolnir'
         AND NOT EXISTS (SELECT 1 FROM tags WHERE tags.session_id = files.session_id AND tag = ?1)`)
	if err != nil {
		return nil, err
	}
	defer statement.Close()
	rows, err := statement.Query(provenanceRevision)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var found [][2]string
	for rows.Next() {
		var row [2]string
		if err := rows.Scan(&row[0], &row[1]); err != nil {
			return nil, err
		}
		found = append(found, row)
	}
	return found, rows.Err()
}

// provenanceReplace is Rust's `replace`: one session's touched files and
// edits, replaced in one transaction and marked with provenanceRevision.
func provenanceReplace(conn *store.Connection, id string, session *brokksessionwiki.Session) error {
	touched := make([]any, len(session.Touched))
	for index, path := range session.Touched {
		touched[index] = path
	}
	encodedTouched, err := jsonvalue.Marshal(touched)
	if err != nil {
		return err
	}
	transaction, err := conn.Begin()
	if err != nil {
		return err
	}
	defer transaction.Rollback()
	if _, err := transaction.Exec("UPDATE archive SET touched = ?2 WHERE session_id = ?1", id, string(encodedTouched)); err != nil {
		return err
	}
	if _, err := transaction.Exec("DELETE FROM touched WHERE session_id = ?1", id); err != nil {
		return err
	}
	if _, err := transaction.Exec("DELETE FROM edits WHERE session_id = ?1", id); err != nil {
		return err
	}
	for _, path := range session.Touched {
		if _, err := transaction.Exec("INSERT OR IGNORE INTO touched(session_id, path) VALUES (?1, ?2)", id, path); err != nil {
			return err
		}
	}
	for _, edit := range session.Edits {
		var ts any
		if edit.Ts != nil {
			ts = core.FormatChronoRFC3339(*edit.Ts)
		}
		if _, err := transaction.Exec(
			"INSERT INTO edits(session_id, path, kind, ts, snippet) VALUES (?1, ?2, ?3, ?4, ?5)",
			id, edit.Path, edit.Kind.AsStr(), ts, brokksessionwiki.Redact(edit.Snippet),
		); err != nil {
			return err
		}
	}
	if _, err := transaction.Exec("INSERT OR IGNORE INTO tags(session_id, tag) VALUES (?1, ?2)", id, provenanceRevision); err != nil {
		return err
	}
	return transaction.Commit()
}
