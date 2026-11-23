// Port of mj-tui/src/review_settings/tests.rs.

package tui

import (
	"encoding/json"
	"maps"
	"slices"
	"strings"
	"testing"
	"time"
	"unicode"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func reviewTestChoice(value string) core.SessionConfigChoice {
	return core.SessionConfigChoice{Value: value, Name: value}
}

func reviewTestReady(choices ...core.SessionConfigChoice) core.CapabilityState[[]core.SessionConfigChoice] {
	if choices == nil {
		choices = []core.SessionConfigChoice{}
	}
	return core.CapabilityState[[]core.SessionConfigChoice]{Kind: core.CapabilityStateReady, Value: choices}
}

// reviewTestCatalog is a capability snapshot in which every enabled profile
// offers models, each with medium, high and xhigh efforts.
func reviewTestCatalog(config *core.Config, models []string) core.ProfileCapabilitiesSnapshot {
	profiles := map[string]core.ProfileCapabilities{}
	for id, profile := range config.EnabledProfiles() {
		var model *string
		if len(models) > 0 {
			first := models[0]
			model = &first
		}
		offered := make([]core.SessionConfigChoice, 0, len(models))
		efforts := map[string]core.CapabilityState[[]core.SessionConfigChoice]{}
		for _, name := range models {
			offered = append(offered, reviewTestChoice(name))
			efforts[name] = reviewTestReady(reviewTestChoice("medium"), reviewTestChoice("high"), reviewTestChoice("xhigh"))
		}
		profiles[profile.CapabilitiesKey(id)] = core.ProfileCapabilities{
			Choices: core.CapabilityState[core.ProfileConfig]{
				Kind: core.CapabilityStateReady,
				Value: core.ProfileConfig{
					Model:      model,
					Models:     offered,
					Efforts:    []core.SessionConfigChoice{},
					ObservedAt: 1,
				},
			},
			Efforts: efforts,
		}
	}
	return core.ProfileCapabilitiesSnapshot{Profiles: profiles}
}

// reviewTestCloneCapabilities is ProfileCapabilities' Clone: the efforts
// map is copied so editing the copy leaves the original alone.
func reviewTestCloneCapabilities(entry core.ProfileCapabilities) core.ProfileCapabilities {
	efforts := make(map[string]core.CapabilityState[[]core.SessionConfigChoice], len(entry.Efforts))
	maps.Copy(efforts, entry.Efforts)
	entry.Efforts = efforts
	return entry
}

// reviewTestCloneSnapshot is ProfileCapabilitiesSnapshot's Clone.
func reviewTestCloneSnapshot(snapshot core.ProfileCapabilitiesSnapshot) core.ProfileCapabilitiesSnapshot {
	profiles := make(map[string]core.ProfileCapabilities, len(snapshot.Profiles))
	for key, entry := range snapshot.Profiles {
		profiles[key] = reviewTestCloneCapabilities(entry)
	}
	return core.ProfileCapabilitiesSnapshot{Profiles: profiles}
}

// reviewTestEditEfforts applies edit to every profile's efforts.
func reviewTestEditEfforts(snapshot *core.ProfileCapabilitiesSnapshot, edit func(efforts map[string]core.CapabilityState[[]core.SessionConfigChoice])) {
	for key, entry := range snapshot.Profiles {
		entry = reviewTestCloneCapabilities(entry)
		edit(entry.Efforts)
		snapshot.Profiles[key] = entry
	}
}

// reviewTestFirstEntry is the first profile entry of a catalog, in key order
// (Rust's `profiles.into_values().next()`).
func reviewTestFirstEntry(snapshot core.ProfileCapabilitiesSnapshot) core.ProfileCapabilities {
	keys := slices.Sorted(maps.Keys(snapshot.Profiles))
	return snapshot.Profiles[keys[0]]
}

func reviewTestModels(choices []core.SubagentChoice) []string {
	models := make([]string, 0, len(choices))
	for _, choice := range choices {
		models = append(models, choice.Model)
	}
	return models
}

func reviewTestSuggested(t *testing.T, dialog *ReviewSettingsDialog) []core.SubagentChoice {
	t.Helper()
	suggested, ok := dialog.suggestedReviewers()
	if !ok {
		t.Fatal("Autoconfigure should have a suggestion")
	}
	return suggested
}

func TestAutoconfigureButtonFillsRowsWithOfferedEfforts(t *testing.T) {
	config := config()
	dialog := newReviewSettingsDialog(&config)
	if dialog.form.IsEnabled(ReviewSettingsFocusAutoconfigure{}) {
		t.Fatal("Autoconfigure is disabled before the catalog arrives")
	}
	catalog := reviewTestCatalog(&config, []string{"gpt-6-terra", "gpt-6-sol"})
	dialog.updateChoices(&config, &catalog, map[string]struct{}{})
	if len(dialog.review.Reviewers) != 0 {
		t.Fatalf("reviewers = %v, want none", dialog.review.Reviewers)
	}
	suggested := reviewTestSuggested(t, dialog)
	if suggested[0].Model != "gpt-6-terra" {
		t.Fatalf("first model = %q", suggested[0].Model)
	}
	if suggested[0].Effort == nil || *suggested[0].Effort != "high" {
		t.Fatalf("first effort = %v, want high", suggested[0].Effort)
	}
	if !dialog.form.IsEnabled(ReviewSettingsFocusAutoconfigure{}) {
		t.Fatal("Autoconfigure is enabled once a suggestion exists")
	}
	catalog = reviewTestCatalog(&config, []string{"gpt-6-terra", "gpt-6-sol"})
	dialog.updateChoices(&config, &catalog, map[string]struct{}{})
	if len(dialog.review.Reviewers) != 0 {
		t.Fatalf("reviewers = %v, want none", dialog.review.Reviewers)
	}
}

func TestAutoconfigureSkipsModelsOnlyUsagePricedProfilesOffer(t *testing.T) {
	config := config()
	subscription := []string{"gpt-6.1-terra", "gpt-6.1-sol", "claude-sonnet-4.5"}
	api := []string{
		"openrouter/~openai/gpt-terra-latest",
		"openrouter/~openai/gpt-sol-latest",
		"openrouter/~anthropic/claude-sonnet-latest",
	}
	snapshot := reviewTestCatalog(&config, subscription)
	// Two usage-priced profiles outnumber the one subscription, so only the
	// billing tier keeps the subscription models.
	apiEntry := reviewTestFirstEntry(reviewTestCatalog(&config, api))
	for _, id := range []string{"codex-1", "codex-2"} {
		snapshot.Profiles[config.Profiles[id].CapabilitiesKey(id)] = reviewTestCloneCapabilities(apiEntry)
	}
	suggested := func(usagePriced []string) []string {
		dialog := newReviewSettingsDialog(&config)
		priced := map[string]struct{}{}
		for _, id := range usagePriced {
			priced[id] = struct{}{}
		}
		dialog.updateChoices(&config, &snapshot, priced)
		return reviewTestModels(reviewTestSuggested(t, dialog))
	}
	if got := suggested([]string{"codex-1", "codex-2"}); !slices.Equal(got, []string{"gpt-6.1-terra", "gpt-6.1-sol"}) {
		t.Fatalf("suggested = %v", got)
	}
	// Unknown billing counts as a subscription, so the newer aliases win.
	if got := suggested(nil); !strings.HasPrefix(got[0], "openrouter/") {
		t.Fatalf("suggested = %v, want the newer aliases first", got)
	}
}

var reviewTestSubscriptionModels = []string{"gpt-6.1-terra", "gpt-6.1-sol", "claude-sonnet-4.5"}

var reviewTestAPIModels = []string{
	"openrouter/~openai/gpt-terra-latest",
	"openrouter/~openai/gpt-sol-latest",
	"openrouter/~anthropic/claude-sonnet-latest",
}

// reviewTestBillingSnapshot: every profile offers the subscription models
// except those in api, which offer only newer-looking OpenRouter aliases.
func reviewTestBillingSnapshot(config *core.Config, api []string) core.ProfileCapabilitiesSnapshot {
	snapshot := reviewTestCatalog(config, reviewTestSubscriptionModels)
	apiEntry := reviewTestFirstEntry(reviewTestCatalog(config, reviewTestAPIModels))
	for _, id := range api {
		snapshot.Profiles[config.Profiles[id].CapabilitiesKey(id)] = reviewTestCloneCapabilities(apiEntry)
	}
	return snapshot
}

func reviewTestAPIQuota(profile string, harness core.HarnessKind) client.ProfileQuota {
	extra := client.APILabel
	return client.ProfileQuota{
		ProfileID:               profile,
		Harness:                 harness,
		Windows:                 []client.QuotaWindow{},
		Extra:                   &extra,
		RefreshedAtEpochSeconds: 0,
	}
}

func reviewTestEditor(t *testing.T, dashboard *DashboardState) *ReviewSettingsDialog {
	t.Helper()
	setup, ok := dashboard.mode.(ModeSetup)
	if !ok {
		t.Fatalf("mode = %T, want setup", dashboard.mode)
	}
	if setup.reviewEditor == nil {
		t.Fatal("the review editor should be open")
	}
	return setup.reviewEditor
}

func reviewTestSetupModels(t *testing.T, dashboard *DashboardState) []string {
	t.Helper()
	return reviewTestModels(reviewTestEditor(t, dashboard).review.Reviewers)
}

func reviewTestPressAutoconfigure(t *testing.T, dashboard *DashboardState) {
	t.Helper()
	reviewTestEditor(t, dashboard).form.Focus(ReviewSettingsFocusAutoconfigure{})
	dashboard.HandleKey(key(tea.KeyEnter))
}

func reviewTestSetupDashboard(config core.Config) *DashboardState {
	dashboard := NewDashboardState(config, core.StateState{}, map[string]client.ProfileQuota{})
	dashboard.BeginSetup()
	dashboard.beginSetupReview()
	return dashboard
}

func TestAutoconfigureButtonUsesTheQuotaReportsPresentWhenPressed(t *testing.T) {
	dashboard := reviewTestSetupDashboard(config())
	// The suggestion is computed before any quota report: both OpenRouter
	// profiles count as subscriptions and their newer aliases win.
	dashboard.SetProfileCapabilities(reviewTestBillingSnapshot(dashboard.config.Get(), []string{"codex-1", "codex-2"}))
	if suggested := reviewTestSuggested(t, reviewTestEditor(t, dashboard)); !strings.HasPrefix(suggested[0].Model, "openrouter/") {
		t.Fatalf("suggested = %v, want the newer aliases first", reviewTestModels(suggested))
	}
	// The reports arrive without any event that refreshes the dialog.
	if dashboard.quotas == nil {
		dashboard.quotas = map[string]client.ProfileQuota{}
	}
	for _, id := range []string{"codex-1", "codex-2"} {
		dashboard.quotas[id] = reviewTestAPIQuota(id, core.HarnessKindCodex)
	}
	reviewTestPressAutoconfigure(t, dashboard)
	if got := reviewTestSetupModels(t, dashboard); !slices.Equal(got, reviewTestSubscriptionModels[:2]) {
		t.Fatalf("reviewers = %v, want %v", got, reviewTestSubscriptionModels[:2])
	}
}

func TestQuotaUpdatesRefreshTheOpenDialogsSuggestion(t *testing.T) {
	dashboard := reviewTestSetupDashboard(config())
	dashboard.SetProfileCapabilities(reviewTestBillingSnapshot(dashboard.config.Get(), []string{"codex-1", "codex-2"}))
	suggested := func() string {
		return reviewTestSuggested(t, reviewTestEditor(t, dashboard))[0].Model
	}
	if got := suggested(); !strings.HasPrefix(got, "openrouter/") {
		t.Fatalf("suggested = %q, want an OpenRouter alias", got)
	}
	dashboard.ApplyQuota(reviewTestAPIQuota("codex-1", core.HarnessKindCodex))
	dashboard.ApplyQuota(reviewTestAPIQuota("codex-2", core.HarnessKindCodex))
	if got := suggested(); got != "gpt-6.1-terra" {
		t.Fatalf("suggested = %q, want gpt-6.1-terra", got)
	}
}

func TestAnOpencodeProfileWithoutAReportIsUsagePriced(t *testing.T) {
	config := config()
	profile := config.Profiles["codex-1"]
	profile.Kind = core.HarnessKindOpenCode
	profile.Home = "/profiles/openrouter/opencode"
	config.Profiles = maps.Clone(config.Profiles)
	config.Profiles["openrouter"] = profile
	dashboard := NewDashboardState(config, core.StateState{}, map[string]client.ProfileQuota{})
	if got := dashboard.usagePricedProfiles(dashboard.config.Get()); !slices.Equal(slices.Sorted(maps.Keys(got)), []string{"openrouter"}) {
		t.Fatalf("usage-priced profiles = %v, want [openrouter]", slices.Sorted(maps.Keys(got)))
	}
	dashboard.BeginSetup()
	dashboard.beginSetupReview()
	dashboard.SetProfileCapabilities(reviewTestBillingSnapshot(dashboard.config.Get(), []string{"openrouter"}))
	reviewTestPressAutoconfigure(t, dashboard)
	if got := reviewTestSetupModels(t, dashboard); !slices.Equal(got, reviewTestSubscriptionModels[:2]) {
		t.Fatalf("reviewers = %v, want %v", got, reviewTestSubscriptionModels[:2])
	}
}

func TestOpeningWithReviewersPreservesThemWhenCatalogArrives(t *testing.T) {
	config := config()
	effort := "high"
	config.Review.Reviewers = []core.SubagentChoice{{Model: "custom-model", Effort: &effort}}
	dialog := newReviewSettingsDialog(&config)
	catalog := reviewTestCatalog(&config, []string{"gpt-6-terra", "gpt-6-sol"})
	dialog.updateChoices(&config, &catalog, map[string]struct{}{})
	if !subagentChoicesEqual(dialog.review.Reviewers, config.Review.Reviewers) {
		t.Fatalf("reviewers = %v, want %v", dialog.review.Reviewers, config.Review.Reviewers)
	}
}

func TestAutoconfigureDisabledWithoutAdvertisedModels(t *testing.T) {
	config := config()
	dialog := newReviewSettingsDialog(&config)
	catalog := reviewTestCatalog(&config, nil)
	dialog.updateChoices(&config, &catalog, map[string]struct{}{})
	if len(dialog.review.Reviewers) != 0 {
		t.Fatalf("reviewers = %v, want none", dialog.review.Reviewers)
	}
	if dialog.form.IsEnabled(ReviewSettingsFocusAutoconfigure{}) {
		t.Fatal("Autoconfigure has nothing to suggest")
	}
	if !dialog.canSave() { // No reviewers means review is off.
		t.Fatal("no reviewers can be saved")
	}
}

func TestEffortChoicesFollowTheSelectedModel(t *testing.T) {
	config := config()
	catalog := reviewTestCatalog(&config, []string{"gpt-6-terra", "gpt-6-sol"})
	reviewTestEditEfforts(&catalog, func(efforts map[string]core.CapabilityState[[]core.SessionConfigChoice]) {
		efforts["gpt-6-terra"] = reviewTestReady(reviewTestChoice("xhigh"))
		efforts["gpt-6-sol"] = reviewTestReady(reviewTestChoice("medium"))
	})
	dialog := newReviewSettingsDialog(&config)
	dialog.updateChoices(&config, &catalog, map[string]struct{}{})
	dialog.review.Reviewers = reviewTestSuggested(t, dialog)
	dialog.updateChoices(&config, &catalog, map[string]struct{}{})
	if values, _ := dialog.choices(ReviewSettingsFocusEffort(0)); !slices.Equal(values, []string{"Default", "xhigh"}) {
		t.Fatalf("first reviewer efforts = %v", values)
	}
	if values, _ := dialog.choices(ReviewSettingsFocusEffort(1)); !slices.Equal(values, []string{"Default", "medium"}) {
		t.Fatalf("second reviewer efforts = %v", values)
	}
}

func TestReviewerRowsCanBeAddedRemovedAndReconfigured(t *testing.T) {
	dashboard := reviewTestSetupDashboard(config())
	snapshot := reviewTestCatalog(dashboard.config.Get(), []string{"gpt-6-terra", "gpt-6-sol"})
	dashboard.SetProfileCapabilities(reviewTestCloneSnapshot(snapshot))
	config := *dashboard.config.Get()
	editor := reviewTestEditor(t, dashboard)
	editor.review.Reviewers = reviewTestSuggested(t, editor)
	editor.updateChoices(&config, &snapshot, map[string]struct{}{})
	if got := len(editor.review.Reviewers); got != 2 {
		t.Fatalf("reviewers = %d, want 2", got)
	}
	editor.form.Focus(ReviewSettingsFocusAdd{})
	dashboard.HandleKey(key(tea.KeyEnter))
	drawn(dashboard, 100, 30)
	editor = reviewTestEditor(t, dashboard)
	if got := len(editor.review.Reviewers); got != 3 {
		t.Fatalf("reviewers = %d, want 3", got)
	}
	if editor.form.IsEnabled(ReviewSettingsFocusAdd{}) {
		t.Fatal("a fourth reviewer cannot be added")
	}
	dashboard.HandleKey(key(tea.KeyTab))
	reviewTestEditor(t, dashboard).form.Focus(ReviewSettingsFocusRemove(1))
	drawn(dashboard, 100, 30)
	if focused, ok := reviewTestEditor(t, dashboard).form.Focused(); !ok || focused != ReviewSettingsFocus(ReviewSettingsFocusRemove(1)) {
		t.Fatalf("focus = %#v, want the second reviewer's Remove", focused)
	}
	dashboard.HandleKey(key(tea.KeyEnter))
	drawn(dashboard, 100, 30)
	if got := len(reviewTestEditor(t, dashboard).review.Reviewers); got != 2 {
		t.Fatalf("reviewers = %d, want 2", got)
	}
	reviewTestEditor(t, dashboard).review.Reviewers[0].Model = "manual"
	dashboard.HandleKey(key(tea.KeyTab))
	reviewTestEditor(t, dashboard).form.Focus(ReviewSettingsFocusAutoconfigure{})
	drawn(dashboard, 100, 30)
	dashboard.HandleKey(key(tea.KeyEnter))
	if model := reviewTestEditor(t, dashboard).review.Reviewers[0].Model; model != "gpt-6-terra" {
		t.Fatalf("first reviewer = %q, want gpt-6-terra", model)
	}
}

func TestAddingReviewerDuringLegacyUpgradeSerializesTheEdit(t *testing.T) {
	initial := config()
	initial.Review = core.ReviewConfigLegacyDefault()
	dashboard := reviewTestSetupDashboard(initial)
	dashboard.SetProfileCapabilities(reviewTestCatalog(dashboard.config.Get(), []string{"gpt-6-terra"}))
	reviewTestEditor(t, dashboard).form.Focus(ReviewSettingsFocusAdd{})
	drawn(dashboard, 100, 30)
	dashboard.HandleKey(key(tea.KeyEnter))
	review := reviewTestEditor(t, dashboard).review
	if review.IsLegacy() {
		t.Fatal("adding a reviewer ends the legacy upgrade")
	}
	if got := len(review.Reviewers); got != 1 {
		t.Fatalf("reviewers = %d, want 1", got)
	}
	encoded, err := json.Marshal(review)
	if err != nil {
		t.Fatalf("review serializes: %v", err)
	}
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(encoded, &fields); err != nil {
		t.Fatalf("review is an object: %v", err)
	}
	if _, ok := fields["reviewers"]; !ok {
		t.Fatalf("review = %s, want reviewers", encoded)
	}
}

func TestKnownEffortChoicesGateSaveButPendingChoicesDoNot(t *testing.T) {
	config := config()
	catalog := reviewTestCatalog(&config, []string{"gpt-6-terra"})
	dialog := newReviewSettingsDialog(&config)
	xhigh := "xhigh"
	dialog.review.Reviewers = append(dialog.review.Reviewers, core.SubagentChoice{Model: "gpt-6-terra", Effort: &xhigh})
	reviewTestEditEfforts(&catalog, func(efforts map[string]core.CapabilityState[[]core.SessionConfigChoice]) {
		efforts["gpt-6-terra"] = reviewTestReady()
	})
	dialog.updateChoices(&config, &catalog, map[string]struct{}{})
	if dialog.canSave() {
		t.Fatal("an effort the model does not offer cannot be saved")
	}
	snapshot := dialog.validationSnapshot()
	if message, ok := reviewSettingsDialogValidationError(&snapshot, &dialog.review); !ok || message != "Reviewer 1 effort is unavailable." {
		t.Fatalf("validation = %q, %v", message, ok)
	}
	dialog.review.Reviewers[0].Effort = nil
	if !dialog.canSave() {
		t.Fatal("the default effort can be saved")
	}
	dialog.review.Reviewers[0].Effort = &xhigh
	reviewTestEditEfforts(&catalog, func(efforts map[string]core.CapabilityState[[]core.SessionConfigChoice]) {
		efforts["gpt-6-terra"] = core.CapabilityState[[]core.SessionConfigChoice]{Kind: core.CapabilityStatePending}
	})
	dialog.updateChoices(&config, &catalog, map[string]struct{}{})
	if !dialog.canSave() {
		t.Fatal("pending effort choices do not gate save")
	}
}

func TestReviewerFocusFollowsModelEffortRemoveOrder(t *testing.T) {
	dashboard := reviewTestSetupDashboard(config())
	snapshot := reviewTestCatalog(dashboard.config.Get(), []string{"gpt-6-terra", "gpt-6-sol"})
	dashboard.SetProfileCapabilities(reviewTestCloneSnapshot(snapshot))
	config := *dashboard.config.Get()
	editor := reviewTestEditor(t, dashboard)
	editor.review.Reviewers = reviewTestSuggested(t, editor)
	editor.updateChoices(&config, &snapshot, map[string]struct{}{})
	editor.form.Focus(ReviewSettingsFocusModel(0))
	drawn(dashboard, 100, 30)
	for _, expected := range []ReviewSettingsFocus{
		ReviewSettingsFocusEffort(0),
		ReviewSettingsFocusRemove(0),
		ReviewSettingsFocusModel(1),
	} {
		dashboard.HandleKey(key(tea.KeyTab))
		if focused, ok := reviewTestEditor(t, dashboard).form.Focused(); !ok || focused != expected {
			t.Fatalf("focus = %#v, want %#v", focused, expected)
		}
	}
}

// Hard-won: a5f1492f8: providers outside the original preset table got no reviewers.
func TestGoldenReviewSettings(t *testing.T) {
	dashboard := reviewTestSetupDashboard(config())
	dashboard.SetProfileCapabilities(reviewTestCatalog(dashboard.config.Get(), []string{
		"gpt-6-terra",
		"gpt-6-sol",
		"claude-sonnet-5-5",
		"claude-haiku-5-5",
	}))
	reviewTestPressAutoconfigure(t, dashboard)
	wide := strings.Join(bufferLines(surfaceControlsDrawBuffer(dashboard, 100, 30)), "\n")
	narrow := strings.Join(bufferLines(surfaceControlsDrawBuffer(dashboard, 60, 30)), "\n")
	anthropic := reviewTestSetupDashboard(config())
	anthropic.SetProfileCapabilities(reviewTestCatalog(anthropic.config.Get(), []string{"claude-sonnet-5-5", "claude-haiku-5-5"}))
	reviewTestPressAutoconfigure(t, anthropic)
	anthropicLines := strings.Join(bufferLines(surfaceControlsDrawBuffer(anthropic, 100, 30)), "\n")
	other := reviewTestSetupDashboard(config())
	other.SetProfileCapabilities(reviewTestCatalog(other.config.Get(), []string{"google/gemini-3-pro", "google/gemini-3.5-flash"}))
	reviewTestPressAutoconfigure(t, other)
	otherLines := strings.Join(bufferLines(surfaceControlsDrawBuffer(other, 100, 30)), "\n")
	rendered := "=== Wide (100x30) ===\n" + wide +
		"\n=== Narrow (60x30) ===\n" + narrow +
		"\n=== Anthropic (100x30) ===\n" + anthropicLines +
		"\n=== Other provider (100x30) ===\n" + otherLines
	core.AssertGolden(t, "testdata", "review-settings", strings.TrimRightFunc(rendered, unicode.IsSpace))
}

func TestAutoconfigureWaitsForEffortsThenUsesHarnessDefaultForUnsupportedEffort(t *testing.T) {
	dashboard := reviewTestSetupDashboard(config())
	snapshot := reviewTestCatalog(dashboard.config.Get(), []string{"gpt-6-terra", "gpt-6-sol"})
	reviewTestEditEfforts(&snapshot, func(efforts map[string]core.CapabilityState[[]core.SessionConfigChoice]) {
		clear(efforts)
	})
	dashboard.SetProfileCapabilities(reviewTestCloneSnapshot(snapshot))
	reviewTestEditor(t, dashboard).form.Focus(ReviewSettingsFocusAutoconfigure{})
	action := dashboard.HandleKey(key(tea.KeyEnter))
	if _, ok := action.(DashboardActionWarmProfileCapabilities); !ok {
		t.Fatalf("action = %#v, want a capability warm-up", action)
	}
	editor := reviewTestEditor(t, dashboard)
	if editor.pendingAutoconfigure == nil {
		t.Fatal("Autoconfigure should wait for efforts")
	}
	if len(editor.review.Reviewers) != 0 {
		t.Fatalf("reviewers = %v, want none while waiting", editor.review.Reviewers)
	}
	if editor.canSave() {
		t.Fatal("nothing can be saved while efforts are checked")
	}
	reviewTestEditEfforts(&snapshot, func(efforts map[string]core.CapabilityState[[]core.SessionConfigChoice]) {
		efforts["gpt-6-terra"] = reviewTestReady()
		efforts["gpt-6-sol"] = reviewTestReady(reviewTestChoice("medium"))
	})
	dashboard.SetProfileCapabilities(reviewTestCloneSnapshot(snapshot))
	editor = reviewTestEditor(t, dashboard)
	editor.updateChoices(dashboard.config.Get(), &snapshot, map[string]struct{}{})
	if editor.pendingAutoconfigure != nil {
		t.Fatal("the wait should be over")
	}
	if effort := editor.review.Reviewers[0].Effort; effort != nil {
		t.Fatalf("first effort = %q, want the harness default", *effort)
	}
	if effort := editor.review.Reviewers[1].Effort; effort == nil || *effort != "medium" {
		t.Fatalf("second effort = %v, want medium", effort)
	}
}

func TestAutoconfigureTimeoutFinishesOnClockTickWithoutACatalogUpdate(t *testing.T) {
	dashboard := reviewTestSetupDashboard(config())
	snapshot := reviewTestCatalog(dashboard.config.Get(), []string{"gpt-6-terra", "gpt-6-sol"})
	reviewTestEditEfforts(&snapshot, func(efforts map[string]core.CapabilityState[[]core.SessionConfigChoice]) {
		clear(efforts)
	})
	dashboard.SetProfileCapabilities(snapshot)
	reviewTestEditor(t, dashboard).form.Focus(ReviewSettingsFocusAutoconfigure{})
	dashboard.HandleKey(key(tea.KeyEnter))
	editor := reviewTestEditor(t, dashboard)
	if editor.pendingAutoconfigure == nil {
		t.Fatal("Autoconfigure should wait for efforts")
	}
	editor.pendingAutoconfigure.started = time.Now().Add(-21 * time.Second)
	if !dashboard.TickReviewAutoconfigure() {
		t.Fatal("the tick should finish the timed-out wait")
	}
	editor = reviewTestEditor(t, dashboard)
	if editor.checkingEfforts() {
		t.Fatal("the wait should be over")
	}
	if got := len(editor.review.Reviewers); got != 2 {
		t.Fatalf("reviewers = %d, want 2", got)
	}
	for _, choice := range editor.review.Reviewers {
		if choice.Effort != nil {
			t.Fatalf("effort = %q, want the harness default", *choice.Effort)
		}
	}
	want := "Reviewer effort discovery failed: claude-1 (timed out); codex-1 (timed out); codex-2 (timed out); using defaults."
	if got := ingestText(editor.effortNotice); got != want {
		t.Fatalf("notice = %q, want %q", got, want)
	}
}

func TestUnchangedLegacyReviewRemainsValidDuringUnrelatedSave(t *testing.T) {
	config := config()
	config.Review = core.ReviewConfigLegacyDefault()
	dialog := newReviewSettingsDialog(&config)
	if !dialog.canSave() {
		t.Fatal("an unchanged legacy review can be saved")
	}
	snapshot := dialog.validationSnapshot()
	if message, ok := reviewSettingsDialogValidationError(&snapshot, &dialog.review); ok {
		t.Fatalf("validation = %q, want none", message)
	}
}

func TestFailedEffortDiscoveryIsReportedWhenAutoconfigureTimesOut(t *testing.T) {
	config := config()
	snapshot := reviewTestCatalog(&config, []string{"gpt-6-terra"})
	reviewTestEditEfforts(&snapshot, func(efforts map[string]core.CapabilityState[[]core.SessionConfigChoice]) {
		efforts["gpt-6-terra"] = core.CapabilityState[[]core.SessionConfigChoice]{Kind: core.CapabilityStateFailed, Error: "probe failed"}
	})
	dialog := newReviewSettingsDialog(&config)
	high := "high"
	dialog.pendingAutoconfigure = &pendingAutoconfigure{
		choices: []core.SubagentChoice{{Model: "gpt-6-terra", Effort: &high}},
		started: time.Now().Add(-21 * time.Second),
	}
	dialog.updateChoices(&config, &snapshot, map[string]struct{}{})
	if effort := dialog.review.Reviewers[0].Effort; effort != nil {
		t.Fatalf("effort = %q, want the harness default", *effort)
	}
	if notice := ingestText(dialog.effortNotice); !strings.Contains(notice, "probe failed") {
		t.Fatalf("notice = %q, want the probe failure", notice)
	}
}

func TestPartialDiscoveryDoesNotRejectConfiguredReviewerOnSave(t *testing.T) {
	config := config()
	high := "high"
	config.Review.Reviewers = []core.SubagentChoice{{Model: "custom-model", Effort: &high}}
	snapshot := reviewTestCatalog(&config, nil)
	for key, entry := range snapshot.Profiles {
		entry.Choices = core.CapabilityState[core.ProfileConfig]{Kind: core.CapabilityStateFailed, Error: "probe failed"}
		snapshot.Profiles[key] = entry
	}
	dialog := newReviewSettingsDialog(&config)
	dialog.updateChoices(&config, &snapshot, map[string]struct{}{})
	if !dialog.canSave() {
		t.Fatal("partial discovery must not reject the configured reviewer")
	}
	validation := dialog.validationSnapshot()
	if message, ok := reviewSettingsDialogValidationError(&validation, &dialog.review); ok {
		t.Fatalf("validation = %q, want none", message)
	}
}

func TestAutoconfigureWarmsNewlySelectedModelWithinExistingWait(t *testing.T) {
	dashboard := reviewTestSetupDashboard(config())
	dashboard.SetProfileCapabilities(reviewTestCatalog(dashboard.config.Get(), []string{"claude-sonnet-5-5"}))
	xhigh := "xhigh"
	reviewTestEditor(t, dashboard).pendingAutoconfigure = &pendingAutoconfigure{
		choices: []core.SubagentChoice{{Model: "claude-sonnet-5-5", Effort: &xhigh}},
		started: time.Now(),
	}
	revised := reviewTestCatalog(dashboard.config.Get(), []string{"gpt-6-terra", "gpt-6-sol"})
	reviewTestEditEfforts(&revised, func(efforts map[string]core.CapabilityState[[]core.SessionConfigChoice]) {
		delete(efforts, "gpt-6-terra")
		delete(efforts, "gpt-6-sol")
	})
	dashboard.SetProfileCapabilities(revised)
	if !reviewTestEditor(t, dashboard).checkingEfforts() {
		t.Fatal("the wait should continue for the newly selected models")
	}
	if action, ok := dashboard.takeProfileHydration().(DashboardActionWarmProfileCapabilities); !ok {
		t.Fatalf("action = %#v, want a capability warm-up", action)
	}
}

func TestLegacyDraftAutoconfigureWarmsTheRecomputedModels(t *testing.T) {
	legacy := config()
	legacy.Review = core.ReviewConfigLegacyDefault()
	dashboard := reviewTestSetupDashboard(legacy)
	dashboard.SetProfileCapabilities(reviewTestCatalog(dashboard.config.Get(), []string{"claude-sonnet-5-5"}))
	xhigh := "xhigh"
	reviewTestEditor(t, dashboard).pendingAutoconfigure = &pendingAutoconfigure{
		choices: []core.SubagentChoice{{Model: "claude-sonnet-5-5", Effort: &xhigh}},
		started: time.Now(),
	}
	revised := reviewTestCatalog(dashboard.config.Get(), []string{"gpt-6-terra", "gpt-6-sol"})
	reviewTestEditEfforts(&revised, func(efforts map[string]core.CapabilityState[[]core.SessionConfigChoice]) {
		delete(efforts, "gpt-6-terra")
		delete(efforts, "gpt-6-sol")
	})
	dashboard.SetProfileCapabilities(revised)
	warm, ok := dashboard.takeProfileHydration().(DashboardActionWarmProfileCapabilities)
	if !ok {
		t.Fatal("expected a warm request")
	}
	var warmed core.Config
	if err := json.Unmarshal([]byte(warm.Config), &warmed); err != nil {
		t.Fatalf("the warmed configuration parses: %v", err)
	}
	if warmed.Review.IsLegacy() {
		t.Fatal("the warmed configuration has reviewers of its own")
	}
	if !slices.ContainsFunc(warmed.Review.Reviewers, func(reviewer core.SubagentChoice) bool { return reviewer.Model == "gpt-6-terra" }) {
		t.Fatalf("warmed configuration = %s, want gpt-6-terra", warm.Config)
	}
}

func TestAutoconfigureIgnoresAFailedProfileThatDoesNotOfferTheModel(t *testing.T) {
	config := config()
	snapshot := reviewTestCatalog(&config, []string{"gpt-6-terra", "gpt-6-sol"})
	// Medium is the closest offered effort below Terra's requested high.
	reviewTestEditEfforts(&snapshot, func(efforts map[string]core.CapabilityState[[]core.SessionConfigChoice]) {
		for _, model := range []string{"gpt-6-terra", "gpt-6-sol"} {
			efforts[model] = reviewTestReady(reviewTestChoice("low"), reviewTestChoice("medium"))
		}
	})
	// An unrelated profile whose discovery failed.
	id := slices.Sorted(maps.Keys(config.Profiles))[0]
	failed := config.Profiles[id]
	failed.Home = "/profiles/failed"
	failedID := id + "-failed"
	snapshot.Profiles[failed.CapabilitiesKey(failedID)] = core.ProfileCapabilities{
		Choices: core.CapabilityState[core.ProfileConfig]{Kind: core.CapabilityStateFailed, Error: "login expired"},
		Efforts: map[string]core.CapabilityState[[]core.SessionConfigChoice]{},
	}
	config.Profiles = maps.Clone(config.Profiles)
	config.Profiles[failedID] = failed
	dialog := newReviewSettingsDialog(&config)
	dialog.pendingAutoconfigure = &pendingAutoconfigure{choices: []core.SubagentChoice{}, started: time.Now()}
	dialog.updateChoices(&config, &snapshot, map[string]struct{}{})
	if dialog.pendingAutoconfigure != nil {
		t.Fatal("the wait should be over")
	}
	if len(dialog.review.Reviewers) == 0 {
		t.Fatal("Autoconfigure should choose reviewers")
	}
	for _, reviewer := range dialog.review.Reviewers {
		if reviewer.Effort == nil || (*reviewer.Effort != "low" && *reviewer.Effort != "medium") {
			t.Fatalf("reviewer = %#v, want low or medium effort", reviewer)
		}
	}
	if !slices.ContainsFunc(dialog.review.Reviewers, func(reviewer core.SubagentChoice) bool {
		return reviewer.Effort != nil && *reviewer.Effort == "medium"
	}) {
		t.Fatal("one reviewer should fall back to medium")
	}
	notice := ingestText(dialog.effortNotice)
	if !strings.Contains(notice, "Adjusted reviewer efforts") {
		t.Fatalf("notice = %q", notice)
	}
	if strings.Contains(notice, "discovery failed") {
		t.Fatalf("notice = %q", notice)
	}
	if strings.Contains(notice, "timed out") {
		t.Fatalf("notice = %q", notice)
	}
}
