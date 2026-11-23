// Port of mj-tui/src/review_settings.rs.

// The review settings editor. Capability choices come from the dashboard's
// published catalog; the editor never performs discovery on the event loop.

package tui

import (
	"encoding/json"
	"fmt"
	"reflect"
	"slices"
	"strings"
	"time"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ReviewSettingsFocus is one control of the review settings editor. Every
// variant is comparable, so it is a form key.
type ReviewSettingsFocus interface {
	isReviewSettingsFocus()
}

// ReviewSettingsFocusModel is reviewer N's model selector.
type ReviewSettingsFocusModel int

// ReviewSettingsFocusEffort is reviewer N's effort selector.
type ReviewSettingsFocusEffort int

// ReviewSettingsFocusRemove is reviewer N's Remove button.
type ReviewSettingsFocusRemove int

type ReviewSettingsFocusAdd struct{}

type ReviewSettingsFocusAutoconfigure struct{}

type ReviewSettingsFocusBack struct{}

type ReviewSettingsFocusCancel struct{}

type ReviewSettingsFocusSave struct{}

func (ReviewSettingsFocusModel) isReviewSettingsFocus()         {}
func (ReviewSettingsFocusEffort) isReviewSettingsFocus()        {}
func (ReviewSettingsFocusRemove) isReviewSettingsFocus()        {}
func (ReviewSettingsFocusAdd) isReviewSettingsFocus()           {}
func (ReviewSettingsFocusAutoconfigure) isReviewSettingsFocus() {}
func (ReviewSettingsFocusBack) isReviewSettingsFocus()          {}
func (ReviewSettingsFocusCancel) isReviewSettingsFocus()        {}
func (ReviewSettingsFocusSave) isReviewSettingsFocus()          {}

// ReviewSettingsOutcome is what one event asked of the setup dialog that
// hosts the editor.
type ReviewSettingsOutcome int

const (
	ReviewSettingsOutcomeContinue ReviewSettingsOutcome = iota
	ReviewSettingsOutcomeBack
	ReviewSettingsOutcomeCancelSetup
	ReviewSettingsOutcomeSave
)

// ReviewSettingsValidation is the editor's view of the catalog at the time
// the setup draft was checked, so validation can run after the editor
// closes.
type ReviewSettingsValidation struct {
	reviewers    []core.SubagentChoice
	legacy       bool
	models       []core.SessionConfigChoice
	modelsKnown  bool
	efforts      [][]core.SessionConfigChoice
	effortsKnown []bool
}

// pendingAutoconfigure is an Autoconfigure waiting for reviewer efforts:
// the draft it would apply and when it started.
type pendingAutoconfigure struct {
	choices []core.SubagentChoice
	started time.Time
}

// ReviewSettingsDialog is the review settings editor.
type ReviewSettingsDialog struct {
	review         core.ReviewConfig
	originalReview core.ReviewConfig
	// form is Rust's RefCell<Dialog<…>>; the editor is always held by
	// pointer, so drawing mutates it in place.
	form           chat.Dialog[ReviewSettingsFocus]
	combo          chat.ComboBoxState[ReviewSettingsFocus]
	models         []core.SessionConfigChoice
	efforts        [][]core.SessionConfigChoice
	modelsKnown    bool
	modelsPartial  bool
	effortsKnown   []bool
	effortsPartial []bool
	effortFailures []string
	// suggested is nil when Autoconfigure has nothing to suggest (Rust's
	// None), distinct from an empty suggestion.
	suggested            *[]core.SubagentChoice
	suggestedAdjustments []core.EffortAdjustment
	pendingAutoconfigure *pendingAutoconfigure
	effortNotice         *string
	saving               bool
	saveError            *string
}

func newReviewSettingsDialog(config *core.Config) *ReviewSettingsDialog {
	dialog := &ReviewSettingsDialog{
		review:         cloneReviewConfig(config.Review),
		originalReview: cloneReviewConfig(config.Review),
		form:           chat.NewDialog[ReviewSettingsFocus](),
	}
	dialog.prepare()
	return dialog
}

// cloneReviewConfig is ReviewConfig's derived Clone: the reviewer list is
// copied so edits to the draft never reach the configuration.
func cloneReviewConfig(review core.ReviewConfig) core.ReviewConfig {
	review.Reviewers = slices.Clone(review.Reviewers)
	if review.Legacy != nil {
		legacy := *review.Legacy
		review.Legacy = &legacy
	}
	return review
}

// Clone is Rust's derived Clone.
func (d *ReviewSettingsDialog) Clone() *ReviewSettingsDialog {
	clone := *d
	clone.review = cloneReviewConfig(d.review)
	clone.originalReview = cloneReviewConfig(d.originalReview)
	clone.form = d.form.Clone()
	clone.models = slices.Clone(d.models)
	clone.efforts = make([][]core.SessionConfigChoice, len(d.efforts))
	for index, efforts := range d.efforts {
		clone.efforts[index] = slices.Clone(efforts)
	}
	clone.effortsKnown = slices.Clone(d.effortsKnown)
	clone.effortsPartial = slices.Clone(d.effortsPartial)
	clone.effortFailures = slices.Clone(d.effortFailures)
	if d.suggested != nil {
		suggested := slices.Clone(*d.suggested)
		clone.suggested = &suggested
	}
	clone.suggestedAdjustments = slices.Clone(d.suggestedAdjustments)
	if d.pendingAutoconfigure != nil {
		pending := *d.pendingAutoconfigure
		pending.choices = slices.Clone(pending.choices)
		clone.pendingAutoconfigure = &pending
	}
	if d.effortNotice != nil {
		notice := *d.effortNotice
		clone.effortNotice = &notice
	}
	if d.saveError != nil {
		saveError := *d.saveError
		clone.saveError = &saveError
	}
	return &clone
}

// Equal is Rust's derived PartialEq.
func (d *ReviewSettingsDialog) Equal(other *ReviewSettingsDialog) bool {
	pendingEqual := (d.pendingAutoconfigure == nil) == (other.pendingAutoconfigure == nil)
	if pendingEqual && d.pendingAutoconfigure != nil {
		pendingEqual = subagentChoicesEqual(d.pendingAutoconfigure.choices, other.pendingAutoconfigure.choices) &&
			d.pendingAutoconfigure.started.Equal(other.pendingAutoconfigure.started)
	}
	suggestedEqual := (d.suggested == nil) == (other.suggested == nil)
	if suggestedEqual && d.suggested != nil {
		suggestedEqual = subagentChoicesEqual(*d.suggested, *other.suggested)
	}
	return reviewConfigEqual(&d.review, &other.review) &&
		reviewConfigEqual(&d.originalReview, &other.originalReview) &&
		d.form.Equal(&other.form) &&
		d.combo == other.combo &&
		slices.EqualFunc(d.models, other.models, sessionConfigChoiceEqual) &&
		slices.EqualFunc(d.efforts, other.efforts, func(a, b []core.SessionConfigChoice) bool {
			return slices.EqualFunc(a, b, sessionConfigChoiceEqual)
		}) &&
		d.modelsKnown == other.modelsKnown &&
		d.modelsPartial == other.modelsPartial &&
		slices.Equal(d.effortsKnown, other.effortsKnown) &&
		slices.Equal(d.effortsPartial, other.effortsPartial) &&
		slices.Equal(d.effortFailures, other.effortFailures) &&
		suggestedEqual &&
		slices.EqualFunc(d.suggestedAdjustments, other.suggestedAdjustments, func(a, b core.EffortAdjustment) bool {
			return a.Model == b.Model && a.Requested == b.Requested && reviewOptionalEqual(a.Used, b.Used)
		}) &&
		pendingEqual &&
		reviewOptionalEqual(d.effortNotice, other.effortNotice) &&
		d.saving == other.saving &&
		reviewOptionalEqual(d.saveError, other.saveError)
}

// reviewOptionalEqual compares two Rust Options held as pointers.
func reviewOptionalEqual(a, b *string) bool {
	if a == nil || b == nil {
		return a == nil && b == nil
	}
	return *a == *b
}

// subagentChoicesEqual is `Vec<SubagentChoice>`'s PartialEq.
func subagentChoicesEqual(a, b []core.SubagentChoice) bool {
	return slices.EqualFunc(a, b, func(left, right core.SubagentChoice) bool {
		return left.Model == right.Model && reviewOptionalEqual(left.Effort, right.Effort)
	})
}

// sessionConfigChoiceEqual is SessionConfigChoice's PartialEq.
func sessionConfigChoiceEqual(a, b core.SessionConfigChoice) bool {
	return a.Value == b.Value && a.Name == b.Name && reviewOptionalEqual(a.Description, b.Description)
}

// reviewConfigEqual is ReviewConfig's PartialEq.
func reviewConfigEqual(a, b *core.ReviewConfig) bool {
	return reflect.DeepEqual(a.Legacy, b.Legacy) &&
		subagentChoicesEqual(a.Reviewers, b.Reviewers) &&
		a.Disabled == b.Disabled &&
		reflect.DeepEqual(a.BriefWriter, b.BriefWriter)
}

// sessionConfigChoicesOffer says whether choices offer value.
func sessionConfigChoicesOffer(choices []core.SessionConfigChoice, value string) bool {
	return slices.ContainsFunc(choices, func(choice core.SessionConfigChoice) bool { return choice.Value == value })
}

// adjustedEffortsNotice is "Adjusted reviewer efforts: …" for the efforts
// Autoconfigure had to change.
func adjustedEffortsNotice(adjustments []core.EffortAdjustment) string {
	parts := make([]string, 0, len(adjustments))
	for _, item := range adjustments {
		used := "default"
		if item.Used != nil {
			used = *item.Used
		}
		parts = append(parts, fmt.Sprintf("%s: %s → %s", item.Model, item.Requested, used))
	}
	return "Adjusted reviewer efforts: " + strings.Join(parts, ", ")
}

func (d *ReviewSettingsDialog) prepareDialogState() {
	d.form.SetActionRole(ReviewSettingsFocusCancel{}, chat.ActionRoleCancel)
	d.form.SetActionRole(ReviewSettingsFocusBack{}, chat.ActionRoleBack)
	d.form.SetDirty(false)
	d.form.SetDismissActions([]ReviewSettingsFocus{ReviewSettingsFocusCancel{}})
	d.form.SetDefaultAction(ReviewSettingsFocusSave{})
}

func (d *ReviewSettingsDialog) updateChoices(config *core.Config, snapshot *core.ProfileCapabilitiesSnapshot, usagePriced map[string]struct{}) {
	models := snapshot.Options(config, nil)
	offers := snapshot.ModelOffers(config, func(profile string) bool {
		_, priced := usagePriced[profile]
		return priced
	})
	efforts := func(model string) ([]core.SessionConfigChoice, bool) {
		return knownEfforts(snapshot, config, model)
	}
	d.modelsKnown = models != nil
	if models != nil {
		d.modelsPartial = len(models.Unavailable) > 0
		d.models = models.Models
		if configured, ok := core.Autoconfigure(offers, efforts); ok {
			d.suggestedAdjustments = slices.Clone(configured.Adjusted)
			reviewers := configured.Reviewers
			d.suggested = &reviewers
		} else {
			d.suggestedAdjustments = nil
			d.suggested = nil
		}
	} else {
		d.models = nil
		d.modelsPartial = false
		d.suggested = nil
		d.suggestedAdjustments = nil
	}
	if pending := d.pendingAutoconfigure; pending != nil {
		pendingChoices := pending.choices
		if d.suggested != nil {
			pendingChoices = *d.suggested
		}
		ready := true
		for _, choice := range pendingChoices {
			model := choice.Model
			options := snapshot.Options(config, &model)
			if options == nil || (len(snapshot.EffortBlockers(config, choice.Model)) != 0 &&
				(choice.Effort == nil || !sessionConfigChoicesOffer(options.Efforts, *choice.Effort))) {
				ready = false
				break
			}
		}
		if ready || time.Since(pending.started) >= 20*time.Second {
			configured, ok := core.Autoconfigure(offers, efforts)
			resolved := slices.Clone(pending.choices)
			var adjustments []core.EffortAdjustment
			if ok {
				resolved = slices.Clone(configured.Reviewers)
				adjustments = configured.Adjusted
			}
			notices := []string{}
			if len(adjustments) > 0 {
				notices = append(notices, adjustedEffortsNotice(adjustments))
			}
			failures := []string{}
			for index := range resolved {
				choice := &resolved[index]
				model := choice.Model
				offered := snapshot.Options(config, &model)
				blockers := snapshot.EffortBlockers(config, choice.Model)
				unknown := offered == nil || (len(blockers) != 0 &&
					(choice.Effort == nil || !sessionConfigChoicesOffer(offered.Efforts, *choice.Effort)))
				if !unknown {
					continue
				}
				choice.Effort = nil
				for _, reason := range blockers {
					if strings.Contains(reason, "discovery pending") {
						reason = strings.ReplaceAll(reason, ": effort discovery pending", "") + " (timed out)"
					}
					if !slices.Contains(failures, reason) {
						failures = append(failures, reason)
					}
				}
			}
			if len(failures) > 0 {
				notices = append(notices, fmt.Sprintf(
					"Reviewer effort discovery failed: %s; using defaults.",
					strings.Join(failures, "; "),
				))
			}
			d.effortNotice = nil
			if len(notices) > 0 {
				notice := strings.Join(notices, " ")
				d.effortNotice = &notice
			}
			d.review.Reviewers = resolved
			d.review.ClearLegacy()
			d.pendingAutoconfigure = nil
		}
	}
	d.efforts = [][]core.SessionConfigChoice{}
	d.effortsKnown = []bool{}
	d.effortsPartial = []bool{}
	d.effortFailures = []string{}
	for _, reviewer := range d.review.Reviewers {
		model := reviewer.Model
		if options := snapshot.Options(config, &model); options != nil {
			d.effortFailures = append(d.effortFailures, strings.Join(options.Unavailable, "; "))
			d.effortsPartial = append(d.effortsPartial, len(options.Unavailable) > 0)
			d.efforts = append(d.efforts, options.Efforts)
			d.effortsKnown = append(d.effortsKnown, true)
		} else {
			d.efforts = append(d.efforts, []core.SessionConfigChoice{})
			d.effortFailures = append(d.effortFailures, "")
			d.effortsPartial = append(d.effortsPartial, false)
			d.effortsKnown = append(d.effortsKnown, false)
		}
	}
	d.prepare()
}

func (d *ReviewSettingsDialog) checkingEfforts() bool {
	return d.pendingAutoconfigure != nil
}

func (d *ReviewSettingsDialog) pendingAutoconfigureChoices() ([]core.SubagentChoice, bool) {
	if d.pendingAutoconfigure == nil {
		return nil, false
	}
	if d.suggested != nil {
		return *d.suggested, true
	}
	return d.pendingAutoconfigure.choices, true
}

// suggestedReviewers is a copy of the suggestion. (Rust's suggested method,
// renamed because the field holds that name.)
func (d *ReviewSettingsDialog) suggestedReviewers() ([]core.SubagentChoice, bool) {
	if d.suggested == nil {
		return nil, false
	}
	return slices.Clone(*d.suggested), true
}

func (d *ReviewSettingsDialog) effortUnavailable(i int) bool {
	effort := d.review.Reviewers[i].Effort
	if effort == nil {
		return false
	}
	known := i < len(d.effortsKnown) && d.effortsKnown[i]
	partial := i < len(d.effortsPartial) && d.effortsPartial[i]
	return known && !partial && !slices.ContainsFunc(d.efforts[i], func(choice core.SessionConfigChoice) bool {
		return choice.Value == *effort
	})
}

func (d *ReviewSettingsDialog) canSave() bool {
	if d.saving || d.pendingAutoconfigure != nil || len(d.review.Reviewers) > core.MaxReviewers {
		return false
	}
	for index, reviewer := range d.review.Reviewers {
		if strings.TrimSpace(reviewer.Model) == "" {
			return false
		}
		if d.modelsKnown && !d.modelsPartial && !sessionConfigChoicesOffer(d.models, reviewer.Model) {
			return false
		}
		if d.effortUnavailable(index) {
			return false
		}
	}
	return true
}

func (d *ReviewSettingsDialog) validationSnapshot() ReviewSettingsValidation {
	effortsKnown := make([]bool, 0, len(d.effortsKnown))
	for i, known := range d.effortsKnown {
		if i >= len(d.effortsPartial) {
			break
		}
		effortsKnown = append(effortsKnown, known && !d.effortsPartial[i])
	}
	return ReviewSettingsValidation{
		reviewers:    slices.Clone(d.review.Reviewers),
		legacy:       d.review.IsLegacy(),
		models:       slices.Clone(d.models),
		modelsKnown:  d.modelsKnown && !d.modelsPartial,
		efforts:      slices.Clone(d.efforts),
		effortsKnown: effortsKnown,
	}
}

// reviewSettingsDialogValidationError is ReviewSettingsDialog::
// validation_error: why review cannot be saved against the catalog snapshot
// taken when it was edited, or false when it can.
func reviewSettingsDialogValidationError(snapshot *ReviewSettingsValidation, review *core.ReviewConfig) (string, bool) {
	if review.IsLegacy() && snapshot.legacy {
		return "", false
	}
	if len(review.Reviewers) > core.MaxReviewers {
		return "Choose at most 3 reviewers.", true
	}
	if !subagentChoicesEqual(snapshot.reviewers, review.Reviewers) {
		return "", false
	}
	for index, reviewer := range review.Reviewers {
		if strings.TrimSpace(reviewer.Model) == "" ||
			(snapshot.modelsKnown && !sessionConfigChoicesOffer(snapshot.models, reviewer.Model)) {
			return fmt.Sprintf("Reviewer %d model is unavailable.", index+1), true
		}
		if reviewer.Effort != nil && index < len(snapshot.effortsKnown) && snapshot.effortsKnown[index] &&
			!sessionConfigChoicesOffer(snapshot.efforts[index], *reviewer.Effort) {
			return fmt.Sprintf("Reviewer %d effort is unavailable.", index+1), true
		}
	}
	return "", false
}

func (d *ReviewSettingsDialog) choices(id ReviewSettingsFocus) ([]string, int) {
	switch id := id.(type) {
	case ReviewSettingsFocusModel:
		current := d.review.Reviewers[id].Model
		values := make([]string, 0, len(d.models)+1)
		for _, choice := range d.models {
			values = append(values, choice.Value)
		}
		if !slices.Contains(values, current) {
			values = slices.Insert(values, 0, current)
		}
		return values, max(slices.Index(values, current), 0)
	case ReviewSettingsFocusEffort:
		current := d.review.Reviewers[id].Effort
		values := []string{"Default"}
		if int(id) < len(d.efforts) {
			for _, choice := range d.efforts[id] {
				values = append(values, choice.Value)
			}
		}
		if current != nil && !slices.Contains(values, *current) {
			values = slices.Insert(values, 1, *current)
		}
		selected := 0
		if current != nil {
			selected = max(slices.Index(values, *current), 0)
		}
		return values, selected
	default:
		return []string{}, 0
	}
}

func (d *ReviewSettingsDialog) selectors() []reviewSettingsSelector {
	selectors := []reviewSettingsSelector{}
	for i := range d.review.Reviewers {
		selectors = append(selectors,
			reviewSettingsSelector{ReviewSettingsFocusModel(i), fmt.Sprintf("Reviewer %d model", i+1)},
			reviewSettingsSelector{ReviewSettingsFocusEffort(i), fmt.Sprintf("Reviewer %d effort", i+1)},
		)
	}
	return selectors
}

// reviewSettingsSelector is one combo box of the editor and its label.
type reviewSettingsSelector struct {
	id    ReviewSettingsFocus
	label string
}

func (d *ReviewSettingsDialog) prepare() {
	form := &d.form
	form.BeginUpdate()
	editable := !d.saving && d.pendingAutoconfigure == nil
	for _, selector := range d.selectors() {
		values, selected := d.choices(selector.id)
		form.DeclareWithEnabled(selector.id, chat.ControlKindComboBox{
			Len:      len(values),
			Selected: d.combo.Selection(selector.id, selected),
			Expanded: d.combo.IsOpen(selector.id),
		}, editable)
		if effort, ok := selector.id.(ReviewSettingsFocusEffort); ok {
			form.DeclareWithEnabled(ReviewSettingsFocusRemove(effort), chat.ControlKindButton{}, editable)
		}
	}
	form.DeclareWithEnabled(ReviewSettingsFocusAdd{}, chat.ControlKindButton{},
		editable && len(d.review.Reviewers) < core.MaxReviewers && len(d.models) > 0)
	_, suggested := d.suggestedReviewers()
	form.DeclareWithEnabled(ReviewSettingsFocusAutoconfigure{}, chat.ControlKindButton{}, editable && suggested)
	form.DeclareWithEnabled(ReviewSettingsFocusBack{}, chat.ControlKindButton{}, true)
	form.DeclareWithEnabled(ReviewSettingsFocusCancel{}, chat.ControlKindButton{}, true)
	form.DeclareWithEnabled(ReviewSettingsFocusSave{}, chat.ControlKindButton{}, d.canSave())
	form.EndFrame(d.initialFocus())
}

// initialFocus is where focus starts: the first reviewer, or the button
// that adds one.
func (d *ReviewSettingsDialog) initialFocus() ReviewSettingsFocus {
	if len(d.review.Reviewers) == 0 {
		return ReviewSettingsFocusAdd{}
	}
	return ReviewSettingsFocusModel(0)
}

func (d *ReviewSettingsDialog) handleEvent(dashboard *DashboardState, config *core.Config, event tea.Msg) (DashboardAction, ReviewSettingsOutcome) {
	result := d.form.Handle(event)
	dashboard.lastEventConsumed = result.Consumed
	interaction := d.combo.Route(result.Action)
	var action DashboardAction = DashboardActionNone{}
	outcome := ReviewSettingsOutcomeContinue
	switch interaction := interaction.(type) {
	case chat.InteractionCancel[ReviewSettingsFocus]:
		outcome = ReviewSettingsOutcomeBack
	case chat.InteractionActivate[ReviewSettingsFocus]:
		switch interaction.ID.(type) {
		case ReviewSettingsFocusBack:
			outcome = ReviewSettingsOutcomeBack
		case ReviewSettingsFocusCancel:
			outcome = ReviewSettingsOutcomeCancelSetup
		case ReviewSettingsFocusSave:
			if d.canSave() {
				outcome = ReviewSettingsOutcomeSave
			}
		}
	}
	refresh := func(config *core.Config) {
		d.updateChoices(config, &dashboard.profileCapabilities, dashboard.usagePricedProfiles(config))
	}
	switch interaction := interaction.(type) {
	case chat.InteractionComboBoxCommit[ReviewSettingsFocus]:
		switch id := interaction.ID.(type) {
		case ReviewSettingsFocusModel:
			if values, _ := d.choices(id); interaction.Index >= 0 && interaction.Index < len(values) {
				d.review.Reviewers[id].Model = values[interaction.Index]
				d.review.Reviewers[id].Effort = nil
				d.review.ClearLegacy()
				refresh(config)
			}
		case ReviewSettingsFocusEffort:
			if values, _ := d.choices(id); interaction.Index >= 0 && interaction.Index < len(values) {
				d.review.Reviewers[id].Effort = nil
				if interaction.Index != 0 {
					value := values[interaction.Index]
					d.review.Reviewers[id].Effort = &value
				}
				d.review.ClearLegacy()
			}
		}
	case chat.InteractionActivate[ReviewSettingsFocus]:
		switch id := interaction.ID.(type) {
		case ReviewSettingsFocusAdd:
			if len(d.review.Reviewers) < core.MaxReviewers && len(d.models) > 0 {
				d.review.Reviewers = append(d.review.Reviewers, core.SubagentChoice{Model: d.models[0].Value})
				d.review.ClearLegacy()
				refresh(config)
			}
		case ReviewSettingsFocusRemove:
			if int(id) < len(d.review.Reviewers) {
				d.review.Reviewers = slices.Delete(slices.Clone(d.review.Reviewers), int(id), int(id)+1)
				d.review.ClearLegacy()
				refresh(config)
			}
		case ReviewSettingsFocusAutoconfigure:
			// The cached suggestion may predate the latest quota reports,
			// which decide which profiles are billed per use.
			refresh(config)
			if reviewers, ok := d.suggestedReviewers(); ok {
				d.effortNotice = nil
				if len(d.suggestedAdjustments) > 0 {
					notice := adjustedEffortsNotice(d.suggestedAdjustments)
					d.effortNotice = &notice
				}
				updated := *config
				updated.Review.Reviewers = slices.Clone(reviewers)
				updated.Review.ClearLegacy()
				capabilities := &dashboard.profileCapabilities
				unknown := slices.ContainsFunc(reviewers, func(choice core.SubagentChoice) bool {
					model := choice.Model
					options := capabilities.Options(&updated, &model)
					if options == nil {
						return true
					}
					return len(capabilities.EffortBlockers(&updated, choice.Model)) != 0 &&
						choice.Effort != nil && !sessionConfigChoicesOffer(options.Efforts, *choice.Effort)
				})
				if unknown {
					key := updated.CapabilityDemandKey()
					dashboard.profileHydrationSent = &key
					if dashboard.profileHydrationRequests == nil {
						dashboard.profileHydrationRequests = map[string]map[string]struct{}{}
					}
					dashboard.profileHydrationRequests[key] = map[string]struct{}{}
					encoded, err := json.Marshal(updated)
					if err != nil {
						panic(fmt.Sprintf("config serializes: %v", err))
					}
					action = DashboardActionWarmProfileCapabilities{Key: key, Config: string(encoded)}
					d.pendingAutoconfigure = &pendingAutoconfigure{choices: reviewers, started: time.Now()}
				} else {
					d.review.Reviewers = reviewers
					d.review.ClearLegacy()
				}
				refresh(&updated)
			}
		case ReviewSettingsFocusModel:
			_, selected := d.choices(id)
			d.combo.Open(id, selected)
		case ReviewSettingsFocusEffort:
			_, selected := d.choices(id)
			d.combo.Open(id, selected)
		case ReviewSettingsFocusCancel:
			d.review = cloneReviewConfig(d.originalReview)
		}
	}
	d.saveError = nil
	d.prepare()
	return action, outcome
}

func renderReviewSettings(frame *chat.Frame, popup uv.Rectangle, dialog *ReviewSettingsDialog, setupSaving bool) {
	form := &dialog.form
	form.BeginFrame()
	_, comboOpen := dialog.combo.OpenID()
	title := dismissibleModalTitle(
		&form.Form,
		popup,
		"Settings › Code Review",
		chat.Title(true),
		!setupSaving && !dialog.saving && !comboOpen,
	)
	frame.RenderWidget(chat.Modal().WithTitle(title), popup)
	inner := chat.Modal().Inner(popup)
	body := uv.Rect(inner.Min.X, inner.Min.Y, inner.Dx(), max(inner.Dy()-2, 0))
	selectors := dialog.selectors()
	_, suggested := dialog.suggestedReviewers()
	var notes string
	switch {
	case dialog.pendingAutoconfigure != nil:
		notes = "Checking reviewer efforts…"
	case dialog.effortNotice != nil:
		notes = *dialog.effortNotice
	case !dialog.modelsKnown:
		notes = "Loading review models…"
	case !suggested:
		notes = "Autoconfigure unavailable: no supported models"
	case slices.Contains(dialog.effortsKnown, false):
		notes = "Loading effort choices…"
	default:
		notes = "Review runs before push when reviewers are set."
	}
	// A roomy dialog keeps each reviewer's three controls together. On a
	// narrow terminal, preserve the same focus order in three adjacent rows.
	inline := body.Dx() >= 64
	reviewerRows := 4
	notInline := 1
	if inline {
		reviewerRows = 2
		notInline = 0
	}
	actionsRow := reviewerRows * len(dialog.review.Reviewers)
	contentHeight := actionsRow + 3
	focusRow := 0
	if focused, ok := form.Focused(); ok {
		switch focused := focused.(type) {
		case ReviewSettingsFocusModel:
			focusRow = reviewerRows * int(focused)
		case ReviewSettingsFocusEffort:
			focusRow = reviewerRows*int(focused) + notInline
		case ReviewSettingsFocusRemove:
			focusRow = reviewerRows * int(focused)
			if !inline {
				focusRow += 2
			}
		case ReviewSettingsFocusAdd, ReviewSettingsFocusAutoconfigure:
			focusRow = actionsRow
		}
	}
	viewport := form.Viewport(body, contentHeight, &chat.RowRange{Start: focusRow, End: focusRow + 1})
	row := func(index int) uv.Rectangle { return viewport.Row(index, 1) }
	type expandedCombo struct {
		id       ReviewSettingsFocus
		field    uv.Rectangle
		value    string
		options  []chat.Line
		selected int
	}
	var expanded *expandedCombo
	const comboTitle = " values · ↑/↓ select · Tab/Enter accept "
	for _, selector := range selectors {
		id, label := selector.id, selector.label
		var index int
		switch id := id.(type) {
		case ReviewSettingsFocusModel:
			index = reviewerRows * int(id)
		case ReviewSettingsFocusEffort:
			index = reviewerRows*int(id) + notInline
		default:
			panic("selectors contain only combos")
		}
		area := row(index)
		var field uv.Rectangle
		if inline {
			const labelWidth = 12
			remaining := max(area.Dx()-labelWidth, 0)
			const removeWidth = 10
			const effortWidth = 18
			modelWidth := max(remaining-(removeWidth+effortWidth+2), 0)
			if _, model := id.(ReviewSettingsFocusModel); model {
				reviewer := label
				for strings.HasSuffix(reviewer, " model") {
					reviewer = strings.TrimSuffix(reviewer, " model")
				}
				frame.RenderWidget(chat.RawLine(reviewer), uv.Rect(area.Min.X, area.Min.Y, labelWidth, 1))
				field = uv.Rect(area.Min.X+labelWidth, area.Min.Y, modelWidth, 1)
			} else {
				field = uv.Rect(area.Min.X+labelWidth+modelWidth+1, area.Min.Y, effortWidth, 1)
			}
		} else {
			labelWidth := min(20, area.Dx())
			frame.RenderWidget(chat.RawLine(label), uv.Rect(area.Min.X, area.Min.Y, labelWidth, 1))
			field = uv.Rect(area.Min.X+labelWidth, area.Min.Y, area.Dx()-labelWidth, 1)
		}
		values, selected := dialog.choices(id)
		selected = dialog.combo.Selection(id, selected)
		value := ""
		if selected >= 0 && selected < len(values) {
			value = values[selected]
		}
		options := make([]chat.Line, 0, len(values))
		for _, value := range values {
			options = append(options, chat.RawLine(value))
		}
		chat.ComboBoxRender(
			frame,
			inner,
			field,
			value,
			options,
			selected,
			false,
			!dialog.saving,
			comboTitle,
			chat.PopupSideBelow,
			chat.ComboBoxDefaultMaxVisibleRows,
			&form.Form,
			id,
		)
		if dialog.combo.IsOpen(id) {
			expanded = &expandedCombo{id: id, field: field, value: value, options: options, selected: selected}
		}
		if effort, ok := id.(ReviewSettingsFocusEffort); ok {
			i := int(effort)
			removeArea := row(index + 1)
			removeLabel := fmt.Sprintf("Remove reviewer %d", i+1)
			if inline {
				removeArea = uv.Rect(max(area.Max.X-10, 0), area.Min.Y, min(10, area.Dx()), 1)
				removeLabel = "Remove"
			}
			form.RenderActions(frame, removeArea, []chat.ButtonSpec[ReviewSettingsFocus]{
				{ID: ReviewSettingsFocusRemove(i), Label: removeLabel, Enabled: !dialog.saving},
			})
			if dialog.effortUnavailable(i) || dialog.effortFailures[i] != "" {
				reason := dialog.effortFailures[i]
				if reason == "" {
					reason = "Effort unavailable for this model"
				}
				frame.RenderWidget(
					chat.StyledLine(reason, chat.Style{}.Foreground(chat.ThemePalette().Warning)),
					row(reviewerRows*i+reviewerRows-1),
				)
			}
		}
	}
	form.RenderActions(frame, row(actionsRow), []chat.ButtonSpec[ReviewSettingsFocus]{
		{
			ID:      ReviewSettingsFocusAdd{},
			Label:   "Add reviewer",
			Enabled: !dialog.saving && len(dialog.review.Reviewers) < core.MaxReviewers && len(dialog.models) > 0,
		},
		{ID: ReviewSettingsFocusAutoconfigure{}, Label: "Autoconfigure", Enabled: !dialog.saving && suggested},
	})
	frame.RenderWidget(
		chat.StyledLine(notes, chat.Style{}.Foreground(chat.ThemePalette().Muted)),
		row(actionsRow+1),
	)
	if dialog.saveError != nil {
		frame.RenderWidget(
			chat.StyledLine(*dialog.saveError, chat.Style{}.Foreground(chat.ThemePalette().Warning)),
			row(actionsRow+2),
		)
	}
	footerHeight := 0
	if inner.Dy() > 0 {
		footerHeight = 1
	}
	footer := uv.Rect(inner.Min.X, max(inner.Max.Y-1, 0), inner.Dx(), footerHeight)
	saveLabel := "Save Settings"
	if dialog.saving {
		saveLabel = "Saving…"
	}
	form.RenderActions(frame, footer, []chat.ButtonSpec[ReviewSettingsFocus]{
		{ID: ReviewSettingsFocusBack{}, Label: "Back", Enabled: true},
		{ID: ReviewSettingsFocusCancel{}, Label: "Cancel", Enabled: true},
		{ID: ReviewSettingsFocusSave{}, Label: saveLabel, Enabled: dialog.canSave()},
	})
	if expanded != nil {
		chat.ComboBoxRender(
			frame,
			inner,
			expanded.field,
			expanded.value,
			expanded.options,
			expanded.selected,
			true,
			!dialog.saving,
			comboTitle,
			chat.PopupSideBelow,
			chat.ComboBoxDefaultMaxVisibleRows,
			&form.Form,
			expanded.id,
		)
	}
	form.EndFrame(dialog.initialFocus())
}

// knownEfforts is a model's offered efforts, or false while a profile that
// may offer the model has not reported them. Profiles that do not offer the
// model are irrelevant to this.
func knownEfforts(snapshot *core.ProfileCapabilitiesSnapshot, config *core.Config, model string) ([]core.SessionConfigChoice, bool) {
	options := snapshot.Options(config, &model)
	if options == nil || len(snapshot.EffortBlockers(config, model)) != 0 {
		return nil, false
	}
	return options.Efforts, true
}
