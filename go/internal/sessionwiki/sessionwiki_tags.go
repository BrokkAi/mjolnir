// Port of mj-sessionwiki/src/sessionwiki/tags.rs.
//
// Mjolnir's own metadata on an indexed session, stored as SessionWiki tags.
//
// SessionWiki's index is a cache of every tool's sessions plus a few
// *durable* tables the crate promises never to drop on a schema bump: `tags`,
// `notes`, `summaries` and `archive` (see `index::open` in the
// `brokk-sessionwiki` crate). Mjolnir needs a Mjolnir session's target,
// profile and harness to survive in the index, because the archive job
// destroys the Mjolnir record and the rows that most need the metadata are
// exactly the ones with no record left to join against. So the metadata is
// written into the durable `tags` table, one tag each: `mj-target:<target
// template id>`, `mj-profile:<harness profile id>` and `mj-harness:<harness
// kind id>`.
//
// Mjolnir writes and reads the table with its own SQL rather than through
// the crate's `add_tag` and `remove_tag`, for two reasons. The crate
// normalizes a tag by lowercasing it, and Mjolnir ids may be mixed case, so a
// round trip through `add_tag` would corrupt an id. And the crate offers no
// way to find a stale `mj-target:` tag after a Move without reading every tag
// back.
//
// Keeping the SQL here, in one module with a round-trip test against a real
// index, means an upstream change to the table fails a test rather than a
// user. Ids contain no commas (core.ValidateID allows only `[A-Za-z0-9._-]`),
// so the crate's comma-joined `tags` column still parses.
//
// The module's public functions carry the module name (TagsWrite, TagsRead,
// TagsTesting...), since bare Write and Read would say nothing in this flat
// package.

package sessionwiki

import (
	"strings"
	"sync"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/brokksessionwiki"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
)

// The tag prefix of each field, and how a tag maps back onto the struct
// (Rust's TARGET, PROFILE and HARNESS).
const (
	tagsTarget  = "mj-target:"
	tagsProfile = "mj-profile:"
	tagsHarness = "mj-harness:"
)

// MjTags is what Mjolnir knows about one of its own indexed sessions beyond
// what SessionWiki stores for every tool.
type MjTags struct {
	// Target is the target template the session ran on.
	Target *string
	// Profile is the harness profile the session last ran under.
	Profile *string
	// Harness is the harness kind, as core.HarnessKind.ID spells it.
	Harness *string
}

// IsEmpty reports whether there is nothing worth storing. A session with no
// record left contributes nothing and must not clear what an earlier sync
// wrote.
func (t *MjTags) IsEmpty() bool {
	return t.Target == nil && t.Profile == nil && t.Harness == nil
}

// tags is the tags this metadata is stored as.
func (t *MjTags) tags() []string {
	var tags []string
	if t.Target != nil {
		tags = append(tags, tagsTarget+*t.Target)
	}
	if t.Profile != nil {
		tags = append(tags, tagsProfile+*t.Profile)
	}
	if t.Harness != nil {
		tags = append(tags, tagsHarness+*t.Harness)
	}
	return tags
}

// absorb records one stored tag, ignoring any `mj-` tag this version does
// not know.
func (t *MjTags) absorb(tag string) {
	if value, ok := strings.CutPrefix(tag, tagsTarget); ok {
		t.Target = &value
	} else if value, ok := strings.CutPrefix(tag, tagsProfile); ok {
		t.Profile = &value
	} else if value, ok := strings.CutPrefix(tag, tagsHarness); ok {
		t.Harness = &value
	}
}

// TagsWrite is Rust's `tags::write`: replace one session's Mjolnir tags with
// the current ones.
//
// The delete is what keeps a Move or a profile switch from leaving a stale
// target behind: the record changed, and the next sync writes the new value
// over the old one. Runs inside the caller's transaction, so a sync pays for
// one commit rather than one per session.
func TagsWrite(conn store.Queryer, sessionID string, tags *MjTags) error {
	panic("unported: mj-sessionwiki/src/sessionwiki/tags.rs write")
}

// TagsRead is Rust's `tags::read`: the Mjolnir metadata of each of
// sessionIDs that has any, in one query.
func TagsRead(conn store.Queryer, sessionIDs []string) (map[string]MjTags, error) {
	panic("unported: mj-sessionwiki/src/sessionwiki/tags.rs read")
}

// Rust's `tags::testing`: opening a SessionWiki index of this test process's
// own.
//
// The location comes from the process-wide `SESSIONWIKI_DATA` variable,
// which is also what tells Mjolnir it may touch an index at all
// (indexIsWritable). Every test that points it somewhere has to take the
// same turn, so the guard lives here and the tests of this package and of the
// packages above it share it. Rust keeps the module public (doc-hidden) for
// those packages' tests; Go test files cannot be imported, so it is ordinary
// code here.

// TagsTestingIndex is held for as long as a test relies on
// `SESSIONWIKI_DATA` (Rust's INDEX).
var TagsTestingIndex sync.Mutex

// TagsTestingLock is Rust's `testing::lock`: it takes TagsTestingIndex for
// the rest of t.
func TagsTestingLock(t testing.TB) {
	TagsTestingIndex.Lock()
	t.Cleanup(TagsTestingIndex.Unlock)
}

// TagsTestingIsolatedIndex is Rust's `testing::isolated_index`: a real
// SessionWiki index in a temporary directory, opened through the crate's own
// Open so the schema under test is the shipped one. Call it only while
// holding TagsTestingLock.
func TagsTestingIsolatedIndex(t testing.TB) *store.Connection {
	t.Setenv(core.SessionIndexEnv, t.TempDir())
	connection, err := brokksessionwiki.Open()
	if err != nil {
		t.Fatalf("open a fresh index: %v", err)
	}
	t.Cleanup(func() { connection.Close() })
	return connection
}

// TagsTestingIndexRow is Rust's `testing::index_row`: one row in the index's
// cache, which is what a query answers from.
func TagsTestingIndexRow(t testing.TB, conn store.Queryer, sessionID string, tool string) {
	if _, err := conn.Exec(
		`INSERT INTO files(path, tool, session_id, project, title, mtime, size, msg_count)
                 VALUES (?1, ?2, ?3, '/src/project', 'an indexed session', 0, 0, 2)`,
		"/checkpoints/"+sessionID, tool, sessionID,
	); err != nil {
		t.Fatalf("insert a cached row: %v", err)
	}
}
