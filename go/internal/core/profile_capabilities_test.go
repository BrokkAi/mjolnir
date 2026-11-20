// Port of the tests in mj-core/src/profile_capabilities.rs.

package core

import (
	"slices"
	"strings"
	"testing"
)

// Hard-won: #1534: settings ignored equivalent harness IDs when collecting efforts.
func TestAliasEffortsUseProfileLaunchKeysAndExcludeStaleVersions(t *testing.T) {
	recorded := loadRecordedModels(t, "testdata/profile_capabilities/claude-antigravity-models.json")
	config := DefaultConfig()
	if config.Profiles == nil {
		config.Profiles = map[string]HarnessProfile{}
	}
	snapshot := ProfileCapabilitiesSnapshot{Profiles: map[string]ProfileCapabilities{}}
	cases := []struct {
		id     string
		kind   HarnessKind
		models []SessionConfigChoice
		launch string
		state  CapabilityState[[]SessionConfigChoice]
	}{
		{"claude", HarnessKindClaude, slices.Clone(recorded["claude"]), "sonnet", CapabilityState[[]SessionConfigChoice]{}},
		{"gemini", HarnessKindAntigravity, slices.Clone(recorded["antigravity"]), "claude-sonnet-5-5",
			CapabilityState[[]SessionConfigChoice]{Kind: CapabilityStateReady, Value: []SessionConfigChoice{{Value: "high", Name: "High"}}}},
		{"a-stale", HarnessKindClaude, slices.Clone(recorded["claude"]), "sonnet", CapabilityState[[]SessionConfigChoice]{}},
	}
	for _, c := range cases {
		if c.id == "a-stale" {
			c.models[1].Name = "Sonnet 5"
		}
		profile := NewHarnessProfile(c.kind, "/profiles/"+c.id)
		snapshot.Profiles[profile.CapabilitiesKey(c.id)] = ProfileCapabilities{
			Choices: CapabilityState[ProfileConfig]{Kind: CapabilityStateReady, Value: ProfileConfig{
				Models:  c.models,
				Efforts: []SessionConfigChoice{},
			}},
			Efforts: map[string]CapabilityState[[]SessionConfigChoice]{c.launch: c.state},
		}
		config.Profiles[c.id] = profile
	}
	if got := snapshot.ModelID(&config, "sonnet"); got != "claude-sonnet-5-5" {
		t.Fatalf("model id %q", got)
	}
	sonnet, high := "sonnet", "high"
	options := snapshot.Options(&config, &sonnet)
	if options == nil {
		t.Fatal("no options")
	}
	if err := options.Validate(&SubagentChoice{Model: "sonnet", Effort: &high}); err != nil {
		t.Fatal(err)
	}
	if got := snapshot.EffortBlockers(&config, "sonnet"); !slices.Equal(got, []string{"claude: effort discovery pending"}) {
		t.Fatalf("blockers %q", got)
	}
	setEnabled := func(id string, enabled bool) {
		profile := config.Profiles[id]
		profile.Enabled = enabled
		config.Profiles[id] = profile
	}
	setEnabled("claude", false)
	setEnabled("a-stale", false)
	options = snapshot.Options(&config, &sonnet)
	if options == nil {
		t.Fatal("no options")
	}
	if err := options.Validate(&SubagentChoice{Model: "sonnet", Effort: &high}); err != nil {
		t.Fatal(err)
	}
	// The older alias sorts first; without identity-preserving merging
	// it would hide the newer profile when Gemini is disabled.
	setEnabled("claude", true)
	setEnabled("a-stale", true)
	setEnabled("gemini", false)
	explicit := "claude-sonnet-5-5"
	options = snapshot.Options(&config, &explicit)
	if options == nil {
		t.Fatal("no options")
	}
	if err := options.Validate(&SubagentChoice{Model: "claude-sonnet-5-5"}); err != nil {
		t.Fatal(err)
	}
	if got := ModelID(options.Models, "sonnet"); got != "claude-sonnet-5-5" {
		t.Fatalf("merged model id %q", got)
	}
}

func profileCapabilitiesFixture() (Config, ProfileCapabilitiesSnapshot) {
	profile := NewHarnessProfile(HarnessKindCodex, "/profiles/parent")
	choice := func(value string) SessionConfigChoice {
		return SessionConfigChoice{Value: value, Name: value}
	}
	key := profile.CapabilitiesKey("parent")
	config := DefaultConfig()
	config.Profiles = map[string]HarnessProfile{"parent": profile}
	model := "a"
	snapshot := ProfileCapabilitiesSnapshot{Profiles: map[string]ProfileCapabilities{
		key: {
			Choices: CapabilityState[ProfileConfig]{Kind: CapabilityStateReady, Value: ProfileConfig{
				Model:      &model,
				Models:     []SessionConfigChoice{choice("a"), choice("b")},
				Efforts:    []SessionConfigChoice{},
				ObservedAt: 1,
			}},
			Efforts: map[string]CapabilityState[[]SessionConfigChoice]{
				"a": {Kind: CapabilityStateReady, Value: []SessionConfigChoice{}},
				"b": {Kind: CapabilityStatePending},
			},
		},
	}}
	return config, snapshot
}

func TestDefaultsAndEligibilityChangeViewsWithoutChangingProfileIdentity(t *testing.T) {
	config, snapshot := profileCapabilitiesFixture()
	original := config.ProfilesDiscoveryKey()

	config.Subagents.MaxConcurrent = 99
	profile := config.Profiles["parent"]
	contextWindow := 42
	profile.ContextWindowBytes = &contextWindow
	config.Profiles["parent"] = profile
	if config.ProfilesDiscoveryKey() != original {
		t.Fatal("discovery key changed")
	}
	a := "a"
	if snapshot.Options(&config, &a) == nil {
		t.Fatal("no options")
	}
	profile = config.Profiles["parent"]
	profile.Home = "/another/home"
	config.Profiles["parent"] = profile
	if config.ProfilesDiscoveryKey() == original {
		t.Fatal("discovery key unchanged")
	}
	if snapshot.Options(&config, nil) != nil {
		t.Fatal("options for a changed profile")
	}
}

func TestCompletedProfileRemainsUsableWhileAnotherProfileIsPending(t *testing.T) {
	config, snapshot := profileCapabilitiesFixture()
	config.Profiles["other"] = config.Profiles["parent"]
	models := snapshot.Options(&config, nil)
	if models == nil {
		t.Fatal("no options")
	}
	if !slices.ContainsFunc(models.Models, func(choice SessionConfigChoice) bool { return choice.Value == "a" }) {
		t.Fatalf("models %+v", models.Models)
	}
	pending := func(reason string) bool { return strings.Contains(reason, "other: discovery pending") }
	if !slices.ContainsFunc(models.Unavailable, pending) {
		t.Fatalf("unavailable %q", models.Unavailable)
	}
	a := "a"
	efforts := snapshot.Options(&config, &a)
	if efforts == nil || !slices.ContainsFunc(efforts.Unavailable, pending) {
		t.Fatalf("efforts %+v", efforts)
	}
}
