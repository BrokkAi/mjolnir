// Port of mj-sessionwiki/src/lib.rs's test setup, and the helpers the
// package's tests share.

package sessionwiki

import (
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/brokksessionwiki"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
)

// Rust: mj_core::enable_test_fixtures_on_startup!().
func init() { core.EnableTestFixtures() }

// sessionwikiUnportedSkip is the reason a test that reaches an unported part
// of the SessionWiki library skips.
const sessionwikiUnportedSkip = "needs brokk-sessionwiki (not yet ported)"

// sessionwikiUnported runs probe and reports whether it reached a part of
// brokk-sessionwiki whose Go body is not ported yet. Any other panic
// propagates.
func sessionwikiUnported(probe func()) (unported bool) {
	defer func() {
		if recovered := recover(); recovered != nil {
			text, ok := recovered.(string)
			if !ok || !strings.HasPrefix(text, "unported: brokk-sessionwiki") {
				panic(recovered)
			}
			unported = true
		}
	}()
	probe()
	return false
}

// requireSessionwiki skips t while any of the SessionWiki library parts the
// probes reach is unported, so the test runs as soon as they are.
func requireSessionwiki(t *testing.T, probes ...func()) {
	t.Helper()
	for _, probe := range probes {
		if sessionwikiUnported(probe) {
			t.Skip(sessionwikiUnportedSkip)
		}
	}
}

// Probes of the SessionWiki library parts the tests reach.
var (
	probeRedact        = func() { brokksessionwiki.Redact("") }
	probeRenderSummary = func() { brokksessionwiki.RenderSummary(&brokksessionwiki.ToolSummary{}) }
	probeFoldToolParts = func() { brokksessionwiki.FoldToolParts(&brokksessionwiki.Session{}) }
	probeAdapters      = func() { brokksessionwiki.All() }
)

// isolatedIndex is Rust's `let _held = tags::testing::lock(); let (_dir,
// connection) = tags::testing::isolated_index();`: the shared lock held for
// the rest of t and a fresh index of t's own. It skips t while the index is
// unported.
func isolatedIndex(t *testing.T) *store.Connection {
	t.Helper()
	TagsTestingLock(t)
	var connection *store.Connection
	if sessionwikiUnported(func() { connection = TagsTestingIsolatedIndex(t) }) {
		t.Skip(sessionwikiUnportedSkip)
	}
	return connection
}

// jsonAt is the member of a JSON value (any value jsonvalue encodes) that the
// path of object keys and array indexes names, as Rust's `value["a"][0]`
// reads it: nil when the path leads nowhere.
func jsonAt(t *testing.T, value any, path ...any) any {
	t.Helper()
	encoded, err := jsonvalue.Marshal(value)
	if err != nil {
		t.Fatalf("encode %T: %v", value, err)
	}
	current, err := jsonvalue.Parse(encoded)
	if err != nil {
		t.Fatalf("parse %s: %v", encoded, err)
	}
	for _, step := range path {
		switch step := step.(type) {
		case string:
			current = jsonMember(current, step)
		case int:
			items, ok := current.([]any)
			if !ok || step >= len(items) {
				return nil
			}
			current = items[step]
		default:
			t.Fatalf("path step %v is neither a key nor an index", step)
		}
	}
	return current
}

// jsonInt is a JSON number jsonAt read, as an int.
func jsonInt(t *testing.T, value any) int {
	t.Helper()
	switch number := value.(type) {
	case int64:
		return int(number)
	case uint64:
		return int(number)
	}
	t.Fatalf("%#v is not an integer", value)
	return 0
}

// jsonLen is the length of a JSON array jsonAt read.
func jsonLen(t *testing.T, value any) int {
	t.Helper()
	items, ok := value.([]any)
	if !ok {
		t.Fatalf("%#v is not an array", value)
	}
	return len(items)
}

// snapshotForTestTopLevel is Rust's Snapshot::for_test_top_level.
func snapshotForTestTopLevel(sessionID string) topLevelSnapshot {
	return topLevelSnapshot{topLevelSessionIDs: map[string]struct{}{sessionID: {}}}
}

// snapshotForTest is Rust's Snapshot::for_test.
func snapshotForTest(childSessionIDs ...string) topLevelSnapshot {
	children := map[string]struct{}{}
	for _, id := range childSessionIDs {
		children[id] = struct{}{}
	}
	return topLevelSnapshot{childSessionIDs: children}
}

// stringSet is a set of strings, as Rust's BTreeSet::from builds one.
func stringSet(values ...string) map[string]struct{} {
	set := map[string]struct{}{}
	for _, value := range values {
		set[value] = struct{}{}
	}
	return set
}

func ptr[T any](value T) *T { return &value }
