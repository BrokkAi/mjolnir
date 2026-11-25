// Port of mj-accounts/src/review_selection.rs.

// Which reviewers a session gets, decided from the profiles on offer.

package accounts

import (
	"context"
	"log/slog"
	"slices"
	"sort"
	"strings"
	"sync"
	"time"

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
	if len(unclassified) == 0 || reviewPinnedProfile(legacy, available) != nil {
		return []string{}
	}
	offers := reviewModelOffers(available, usagePriced)
	draft, ok := core.Autoconfigure(offers, func(string) ([]core.SessionConfigChoice, bool) { return nil, false })
	if !ok {
		return []string{}
	}
	all := reviewAllModels(available)
	blocked := []string{}
	for _, profile := range available {
		if unclassified[profile.ID] && core.OffersDraftedFamily(profile.Choices.Models, all, draft.Reviewers) {
			blocked = append(blocked, profile.ID)
		}
	}
	return blocked
}

// reviewPinnedProfile is the available profile the legacy section names.
func reviewPinnedProfile(legacy *core.LegacyReview, available []AvailableProfile) *AvailableProfile {
	if legacy.Profile == nil {
		return nil
	}
	for i := range available {
		if available[i].ID == *legacy.Profile {
			return &available[i]
		}
	}
	return nil
}

// reviewModelOffers is the offers of every available profile, flagged
// usage-priced or not.
func reviewModelOffers(available []AvailableProfile, usagePriced map[string]bool) []core.AutoconfigureModelOffer {
	profiles := make([]core.AutoconfigureProfileModels, 0, len(available))
	for _, profile := range available {
		profiles = append(profiles, core.AutoconfigureProfileModels{
			Models:      profile.Choices.Models,
			UsagePriced: usagePriced[profile.ID],
		})
	}
	return core.ModelOffers(profiles)
}

// reviewAllModels is every available profile's models, in order.
func reviewAllModels(available []AvailableProfile) []core.SessionConfigChoice {
	all := []core.SessionConfigChoice{}
	for _, profile := range available {
		all = append(all, profile.Choices.Models...)
	}
	return all
}

// UpgradeReviewers is the reviewers a legacy `[review]` section upgrades
// to, whether their efforts are known, and the efforts it had to adjust.
func UpgradeReviewers(ctx context.Context, legacy *core.LegacyReview, available []AvailableProfile, usagePriced map[string]bool, probe ReviewerProbe) ([]core.SubagentChoice, bool, []core.EffortAdjustment) {
	if pinned := reviewPinnedProfile(legacy, available); pinned != nil {
		choices := &pinned.Choices
		var model string
		found := false
		if choices.Model != nil {
			model, found = *choices.Model, true
		} else if len(choices.Models) > 0 {
			model, found = choices.Models[0].Value, true
		}
		if found {
			var efforts []core.SessionConfigChoice
			known := false
			if choices.Model != nil && *choices.Model == model {
				efforts, known = slices.Clone(choices.Efforts), true
			} else {
				probeCtx, cancel := context.WithTimeout(ctx, 10*time.Second)
				discovered, err := reviewRunProbe(probeCtx, probe, *legacy.Profile, model)
				cancel()
				if err == nil {
					efforts, known = discovered.Efforts, true
				}
			}
			var effort *string
			if legacy.Effort != nil {
				if !known {
					requested := *legacy.Effort
					effort = &requested
				} else if fallback, ok := core.FallbackEffort(*legacy.Effort, efforts); ok {
					effort = &fallback
				}
			}
			adjustments := []core.EffortAdjustment{}
			if legacy.Effort != nil && (effort == nil || *effort != *legacy.Effort) {
				adjustments = append(adjustments, core.EffortAdjustment{
					Model:     model,
					Requested: *legacy.Effort,
					Used:      cloneOptionalString(effort),
				})
			}
			count := 2
			if legacy.Count != nil {
				count = *legacy.Count
			}
			count = min(max(count, 1), core.MaxReviewers)
			reviewers := make([]core.SubagentChoice, count)
			for index := range reviewers {
				reviewers[index] = core.SubagentChoice{Model: model, Effort: cloneOptionalString(effort)}
			}
			return reviewers, known, adjustments
		}
	}
	choices, known := AutoconfigureWithEfforts(ctx, available, usagePriced, probe)
	allKnown := true
	for _, modelKnown := range known {
		allKnown = allKnown && modelKnown
	}
	return choices.Reviewers, allKnown, choices.Adjusted
}

// cloneOptionalString is a copy of an optional string.
func cloneOptionalString(value *string) *string {
	if value == nil {
		return nil
	}
	copied := *value
	return &copied
}

// errReviewProbeTimedOut is a probe that did not answer before its
// deadline (Rust: tokio's Elapsed).
var errReviewProbeTimedOut = context.DeadlineExceeded

// reviewRunProbe runs probe until ctx ends. A probe that does not answer
// in time is abandoned, as Rust drops a timed-out future.
func reviewRunProbe(ctx context.Context, probe ReviewerProbe, profileID, model string) (core.ProfileConfig, error) {
	type outcome struct {
		choices core.ProfileConfig
		err     error
	}
	done := make(chan outcome, 1)
	go func() {
		choices, err := probe(ctx, profileID, model)
		done <- outcome{choices: choices, err: err}
	}()
	select {
	case result := <-done:
		return result.choices, result.err
	case <-ctx.Done():
		return core.ProfileConfig{}, errReviewProbeTimedOut
	}
}

// AutoconfigureWithEfforts computes the ordered legacy Auto preset using the
// same model and effort discovery for the daemon upgrade and the startup
// review fallback. The map records, per drafted model, whether its efforts
// are known.
func AutoconfigureWithEfforts(ctx context.Context, available []AvailableProfile, usagePriced map[string]bool, probe ReviewerProbe) (core.Autoconfigured, map[string]bool) {
	offers := reviewModelOffers(available, usagePriced)
	drafted, _ := core.Autoconfigure(offers, func(string) ([]core.SessionConfigChoice, bool) { return nil, false })
	draft := drafted.Reviewers
	probeCtx, cancel := context.WithDeadline(ctx, time.Now().Add(10*time.Second))
	defer cancel()
	all := reviewAllModels(available)
	type pair struct {
		model   string
		profile string
		offered *core.ProfileConfig
		value   string
	}
	models := []string{}
	for _, choice := range draft {
		if !slices.Contains(models, choice.Model) {
			models = append(models, choice.Model)
		}
	}
	sort.Strings(models)
	pairs := []pair{}
	for _, model := range models {
		identity := core.ModelID(all, model)
		for index := range available {
			offered := &available[index].Choices
			if offer, ok := core.Offer(offered.Models, identity, model); ok {
				pairs = append(pairs, pair{model: model, profile: available[index].ID, offered: offered, value: offer.Value})
			}
		}
	}
	type discovery struct {
		efforts []core.SessionConfigChoice
		known   bool
	}
	discovered := make([]discovery, len(pairs))
	var wait sync.WaitGroup
	for index, candidate := range pairs {
		wait.Go(func() {
			if candidate.offered.Model != nil && *candidate.offered.Model == candidate.value {
				discovered[index] = discovery{efforts: slices.Clone(candidate.offered.Efforts), known: true}
				return
			}
			choices, err := reviewRunProbe(probeCtx, probe, candidate.profile, candidate.value)
			switch {
			case err == errReviewProbeTimedOut && probeCtx.Err() != nil:
				slog.Warn("reviewer effort discovery timed out during legacy Auto configuration",
					"profile", candidate.profile, "model", candidate.model)
			case err != nil:
				slog.Warn("reviewer effort discovery failed during legacy Auto configuration",
					"profile", candidate.profile, "model", candidate.model, "error", err)
			case !slices.ContainsFunc(choices.Models, func(choice core.SessionConfigChoice) bool { return choice.Value == candidate.value }):
				slog.Warn("model-specific reviewer discovery omitted the requested model",
					"profile", candidate.profile, "model", candidate.model)
			default:
				discovered[index] = discovery{efforts: choices.Efforts, known: true}
			}
		})
	}
	wait.Wait()
	efforts := map[string][]core.SessionConfigChoice{}
	known := map[string]bool{}
	for index, candidate := range pairs {
		if _, seen := known[candidate.model]; !seen {
			known[candidate.model] = true
		}
		result := discovered[index]
		if !result.known {
			known[candidate.model] = false
			continue
		}
		union := efforts[candidate.model]
		if union == nil {
			union = []core.SessionConfigChoice{}
		}
		for _, item := range result.efforts {
			if !slices.ContainsFunc(union, func(existing core.SessionConfigChoice) bool { return existing.Value == item.Value }) {
				union = append(union, item)
			}
		}
		efforts[candidate.model] = union
	}
	choices, _ := core.Autoconfigure(offers, func(model string) ([]core.SessionConfigChoice, bool) {
		offered, hasOffered := efforts[model]
		if known[model] {
			if !hasOffered {
				return []core.SessionConfigChoice{}, true
			}
			return slices.Clone(offered), true
		}
		if !hasOffered {
			return nil, false
		}
		for _, choice := range draft {
			if choice.Model != model {
				continue
			}
			if choice.Effort == nil || !slices.ContainsFunc(offered, func(item core.SessionConfigChoice) bool { return item.Value == *choice.Effort }) {
				return nil, false
			}
		}
		return slices.Clone(offered), true
	})
	for _, adjustment := range choices.Adjusted {
		slog.Warn("legacy reviewer preset effort adjusted",
			"model", adjustment.Model, "requested", adjustment.Requested, "used", adjustment.Used)
	}
	for _, choice := range choices.Reviewers {
		if choice.Effort == nil {
			continue
		}
		if items, ok := efforts[choice.Model]; ok && slices.ContainsFunc(items, func(item core.SessionConfigChoice) bool { return item.Value == *choice.Effort }) {
			known[choice.Model] = true
		}
	}
	return choices, known
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
