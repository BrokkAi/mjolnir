// Port of mj-transcript/src/authorization.rs.
//
// Whole-message authorization evidence shared by workers and controllers.

package transcript

import (
	"iter"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Evidence is the authorization evidence of a materialized session.
func Evidence(session *core.MaterializedSession) (core.ContinuationEvidence, error) {
	return EvidenceFromItems(func(yield func(*core.TranscriptItem, error) bool) {
		for i := len(session.Transcript) - 1; i >= 0; i-- {
			if !yield(session.Transcript[i], nil) {
				return
			}
		}
	})
}

// EvidenceFromItems reads transcript items newest first; an error from the
// sequence ends the read with that error.
func EvidenceFromItems(items iter.Seq2[*core.TranscriptItem, error]) (core.ContinuationEvidence, error) {
	panic("unported: mj-transcript/src/authorization.rs evidence_from_items")
}
