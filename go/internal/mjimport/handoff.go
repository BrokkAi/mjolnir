// Port of mj-import/src/handoff.rs.

// The shared decision that turns a session snapshot into a handoff message.
//
// Both the cross-harness resume and the native-continuity recovery need the
// same choice: resolve a utility model and summarize the transcript, or,
// when no model is available, hand the shared recent transcript view over.
// That decision lives here once so the two callers cannot drift apart.
//
// This file makes only the resolve/summarize/recent-context decision. The
// executor and cancellation-poll wrapper that keeps a cancelled operation
// from waiting out several network requests belongs to each caller.

package mjimport

import (
	"context"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// NativePriorFromArchive: checkpoint native history is optional evidence
// for a cross-harness handoff. A missing or unusable native summary leaves
// the canonical handoff intact (nil, nil); only a cancellation (ctx) is an
// error once reading has started.
func NativePriorFromArchive(ctx context.Context, sessionID string, archivePath string, manifest *core.ArchiveManifest, snapshot *core.CanonicalSessionSnapshot) (*PriorCompaction, error) {
	panic("unported: mj-import/src/handoff.rs native_prior_from_archive")
}

// ProfileHandoffBytes is the handoff byte budget for a target profile: its
// configured context window, or core.DefaultContextBytes when it names none
// (or there is no profile). This is the target harness's budget for its
// first message, not the summarizer's page size.
func ProfileHandoffBytes(profile *core.HarnessProfile) int {
	if profile != nil && profile.ContextWindowBytes != nil {
		return *profile.ContextWindowBytes
	}
	return core.DefaultContextBytes
}

// BuildHandoffContext resolves a utility model and summarizes the snapshot
// into a handoff, falling back to a bounded recent-transcript handoff when
// no model is available.
//
// A cancelled discovery (ctx) is the caller's own doing and propagates as
// an error. Any other resolve failure means no utility model is configured,
// credentialed, or in quota; the handoff still has to happen, so it is
// logged and answered with the bounded recent handoff.
func BuildHandoffContext(ctx context.Context, sessionID string, config *core.Config, snapshot *core.CanonicalSessionSnapshot, contextBytes int) (string, error) {
	return BuildHandoffContextWithPrior(ctx, sessionID, config, snapshot, contextBytes, nil)
}

// BuildHandoffContextWithPrior is BuildHandoffContext continuing from an
// earlier compaction prior, when there is one.
func BuildHandoffContextWithPrior(ctx context.Context, sessionID string, config *core.Config, snapshot *core.CanonicalSessionSnapshot, contextBytes int, prior *PriorCompaction) (string, error) {
	panic("unported: mj-import/src/handoff.rs build_handoff_context_with_prior")
}

// summarizeOrRecent summarizes the snapshot through an already-resolved
// backend, falling back to the bounded recent transcript when the
// summarizer fails. This is the seam the branch tests drive with a fake
// backend.
//
// A summarizer that resolved but then failed must not cost the whole
// handoff: the bounded recent context is the same floor used when no model
// was available, so a caller never loses the transcript to a summarizer
// error. The native-continuity path in particular installed bounded recent
// context before this code was shared, and must keep that floor. A
// cancellation (ctx) is the caller's own doing and propagates.
func summarizeOrRecent(ctx context.Context, sessionID string, snapshot *core.CanonicalSessionSnapshot, contextBytes int, backend CompactionBackend, pageBytes int, prior *PriorCompaction) (string, error) {
	panic("unported: mj-import/src/handoff.rs summarize_or_recent")
}
