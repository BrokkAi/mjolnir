// Port of mj-store/src/database/client_state.rs.

package store

import (
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func ClientReadFrontier(clientID, workspaceID, sessionID string) (uint64, error) {
	return clientReadFrontierAt(DatabasePath(), clientID, workspaceID, sessionID)
}

func clientReadFrontierAt(path, clientID, workspaceID, sessionID string) (uint64, error) {
	panic("unported: mj-store/src/database/client_state.rs client_read_frontier_at")
}

func AdvanceClientReadFrontier(clientID, workspaceID, sessionID string, through uint64) (uint64, error) {
	panic("unported: mj-store/src/database/client_state.rs advance_client_read_frontier")
}

// ClientSessionState is what this viewer has stored for this session: an
// unsent draft and how far it has read.
func ClientSessionState(clientID, workspaceID, sessionID string) (core.ClientSessionState, error) {
	panic("unported: mj-store/src/database/client_state.rs client_session_state")
}

// PersistClientDraft stores one viewer's unsent draft.
//
// An empty draft deletes the row rather than storing emptiness, so a viewer
// that cleared its composer stops occupying a row and stops being pruned
// later for something it no longer holds.
func PersistClientDraft(clientID, workspaceID, sessionID, draft string) error {
	panic("unported: mj-store/src/database/client_state.rs persist_client_draft")
}

// PrunePhoneClientState forgets web-viewer state that has passed its
// retention.
//
// Only rows whose client id names a phone are considered. A terminal
// client's read frontier is not the phone's to expire, and deleting one
// would lose a person's place in a conversation they are still reading.
func PrunePhoneClientState(olderThan time.Duration) (int, error) {
	panic("unported: mj-store/src/database/client_state.rs prune_phone_client_state")
}

func PersistReadReceipt(clientID, workspaceID, sessionID string, through uint64) (uint64, error) {
	panic("unported: mj-store/src/database/client_state.rs persist_read_receipt")
}

func persistReadReceiptWith(conn *Connection, clientID, workspaceID, sessionID string, through uint64) (uint64, error) {
	panic("unported: mj-store/src/database/client_state.rs persist_read_receipt_with")
}

func advanceClientReadFrontierAt(path, clientID, workspaceID, sessionID string, through uint64) (uint64, error) {
	panic("unported: mj-store/src/database/client_state.rs advance_client_read_frontier_at")
}

// SaveTerminalDraft persists one revision of a terminal's composer while
// retiring its unchanged legacy seed on the first accepted save.
func SaveTerminalDraft(sessionID, source string, ownerPid uint32, revision uint64, draft core.DetachedSessionDraft) error {
	panic("unported: mj-store/src/database/client_state.rs save_terminal_draft")
}

func saveTerminalDraftIn(conn *Connection, sessionID, source string, ownerPid uint32, revision int64, draft *core.DetachedSessionDraft) error {
	panic("unported: mj-store/src/database/client_state.rs save_terminal_draft_in")
}

func SaveDetachedDraft(workspaceID string, sessionID *string, source string, ownerPid *uint32, text string) (*string, error) {
	panic("unported: mj-store/src/database/client_state.rs save_detached_draft")
}

func saveDetachedDraftAt(path, workspaceID string, sessionID *string, source string, ownerPid *uint32, text string) (*string, error) {
	panic("unported: mj-store/src/database/client_state.rs save_detached_draft_at")
}

func insertDetachedDraft(conn Queryer, workspaceID string, sessionID *string, source string, ownerPid *uint32, text string) (*string, error) {
	panic("unported: mj-store/src/database/client_state.rs insert_detached_draft")
}

func ListDetachedDrafts(workspaceID string) ([]core.DetachedDraft, error) {
	return listDetachedDraftsAt(DatabasePath(), workspaceID)
}

// DetachedDraftListing is a detached draft as the workspace manager lists
// it. The text stays in the store: the manager shows source and time without
// reading it.
type DetachedDraftListing struct {
	ID          string
	WorkspaceID string
	SessionID   *string
	Source      string
	OwnerPid    *uint32
	SavedAt     string
}

// ListAllDetachedDrafts is every workspace's unrecovered detached drafts,
// newest first within each workspace.
func ListAllDetachedDrafts() ([]DetachedDraftListing, error) {
	panic("unported: mj-store/src/database/client_state.rs list_all_detached_drafts")
}

func listDetachedDraftsAt(path, workspaceID string) ([]core.DetachedDraft, error) {
	panic("unported: mj-store/src/database/client_state.rs list_detached_drafts_at")
}

func MarkDraftRecovered(draftID string) error {
	panic("unported: mj-store/src/database/client_state.rs mark_draft_recovered")
}

func markDraftRecoveredAt(path, draftID string) error {
	panic("unported: mj-store/src/database/client_state.rs mark_draft_recovered_at")
}

// RecoverDetachedDraft explicitly restores a detached draft into its session
// composer. This is the only operation that merges client-local draft state
// back into the legacy session field, and the transaction marks the source
// draft recovered at the same durable boundary.
func RecoverDetachedDraft(draftID string) (string, error) {
	panic("unported: mj-store/src/database/client_state.rs recover_detached_draft")
}

func recoverDetachedDraftAt(path, draftID string) (string, error) {
	panic("unported: mj-store/src/database/client_state.rs recover_detached_draft_at")
}
