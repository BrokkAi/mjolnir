// Port of mj-core/src/history.rs.
//
// Read-only session-history requests shared by worker and controller.

package core

import (
	"encoding/json"
	"time"
)

const QueryTimeout = 60 * time.Second
const MaxPending = 8
const MaxResponseBytes = 1024 * 1024
const MaxBlameBytes = 512 * 1024

func PageDefault() int { return 20 }
func TextDefault() int { return 16_000 }

// HistoryQuery is one history tool call, adjacently tagged as
// {"tool": ..., "arguments": {...}} with unknown fields denied. Decode with
// DecodeHistoryQuery.
type HistoryQuery interface {
	isHistoryQuery()
	// Validate checks the query's arguments.
	Validate() error
}

type HistoryQuerySearchSessions struct {
	Query string `json:"query"`
	Limit int    `json:"limit" serde:"default"`
}

type HistoryQueryGetSessionBrief struct {
	SessionID string `json:"session_id"`
	MaxChars  int    `json:"max_chars" serde:"default"`
}

type HistoryQuerySearchSession struct {
	SessionID string `json:"session_id"`
	Query     string `json:"query"`
	Start     int    `json:"start" serde:"default"`
	Context   int    `json:"context" serde:"default"`
	Limit     int    `json:"limit" serde:"default"`
	MaxChars  int    `json:"max_chars" serde:"default"`
}

type HistoryQueryReadSession struct {
	SessionID string  `json:"session_id"`
	Start     int     `json:"start" serde:"default"`
	Offset    int     `json:"offset" serde:"default"`
	Role      *string `json:"role"`
	Limit     int     `json:"limit" serde:"default"`
	MaxChars  int     `json:"max_chars" serde:"default"`
}

type HistoryQueryTraceFile struct {
	Path  string `json:"path"`
	Limit int    `json:"limit" serde:"default"`
}

type HistoryQuerySessionFiles struct {
	SessionID string `json:"session_id"`
	Start     int    `json:"start" serde:"default"`
	Limit     int    `json:"limit" serde:"default"`
}

type HistoryQueryBlameFile struct {
	Path      string `json:"path"`
	StartLine int    `json:"start_line"`
	EndLine   int    `json:"end_line"`
}

func (HistoryQuerySearchSessions) isHistoryQuery()  {}
func (HistoryQueryGetSessionBrief) isHistoryQuery() {}
func (HistoryQuerySearchSession) isHistoryQuery()   {}
func (HistoryQueryReadSession) isHistoryQuery()     {}
func (HistoryQueryTraceFile) isHistoryQuery()       {}
func (HistoryQuerySessionFiles) isHistoryQuery()    {}
func (HistoryQueryBlameFile) isHistoryQuery()       {}

func (q HistoryQuerySearchSessions) MarshalJSON() ([]byte, error) {
	type plain HistoryQuerySearchSessions
	return transcriptMarshalAdjacent("tool", "search_sessions", "arguments", plain(q))
}

func (q HistoryQueryGetSessionBrief) MarshalJSON() ([]byte, error) {
	type plain HistoryQueryGetSessionBrief
	return transcriptMarshalAdjacent("tool", "get_session_brief", "arguments", plain(q))
}

func (q HistoryQuerySearchSession) MarshalJSON() ([]byte, error) {
	type plain HistoryQuerySearchSession
	return transcriptMarshalAdjacent("tool", "search_session", "arguments", plain(q))
}

func (q HistoryQueryReadSession) MarshalJSON() ([]byte, error) {
	type plain HistoryQueryReadSession
	return transcriptMarshalAdjacent("tool", "read_session", "arguments", plain(q))
}

func (q HistoryQueryTraceFile) MarshalJSON() ([]byte, error) {
	type plain HistoryQueryTraceFile
	return transcriptMarshalAdjacent("tool", "trace_file", "arguments", plain(q))
}

func (q HistoryQuerySessionFiles) MarshalJSON() ([]byte, error) {
	type plain HistoryQuerySessionFiles
	return transcriptMarshalAdjacent("tool", "session_files", "arguments", plain(q))
}

func (q HistoryQueryBlameFile) MarshalJSON() ([]byte, error) {
	type plain HistoryQueryBlameFile
	return transcriptMarshalAdjacent("tool", "blame_file", "arguments", plain(q))
}

// DecodeHistoryQuery reads a HistoryQuery, filling serde's defaults for
// absent arguments.
func DecodeHistoryQuery(data []byte) (HistoryQuery, error) {
	var outer struct {
		Tool      string          `json:"tool"`
		Arguments json.RawMessage `json:"arguments"`
	}
	if err := transcriptStrictDecode(data, &outer, true); err != nil {
		return nil, err
	}
	var err error
	switch outer.Tool {
	case "search_sessions":
		type plain HistoryQuerySearchSessions
		v := plain{Limit: PageDefault()}
		err = transcriptStrictDecode(outer.Arguments, &v, true)
		return HistoryQuerySearchSessions(v), err
	case "get_session_brief":
		type plain HistoryQueryGetSessionBrief
		v := plain{MaxChars: TextDefault()}
		err = transcriptStrictDecode(outer.Arguments, &v, true)
		return HistoryQueryGetSessionBrief(v), err
	case "search_session":
		type plain HistoryQuerySearchSession
		v := plain{Limit: PageDefault(), MaxChars: TextDefault()}
		err = transcriptStrictDecode(outer.Arguments, &v, true)
		return HistoryQuerySearchSession(v), err
	case "read_session":
		type plain HistoryQueryReadSession
		v := plain{Limit: PageDefault(), MaxChars: TextDefault()}
		err = transcriptStrictDecode(outer.Arguments, &v, true)
		return HistoryQueryReadSession(v), err
	case "trace_file":
		type plain HistoryQueryTraceFile
		v := plain{Limit: PageDefault()}
		err = transcriptStrictDecode(outer.Arguments, &v, true)
		return HistoryQueryTraceFile(v), err
	case "session_files":
		type plain HistoryQuerySessionFiles
		v := plain{Limit: PageDefault()}
		err = transcriptStrictDecode(outer.Arguments, &v, true)
		return HistoryQuerySessionFiles(v), err
	case "blame_file":
		type plain HistoryQueryBlameFile
		var v plain
		err = transcriptStrictDecode(outer.Arguments, &v, true)
		return HistoryQueryBlameFile(v), err
	}
	return nil, transcriptUnknownVariant(outer.Tool, "search_sessions", "get_session_brief",
		"search_session", "read_session", "trace_file", "session_files", "blame_file")
}

func (q HistoryQuerySearchSessions) Validate() error {
	panic("unported: mj-core/src/history.rs HistoryQuery::validate")
}

func (q HistoryQueryGetSessionBrief) Validate() error { return nil }

func (q HistoryQuerySearchSession) Validate() error {
	panic("unported: mj-core/src/history.rs HistoryQuery::validate")
}

func (q HistoryQueryReadSession) Validate() error {
	panic("unported: mj-core/src/history.rs HistoryQuery::validate")
}

func (q HistoryQueryTraceFile) Validate() error {
	panic("unported: mj-core/src/history.rs HistoryQuery::validate")
}

func (q HistoryQuerySessionFiles) Validate() error { return nil }

func (q HistoryQueryBlameFile) Validate() error {
	panic("unported: mj-core/src/history.rs HistoryQuery::validate")
}

type BlameEvidence struct {
	Repository   string `json:"repository"`
	RelativePath string `json:"relative_path"`
	Porcelain    string `json:"porcelain"`
}

func (e *BlameEvidence) UnmarshalJSON(data []byte) error {
	type plain BlameEvidence
	return transcriptStrictDecode(data, (*plain)(e), true)
}

type HistoryRequest struct {
	RequestID string         `json:"request_id"`
	Query     HistoryQuery   `json:"query"`
	Blame     *BlameEvidence `json:"blame"`
}

func (r *HistoryRequest) UnmarshalJSON(data []byte) error {
	var v struct {
		RequestID string          `json:"request_id"`
		Query     json.RawMessage `json:"query"`
		Blame     *BlameEvidence  `json:"blame"`
	}
	if err := transcriptStrictDecode(data, &v, true); err != nil {
		return err
	}
	query, err := DecodeHistoryQuery(v.Query)
	if err != nil {
		return err
	}
	*r = HistoryRequest{RequestID: v.RequestID, Query: query, Blame: v.Blame}
	return nil
}

type HistoryResult struct {
	RequestID string          `json:"request_id"`
	Value     json.RawMessage `json:"value"`
	IsError   bool            `json:"is_error"`
}

func (r *HistoryResult) UnmarshalJSON(data []byte) error {
	type plain HistoryResult
	return transcriptStrictDecode(data, (*plain)(r), true)
}

func HistoryResultFailed(requestID string, err error) HistoryResult {
	value, _ := transcriptMarshalMembers([]transcriptMember{{"error", err.Error()}})
	return HistoryResult{RequestID: requestID, Value: value, IsError: true}
}
