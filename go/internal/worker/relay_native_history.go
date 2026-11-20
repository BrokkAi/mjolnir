// Port of mj-worker/src/relay/native_history.rs.
//
// Upgrade-time recovery of the native-session use evidence and pending
// questions older relay snapshots did not record.

package worker

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// recoverNativeHistoryEvidence: older snapshots predate the evidence that
// permits replacing an empty native session. Recover that evidence once,
// without replaying historical commands over the live snapshot or changing
// any journal frontiers.
func (r *DurableRelay) recoverNativeHistoryEvidence() bool {
	panic("unported: mj-worker/src/relay/native_history.rs recover_native_history_evidence")
}

func (r *DurableRelay) readNativeHistoryEvidence() (NativeHistoryEvidence, error) {
	var evidence NativeHistoryEvidence
	err := r.visitLegacyEvidence(func(event core.RelayEvent) {
		evidence.observe(event, r.snapshot.Dispatches)
	})
	return evidence, err
}

func (r *DurableRelay) recoverPendingElicitations() bool {
	panic("unported: mj-worker/src/relay/native_history.rs recover_pending_elicitations")
}

func (r *DurableRelay) visitLegacyEvidence(visit func(core.RelayEvent)) error {
	panic("unported: mj-worker/src/relay/native_history.rs visit_legacy_evidence")
}

// nativeHistoryOpened is the native session a SessionOpened event named and
// that event's ordinal (Rust's (String, u64)).
type nativeHistoryOpened struct {
	nativeSessionID string
	ordinal         uint64
}

type NativeHistoryEvidence struct {
	opened *nativeHistoryOpened
	used   bool
	// commands keeps identities and kinds only; queued prompt bodies can be
	// large.
	commands map[string]core.RelayCommandKind
}

func (e *NativeHistoryEvidence) observe(event core.RelayEvent, dispatches map[string]core.RelayDispatchRecord) {
	panic("unported: mj-worker/src/relay/native_history.rs observe")
}
