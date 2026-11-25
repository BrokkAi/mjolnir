// Port of the tests in mj-accounts/src/review_selection.rs.

package accounts

import (
	"context"
	"errors"
	"fmt"
	"maps"
	"slices"
	"testing"
	"testing/synctest"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func reviewChoice(value, name string) core.SessionConfigChoice {
	return core.SessionConfigChoice{Value: value, Name: name}
}

func reviewUnreachableProbe(context.Context, string, string) (core.ProfileConfig, error) {
	panic("unreachable")
}

func TestLegacyNamedProfileUsesItsDefaultModelAndValidatesEffort(t *testing.T) {
	count := 3
	legacy := core.LegacyReview{Count: &count, Profile: schedulerText("claude"), Effort: schedulerText("xhigh")}
	available := []AvailableProfile{{
		ID: "claude",
		Choices: core.ProfileConfig{
			Model:      schedulerText("haiku"),
			Models:     []core.SessionConfigChoice{reviewChoice("haiku", "Haiku")},
			Efforts:    []core.SessionConfigChoice{reviewChoice("high", "High")},
			ObservedAt: 1,
		},
	}}
	choices, known, adjusted := UpgradeReviewers(context.Background(), &legacy, available, map[string]bool{}, reviewUnreachableProbe)
	if !known || len(choices) != 3 || len(adjusted) != 1 {
		t.Fatalf("%v %v %v", choices, known, adjusted)
	}
	if adjusted[0].Requested != "xhigh" || adjusted[0].Used == nil || *adjusted[0].Used != "high" {
		t.Fatalf("%+v", adjusted[0])
	}
	for _, choice := range choices {
		if choice.Model != "haiku" || choice.Effort == nil || *choice.Effort != "high" {
			t.Fatalf("%+v", choice)
		}
	}
}

func TestLegacyNamedProfileWithUnknownEffortsCannotPersist(t *testing.T) {
	legacy := core.LegacyReview{Profile: schedulerText("claude"), Effort: schedulerText("high")}
	available := []AvailableProfile{{
		ID: "claude",
		Choices: core.ProfileConfig{
			Models:     []core.SessionConfigChoice{reviewChoice("haiku", "Haiku")},
			Efforts:    []core.SessionConfigChoice{},
			ObservedAt: 1,
		},
	}}
	choices, known, _ := UpgradeReviewers(context.Background(), &legacy, available, map[string]bool{},
		func(context.Context, string, string) (core.ProfileConfig, error) {
			return core.ProfileConfig{}, errors.New("probe unavailable")
		})
	if known || len(choices) != 2 {
		t.Fatalf("%v %v", choices, known)
	}
	for _, choice := range choices {
		if choice.Model != "haiku" || choice.Effort == nil || *choice.Effort != "high" {
			t.Fatalf("%+v", choice)
		}
	}
}

func TestLegacyAutoKeepsEffortOfferedByAnyMatchingProfile(t *testing.T) {
	model := reviewChoice("claude-sonnet-4.5", "Sonnet")
	profile := func(efforts []core.SessionConfigChoice) core.ProfileConfig {
		return core.ProfileConfig{
			Model:      schedulerText(model.Value),
			Models:     []core.SessionConfigChoice{model},
			Efforts:    efforts,
			ObservedAt: 1,
		}
	}
	available := []AvailableProfile{
		{ID: "no-effort", Choices: profile([]core.SessionConfigChoice{})},
		{ID: "high-effort", Choices: profile([]core.SessionConfigChoice{reviewChoice("xhigh", "Extra high")})},
	}
	choices, known := AutoconfigureWithEfforts(context.Background(), available, map[string]bool{}, reviewUnreachableProbe)
	for _, modelKnown := range known {
		if !modelKnown {
			t.Fatalf("%v", known)
		}
	}
	for _, choice := range choices.Reviewers {
		if choice.Effort == nil || *choice.Effort != "xhigh" {
			t.Fatalf("%+v", choice)
		}
	}
}

func TestLegacyAutoKnownEmptyEffortsUseDefaultAndRemainPersistable(t *testing.T) {
	model := reviewChoice("claude-sonnet-4.5", "Sonnet")
	available := []AvailableProfile{{
		ID: "sonnet",
		Choices: core.ProfileConfig{
			Model:      schedulerText(model.Value),
			Models:     []core.SessionConfigChoice{model},
			Efforts:    []core.SessionConfigChoice{},
			ObservedAt: 1,
		},
	}}
	configured, known := AutoconfigureWithEfforts(context.Background(), available, map[string]bool{}, reviewUnreachableProbe)
	if modelKnown, ok := known["claude-sonnet-4.5"]; !ok || !modelKnown {
		t.Fatalf("%v", known)
	}
	for _, choice := range configured.Reviewers {
		if choice.Effort != nil {
			t.Fatalf("%+v", choice)
		}
	}
	if len(configured.Adjusted) != 2 {
		t.Fatalf("%+v", configured.Adjusted)
	}
}

func catalogProfile(models ...string) core.ProfileConfig {
	choices := []core.SessionConfigChoice{}
	for _, value := range models {
		name := value
		if value == "sonnet" {
			name = "Sonnet 4.5"
		}
		choices = append(choices, reviewChoice(value, name))
	}
	return core.ProfileConfig{Models: choices, Efforts: []core.SessionConfigChoice{}, ObservedAt: 1}
}

// liveCatalog is four codex and six claude subscriptions plus one
// usage-priced OpenCode profile that advertises the OpenRouter catalogue.
func liveCatalog() []AvailableProfile {
	codex := catalogProfile("gpt-6.1-terra", "gpt-6.1-sol", "gpt-6-sol", "gpt-5.6-terra")
	claude := catalogProfile("sonnet", "claude-sonnet-4-5-20251001")
	openrouter := catalogProfile(
		"openrouter/~openai/gpt-terra-latest",
		"openrouter/~openai/gpt-sol-latest",
		"openrouter/~anthropic/claude-sonnet-latest",
		"openrouter/anthropic/claude-sonnet-5.5",
		"openrouter/openai/gpt-5.6-terra",
	)
	available := []AvailableProfile{{ID: "openrouter", Choices: openrouter}}
	for n := 1; n <= 4; n++ {
		available = append(available, AvailableProfile{ID: fmt.Sprintf("codex%d", n), Choices: codex})
	}
	for n := 1; n <= 6; n++ {
		available = append(available, AvailableProfile{ID: fmt.Sprintf("claude%d", n), Choices: claude})
	}
	return available
}

// reviewPicked is each reviewer's model and effort ("" for none).
func reviewPicked(configured core.Autoconfigured) [][2]string {
	picked := [][2]string{}
	for _, choice := range configured.Reviewers {
		effort := ""
		if choice.Effort != nil {
			effort = *choice.Effort
		}
		picked = append(picked, [2]string{choice.Model, effort})
	}
	return picked
}

func reviewSet(names ...string) map[string]bool {
	set := map[string]bool{}
	for _, name := range names {
		set[name] = true
	}
	return set
}

func TestOnlyUnclassifiedProfilesOfferingADraftedFamilyBlockTheUpgrade(t *testing.T) {
	legacy := core.LegacyReview{}
	available := append(liveCatalog(), AvailableProfile{ID: "other", Choices: catalogProfile("some-unrelated-model")})
	usagePriced := reviewSet("openrouter")
	blocked := func(names ...string) []string {
		return UnclassifiedInDraft(&legacy, available, usagePriced, reviewSet(names...))
	}
	if got := blocked(); len(got) != 0 {
		t.Fatalf("%v", got)
	}
	// Outside every drafted family: nothing to guess about.
	if got := blocked("other"); len(got) != 0 {
		t.Fatalf("%v", got)
	}
	if got := blocked("other", "claude3", "codex2"); !slices.Equal(got, []string{"codex2"}) {
		t.Fatalf("%v", got)
	}
	// Generic providers retain separate families: an unrelated provider's
	// unknown billing cannot delay the selected flagship's upgrade.
	generic := []AvailableProfile{
		{ID: "gemini", Choices: catalogProfile("google/gemini-3-pro", "google/gemini-3.5-flash")},
		{ID: "glm", Choices: catalogProfile("z-ai/glm-5.3")},
	}
	if got := UnclassifiedInDraft(&legacy, generic, map[string]bool{}, reviewSet("gemini", "glm")); !slices.Equal(got, []string{"gemini"}) {
		t.Fatalf("%v", got)
	}
	// A pinned profile decides without Autoconfigure.
	pinned := core.LegacyReview{Profile: schedulerText("codex1")}
	if got := UnclassifiedInDraft(&pinned, available, usagePriced, reviewSet("claude3")); len(got) != 0 {
		t.Fatalf("%v", got)
	}
}

func TestCandidatesWithoutAReportAreUsagePricedOnlyWhenTheirHarnessSaysSo(t *testing.T) {
	candidate := func(id string, harness core.HarnessKind, quota CandidateQuota) SubagentCandidate {
		return SubagentCandidate{
			ProfileID: id,
			Harness:   harness,
			Choices:   catalogProfile("sonnet"),
			Quota:     quota,
			Unhealthy: map[string]Unhealthy{},
		}
	}
	candidates := SubagentCandidates{
		Offered: []SubagentCandidate{
			candidate("opencode", core.HarnessKindOpenCode, CandidateQuota{Kind: CandidateQuotaUnknown}),
			candidate("dsh", core.HarnessKindDsh, CandidateQuota{Kind: CandidateQuotaUnknown}),
			candidate("claude", core.HarnessKindClaude, CandidateQuota{Kind: CandidateQuotaUnknown}),
			candidate("api", core.HarnessKindCodex, CandidateQuota{Kind: CandidateQuotaUsagePriced}),
			candidate("codex", core.HarnessKindCodex, CandidateQuota{Kind: CandidateQuotaSubscription, Windows: nil}),
		},
	}
	if got := UsagePricedProfiles(&candidates); !maps.Equal(got, reviewSet("opencode", "dsh", "api")) {
		t.Fatalf("%v", got)
	}
}

func TestUsagePricedOpenrouterModelsDoNotDisplaceSubscriptionReviewers(t *testing.T) {
	available := liveCatalog()
	configured, _ := AutoconfigureWithEfforts(context.Background(), available, reviewSet("openrouter"),
		func(_ context.Context, _, model string) (core.ProfileConfig, error) {
			return catalogProfile(model), nil
		})
	if got := reviewPicked(configured); !slices.Equal(got, [][2]string{{"gpt-6.1-terra", ""}, {"gpt-6.1-sol", ""}}) {
		t.Fatalf("%v", got)
	}
}

func TestAutoEffortProbesShareOneDeadline(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		model := reviewChoice("claude-sonnet-4.5", "Sonnet")
		offered := core.ProfileConfig{
			Models:     []core.SessionConfigChoice{model},
			Efforts:    []core.SessionConfigChoice{},
			ObservedAt: 1,
		}
		available := []AvailableProfile{{ID: "first", Choices: offered}, {ID: "second", Choices: offered}}
		start := time.Now()
		choices, known := AutoconfigureWithEfforts(context.Background(), available, map[string]bool{},
			func(ctx context.Context, _, _ string) (core.ProfileConfig, error) {
				// Never answers on its own (Rust: `std::future::pending`).
				<-ctx.Done()
				return core.ProfileConfig{}, ctx.Err()
			})
		if len(choices.Reviewers) != 2 {
			t.Fatalf("%+v", choices)
		}
		if modelKnown, ok := known["claude-sonnet-4.5"]; !ok || modelKnown {
			t.Fatalf("%v", known)
		}
		if elapsed := time.Since(start); elapsed != 10*time.Second {
			t.Fatalf("elapsed %v", elapsed)
		}
	})
}
