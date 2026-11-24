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

	"github.com/BrokkAi/mjolnir/go/internal/brokksessionwiki"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/mjimport"
	"github.com/BrokkAi/mjolnir/go/internal/store"
)

// historyPermits is Rust's PERMITS, the four-permit semaphore that bounds
// concurrent history queries: a token in the channel is a permit taken.
var historyPermits = make(chan struct{}, 4)

// HistoryExecute is Rust's `history::execute`: answer one history request
// within core.QueryTimeout and core.MaxResponseBytes. ctx is the caller's;
// the query runs as blocking work and its answer is dropped when ctx ends
// first.
func HistoryExecute(ctx context.Context, request core.HistoryRequest) core.HistoryResult {
	panic("unported: mj-sessionwiki/src/sessionwiki/history.rs execute")
}

// historyResolve is Rust's `resolve`: the one visible row an id or unique id
// prefix names.
func historyResolve(conn store.Queryer, id string, ownership *topLevelSnapshot, cache *mjimport.NativeScanCache) (*brokksessionwiki.SessionRow, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki/history.rs resolve")
}

// historyLimit is Rust's `limit`: a page size within 1..=100.
func historyLimit(value int) int { return min(max(value, 1), 100) }

// historyBudget is Rust's `budget`: a character budget within 1..=64000.
func historyBudget(value int) int { return min(max(value, 1), 64_000) }

// historyQuery is Rust's `query`: one request against the readable index,
// wrapped with the index state and the coverage note.
func historyQuery(request *core.HistoryRequest) (any, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki/history.rs query")
}

// historyQueryInWith is Rust's `query_in_with`: one request against an open
// index.
func historyQueryInWith(conn store.Queryer, request *core.HistoryRequest, ownership *topLevelSnapshot, cache *mjimport.NativeScanCache) (any, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki/history.rs query_in_with")
}

// historyReadPage is Rust's `read_page`: one page of a session's messages
// within count messages and chars characters. role nil reads every role.
func historyReadPage(session *brokksessionwiki.Session, start int, offset int, role *string, count int, chars int) any {
	panic("unported: mj-sessionwiki/src/sessionwiki/history.rs read_page")
}

// historyBlame is Rust's `blame`: the sessions Git blame evidence attributes
// each run of lines to.
func historyBlame(conn store.Queryer, evidence *core.BlameEvidence, ownership *topLevelSnapshot, cache *mjimport.NativeScanCache) (any, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki/history.rs blame")
}
