// Port of mj-tui/src/review_settings.rs.

// The review settings editor. Capability choices come from the dashboard's
// published catalog; the editor never performs discovery on the event loop.

package tui

import (
	"fmt"
	"slices"
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
	panic("unported: mj-tui/src/review_settings.rs ReviewSettingsDialog::clone")
}

// Equal is Rust's derived PartialEq.
func (d *ReviewSettingsDialog) Equal(other *ReviewSettingsDialog) bool {
	panic("unported: mj-tui/src/review_settings.rs ReviewSettingsDialog::eq")
}

func (d *ReviewSettingsDialog) prepareDialogState() {
	d.form.SetActionRole(ReviewSettingsFocusCancel{}, chat.ActionRoleCancel)
	d.form.SetActionRole(ReviewSettingsFocusBack{}, chat.ActionRoleBack)
	d.form.SetDirty(false)
	d.form.SetDismissActions([]ReviewSettingsFocus{ReviewSettingsFocusCancel{}})
	d.form.SetDefaultAction(ReviewSettingsFocusSave{})
}

func (d *ReviewSettingsDialog) updateChoices(config *core.Config, snapshot *core.ProfileCapabilitiesSnapshot, usagePriced map[string]struct{}) {
	panic("unported: mj-tui/src/review_settings.rs ReviewSettingsDialog::update_choices")
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
	panic("unported: mj-tui/src/review_settings.rs ReviewSettingsDialog::can_save")
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
	panic("unported: mj-tui/src/review_settings.rs ReviewSettingsDialog::validation_error")
}

func (d *ReviewSettingsDialog) choices(id ReviewSettingsFocus) ([]string, int) {
	panic("unported: mj-tui/src/review_settings.rs ReviewSettingsDialog::choices")
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
	panic("unported: mj-tui/src/review_settings.rs ReviewSettingsDialog::prepare")
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
	panic("unported: mj-tui/src/review_settings.rs ReviewSettingsDialog::handle_event")
}

func renderReviewSettings(frame *chat.Frame, popup uv.Rectangle, dialog *ReviewSettingsDialog, setupSaving bool) {
	panic("unported: mj-tui/src/review_settings.rs render_review_settings")
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
