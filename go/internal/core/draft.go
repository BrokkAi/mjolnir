// Port of mj-core/src/draft.rs.
//
// Shared persisted composer-envelope operations. Clipboard processing stays
// in the composer; storage merges envelopes without interpreting native
// images.

package core

const ChatDraftPrefix = "mjolnir-chat-draft-v1:"

type envelope struct {
	fields map[string]any
	text   string
	images []any
}

func envelopeDecode(draft string) (*envelope, error) {
	panic("unported: mj-core/src/draft.rs Envelope::decode")
}

func (e *envelope) encode() (string, error) {
	panic("unported: mj-core/src/draft.rs Envelope::encode")
}

// RemoveStartupReceipt retires exactly one saved startup receipt when its
// payload becomes a draft. Native submissions and other startup identities
// keep their saved ownership.
func RemoveStartupReceipt(draft, commandID string) (string, error) {
	panic("unported: mj-core/src/draft.rs remove_startup_receipt")
}

// imageRange is an image's (start, end, number).
func imageRange(image any) (int, int, uint64, error) {
	panic("unported: mj-core/src/draft.rs image_range")
}

// MergeDrafts appends recovered input while preserving tracked images and
// envelope metadata. Conflicting unknown metadata fails visibly rather than
// silently discarding it.
func MergeDrafts(existing, recovered string) (string, error) {
	panic("unported: mj-core/src/draft.rs merge_drafts")
}

// ValidateReferenceDraft: durable startup drafts must carry installed
// references, never inline bytes.
func ValidateReferenceDraft(draft string) error {
	panic("unported: mj-core/src/draft.rs validate_reference_draft")
}
