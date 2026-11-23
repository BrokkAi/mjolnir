// Port of mj-store/src/database/container_size.rs.

// Breaking revision 132: one durable per-session container size.

package store

import (
	"encoding/json"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// decimalUnits parses positive decimal engine sizes exactly, rounding up
// only the final unit; false when text is not one or the size is zero or
// overflows.
func decimalUnits(text string, multiplier uint64) (uint64, bool) {
	panic("unported: mj-store/src/database/container_size.rs decimal_units")
}

func memoryBytes(text string) (uint64, bool) {
	panic("unported: mj-store/src/database/container_size.rs memory_bytes")
}

// fold combines a legacy allocation with the session's legacy cpu and memory
// override strings.
func fold(allocation core.SessionResourceAllocation, cpus, memory *string) core.SessionResourceAllocation {
	panic("unported: mj-store/src/database/container_size.rs fold")
}

// rewriteEmbeddedSizes uses persisted type locations rather than
// interpreting arbitrary JSON keys (a runtime environment map may itself
// contain a variable named container_cpus). value is a decoded JSON value
// (map[string]any and friends) rewritten in place.
func rewriteEmbeddedSizes(value any, sessionPaths, snapshotPaths []string, changes *[]any) error {
	panic("unported: mj-store/src/database/container_size.rs rewrite_embedded_sizes")
}

func foldJSONAllocation(allocation, cpus, memory json.RawMessage) (core.SessionResourceAllocation, error) {
	panic("unported: mj-store/src/database/container_size.rs fold_json_allocation")
}

// rewriteMoveSource preserves the effective source size of a legacy failed
// Move: its Resume restored this allocation together with the recovery
// session's override strings.
func rewriteMoveSource(value any, inheritedCpus, inheritedMemory *string, changes *[]any) error {
	panic("unported: mj-store/src/database/container_size.rs rewrite_move_source")
}

func foldFields(fields map[string]any, cpuKey, memoryKey, path string, changes *[]any) error {
	panic("unported: mj-store/src/database/container_size.rs fold_fields")
}

// conversion is one row the migration changed or whose overrides it dropped.
type conversion struct {
	table      string
	recordID   string
	sessionID  string
	cpus       *string
	memory     *string
	allocation core.SessionResourceAllocation
	snapshot   any
}

// migrate converts every stored container size and returns the
// changed/dropped rows after committing, so no rolled-back change is
// reported.
func migrate(conn *Connection) ([]conversion, error) {
	panic("unported: mj-store/src/database/container_size.rs migrate")
}

// Report logs each conversion migrate made.
func Report(changes []conversion) {
	panic("unported: mj-store/src/database/container_size.rs report")
}
