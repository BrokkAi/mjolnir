// Port of mj-accounts/src/review_selection.rs.

// Which reviewers a session gets, decided from the profiles on offer.

package accounts

import (
	"context"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// AvailableProfile is one profile on offer with its discovered choices
// (Rust: a `(String, ProfileConfig)` pair).
type AvailableProfile struct {
	ID      string
	Choices core.ProfileConfig
}

// ReviewerProbe discovers a profile's choices with model selected. Rust:
// `Fn(String, String) -> Future<Output = anyhow::Result<ProfileConfig>>`,
// called as `probe(profile_id, model)`.
type ReviewerProbe func(ctx context.Context, profileID, model string) (core.ProfileConfig, error)

// UsagePricedProfiles is the profiles among candidates billed per use:
// their last quota report says so, or their harness implies it (OpenCode
// reports no allowance, so a profile on it has no subscription report). A
// profile whose billing neither decides counts as a subscription here; see
// UnclassifiedInDraft for callers that must not guess.
func UsagePricedProfiles(candidates *SubagentCandidates) map[string]bool {
	priced := map[string]bool{}
	for i := range candidates.Offered {
		candidate := &candidates.Offered[i]
		if candidate.Quota.Billing(candidate.Harness) == client.QuotaBillingUsage {
			priced[candidate.ProfileID] = true
		}
	}
	return priced
}

// UnclassifiedInDraft is the profiles in unclassified (billing not yet
// known) that offer a model in a family the legacy upgrade would draft from
// available. Empty when a pinned profile decides the reviewers without
// Autoconfigure.
func UnclassifiedInDraft(legacy *core.LegacyReview, available []AvailableProfile, usagePriced, unclassified map[string]bool) []string {
	panic("unported: mj-accounts/src/review_selection.rs unclassified_in_draft")
}

// UpgradeReviewers is the reviewers a legacy `[review]` section upgrades
// to, whether their efforts are known, and the efforts it had to adjust.
func UpgradeReviewers(ctx context.Context, legacy *core.LegacyReview, available []AvailableProfile, usagePriced map[string]bool, probe ReviewerProbe) ([]core.SubagentChoice, bool, []core.EffortAdjustment) {
	panic("unported: mj-accounts/src/review_selection.rs upgrade_reviewers")
}

// AutoconfigureWithEfforts computes the ordered legacy Auto preset using the
// same model and effort discovery for the daemon upgrade and the startup
// review fallback. The map records, per drafted model, whether its efforts
// are known.
func AutoconfigureWithEfforts(ctx context.Context, available []AvailableProfile, usagePriced map[string]bool, probe ReviewerProbe) (core.Autoconfigured, map[string]bool) {
	panic("unported: mj-accounts/src/review_selection.rs autoconfigure_with_efforts")
}

// PreemptedByLifecycle is whether a refusal only says another operation
// held the session: a lease refused the reviewer action, a lease taken
// meanwhile cancelled it, or the worker turned reviewer work away during a
// checkpoint or replacement (the recovery copy a finished turn starts, or an
// upgrade to the current build).
func PreemptedByLifecycle(reason string) bool {
	return strings.Contains(reason, "session is reserved for a lifecycle operation") ||
		strings.Contains(reason, "cancelled for session lifecycle change") ||
		strings.Contains(reason, "worker is reserved for checkpoint or replacement")
}
