// Port of mj-store/src/database/tests.rs.

package store

import "github.com/BrokkAi/mjolnir/go/internal/core"

// session is the store tests' session record (database.rs TestSession).
func session(id, bundle string) core.SessionRecord {
	return TestSession(id, bundle)
}

// registerTestSubagent writes a parent, a child, and the relation between
// them, the way the controller writes them.
func registerTestSubagent(path string, session *core.SessionRecord, relation *core.SubagentRecord) error {
	panic("unported: mj-store/src/database/tests.rs register_test_subagent")
}

// testSubagent is a worker child relation from parentID to childID.
func testSubagent(parentID, childID string) core.SubagentRecord {
	panic("unported: mj-store/src/database/tests.rs test_subagent")
}
