// Port of mj-store/src/database/prompts.rs (its #[cfg(test)] items).

package store

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// recordPromptTo records a delivered prompt keyed by its event ordinal, for
// fixtures that need several rows of the same text. submittedAt nil records
// the current time.
func recordPromptTo(path, sessionID, bundleID string, eventOrdinal uint64, submittedAt *string, text string) error {
	panic("unported: mj-store/src/database/prompts.rs record_prompt_to")
}

// searchPromptsFrom is every match in one page, for tests that check what a
// search finds rather than how it pages.
func searchPromptsFrom(path, sessionID, bundleID string, scope core.HistoryScope, query string) ([]core.PromptHistoryEntry, error) {
	panic("unported: mj-store/src/database/prompts.rs search_prompts_from")
}
