// Port of the tests in mj-core/src/review/autoconfigure.rs.

package core

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"testing"
)

type autoconfigureRow struct {
	model  string
	effort string
}

func autoconfigureNoEfforts(string) ([]SessionConfigChoice, bool) { return nil, false }

// Hard-won: #1534: alias spellings hid shared capacity and could double-count a profile.
func TestModelOffersCountEachProfileOncePerIdentityAcrossHarnessAliases(t *testing.T) {
	data, err := os.ReadFile(filepath.Join("testdata", "review_autoconfigure", "claude-antigravity-models.json"))
	if err != nil {
		t.Fatal(err)
	}
	var recorded map[string][]SessionConfigChoice
	if err := json.Unmarshal(data, &recorded); err != nil {
		t.Fatal(err)
	}
	claude := append(slices.Clone(recorded["claude"]), recorded["antigravity"][2])
	offers := ModelOffers([]AutoconfigureProfileModels{
		{Models: claude},
		{Models: recorded["antigravity"]},
	})
	find := func(offers []AutoconfigureModelOffer, identity string) AutoconfigureModelOffer {
		index := slices.IndexFunc(offers, func(offer AutoconfigureModelOffer) bool { return offer.Identity == identity })
		if index < 0 {
			t.Fatalf("no offer for %s", identity)
		}
		return offers[index]
	}
	if sonnet := find(offers, "claude-sonnet-5-5"); sonnet.Profiles != 2 || sonnet.SubscriptionProfiles != 2 {
		t.Fatalf("sonnet = %+v", sonnet)
	}
	if got := find(offers, "claude-sonnet-5").Profiles; got != 1 {
		t.Fatalf("claude-sonnet-5 profiles = %d", got)
	}
	// An older, more widely offered alias must remain pinned to that version
	// when Autoconfigure prefers its larger subscription pool.
	stale := slices.Clone(recorded["claude"])
	stale[1].Name = "Sonnet 5"
	offers = ModelOffers([]AutoconfigureProfileModels{{Models: stale}, {Models: stale}, {Models: claude}})
	configured, ok := Autoconfigure(offers, autoconfigureNoEfforts)
	if !ok || configured.Reviewers[0].Model != "claude-sonnet-5" {
		t.Fatalf("configured = %+v, %v", configured, ok)
	}
}

func autoconfigureChoices(ids ...string) []SessionConfigChoice {
	choices := make([]SessionConfigChoice, 0, len(ids))
	for _, id := range ids {
		choices = append(choices, SessionConfigChoice{Value: id, Name: id})
	}
	return choices
}

func autoconfigureModels(result Autoconfigured, ok bool) []autoconfigureRow {
	rows := []autoconfigureRow{}
	if !ok {
		return rows
	}
	for _, choice := range result.Reviewers {
		rows = append(rows, autoconfigureRow{choice.Model, *choice.Effort})
	}
	return rows
}

func TestKnownEmptySonnetEffortsUseHarnessDefaultButUnknownKeepsTableEffort(t *testing.T) {
	models := autoconfigureChoices("claude-sonnet-4.5")
	known, ok := Autoconfigure(MergedModelOffers(models), func(string) ([]SessionConfigChoice, bool) { return []SessionConfigChoice{}, true })
	if !ok {
		t.Fatal("no answer for known efforts")
	}
	unknown, ok := Autoconfigure(MergedModelOffers(models), autoconfigureNoEfforts)
	if !ok {
		t.Fatal("no answer for unknown efforts")
	}
	for _, choice := range known.Reviewers {
		if choice.Effort != nil {
			t.Fatalf("known effort = %q", *choice.Effort)
		}
	}
	for _, choice := range unknown.Reviewers {
		if choice.Effort == nil || *choice.Effort != "high" {
			t.Fatalf("unknown effort = %v", choice.Effort)
		}
	}
}

func TestKnownEffortsChooseNearestLowerThenLowestAndReportAdjustments(t *testing.T) {
	models := autoconfigureChoices("gpt-6-terra", "gpt-6-sol")
	configured, ok := Autoconfigure(MergedModelOffers(models), func(model string) ([]SessionConfigChoice, bool) {
		if len(model) >= 5 && model[len(model)-5:] == "terra" {
			return autoconfigureChoices("low", "max"), true
		}
		return autoconfigureChoices("high", "xhigh"), true
	})
	if !ok {
		t.Fatal("no answer")
	}
	effort := func(index int) string {
		if configured.Reviewers[index].Effort == nil {
			return "<none>"
		}
		return *configured.Reviewers[index].Effort
	}
	if effort(0) != "low" || effort(1) != "high" {
		t.Fatalf("efforts = %s, %s", effort(0), effort(1))
	}
	if len(configured.Adjusted) != 2 {
		t.Fatalf("adjusted = %+v", configured.Adjusted)
	}
	if adjusted := configured.Adjusted[0]; adjusted.Requested != "high" || adjusted.Used == nil || *adjusted.Used != "low" {
		t.Fatalf("adjusted[0] = %+v", adjusted)
	}
	if adjusted := configured.Adjusted[1]; adjusted.Requested != "medium" || adjusted.Used == nil || *adjusted.Used != "high" {
		t.Fatalf("adjusted[1] = %+v", adjusted)
	}
}

// Hard-won: ed508ae8c: stripping Kimi Code's provider selected an older Kimi model.
// Hard-won: 9541bf079: K3's 256k context suffix outranked the newer K3.1 Pro release.
func TestAutoconfigureUsesFirstMatchingRowAndNewestFamilyVersions(t *testing.T) {
	cases := []struct {
		ids      []string
		expected []autoconfigureRow
	}{
		{
			[]string{"gpt-6-terra", "gpt-6.10-terra", "gpt-6.1-sol", "claude-sonnet-4.5", "grok-4.6"},
			[]autoconfigureRow{{"gpt-6.10-terra", "high"}, {"gpt-6.1-sol", "medium"}},
		},
		{
			[]string{"gpt-6-terra", "gpt-6-sol", "grok-4.6"},
			[]autoconfigureRow{{"gpt-6-terra", "high"}, {"gpt-6-sol", "medium"}},
		},
		{
			[]string{"claude-sonnet-4.5", "deepseek-v4-pro"},
			[]autoconfigureRow{{"claude-sonnet-4.5", "high"}, {"claude-sonnet-4.5", "high"}},
		},
		{
			[]string{"deepseek-v4-pro", "deepseek-v4-flash", "deepseek-v4-flash-max"},
			[]autoconfigureRow{{"deepseek-v4-pro", "high"}, {"deepseek-v4-pro", "high"}},
		},
		{
			[]string{"grok-4.6", "kimi-code/k3.1-pro", "grok-4.20-multi-agent-0309"},
			[]autoconfigureRow{{"grok-4.6", "high"}, {"grok-4.6", "high"}},
		},
		{
			// The live Kimi Code catalog, including an older model with a
			// larger context and the same-version Highspeed variant.
			[]string{
				"kimi-code/kimi-for-coding",
				"kimi-code/kimi-for-coding-highspeed",
				"kimi-code/k3",
				"kimi-code/k3.1-pro",
				"kimi-code/k3-256k",
				"kimi-code/k3.1-pro-highspeed",
			},
			[]autoconfigureRow{{"kimi-code/k3.1-pro", "high"}, {"kimi-code/k3.1-pro", "high"}},
		},
		{
			[]string{"unrelated-model", "kimi-k3"},
			[]autoconfigureRow{{"kimi-k3", "high"}, {"kimi-k3", "high"}},
		},
		{
			[]string{"kimi-k2.7-code", "kimi-code/k3"},
			[]autoconfigureRow{{"kimi-code/k3", "high"}, {"kimi-code/k3", "high"}},
		},
	}
	for _, c := range cases {
		got := autoconfigureModels(Autoconfigure(MergedModelOffers(autoconfigureChoices(c.ids...)), autoconfigureNoEfforts))
		if !reflect.DeepEqual(got, c.expected) {
			t.Fatalf("%q: got %v, want %v", c.ids, got, c.expected)
		}
	}
}

type autoconfigureProfile struct {
	usagePriced bool
	ids         []string
}

// autoconfigurePick runs Autoconfigure over (usage_priced, models) profiles.
func autoconfigurePick(profiles ...autoconfigureProfile) []autoconfigureRow {
	lists := make([]AutoconfigureProfileModels, 0, len(profiles))
	for _, profile := range profiles {
		lists = append(lists, AutoconfigureProfileModels{Models: autoconfigureChoices(profile.ids...), UsagePriced: profile.usagePriced})
	}
	return autoconfigureModels(Autoconfigure(ModelOffers(lists), autoconfigureNoEfforts))
}

type autoconfigureNamedProfile struct {
	usagePriced bool
	items       [][2]string
}

// autoconfigurePickNamed runs Autoconfigure over named (usage_priced,
// [(value, name)]) profiles.
func autoconfigurePickNamed(profiles ...autoconfigureNamedProfile) []autoconfigureRow {
	lists := make([]AutoconfigureProfileModels, 0, len(profiles))
	for _, profile := range profiles {
		models := make([]SessionConfigChoice, 0, len(profile.items))
		for _, item := range profile.items {
			models = append(models, SessionConfigChoice{Value: item[0], Name: item[1]})
		}
		lists = append(lists, AutoconfigureProfileModels{Models: models, UsagePriced: profile.usagePriced})
	}
	return autoconfigureModels(Autoconfigure(ModelOffers(lists), autoconfigureNoEfforts))
}

var autoconfigureCodex = [][2]string{
	{"gpt-6.1-terra", "gpt-6.1-terra"},
	{"gpt-6.1-sol", "gpt-6.1-sol"},
	{"gpt-6-sol", "gpt-6-sol"},
	{"gpt-5.6-terra", "gpt-5.6-terra"},
}

var autoconfigureClaude = [][2]string{
	{"sonnet", "Sonnet 4.5"},
	{"claude-sonnet-4-5-20251001", "claude-sonnet-4-5-20251001"},
}

var autoconfigureOpenRouter = [][2]string{
	{"openrouter/~openai/gpt-terra-latest", "GPT Terra (latest)"},
	{"openrouter/~openai/gpt-sol-latest", "GPT Sol (latest)"},
	{"openrouter/~anthropic/claude-sonnet-latest", "Claude Sonnet (latest)"},
	{"openrouter/anthropic/claude-sonnet-5.5", "Claude Sonnet 5.5"},
	{"openrouter/openai/gpt-5.6-terra", "GPT 5.6 Terra"},
}

func autoconfigureExpect(t *testing.T, got, want []autoconfigureRow) {
	t.Helper()
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v, want %v", got, want)
	}
}

func TestSubscriptionModelsBeatNewerLatestAliasesFromAUsagePricedProfile(t *testing.T) {
	expected := []autoconfigureRow{{"gpt-6.1-terra", "high"}, {"gpt-6.1-sol", "medium"}}
	// The live catalog: four codex and six claude profiles, one OpenRouter profile.
	profiles := []autoconfigureNamedProfile{{true, autoconfigureOpenRouter}}
	for range 4 {
		profiles = append(profiles, autoconfigureNamedProfile{false, autoconfigureCodex})
	}
	for range 6 {
		profiles = append(profiles, autoconfigureNamedProfile{false, autoconfigureClaude})
	}
	autoconfigureExpect(t, autoconfigurePickNamed(profiles...), expected)
	slices.Reverse(profiles)
	autoconfigureExpect(t, autoconfigurePickNamed(profiles...), expected)
	// One profile each ties the profile counts, so only the subscription tier
	// keeps the newer `latest` aliases and Sonnet 5.5 from winning.
	autoconfigureExpect(t, autoconfigurePickNamed(
		autoconfigureNamedProfile{true, autoconfigureOpenRouter},
		autoconfigureNamedProfile{false, autoconfigureCodex},
		autoconfigureNamedProfile{false, autoconfigureClaude},
	), expected)
}

func TestAnUndatedAliasBeatsItsOwnDatedSnapshot(t *testing.T) {
	got := autoconfigurePickNamed(autoconfigureNamedProfile{false, [][2]string{
		{"claude-sonnet-4-5-20251001", "claude-sonnet-4-5-20251001"},
		{"sonnet", "Sonnet 4.5"},
	}})
	if len(got) == 0 || got[0].model != "sonnet" {
		t.Fatalf("got %v", got)
	}
}

func TestUsagePricedModelsAreUsedWhenNoSubscriptionOffersTheFamily(t *testing.T) {
	autoconfigureExpect(t, autoconfigurePick(
		autoconfigureProfile{false, []string{"grok-4.6"}},
		autoconfigureProfile{true, []string{"claude-sonnet-4.5", "gpt-6.1-terra"}},
	), []autoconfigureRow{{"claude-sonnet-4.5", "high"}, {"claude-sonnet-4.5", "high"}})
}

func TestMoreOfferingProfilesBeatANewerVersionWithinSubscriptions(t *testing.T) {
	autoconfigureExpect(t, autoconfigurePick(
		autoconfigureProfile{false, []string{"gpt-6-terra", "gpt-6-sol", "claude-sonnet-4.5"}},
		autoconfigureProfile{false, []string{"gpt-6-terra", "gpt-6.1-sol", "claude-sonnet-4.5"}},
		autoconfigureProfile{false, []string{"gpt-6.1-terra", "gpt-6-sol", "claude-sonnet-5"}},
	), []autoconfigureRow{{"gpt-6-terra", "high"}, {"gpt-6-sol", "medium"}})
}

func TestNewestVersionDecidesBetweenEquallyOfferedSubscriptionModels(t *testing.T) {
	autoconfigureExpect(t, autoconfigurePick(
		autoconfigureProfile{false, []string{"gpt-6-terra", "gpt-6.1-terra", "gpt-6-sol"}},
		autoconfigureProfile{false, []string{"gpt-6-terra", "gpt-6.1-terra", "gpt-6-sol"}},
	), []autoconfigureRow{{"gpt-6.1-terra", "high"}, {"gpt-6-sol", "medium"}})
}

func TestUsagePricedProfilesDoNotRaiseASubscriptionModelsCount(t *testing.T) {
	// Three usage-priced profiles offer the other value; one subscription
	// offers this one. Subscription wins before counts are compared.
	autoconfigureExpect(t, autoconfigurePick(
		autoconfigureProfile{false, []string{"kimi-code/k3"}},
		autoconfigureProfile{true, []string{"kimi-code/k3.1-pro"}},
		autoconfigureProfile{true, []string{"kimi-code/k3.1-pro"}},
		autoconfigureProfile{true, []string{"kimi-code/k3.1-pro"}},
	), []autoconfigureRow{{"kimi-code/k3", "high"}, {"kimi-code/k3", "high"}})
}

func TestFlagshipProBeatsFlashEvenWhenFlashIsSubscribedOrMoreOffered(t *testing.T) {
	pro := autoconfigureRow{"deepseek-v4-pro", "high"}
	for _, reversed := range []bool{false, true} {
		// Only a usage-priced profile offers Pro; a subscription offers
		// Flash Max.
		profiles := []autoconfigureProfile{
			{true, []string{"deepseek-v4-pro"}},
			{false, []string{"deepseek-v4-flash-max"}},
		}
		if reversed {
			slices.Reverse(profiles)
		}
		autoconfigureExpect(t, autoconfigurePick(profiles...), []autoconfigureRow{pro, pro})
		// Three profiles offer Flash Max, one offers Pro.
		profiles = []autoconfigureProfile{
			{false, []string{"deepseek-v4-pro"}},
			{false, []string{"deepseek-v4-flash-max"}},
			{false, []string{"deepseek-v4-flash-max"}},
			{false, []string{"deepseek-v4-flash-max"}},
		}
		if reversed {
			slices.Reverse(profiles)
		}
		autoconfigureExpect(t, autoconfigurePick(profiles...), []autoconfigureRow{pro, pro})
	}
}

func TestAValueListedTwiceByOneProfileCountsOnce(t *testing.T) {
	offers := ModelOffers([]AutoconfigureProfileModels{{Models: autoconfigureChoices("a", "a")}})
	if len(offers) != 1 {
		t.Fatalf("offers = %+v", offers)
	}
	if offers[0].Profiles != 1 || offers[0].SubscriptionProfiles != 1 {
		t.Fatalf("offer = %+v", offers[0])
	}
}
