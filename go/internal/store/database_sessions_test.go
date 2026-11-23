// Port of the tests in mj-store/src/database/sessions.rs.

package store

import (
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// loadedSession unwraps a session read the test expects to find; a
// failure panics, failing the test.
func loadedSession(record *core.SessionRecord, err error) core.SessionRecord {
	if err != nil {
		panic(err)
	}
	if record == nil {
		panic("session record is missing")
	}
	return *record
}

// The unguarded catch-up must not resurrect a lifecycle that won before
// publication.
func TestAdoptionConnectionPublicationDoesNotResurrectAStoppedWorker(t *testing.T) {
	path := filepath.Join(t.TempDir(), "adoption-race.sqlite3")
	expected := session("adoption-owner", "project")
	expected.State = core.SessionStateDisconnected
	expected.NativeSessionID = nil
	expected.Target = core.TargetLocatorLocalBare{WorkerRoot: "/workers/" + expected.ID}
	if err := SaveSessionTo(path, &expected); err != nil {
		t.Fatal(err)
	}
	stopped := expected
	stopped.State = core.SessionStateStopped
	stopped.Target = nil
	if err := SaveSessionTo(path, &stopped); err != nil {
		t.Fatal(err)
	}
	opened, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	stopped = loadedSession(loadSessionWith(opened, expected.ID))
	opened.Close()
	connection, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	late := "late-native"
	if _, err := publishAdoptionConnectedWith(connection, &expected, &late, "2030-01-01T00:00:00Z"); err == nil {
		t.Fatal("a late adoption publication resurrected a stopped worker")
	}
	if current := loadedSession(loadSessionWith(connection, expected.ID)); !reflect.DeepEqual(current, stopped) {
		t.Fatalf("session = %#v, want %#v", current, stopped)
	}
}

// Hard-won: b8acf76: a legacy refusal rewrote concurrent title/notification edits from a stale record.
func TestLegacyAdoptionRefusalPreservesConcurrentEditsAndResourceOwnership(t *testing.T) {
	path := filepath.Join(t.TempDir(), "legacy-adoption.sqlite3")
	expected := session("legacy-owner", "project")
	expected.State = core.SessionStateDisconnected
	expected.NativeSessionID = nil
	diagnosis := "previous diagnosis"
	expected.LastError = &diagnosis
	expected.Target = core.TargetLocatorLocalBare{WorkerRoot: "/workers/" + expected.ID}
	if err := SaveSessionTo(path, &expected); err != nil {
		t.Fatal(err)
	}
	connection, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	if _, err := connection.Exec("UPDATE sessions SET title='concurrent edit', notifications_ignored=1 WHERE session_id=?1", expected.ID); err != nil {
		t.Fatal(err)
	}
	refused, err := refuseAdoptionWith(connection, &expected, "legacy refusal", "2030-01-01T00:00:00Z")
	if err != nil {
		t.Fatal(err)
	}
	if refused.Title != "concurrent edit" {
		t.Fatalf("title = %q", refused.Title)
	}
	if !refused.NotificationsIgnored {
		t.Fatal("the refusal reverted the notification choice")
	}
	if refused.State != core.SessionStateLost {
		t.Fatalf("state = %v, want lost", refused.State)
	}
	if refused.Target != nil {
		t.Fatal("the refusal kept the worker")
	}
	if refused.LastError == nil || *refused.LastError != "previous diagnosis\nlegacy refusal" {
		t.Fatalf("last error = %v", refused.LastError)
	}
	// A later lifecycle owner cannot be detached by the same stale refusal.
	stopped := refused
	stopped.State = core.SessionStateStopped
	if err := SaveSessionTo(path, &stopped); err != nil {
		t.Fatal(err)
	}
	stopped = loadedSession(loadSessionWith(connection, expected.ID))
	if _, err := refuseAdoptionWith(connection, &expected, "late refusal", "2030-01-01T00:00:01Z"); err == nil {
		t.Fatal("a stale refusal detached a later owner")
	}
	if current := loadedSession(loadSessionWith(connection, expected.ID)); !reflect.DeepEqual(current, stopped) {
		t.Fatalf("session = %#v, want %#v", current, stopped)
	}
}

// Hard-won: de056ae: a late adoption failure overwrote a successful worker connection.
func TestAStaleAdoptionFailurePreservesTheConnectedOwner(t *testing.T) {
	path := filepath.Join(t.TempDir(), "adoption.sqlite3")
	stale := session("adoption-owner", "project")
	stale.State = core.SessionStateDisconnected
	stale.NativeSessionID = nil
	if err := SaveSessionTo(path, &stale); err != nil {
		t.Fatal(err)
	}
	surviving := "surviving-native"
	if err := markSessionWorkerConnectedTo(path, stale.ID, &surviving, "2030-01-01T00:00:00Z"); err != nil {
		t.Fatal(err)
	}
	current := loadedSession(recordAdoptionFailureIfCurrentTo(path, &stale, "late failure", "2030-01-01T00:00:01Z"))
	if current.State != core.SessionStateRunning {
		t.Fatalf("state = %v, want running", current.State)
	}
	if current.NativeSessionID == nil || *current.NativeSessionID != "surviving-native" {
		t.Fatalf("native session = %v", current.NativeSessionID)
	}
	if current.LastError != nil {
		t.Fatalf("last error = %q", *current.LastError)
	}
	saved, err := LoadStateFrom(path)
	if err != nil {
		t.Fatal(err)
	}
	if got := *saved.Sessions.Index(stale.ID); !reflect.DeepEqual(got, current) {
		t.Fatalf("saved = %#v, want %#v", got, current)
	}
	// Hard-won: 3210bf3: a captured failure owner must not outlive identity changes.
	for _, field := range []string{"profile", "target"} {
		changed := stale
		diagnosis := "new owner's diagnosis"
		changed.LastError = &diagnosis
		switch field {
		case "profile":
			changed.LastProfile += "-new"
		case "target":
			changed.TargetTemplateID += "-new"
		}
		if err := SaveSessionTo(path, &changed); err != nil {
			t.Fatal(err)
		}
		state, err := LoadStateFrom(path)
		if err != nil {
			t.Fatal(err)
		}
		expected := *state.Sessions.Index(stale.ID)
		actual := loadedSession(recordAdoptionFailureIfCurrentTo(path, &stale, "stale refusal", "2030-01-01T00:00:02Z"))
		if !reflect.DeepEqual(actual, expected) {
			t.Fatalf("%s change: session = %#v, want %#v", field, actual, expected)
		}
	}
}

// Hard-won: #1538: a resume save overwrote a Container settings edit made while it provisioned
func TestResumeAndRollbackPreserveEditsCommittedAfterTheirSnapshot(t *testing.T) {
	path := filepath.Join(t.TempDir(), "resume.sqlite3")
	original := session("resume-owner", "project")
	if err := SaveSessionTo(path, &original); err != nil {
		t.Fatal(err)
	}
	connection, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	if _, err := connection.Exec(`UPDATE sessions SET session_title_override = 'renamed during resume',
                 acp_session_title = 'new worker title', draft_input = 'new draft',
                 archived = 1, viewed_through_event_ordinal = 99 WHERE session_id = ?1`, original.ID); err != nil {
		t.Fatal(err)
	}
	provisioning := original
	provisioning.State = core.SessionStateProvisioning
	provisioning.Target = nil
	resumed := "resumed-native"
	provisioning.NativeSessionID = &resumed
	provisioning.AdditionalMounts = nil
	if err := saveResumedSessionTo(path, &provisioning, nil); err != nil {
		t.Fatal(err)
	}
	current := loadedSession(loadSessionWith(connection, original.ID))
	if current.State != core.SessionStateProvisioning {
		t.Fatalf("state = %v, want provisioning", current.State)
	}
	if !reflect.DeepEqual(current.NativeSessionID, provisioning.NativeSessionID) {
		t.Fatalf("native session = %v", current.NativeSessionID)
	}
	if !reflect.DeepEqual(current.AdditionalMounts, original.AdditionalMounts) {
		t.Fatalf("mounts = %#v, want %#v", current.AdditionalMounts, original.AdditionalMounts)
	}
	if current.SessionTitleOverride == nil || *current.SessionTitleOverride != "renamed during resume" {
		t.Fatalf("title override = %v", current.SessionTitleOverride)
	}
	if current.ACPSessionTitle == nil || *current.ACPSessionTitle != "new worker title" {
		t.Fatalf("ACP title = %v", current.ACPSessionTitle)
	}
	if current.DraftInput != "new draft" {
		t.Fatalf("draft = %q", current.DraftInput)
	}
	if !current.Archived {
		t.Fatal("archive choice was reverted")
	}
	if current.ViewedThroughEventOrdinal != 99 {
		t.Fatalf("viewed through = %d, want 99", current.ViewedThroughEventOrdinal)
	}

	var allocation core.SessionResourceAllocation = core.SessionResourceAllocationContainer{Cpus: 8, MemoryBytes: 16 << 30}
	mounts := []core.AdditionalMount{{
		Source:      "/new/source",
		Destination: "/new/destination",
		Access:      core.MountAccessRo,
	}}
	if err := setSessionContainerSettingsTo(path, original.ID, allocation, mounts, "2026-11-13T00:00:00Z"); err != nil {
		t.Fatal(err)
	}
	// The converted-checkpoint publication still holds the provisioning snapshot.
	provisioning.Checkpoint = &core.CheckpointMetadata{
		ArchivePath:   "converted.hel.zip",
		Sha256:        strings.Repeat("c", 64),
		CreatedAt:     "2026-11-13T00:01:00Z",
		EventFrontier: 1,
	}
	if err := saveResumedSessionTo(path, &provisioning, nil); err != nil {
		t.Fatal(err)
	}
	converted := loadedSession(loadSessionWith(connection, original.ID))
	if !reflect.DeepEqual(converted.ResourceAllocation, allocation) {
		t.Fatalf("allocation = %#v, want %#v", converted.ResourceAllocation, allocation)
	}
	if !reflect.DeepEqual(converted.AdditionalMounts, mounts) {
		t.Fatalf("mounts = %#v, want %#v", converted.AdditionalMounts, mounts)
	}

	// Failure restores a snapshot captured before those independent edits.
	if err := saveResumedSessionTo(path, &original, nil); err != nil {
		t.Fatal(err)
	}
	rolledBack := loadedSession(loadSessionWith(connection, original.ID))
	if rolledBack.State != original.State {
		t.Fatalf("state = %v, want %v", rolledBack.State, original.State)
	}
	if !reflect.DeepEqual(rolledBack.AdditionalMounts, mounts) {
		t.Fatalf("mounts = %#v, want %#v", rolledBack.AdditionalMounts, mounts)
	}
	if !reflect.DeepEqual(rolledBack.ResourceAllocation, allocation) {
		t.Fatalf("allocation = %#v, want %#v", rolledBack.ResourceAllocation, allocation)
	}
	if !reflect.DeepEqual(rolledBack.SessionTitleOverride, current.SessionTitleOverride) {
		t.Fatalf("title override = %v", rolledBack.SessionTitleOverride)
	}
	if !reflect.DeepEqual(rolledBack.ACPSessionTitle, current.ACPSessionTitle) {
		t.Fatalf("ACP title = %v", rolledBack.ACPSessionTitle)
	}
	if rolledBack.DraftInput != current.DraftInput {
		t.Fatalf("draft = %q", rolledBack.DraftInput)
	}
	if rolledBack.Archived != current.Archived {
		t.Fatal("archive choice was reverted")
	}
	if rolledBack.ViewedThroughEventOrdinal != 99 {
		t.Fatalf("viewed through = %d, want 99", rolledBack.ViewedThroughEventOrdinal)
	}
}
