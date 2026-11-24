// Port of mj-sessionwiki/src/sessionwiki/provenance.rs.
//
// File evidence from stored ACP calls, shared by live and checkpoint
// indexing.

package sessionwiki

import (
	"encoding/json"

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

// observeTool records the file evidence of one stored ACP tool call. Only a
// completed call supplies evidence.
func (e *provenanceEvidence) observeTool(call json.RawMessage, presentation *core.ToolCallPresentation, createdAtMs int64) {
	panic("unported: mj-sessionwiki/src/sessionwiki/provenance.rs Evidence::observe_tool")
}

// provenanceFilePath is Rust's `file_path`: the non-empty `path` of a JSON
// value (acp-go's jsonvalue representation), with forward slashes.
func provenanceFilePath(value any) (string, bool) {
	panic("unported: mj-sessionwiki/src/sessionwiki/provenance.rs file_path")
}

// provenanceBackfill is Rust's `backfill`: a compatible data repair, not a
// schema migration. Scope it to this instance's keys and only mark a row
// after reading its source and committing evidence.
func provenanceBackfill(conn *store.Connection, adapter *MjolnirAdapter) error {
	panic("unported: mj-sessionwiki/src/sessionwiki/provenance.rs backfill")
}

// provenanceReplace is Rust's `replace`: one session's touched files and
// edits, replaced in one transaction and marked with provenanceRevision.
func provenanceReplace(conn *store.Connection, id string, session *brokksessionwiki.Session) error {
	panic("unported: mj-sessionwiki/src/sessionwiki/provenance.rs replace")
}
