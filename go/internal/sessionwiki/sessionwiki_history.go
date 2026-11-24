// Port of mj-sessionwiki/src/sessionwiki/history.rs.
//
// Bounded, read-only history operations for target-side MCP clients.
//
// JSON values are built in acp-go's jsonvalue representation (`any`). The
// module's functions carry the module name, since resolve, query, limit and
// blame would say nothing in this flat package.

package sessionwiki

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/brokksessionwiki"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/mjimport"
	"github.com/BrokkAi/mjolnir/go/internal/store"
)

// historyPermits is Rust's PERMITS, the four-permit semaphore that bounds
// concurrent history queries: a token in the channel is a permit taken.
var historyPermits = make(chan struct{}, 4)

// historyAnswer is one history query's value or error.
type historyAnswer struct {
	value any
	err   error
}

// HistoryExecute is Rust's `history::execute`: answer one history request
// within core.QueryTimeout and core.MaxResponseBytes. ctx is the caller's;
// the query runs as blocking work and its answer is dropped when ctx ends
// first.
func HistoryExecute(ctx context.Context, request core.HistoryRequest) core.HistoryResult {
	id := request.RequestID
	bounded, cancel := context.WithTimeout(ctx, core.QueryTimeout)
	defer cancel()
	value, err := func() (any, error) {
		select {
		case historyPermits <- struct{}{}:
		case <-bounded.Done():
			return nil, fmt.Errorf("history service stopped: %w", bounded.Err())
		}
		handle := core.SpawnBlocking(func() historyAnswer {
			// The permit is held until the query itself returns, even when
			// its caller has stopped waiting.
			defer func() { <-historyPermits }()
			value, err := historyQuery(&request)
			return historyAnswer{value: value, err: err}
		})
		answer, err := handle.Join(bounded)
		if err != nil {
			return nil, fmt.Errorf("history query task failed: %w", err)
		}
		return answer.value, answer.err
	}()
	switch {
	case err == nil:
		encoded, err := jsonvalue.Marshal(value)
		if err == nil && len(encoded) <= core.MaxResponseBytes {
			return core.HistoryResult{RequestID: id, Value: encoded, IsError: false}
		}
		return core.HistoryResultFailed(id, errors.New("history response exceeds its byte budget; narrow the query"))
	case errors.Is(err, context.DeadlineExceeded) && ctx.Err() == nil:
		return core.HistoryResultFailed(id, errors.New("history query timed out; retry or narrow the query"))
	}
	return core.HistoryResultFailed(id, err)
}

// historyResolve is Rust's `resolve`: the one visible row an id or unique id
// prefix names.
func historyResolve(conn store.Queryer, id string, ownership *topLevelSnapshot, cache *mjimport.NativeScanCache) (*brokksessionwiki.SessionRow, error) {
	if id == "" || len(id) > 256 {
		return nil, errors.New("invalid session id")
	}
	rows, err := brokksessionwiki.Resolve(conn, id)
	if err != nil {
		return nil, err
	}
	var visible []brokksessionwiki.SessionRow
	for _, row := range rows {
		if !strings.HasPrefix(row.SessionID, id) {
			continue
		}
		child, err := isChild(&row, ownership, cache)
		if err != nil {
			return nil, err
		}
		if !child {
			visible = append(visible, row)
		}
	}
	for index := range visible {
		if visible[index].SessionID == id {
			return &visible[index], nil
		}
	}
	switch len(visible) {
	case 0:
		if ownership.isKnownTopLevel(id) {
			return nil, fmt.Errorf("session %s exists but is not in the index", core.RustDebugString(id))
		}
		return nil, fmt.Errorf("session %s was not found in the index", core.RustDebugString(id))
	case 1:
		return &visible[0], nil
	}
	return nil, fmt.Errorf("session prefix %s is ambiguous; use an id from search results", core.RustDebugString(id))
}

// historyLimit is Rust's `limit`: a page size within 1..=100.
func historyLimit(value int) int { return min(max(value, 1), 100) }

// historyBudget is Rust's `budget`: a character budget within 1..=64000.
func historyBudget(value int) int { return min(max(value, 1), 64_000) }

// historyQuery is Rust's `query`: one request against the readable index,
// wrapped with the index state and the coverage note.
func historyQuery(request *core.HistoryRequest) (any, error) {
	if err := request.Query.Validate(); err != nil {
		return nil, err
	}
	connection, err := readableIndex()
	if err != nil {
		return nil, err
	}
	if connection == nil {
		return nil, errors.New(unreadableReason())
	}
	defer connection.Close()
	// rusqlite's busy_timeout(Duration::from_secs(2)).
	if _, err := connection.Exec("PRAGMA busy_timeout = 2000"); err != nil {
		return nil, err
	}
	ownership, err := currentSnapshot()
	if err != nil {
		return nil, err
	}
	value, err := historyQueryInWith(connection, request, &ownership, mjimport.NativeScanCacheShared())
	if err != nil {
		return nil, err
	}
	return jsonvalue.Object{
		{Key: "data", Value: value},
		{Key: "index_state", Value: IndexState()},
		{Key: "source", Value: "indexed_snapshot"},
		{Key: "coverage", Value: "Indexed history can lag live sessions. Retained transcripts may omit original timestamps, tool output, or older messages. File evidence discarded before provenance indexing cannot be recovered."},
	}, nil
}

// historyQueryInWith is Rust's `query_in_with`: one request against an open
// index.
func historyQueryInWith(conn store.Queryer, request *core.HistoryRequest, ownership *topLevelSnapshot, cache *mjimport.NativeScanCache) (any, error) {
	switch query := request.Query.(type) {
	case core.HistoryQuerySearchSessions:
		rows, err := queryRowsFrom(conn, query.Query, historyLimit(query.Limit), map[string]struct{}{}, true, ownership, cache)
		if err != nil {
			return nil, err
		}
		if rows == nil {
			rows = []client.WikiRow{}
		}
		return jsonvalue.Object{
			{Key: "sessions", Value: rows},
			{Key: "limit", Value: historyLimit(query.Limit)},
		}, nil
	case core.HistoryQueryTraceFile:
		candidates, err := brokksessionwiki.SessionsForFile(conn, strings.ReplaceAll(query.Path, `\`, "/"), MaxWikiLimit)
		if err != nil {
			return nil, err
		}
		rows := []any{}
		for _, candidate := range candidates {
			if len(rows) >= historyLimit(query.Limit) {
				break
			}
			child, err := isChild(&candidate.Row, ownership, cache)
			if err != nil {
				return nil, err
			}
			if !child {
				rows = append(rows, jsonvalue.Object{
					{Key: "session", Value: candidate.Row},
					{Key: "matched_path", Value: candidate.MatchedPath},
				})
			}
		}
		return jsonvalue.Object{{Key: "sessions", Value: rows}}, nil
	case core.HistoryQuerySessionFiles:
		row, err := historyResolve(conn, query.SessionID, ownership, cache)
		if err != nil {
			return nil, err
		}
		files, err := brokksessionwiki.FilesFor(conn, row.SessionID)
		if err != nil {
			return nil, err
		}
		start := max(query.Start, 0)
		end := min(saturatingAddInt(start, historyLimit(query.Limit)), len(files))
		page := []any{}
		if start <= end {
			for _, file := range files[start:end] {
				page = append(page, file)
			}
		}
		var nextStart any
		if end < len(files) {
			nextStart = end
		}
		provenanceIndexed := row.Tool != "mjolnir"
		if !provenanceIndexed && row.Tags != nil {
			for _, tag := range strings.Split(*row.Tags, ",") {
				if tag == provenanceRevision {
					provenanceIndexed = true
					break
				}
			}
		}
		return jsonvalue.Object{
			{Key: "session_id", Value: row.SessionID},
			{Key: "files", Value: page},
			{Key: "next_start", Value: nextStart},
			{Key: "total", Value: len(files)},
			{Key: "provenance_indexed", Value: provenanceIndexed},
		}, nil
	case core.HistoryQueryBlameFile:
		if request.Blame == nil {
			return nil, errors.New("target did not supply Git blame evidence")
		}
		return historyBlame(conn, request.Blame, ownership, cache)
	case core.HistoryQueryGetSessionBrief:
		row, err := historyResolve(conn, query.SessionID, ownership, cache)
		if err != nil {
			return nil, err
		}
		session, err := brokksessionwiki.SessionFromIndex(conn, row)
		if err != nil {
			return nil, err
		}
		budget := historyBudget(query.MaxChars)
		text := brokksessionwiki.BriefMarkdown(session, budget, true)
		return jsonvalue.Object{
			{Key: "session_id", Value: row.SessionID},
			{Key: "text", Value: firstChars(text, budget)},
			{Key: "truncated", Value: utf8.RuneCountInString(text) > budget},
		}, nil
	case core.HistoryQueryReadSession:
		row, err := historyResolve(conn, query.SessionID, ownership, cache)
		if err != nil {
			return nil, err
		}
		session, err := brokksessionwiki.SessionFromIndex(conn, row)
		if err != nil {
			return nil, err
		}
		return historyReadPage(session, query.Start, query.Offset, query.Role, historyLimit(query.Limit), historyBudget(query.MaxChars)), nil
	case core.HistoryQuerySearchSession:
		row, err := historyResolve(conn, query.SessionID, ownership, cache)
		if err != nil {
			return nil, err
		}
		session, err := brokksessionwiki.SessionFromIndex(conn, row)
		if err != nil {
			return nil, err
		}
		opts := defaultGrepOpts()
		opts.contextMessages = min(query.Context, 5)
		opts.chars = 2000
		found := grepSession(session, query.Query, &opts)
		remaining := historyBudget(query.MaxChars)
		limit := historyLimit(query.Limit)
		hits := []any{}
		var next any
		for _, hit := range found.hits {
			if hit.i < query.Start {
				continue
			}
			if len(hits) == limit || remaining == 0 {
				next = hit.i
				break
			}
			text := firstChars(hit.text, remaining)
			remaining = max(remaining-utf8.RuneCountInString(text), 0)
			matches := []any{}
			for _, match := range hit.matches {
				if match[1] <= len(text) {
					matches = append(matches, []any{match[0], match[1]})
				}
			}
			hits = append(hits, jsonvalue.Object{
				{Key: "i", Value: hit.i},
				{Key: "role", Value: string(hit.role)},
				{Key: "ts", Value: chronoJSONTime(hit.ts)},
				{Key: "text", Value: text},
				{Key: "matches", Value: matches},
				{Key: "truncated", Value: hit.truncated || len(text) < len(hit.text)},
				{Key: "omitted_before", Value: hit.omittedBefore},
			})
		}
		return jsonvalue.Object{
			{Key: "session_id", Value: row.SessionID},
			{Key: "hits", Value: hits},
			{Key: "next_start", Value: next},
			{Key: "total_messages", Value: len(session.Messages)},
		}, nil
	}
	panic(fmt.Sprintf("unknown history query %T", request.Query))
}

// historyReadPage is Rust's `read_page`: one page of a session's messages
// within count messages and chars characters. role nil reads every role.
func historyReadPage(session *brokksessionwiki.Session, start int, offset int, role *string, count int, chars int) any {
	remaining := chars
	messages := []any{}
	var next any
	for i := max(start, 0); i < len(session.Messages); i++ {
		message := &session.Messages[i]
		if role != nil && *role != roleName(message.Role) {
			continue
		}
		if remaining == 0 || len(messages) == count {
			next = jsonvalue.Object{{Key: "start", Value: i}, {Key: "offset", Value: 0}}
			break
		}
		text := []rune(brokksessionwiki.Redact(message.Text))
		skip := 0
		if i == start {
			skip = max(offset, 0)
		}
		available := max(len(text)-skip, 0)
		part := text[min(skip, len(text)):]
		part = part[:min(remaining, len(part))]
		taken := len(part)
		remaining -= taken
		truncated := taken < available
		messages = append(messages, jsonvalue.Object{
			{Key: "i", Value: i},
			{Key: "role", Value: string(message.Role)},
			{Key: "ts", Value: chronoJSONTime(message.Ts)},
			{Key: "text", Value: string(part)},
			{Key: "offset", Value: skip},
			{Key: "truncated", Value: truncated},
		})
		if truncated {
			next = jsonvalue.Object{{Key: "start", Value: i}, {Key: "offset", Value: skip + taken}}
			break
		}
	}
	return jsonvalue.Object{
		{Key: "session_id", Value: session.ID},
		{Key: "messages", Value: messages},
		{Key: "next", Value: next},
		{Key: "total_messages", Value: len(session.Messages)},
	}
}

// historyBlame is Rust's `blame`: the sessions Git blame evidence attributes
// each run of lines to.
func historyBlame(conn store.Queryer, evidence *core.BlameEvidence, ownership *topLevelSnapshot, cache *mjimport.NativeScanCache) (any, error) {
	if len(evidence.Porcelain) > core.MaxBlameBytes {
		return nil, errors.New("Git blame evidence exceeds its byte budget")
	}
	query := strings.ReplaceAll(evidence.RelativePath, `\`, "/")
	// SessionsTouching uses SQL LIKE; intersect with the literal-path matcher
	// so '%' and '_' in a real file name cannot introduce unrelated
	// candidates.
	ids := map[string]struct{}{}
	matches, err := brokksessionwiki.SessionsForFile(conn, query, 10_000)
	if err != nil {
		return nil, err
	}
	for _, match := range matches {
		child, err := isChild(&match.Row, ownership, cache)
		if err != nil {
			return nil, err
		}
		if !child {
			ids[match.Row.SessionID] = struct{}{}
		}
	}
	touching, err := brokksessionwiki.SessionsTouching(conn, query)
	if err != nil {
		return nil, err
	}
	var candidates []brokksessionwiki.TouchingSession
	for _, candidate := range touching {
		if _, ok := ids[candidate.SessionID]; ok {
			candidates = append(candidates, candidate)
		}
	}
	candidate := func(session brokksessionwiki.TouchingSession) any {
		return jsonvalue.Object{
			{Key: "session_id", Value: session.SessionID},
			{Key: "tool", Value: session.Tool},
			{Key: "title", Value: session.Title},
			{Key: "project", Value: session.Project},
			{Key: "archived", Value: session.Archived},
		}
	}
	runs := []any{}
	for _, run := range brokksessionwiki.GroupRuns(brokksessionwiki.ParseLinePorcelain(evidence.Porcelain)) {
		var attribution brokksessionwiki.Attribution = brokksessionwiki.AttributionUnattributed{}
		if strings.Trim(run.Commit, "0") != "" {
			attribution = brokksessionwiki.AttributeCommit(run.AuthorTime, evidence.Repository, candidates)
		}
		var status string
		sessions := []any{}
		switch attributed := attribution.(type) {
		case brokksessionwiki.AttributionConfident:
			status = "confident"
			sessions = append(sessions, candidate(attributed.Session))
		case brokksessionwiki.AttributionAmbiguous:
			status = "ambiguous"
			for _, session := range attributed.Sessions {
				sessions = append(sessions, candidate(session))
			}
		case brokksessionwiki.AttributionUnattributed:
			status = "unattributed"
		default:
			panic(fmt.Sprintf("unknown attribution %T", attribution))
		}
		runs = append(runs, jsonvalue.Object{
			{Key: "start", Value: run.Start},
			{Key: "end", Value: run.End},
			{Key: "commit", Value: run.Commit},
			{Key: "status", Value: status},
			{Key: "sessions", Value: sessions},
		})
	}
	return jsonvalue.Object{
		{Key: "path", Value: evidence.RelativePath},
		{Key: "runs", Value: runs},
		{Key: "heuristic", Value: true},
		{Key: "note", Value: "Attribution uses recorded file edits and commit timing; verify against the conversation and Git history."},
	}, nil
}
