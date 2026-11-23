// Port of mj-tui/src/wizards/tests.rs.
//
// ratatui's Terminal over a TestBackend is [wizardsTerminal] here: each draw
// renders a fresh frame into a buffer of the terminal's size and keeps it.
// Rust's test-only render::render is [wizardsRender]. Rust compares actions
// with their derived PartialEq; [wzEqual] does the same over Go values,
// where an empty slice or map and a nil one are the same value, as they are
// for a Rust Vec or BTreeMap.

package tui

import (
	"errors"
	"fmt"
	"maps"
	"path/filepath"
	"reflect"
	"slices"
	"strconv"
	"strings"
	"testing"
	"unicode"
	"unicode/utf8"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// goldenCapture is Rust's thread-local GOLDEN_CAPTURE. The tests of this
// package do not run in parallel, so one package variable serves.
type goldenCapture struct {
	label      *string
	lastAction *string
	output     strings.Builder
}

var wizardsGoldenCapture goldenCapture

func captureScenario(t *testing.T, label string, run func(*testing.T)) {
	t.Helper()
	wizardsGoldenCapture.label = &label
	wizardsGoldenCapture.lastAction = nil
	// A failing scenario leaves through t.Fatal; the label must not outlive
	// it into the next test.
	defer func() { wizardsGoldenCapture.label = nil }()
	run(t)
}

func captureAction(action DashboardAction) {
	if wizardsGoldenCapture.label == nil {
		return
	}
	text := wzDebug(action)
	if last := wizardsGoldenCapture.lastAction; last != nil && *last == text {
		return
	}
	fmt.Fprintf(&wizardsGoldenCapture.output, "action: %s\n", text)
	wizardsGoldenCapture.lastAction = &text
}

func captureDetail(label, value string) {
	if wizardsGoldenCapture.label != nil {
		fmt.Fprintf(&wizardsGoldenCapture.output, "%s: %s\n", label, value)
	}
}

func captureRender(width, height int, lines []string) {
	if label := wizardsGoldenCapture.label; label != nil {
		fmt.Fprintf(&wizardsGoldenCapture.output, "=== %s (%dx%d) ===\n%s\n", *label, width, height, strings.Join(lines, "\n"))
	}
}

func setLocalContainerCapacity(dashboard *DashboardState, total, committed, remaining core.HostContainerSize, complete bool) {
	setContainerCapacityForTargets(dashboard, []string{"podman"}, total, committed, remaining, complete)
}

func setContainerCapacityForTargets(dashboard *DashboardState, targetIDs []string, total, committed, remaining core.HostContainerSize, complete bool) {
	reserved := core.ReservedContainerCapacity(total)
	dashboard.SetContainerCapacities(map[string]client.RuntimeContainerCapacity{
		"local": {
			TargetIDs: targetIDs,
			Capacity: core.HostContainerCapacity{
				Total:     total,
				Reserved:  reserved,
				Committed: committed,
				Remaining: remaining,
				Complete:  complete,
			},
		},
	})
}

func capturedDrawn(dashboard *DashboardState, width, height int) []string {
	lines := drawn(dashboard, width, height)
	captureRender(width, height, lines)
	return lines
}

func takeGoldenOutput() string {
	output := wizardsGoldenCapture.output.String()
	wizardsGoldenCapture.output.Reset()
	return output
}

// wizardsRender draws the combined surface with no conversation on it, as
// Rust's test-only render::render does.
func wizardsRender(frame *chat.Frame, dashboard *DashboardState) {
	withNoColorOverrideForTest(true, func() struct{} {
		renderCombinedForTest(frame, dashboard, map[string]*chat.ActiveChat{}, map[PaneId]string{}, false)
		return struct{}{}
	})
}

// wizardsTerminal stands in for ratatui's Terminal over a TestBackend.
type wizardsTerminal struct {
	width, height int
	buffer        *uv.Buffer
}

func newWizardsTerminal(width, height int) *wizardsTerminal {
	return &wizardsTerminal{width: width, height: height, buffer: uv.NewBuffer(width, height)}
}

// draw renders one frame of the dashboard and keeps its buffer.
func (t *wizardsTerminal) draw(dashboard *DashboardState) {
	t.buffer = uv.NewBuffer(t.width, t.height)
	wizardsRender(chat.NewFrame(t.buffer), dashboard)
}

// lines is the last drawn buffer as one string per row.
func (t *wizardsTerminal) lines() []string {
	return bufferLines(t.buffer)
}

// wzEqual is Rust's derived PartialEq over Go values: structs field by
// field (unexported fields included), pointers and interfaces by what they
// hold, and nil slices and maps equal to empty ones.
func wzEqual(a, b any) bool {
	return wzEqualValues(reflect.ValueOf(a), reflect.ValueOf(b))
}

func wzEqualValues(a, b reflect.Value) bool {
	if !a.IsValid() || !b.IsValid() {
		return a.IsValid() == b.IsValid()
	}
	if a.Type() != b.Type() {
		return false
	}
	switch a.Kind() {
	case reflect.Pointer:
		if a.IsNil() || b.IsNil() {
			return a.IsNil() == b.IsNil()
		}
		if a.Pointer() == b.Pointer() {
			return true
		}
		return wzEqualValues(a.Elem(), b.Elem())
	case reflect.Interface:
		if a.IsNil() || b.IsNil() {
			return a.IsNil() == b.IsNil()
		}
		return wzEqualValues(a.Elem(), b.Elem())
	case reflect.Struct:
		for i := range a.NumField() {
			if !wzEqualValues(a.Field(i), b.Field(i)) {
				return false
			}
		}
		return true
	case reflect.Slice, reflect.Array:
		if a.Len() != b.Len() {
			return false
		}
		for i := range a.Len() {
			if !wzEqualValues(a.Index(i), b.Index(i)) {
				return false
			}
		}
		return true
	case reflect.Map:
		if a.Len() != b.Len() {
			return false
		}
		iter := a.MapRange()
		for iter.Next() {
			other := b.MapIndex(iter.Key())
			if !other.IsValid() || !wzEqualValues(iter.Value(), other) {
				return false
			}
		}
		return true
	case reflect.Bool:
		return a.Bool() == b.Bool()
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		return a.Int() == b.Int()
	case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64, reflect.Uintptr:
		return a.Uint() == b.Uint()
	case reflect.Float32, reflect.Float64:
		return a.Float() == b.Float()
	case reflect.Complex64, reflect.Complex128:
		return a.Complex() == b.Complex()
	case reflect.String:
		return a.String() == b.String()
	case reflect.Func, reflect.Chan, reflect.UnsafePointer:
		return a.Pointer() == b.Pointer()
	}
	panic(fmt.Sprintf("wzEqual: unsupported kind %v", a.Kind()))
}

// wzAction fails t unless got is want.
func wzAction(t testing.TB, got, want DashboardAction, context ...any) {
	t.Helper()
	if !wzEqual(got, want) {
		t.Fatalf("action %s, want %s %s", wzDebug(got), wzDebug(want), fmt.Sprint(context...))
	}
}

// wzContains fails t unless text contains needle.
func wzContains(t testing.TB, text, needle string) {
	t.Helper()
	if !strings.Contains(text, needle) {
		t.Fatalf("missing %q in:\n%s", needle, text)
	}
}

// wzNotContains fails t if text contains needle.
func wzNotContains(t testing.TB, text, needle string) {
	t.Helper()
	if strings.Contains(text, needle) {
		t.Fatalf("unexpected %q in:\n%s", needle, text)
	}
}

// wzDebug renders an action as Rust's derived Debug does, for the actions
// the goldens record; any other action falls back to Go's own notation,
// which keeps distinct actions distinct.
func wzDebug(action DashboardAction) string {
	switch action := action.(type) {
	case nil:
		return "<no action>"
	case DashboardActionNone:
		return "None"
	case DashboardActionLoadMountHistory:
		return "LoadMountHistory"
	case DashboardActionCreateBundle:
		return fmt.Sprintf("CreateBundle { sources: %s }", wzDebugStrings(action.Sources))
	case DashboardActionValidateProjectDirectory:
		return fmt.Sprintf("ValidateProjectDirectory { target_template_id: %s, directory: %s }",
			wzDebugString(action.TargetTemplateID), wzDebugString(action.Directory))
	case DashboardActionPreflightResumeRepositories:
		return fmt.Sprintf("PreflightResumeRepositories { launch: %s }", wzDebug(action.Launch))
	case DashboardActionPreflightCreateSession:
		return fmt.Sprintf("PreflightCreateSession { launch: %s }", wzDebug(action.Launch))
	case DashboardActionResumeSession:
		return fmt.Sprintf("ResumeSession { workspace_id: %s, session_id: %s, profile_id: %s, target_template_id: %s, additional_mounts: %s, resource_allocation: %s, discard_queue: %t }",
			wzDebugString(action.WorkspaceID), wzDebugString(action.SessionID), wzDebugString(action.ProfileID),
			wzDebugString(action.TargetTemplateID), wzDebugMounts(action.AdditionalMounts),
			wzDebugAllocation(action.ResourceAllocation), action.DiscardQueue)
	case DashboardActionCreateSession:
		return fmt.Sprintf("CreateSession { remember_container_size: %t, create_managed_worktree: %s, subagents: %s, oracle: %s, oracle_choice: %s, review: %s, workspace_id: %s, profile_id: %s, provider: %s, bundle_id: %s, project_directory: %s, target_template_id: %s, additional_mounts: %s, resource_allocation: %s, carried: CarriedLaunchSettings { no_project_memory: %t, no_github_events: %t, no_github_credentials: %t, note: %s } }",
			action.RememberContainerSize, wzDebugOptionBool(action.CreateManagedWorktree),
			wzDebugOption(action.Subagents, wzDebugPolicy), wzDebugOptionBool(action.Oracle),
			wzDebugOption(action.OracleChoice, wzDebugChoice),
			wzDebugOption(action.Review, func(review core.SessionReview) string { return fmt.Sprintf("%+v", review) }),
			wzDebugString(action.WorkspaceID), wzDebugString(action.ProfileID),
			wzDebugOption(action.Provider, func(provider core.QuotaProvider) string { return fmt.Sprint(provider) }),
			wzDebugString(action.BundleID), wzDebugOption(action.ProjectDirectory, wzDebugString),
			wzDebugString(action.TargetTemplateID), wzDebugMounts(action.AdditionalMounts),
			wzDebugAllocation(action.ResourceAllocation), action.Carried.NoProjectMemory,
			action.Carried.NoGithubEvents, action.Carried.NoGithubCredentials,
			wzDebugOption(action.Carried.Note, wzDebugString))
	case DashboardActionMoveSession:
		return fmt.Sprintf("MoveSession { subagents: %s, oracle: %s, oracle_choice: %s, consent: %s, skip_keep_paths: %s, session_id: %s, profile_id: %s, target_template_id: %s, additional_mounts: %s, resource_allocation: %s, clear_resource_allocation: %t, preparation_request_id: %s, queue: %s }",
			wzDebugOption(action.Subagents, wzDebugPolicy), wzDebugOptionBool(action.Oracle),
			wzDebugOption(action.OracleChoice, wzDebugChoice),
			wzDebugConsent(action.Consent), wzDebugStrings(action.SkipKeepPaths),
			wzDebugString(action.SessionID), wzDebugString(action.ProfileID),
			wzDebugString(action.TargetTemplateID), wzDebugMounts(action.AdditionalMounts),
			wzDebugAllocation(action.ResourceAllocation), action.ClearResourceAllocation,
			wzDebugOption(action.PreparationRequestID, func(id uint64) string { return strconv.FormatUint(id, 10) }),
			wzDebugOption(action.Queue, func(queue core.ResumeQueueDisposition) string { return wzDebugQueue(queue) }))
	}
	return fmt.Sprintf("%T%+v", action, action)
}

// wzDebugString is Rust's `{:?}` for a str.
func wzDebugString(text string) string {
	return strconv.Quote(text)
}

func wzDebugStrings(texts []string) string {
	quoted := make([]string, len(texts))
	for i, text := range texts {
		quoted[i] = wzDebugString(text)
	}
	return "[" + strings.Join(quoted, ", ") + "]"
}

func wzDebugOption[T any](value *T, debug func(T) string) string {
	if value == nil {
		return "None"
	}
	return "Some(" + debug(*value) + ")"
}

func wzDebugOptionBool(value *bool) string {
	return wzDebugOption(value, func(value bool) string { return strconv.FormatBool(value) })
}

func wzDebugPolicy(policy core.SubagentPolicy) string {
	switch policy.Mode {
	case core.SubagentPolicyNone:
		return "None"
	case core.SubagentPolicyMjolnir:
		return fmt.Sprintf("Mjolnir { straightforward: %s, subtle: %s, orchestrators: %t }",
			wzDebugOption(policy.Straightforward, wzDebugChoice),
			wzDebugOption(policy.Subtle, wzDebugChoice),
			policy.Orchestrators)
	}
	return "Native"
}

// wzDebugMountAccess is the variant name Rust's Debug prints (Ro, Cow, Rw).
func wzDebugMountAccess(access core.MountAccess) string {
	text := string(access)
	if text == "" {
		return text
	}
	return strings.ToUpper(text[:1]) + text[1:]
}

func wzDebugChoice(choice core.SubagentChoice) string {
	return fmt.Sprintf("SubagentChoice { model: %s, effort: %s }",
		wzDebugString(choice.Model), wzDebugOption(choice.Effort, wzDebugString))
}

func wzDebugMounts(mounts []core.AdditionalMount) string {
	rendered := make([]string, len(mounts))
	for i, mount := range mounts {
		rendered[i] = fmt.Sprintf("AdditionalMount { source: %s, destination: %s, access: %s }",
			wzDebugString(mount.Source), wzDebugString(mount.Destination), wzDebugMountAccess(mount.Access))
	}
	return "[" + strings.Join(rendered, ", ") + "]"
}

func wzDebugAllocation(allocation core.SessionResourceAllocation) string {
	switch allocation := allocation.(type) {
	case nil:
		return "None"
	case core.SessionResourceAllocationContainer:
		return fmt.Sprintf("Some(Container { cpus: %d, memory_bytes: %d })", allocation.Cpus, allocation.MemoryBytes)
	}
	return fmt.Sprintf("Some(%+v)", allocation)
}

func wzDebugConsent(consent core.SuspendConsent) string {
	return fmt.Sprintf("SuspendConsent { discard_untracked: %s, abort_operations: %s, without_checkpoint: %t, confirm_no_checkpoint: %t }",
		wzDebugConsentKind(consent.DiscardUntracked.Kind, consent.DiscardUntracked.Shown),
		wzDebugConsentKind(consent.AbortOperations.Kind, consent.AbortOperations.Shown),
		consent.WithoutCheckpoint, consent.ConfirmNoCheckpoint)
}

func wzDebugConsentKind[T any](kind core.ConsentKind, shown []T) string {
	switch kind {
	case core.ConsentAny:
		return "Any"
	case core.ConsentShown:
		return fmt.Sprintf("Shown(%+v)", shown)
	}
	return "None"
}

func wzDebugQueue(queue core.ResumeQueueDisposition) string {
	text := fmt.Sprint(queue)
	if text == "" {
		return text
	}
	return strings.ToUpper(text[:1]) + text[1:]
}

// wzDebugRequest is Rust's derived Debug for a project discovery request.
func wzDebugRequest(request core.ProjectDiscoveryRequest) string {
	switch request := request.(type) {
	case core.ProjectDiscoveryRequestGithub:
		return fmt.Sprintf("Github { query: %s }", wzDebugString(request.Query))
	case core.ProjectDiscoveryRequestDirectory:
		return fmt.Sprintf("Directory { path: %s, filter: %s }", wzDebugString(request.Path), wzDebugString(request.Filter))
	}
	return fmt.Sprintf("%+v", request)
}

func wzPtr[T any](value T) *T {
	return &value
}

// wzNewDashboard is Rust's `DashboardState::new(config, state,
// BTreeMap::new())`.
func wzNewDashboard(configuration core.Config, state core.StateState) *DashboardState {
	return NewDashboardState(configuration, state, map[string]client.ProfileQuota{})
}

// wzFocusIs fails t unless control holds the form's focus.
func wzFocusIs(t testing.TB, form *chat.Dialog[WizardControl], control WizardControl) {
	t.Helper()
	focused, ok := form.Focused()
	if !ok || focused != control {
		t.Fatalf("focus %v (any focus: %t), want %v", focused, ok, control)
	}
}

// wzRemoteHel is the single remote repository the creation preflight
// reports for the shared `hel` project.
func wzRemoteHel() []RemoteRepositoryPreview {
	return []RemoteRepositoryPreview{{
		RepositoryID:  "hel",
		FetchURL:      "https://github.com/example/hel.git",
		DefaultBranch: "main",
		PushURLs:      []string{"https://github.com/example/hel.git"},
	}}
}

// wzConfigWithoutBundle is a copy of configuration that no longer holds
// bundleID; the copy owns its bundle map, as Rust's clone does.
func wzConfigWithoutBundle(configuration *core.Config, bundleID string) core.Config {
	removed := *configuration
	removed.Bundles = maps.Clone(removed.Bundles)
	delete(removed.Bundles, bundleID)
	return removed
}

// Hard-won: 93d7fde8: New review could not finish because only disabled Create started preflight
func TestIsolatedCreationReviewChecksPrerequisitesBeforeEnablingCreate(t *testing.T) {
	dashboard := wzNewDashboard(config(), core.NewStateState())
	readyOpenNewWizard(dashboard)
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	if check, _ := dashboard.TakePrerequisiteCheck(); !isPreflightCreateSession(check) {
		t.Fatalf("expected a creation preflight, got %s", wzDebug(check))
	}

	// Draw between inputs as the terminal does: Enter on a Create button that
	// was drawn disabled must not act until a frame shows it enabled.
	terminal := newWizardsTerminal(120, 30)
	terminal.draw(dashboard)
	wzContains(t, strings.Join(terminal.lines(), "\n"), "Checking prerequisites…")
	wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionNone{})

	generation := dashboard.SessionPreflightGeneration()
	dashboard.BeginRemoteSessionPreflight(generation)
	dashboard.ApplyRemoteSessionPreflight(generation, wzRemoteHel(), nil)
	terminal.draw(dashboard)
	action := readyKey(dashboard, key(tea.KeyEnter))
	if _, ok := action.(DashboardActionCreateSession); !ok {
		t.Fatalf("expected CreateSession, got %s", wzDebug(action))
	}
	if _, ok := dashboard.mode.(ModeDashboard); !ok {
		t.Fatalf("mode %T, want the dashboard", dashboard.mode)
	}
}

func isPreflightCreateSession(action DashboardAction) bool {
	_, ok := action.(DashboardActionPreflightCreateSession)
	return ok
}

func TestPendingCreationPreflightKeepsBackAndCancelLiveAndIgnoresLateResults(t *testing.T) {
	for _, control := range []WizardControl{WizardControlBack, WizardControlCancel} {
		dashboard := wzNewDashboard(config(), core.NewStateState())
		readyOpenNewWizard(dashboard)
		for range 3 {
			readyKey(dashboard, key(tea.KeyEnter))
		}
		if check, _ := dashboard.TakePrerequisiteCheck(); !isPreflightCreateSession(check) {
			t.Fatalf("expected a creation preflight, got %s", wzDebug(check))
		}
		generation := dashboard.SessionPreflightGeneration()
		dashboard.BeginRemoteSessionPreflight(generation)
		projectWizard(dashboard).form.Focus(control)
		readyKey(dashboard, key(tea.KeyEnter))
		dashboard.ApplyRemoteSessionPreflight(generation, nil, errors.New("late failure"))
		switch control {
		case WizardControlBack:
			wizard := projectWizard(dashboard)
			if wizard.step != WizardStepBundle {
				t.Fatalf("step %v, want Bundle", wizard.step)
			}
			if wizard.remotePreflightInFlight {
				t.Fatal("the late result must not restart the preflight")
			}
			if wizard.remotePreflightError != nil {
				t.Fatalf("late error recorded: %q", *wizard.remotePreflightError)
			}
		case WizardControlCancel:
			if _, ok := dashboard.mode.(ModeDashboard); !ok {
				t.Fatalf("mode %T, want the dashboard", dashboard.mode)
			}
		default:
			panic("unreachable")
		}
	}
}

func TestIsolatedCreationRunsOneCheckAtATimeAndRetriesAfterFailure(t *testing.T) {
	dashboard := wzNewDashboard(config(), core.NewStateState())
	readyOpenNewWizard(dashboard)
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	originalCheck, ok := dashboard.TakePrerequisiteCheck()
	if !ok {
		t.Fatal("opening the review starts its prerequisite check")
	}
	if !isPreflightCreateSession(originalCheck) {
		t.Fatalf("expected a creation preflight, got %s", wzDebug(originalCheck))
	}
	if check, ok := dashboard.TakePrerequisiteCheck(); ok {
		t.Fatalf("a running check must not start a second resolver worker: %s", wzDebug(check))
	}
	wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionNone{})

	generation := dashboard.SessionPreflightGeneration()
	dashboard.ApplyRemoteSessionPreflight(generation, nil, errors.New("remote unavailable"))
	if check, ok := dashboard.TakePrerequisiteCheck(); ok {
		t.Fatalf("a failed check waits for Retry: %s", wzDebug(check))
	}
	terminal := newWizardsTerminal(120, 30)
	terminal.draw(dashboard)
	wzContains(t, strings.Join(terminal.lines(), "\n"), "Retry")
	wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionNone{})
	retried, ok := dashboard.TakePrerequisiteCheck()
	if !ok || !wzEqual(retried, originalCheck) {
		t.Fatalf("Retry starts the same check again: %s", wzDebug(retried))
	}
}

func TestSwitchingWorkspacesPreservesRemotePreflightInItsOriginalWorkspace(t *testing.T) {
	workspaceA := core.DefaultWorkspaceID
	workspaceB := "workspace-b"
	dashboard := wzNewDashboard(config(), core.NewStateState())

	readyOpenNewWizard(dashboard)
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	check, _ := dashboard.TakePrerequisiteCheck()
	preflightA, ok := check.(DashboardActionPreflightCreateSession)
	if !ok {
		t.Fatal("network creation should start a remote preflight")
	}
	if launch, ok := preflightA.Launch.(DashboardActionCreateSession); !ok || launch.WorkspaceID != workspaceA {
		t.Fatalf("launch %s, want a creation in %s", wzDebug(preflightA.Launch), workspaceA)
	}

	generationA := dashboard.SessionPreflightGeneration()
	dashboard.BeginRemoteSessionPreflight(generationA)
	dashboard.SetActiveWorkspace(&workspaceB)
	if !dashboard.ModalOpen() {
		t.Fatal("switching tabs preserves the wizard")
	}
	if generation := dashboard.SessionPreflightGeneration(); generation != generationA {
		t.Fatalf("generation %d, want %d", generation, generationA)
	}

	dashboard.ApplyRemoteSessionPreflight(generationA, wzRemoteHel(), nil)
	completed := readyKey(dashboard, key(tea.KeyEnter))
	if create, ok := completed.(DashboardActionCreateSession); !ok || create.WorkspaceID != workspaceA {
		t.Fatalf("completed %s, want a creation in %s", wzDebug(completed), workspaceA)
	}
	if active, ok := dashboard.ActiveWorkspaceID(); !ok || active != workspaceB {
		t.Fatalf("active workspace %q (%t), want %q", active, ok, workspaceB)
	}

	readyOpenNewWizard(dashboard)
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	check, _ = dashboard.TakePrerequisiteCheck()
	preflightB, ok := check.(DashboardActionPreflightCreateSession)
	if !ok {
		t.Fatal("workspace B should start its own remote preflight")
	}
	if launch, ok := preflightB.Launch.(DashboardActionCreateSession); !ok || launch.WorkspaceID != workspaceB {
		t.Fatalf("launch %s, want a creation in %s", wzDebug(preflightB.Launch), workspaceB)
	}
}

func dashboardAtProjectPicker(t testing.TB) *DashboardState {
	t.Helper()
	configuration := config()
	clear(configuration.Bundles)
	dashboard := wzNewDashboard(configuration, core.NewStateState())
	readyOpenNewWizard(dashboard)
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	if step := projectWizard(dashboard).step; step != WizardStepNewBundle {
		t.Fatalf("step %v, want NewBundle", step)
	}
	return dashboard
}

func projectWizard(dashboard *DashboardState) *NewWizard {
	mode, ok := dashboard.mode.(ModeNew)
	if !ok {
		panic("expected the new-session wizard")
	}
	return mode.NewWizard
}

func focusProjectControl(dashboard *DashboardState, control WizardControl) {
	projectWizard(dashboard).form.Focus(control)
}

func activateProjectControl(dashboard *DashboardState, control WizardControl) DashboardAction {
	focusProjectControl(dashboard, control)
	return readyKey(dashboard, key(tea.KeyEnter))
}

func typeSource(dashboard *DashboardState, source string) {
	for _, character := range source {
		readyKey(dashboard, key(character))
	}
}

func takeProjectRequest(t testing.TB, dashboard *DashboardState) (string, core.ProjectDiscoveryRequest) {
	t.Helper()
	discover, ok := dashboard.TakeProjectDiscovery().(DashboardActionDiscoverProjects)
	if !ok {
		t.Fatal("expected a pending project discovery request")
	}
	if next := dashboard.TakeProjectDiscovery(); next != nil {
		t.Fatalf("a second project discovery request is pending: %s", wzDebug(next))
	}
	captureDetail("project request", wzDebugRequest(discover.Request))
	return discover.Context, discover.Request
}

func discoveredRepository(source string) core.ProjectEntry {
	return core.ProjectEntry{
		Name:        source[strings.LastIndex(source, "/")+1:],
		Source:      source,
		Description: source,
		Kind:        core.ProjectEntryKindRepository,
	}
}

func discoveredProjects(entries []core.ProjectEntry) core.ProjectDiscovery {
	return core.ProjectDiscovery{Entries: entries}
}

func drawProjectPicker(dashboard *DashboardState, terminal *wizardsTerminal) []string {
	dashboard.resetComponentGeometry()
	terminal.draw(dashboard)
	lines := terminal.lines()
	captureRender(terminal.width, terminal.height, lines)
	return lines
}

func clickProjectText(t testing.TB, dashboard *DashboardState, terminal *wizardsTerminal, label string) DashboardAction {
	t.Helper()
	lines := drawProjectPicker(dashboard, terminal)
	row, column := -1, 0
	for index, line := range lines {
		if offset := strings.Index(line, label); offset >= 0 {
			row, column = index, utf8.RuneCountInString(line[:offset])
			break
		}
	}
	if row < 0 {
		t.Fatalf("missing clickable %q:\n%s", label, strings.Join(lines, "\n"))
	}
	var action DashboardAction = DashboardActionNone{}
	for _, kind := range []tea.MouseMsg{
		tea.MouseClickMsg{Button: tea.MouseLeft},
		tea.MouseReleaseMsg{Button: tea.MouseLeft},
	} {
		next := dashboard.HandleMouse(mouseAt(kind, uv.Pos(column, row)))
		captureAction(next)
		if !wzEqual(next, DashboardActionNone{}) {
			wzAction(t, action, DashboardActionNone{}, "one click submits only once")
			action = next
		}
		drawProjectPicker(dashboard, terminal)
	}
	return action
}

func projectPickerSkipsEmptySavedProjectsAndOffersGithubFromRecent(t *testing.T) {
	dashboard := dashboardAtProjectPicker(t)
	wizard := projectWizard(dashboard)
	if wizard.projectPicker.tab != ProjectTabRecent {
		t.Fatalf("tab %v, want Recent", wizard.projectPicker.tab)
	}
	wzFocusIs(t, &wizard.form, WizardControlProjectGithub)
	if wizard.projectPicker.multiple {
		t.Fatal("the picker starts with one repository")
	}
	discovery := dashboard.TakeProjectDiscovery()
	if discovery != nil {
		captureAction(discovery)
	}
	wzAction(t, discovery, DashboardActionLoadMountHistory{})
	terminal := newWizardsTerminal(80, 24)
	text := strings.Join(drawProjectPicker(dashboard, terminal), "\n")
	wzContains(t, text, "Choose a project")
	wzContains(t, text, "No recent projects yet")
	wzNotContains(t, text, "No bundles yet")
	wzNotContains(t, text, "Add repository")
	wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionNone{})
	if _, request := takeProjectRequest(t, dashboard); !wzEqual(request, core.ProjectDiscoveryRequest(core.ProjectDiscoveryRequestGithub{Query: ""})) {
		t.Fatalf("request %s, want an empty GitHub search", wzDebugRequest(request))
	}
	wzAction(t, activateProjectControl(dashboard, WizardControlBack), DashboardActionNone{})
	if step := projectWizard(dashboard).step; step != WizardStepTarget {
		t.Fatalf("step %v, want Target", step)
	}
}

// twoProjectConfig is a saved-projects fixture: `hel` from the shared config,
// plus a second project with two repositories, one of them a path too long
// for a row.
func twoProjectConfig() core.Config {
	configuration := config()
	longPath := "/tmp/claude-1000/-home-jonathan-Projects-hel/b295cc91-b89b-4bf0-9d1a-0123456789ab/bifrost"
	configuration.Bundles["bifrost2"] = core.ProjectBundle{
		PrimaryRepo: "bifrost-dev",
		Repositories: []core.ProjectRepository{
			{
				ID:          "bifrost-dev",
				Github:      wzPtr("BrokkAi/bifrost-dev"),
				Destination: "bifrost-dev",
			},
			{
				ID:          "bifrost",
				Local:       wzPtr(longPath),
				Destination: "bifrost",
			},
		},
	}
	return configuration
}

func dashboardAtSavedProjects(t testing.TB, state core.StateState) *DashboardState {
	t.Helper()
	dashboard := wzNewDashboard(twoProjectConfig(), state)
	readyOpenNewWizard(dashboard)
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	if step := projectWizard(dashboard).step; step != WizardStepBundle {
		t.Fatalf("step %v, want Bundle", step)
	}
	return dashboard
}

func selectSavedProject(dashboard *DashboardState, bundleID string) {
	index := slices.Index(bundleIDsByRecentCreation(dashboard.config.Get(), dashboard.launchRecency), bundleID)
	if index < 0 {
		panic(fmt.Sprintf("no saved project %q", bundleID))
	}
	wizard := projectWizard(dashboard)
	wizard.bundle = index
	wizard.form.Focus(WizardControlBundleList)
}

// wzMissingProjectBundle is a saved project whose only repository is a local
// directory that does not exist.
func wzMissingProjectBundle(missing string) core.ProjectBundle {
	return core.ProjectBundle{
		PrimaryRepo: "missingpath",
		Repositories: []core.ProjectRepository{{
			ID:          "missingpath",
			Local:       wzPtr(missing),
			Destination: "missingpath",
		}},
	}
}

// A configured project whose local directory is gone stays listed, marked
// unavailable with the reason. Finding T-6.
// Hard-won: d07fc1f0: missing saved project directories appeared usable and leaked raw git errors
func TestASavedProjectWithAMissingDirectoryIsMarkedUnavailable(t *testing.T) {
	missing := "/definitely/not/here/missingpath"
	configuration := config()
	configuration.Bundles["missingpath"] = wzMissingProjectBundle(missing)
	dashboard := wzNewDashboard(configuration, core.NewStateState())
	// The dashboard loop asks for checks after every event; collect them
	// while moving to the project step.
	var checks []DashboardAction
	collect := func(dashboard *DashboardState) {
		for {
			action, ok := dashboard.TakePrerequisiteCheck()
			if !ok {
				return
			}
			checks = append(checks, action)
		}
	}
	openNewSessionWizard(dashboard)
	collect(dashboard)
	for range 2 {
		dashboard.HandleKey(key(tea.KeyEnter))
		collect(dashboard)
	}
	if step := projectWizard(dashboard).step; step != WizardStepBundle {
		t.Fatalf("step %v, want Bundle", step)
	}
	if want := []DashboardAction{DashboardActionCheckProjectDirectories{Paths: []string{missing}}}; !wzEqual(checks, want) {
		t.Fatalf("the directory is checked once, off the draw loop: %v", checks)
	}
	terminal := newWizardsTerminal(120, 30)
	before := strings.Join(drawProjectPicker(dashboard, terminal), "\n")
	wzContains(t, before, "missingpath")
	wzNotContains(t, before, "unavailable")

	// The answer marks the row.
	dashboard.ApplyProjectDirectoryCheck(missing, wzPtr(false))
	selectSavedProject(dashboard, "missingpath")
	lines := drawProjectPicker(dashboard, terminal)
	text := strings.Join(lines, "\n")
	if !slices.ContainsFunc(lines, func(line string) bool {
		return strings.Contains(line, "missingpath") && strings.Contains(line, "(unavailable")
	}) {
		t.Fatalf("no unavailable row:\n%s", text)
	}
	// Read the dialog column: surrounding pane borders can sit between
	// wrapped words in the full dashboard capture.
	var column []string
	for _, line := range lines {
		if parts := strings.Split(line, "│"); len(parts) > 2 {
			column = append(column, parts[2])
		}
	}
	joined := strings.Join(strings.Fields(strings.Join(column, " ")), " ")
	if !strings.Contains(joined, "Unavailable: the directory") || !strings.Contains(joined, "does not exist.") {
		t.Fatalf("no reason in the details:\n%s", text)
	}

	// An unavailable local path does not disable removing the saved project.
	wzAction(t, clickProjectText(t, dashboard, terminal, "Remove"), DashboardActionRemoveBundle{BundleID: "missingpath"})
	dashboard.FailBundleRemoval("simulated persistence error")
	failure := strings.Join(drawProjectPicker(dashboard, terminal), "\n")
	if !strings.Contains(failure, "Could not remove project: simulated persistence error") {
		t.Fatalf("the async failure must be shown inside the picker:\n%s", failure)
	}

	wzAction(t, clickProjectText(t, dashboard, terminal, "Remove"), DashboardActionRemoveBundle{BundleID: "missingpath"})
	retry := strings.Join(drawProjectPicker(dashboard, terminal), "\n")
	wzContains(t, retry, "Removing project…")
	wzNotContains(t, retry, "simulated persistence error")

	dashboard.ApplyRemovedBundle(wzConfigWithoutBundle(dashboard.config.Get(), "missingpath"), "missingpath")
	success := strings.Join(drawProjectPicker(dashboard, terminal), "\n")
	wzNotContains(t, success, "missingpath ·")
	wzNotContains(t, success, "/definitely/not/here/missingpath")
	wzNotContains(t, success, "simulated persistence error")
}

// Hard-won: 848b4781f: unavailable-project removal refusal appeared only in the dashboard notice behind the modal.
func TestRefusalToRemoveUnavailableProjectIsVisibleInsidePicker(t *testing.T) {
	missing := "/definitely/not/here/missingpath"
	configuration := twoProjectConfig()
	configuration.Bundles["missingpath"] = wzMissingProjectBundle(missing)
	session := stoppedSession()
	session.BundleID = "missingpath"
	state := core.NewStateState()
	state.EnvironmentOwners.Insert(session.ID, session.ID)
	state.Sessions.Insert(session.ID, session)
	dashboard := wzNewDashboard(configuration, state)
	openNewSessionWizard(dashboard)
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	dashboard.ApplyProjectDirectoryCheck(missing, wzPtr(false))
	selectSavedProject(dashboard, "missingpath")

	terminal := newWizardsTerminal(120, 16)
	wzAction(t, clickProjectText(t, dashboard, terminal, "Remove"), DashboardActionNone{})
	text := strings.Join(drawProjectPicker(dashboard, terminal), "\n")
	if !strings.Contains(text, "is used by a session") {
		t.Fatalf("the modal must show the refusal at short height:\n%s", text)
	}
	wzContains(t, text, "(unavailable)")

	focusProjectControl(dashboard, WizardControlBundleList)
	wzAction(t, readyKey(dashboard, key(tea.KeyDown)), DashboardActionNone{})
	if selected := nthBundleKey(dashboard.config.Get(), dashboard.launchRecency, projectWizard(dashboard).bundle); selected == "missingpath" {
		t.Fatal("Down must leave the refused project")
	}
	changedSelection := strings.Join(drawProjectPicker(dashboard, terminal), "\n")
	wzNotContains(t, changedSelection, "is used by a session")
}

func savedProjectsShowEverySourceOfTheSelectionAndStackAddAndRemove(t *testing.T) {
	dashboard := dashboardAtSavedProjects(t, core.NewStateState())
	selectSavedProject(dashboard, "bifrost2")
	terminal := newWizardsTerminal(120, 30)
	lines := drawProjectPicker(dashboard, terminal)
	text := strings.Join(lines, "\n")
	wzContains(t, text, "choose a project")
	// Rows name the first source and count the rest.
	if !slices.ContainsFunc(lines, func(line string) bool {
		return strings.Contains(line, "bifrost2") && strings.Contains(line, "BrokkAi/bifrost-dev  +1 more")
	}) {
		t.Fatalf("no summarized row:\n%s", text)
	}
	// The details under the list spell out every source of the selection,
	// wrapped rather than cut, and name the primary repository.
	wzContains(t, text, "bifrost2 · 2 repositories")
	wzContains(t, text, "BrokkAi/bifrost-dev  (primary)")
	var joined strings.Builder
	for _, line := range lines {
		joined.WriteString(strings.TrimFunc(line, func(r rune) bool { return r == '│' || unicode.IsSpace(r) }))
	}
	wzContains(t, joined.String(), "0123456789ab/bifrost")
	// Add and Remove stack at the upper right, apart from the navigation row.
	rowOf := func(label string) int {
		row := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, label) })
		if row < 0 {
			t.Fatalf("missing %q:\n%s", label, text)
		}
		return row
	}
	add, remove, cancel := rowOf("Add…"), rowOf("Remove"), rowOf("Cancel")
	if remove != add+1 {
		t.Fatalf("Remove on row %d, Add on row %d:\n%s", remove, add, text)
	}
	if !(add < rowOf("bifrost2 ·") && cancel > remove) {
		t.Fatalf("Add/Remove are not above the details and the navigation row:\n%s", text)
	}
	wzNotContains(t, lines[cancel], "Add")
	wzAction(t, clickProjectText(t, dashboard, terminal, "Add…"), DashboardActionNone{})
	if step := projectWizard(dashboard).step; step != WizardStepNewBundle {
		t.Fatalf("step %v, want NewBundle", step)
	}
	activateProjectControl(dashboard, WizardControlBack)
	if step := projectWizard(dashboard).step; step != WizardStepBundle {
		t.Fatalf("step %v, want Bundle", step)
	}
	drawProjectPicker(dashboard, terminal)
}

func TestRemovingASavedProjectRefusesOneASessionUsesAndKeepsAValidSelection(t *testing.T) {
	state := core.NewStateState()
	session := stoppedSession()
	state.EnvironmentOwners.Insert(session.ID, session.ID)
	state.Sessions.Insert(session.ID, session)
	dashboard := dashboardAtSavedProjects(t, state)

	// A suspended session still resumes from `hel`, so Delete explains why
	// it stays instead of removing it.
	selectSavedProject(dashboard, "hel")
	wzAction(t, readyKey(dashboard, key(tea.KeyDelete)), DashboardActionNone{})
	notice, ok := dashboard.Notice()
	if !ok {
		t.Fatal("Delete explains the refusal")
	}
	wzContains(t, notice, "\"hel\" is used by a session")
	wzContains(t, notice, "ACP pretty name")
	if projectWizard(dashboard).bundleRemovalInFlight {
		t.Fatal("a refused removal is not in flight")
	}

	selectSavedProject(dashboard, "bifrost2")
	wzAction(t, activateProjectControl(dashboard, WizardControlRemoveBundle), DashboardActionRemoveBundle{BundleID: "bifrost2"})
	if !projectWizard(dashboard).bundleRemovalInFlight {
		t.Fatal("the removal is in flight")
	}
	// The list is frozen until the config write is confirmed.
	for _, code := range []rune{tea.KeyDelete, tea.KeyEnter, tea.KeyUp} {
		wzAction(t, readyKey(dashboard, key(code)), DashboardActionNone{})
	}

	dashboard.ApplyRemovedBundle(wzConfigWithoutBundle(dashboard.config.Get(), "bifrost2"), "bifrost2")
	wizard := projectWizard(dashboard)
	if wizard.bundleRemovalInFlight {
		t.Fatal("the confirmed removal is no longer in flight")
	}
	if wizard.step != WizardStepBundle {
		t.Fatalf("step %v, want Bundle", wizard.step)
	}
	if selected := nthBundleKey(dashboard.config.Get(), dashboard.launchRecency, wizard.bundle); selected != "hel" {
		t.Fatalf("selection %q, want hel", selected)
	}
	if notice, ok := dashboard.Notice(); !ok || notice != "Removed project bifrost2." {
		t.Fatalf("notice %q (%t)", notice, ok)
	}
}

func projectPickerURLEnterCreatesOneRepositoryAndAdvancesToReview(t *testing.T) {
	dashboard := dashboardAtProjectPicker(t)
	activateProjectControl(dashboard, WizardControlProjectUrl)
	typeSource(dashboard, "owner/only")
	wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionCreateBundle{Sources: []string{"owner/only"}})
	if !projectWizard(dashboard).bundleCreationInFlight {
		t.Fatal("the creation is in flight")
	}
	for _, code := range []rune{tea.KeyEnter, tea.KeyEscape, tea.KeyBackspace} {
		wzAction(t, readyKey(dashboard, key(code)), DashboardActionNone{})
	}
	if source := projectWizard(dashboard).newBundleSource.String(); source != "owner/only" {
		t.Fatalf("source %q, want owner/only", source)
	}
	created := config()
	id := slices.Sorted(maps.Keys(created.Bundles))[0]
	dashboard.ApplyCreatedBundle(created, id)
	capturedDrawn(dashboard, 100, 30)
	wizard := projectWizard(dashboard)
	if wizard.step != WizardStepReview {
		t.Fatalf("step %v, want Review", wizard.step)
	}
	if wizard.bundleCreationInFlight {
		t.Fatal("the creation finished")
	}
	if selected := nthBundleKey(dashboard.config.Get(), dashboard.launchRecency, wizard.bundle); selected != id {
		t.Fatalf("selection %q, want %q", selected, id)
	}
}

func TestProjectPickerCreationFailurePreservesSourceForRetry(t *testing.T) {
	dashboard := dashboardAtProjectPicker(t)
	activateProjectControl(dashboard, WizardControlProjectUrl)
	typeSource(dashboard, "owner/only")
	expected := DashboardActionCreateBundle{Sources: []string{"owner/only"}}
	wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), expected)
	dashboard.FailBundleCreation("repository not found")
	wizard := projectWizard(dashboard)
	if wizard.step != WizardStepNewBundle {
		t.Fatalf("step %v, want NewBundle", wizard.step)
	}
	if wizard.projectPicker.tab != ProjectTabUrl {
		t.Fatalf("tab %v, want Url", wizard.projectPicker.tab)
	}
	if wizard.bundleCreationInFlight {
		t.Fatal("the failed creation is not in flight")
	}
	if source := wizard.newBundleSource.String(); source != "owner/only" {
		t.Fatalf("source %q, want owner/only", source)
	}
	notice, ok := dashboard.Notice()
	if !ok {
		t.Fatal("the failure is noticed")
	}
	wzContains(t, notice, "repository not found")
	wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), expected)
}

func TestProjectPickerCreationRetryResubmitsSelectedRepositoryWithoutRediscovery(t *testing.T) {
	dashboard := dashboardAtProjectPicker(t)
	activateProjectControl(dashboard, WizardControlProjectGithub)
	context, _ := takeProjectRequest(t, dashboard)
	dashboard.ApplyProjectDiscovery(context, discoveredProjects([]core.ProjectEntry{discoveredRepository("owner/app")}), nil)
	expected := DashboardActionCreateBundle{Sources: []string{"owner/app"}}
	wzAction(t, activateProjectControl(dashboard, WizardControlProjectResults), expected)
	dashboard.FailBundleCreation("Could not save the selected project")
	wizard := projectWizard(dashboard)
	if wizard.projectPicker.error != nil {
		t.Fatalf("picker error %q", *wizard.projectPicker.error)
	}
	if wizard.bundleCreationInFlight {
		t.Fatal("the failed creation is not in flight")
	}
	terminal := newWizardsTerminal(80, 24)
	text := strings.Join(drawProjectPicker(dashboard, terminal), "\n")
	wzContains(t, text, "Could not save the selected project")
	wzAction(t, clickProjectText(t, dashboard, terminal, "Retry"), expected)
	if !projectWizard(dashboard).bundleCreationInFlight {
		t.Fatal("Retry submits the creation again")
	}
	if next := dashboard.TakeProjectDiscovery(); next != nil {
		t.Fatalf("Retry must not rediscover: %s", wzDebug(next))
	}
}

func TestProjectPickerTurningMultipleOffKeepsChosenMemberAndRejectsPendingResults(t *testing.T) {
	dashboard := dashboardAtProjectPicker(t)
	activateProjectControl(dashboard, WizardControlProjectUrl)
	focusProjectControl(dashboard, WizardControlProjectMultiple)
	readyKey(dashboard, key(' '))
	focusProjectControl(dashboard, WizardControlNewBundleSource)
	for _, source := range []string{"owner/keep", "owner/other"} {
		typeSource(dashboard, source)
		readyKey(dashboard, key(tea.KeyEnter))
	}
	focusProjectControl(dashboard, WizardControlNewBundleRepositories)
	readyKey(dashboard, key(tea.KeyUp))
	activateProjectControl(dashboard, WizardControlProjectGithub)
	context, _ := takeProjectRequest(t, dashboard)
	focusProjectControl(dashboard, WizardControlProjectMultiple)
	readyKey(dashboard, key(' '))
	wizard := projectWizard(dashboard)
	if wizard.projectPicker.multiple {
		t.Fatal("multiple is off")
	}
	if wizard.projectPicker.tab != ProjectTabUrl {
		t.Fatalf("tab %v, want Url", wizard.projectPicker.tab)
	}
	if source := wizard.newBundleSource.String(); source != "owner/keep" {
		t.Fatalf("source %q, want owner/keep", source)
	}
	if len(wizard.newBundleRepositories) != 0 {
		t.Fatalf("repositories %v, want none", wizard.newBundleRepositories)
	}
	dashboard.ApplyProjectDiscovery(context, core.ProjectDiscovery{}, errors.New("obsolete GitHub failure"))
	if picker := projectWizard(dashboard).projectPicker; picker.error != nil {
		t.Fatalf("obsolete failure recorded: %q", *picker.error)
	}
	wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionCreateBundle{Sources: []string{"owner/keep"}})
}

func projectPickerMultipleSourcesWaitForSubmitAndRemoveSelected(t *testing.T) {
	dashboard := dashboardAtProjectPicker(t)
	activateProjectControl(dashboard, WizardControlProjectUrl)
	focusProjectControl(dashboard, WizardControlProjectMultiple)
	readyKey(dashboard, key(' '))
	if !projectWizard(dashboard).projectPicker.multiple {
		t.Fatal("multiple is on")
	}
	focusProjectControl(dashboard, WizardControlNewBundleSource)
	for _, source := range []string{"owner/main", "owner/remove", "owner/shared"} {
		typeSource(dashboard, source)
		wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionNone{})
	}
	wzRepositories(t, dashboard, "owner/main", "owner/remove", "owner/shared")
	capturedDrawn(dashboard, 100, 30)
	if !projectWizard(dashboard).newBundleSource.IsEmpty() {
		t.Fatal("adding a source clears the field")
	}
	focusProjectControl(dashboard, WizardControlNewBundleRepositories)
	readyKey(dashboard, key(tea.KeyUp))
	readyKey(dashboard, key(tea.KeyDelete))
	wzRepositories(t, dashboard, "owner/main", "owner/shared")
	capturedDrawn(dashboard, 100, 30)
	wzAction(t, activateProjectControl(dashboard, WizardControlNext), DashboardActionCreateBundle{Sources: []string{"owner/main", "owner/shared"}})
	wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionNone{})
	captureDetail("selection", "keyboard-selected owner/app is submitted")
}

// wzRepositories fails t unless the new project holds exactly sources.
func wzRepositories(t testing.TB, dashboard *DashboardState, sources ...string) {
	t.Helper()
	if repositories := projectWizard(dashboard).newBundleRepositories; !slices.Equal(repositories, sources) {
		t.Fatalf("repositories %q, want %q", repositories, sources)
	}
}

func TestProjectPickerRecentDeduplicatesLaunchDirectoryAndUsesOnlyLocalHistory(t *testing.T) {
	configuration := config()
	clear(configuration.Bundles)
	state := core.NewStateState()
	state.RememberProjectDirectory("local", "/work/older")
	state.RememberProjectDirectory("local", "/work/current")
	state.RememberProjectDirectory("builder", "/remote/other")
	dashboard := wzNewDashboard(configuration, state)
	dashboard.SetLaunchProjectDirectory(wzPtr("/work/current"))
	readyOpenNewWizard(dashboard)
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	wizard := projectWizard(dashboard)
	var sources []string
	for _, entry := range wizard.projectPicker.entries {
		sources = append(sources, entry.Source)
	}
	if want := []string{"/work/current", "/work/older"}; !slices.Equal(sources, want) {
		t.Fatalf("recent %q, want %q", sources, want)
	}
	wzFocusIs(t, &wizard.form, WizardControlProjectResults)
	terminal := newWizardsTerminal(80, 24)
	text := strings.Join(drawProjectPicker(dashboard, terminal), "\n")
	wzContains(t, text, "Current project")
	wzContains(t, text, "/work/older")
	wzNotContains(t, text, "/remote/other")
	readyKey(dashboard, key(tea.KeyDown))
	wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionCreateBundle{Sources: []string{"/work/older"}})
}

func projectPickerGithubSearchChoosesTheKeyboardSelectedResult(t *testing.T) {
	dashboard := dashboardAtProjectPicker(t)
	activateProjectControl(dashboard, WizardControlProjectGithub)
	initial, request := takeProjectRequest(t, dashboard)
	wzGithubRequest(t, request, "")
	typeSource(dashboard, "owner/app")
	readyKey(dashboard, key(tea.KeyEnter))
	search, request := takeProjectRequest(t, dashboard)
	wzGithubRequest(t, request, "owner/app")
	dashboard.ApplyProjectDiscovery(initial, discoveredProjects([]core.ProjectEntry{discoveredRepository("owner/stale")}), nil)
	if picker := projectWizard(dashboard).projectPicker; len(picker.entries) != 0 || !picker.loading {
		t.Fatalf("a stale answer must not replace the search: %+v", picker.entries)
	}
	dashboard.ApplyProjectDiscovery(search, discoveredProjects([]core.ProjectEntry{
		discoveredRepository("owner/apple"),
		discoveredRepository("owner/app"),
	}), nil)
	terminal := newWizardsTerminal(80, 24)
	text := strings.Join(drawProjectPicker(dashboard, terminal), "\n")
	wzContains(t, text, "owner/apple")
	wzContains(t, text, "owner/app")
	wzNotContains(t, text, "owner/stale")
	focusProjectControl(dashboard, WizardControlProjectResults)
	readyKey(dashboard, key(tea.KeyDown))
	wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionCreateBundle{Sources: []string{"owner/app"}})
}

// wzGithubRequest fails t unless request is a GitHub search for query.
func wzGithubRequest(t testing.TB, request core.ProjectDiscoveryRequest, query string) {
	t.Helper()
	if !wzEqual(request, core.ProjectDiscoveryRequest(core.ProjectDiscoveryRequestGithub{Query: query})) {
		t.Fatalf("request %s, want a GitHub search for %q", wzDebugRequest(request), query)
	}
}

// Hard-won: 42149aaf0: Next could submit stale GitHub search results.
func TestProjectPickerGithubNextSearchesChangedQueryBeforeSelection(t *testing.T) {
	dashboard := dashboardAtProjectPicker(t)
	activateProjectControl(dashboard, WizardControlProjectGithub)
	initial, _ := takeProjectRequest(t, dashboard)
	dashboard.ApplyProjectDiscovery(initial, discoveredProjects([]core.ProjectEntry{discoveredRepository("www-site")}), nil)

	typeSource(dashboard, "BrokkAi/attest")
	if entries := projectWizard(dashboard).projectPicker.entries; len(entries) != 0 {
		t.Fatalf("editing the query clears the results: %+v", entries)
	}
	dashboard.ApplyProjectDiscovery(initial, discoveredProjects([]core.ProjectEntry{discoveredRepository("www-site")}), nil)
	if entries := projectWizard(dashboard).projectPicker.entries; len(entries) != 0 {
		t.Fatalf("a stale answer must not return: %+v", entries)
	}
	terminal := newWizardsTerminal(100, 30)
	wzAction(t, clickProjectText(t, dashboard, terminal, "Use project"), DashboardActionNone{})
	fresh, request := takeProjectRequest(t, dashboard)
	wzGithubRequest(t, request, "BrokkAi/attest")
	dashboard.ApplyProjectDiscovery(fresh, discoveredProjects([]core.ProjectEntry{discoveredRepository("BrokkAi/attest")}), nil)
	if projectWizard(dashboard).bundleCreationInFlight {
		t.Fatal("results alone must not create the project")
	}
	capturedDrawn(dashboard, 100, 30)
	wzAction(t, activateProjectControl(dashboard, WizardControlNext), DashboardActionCreateBundle{Sources: []string{"BrokkAi/attest"}})
}

func TestProjectPickerGithubResultDoesNotOverwriteQueryOrStealEditFocus(t *testing.T) {
	dashboard := dashboardAtProjectPicker(t)
	activateProjectControl(dashboard, WizardControlProjectGithub)
	context, _ := takeProjectRequest(t, dashboard)
	typeSource(dashboard, "still typing")
	dashboard.ApplyProjectDiscovery(context, discoveredProjects([]core.ProjectEntry{discoveredRepository("owner/app")}), nil)
	wizard := projectWizard(dashboard)
	if query := wizard.projectPicker.query.String(); query != "still typing" {
		t.Fatalf("query %q, want %q", query, "still typing")
	}
	wzFocusIs(t, &wizard.form, WizardControlProjectQuery)
	typeSource(dashboard, " now")
	if query := projectWizard(dashboard).projectPicker.query.String(); query != "still typing now" {
		t.Fatalf("query %q, want %q", query, "still typing now")
	}
}

func projectPickerMultipleDiscoverySelectionTogglesMembersAndCanChangePrimary(t *testing.T) {
	dashboard := dashboardAtProjectPicker(t)
	focusProjectControl(dashboard, WizardControlProjectMultiple)
	readyKey(dashboard, key(' '))
	activateProjectControl(dashboard, WizardControlProjectGithub)
	context, _ := takeProjectRequest(t, dashboard)
	dashboard.ApplyProjectDiscovery(context, discoveredProjects([]core.ProjectEntry{
		discoveredRepository("owner/app"),
		discoveredRepository("owner/shared"),
	}), nil)
	wzAction(t, activateProjectControl(dashboard, WizardControlProjectResults), DashboardActionNone{})
	readyKey(dashboard, key(tea.KeyDown))
	wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionNone{})
	wzRepositories(t, dashboard, "owner/app", "owner/shared")
	// Choosing an already-selected result removes it, without launching.
	wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionNone{})
	wzRepositories(t, dashboard, "owner/app")
	capturedDrawn(dashboard, 100, 30)
	readyKey(dashboard, key(tea.KeyEnter))
	activateProjectControl(dashboard, WizardControlProjectMakePrimary)
	capturedDrawn(dashboard, 100, 30)
	wzAction(t, activateProjectControl(dashboard, WizardControlNext), DashboardActionCreateBundle{Sources: []string{"owner/shared", "owner/app"}})
}

func TestProjectPickerRejectsLateSuccessAndFailureAfterSwitchingTabs(t *testing.T) {
	dashboard := dashboardAtProjectPicker(t)
	activateProjectControl(dashboard, WizardControlProjectGithub)
	github, _ := takeProjectRequest(t, dashboard)
	activateProjectControl(dashboard, WizardControlProjectGithub)
	folders, _ := takeProjectRequest(t, dashboard)
	dashboard.ApplyProjectDiscovery(github, discoveredProjects([]core.ProjectEntry{discoveredRepository("owner/stale")}), nil)
	dashboard.ApplyProjectDiscovery(github, core.ProjectDiscovery{}, errors.New("old GitHub error"))
	picker := projectWizard(dashboard).projectPicker
	if picker.tab != ProjectTabGithub {
		t.Fatalf("tab %v, want Github", picker.tab)
	}
	if !picker.loading || len(picker.entries) != 0 || picker.error != nil {
		t.Fatalf("late answers must be rejected: loading %t, entries %+v, error %v", picker.loading, picker.entries, picker.error)
	}
	activateProjectControl(dashboard, WizardControlProjectUrl)
	typeSource(dashboard, "owner/draft")
	dashboard.ApplyProjectDiscovery(folders, discoveredProjects([]core.ProjectEntry{discoveredRepository("/stale/repo")}), nil)
	wizard := projectWizard(dashboard)
	if wizard.projectPicker.tab != ProjectTabUrl {
		t.Fatalf("tab %v, want Url", wizard.projectPicker.tab)
	}
	if len(wizard.projectPicker.entries) != 0 {
		t.Fatalf("entries %+v, want none", wizard.projectPicker.entries)
	}
	if source := wizard.newBundleSource.String(); source != "owner/draft" {
		t.Fatalf("source %q, want owner/draft", source)
	}
	wzFocusIs(t, &wizard.form, WizardControlNewBundleSource)
}

func TestProjectPickerReopenedWizardRejectsPreviousDiscoveryContext(t *testing.T) {
	dashboard := dashboardAtProjectPicker(t)
	activateProjectControl(dashboard, WizardControlProjectGithub)
	oldContext, _ := takeProjectRequest(t, dashboard)
	activateProjectControl(dashboard, WizardControlCancel)
	if _, ok := dashboard.mode.(ModeNew); ok {
		t.Fatal("Cancel closes the wizard")
	}
	readyOpenNewWizard(dashboard)
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	activateProjectControl(dashboard, WizardControlProjectGithub)
	context, _ := takeProjectRequest(t, dashboard)
	if context == oldContext {
		t.Fatalf("the reopened wizard reused context %q", context)
	}
	dashboard.ApplyProjectDiscovery(oldContext, discoveredProjects([]core.ProjectEntry{discoveredRepository("owner/stale")}), nil)
	dashboard.ApplyProjectDiscovery(oldContext, core.ProjectDiscovery{}, errors.New("stale failure"))
	picker := projectWizard(dashboard).projectPicker
	if !picker.loading || len(picker.entries) != 0 || picker.error != nil {
		t.Fatalf("stale answers must be rejected: loading %t, entries %+v, error %v", picker.loading, picker.entries, picker.error)
	}
	dashboard.ApplyProjectDiscovery(context, discoveredProjects([]core.ProjectEntry{discoveredRepository("owner/current")}), nil)
	if source := projectWizard(dashboard).projectPicker.entries[0].Source; source != "owner/current" {
		t.Fatalf("first entry %q, want owner/current", source)
	}
}

func TestProjectPickerRetryPreservesQueryAndMultipleRepositorySelection(t *testing.T) {
	dashboard := dashboardAtProjectPicker(t)
	activateProjectControl(dashboard, WizardControlProjectUrl)
	focusProjectControl(dashboard, WizardControlProjectMultiple)
	readyKey(dashboard, key(' '))
	focusProjectControl(dashboard, WizardControlNewBundleSource)
	typeSource(dashboard, "owner/main")
	readyKey(dashboard, key(tea.KeyEnter))
	activateProjectControl(dashboard, WizardControlProjectGithub)
	takeProjectRequest(t, dashboard)
	typeSource(dashboard, "owner/shared")
	readyKey(dashboard, key(tea.KeyEnter))
	failed, request := takeProjectRequest(t, dashboard)
	dashboard.ApplyProjectDiscovery(failed, core.ProjectDiscovery{}, errors.New("GitHub unavailable."))
	terminal := newWizardsTerminal(80, 24)
	text := strings.Join(drawProjectPicker(dashboard, terminal), "\n")
	wzContains(t, text, "GitHub unavailable")
	wzContains(t, text, "owner/main")
	wzAction(t, clickProjectText(t, dashboard, terminal, "Retry"), DashboardActionNone{})
	retry, retriedRequest := takeProjectRequest(t, dashboard)
	if !wzEqual(retriedRequest, request) {
		t.Fatalf("retried %s, want %s", wzDebugRequest(retriedRequest), wzDebugRequest(request))
	}
	if retry == failed {
		t.Fatalf("Retry reused context %q", retry)
	}
	if query := projectWizard(dashboard).projectPicker.query.String(); query != "owner/shared" {
		t.Fatalf("query %q, want owner/shared", query)
	}
	wzRepositories(t, dashboard, "owner/main")
	dashboard.ApplyProjectDiscovery(retry, discoveredProjects([]core.ProjectEntry{discoveredRepository("owner/shared")}), nil)
	wzAction(t, activateProjectControl(dashboard, WizardControlProjectResults), DashboardActionNone{})
	wzRepositories(t, dashboard, "owner/main", "owner/shared")
	wzAction(t, activateProjectControl(dashboard, WizardControlNext), DashboardActionCreateBundle{Sources: []string{"owner/main", "owner/shared"}})
}

func projectPickerMouseTabsAndSingleRepositoryChoiceWorkOnSmallTerminals(t *testing.T) {
	for _, size := range [][2]int{{80, 24}, {60, 20}} {
		width, height := size[0], size[1]
		dashboard := dashboardAtProjectPicker(t)
		terminal := newWizardsTerminal(width, height)
		wzAction(t, clickProjectText(t, dashboard, terminal, "GitHub"), DashboardActionNone{})
		context, _ := takeProjectRequest(t, dashboard)
		dashboard.ApplyProjectDiscovery(context, discoveredProjects([]core.ProjectEntry{discoveredRepository("owner/visible")}), nil)
		lines := drawProjectPicker(dashboard, terminal)
		text := strings.Join(lines, "\n")
		for _, label := range []string{"Choose a project", "URL or Path", "Search", "owner/visible", "Use project"} {
			if !strings.Contains(text, label) {
				t.Fatalf("missing %q at %dx%d:\n%s", label, width, height, text)
			}
		}
		wzNotContains(t, text, "Add repository")
		wzAction(t, clickProjectText(t, dashboard, terminal, "owner/visible"), DashboardActionNone{})
		wzFocusIs(t, &projectWizard(dashboard).form, WizardControlProjectResults)
		wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionCreateBundle{Sources: []string{"owner/visible"}})
	}
}

func projectPickerURLOrPathSubmitsALocalRepositoryPath(t *testing.T) {
	dashboard := dashboardAtProjectPicker(t)
	activateProjectControl(dashboard, WizardControlProjectUrl)
	typeSource(dashboard, "/work/visible")
	wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionCreateBundle{Sources: []string{"/work/visible"}})
}

func projectPickerResultsRemainVisibleWithMultipleSelectedRepositories(t *testing.T) {
	for _, size := range [][2]int{{80, 24}, {60, 20}} {
		width, height := size[0], size[1]
		dashboard := dashboardAtProjectPicker(t)
		focusProjectControl(dashboard, WizardControlProjectMultiple)
		readyKey(dashboard, key(' '))
		activateProjectControl(dashboard, WizardControlProjectGithub)
		context, _ := takeProjectRequest(t, dashboard)
		dashboard.ApplyProjectDiscovery(context, discoveredProjects([]core.ProjectEntry{
			discoveredRepository("owner/app"),
			discoveredRepository("owner/shared"),
			discoveredRepository("owner/docs"),
			discoveredRepository("owner/next"),
		}), nil)
		focusProjectControl(dashboard, WizardControlProjectResults)
		for range 3 {
			readyKey(dashboard, key(tea.KeyEnter))
			readyKey(dashboard, key(tea.KeyDown))
		}
		terminal := newWizardsTerminal(width, height)
		text := strings.Join(drawProjectPicker(dashboard, terminal), "\n")
		if !strings.Contains(text, "owner/next") {
			t.Fatalf("selected search result disappears at %dx%d:\n%s", width, height, text)
		}
		wzContains(t, text, "Use 3 repositories")
		wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionNone{})
		wzAction(t, activateProjectControl(dashboard, WizardControlNext), DashboardActionCreateBundle{
			Sources: []string{"owner/app", "owner/shared", "owner/docs", "owner/next"},
		})
	}
}

func projectPickerMouseMultipleToggleAndURLAddKeepInputAboveActions(t *testing.T) {
	for _, size := range [][2]int{{80, 24}, {60, 20}} {
		width, height := size[0], size[1]
		dashboard := dashboardAtProjectPicker(t)
		terminal := newWizardsTerminal(width, height)
		clickProjectText(t, dashboard, terminal, "URL or Path")
		clickProjectText(t, dashboard, terminal, "Use several repositories together")
		if !projectWizard(dashboard).projectPicker.multiple {
			t.Fatal("the checkbox turns multiple on")
		}
		focusProjectControl(dashboard, WizardControlNewBundleSource)
		typeSource(dashboard, "owner/visible")
		lines := drawProjectPicker(dashboard, terminal)
		text := strings.Join(lines, "\n")
		sourceRow := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, "owner/visible") })
		if sourceRow < 0 {
			t.Fatalf("source missing at %dx%d:\n%s\n%q", width, height, text, projectWizard(dashboard).newBundleSource.String())
		}
		actionRow := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, "Add repository") })
		if actionRow < 0 {
			t.Fatalf("Add repository missing:\n%s", text)
		}
		if sourceRow >= actionRow {
			t.Fatalf("%dx%d:\n%s", width, height, text)
		}
		wzAction(t, clickProjectText(t, dashboard, terminal, "Add repository"), DashboardActionNone{})
		wzRepositories(t, dashboard, "owner/visible")
		if !projectWizard(dashboard).newBundleSource.IsEmpty() {
			t.Fatal("adding a source clears the field")
		}
		// The spaced form can exceed a short terminal. Tab must reveal the
		// repository actions rather than leave their controls off screen.
		for range 20 {
			if focused, ok := projectWizard(dashboard).form.Focused(); ok && focused == WizardControlNewBundleRemove {
				break
			}
			readyKey(dashboard, key(tea.KeyTab))
			drawProjectPicker(dashboard, terminal)
		}
		wzFocusIs(t, &projectWizard(dashboard).form, WizardControlNewBundleRemove)
		text = strings.Join(drawProjectPicker(dashboard, terminal), "\n")
		wzContains(t, text, "Main  owner/visible")
		wzContains(t, text, "Remove")
		assertDialogSpacing(t, terminal.lines(), "Choose a project", "Use 1 repository")
	}
}

// readyOpenNewWizard opens the full new-session wizard through its live
// binding, for wizard tests concerned with other behavior. No availability
// answer is needed: until one arrives, everything configured is offered.
func readyOpenNewWizard(dashboard *DashboardState) DashboardAction {
	return openNewSessionWizard(dashboard)
}

func readyKey(dashboard *DashboardState, event tea.KeyPressMsg) DashboardAction {
	action := dashboard.HandleKey(event)
	captureAction(action)
	return action
}

// wzBufferSymbols is every cell's symbol in row order with nothing between
// the rows, as Rust's `buffer.content().iter().map(Cell::symbol)` collects
// them.
func wzBufferSymbols(buffer *uv.Buffer) string {
	var text strings.Builder
	for y := range buffer.Height() {
		for x := range buffer.Width() {
			if cell := buffer.CellAt(x, y); cell != nil {
				text.WriteString(cell.Content)
			}
		}
	}
	return text.String()
}

func TestBareSshNewSessionSelectsTargetThenRawProjectWithoutAttachments(t *testing.T) {
	configuration := config()
	configuration.Targets = map[string]core.TargetTemplate{"machine": bareSshTarget()}
	state := core.NewStateState()
	state.RememberProjectDirectory("builder.example.com", "/srv/recent")
	state.RememberProjectDirectory("builder.example.com", "/srv/older")
	dashboard := wzNewDashboard(configuration, state)

	readyOpenNewWizard(dashboard)
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	if directory := newWizard(dashboard).projectDirectory.String(); directory != "/srv/older" {
		t.Fatalf("directory %q, want /srv/older", directory)
	}
	readyKey(dashboard, key(tea.KeyDown))
	if directory := newWizard(dashboard).projectDirectory.String(); directory != "/srv/recent" {
		t.Fatalf("directory %q, want /srv/recent", directory)
	}
	for {
		mode, ok := dashboard.mode.(ModeNew)
		if !ok || mode.projectDirectory.IsEmpty() {
			break
		}
		readyKey(dashboard, key(tea.KeyBackspace))
	}
	typeSource(dashboard, "/srv/project")
	wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionValidateProjectDirectory{
		TargetTemplateID: "machine",
		Directory:        "/srv/project",
	})
	dashboard.ApplyProjectDirectoryValidation("/srv/project",
		errors.New("remote project directory /srv/project does not exist or is not a directory"))

	terminal := newWizardsTerminal(100, 28)
	terminal.draw(dashboard)
	wzContains(t, wzBufferSymbols(terminal.buffer), "Error: remote project directory /srv/project does not exist")

	readyKey(dashboard, tea.KeyPressMsg{Code: 'u', Mod: tea.ModCtrl})
	dashboard.HandlePaste("/srv/repaired")
	mode, ok := dashboard.mode.(ModeNew)
	if !ok {
		t.Fatal("invalid project validation should keep the wizard open")
	}
	if directory := mode.projectDirectory.String(); directory != "/srv/repaired" {
		t.Fatalf("directory %q, want /srv/repaired", directory)
	}
	if mode.projectDirectoryError != nil {
		t.Fatalf("error %q survived the edit", *mode.projectDirectoryError)
	}
	wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionValidateProjectDirectory{
		TargetTemplateID: "machine",
		Directory:        "/srv/repaired",
	})

	dashboard.ApplyResolvedProjectDirectory(dashboard.PathInputContext(), "/srv/repaired", "/srv/repaired",
		core.ManagedWorktreeOptions{Available: true, DefaultCreate: true}, false, nil)

	terminal.draw(dashboard)
	rendered := wzBufferSymbols(terminal.buffer)
	wzContains(t, rendered, "Project directory: /srv/repaired")
	wzNotContains(t, rendered, "Attached directories")

	wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionCreateSession{
		RememberContainerSize: false,
		CreateManagedWorktree: wzPtr(true),
		WorkspaceID:           core.DefaultWorkspaceID,
		ProfileID:             "claude-1",
		BundleID:              core.RawProjectContextID("/srv/repaired"),
		ProjectDirectory:      wzPtr("/srv/repaired"),
		TargetTemplateID:      "machine",
		AdditionalMounts:      []core.AdditionalMount{},
	})
}

// wzKimiProfile is a Kimi Code profile homed at /profiles/kimi.
func wzKimiProfile() core.HarnessProfile {
	return core.NewHarnessProfile(core.HarnessKindKimi, "/profiles/kimi")
}

// aMissingProfileIsLeftOutOfTheProfileStepAndNamed: the profile step reads
// as a table: the profile, harness, and quota columns start at the same cell
// on the heading and on every row. A profile whose installation is gone is
// not offered: the profile step lists only what is there and names the
// missing one under the table, with why and what to do.
func aMissingProfileIsLeftOutOfTheProfileStepAndNamed(t *testing.T) {
	configuration := config()
	configuration.Profiles["kimi-1"] = wzKimiProfile()
	dashboard := wzNewDashboard(configuration, core.NewStateState())
	readyOpenNewWizard(dashboard)
	answerAvailability(dashboard, func(dashboard *DashboardState) client.RuntimeAvailability {
		answer := availabilityAnswer(dashboard, nil, nil)
		if _, ok := answer.Profiles["kimi-1"]; !ok {
			panic("the answer covers kimi-1")
		}
		answer.Profiles["kimi-1"] = client.AvailabilityVerdict{
			Kind: client.AvailabilityVerdictMissing,
			Reason: "Kimi Code is not installed here: /profiles/kimi does not exist and `kimi` is not " +
				"on PATH. Install Kimi Code, or turn the profile off in Settings › Profiles.",
		}
		return answer
	})
	text := strings.Join(capturedDrawn(dashboard, 120, 30), "\n")
	wzContains(t, text, "claude-1")
	wzContains(t, text, "kimi-1: Kimi Code is not installed here")
	for id := range dashboard.offeredProfiles() {
		if id == "kimi-1" {
			t.Fatal("a missing profile is not offered")
		}
	}
}

func newSessionProfileStepAlignsItsColumns(t *testing.T) {
	configuration := config()
	configuration.Profiles["kimi-1"] = wzKimiProfile()
	dashboard := wzNewDashboard(configuration, core.NewStateState())
	dashboard.quotaProviders.settle()
	readyOpenNewWizard(dashboard)
	// ANTHROPIC starts expanded for the initially selected claude-1; expand
	// MOONSHOT too so both providers' rows are visible for column alignment.
	readyKey(dashboard, key(tea.KeyDown))
	readyKey(dashboard, key(tea.KeyDown))
	readyKey(dashboard, key(tea.KeyRight))
	terminal := newWizardsTerminal(120, 30)
	terminal.draw(dashboard)
	lines := terminal.lines()
	captureRender(120, 30, lines)
	row := func(matches func(string) bool, what string) string {
		index := slices.IndexFunc(lines, matches)
		if index < 0 {
			t.Fatalf("missing row %s in %q", what, lines)
		}
		return lines[index]
	}
	containing := func(needle string) string {
		return row(func(line string) bool { return strings.Contains(line, needle) }, strconv.Quote(needle))
	}

	headings := containing("PROVIDER")
	anthropic := containing("▾ ANTHROPIC")
	moonshot := containing("▾ MOONSHOT")
	claude := row(func(line string) bool {
		return strings.Contains(line, "claude-1") && !strings.Contains(line, "ANTHROPIC")
	}, "expanded claude-1")
	kimi := row(func(line string) bool {
		return strings.Contains(line, "kimi-1") && !strings.Contains(line, "MOONSHOT")
	}, "expanded kimi-1")
	wzNotContains(t, headings, "HARNESS")
	providerColumn := cellColumn(headings, "PROVIDER")
	profileColumn := cellColumn(headings, "PROFILE")
	wzSameColumn(t, cellColumn(anthropic, "▾ ANTHROPIC"), providerColumn, anthropic)
	wzSameColumn(t, cellColumn(moonshot, "▾ MOONSHOT"), providerColumn, moonshot)
	wzSameColumn(t, cellColumn(anthropic, "claude-1"), profileColumn, anthropic)
	wzSameColumn(t, cellColumn(moonshot, "kimi-1"), profileColumn, moonshot)
	for _, pair := range [][2]string{{claude, "claude-1"}, {kimi, "kimi-1"}} {
		line, profile := pair[0], pair[1]
		wzSameColumn(t, cellColumn(line, profile), profileColumn, line)
		wzSameColumn(t, cellColumn(line, "refreshing"), cellColumn(headings, "WEEKLY"), line)
	}
}

// wzSameColumn fails t unless got is the want column.
func wzSameColumn(t testing.TB, got, want int, line string) {
	t.Helper()
	if got != want {
		t.Fatalf("column %d, want %d: %q", got, want, line)
	}
}

func TestNewSessionProfileStepGroupsByProviderWithPredictedProfiles(t *testing.T) {
	configuration := config()
	configuration.Subagents.Primary = append(configuration.Subagents.Primary, core.SubagentChoice{Model: "claude-sonnet"})
	configuration.Profiles["kimi-1"] = wzKimiProfile()
	claudeKey := configuration.Profiles["claude-1"].CapabilitiesKey("claude-1")
	dashboard := wzNewDashboard(configuration, core.NewStateState())
	dashboard.SetProfileCapabilities(core.ProfileCapabilitiesSnapshot{
		Profiles: map[string]core.ProfileCapabilities{
			claudeKey: {
				Choices: core.CapabilityState[core.ProfileConfig]{
					Kind: core.CapabilityStateReady,
					Value: core.ProfileConfig{
						Model: wzPtr("claude-sonnet"),
						Models: []core.SessionConfigChoice{{
							Value: "claude-sonnet",
							Name:  "Claude Sonnet",
						}},
						Efforts:    []core.SessionConfigChoice{},
						ObservedAt: 0,
					},
				},
				Efforts: map[string]core.CapabilityState[[]core.SessionConfigChoice]{},
			},
		},
	})
	dashboard.quotaProviders.settle()
	readyOpenNewWizard(dashboard)

	terminal := newWizardsTerminal(120, 30)
	drawLines := func() []string {
		terminal.draw(dashboard)
		return terminal.lines()
	}

	// Auto sits directly in the list without blank bracketing, showing the
	// predicted profile in the 2nd column; providers start collapsed with
	// their predicted profiles in the 2nd column in Profiles-pane order.
	lines := drawLines()
	headingIndex := slices.IndexFunc(lines, func(line string) bool {
		return strings.Contains(line, "PROVIDER") && strings.Contains(line, "PROFILE")
	})
	if headingIndex < 0 {
		t.Fatalf("profile heading row missing:\n%s", strings.Join(lines, "\n"))
	}
	wzNotContains(t, lines[headingIndex], "HARNESS")
	for offset, needles := range [][]string{
		1: {"Auto", "claude-1"},
		2: {"▸ ANTHROPIC", "claude-1"},
		3: {"▸ OPENAI", "codex-1"},
		4: {"▸ MOONSHOT", "kimi-1"},
	} {
		for _, needle := range needles {
			wzContains(t, lines[headingIndex+offset], needle)
		}
	}
	hasExpandedClaude := func(lines []string) bool {
		return slices.ContainsFunc(lines, func(line string) bool {
			return strings.Contains(line, "claude-1") && !strings.Contains(line, "Auto") && !strings.Contains(line, "ANTHROPIC")
		})
	}
	hasExpandedCodex := func(lines []string) bool {
		return slices.ContainsFunc(lines, func(line string) bool {
			return strings.Contains(line, "codex-1") && !strings.Contains(line, "OPENAI")
		})
	}
	if hasExpandedClaude(lines) || hasExpandedCodex(lines) {
		t.Fatalf("providers start collapsed:\n%s", strings.Join(lines, "\n"))
	}

	// Down moves onto ANTHROPIC: Next is enabled because a provider is
	// selectable to load-balance across its profiles; Space and Right/Left
	// toggle expansion without advancing.
	readyKey(dashboard, key(tea.KeyDown))
	if !newWizard(dashboard).ProfileAdvanceEnabled(dashboard) {
		t.Fatal("a provider row can advance")
	}
	wzProvider(t, dashboard, core.QuotaProviderAnthropic{})
	if label := newWizard(dashboard).selectedProfileLabel(dashboard); label != "Auto (Anthropic)" {
		t.Fatalf("label %q, want %q", label, "Auto (Anthropic)")
	}
	readyKey(dashboard, key(' '))
	if step := newWizard(dashboard).step; step != WizardStepProfile {
		t.Fatalf("step %v, want Profile", step)
	}
	expanded := drawLines()
	if !slices.ContainsFunc(expanded, func(line string) bool { return strings.Contains(line, "▾ ANTHROPIC") }) || !hasExpandedClaude(expanded) {
		t.Fatalf("Space expands ANTHROPIC:\n%s", strings.Join(expanded, "\n"))
	}

	// Space collapses ANTHROPIC; Right expands it and a second Right enters
	// its first profile; Left collapses back to the provider row.
	readyKey(dashboard, key(' '))
	if hasExpandedClaude(drawLines()) {
		t.Fatal("Space collapses ANTHROPIC")
	}
	readyKey(dashboard, key(tea.KeyRight))
	if !hasExpandedClaude(drawLines()) {
		t.Fatal("Right expands ANTHROPIC")
	}
	readyKey(dashboard, key(tea.KeyRight))
	if !newWizard(dashboard).ProfileAdvanceEnabled(dashboard) {
		t.Fatal("a profile row can advance")
	}
	wzProvider(t, dashboard, nil)
	if id, ok := dashboard.newWizardProfileID(newWizard(dashboard).profile); !ok || id != "claude-1" {
		t.Fatalf("profile %q (%t), want claude-1", id, ok)
	}
	readyKey(dashboard, key(tea.KeyLeft))
	wzProvider(t, dashboard, core.QuotaProviderAnthropic{})
	if hasExpandedClaude(drawLines()) {
		t.Fatal("Left collapses ANTHROPIC")
	}

	// Clicking the arrow on a collapsed provider expands it; clicking the
	// provider name selects it without collapsing, and Enter advances with
	// that provider selected for load-balancing.
	clickProjectText(t, dashboard, terminal, "▸ OPENAI")
	if !hasExpandedCodex(drawLines()) {
		t.Fatal("clicking the arrow expands OPENAI")
	}
	clickProjectText(t, dashboard, terminal, "OPENAI")
	if !hasExpandedCodex(drawLines()) {
		t.Fatal("clicking the name keeps OPENAI expanded")
	}
	wzProvider(t, dashboard, core.QuotaProviderOpenAI{})
	readyKey(dashboard, key(tea.KeyEnter))
	if step := newWizard(dashboard).step; step != WizardStepTarget {
		t.Fatalf("step %v, want Target", step)
	}
	wzProvider(t, dashboard, core.QuotaProviderOpenAI{})
	action := dashboard.createSessionActionWithoutClosing(newWizard(dashboard))
	create, ok := action.(DashboardActionCreateSession)
	if !ok || create.ProfileID != "" || create.Provider == nil || !wzEqual(*create.Provider, core.QuotaProvider(core.QuotaProviderOpenAI{})) {
		t.Fatalf("action %s, want a load-balanced OpenAI creation", wzDebug(action))
	}
}

// wzProvider fails t unless the new-session wizard's selected provider is
// want (nil for none).
func wzProvider(t testing.TB, dashboard *DashboardState, want core.QuotaProvider) {
	t.Helper()
	if got := newWizard(dashboard).selectedProvider(dashboard); !wzEqual(got, want) {
		t.Fatalf("provider %v, want %v", got, want)
	}
}

func TestNewSessionProfileStepReconcilesWhenProviderDiscoveryCompletes(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	readyOpenNewWizard(dashboard)
	// Simulate having opened while Codex provider discovery was still
	// resolving: only the temporary "(resolving)" group is in
	// expandedProviders, while discovery has now settled to OpenAI.
	resolving := core.QuotaProviderOther{Value: "Codex (resolving)"}
	if mode, ok := dashboard.mode.(ModeNew); ok {
		mode.expandedProviders = map[core.QuotaProvider]struct{}{resolving: {}}
		mode.focusedProvider = nil
	}
	terminal := newWizardsTerminal(120, 30)
	terminal.draw(dashboard)
	rendered := strings.Join(terminal.lines(), "\n")
	wzContains(t, rendered, "▾ OPENAI")
	wzContains(t, rendered, "codex-1")
	if !newWizard(dashboard).ProfileAdvanceEnabled(dashboard) {
		t.Fatal("the reconciled picker can advance")
	}

	// A stale focused "(resolving)" header clears back to the selected
	// profile instead of leaving Next disabled or swallowing Enter.
	if mode, ok := dashboard.mode.(ModeNew); ok {
		mode.focusedProvider = resolving
	}
	if !newWizard(dashboard).ProfileAdvanceEnabled(dashboard) {
		t.Fatal("a stale provider header must not disable Next")
	}
	readyKey(dashboard, key(tea.KeyEnter))
	if step := newWizard(dashboard).step; step != WizardStepTarget {
		t.Fatalf("step %v, want Target", step)
	}
}

// newSessionProfileStepShowsWeeklyAndFiveHourPercentages: the profile step
// reports quota as two percentages remaining, one per window, in their own
// columns and with no reset countdown.
func newSessionProfileStepShowsWeeklyAndFiveHourPercentages(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.quotas = map[string]client.ProfileQuota{
		"claude-1": {
			ProfileID: "claude-1",
			Harness:   core.HarnessKindClaude,
			Windows: []client.QuotaWindow{
				{
					Label:                "Week",
					RemainingPercent:     wzPtr(uint8(63)),
					Resets:               wzPtr("09:00 Aug 20"),
					ResetsAtEpochSeconds: wzPtr(int64(604_800)),
				},
				{
					Label:                "5H",
					RemainingPercent:     wzPtr(uint8(40)),
					Resets:               wzPtr("14:00 Aug 13"),
					ResetsAtEpochSeconds: wzPtr(int64(14_400)),
				},
			},
			RefreshedAtEpochSeconds: 0,
		},
	}
	dashboard.beginNew()
	// runningSession opens on codex-1 under OPENAI; move up to ANTHROPIC
	// and expand it so claude-1's quota columns are shown.
	readyKey(dashboard, key(tea.KeyUp))
	readyKey(dashboard, key(tea.KeyUp))
	readyKey(dashboard, key(tea.KeyRight))
	lines := capturedDrawn(dashboard, 120, 30)

	headingIndex := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, "PROFILE") })
	if headingIndex < 0 {
		t.Fatalf("missing profile heading in %q", lines)
	}
	headings := lines[headingIndex]
	wzContains(t, lines[headingIndex+1], "▾ ANTHROPIC")
	claude := lines[headingIndex+2]
	wzContains(t, claude, "claude-1")
	wzSameColumn(t, cellColumn(claude, "63%"), cellColumn(headings, "WEEKLY"), claude)
	wzSameColumn(t, cellColumn(claude, "40%"), cellColumn(headings, "5H"), claude)
	wzNotContains(t, claude, "09:00 Aug 20")
	wzNotContains(t, claude, "resets")
}

func TestRawLocalhostUsesLocalProjectHistory(t *testing.T) {
	configuration := config()
	configuration.Profiles = map[string]core.HarnessProfile{"kimi": wzKimiProfile()}
	configuration.Targets = map[string]core.TargetTemplate{"localhost": core.TargetTemplateLocalBare{}}
	state := core.NewStateState()
	state.RememberProjectDirectory("local", "/home/me/project")
	dashboard := wzNewDashboard(configuration, state)

	readyOpenNewWizard(dashboard)
	terminal := newWizardsTerminal(140, 28)
	terminal.draw(dashboard)
	// Both single-choice selectors were skipped when the wizard opened.
	mode, ok := dashboard.mode.(ModeNew)
	if !ok {
		t.Fatal("expected local project directory step")
	}
	if directory := mode.projectDirectory.String(); directory != "/home/me/project" {
		t.Fatalf("directory %q, want /home/me/project", directory)
	}
	wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionValidateProjectDirectory{
		TargetTemplateID: "localhost",
		Directory:        "/home/me/project",
	})
	dashboard.ApplyResolvedProjectDirectory(dashboard.PathInputContext(), "/home/me/project", "/home/me/project",
		core.ManagedWorktreeOptions{Available: true, DefaultCreate: true}, false, nil)
	check, ok := dashboard.TakePrerequisiteCheck()
	if !ok {
		t.Fatal("the resolved directory starts the creation check")
	}
	wzAction(t, check, DashboardActionCreateSession{
		RememberContainerSize: false,
		CreateManagedWorktree: wzPtr(true),
		WorkspaceID:           core.DefaultWorkspaceID,
		ProfileID:             "kimi",
		BundleID:              core.RawProjectContextID("/home/me/project"),
		ProjectDirectory:      wzPtr("/home/me/project"),
		TargetTemplateID:      "localhost",
		AdditionalMounts:      []core.AdditionalMount{},
	})
}

// dashboardAtMountEditor walks the new-session wizard as far as an open
// mount editor with the source already typed and the destination filled in.
func dashboardAtMountEditor(source string) *DashboardState {
	dashboard := wzNewDashboard(config(), core.NewStateState())
	dashboard.quotaProviders.settle()
	readyOpenNewWizard(dashboard)
	// Move to the collapsed OPENAI provider, expand it, select codex-1, and
	// advance.
	readyKey(dashboard, key(tea.KeyDown))
	readyKey(dashboard, key(tea.KeyRight))
	readyKey(dashboard, key(tea.KeyDown))
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, wzBackTab())
	readyKey(dashboard, key(tea.KeyEnter))
	typeSource(dashboard, source)
	// Enter on the source fills the default destination and moves on.
	readyKey(dashboard, key(tea.KeyEnter))
	return dashboard
}

// wzBackTab is crossterm's BackTab: Bubble Tea reports Shift-Tab as Tab
// with Shift held.
func wzBackTab() tea.KeyPressMsg {
	return tea.KeyPressMsg{Code: tea.KeyTab, Mod: tea.ModShift}
}

func newWizardFocus(dashboard *DashboardState) (WizardControl, bool) {
	return newWizard(dashboard).form.Focused()
}

func wizardMounts(dashboard *DashboardState) *MountWizard {
	return &newWizard(dashboard).mounts
}

// chooseMountAccess opens the focused access combobox, moves down steps
// choices, and accepts.
func chooseMountAccess(t testing.TB, dashboard *DashboardState, steps int) {
	t.Helper()
	readyKey(dashboard, key(tea.KeyEnter))
	if !wizardMounts(dashboard).accessCombo.IsOpen(WizardControlMountAccess) {
		t.Fatal("Enter opens the access combobox")
	}
	for range steps {
		readyKey(dashboard, key(tea.KeyDown))
	}
	readyKey(dashboard, key(tea.KeyEnter))
	if _, open := wizardMounts(dashboard).accessCombo.OpenID(); open {
		t.Fatal("Enter accepts and closes the access combobox")
	}
}

// standardLocalTargetsConfig is the fixture's targets plus the other
// standard local ones that Config.WithLocalTargets supplies on Linux:
// `localhost`, `podman` and `docker`.
func standardLocalTargetsConfig() core.Config {
	configuration := config()
	podman, ok := configuration.Targets["podman"].(core.TargetTemplateLocalPodman)
	if !ok {
		panic("the fixture's podman target is a Podman container")
	}
	configuration.Targets["localhost"] = core.TargetTemplateLocalBare{}
	configuration.Targets["docker"] = core.TargetTemplateLocalDocker{ContainerTemplate: podman.ContainerTemplate}
	// As Config.WithLocalTargets supplies them, not as the user wrote.
	configuration.DefaultTargets = map[string]struct{}{"localhost": {}, "podman": {}, "docker": {}}
	return configuration
}

// bareSshTarget is a bare SSH target, which has no size to set.
func bareSshTarget() core.TargetTemplate {
	return core.TargetTemplateSshBare{
		SSH:             core.SshConnection{Host: "builder.example.com"},
		Permissions:     core.PermissionModeGuardian,
		WorkspacePrefix: ".local/share/hel/workspaces",
	}
}

// answerTargetChecks answers the availability check a wizard asks for when
// it opens: the engines of the targets in uninstalled were never installed
// here, and everything else is there.
func answerTargetChecks(dashboard *DashboardState, uninstalled []string) {
	answerAvailability(dashboard, func(dashboard *DashboardState) client.RuntimeAvailability {
		return availabilityAnswer(dashboard, uninstalled, nil)
	})
}

// pressBack activates the open wizard's Back button from the keyboard: Tab
// from Cancel, which comes just before it, then Enter.
func pressBack(t testing.TB, dashboard *DashboardState) DashboardAction {
	t.Helper()
	switch mode := dashboard.mode.(type) {
	case ModeNew:
		mode.form.Focus(WizardControlCancel)
	case ModeResume:
		mode.form.Focus(WizardControlCancel)
	default:
		panic("expected a wizard")
	}
	dashboard.HandleKey(key(tea.KeyTab))
	var focused *WizardControl
	switch mode := dashboard.mode.(type) {
	case ModeNew:
		if control, ok := mode.form.Focused(); ok {
			focused = &control
		}
	case ModeResume:
		if control, ok := mode.form.Focused(); ok {
			focused = &control
		}
	}
	if focused == nil || *focused != WizardControlBack {
		t.Fatalf("Tab from Cancel focuses Back, got %v", focused)
	}
	return dashboard.HandleKey(key(tea.KeyEnter))
}

func newWizard(dashboard *DashboardState) *NewWizard {
	mode, ok := dashboard.mode.(ModeNew)
	if !ok {
		panic("expected the new-session wizard")
	}
	return mode.NewWizard
}

func TestASourceThatCannotHoldTheOverlaySkipsCopyOnWrite(t *testing.T) {
	dashboard := dashboardAtMountEditor("/nfs/share")

	// Choose copy-on-write before the host reports the filesystem.
	readyKey(dashboard, key(tea.KeyTab))
	chooseMountAccess(t, dashboard, 1)
	if access := wizardMounts(dashboard).access; access != core.MountAccessCow {
		t.Fatalf("access %v, want cow", access)
	}
	for range 3 {
		readyKey(dashboard, key(tea.KeyTab))
	}
	readyKey(dashboard, key(tea.KeyEnter))
	dashboard.ApplyMountSourceValidation("/nfs/share", wzPtr("nfs (network filesystem)"), nil)
	if mounts := wizardMounts(dashboard).mounts; !wzEqual(mounts, []core.AdditionalMount{{
		Source:      "/nfs/share",
		Destination: "/mnt/share",
		Access:      core.MountAccessRo,
	}}) {
		t.Fatalf("mounts %+v", mounts)
	}

	// Reopen the entry: read-only and read-write remain, copy-on-write does
	// not.
	readyKey(dashboard, key(tea.KeyEnter))
	if access := wizardMounts(dashboard).access; access != core.MountAccessRo {
		t.Fatalf("access %v, want ro", access)
	}
	if reason, ok := wizardMounts(dashboard).overlayUnavailable(); !ok || reason != "nfs (network filesystem)" {
		t.Fatalf("overlay unavailable %q (%t)", reason, ok)
	}
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyTab))
	if focused, ok := newWizardFocus(dashboard); !ok || focused != WizardControlMountAccess {
		t.Fatalf("focus %v (%t), want MountAccess", focused, ok)
	}
	if choices := wizardMounts(dashboard).accessChoices(); !slices.Equal(choices, []core.MountAccess{core.MountAccessRo, core.MountAccessRw}) {
		t.Fatalf("choices %v, want ro and rw", choices)
	}
	chooseMountAccess(t, dashboard, 1)
	if access := wizardMounts(dashboard).access; access != core.MountAccessRw {
		t.Fatalf("access %v, want rw", access)
	}

	// The open list offers only the two usable modes.
	readyKey(dashboard, key(tea.KeyEnter))
	terminal := newWizardsTerminal(120, 30)
	terminal.draw(dashboard)
	rendered := wzBufferSymbols(terminal.buffer)
	wzContains(t, rendered, "rw · writes reach the host directory")
	wzContains(t, rendered, "ro · read-only")
	wzNotContains(t, rendered, "cow ·")

	// Escape closes only the list; the changed mode is an unsaved edit, so
	// leaving the editor then asks before discarding it.
	readyKey(dashboard, key(tea.KeyEscape))
	if _, open := wizardMounts(dashboard).accessCombo.OpenID(); open {
		t.Fatal("Escape closes the list")
	}
	if mode, ok := dashboard.mode.(ModeNew); !ok || mode.step != WizardStepMounts {
		t.Fatal("Escape keeps the mount editor")
	}
	readyKey(dashboard, key(tea.KeyEscape))
	if !dashboard.dialogConfirmationOpen() {
		t.Fatal("leaving with an unsaved edit asks first")
	}
}

// Hard-won: 6c5ea1b36: default destinations were joined with `\` on Windows (`/mnt\cache`).
func TestRecentMountEntryUsesAddValidationAndReturnsToReview(t *testing.T) {
	dashboard := dashboardAtMountEditor("")
	terminal := newWizardsTerminal(120, 30)
	terminal.draw(dashboard)
	wzNotContains(t, strings.Join(terminal.lines(), "\n"), "Recent on this host:")
	newWizard(dashboard).mounts.history = []string{"/opt/cache"}
	terminal.draw(dashboard)
	newWizard(dashboard).form.Focus(WizardControlMountHistory)
	if action := readyKey(dashboard, key(tea.KeyEnter)); !isValidateMountSource(action) {
		t.Fatalf("action %s, want a mount source validation", wzDebug(action))
	}
	if source := wizardMounts(dashboard).source.String(); source != "/opt/cache" {
		t.Fatalf("source %q, want /opt/cache", source)
	}
	if destination := wizardMounts(dashboard).destination.String(); destination != "/mnt/cache" {
		t.Fatalf("destination %q, want /mnt/cache", destination)
	}
	dashboard.ApplyMountSourceValidation("/opt/cache", nil, nil)
	if mode, ok := dashboard.mode.(ModeNew); !ok || mode.step != WizardStepReview {
		t.Fatal("the validated entry returns to the review")
	}
}

func isValidateMountSource(action DashboardAction) bool {
	_, ok := action.(DashboardActionValidateMountSource)
	return ok
}

func TestRecentMountListFiltersAttachedSourcesAndScrolls(t *testing.T) {
	var history []string
	for n := range 8 {
		history = append(history, fmt.Sprintf("/opt/recent-%d", n))
	}
	mounts := newMountWizard(history)
	mounts.mounts = append(mounts.mounts, core.AdditionalMount{
		Source:      "/opt/recent-0",
		Destination: "/mnt/recent-0",
		Access:      core.MountAccessRo,
	})
	if available := mounts.availableHistory(); len(available) != 7 || available[0] != "/opt/recent-1" {
		t.Fatalf("available history %q", available)
	}
	editing := 0
	mounts.editingMount = &editing
	if available := mounts.availableHistory(); len(available) != 8 {
		t.Fatalf("available history %q, want all 8 while editing", available)
	}

	dashboard := dashboardAtMountEditor("")
	newWizard(dashboard).mounts.history = mounts.history
	terminal := newWizardsTerminal(120, 30)
	terminal.draw(dashboard)
	newWizard(dashboard).form.Focus(WizardControlMountHistory)
	for range 7 {
		readyKey(dashboard, key(tea.KeyDown))
	}
	if selection := wizardMounts(dashboard).historySelection; selection != 7 {
		t.Fatalf("history selection %d, want 7", selection)
	}
	terminal.draw(dashboard)
	lines := terminal.lines()
	captureRender(120, 30, lines)
	wzContains(t, strings.Join(lines, "\n"), "/opt/recent-7")
	small := newWizardsTerminal(60, 22)
	small.draw(dashboard)
	wzContains(t, strings.Join(small.lines(), "\n"), "/opt/recent-7")
}

func TestRecentMountSingleClickSelectsAndDoubleClickValidates(t *testing.T) {
	dashboard := dashboardAtMountEditor("")
	newWizard(dashboard).mounts.history = []string{"/opt/cache"}
	terminal := newWizardsTerminal(120, 30)
	wzAction(t, clickProjectText(t, dashboard, terminal, "/opt/cache"), DashboardActionNone{})
	if !wizardMounts(dashboard).source.IsEmpty() {
		t.Fatal("a single click only selects")
	}
	if action := clickProjectText(t, dashboard, terminal, "/opt/cache"); !isValidateMountSource(action) {
		t.Fatalf("action %s, want a mount source validation", wzDebug(action))
	}
}

func TestRecentMountEntryWorksInMoveWizard(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginResumeFor("session-1")
	wizard := resumeWizard(dashboard)
	wizard.step = WizardStepMounts
	wizard.mounts.history = []string{"/opt/cache"}
	terminal := newWizardsTerminal(120, 30)
	terminal.draw(dashboard)
	resumeWizard(dashboard).form.Focus(WizardControlMountHistory)
	if action := readyKey(dashboard, key(tea.KeyEnter)); !isValidateMountSource(action) {
		t.Fatalf("action %s, want a mount source validation", wzDebug(action))
	}
	dashboard.ApplyMountSourceValidation("/opt/cache", nil, nil)
	if mode, ok := dashboard.mode.(ModeResume); !ok || mode.step != WizardStepReview {
		t.Fatal("the validated entry returns to the review")
	}
}

func TestFailedSubmitPreflightReopensTheInvalidMount(t *testing.T) {
	dashboard := dashboardAtMountEditor("/opt/cache")
	readyKey(dashboard, key(tea.KeyEnter))
	dashboard.ApplyMountSourceValidation("/opt/cache", nil, nil)
	if check, _ := dashboard.TakePrerequisiteCheck(); !isValidateSessionMounts(check) {
		t.Fatalf("check %s, want a session mount validation", wzDebug(check))
	}

	dashboard.ApplySessionMountPreflightFailure("/opt/cache",
		"source path /opt/cache does not exist or is not a directory")

	mode, ok := dashboard.mode.(ModeNew)
	if !ok {
		t.Fatal("preflight failure should keep the new-session dialog open")
	}
	if mode.step != WizardStepMounts {
		t.Fatalf("step %v, want Mounts", mode.step)
	}
	if source := mode.mounts.source.String(); source != "/opt/cache" {
		t.Fatalf("source %q, want /opt/cache", source)
	}
	if mode.mounts.error == nil || *mode.mounts.error != "source path /opt/cache does not exist or is not a directory" {
		t.Fatalf("error %v", mode.mounts.error)
	}

	// The failed check is over, so correcting the directory starts a new one
	// instead of leaving the review waiting.
	dashboard.ApplyMountSourceValidation("/opt/cache", nil, nil)
	if check, _ := dashboard.TakePrerequisiteCheck(); !isValidateSessionMounts(check) {
		t.Fatalf("check %s, want a session mount validation", wzDebug(check))
	}
}

func isValidateSessionMounts(action DashboardAction) bool {
	_, ok := action.(DashboardActionValidateSessionMounts)
	return ok
}

func directoryCompletionListsEveryCandidateAndSelectsWithKeys(t *testing.T) {
	dashboard := wzNewDashboard(config(), core.NewStateState())
	readyOpenNewWizard(dashboard)
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, wzBackTab())
	readyKey(dashboard, key(tea.KeyEnter))
	typeSource(dashboard, "/opt/")
	var candidates []string
	for index := range 12 {
		candidates = append(candidates, fmt.Sprintf("/opt/directory-%d/", index))
	}
	dashboard.ApplyPathCompletions(dashboard.PathInputContext(), "/opt/", core.PathCompletion{
		Candidates: slices.Clone(candidates),
	})

	// The popup scrolls, so every candidate the host sent is kept.
	if completions := newWizard(dashboard).mounts.source.Completions(); len(completions) != len(candidates) {
		t.Fatalf("completions %q, want all %d", completions, len(candidates))
	}
	rendered := strings.Join(capturedDrawn(dashboard, 100, 30), "\n")
	wzContains(t, rendered, " matches ")
	wzContains(t, rendered, "/opt/directory-0/")

	readyKey(dashboard, key(tea.KeyDown))
	readyKey(dashboard, key(tea.KeyEnter))
	wizard := newWizard(dashboard)
	if source := wizard.mounts.source.String(); source != "/opt/directory-1/" {
		t.Fatalf("source %q, want /opt/directory-1/", source)
	}
	if wizard.mounts.source.IsCompleting() {
		t.Fatal("Enter accepts the completion")
	}

	rendered = strings.Join(capturedDrawn(dashboard, 100, 30), "\n")
	wzContains(t, rendered, "Add directory")
	wzContains(t, rendered, "Cancel")
}

func TestResumeKeepsTheWorkspaceWhereItsDialogWasOpened(t *testing.T) {
	workspaceA := core.DefaultWorkspaceID
	workspaceB := "workspace-b"
	dashboard := dashboardWithSession(stoppedSession())
	openResumeWizard(dashboard)
	dashboard.SetActiveWorkspace(&workspaceB)

	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	preflight, ok := readyKey(dashboard, key(tea.KeyEnter)).(DashboardActionPreflightResumeRepositories)
	if !ok {
		t.Fatal("resume should retain its captured workspace")
	}
	if launch, ok := preflight.Launch.(DashboardActionResumeSession); !ok || launch.WorkspaceID != workspaceA {
		t.Fatalf("launch %s, want a resume in %s", wzDebug(preflight.Launch), workspaceA)
	}
}

func TestResumeRefusesATargetTheSessionCannotUseAndSaysWhy(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.config.Mut().Targets["bare"] = core.TargetTemplateLocalBare{}

	openResumeWizard(dashboard)
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyUp))
	if target := nthKey(dashboard.config.Get().Targets, resumeWizard(dashboard).target); target != "bare" {
		t.Fatalf("target %q, want bare", target)
	}

	wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionNone{})

	if step := resumeWizard(dashboard).step; step != WizardStepTarget {
		t.Fatalf("step %v, want Target", step)
	}
	notice, _ := dashboard.notices.Current()
	wzContains(t, notice, "came from GitHub")
}

func resumeWizard(dashboard *DashboardState) *ResumeWizard {
	mode, ok := dashboard.mode.(ModeResume)
	if !ok {
		panic("expected resume wizard")
	}
	return mode.ResumeWizard
}

func openMoveReview(t testing.TB, dashboard *DashboardState) uint64 {
	t.Helper()
	dashboard.FocusSessions()
	wzAction(t, dashboard.beginMove(), DashboardActionNone{})
	wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionNone{},
		"profile selection advances to the move target")
	move, ok := readyKey(dashboard, key(tea.KeyEnter)).(DashboardActionMoveSession)
	if !ok || move.PreparationRequestID == nil {
		t.Fatal("entering move review should request preparation")
	}
	if !resumeWizard(dashboard).preparing {
		t.Fatal("the review is preparing")
	}
	if step := resumeWizard(dashboard).step; step != WizardStepReview {
		t.Fatalf("step %v, want Review", step)
	}
	return *move.PreparationRequestID
}

func movePreparation() core.MovePreparation {
	return core.MovePreparation{
		SkipKeepSizes:     []core.SkipKeepSize{},
		InPlace:           false,
		StopsSubagents:    true,
		SourceUnavailable: false,
		Selection: core.MoveSelection{
			SkipKeepPaths:    []string{},
			SessionID:        "session-1",
			ProfileID:        wzPtr("codex-1"),
			TargetTemplateID: wzPtr("podman"),
			AdditionalMounts: &[]core.AdditionalMount{},
		},
		SizedByDestinationDefault: false,
		SourceProfileID:           "codex-1",
		SourceTargetTemplateID:    "podman",
		CrossHarness:              false,
		Active:                    true,
		QueuedCommands:            []core.MaterializedQueuedPrompt{},
		Fingerprint:               "fingerprint",
		OperationID:               "move-1",
	}
}

func rawConversionPreview() core.RawConversionPreview {
	return core.RawConversionPreview{
		Checkout:             "/work/repo",
		Destination:          "/workspace/repo",
		Branch:               wzPtr("mj/session-1"),
		FetchURL:             "https://github.com/example/repo.git",
		PushURLs:             []string{"https://github.com/example/repo.git"},
		DefaultBranch:        "main",
		UnpushedCommits:      2,
		StagedFiles:          1,
		UnstagedFiles:        1,
		UntrackedFiles:       3,
		UntrackedBytes:       2_621_440,
		HostCheckoutRetained: true,
	}
}

// wzSpacedText is the lines joined and their whitespace runs collapsed to
// single spaces.
func wzSpacedText(lines []string) string {
	return strings.Join(strings.Fields(strings.Join(lines, " ")), " ")
}

// A Move stops the parent's sub-agents as a suspend does. The review counts
// only the ones still at their task; idle ones have handed back and are not
// mentioned. An in-place Move that keeps its children says nothing.
// Hard-won: 63ed79ef: Move review counted idle children as needing suspension
func TestMoveReviewCountsOnlyWorkingSubagents(t *testing.T) {
	reviewWith := func(working, stopsSubagents bool) string {
		dashboard, _ := dashboardWithOneSubagent()
		if working {
			setWorking(dashboard, "child-session")
		}
		requestID := openMoveReview(t, dashboard)
		preparation := movePreparation()
		preparation.InPlace = !stopsSubagents
		preparation.StopsSubagents = stopsSubagents
		if !dashboard.ApplyMovePreparation(requestID, preparation) {
			t.Fatal("the preparation applies")
		}
		terminal := newWizardsTerminal(200, 44)
		terminal.draw(dashboard)
		return wzSpacedText(terminal.lines())
	}
	idle := reviewWith(false, true)
	wzNotContains(t, idle, "will be stopped")
	working := reviewWith(true, true)
	wzContains(t, working, "1 working sub-agent will be stopped; the session is told which when it resumes.")
	kept := reviewWith(true, false)
	wzNotContains(t, kept, "will be stopped")
	wzContains(t, kept, "Only the harness and profile are replaced")
}

// wzSnapshotMode is what Rust's `dashboard.mode.clone()` keeps for a later
// comparison: the wizards are copied, so a later edit of the open one does
// not change the snapshot.
func wzSnapshotMode(mode Mode) Mode {
	switch mode := mode.(type) {
	case ModeResume:
		snapshot := *mode.ResumeWizard
		return ModeResume{&snapshot}
	case ModeNew:
		snapshot := *mode.NewWizard
		return ModeNew{&snapshot}
	}
	return mode
}

// wzModeEqual is Rust's PartialEq for Mode: the wizards compare every field
// but their forms.
func wzModeEqual(a, b Mode) bool {
	switch a := a.(type) {
	case ModeResume:
		other, ok := b.(ModeResume)
		return ok && a.equal(other.ResumeWizard)
	case ModeNew:
		other, ok := b.(ModeNew)
		return ok && a.equal(other.NewWizard)
	}
	return wzEqual(a, b)
}

func wzResumeLaunchFixture() DashboardActionResumeSession {
	return DashboardActionResumeSession{
		WorkspaceID:      core.DefaultWorkspaceID,
		SessionID:        "session-1",
		ProfileID:        "codex-1",
		TargetTemplateID: "podman",
		AdditionalMounts: []core.AdditionalMount{},
		DiscardQueue:     false,
	}
}

// The resume preflight answers "this moves your checkout" before anything
// is stopped. Nothing may launch until the person says yes, and cancelling
// has to leave the wizard exactly where it was.
func TestARawConversionResumeLaunchesOnlyAfterItIsConfirmed(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	openResumeWizard(dashboard)
	before := wzSnapshotMode(dashboard.mode)
	launch := wzResumeLaunchFixture()
	receipt := resumeReceipt()

	dashboard.ShowRawConversionConfirmation(launch, receipt, rawConversionPreview())
	terminal := newWizardsTerminal(200, 44)
	terminal.draw(dashboard)
	wzContains(t, wzSpacedText(terminal.lines()), "2 commits not on https://github.com/example/repo.git travel")

	// Cancel is focused first and returns to the wizard without launching.
	wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionNone{})
	if !wzModeEqual(dashboard.mode, before) {
		t.Fatalf("Cancel must restore the wizard, mode %T", dashboard.mode)
	}

	dashboard.ShowRawConversionConfirmation(launch, receipt, rawConversionPreview())
	wzAction(t, readyKey(dashboard, key(tea.KeyTab)), DashboardActionNone{})
	confirmed := readyKey(dashboard, key(tea.KeyEnter))
	wzAction(t, confirmed, DashboardActionConfirmRawConversion{Launch: launch, Receipt: receipt})
}

// Hard-won: 16e952f7: same-harness resumes incorrectly displayed a Lossy warning
func TestResumeProfileStepMarksCrossHarnessProfilesAsLossy(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	openResumeWizard(dashboard)
	terminal := newWizardsTerminal(120, 24)
	terminal.draw(dashboard)
	rendered := wzBufferSymbols(terminal.buffer)

	wzContains(t, rendered, "(lossy: text-only transcript)")
	wzContains(t, rendered, "Resume · 1/3")
	// I1-6: the session's own profile is selected, and a same-harness
	// resume continues the native conversation, so it carries no loss
	// warning.
	wzContains(t, rendered, "Same agent: the conversation continues natively.")
	wzNotContains(t, rendered, "Lossy: text only")

	wizard := resumeWizard(dashboard)
	initial := wizard.profile
	session, ok := dashboard.state.Get().Sessions.Get(wizard.sessionID)
	if !ok {
		t.Fatalf("no session %q", wizard.sessionID)
	}
	lossy := slices.IndexFunc(dashboard.resumeWizardProfiles(wizard), func(profile CompatibleProfile) bool {
		return profile.Kind != session.HarnessKind
	})
	if lossy < 0 {
		t.Fatal("the fixture offers a cross-harness profile")
	}
	resumeWizard(dashboard).profile = lossy
	terminal.draw(dashboard)
	wzContains(t, wzBufferSymbols(terminal.buffer), "Lossy: text only; tool calls + reasoning dropped.")
	resumeWizard(dashboard).profile = initial

	readyKey(dashboard, key(tea.KeyEnter))
	terminal.draw(dashboard)
	wzContains(t, wzBufferSymbols(terminal.buffer), "Resume · 2/3")

	readyKey(dashboard, key(tea.KeyEnter))
	terminal.draw(dashboard)
	wzContains(t, wzBufferSymbols(terminal.buffer), "Resume · 3/3")
}

// pressArchiveRestore opens the restore wizard for wikiID, walks it to the
// review, and presses its Restore button.
func pressArchiveRestore(t testing.TB, dashboard *DashboardState, wikiID string) DashboardAction {
	t.Helper()
	wzAction(t, dashboard.beginArchiveRestore(wikiID, "Pomegranate work", nil, nil), DashboardActionNone{})
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	mode, ok := dashboard.mode.(ModeResume)
	if !ok {
		t.Fatalf("expected the restore wizard, got %T", dashboard.mode)
	}
	if mode.step != WizardStepReview {
		t.Fatalf("step %v, want Review", mode.step)
	}
	return readyKey(dashboard, key(tea.KeyEnter))
}

func restores(action DashboardAction, expected string) bool {
	restore, ok := action.(DashboardActionRestoreArchivedSession)
	return ok && restore.WikiID == expected
}

// wzInDashboard fails t unless no dialog is open.
func wzInDashboard(t testing.TB, dashboard *DashboardState) {
	t.Helper()
	if _, ok := dashboard.mode.(ModeDashboard); !ok {
		t.Fatalf("mode %T, want the dashboard", dashboard.mode)
	}
}

// wzNotice fails t unless the current notice is want.
func wzNotice(t testing.TB, dashboard *DashboardState, want string) {
	t.Helper()
	if notice, ok := dashboard.Notice(); !ok || notice != want {
		t.Fatalf("notice %q (%t), want %q", notice, ok, want)
	}
}

// Restore on a restore's review closes the wizard, as a live resume does
// once its launch starts. The restore used to leave the review open, and a
// second press restored the archive a second time.
// Hard-won: 9332aae3: successful restore left its review open and allowed a second restore
func TestRestoringAnArchiveClosesTheWizardWhenRestoreIsPressed(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	action := pressArchiveRestore(t, dashboard, "wiki-1")
	if !restores(action, "wiki-1") {
		t.Fatalf("action %s", wzDebug(action))
	}
	wzInDashboard(t, dashboard)

	second := readyKey(dashboard, key(tea.KeyEnter))
	if _, ok := second.(DashboardActionRestoreArchivedSession); ok {
		t.Fatalf("a second restore: %s", wzDebug(second))
	}
}

// Until a restore reports back, its archive cannot be restored again.
// Opening it is refused with a notice; other archives are not held up.
// Hard-won: 9332aae3: an archive could be restored again while its first restore was pending
func TestAnArchiveCannotBeRestoredAgainUntilItsRestoreReportsBack(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	first := pressArchiveRestore(t, dashboard, "wiki-1")
	if !restores(first, "wiki-1") {
		t.Fatalf("action %s", wzDebug(first))
	}

	wzAction(t, dashboard.beginArchiveRestore("wiki-1", "Pomegranate work", nil, nil), DashboardActionNone{})
	wzInDashboard(t, dashboard)
	wzNotice(t, dashboard, "Restore already started.")

	other := pressArchiveRestore(t, dashboard, "wiki-2")
	if !restores(other, "wiki-2") {
		t.Fatalf("action %s", wzDebug(other))
	}

	// The dashboard loop ends the restore when the daemon registers the new
	// session. After that the archive can be restored again.
	dashboard.FinishArchiveRestore(first)
	again := pressArchiveRestore(t, dashboard, "wiki-1")
	if !restores(again, "wiki-1") {
		t.Fatalf("action %s", wzDebug(again))
	}
}

// The review refuses Restore for an archive whose restore has already
// started, whatever started it, and keeps the draft on screen.
// Hard-won: 9332aae3: pressing Restore during an in-flight restore was accepted
func TestPressingRestoreAfterTheRestoreStartedReturnsNoAction(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.beginArchiveRestore("wiki-1", "Pomegranate work", nil, nil)
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	if !dashboard.claimArchiveRestore("wiki-1") {
		t.Fatal("the restore can be claimed")
	}

	wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionNone{})
	if mode, ok := dashboard.mode.(ModeResume); !ok || mode.step != WizardStepReview {
		t.Fatalf("mode %T, want the restore review", dashboard.mode)
	}
	wzNotice(t, dashboard, "Restore already started.")
}

// A failed restore shows the launch-failure dialog over the dashboard, not
// over a restore review that could be pressed again. Its Retry sends the
// same restore once, and that retry holds the archive until it reports back.
// Hard-won: 9332aae3: restore failures were silent and could not be retried
func TestAFailedRestoreReportsTheFailureAndRetriesItOnce(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	restore := pressArchiveRestore(t, dashboard, "wiki-1")
	if !restores(restore, "wiki-1") {
		t.Fatalf("action %s", wzDebug(restore))
	}

	// What the dashboard loop does with the restore's failure report.
	dashboard.FinishArchiveRestore(restore)
	dashboard.ShowLaunchFailure("wiki restore failed", restore)
	terminal := newWizardsTerminal(90, 25)
	terminal.draw(dashboard)
	text := strings.Join(terminal.lines(), "\n")
	wzContains(t, text, "Launch failed")
	wzContains(t, text, "wiki restore failed")

	dashboard.HandleKey(key(tea.KeyRight))
	wzAction(t, dashboard.HandleKey(key(tea.KeyEnter)), restore)
	wzInDashboard(t, dashboard)
	wzAction(t, dashboard.beginArchiveRestore("wiki-1", "Pomegranate work", nil, nil), DashboardActionNone{})
	wzInDashboard(t, dashboard)

	dashboard.FinishArchiveRestore(restore)
	again := pressArchiveRestore(t, dashboard, "wiki-1")
	if !restores(again, "wiki-1") {
		t.Fatalf("action %s", wzDebug(again))
	}
}

func cacheMount() core.AdditionalMount {
	return core.AdditionalMount{
		Source:      "/opt/cache",
		Destination: "/mnt/cache",
		Access:      core.MountAccessCow,
	}
}

// pressResume walks the resume wizard for the stopped session to its review
// and presses Resume.
func pressResume(t testing.TB, dashboard *DashboardState) DashboardAction {
	t.Helper()
	openResumeWizard(dashboard)
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	if step := resumeWizard(dashboard).step; step != WizardStepReview {
		t.Fatalf("step %v, want Review", step)
	}
	return readyKey(dashboard, key(tea.KeyEnter))
}

// resumeLaunch is the resume launch inside the check that Resume sends.
func resumeLaunch(t testing.TB, check DashboardAction) DashboardAction {
	t.Helper()
	switch check := check.(type) {
	case DashboardActionValidateSessionMounts:
		return resumeLaunch(t, check.Launch)
	case DashboardActionPreflightResumeRepositories:
		if _, ok := check.Launch.(DashboardActionResumeSession); !ok {
			t.Fatalf("launch %s", wzDebug(check.Launch))
		}
		return check.Launch
	}
	t.Fatalf("expected the resume check, got %s", wzDebug(check))
	return nil
}

func resumeReceipt() core.ResumeRepositorySourceReceipt {
	return core.ResumeRepositorySourceReceipt{
		SessionID:        "session-1",
		BundleID:         "hel",
		CheckpointSha256: strings.Repeat("a", 64),
		Repositories:     []core.ProjectRepository{},
	}
}

// deliverResumeCheckResult is what the dashboard loop does with a result of
// the resume check (SessionMountValidation and ResumeRepositoryPreflight in
// mj-cli/src/dashboard/io.rs): it applies the result while the generation
// the check was sent under is current, and drops it otherwise. Answers
// whether the result was applied.
func deliverResumeCheckResult(dashboard *DashboardState, generation uint64, apply func(*DashboardState)) bool {
	if generation != dashboard.SessionPreflightGeneration() {
		return false
	}
	apply(dashboard)
	return true
}

// Resume on the review stays open while the check runs. A second press used
// to send a second check beside the first, with or without attached
// directories to check first.
// Hard-won: 50b85cae: Resume could send duplicate checks
func TestPressingResumeTwiceSendsOneCheck(t *testing.T) {
	for _, mounts := range [][]core.AdditionalMount{{}, {cacheMount()}} {
		withMounts := len(mounts) != 0
		session := stoppedSession()
		session.AdditionalMounts = mounts
		dashboard := dashboardWithSession(session)

		first := pressResume(t, dashboard)
		if withMounts {
			if !isValidateSessionMounts(first) {
				t.Fatalf("first %s", wzDebug(first))
			}
		} else if _, ok := first.(DashboardActionPreflightResumeRepositories); !ok {
			t.Fatalf("first %s", wzDebug(first))
		}

		wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionNone{}, "with mounts: ", withMounts)
		wzNotice(t, dashboard, "Resume already started.")
		if step := resumeWizard(dashboard).step; step != WizardStepReview {
			t.Fatalf("step %v, want Review", step)
		}
	}
}

// Once the check has reported back, another result sent under the same
// generation is dropped. A late Ready used to close whatever the first
// result had opened and start the launch anyway.
// Hard-won: 50b85cae: late Ready could close the wrong Resume dialog and launch it
func TestALateSecondReadyIsDropped(t *testing.T) {
	for _, first := range []string{"Ready", "RawConversion", "MovedOrigin", "AttachmentFailure"} {
		session := stoppedSession()
		if first == "AttachmentFailure" {
			session.AdditionalMounts = []core.AdditionalMount{cacheMount()}
		}
		dashboard := dashboardWithSession(session)
		launch := resumeLaunch(t, pressResume(t, dashboard))
		// The dashboard loop sends the check under the generation current
		// when it receives the action.
		generation := dashboard.SessionPreflightGeneration()

		applied := deliverResumeCheckResult(dashboard, generation, func(dashboard *DashboardState) {
			switch first {
			case "Ready":
				dashboard.FinishResumeRepositoryPreflight()
			case "RawConversion":
				dashboard.ShowRawConversionConfirmation(launch, resumeReceipt(), rawConversionPreview())
			case "MovedOrigin":
				dashboard.ShowRepositoryOriginDialog("session-1", "hel", strings.Repeat("b", 40),
					"https://github.com/example/old.git", "https://github.com/example/new.git", launch)
			case "AttachmentFailure":
				dashboard.ApplySessionMountPreflightFailure("/opt/cache",
					"source path /opt/cache does not exist or is not a directory")
			}
		})
		if !applied {
			t.Fatalf("%s: the first result was dropped", first)
		}
		afterFirst := wzSnapshotMode(dashboard.mode)

		late := deliverResumeCheckResult(dashboard, generation, func(dashboard *DashboardState) {
			dashboard.FinishResumeRepositoryPreflight()
		})
		if late {
			t.Fatalf("%s: the late Ready was applied", first)
		}
		if !wzModeEqual(dashboard.mode, afterFirst) {
			t.Fatalf("%s: the mode changed", first)
		}
	}
}

// A second raw-conversion result for the same check is dropped instead of
// opening a second confirmation over the first. Cancel on the one
// confirmation returns to the wizard, where Resume starts a new check.
// Hard-won: 50b85cae: late raw conversion could stack another confirmation
func TestALateSecondRawConversionResultDoesNotStackADialog(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	launch := resumeLaunch(t, pressResume(t, dashboard))
	generation := dashboard.SessionPreflightGeneration()
	show := func(dashboard *DashboardState) {
		dashboard.ShowRawConversionConfirmation(launch, resumeReceipt(), rawConversionPreview())
	}

	if !deliverResumeCheckResult(dashboard, generation, show) {
		t.Fatal("the first result applies")
	}
	if deliverResumeCheckResult(dashboard, generation, show) {
		t.Fatal("the second result is dropped")
	}
	confirm, ok := dashboard.mode.(ModeConfirm)
	if !ok {
		t.Fatalf("expected the conversion confirmation, got %T", dashboard.mode)
	}
	conversion, ok := confirm.confirmation.(ConfirmationConvertRawCheckout)
	if !ok {
		t.Fatalf("expected the conversion confirmation, got %T", confirm.confirmation)
	}
	if _, ok := conversion.Previous.(ModeResume); !ok {
		t.Fatalf("the confirmation sits over another dialog: %T", conversion.Previous)
	}

	// Cancel is focused first.
	wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionNone{})
	if step := resumeWizard(dashboard).step; step != WizardStepReview {
		t.Fatalf("step %v, want Review", step)
	}
	again := readyKey(dashboard, key(tea.KeyEnter))
	if _, ok := again.(DashboardActionPreflightResumeRepositories); !ok {
		t.Fatalf("again %s", wzDebug(again))
	}
}

// A failed check ends the hold, so Resume on the same review sends a new
// check. Closing the wizard ends it too: its check's result is dropped and a
// reopened wizard's Resume is not refused.
func TestResumeStartsANewCheckOnceTheFirstFailsOrTheWizardCloses(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	pressResume(t, dashboard)
	generation := dashboard.SessionPreflightGeneration()
	// What the dashboard loop does when the repository check fails.
	if !deliverResumeCheckResult(dashboard, generation, func(dashboard *DashboardState) {
		dashboard.SetNotice("Could not check checkpoint repositories: offline")
		dashboard.EndResumePreflight()
	}) {
		t.Fatal("the failure applies")
	}
	if step := resumeWizard(dashboard).step; step != WizardStepReview {
		t.Fatalf("step %v, want Review", step)
	}
	retry := readyKey(dashboard, key(tea.KeyEnter))
	if _, ok := retry.(DashboardActionPreflightResumeRepositories); !ok {
		t.Fatalf("retry %s", wzDebug(retry))
	}

	generation = dashboard.SessionPreflightGeneration()
	readyKey(dashboard, key(tea.KeyEscape))
	wzInDashboard(t, dashboard)
	if deliverResumeCheckResult(dashboard, generation, func(dashboard *DashboardState) {
		dashboard.FinishResumeRepositoryPreflight()
	}) {
		t.Fatal("the closed wizard's result is dropped")
	}
	reopened := pressResume(t, dashboard)
	if _, ok := reopened.(DashboardActionPreflightResumeRepositories); !ok {
		t.Fatalf("reopened %s", wzDebug(reopened))
	}
}

func resumeProfileStepAlignsItsColumns(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.config.Mut().Profiles["kimi-1"] = wzKimiProfile()
	openResumeWizard(dashboard)
	terminal := newWizardsTerminal(120, 24)
	terminal.draw(dashboard)
	lines := terminal.lines()
	captureRender(120, 24, lines)
	row := func(needle string) string {
		index := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, needle) })
		if index < 0 {
			t.Fatalf("missing row %q in %q", needle, lines)
		}
		return lines[index]
	}
	// The session runs Codex, so the Claude and Kimi rows carry the note
	// that says the cross-harness resume drops everything but text.
	headings := row("PROFILE")
	profileColumn, harnessColumn, weeklyColumn :=
		cellColumn(headings, "PROFILE"), cellColumn(headings, "HARNESS"), cellColumn(headings, "WEEKLY")
	noteColumn := -1
	for _, profile := range []struct {
		id, harness string
		lossy       bool
	}{
		{"claude-1", "Claude Code", true},
		{"codex-1", "Codex", false},
		{"kimi-1", "Kimi Code", true},
	} {
		line := row(profile.id)
		wzSameColumn(t, cellColumn(line, profile.id), profileColumn, line)
		wzSameColumn(t, cellColumn(line, profile.harness), harnessColumn, line)
		wzSameColumn(t, cellColumn(line, "refreshing"), weeklyColumn, line)
		if profile.lossy {
			column := cellColumn(line, "(lossy: text-only transcript)")
			if column <= weeklyColumn {
				t.Fatalf("%s: the note is left of WEEKLY: %q", profile.id, line)
			}
			if noteColumn < 0 {
				noteColumn = column
			}
			wzSameColumn(t, column, noteColumn, line)
		} else {
			wzNotContains(t, line, "(lossy")
		}
	}
}

func TestTakingMovePreparationClosesOnlyAValidConfirmationHandoff(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	requestID := openMoveReview(t, dashboard)

	// A submission that arrives before preparation is ready must leave the
	// wizard open so the user can wait or reprepare the move.
	wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionNone{})
	if taken := dashboard.TakeMovePreparation("session-1"); taken != nil {
		t.Fatal("nothing is prepared yet")
	}
	if _, ok := dashboard.mode.(ModeResume); !ok {
		t.Fatalf("mode %T, want the move wizard", dashboard.mode)
	}

	preparation := movePreparation()
	inheritedSize := core.SessionResourceAllocation(core.SessionResourceAllocationContainer{Cpus: 4, MemoryBytes: 8 << 30})
	resumeWizard(dashboard).resourceAllocation = inheritedSize
	if preparation.Selection.ResourceAllocation != nil {
		t.Fatal("the fixture selection carries no size")
	}
	if !dashboard.ApplyMovePreparation(requestID, preparation) {
		t.Fatal("the preparation applies")
	}
	if allocation := resumeWizard(dashboard).resourceAllocation; !wzEqual(allocation, inheritedSize) {
		t.Fatalf("allocation %s, want the inherited size", wzDebugAllocation(allocation))
	}

	// A stale action for another session must not consume the confirmation.
	if taken := dashboard.TakeMovePreparation("other-session"); taken != nil {
		t.Fatal("another session's handoff took the preparation")
	}
	mode, ok := dashboard.mode.(ModeResume)
	if !ok {
		t.Fatal("invalid session handoff must keep the move wizard open")
	}
	if mode.preparation == nil || !wzEqual(*mode.preparation, preparation) {
		t.Fatal("the preparation is retained")
	}

	// Once ready, one click emits execution directly. The controller then
	// takes the retained preparation as the lifecycle handoff.
	if move, ok := readyKey(dashboard, key(tea.KeyEnter)).(DashboardActionMoveSession); !ok || move.PreparationRequestID != nil {
		t.Fatal("the ready review executes the move")
	}
	if taken := dashboard.TakeMovePreparation("session-1"); taken == nil || !wzEqual(*taken, preparation) {
		t.Fatal("the handoff takes the retained preparation")
	}
	wzInDashboard(t, dashboard)

	// State/lifecycle replies arriving after handoff must not resurrect the
	// confirmation modal or reapply the consumed preparation.
	dashboard.SetState(*dashboard.state.Get())
	if dashboard.ApplyMovePreparation(requestID, preparation) {
		t.Fatal("the consumed preparation was applied again")
	}
	wzInDashboard(t, dashboard)
}

// wzPreparationRequest is the preparation request a Move action carries,
// failing t when it carries none.
func wzPreparationRequest(t testing.TB, action DashboardAction, context string) uint64 {
	t.Helper()
	move, ok := action.(DashboardActionMoveSession)
	if !ok || move.PreparationRequestID == nil {
		t.Fatalf("%s, got %s", context, wzDebug(action))
	}
	return *move.PreparationRequestID
}

func TestFailedMovePreparationStaysVisibleAndRetryRequestsPreparation(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	requestID := openMoveReview(t, dashboard)
	if !dashboard.SetMovePreparationFailed("session-1", requestID, "destination is unavailable") {
		t.Fatal("the failure applies")
	}

	mode, ok := dashboard.mode.(ModeResume)
	if !ok {
		t.Fatal("preparation failure should keep the review open")
	}
	if mode.preparing || mode.preparation != nil {
		t.Fatal("the failed preparation is neither running nor ready")
	}
	if mode.preparationError == nil || *mode.preparationError != "destination is unavailable" {
		t.Fatalf("error %v", mode.preparationError)
	}

	terminal := newWizardsTerminal(120, 28)
	terminal.draw(dashboard)
	rendered := strings.Join(terminal.lines(), "\n")
	wzContains(t, rendered, "Move preparation failed: destination is unavailable")
	wzContains(t, rendered, "Retry")

	retryID := wzPreparationRequest(t, readyKey(dashboard, key(tea.KeyEnter)), "retry should request preparation")
	if retryID == requestID {
		t.Fatal("Retry reused the failed request")
	}
	if !resumeWizard(dashboard).preparing || resumeWizard(dashboard).preparationError != nil {
		t.Fatal("Retry prepares again and clears the error")
	}

	preparation := movePreparation()
	if dashboard.ApplyMovePreparation(requestID, preparation) {
		t.Fatal("the failed request's late answer was applied")
	}
	if !dashboard.ApplyMovePreparation(retryID, preparation) {
		t.Fatal("the retry's answer applies")
	}
}

// Hard-won: #1372: a feed update could leave Move prepared for a discarded account.
func TestMoveRepreparesAfterFeedDisablesSelectedProfile(t *testing.T) {
	for _, alreadyPrepared := range []bool{false, true} {
		dashboard := dashboardWithSession(runningSession())
		requestID := openMoveReview(t, dashboard)
		stale := movePreparation()
		selected := resumeWizard(dashboard).destinationProfile(dashboard)
		if stale.Selection.ProfileID == nil || *stale.Selection.ProfileID != selected {
			t.Fatalf("the fixture prepares %v, the wizard selects %q", stale.Selection.ProfileID, selected)
		}
		if alreadyPrepared && !dashboard.ApplyMovePreparation(requestID, stale) {
			t.Fatal("the preparation applies")
		}

		configuration := *dashboard.config.Get()
		configuration.Profiles = maps.Clone(configuration.Profiles)
		profile := configuration.Profiles[selected]
		profile.Enabled = false
		configuration.Profiles[selected] = profile
		dashboard.SetRuntimeConfig(configuration, core.SharedSnapshot[client.RuntimeAvailability]{})
		if step := resumeWizard(dashboard).step; step != WizardStepProfile {
			t.Fatalf("step %v, want Profile", step)
		}
		replacement := resumeWizard(dashboard).destinationProfile(dashboard)
		if replacement == selected {
			t.Fatalf("the disabled profile %q is still the destination", selected)
		}

		wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionNone{})
		move, ok := readyKey(dashboard, key(tea.KeyEnter)).(DashboardActionMoveSession)
		if !ok || move.PreparationRequestID == nil {
			t.Fatal("the replacement destination must start a new preparation")
		}
		if move.ProfileID != replacement {
			t.Fatalf("profile %q, want %q", move.ProfileID, replacement)
		}
		if *move.PreparationRequestID == requestID {
			t.Fatal("the new preparation reused the stale request")
		}
		if dashboard.ApplyMovePreparation(requestID, stale) {
			t.Fatal("the stale preparation was applied")
		}
	}
}

func TestStaleMovePreparationIsIgnoredAfterCancelAndReopen(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	oldRequestID := openMoveReview(t, dashboard)
	preparation := movePreparation()

	readyKey(dashboard, key(tea.KeyEscape))
	wzInDashboard(t, dashboard)
	if dashboard.ApplyMovePreparation(oldRequestID, preparation) {
		t.Fatal("a cancelled move's preparation was applied")
	}
	if dashboard.SetMovePreparationFailed("session-1", oldRequestID, "late failure") {
		t.Fatal("a cancelled move's failure was applied")
	}

	newRequestID := openMoveReview(t, dashboard)
	if newRequestID == oldRequestID {
		t.Fatal("the reopened move reused the old request")
	}
	if dashboard.ApplyMovePreparation(oldRequestID, preparation) {
		t.Fatal("the old preparation was applied to the reopened move")
	}
	if !resumeWizard(dashboard).preparing {
		t.Fatal("the reopened move is still preparing")
	}
}

func TestStaleMovePreparationIsIgnoredAfterBackAndReenteringReview(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	oldRequestID := openMoveReview(t, dashboard)
	preparation := movePreparation()

	// Submit is disabled while loading; Tab reaches Back from the form's
	// fallback focus, past the delegation choice a Codex destination offers.
	// Returning to the target picker invalidates the request.
	for range 8 {
		if focused, ok := resumeWizard(dashboard).form.Focused(); ok && focused == WizardControlBack {
			break
		}
		readyKey(dashboard, key(tea.KeyTab))
	}
	wzFocusIs(t, &resumeWizard(dashboard).form, WizardControlBack)
	wzAction(t, readyKey(dashboard, key(tea.KeyEnter)), DashboardActionNone{})
	if step := resumeWizard(dashboard).step; step != WizardStepTarget {
		t.Fatalf("step %v, want Target", step)
	}
	if dashboard.ApplyMovePreparation(oldRequestID, preparation) {
		t.Fatal("the abandoned preparation was applied")
	}

	newRequestID := wzPreparationRequest(t, readyKey(dashboard, key(tea.KeyEnter)), "reentering review should request preparation")
	if newRequestID == oldRequestID {
		t.Fatal("reentering reused the old request")
	}
	if !resumeWizard(dashboard).preparing {
		t.Fatal("the review is preparing again")
	}
}

func TestQueueChoiceKeepsAReadyMoveConfirmationWhenOnlyPreparedQueueExists(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	requestID := openMoveReview(t, dashboard)
	preparation := movePreparation()
	preparation.QueuedCommands = []core.MaterializedQueuedPrompt{{
		CommandID:  "queued-1",
		Kind:       core.QueuedCommandKindPrompt{},
		Content:    []any{},
		QueuedAtMs: 1,
	}}
	if !dashboard.ApplyMovePreparation(requestID, preparation) {
		t.Fatal("the preparation applies")
	}

	// There is no queue in sessionDetails; the prepared queue still owns the
	// review checkbox and changing its disposition must preserve readiness.
	wzAction(t, readyKey(dashboard, key('q')), DashboardActionNone{})
	if resumeWizard(dashboard).discardQueue {
		t.Fatal("q keeps the queue")
	}
	if prepared := resumeWizard(dashboard).preparation; prepared == nil || !wzEqual(*prepared, preparation) {
		t.Fatal("the disposition change keeps the preparation")
	}

	move, ok := readyKey(dashboard, key(tea.KeyEnter)).(DashboardActionMoveSession)
	if !ok || move.PreparationRequestID != nil || move.Queue == nil || *move.Queue != core.ResumeQueueDispositionStart {
		t.Fatal("the ready review executes the move and starts the queue")
	}
}

func TestRemovingAMoveAttachmentInvalidatesAndRepreparesTheReview(t *testing.T) {
	session := runningSession()
	session.AdditionalMounts = []core.AdditionalMount{cacheMount()}
	dashboard := dashboardWithSession(session)
	oldRequestID := openMoveReview(t, dashboard)
	if !dashboard.ApplyMovePreparation(oldRequestID, movePreparation()) {
		t.Fatal("the preparation applies")
	}

	// Submit wraps to the attachment list; removing the selected directory
	// changes the move selection and immediately starts a fresh preparation.
	readyKey(dashboard, key(tea.KeyTab))
	readyKey(dashboard, key(tea.KeyTab))
	newRequestID := wzPreparationRequest(t, readyKey(dashboard, key(tea.KeyDelete)), "removing an attachment should reprepare")
	if newRequestID == oldRequestID {
		t.Fatal("the removal reused the old request")
	}
	wizard := resumeWizard(dashboard)
	if len(wizard.mounts.mounts) != 0 || wizard.preparation != nil || !wizard.preparing {
		t.Fatal("the removal empties the list and prepares again")
	}
}

// wzCapacityUsage is a probe of a host with totalMemory bytes and cores
// logical CPUs, nothing in use.
func wzCapacityUsage(totalMemory, cores uint64) *core.DeploymentCapacityUsage {
	return &core.DeploymentCapacityUsage{
		MemoryTotalBytes: totalMemory,
		LogicalCores:     cores,
		Storage:          []core.HostStorageSample{},
	}
}

func TestNewSessionDefaultsToTheLatestSizeOnItsHostAndClampsToCapacity(t *testing.T) {
	const gib = uint64(1024 * 1024 * 1024)
	state := core.NewStateState()
	state.ContainerSizes.Insert("local", core.HostContainerSize{Cpus: 24, MemoryBytes: 36 * gib})
	dashboard := wzNewDashboard(config(), state)
	dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{{
		ID:                "local",
		Host:              "local",
		TargetIDs:         []string{"podman"},
		Kind:              core.DeploymentCapacityKindHost,
		Local:             true,
		Probes:            []core.CommandSpec{},
		LocalStoragePaths: []string{},
	}})
	dashboard.applyDeploymentCapacity("local", wzCapacityUsage(48*gib, 12), nil, 0)
	setLocalContainerCapacity(dashboard,
		core.HostContainerSize{Cpus: 12, MemoryBytes: 48 * gib},
		core.HostContainerSize{Cpus: 0, MemoryBytes: 0},
		core.HostContainerSize{Cpus: 12, MemoryBytes: 48 * gib * 4 / 5},
		true)

	readyOpenNewWizard(dashboard)
	readyKey(dashboard, key(tea.KeyEnter))
	if allocation := newWizard(dashboard).resourceAllocation; !wzEqual(allocation,
		core.SessionResourceAllocation(core.SessionResourceAllocationContainer{Cpus: 12, MemoryBytes: 36 * gib})) {
		t.Fatalf("allocation %s", wzDebugAllocation(allocation))
	}
}

func wizardWithCapacity(template core.TargetTemplate, remaining core.HostContainerSize) *DashboardState {
	configuration := config()
	podman, ok := configuration.Targets["podman"].(core.TargetTemplateLocalPodman)
	if !ok {
		panic("podman fixture target")
	}
	switch template.(type) {
	case core.TargetTemplateLocalPodman:
		template = core.TargetTemplateLocalPodman{ContainerTemplate: podman.ContainerTemplate}
	case core.TargetTemplateAppleContainer:
		template = core.TargetTemplateAppleContainer{ContainerTemplate: podman.ContainerTemplate}
	default:
		panic("unexpected test target")
	}
	configuration.Targets["podman"] = template
	dashboard := wzNewDashboard(configuration, core.NewStateState())
	setContainerCapacityForTargets(dashboard, []string{"podman"},
		core.HostContainerSize{Cpus: 12, MemoryBytes: 64 << 30},
		core.HostContainerSize{Cpus: 10, MemoryBytes: 20 << 30},
		remaining, true)
	readyOpenNewWizard(dashboard)
	readyKey(dashboard, key(tea.KeyEnter))
	return dashboard
}

// wzAppleContainerTarget is an Apple container target built from the
// fixture's Podman container template.
func wzAppleContainerTarget() core.TargetTemplate {
	return core.TargetTemplateAppleContainer{ContainerTemplate: *config().Targets["podman"].Container()}
}

func TestNonAppleContainerOvercommitWarnsAndCanSubmit(t *testing.T) {
	dashboard := wizardWithCapacity(config().Targets["podman"], core.HostContainerSize{Cpus: 2, MemoryBytes: 2 << 30})
	wizard := newWizard(dashboard)
	if wizard.resourceAllocation == nil || wizard.sizingError != nil {
		t.Fatalf("allocation %s, error %v", wzDebugAllocation(wizard.resourceAllocation), wizard.sizingError)
	}
	if !targetAdvanceEnabled(dashboard, wizard) {
		t.Fatal("an overcommit only warns")
	}
	screen := strings.Join(capturedDrawn(dashboard, 120, 30), "\n")
	if !strings.Contains(screen, "Asks for more RAM than the") || !strings.Contains(screen, "it will still start.") {
		t.Fatalf("no overcommit warning:\n%s", screen)
	}
	wzNotContains(t, screen, "more CPUs")
	readyKey(dashboard, key(tea.KeyEnter))
	if step := newWizard(dashboard).step; step == WizardStepTarget {
		t.Fatal("the overcommitted size can be submitted")
	}
}

func TestAppleContainerCpuOvercommitDoesNotWarnAndCanSubmit(t *testing.T) {
	dashboard := wizardWithCapacity(wzAppleContainerTarget(), core.HostContainerSize{Cpus: 2, MemoryBytes: 48 << 30})
	wizard := newWizard(dashboard)
	if wizard.resourceAllocation == nil || wizard.sizingError != nil {
		t.Fatalf("allocation %s, error %v", wzDebugAllocation(wizard.resourceAllocation), wizard.sizingError)
	}
	if !targetAdvanceEnabled(dashboard, wizard) {
		t.Fatal("a CPU overcommit can advance")
	}
	screen := strings.Join(capturedDrawn(dashboard, 120, 30), "\n")
	wzNotContains(t, screen, "more CPUs")
	wzNotContains(t, screen, "more RAM than")
	readyKey(dashboard, key(tea.KeyEnter))
	if step := newWizard(dashboard).step; step == WizardStepTarget {
		t.Fatal("the CPU overcommit can be submitted")
	}
}

func TestAppleContainerRamOvercommitBlocksSubmission(t *testing.T) {
	dashboard := wizardWithCapacity(wzAppleContainerTarget(), core.HostContainerSize{Cpus: 12, MemoryBytes: 4 << 30})
	wizard := newWizard(dashboard)
	if wizard.resourceAllocation != nil {
		t.Fatalf("allocation %s, want none", wzDebugAllocation(wizard.resourceAllocation))
	}
	if wizard.sizingError == nil || !strings.Contains(*wizard.sizingError, "RAM is a hard limit on this host") {
		t.Fatalf("error %v", wizard.sizingError)
	}
	if targetAdvanceEnabled(dashboard, wizard) {
		t.Fatal("a RAM overcommit cannot advance")
	}
}

func TestNewContainerSizeCheckboxIsExplicitAndReachesTheCreateAction(t *testing.T) {
	dashboard := wzNewDashboard(config(), core.NewStateState())
	dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{testCapacityTarget()})
	dashboard.applyDeploymentCapacity("local", wzCapacityUsage(64<<30, 24), nil, 0)
	setLocalContainerCapacity(dashboard,
		core.HostContainerSize{Cpus: 24, MemoryBytes: 64 << 30},
		core.HostContainerSize{Cpus: 0, MemoryBytes: 0},
		core.HostContainerSize{Cpus: 24, MemoryBytes: (64 << 30) * 4 / 5},
		true)
	readyOpenNewWizard(dashboard)
	readyKey(dashboard, key(tea.KeyEnter))
	if allocation := newWizard(dashboard).resourceAllocation; !wzEqual(allocation,
		core.SessionResourceAllocation(core.SessionResourceAllocationContainer{Cpus: 4, MemoryBytes: 8 << 30})) {
		t.Fatalf("allocation %s", wzDebugAllocation(allocation))
	}
	if newWizard(dashboard).rememberContainerSize {
		t.Fatal("saving the size is opt-in")
	}
	screen := strings.Join(capturedDrawn(dashboard, 100, 30), "\n")
	wzContains(t, screen, "Save as default sizing")
	wzContains(t, screen, "RAM left")

	readyKey(dashboard, key(tea.KeyTab))
	readyKey(dashboard, key(tea.KeyTab))
	readyKey(dashboard, key(tea.KeyTab))
	wzFocusIs(t, &newWizard(dashboard).form, WizardControlRememberContainerSize)
	readyKey(dashboard, key(' '))
	if !newWizard(dashboard).rememberContainerSize {
		t.Fatal("Space checks the box")
	}

	action := dashboard.createSessionActionWithoutClosing(newWizard(dashboard))
	if create, ok := action.(DashboardActionCreateSession); !ok || !create.RememberContainerSize {
		t.Fatalf("action %s, want a creation that remembers the size", wzDebug(action))
	}
}

func TestUnknownLiveContainerCapacityWarnsInsteadOfBlockingNonAppleTargets(t *testing.T) {
	session := runningSession()
	session.Target = core.TargetLocatorLocalPodman{ContainerID: "unknown-size"}
	session.ResourceAllocation = nil
	state := core.NewStateState()
	state.Sessions.Insert(session.ID, session)
	dashboard := wzNewDashboard(config(), state)
	dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{testCapacityTarget()})
	dashboard.applyDeploymentCapacity("local", wzCapacityUsage(64<<30, 24), nil, 0)
	setLocalContainerCapacity(dashboard,
		core.HostContainerSize{Cpus: 24, MemoryBytes: 64 << 30},
		core.HostContainerSize{Cpus: 24, MemoryBytes: (64 << 30) * 4 / 5},
		core.HostContainerSize{Cpus: 0, MemoryBytes: 0},
		false)

	readyOpenNewWizard(dashboard)
	readyKey(dashboard, key(tea.KeyEnter))

	wizard := newWizard(dashboard)
	if wizard.resourceAllocation == nil || wizard.sizingError != nil {
		t.Fatalf("allocation %s, error %v", wzDebugAllocation(wizard.resourceAllocation), wizard.sizingError)
	}
	if !targetAdvanceEnabled(dashboard, wizard) {
		t.Fatal("unknown capacity only warns")
	}
	screen := strings.Join(capturedDrawn(dashboard, 100, 30), "\n")
	wzContains(t, screen, "RAM left unavailable")
	wzNotContains(t, screen, "RAM left: 0")
	wzNotContains(t, screen, "will still start")
}

// Hard-won: d27eb9f56: the TUI ignored daemon reservations while sizing a new session.
func TestTerminalHeadroomComesFromTheDaemonsReservationAdjustedCapacity(t *testing.T) {
	dashboard := wzNewDashboard(config(), core.NewStateState())
	dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{testCapacityTarget()})
	usage := wzCapacityUsage(64<<30, 10)
	usage.CPUPercent = wzPtr(uint8(20))
	usage.MemoryUsedBytes = 16 << 30
	dashboard.applyDeploymentCapacity("local", usage, nil, 0)
	setLocalContainerCapacity(dashboard,
		core.HostContainerSize{Cpus: 10, MemoryBytes: 64 << 30},
		core.HostContainerSize{Cpus: 8, MemoryBytes: 16 << 30},
		core.HostContainerSize{Cpus: 2, MemoryBytes: (64<<30)*4/5 - (16 << 30)},
		true)

	capacity := dashboard.hostContainerCapacity("podman")
	if capacity == nil {
		t.Fatal("the podman host has capacity")
	}
	if capacity.Remaining.Cpus != 2 {
		t.Fatalf("remaining CPUs %d, want 2", capacity.Remaining.Cpus)
	}
	readyOpenNewWizard(dashboard)
	readyKey(dashboard, key(tea.KeyEnter))
	screen := strings.Join(capturedDrawn(dashboard, 100, 30), "\n")
	wzContains(t, screen, "2 CPUs")
}

func TestResumeKeepsTheSessionsSizeInsteadOfTheHostsLatestSize(t *testing.T) {
	const gib = uint64(1024 * 1024 * 1024)
	session := stoppedSession()
	session.ResourceAllocation = core.SessionResourceAllocationContainer{Cpus: 4, MemoryBytes: 16 * gib}
	state := core.NewStateState()
	state.Sessions.Insert(session.ID, session)
	state.ContainerSizes.Insert("local", core.HostContainerSize{Cpus: 12, MemoryBytes: 48 * gib})
	dashboard := wzNewDashboard(config(), state)

	publishFixtureVerdicts(dashboard)
	dashboard.BeginResumeFor("session-1")
	readyKey(dashboard, key(tea.KeyEnter))
	mode, ok := dashboard.mode.(ModeResume)
	if !ok {
		t.Fatal("expected resume wizard on target step")
	}
	if !wzEqual(mode.resourceAllocation, core.SessionResourceAllocation(core.SessionResourceAllocationContainer{Cpus: 4, MemoryBytes: 16 * gib})) {
		t.Fatalf("allocation %s", wzDebugAllocation(mode.resourceAllocation))
	}
}

func TestResumeKeepsALegacyAllocationBelowTheNewSessionMinimum(t *testing.T) {
	session := stoppedSession()
	session.ResourceAllocation = core.SessionResourceAllocationContainer{Cpus: 1, MemoryBytes: 2 << 30}
	state := core.NewStateState()
	state.Sessions.Insert(session.ID, session)
	dashboard := wzNewDashboard(config(), state)

	publishFixtureVerdicts(dashboard)
	dashboard.BeginResumeFor("session-1")
	readyKey(dashboard, key(tea.KeyEnter))

	mode, ok := dashboard.mode.(ModeResume)
	if !ok {
		t.Fatal("expected resume wizard on target step")
	}
	if !wzEqual(mode.resourceAllocation, core.SessionResourceAllocation(core.SessionResourceAllocationContainer{Cpus: 1, MemoryBytes: 2 << 30})) {
		t.Fatalf("allocation %s", wzDebugAllocation(mode.resourceAllocation))
	}
	if mode.sizingError != nil {
		t.Fatalf("sizing error %q", *mode.sizingError)
	}
}

func TestCancellingAWizardInvalidatesChecksBeforeReopeningTheSameForm(t *testing.T) {
	dashboard := wzNewDashboard(config(), core.NewStateState())
	readyOpenNewWizard(dashboard)
	pending := dashboard.SessionPreflightGeneration()
	readyKey(dashboard, key(tea.KeyEscape))
	readyOpenNewWizard(dashboard)
	if _, ok := dashboard.mode.(ModeNew); !ok {
		t.Fatalf("mode %T, want the new-session wizard", dashboard.mode)
	}
	if pending == dashboard.SessionPreflightGeneration() {
		t.Fatal("cancelling must invalidate the pending checks")
	}
}

func TestHomeMountApplyUsesResolvedSourceAndIgnoresChangedDestination(t *testing.T) {
	dashboard := dashboardAtMountEditor("~/cache")
	if message, invalid := validateMountEntry(wizardMounts(dashboard)); invalid {
		t.Fatalf("the entry is valid: %s", message)
	}
	context := dashboard.PathInputContext()
	newWizard(dashboard).mounts.destination.SetValue("/mnt/newer")
	dashboard.ApplyResolvedMountSource(context, "~/cache", "/remote/cache", nil, nil)
	if mounts := wizardMounts(dashboard).mounts; len(mounts) != 0 {
		t.Fatalf("a resolution for a changed entry was applied: %+v", mounts)
	}
	context = dashboard.PathInputContext()
	dashboard.ApplyResolvedMountSource(context, "~/cache", "/remote/cache", nil, nil)
	mounts := wizardMounts(dashboard).mounts
	if len(mounts) == 0 || mounts[0].Source != "/remote/cache" || mounts[0].Destination != "/mnt/newer" {
		t.Fatalf("mounts %+v", mounts)
	}
}

func TestRevisitingOrReselectingATargetPreservesEditedResources(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.beginNew()
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyTab))
	replaceResourceText(dashboard, "4")
	readyKey(dashboard, wzBackTab())
	edited := newWizard(dashboard).resourceAllocation
	readyKey(dashboard, key(tea.KeyHome))
	wizard := newWizard(dashboard)
	if !wzEqual(wizard.resourceAllocation, edited) {
		t.Fatalf("allocation %s, want %s", wzDebugAllocation(wizard.resourceAllocation), wzDebugAllocation(edited))
	}
	wizard.form.Focus(WizardControlBack)
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	wizard = newWizard(dashboard)
	if wizard.step != WizardStepTarget {
		t.Fatalf("step %v, want Target", wizard.step)
	}
	if !wzEqual(wizard.resourceAllocation, edited) {
		t.Fatalf("allocation %s, want %s", wzDebugAllocation(wizard.resourceAllocation), wzDebugAllocation(edited))
	}
}

// wzLocalBareConfig is the fixture with its targets replaced by one bare
// local target, `local`.
func wzLocalBareConfig() core.Config {
	configuration := config()
	configuration.Targets = map[string]core.TargetTemplate{"local": core.TargetTemplateLocalBare{}}
	return configuration
}

func TestRawReviewWaitsForWorktreeInspectionAndPreservesExplicitSelection(t *testing.T) {
	dashboard := wzNewDashboard(wzLocalBareConfig(), core.NewStateState())
	dashboard.beginNew()
	wizard := newWizard(dashboard)
	wizard.step = WizardStepReview
	wizard.projectDirectory = chat.PathInputFromValue("/work/main")
	if check, _ := dashboard.TakePrerequisiteCheck(); !isValidateProjectDirectory(check) {
		t.Fatalf("check %s, want a project directory validation", wzDebug(check))
	}
	context := dashboard.PathInputContext()
	resolved := core.ManagedWorktreeOptions{Available: true, DefaultCreate: true}
	dashboard.ApplyResolvedProjectDirectory(context, "/work/main", "/work/main", resolved, false, nil)
	wizard = newWizard(dashboard)
	if !wizard.createManagedWorktree {
		t.Fatal("the inspection's default turns the managed worktree on")
	}
	wizard.form.Focus(WizardControlCreateManagedWorktree)
	readyKey(dashboard, key(' '))
	if newWizard(dashboard).createManagedWorktree {
		t.Fatal("Space turns the managed worktree off")
	}
	// Revalidating an unchanged directory keeps the explicit override.
	dashboard.ApplyResolvedProjectDirectory(context, "/work/main", "/work/main", resolved, false, nil)
	wizard = newWizard(dashboard)
	if wizard.createManagedWorktree {
		t.Fatal("revalidation overrode the explicit choice")
	}
	wizard.form.Focus(WizardControlSubmit)
	action := readyKey(dashboard, key(tea.KeyEnter))
	create, ok := action.(DashboardActionCreateSession)
	if !ok || create.CreateManagedWorktree == nil || *create.CreateManagedWorktree ||
		create.ProjectDirectory == nil || filepath.Clean(*create.ProjectDirectory) != filepath.Clean("/work/main") {
		t.Fatalf("action %s", wzDebug(action))
	}
}

func isValidateProjectDirectory(action DashboardAction) bool {
	_, ok := action.(DashboardActionValidateProjectDirectory)
	return ok
}

func TestWorktreeInspectionIgnoresOldDirectoriesAndAllowsLinkedCheckoutOptIn(t *testing.T) {
	dashboard := wzNewDashboard(wzLocalBareConfig(), core.NewStateState())
	dashboard.beginNew()
	wizard := newWizard(dashboard)
	wizard.step = WizardStepReview
	wizard.projectDirectory = chat.PathInputFromValue("/work/main")
	oldContext := dashboard.PathInputContext()
	newWizard(dashboard).projectDirectory = chat.PathInputFromValue("/work/linked")
	dashboard.ApplyResolvedProjectDirectory(oldContext, "/work/main", "/work/main",
		core.ManagedWorktreeOptions{Available: true, DefaultCreate: true}, false, nil)
	if options := newWizard(dashboard).worktreeOptions; options != nil {
		t.Fatalf("an old directory's inspection was applied: %+v", *options)
	}
	dashboard.ApplyResolvedProjectDirectory(dashboard.PathInputContext(), "/work/linked", "/work/linked",
		core.ManagedWorktreeOptions{Available: true, DefaultCreate: false}, false, nil)
	wizard = newWizard(dashboard)
	if wizard.createManagedWorktree {
		t.Fatal("a linked checkout defaults to no managed worktree")
	}
	wizard.form.Focus(WizardControlCreateManagedWorktree)
	readyKey(dashboard, key(' '))
	newWizard(dashboard).form.Focus(WizardControlSubmit)
	action := readyKey(dashboard, key(tea.KeyEnter))
	if create, ok := action.(DashboardActionCreateSession); !ok || create.CreateManagedWorktree == nil || !*create.CreateManagedWorktree {
		t.Fatalf("action %s, want a creation with a managed worktree", wzDebug(action))
	}
}

// An answer that a runtime is gone removes it from the open picker, and an
// answer that it is back returns it, with no transient checking UI.
func TestAvailabilityAnswersChangeTheOpenPickerOnlyWhenTheyDiffer(t *testing.T) {
	configuration := standardLocalTargetsConfig()
	clear(configuration.DefaultTargets)
	dashboard := wzNewDashboard(configuration, core.NewStateState())
	wzAction(t, dashboard.TakeAvailabilityCheck(), DashboardActionCheckAvailability{})
	openNewSessionWizard(dashboard)
	wzAction(t, dashboard.TakeAvailabilityCheck(), DashboardActionCheckAvailability{})
	dashboard.HandleKey(key(tea.KeyEnter))
	before := strings.Join(drawn(dashboard, 160, 40), "\n")
	wzContains(t, before, "docker")
	wzNotContains(t, before, "checking")

	answerAvailability(dashboard, func(dashboard *DashboardState) client.RuntimeAvailability {
		return availabilityAnswer(dashboard, []string{"docker"}, nil)
	})
	gone := strings.Join(drawn(dashboard, 160, 40), "\n")
	if !strings.Contains(gone, "docker: docker is not installed here.") {
		t.Fatalf("a configured runtime that is gone is named: %s", gone)
	}
	wzNotContains(t, gone, "unavailable")

	answerAvailability(dashboard, func(dashboard *DashboardState) client.RuntimeAvailability {
		return availabilityAnswer(dashboard, nil, nil)
	})
	back := strings.Join(drawn(dashboard, 160, 40), "\n")
	wzNotContains(t, back, "not installed")
	if !dashboard.targetOffered("docker") {
		t.Fatal("the returned runtime is offered again")
	}
}

// Hard-won: #1370: an availability answer left a stale profile index, closed a mid-launch wizard, and raced first-run setup.
func TestAvailabilityAnswersUpdateAWizardAndSpareALaunch(t *testing.T) {
	dashboard := wzNewDashboard(config(), core.NewStateState())
	dashboard.HoldAvailabilityCheck()
	if check := dashboard.TakeAvailabilityCheck(); check != nil {
		t.Fatalf("nothing is checked while first-run setup decides: %s", wzDebug(check))
	}
	dashboard.ReleaseAvailabilityCheck()
	wzAction(t, dashboard.TakeAvailabilityCheck(), DashboardActionCheckAvailability{}, "the startup check starts once setup is done")
	openNewSessionWizard(dashboard)
	offered := 0
	for range dashboard.offeredProfiles() {
		offered++
	}
	last := offered - 1
	chosen, ok := dashboard.nthOfferedProfile(last)
	if !ok {
		t.Fatal("the last offered profile exists")
	}
	newWizard(dashboard).profile = last
	first, ok := dashboard.nthOfferedProfile(0)
	if !ok {
		t.Fatal("the first offered profile exists")
	}
	answerAvailability(dashboard, func(dashboard *DashboardState) client.RuntimeAvailability {
		answer := availabilityAnswer(dashboard, nil, nil)
		answer.Profiles[first] = client.AvailabilityVerdict{Kind: client.AvailabilityVerdictMissing, Reason: "gone"}
		return answer
	})
	if profile, ok := dashboard.nthOfferedProfile(newWizard(dashboard).profile); !ok || profile != chosen {
		t.Fatalf("the wizard keeps its choice: %q (%t), want %q", profile, ok, chosen)
	}
	drawn(dashboard, 140, 40)

	newWizard(dashboard).step = WizardStepLaunching
	answerAvailability(dashboard, func(dashboard *DashboardState) client.RuntimeAvailability {
		answer := availabilityAnswer(dashboard, nil, nil)
		for id := range answer.Profiles {
			answer.Profiles[id] = client.AvailabilityVerdict{Kind: client.AvailabilityVerdictMissing, Reason: "gone"}
		}
		return answer
	})
	if step := newWizard(dashboard).step; step != WizardStepLaunching {
		t.Fatalf("a submitted launch is left to its own preflight: step %v", step)
	}
	drawn(dashboard, 140, 40)
}

// A runtime the wizard already chose that turns out to be gone sends the
// wizard back to the target step with the reason, rather than launching on
// it or silently swapping in another.
func TestAChosenRuntimeThatDisappearsReturnsTheWizardToItsStep(t *testing.T) {
	configuration := standardLocalTargetsConfig()
	clear(configuration.DefaultTargets)
	dashboard := wzNewDashboard(configuration, core.NewStateState())

	openNewSessionWizard(dashboard)
	docker := slices.Index(slices.Sorted(maps.Keys(dashboard.config.Get().Targets)), "docker")
	if docker < 0 {
		t.Fatal("the fixture has docker")
	}
	wizard := newWizard(dashboard)
	wizard.target = docker
	wizard.step = WizardStepBundle
	answerAvailability(dashboard, func(dashboard *DashboardState) client.RuntimeAvailability {
		return availabilityAnswer(dashboard, []string{"docker"}, nil)
	})
	wizard = newWizard(dashboard)
	if wizard.step != WizardStepTarget {
		t.Fatalf("step %v, want Target", wizard.step)
	}
	if target := nthKey(dashboard.config.Get().Targets, wizard.target); target == "docker" {
		t.Fatal("the gone runtime is still selected")
	}
	if notice, ok := dashboard.Notice(); !ok || !strings.Contains(notice, "docker is no longer available") {
		t.Fatalf("notice %q (%t)", notice, ok)
	}
}

func replaceResourceText(dashboard *DashboardState, text string) {
	readyKey(dashboard, key(tea.KeyEnd))
	readyKey(dashboard, tea.KeyPressMsg{Code: 'u', Mod: tea.ModCtrl})
	typeSource(dashboard, text)
}

// wzStateWith is State::default holding sessions.
func wzStateWith(sessions ...core.SessionRecord) core.StateState {
	state := core.NewStateState()
	for _, session := range sessions {
		state.Sessions.Insert(session.ID, session)
	}
	return state
}

// wzNthEnabledProfile is the id of the index-th enabled profile.
func wzNthEnabledProfile(configuration *core.Config, index int) string {
	position := 0
	for id := range configuration.EnabledProfiles() {
		if position == index {
			return id
		}
		position++
	}
	panic(fmt.Sprintf("no enabled profile %d", index))
}

func newSessionPrefersInteractiveChoicesOverNewerUnattendedSessions(t *testing.T) {
	configuration := config()
	configuration.Targets["docker"] = configuration.Targets["podman"]
	configuration.Bundles["interactive-project"] = configuration.Bundles["hel"]
	latest := stoppedSession()
	dashboard := wzNewDashboard(configuration, wzStateWith(latest))
	dashboard.SetRecentLaunch(&core.RecentLaunch{
		ProfileID: "claude-1",
		TargetID:  "docker",
		BundleID:  "interactive-project",
	})
	// Project ordering still follows session history, independently of
	// selection.
	if first := bundleIDsByRecentCreation(dashboard.config.Get(), dashboard.launchRecency)[0]; first != "hel" {
		t.Fatalf("first project %q, want hel", first)
	}
	openNewSessionWizard(dashboard)
	wizard := newWizard(dashboard)
	if profile := wzNthEnabledProfile(dashboard.config.Get(), wizard.profile); profile != "claude-1" {
		t.Fatalf("profile %q, want claude-1", profile)
	}
	if target := nthKey(dashboard.config.Get().Targets, wizard.target); target != "docker" {
		t.Fatalf("target %q, want docker", target)
	}
	if bundle := nthBundleKey(dashboard.config.Get(), dashboard.launchRecency, wizard.bundle); bundle != "interactive-project" {
		t.Fatalf("project %q, want interactive-project", bundle)
	}
	capturedDrawn(dashboard, 140, 40)
	dashboard.HandleKey(key(tea.KeyEnter))
	capturedDrawn(dashboard, 140, 40)
}

// wzAddMachine adds a machine to configuration.
func wzAddMachine(configuration *core.Config, id string, machine core.Machine) {
	if configuration.Machines == nil {
		configuration.Machines = map[string]core.Machine{}
	}
	configuration.Machines[id] = machine
}

// aMachineWithoutARuntimeKeepsTheTargetStepAndSaysWhy: a machine no runtime
// runs on has no row, so the target step is kept to name it and say why,
// even when one target is all it offers.
func aMachineWithoutARuntimeKeepsTheTargetStepAndSaysWhy(t *testing.T) {
	configuration := standardLocalTargetsConfig()
	wzAddMachine(&configuration, "nas", core.MachineSsh{
		SSH: core.SshConnection{
			Host:      "nas.example.test",
			User:      wzPtr("dev"),
			ExtraArgs: []string{},
		},
		WorkspacePrefix: core.DefaultWorkspacePrefix,
	})
	dashboard := wzNewDashboard(configuration, core.NewStateState())
	openNewSessionWizard(dashboard)
	answerAvailability(dashboard, func(dashboard *DashboardState) client.RuntimeAvailability {
		return availabilityAnswer(dashboard, []string{"podman", "docker"}, []string{"nas"})
	})
	dashboard.HandleKey(key(tea.KeyEnter))
	wizard := newWizard(dashboard)
	if wizard.step != WizardStepTarget {
		t.Fatalf("step %v, want Target", wizard.step)
	}
	if wizard.targetStepSkipped {
		t.Fatal("the target step is kept")
	}
	target := strings.Join(capturedDrawn(dashboard, 140, 40), "\n")
	if !strings.Contains(target, "nas did not answer.") || strings.Contains(target, "Check that") {
		t.Fatalf("the unanswered machine is named without remediation:\n%s", target)
	}
}

// Hard-won: 2c467ecc9: shortened picker reasons dropped the recovery steps from blocking notices
func TestUnreachableOnlyTargetBlockingNoticeKeepsRemediation(t *testing.T) {
	configuration := config()
	configuration.Targets = map[string]core.TargetTemplate{"machine": bareSshTarget()}
	configuration.DefaultTargets = map[string]struct{}{"machine": {}}
	wzAddMachine(&configuration, "machine", bareSshMachine())
	dashboard := wzNewDashboard(configuration, core.NewStateState())
	openNewSessionWizard(dashboard)
	answerAvailability(dashboard, func(dashboard *DashboardState) client.RuntimeAvailability {
		return availabilityAnswer(dashboard, nil, []string{"machine"})
	})
	dashboard.beginNewWizard()
	notice, _ := dashboard.notices.Current()
	wzContains(t, notice, "Check that machine is up.")
}

// newSessionSkipsTheTargetStepWhenOnlyOneTargetIsOffered: on a host without
// Podman or Docker, localhost is the only target the target step offers, so
// the wizard chooses it and goes from the profile step to the project step.
// The titles count three steps, Back skips the hidden step, and the review
// names the target.
func newSessionSkipsTheTargetStepWhenOnlyOneTargetIsOffered(t *testing.T) {
	// The last session ran on podman, so the draft starts there.
	dashboard := wzNewDashboard(standardLocalTargetsConfig(), wzStateWith(stoppedSession()))
	openNewSessionWizard(dashboard)
	if target := nthKey(dashboard.config.Get().Targets, newWizard(dashboard).target); target != "podman" {
		t.Fatalf("target %q, want podman", target)
	}
	answerTargetChecks(dashboard, []string{"podman", "docker"})
	profile := strings.Join(capturedDrawn(dashboard, 140, 40), "\n")
	wzContains(t, profile, "New session · 1/3 profile")

	dashboard.HandleKey(key(tea.KeyEnter))
	wizard := newWizard(dashboard)
	if wizard.step != WizardStepProjectDirectory || !wizard.targetStepSkipped {
		t.Fatalf("step %v (target step skipped %t), want the project step", wizard.step, wizard.targetStepSkipped)
	}
	if target := nthKey(dashboard.config.Get().Targets, wizard.target); target != "localhost" {
		t.Fatalf("target %q, want localhost", target)
	}
	project := strings.Join(capturedDrawn(dashboard, 140, 40), "\n")
	wzContains(t, project, "New session · 2/3 local project")

	pressBack(t, dashboard)
	if step := newWizard(dashboard).step; step != WizardStepProfile {
		t.Fatalf("step %v, want Profile", step)
	}

	dashboard.HandleKey(key(tea.KeyEnter))
	if step := newWizard(dashboard).step; step != WizardStepProjectDirectory {
		t.Fatalf("step %v, want ProjectDirectory", step)
	}
	dashboard.HandlePaste("/work/project")
	action := dashboard.HandleKey(key(tea.KeyEnter))
	captureAction(action)
	wzAction(t, action, DashboardActionValidateProjectDirectory{
		TargetTemplateID: "localhost",
		Directory:        "/work/project",
	})
	// A folder without Git commits while review is on: the review page says
	// the session will be created with review off.
	dashboard.ApplyResolvedProjectDirectory(dashboard.PathInputContext(), "/work/project", "/work/project",
		core.ManagedWorktreeOptions{}, true, nil)
	if step := newWizard(dashboard).step; step != WizardStepReview {
		t.Fatalf("step %v, want Review", step)
	}
	review := strings.Join(capturedDrawn(dashboard, 140, 40), "\n")
	wzContains(t, review, "New session · 3/3 review")
	wzContains(t, review, "Target: localhost")

	pressBack(t, dashboard)
	if step := newWizard(dashboard).step; step != WizardStepProjectDirectory {
		t.Fatalf("step %v, want ProjectDirectory", step)
	}
	pressBack(t, dashboard)
	if step := newWizard(dashboard).step; step != WizardStepProfile {
		t.Fatalf("step %v, want Profile", step)
	}
}

// newSessionShowsTheTargetStepWhenASecondTargetMayBeChosen: a second target
// the step could offer keeps the step: one that is there, and one no
// availability answer has reached yet.
func newSessionShowsTheTargetStepWhenASecondTargetMayBeChosen(t *testing.T) {
	for _, podmanReady := range []bool{true, false} {
		dashboard := wzNewDashboard(standardLocalTargetsConfig(), core.NewStateState())
		openNewSessionWizard(dashboard)
		// A check that has not answered leaves every target offered.
		if podmanReady {
			answerTargetChecks(dashboard, []string{"docker"})
		}
		profile := strings.Join(capturedDrawn(dashboard, 140, 40), "\n")
		wzContains(t, profile, "New session · 1/4 profile")

		dashboard.HandleKey(key(tea.KeyEnter))
		wizard := newWizard(dashboard)
		if wizard.step != WizardStepTarget {
			t.Fatalf("podman ready: %t: step %v, want Target", podmanReady, wizard.step)
		}
		if wizard.targetStepSkipped {
			t.Fatal("the target step is shown")
		}
		target := strings.Join(capturedDrawn(dashboard, 140, 40), "\n")
		wzContains(t, target, "New session · 2/4 target")
	}
}

// aLoneContainerTargetKeepsItsStepForSizing: a container target is sized on
// the target step, so even as the only target its step is shown.
func aLoneContainerTargetKeepsItsStepForSizing(t *testing.T) {
	dashboard := wzNewDashboard(config(), core.NewStateState())
	openNewSessionWizard(dashboard)
	answerTargetChecks(dashboard, nil)

	dashboard.HandleKey(key(tea.KeyEnter))

	if step := newWizard(dashboard).step; step != WizardStepTarget {
		t.Fatalf("step %v, want Target", step)
	}
	target := strings.Join(capturedDrawn(dashboard, 140, 40), "\n")
	wzContains(t, target, "New session · 2/4 target")
}

// dashboardWithRawSshSession is a stopped session that opens a directory on
// an SSH host, with that host's bare target added to the standard local
// ones.
func dashboardWithRawSshSession(state core.SessionState) *DashboardState {
	session := stoppedSession()
	session.State = state
	session.TargetTemplateID = "machine"
	session.ProjectDirectory = wzPtr("/srv/project")
	dashboard := dashboardWithSession(session)
	dashboard.config.Set(standardLocalTargetsConfig())
	dashboard.config.Mut().Targets["machine"] = bareSshTarget()
	return dashboard
}

// resumeSkipsTheTargetStepWhenOnlyOneTargetSuitsTheSession: a session that
// opens a directory on an SSH host can only resume on a bare target there.
// Every other target is ready, but it cannot use them, so the wizard
// chooses the SSH target without showing the step.
func resumeSkipsTheTargetStepWhenOnlyOneTargetSuitsTheSession(t *testing.T) {
	dashboard := dashboardWithRawSshSession(core.SessionStateStopped)
	action := dashboard.BeginResumeFor("session-1")
	captureAction(action)
	wzAction(t, action, DashboardActionNone{})
	answerTargetChecks(dashboard, nil)
	profile := strings.Join(capturedDrawn(dashboard, 140, 40), "\n")
	wzContains(t, profile, "Resume · 1/2 profile")

	dashboard.HandleKey(key(tea.KeyEnter))
	wizard := resumeWizard(dashboard)
	if wizard.step != WizardStepReview || !wizard.targetStepSkipped {
		t.Fatalf("step %v (target step skipped %t), want the review", wizard.step, wizard.targetStepSkipped)
	}
	if target := nthKey(dashboard.config.Get().Targets, wizard.target); target != "machine" {
		t.Fatalf("target %q, want machine", target)
	}
	review := strings.Join(capturedDrawn(dashboard, 140, 40), "\n")
	wzContains(t, review, "Resume · 2/2 review")
	wzContains(t, review, "Target: machine")

	pressBack(t, dashboard)
	if step := resumeWizard(dashboard).step; step != WizardStepProfile {
		t.Fatalf("step %v, want Profile", step)
	}

	dashboard.HandleKey(key(tea.KeyEnter))
	if step := resumeWizard(dashboard).step; step != WizardStepReview {
		t.Fatalf("step %v, want Review", step)
	}
	action = dashboard.HandleKey(key(tea.KeyEnter))
	captureAction(action)
	preflight, ok := action.(DashboardActionPreflightResumeRepositories)
	if !ok {
		t.Fatal("Resume on the review starts the resume")
	}
	if launch, ok := preflight.Launch.(DashboardActionResumeSession); !ok || launch.TargetTemplateID != "machine" {
		t.Fatalf("launch %s, want a resume on machine", wzDebug(preflight.Launch))
	}
}

// resumeShowsTheTargetStepWhenTwoTargetsSuitTheSession: a checkout on this
// machine can resume in place or move into a container, so a ready podman
// is a second choice and the step is shown.
func resumeShowsTheTargetStepWhenTwoTargetsSuitTheSession(t *testing.T) {
	session := stoppedSession()
	session.TargetTemplateID = "localhost"
	session.ProjectDirectory = wzPtr("/work/project")
	dashboard := dashboardWithSession(session)
	dashboard.config.Set(standardLocalTargetsConfig())
	action := dashboard.BeginResumeFor("session-1")
	captureAction(action)
	wzAction(t, action, DashboardActionNone{})
	answerTargetChecks(dashboard, []string{"docker"})
	profile := strings.Join(capturedDrawn(dashboard, 140, 40), "\n")
	wzContains(t, profile, "Resume · 1/3 profile")

	dashboard.HandleKey(key(tea.KeyEnter))

	wizard := resumeWizard(dashboard)
	if wizard.step != WizardStepTarget || wizard.targetStepSkipped {
		t.Fatalf("step %v (target step skipped %t), want the target step", wizard.step, wizard.targetStepSkipped)
	}
	target := strings.Join(capturedDrawn(dashboard, 140, 40), "\n")
	wzContains(t, target, "Resume · 2/3 new target")
}

// moveSkipsTheTargetStepWhenOnlyOneTargetSuitsTheSession: Move uses the
// resume wizard's rule: a live session on an SSH host can only move to a
// bare target there, so the step is skipped and the move is prepared for
// that target at once.
func moveSkipsTheTargetStepWhenOnlyOneTargetSuitsTheSession(t *testing.T) {
	dashboard := dashboardWithRawSshSession(core.SessionStateRunning)
	dashboard.FocusSessions()
	action := dashboard.beginMove()
	captureAction(action)
	wzAction(t, action, DashboardActionNone{})
	answerTargetChecks(dashboard, nil)
	profile := strings.Join(capturedDrawn(dashboard, 140, 40), "\n")
	wzContains(t, profile, "Move · 1/2 profile")

	preparation := dashboard.HandleKey(key(tea.KeyEnter))
	captureAction(preparation)
	if move, ok := preparation.(DashboardActionMoveSession); !ok || move.TargetTemplateID != "machine" || move.PreparationRequestID == nil {
		t.Fatalf("preparation %s, want a prepared move to machine", wzDebug(preparation))
	}
	wizard := resumeWizard(dashboard)
	if wizard.step != WizardStepReview || !wizard.targetStepSkipped {
		t.Fatalf("step %v (target step skipped %t), want the review", wizard.step, wizard.targetStepSkipped)
	}
	review := strings.Join(capturedDrawn(dashboard, 140, 40), "\n")
	wzContains(t, review, "Move · 2/2 confirm")
	wzContains(t, review, "Target: machine")

	pressBack(t, dashboard)
	wizard = resumeWizard(dashboard)
	if wizard.step != WizardStepProfile {
		t.Fatalf("step %v, want Profile", wizard.step)
	}
	if wizard.preparing {
		t.Fatal("leaving the review drops its preparation")
	}
}

// moveShowsTheTargetStepWhenTwoTargetsSuitTheSession: a live checkout on
// this machine can also move into a ready podman, so the Move wizard shows
// its target step.
func moveShowsTheTargetStepWhenTwoTargetsSuitTheSession(t *testing.T) {
	session := runningSession()
	session.TargetTemplateID = "localhost"
	session.ProjectDirectory = wzPtr("/work/project")
	dashboard := dashboardWithSession(session)
	dashboard.config.Set(standardLocalTargetsConfig())
	dashboard.FocusSessions()
	action := dashboard.beginMove()
	captureAction(action)
	wzAction(t, action, DashboardActionNone{})
	answerTargetChecks(dashboard, []string{"docker"})
	profile := strings.Join(capturedDrawn(dashboard, 140, 40), "\n")
	wzContains(t, profile, "Move · 1/3 profile")

	wzAction(t, dashboard.HandleKey(key(tea.KeyEnter)), DashboardActionNone{})

	wizard := resumeWizard(dashboard)
	if wizard.step != WizardStepTarget || wizard.targetStepSkipped {
		t.Fatalf("step %v (target step skipped %t), want the target step", wizard.step, wizard.targetStepSkipped)
	}
	target := strings.Join(capturedDrawn(dashboard, 140, 40), "\n")
	wzContains(t, target, "Move · 2/3 new target")
}

// answerChecksWithoutDocker answers the availability check the way the
// dashboard loop does on a host with no Docker: `docker` was never
// installed, `machine` did not answer, and everything else is there.
func answerChecksWithoutDocker(dashboard *DashboardState) {
	answerAvailability(dashboard, func(dashboard *DashboardState) client.RuntimeAvailability {
		return availabilityAnswer(dashboard, []string{"docker"}, []string{"machine"})
	})
}

// bareSshMachine is the machine bareSshTarget runs on.
func bareSshMachine() core.Machine {
	target, ok := bareSshTarget().(core.TargetTemplateSshBare)
	if !ok {
		panic("a bare SSH target")
	}
	return core.MachineSsh{SSH: target.SSH, WorkspacePrefix: target.WorkspacePrefix}
}

// The user's rule: the pickers list only what is there. A standard runtime
// whose engine was never installed is left out without a word; a runtime
// whose host did not answer is left out and named under the list with the
// reason. The step shows the same rows for New, Resume and Move.
// Hard-won: 2c467ecc9: an SSH auth or host-key failure was muted as "did not answer" in the target picker
func TestTargetStepsListOnlyWhatIsThereAndNameAnUnresponsiveHost(t *testing.T) {
	for _, flow := range []string{"New session", "Resume", "Move"} {
		session := stoppedSession()
		if flow == "Move" {
			session = runningSession()
		}
		session.TargetTemplateID = "localhost"
		session.ProjectDirectory = wzPtr("/work/project")
		dashboard := dashboardWithSession(session)
		dashboard.config.Set(standardLocalTargetsConfig())
		dashboard.config.Mut().Targets["machine"] = bareSshTarget()
		wzAddMachine(dashboard.config.Mut(), "machine", bareSshMachine())
		// A second runtime on the same machine shares its one line.
		ssh := bareSshTarget().(core.TargetTemplateSshBare).SSH
		container := *dashboard.config.Get().Targets["podman"].Container()
		dashboard.config.Mut().Targets["machine-podman"] = core.TargetTemplateSshPodman{SSH: ssh, ContainerTemplate: container}
		switch flow {
		case "New session":
			openNewSessionWizard(dashboard)
		case "Resume":
			wzAction(t, dashboard.BeginResumeFor("session-1"), DashboardActionNone{})
		default:
			dashboard.FocusSessions()
			wzAction(t, dashboard.beginMove(), DashboardActionNone{})
		}
		answerChecksWithoutDocker(dashboard)
		dashboard.HandleKey(key(tea.KeyEnter))
		text := strings.Join(capturedDrawn(dashboard, 160, 40), "\n")
		wzContains(t, text, "target")
		wzContains(t, text, "podman")
		if !strings.Contains(text, "machine did not answer; not listing machine, machine-podman.") ||
			strings.Contains(text, "unavailable") ||
			strings.Contains(text, "Connection timed out") ||
			strings.Contains(text, "Check that") {
			t.Fatalf("%s: an unresponsive host is named, not listed: %s", flow, text)
		}
		if count := strings.Count(text, "machine did not answer"); count != 1 {
			t.Fatalf("%s: the machine is named once, not %d times: %s", flow, count, text)
		}
		if strings.Contains(text, "docker") {
			t.Fatalf("%s: a missing runtime is not listed: %s", flow, text)
		}
		if strings.Contains(text, "Docker is not installed") {
			t.Fatalf("%s: nor is its reason: %s", flow, text)
		}

		dashboard.wantAvailabilityCheck()
		answerAvailability(dashboard, func(dashboard *DashboardState) client.RuntimeAvailability {
			answer := availabilityAnswer(dashboard, []string{"docker"}, nil)
			publishUnreachableMachine(dashboard, &answer, "machine",
				"Permission denied (publickey).",
				"Install your public key with `ssh-copy-id dev@machine`.",
				false)
			return answer
		})
		lines := dashboard.missingTargetLines()
		if len(lines) != 1 || lines[0].Routine {
			t.Fatalf("%s: %+v", flow, lines)
		}
		wzContains(t, lines[0].Text, "SSH to machine failed; not listing machine, machine-podman.")
		wzContains(t, lines[0].Full, "Permission denied")
		missing, ok := dashboard.targetMissingLine("machine")
		if !ok {
			t.Fatal("machine has a missing line")
		}
		wzNotContains(t, missing, "did not answer")
		text = strings.Join(capturedDrawn(dashboard, 160, 40), "\n")
		wzContains(t, text, "podman")
		wzContains(t, text, "Permission denied")
		wzContains(t, text, "ssh-copy-id dev@machine")
		wzNotContains(t, text, "did not answer")
	}
}

// selectingARowSkipsTheHiddenTarget: moving the selection lands on the next
// listed target, not the next config entry, because the hidden target
// shifts the rows.
func selectingARowSkipsTheHiddenTarget(t *testing.T) {
	dashboard := wzNewDashboard(standardLocalTargetsConfig(), core.NewStateState())
	openNewSessionWizard(dashboard)
	answerChecksWithoutDocker(dashboard)
	dashboard.HandleKey(key(tea.KeyEnter))
	// Config order is docker, localhost, podman; the rows are the last two.
	position := func(id string) int {
		index := slices.Index(slices.Sorted(maps.Keys(dashboard.config.Get().Targets)), id)
		if index < 0 {
			panic(fmt.Sprintf("no target %q", id))
		}
		return index
	}
	localhost := position("localhost")
	podman := position("podman")
	newWizard(dashboard).target = localhost
	capturedDrawn(dashboard, 160, 40)
	dashboard.HandleKey(key(tea.KeyDown))
	if target := newWizard(dashboard).target; target != podman {
		t.Fatalf("target %d, want podman (%d)", target, podman)
	}
	captureDetail("selection", "podman (the next visible row; docker is hidden)")
	capturedDrawn(dashboard, 160, 40)
}

func wizardsCtrlSpace() tea.KeyPressMsg {
	return tea.KeyPressMsg{Code: ' ', Mod: tea.ModCtrl}
}

// A bare SSH target owns the project directory, so its completions come
// from the target's own machine rather than the controller.
func TestProjectDirectoryCompletesOnTheTargetHost(t *testing.T) {
	configuration := config()
	configuration.Targets = map[string]core.TargetTemplate{"remote": core.TargetTemplateSshBare{
		SSH:             core.SshConnection{Host: "example.test", ExtraArgs: []string{}},
		Permissions:     core.PermissionModeGuardian,
		WorkspacePrefix: "workspaces",
	}}
	dashboard := wzNewDashboard(configuration, core.NewStateState())
	readyOpenNewWizard(dashboard)
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	if mode, ok := dashboard.mode.(ModeNew); !ok || mode.step != WizardStepProjectDirectory {
		t.Fatal("expected the project directory step")
	}
	typeSource(dashboard, "/srv/p")

	wzAction(t, readyKey(dashboard, wizardsCtrlSpace()), DashboardActionCompletePath{
		Host:   core.CompletionHostTarget{ID: "remote"},
		Kind:   core.CompletionKindDirectories,
		Prefix: "/srv/p",
	})
	dashboard.ApplyPathCompletions(dashboard.PathInputContext(), "/srv/p", core.PathCompletion{
		Candidates: []string{"/srv/project/", "/srv/proxy/"},
		Insert:     wzPtr("/srv/pro"),
	})
	mode, ok := dashboard.mode.(ModeNew)
	if !ok {
		t.Fatal("expected the project directory step")
	}
	if directory := mode.projectDirectory.String(); directory != "/srv/pro" {
		t.Fatalf("directory %q, want /srv/pro", directory)
	}
	if !mode.projectDirectory.IsCompleting() {
		t.Fatal("two candidates open the completion list")
	}

	readyKey(dashboard, key(tea.KeyDown))
	readyKey(dashboard, key(tea.KeyEnter))
	if directory := newWizard(dashboard).projectDirectory.String(); directory != "/srv/proxy/" {
		t.Fatalf("directory %q, want /srv/proxy/", directory)
	}
}

// A reply that arrives after another keystroke belongs to text the field no
// longer holds, so it is dropped without touching what was typed.
func TestStaleCompletionReplyIsDropped(t *testing.T) {
	dashboard := dashboardAtMountEditor("/opt")
	newWizard(dashboard).form.Focus(WizardControlMountSource)
	if _, ok := readyKey(dashboard, wizardsCtrlSpace()).(DashboardActionCompletePath); !ok {
		t.Fatal("Ctrl+Space asks for completions")
	}
	context := dashboard.PathInputContext()
	readyKey(dashboard, key('e'))

	dashboard.ApplyPathCompletions(context, "/opt", core.PathCompletion{
		Candidates: []string{"/opt/one/", "/opt/two/"},
		Insert:     wzPtr("/opt/"),
	})
	wizard := newWizard(dashboard)
	if source := wizard.mounts.source.String(); source != "/opte" {
		t.Fatalf("source %q, want /opte", source)
	}
	if wizard.mounts.source.IsCompleting() {
		t.Fatal("the stale reply must not open completions")
	}
}

// dashboardAtLocalProjectStep is a dashboard on the local project step of
// the New wizard, with history already remembered for this host.
func dashboardAtLocalProjectStep(history ...string) *DashboardState {
	configuration := config()
	configuration.Targets = map[string]core.TargetTemplate{"localhost": core.TargetTemplateLocalBare{}}
	state := core.NewStateState()
	for _, directory := range history {
		state.RememberProjectDirectory("local", directory)
	}
	dashboard := wzNewDashboard(configuration, state)
	readyOpenNewWizard(dashboard)
	// localhost is the only target, so its step is passed.
	readyKey(dashboard, key(tea.KeyEnter))
	if mode, ok := dashboard.mode.(ModeNew); !ok || mode.step != WizardStepProjectDirectory {
		panic("expected the local project step")
	}
	return dashboard
}

// wizardsRowOf is the row of lines holding label, panicking with the screen
// when it is nowhere on it.
func wizardsRowOf(lines []string, label string) int {
	row := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, label) })
	if row < 0 {
		panic(fmt.Sprintf("missing %q: %q", label, lines))
	}
	return row
}

// `cd ~/demo && mj`: the local project step starts with the repository `mj`
// was started in, ahead of any remembered directory (J-10).
// Hard-won: a16ff9c6: the prefilled repository path was hidden, so Enter launched there without notice
func TestTheLocalProjectStartsAsTheRepositoryMjWasStartedIn(t *testing.T) {
	configuration := config()
	configuration.Targets = map[string]core.TargetTemplate{"localhost": core.TargetTemplateLocalBare{}}
	state := core.NewStateState()
	state.RememberProjectDirectory("local", "/work/remembered")
	dashboard := wzNewDashboard(configuration, state)
	dashboard.SetLaunchProjectDirectory(wzPtr("/home/me/demo"))
	readyOpenNewWizard(dashboard)
	// localhost is the only target, so its step is passed.
	readyKey(dashboard, key(tea.KeyEnter))
	wizard := newWizard(dashboard)
	if wizard.step != WizardStepProjectDirectory {
		t.Fatalf("step %v, want ProjectDirectory", wizard.step)
	}
	if directory := wizard.projectDirectory.String(); directory != "/home/me/demo" {
		t.Fatalf("directory %q, want /home/me/demo", directory)
	}
	// Launch finding R1-1: the step says where the path came from, so a
	// plain Enter is a visible choice of that repository.
	draw := func() string {
		terminal := newWizardsTerminal(140, 36)
		terminal.draw(dashboard)
		return strings.Join(terminal.lines(), "\n")
	}
	wzContains(t, draw(), "Filled in with the repository mj was started in")
	readyKey(dashboard, key(tea.KeyEnter))
	dashboard.ApplyProjectDirectoryValidation("/home/me/demo", nil)
	if mode, ok := dashboard.mode.(ModeNew); !ok || mode.step != WizardStepReview {
		t.Fatal("the validated directory goes to the review")
	}
	wzContains(t, draw(), "/home/me/demo  (the repository mj was started in)")
}

// The step pre-fills a remembered directory, so it has to draw it with the
// caret at its end. A value the field holds but does not show takes every
// character typed after it and turns the path into two paths.
func TestARememberedProjectDirectoryIsDrawnWithTheCaretAtItsEnd(t *testing.T) {
	dashboard := dashboardAtLocalProjectStep("/work/remembered")

	wizard := newWizard(dashboard)
	if directory := wizard.projectDirectory.String(); directory != "/work/remembered" {
		t.Fatalf("directory %q, want /work/remembered", directory)
	}
	if cursor := wizard.projectDirectory.Cursor(); cursor != len("/work/remembered") {
		t.Fatalf("typing must continue at the end of what is drawn: cursor %d", cursor)
	}

	lines := capturedDrawn(dashboard, 140, 40)
	field := wizardsRowOf(lines, "New session · 2/3 local project") + 4
	if !strings.Contains(lines[field], "/work/remembered") {
		t.Fatalf("the field draws its value: %q", lines[field])
	}
}

// Everything the step composes has to be on the screen. The remembered
// directories and the key hints are the last rows it writes, so a dialog
// that is not sized for its own field and buttons loses exactly those.
// Hard-won: ee7ee201: project details, recents, or errors were hidden under wizard controls
func TestTheProjectStepDrawsItsRecentListAndErrorAboveTheButtons(t *testing.T) {
	dashboard := dashboardAtLocalProjectStep("/work/newer", "/work/older")
	typeSource(dashboard, "/work/newer")
	readyKey(dashboard, key(tea.KeyEnter))
	typed := newWizard(dashboard).projectDirectory.String()
	dashboard.ApplyProjectDirectoryValidation(typed,
		errors.New("project directory does not exist or is not a directory"))

	lines := capturedDrawn(dashboard, 140, 40)
	assertDialogSpacing(t, lines, "New session", "Cancel")
	buttons := wizardsRowOf(lines, "Cancel")
	for _, label := range []string{
		"Error: project directory does not exist",
		"Recent on this host",
		"/work/older",
	} {
		if wizardsRowOf(lines, label) >= buttons {
			t.Fatalf("%q is drawn above the button row: %q", label, lines)
		}
	}
}

func clickingARecentProjectFillsTheFieldAndEnterValidatesIt(t *testing.T) {
	dashboard := dashboardAtLocalProjectStep("/work/older", "/work/newer")
	newWizard(dashboard).projectDirectoryError = wzPtr("previous path was invalid")
	lines := capturedDrawn(dashboard, 140, 40)
	row := wizardsRowOf(lines, "/work/older")
	column := utf8.RuneCountInString(lines[row][:strings.Index(lines[row], "/work/older")])
	for _, kind := range []tea.MouseMsg{
		tea.MouseClickMsg{Button: tea.MouseLeft},
		tea.MouseReleaseMsg{Button: tea.MouseLeft},
	} {
		wzAction(t, dashboard.HandleMouse(mouseAt(kind, uv.Pos(column, row))), DashboardActionNone{})
		capturedDrawn(dashboard, 140, 40)
	}
	mode, ok := dashboard.mode.(ModeNew)
	if !ok {
		t.Fatal("clicking a recent directory stays on the project step")
	}
	if directory := mode.projectDirectory.String(); directory != "/work/older" {
		t.Fatalf("directory %q, want /work/older", directory)
	}
	if cursor := mode.projectDirectory.Cursor(); cursor != len("/work/older") {
		t.Fatalf("cursor %d, want the end", cursor)
	}
	if mode.projectHistoryIndex != 1 {
		t.Fatalf("history index %d, want 1", mode.projectHistoryIndex)
	}
	if mode.projectDirectoryError != nil {
		t.Fatalf("error %q survived the choice", *mode.projectDirectoryError)
	}
	wzFocusIs(t, &mode.form, WizardControlProjectDirectory)
	if action := readyKey(dashboard, key(tea.KeyEnter)); !isValidateProjectDirectory(action) {
		t.Fatalf("action %s, want a project directory validation", wzDebug(action))
	}
}

// The completion popup belongs to the dialog that owns the field: it may
// cover the rows under the field, but the button row and the dialog's frame
// stay readable, and no candidate is drawn on the dashboard behind the
// modal.
// Hard-won: 792e7ede: completion suggestions covered Cancel, Back, and Next
func TestTheCompletionPopupKeepsOffTheProjectStepsButtonRow(t *testing.T) {
	dashboard := dashboardAtLocalProjectStep()
	typeSource(dashboard, "/srv/pro")
	wzAction(t, readyKey(dashboard, wizardsCtrlSpace()), DashboardActionCompletePath{
		Host:   core.CompletionHostTarget{ID: "localhost"},
		Kind:   core.CompletionKindDirectories,
		Prefix: "/srv/pro",
	})
	dashboard.ApplyPathCompletions(dashboard.PathInputContext(), "/srv/pro", core.PathCompletion{
		Candidates: []string{"/srv/profile/", "/srv/project/"},
	})

	lines := capturedDrawn(dashboard, 140, 40)
	field := wizardsRowOf(lines, "New session · 2/3 local project") + 4
	buttons := wizardsRowOf(lines, "Cancel")
	if !strings.Contains(lines[field], "/srv/pro") {
		t.Fatalf("the popup leaves the field readable: %q", lines[field])
	}
	for _, candidate := range []string{"/srv/profile/", "/srv/project/"} {
		row := wizardsRowOf(lines, candidate)
		if row <= field || row >= buttons {
			t.Fatalf("%s is drawn between the field and the buttons: %q", candidate, lines)
		}
		count := 0
		for _, line := range lines {
			if strings.Contains(line, candidate) {
				count++
			}
		}
		if count != 1 {
			t.Fatalf("%s is drawn once, not %d times: %q", candidate, count, lines)
		}
	}
	if !strings.Contains(lines[buttons], "Back") || !strings.Contains(lines[buttons], "Next") {
		t.Fatalf("the button row is whole: %q", lines[buttons])
	}
	title := []rune(lines[wizardsRowOf(lines, "New session · 2/3 local project")])
	modalLeft := slices.Index(title, '╭')
	if modalLeft < 0 {
		t.Fatalf("no modal corner in %q", string(title))
	}
	modalWidth := slices.Index(title[modalLeft+1:], '╮')
	if modalWidth < 0 {
		t.Fatalf("no modal corner in %q", string(title))
	}
	buttonRow := []rune(lines[buttons])
	inside := buttonRow[min(modalLeft+1, len(buttonRow)):min(modalLeft+1+modalWidth, len(buttonRow))]
	if slices.Contains(inside, '─') {
		t.Fatalf("no popup border is drawn through the buttons: %q", lines[buttons])
	}
}

// The review's Compute row states the allocation itself; the " · " that
// joins it to a target name in the target list does not belong there.
// Launch campaign finding C-7.
// Hard-won: b87a404a: review rendered an empty compute value with a stray separator
func TestTheReviewComputeRowDoesNotStartWithASeparator(t *testing.T) {
	dashboard := wzNewDashboard(wzLocalBareConfig(), core.NewStateState())
	dashboard.beginNew()
	wizard := newWizard(dashboard)
	wizard.step = WizardStepReview
	wizard.projectDirectory = chat.PathInputFromValue("/work/main")
	dashboard.TakePrerequisiteCheck()
	context := dashboard.PathInputContext()
	dashboard.ApplyResolvedProjectDirectory(context, "/work/main", "/work/main",
		core.ManagedWorktreeOptions{Available: false, DefaultCreate: false}, false, nil)

	lines := capturedDrawn(dashboard, 120, 40)
	wzContains(t, lines[wizardsRowOf(lines, "Compute:")], "Compute: fixed/default resources")

	newWizard(dashboard).resourceAllocation = core.SessionResourceAllocationContainer{
		Cpus:        2,
		MemoryBytes: 4 * 1024 * 1024 * 1024,
	}
	lines = capturedDrawn(dashboard, 120, 40)
	wzContains(t, lines[wizardsRowOf(lines, "Compute:")], "Compute: 2 CPU / ")
}

// wzMountHistory is a state's mount history as the map the dashboard
// applies.
func wzMountHistory(state *core.StateState) map[string][]string {
	history := map[string][]string{}
	for host, directories := range state.MountHistory.All() {
		history[host] = slices.Clone(*directories)
	}
	return history
}

// Another surface or this run's own sessions can remember a project
// directory after the dashboard loaded its state. Opening the New wizard
// asks for the stored history again, and the project step offers what comes
// back. Launch campaign finding C-1.
// Hard-won: 5a8c036f: a project created in one run was missing from the next picker until restart
func TestOpeningTheNewWizardRefreshesRecentProjects(t *testing.T) {
	configuration := config()
	configuration.Targets = map[string]core.TargetTemplate{"localhost": core.TargetTemplateLocalBare{}}
	dashboard := wzNewDashboard(configuration, core.NewStateState())
	readyOpenNewWizard(dashboard)
	refresh, ok := dashboard.TakeMountHistoryRefresh()
	if !ok {
		t.Fatal("opening the wizard asks for the history")
	}
	wzAction(t, refresh, DashboardActionLoadMountHistory{})
	if again, ok := dashboard.TakeMountHistoryRefresh(); ok {
		t.Fatalf("one open asks once: %s", wzDebug(again))
	}
	stored := core.NewStateState()
	stored.RememberProjectDirectory("local", "/work/used-this-run")
	dashboard.ApplyMountHistory(wzMountHistory(&stored))

	// localhost is the only target, so its step is passed.
	readyKey(dashboard, key(tea.KeyEnter))
	mode, ok := dashboard.mode.(ModeNew)
	if !ok {
		t.Fatal("expected the New wizard")
	}
	if mode.step != WizardStepProjectDirectory {
		t.Fatalf("step %v, want ProjectDirectory", mode.step)
	}
	if !slices.Equal(mode.projectHistory, []string{"/work/used-this-run"}) {
		t.Fatalf("history %q", mode.projectHistory)
	}
}

func TestRecentProjectsUpdateAnOpenPickerAndFailedDiscoveryHasAnExplicitRetry(t *testing.T) {
	dashboard := wzNewDashboard(config(), core.NewStateState())
	readyOpenNewWizard(dashboard)
	mode, ok := dashboard.mode.(ModeNew)
	if !ok {
		t.Fatal("new wizard")
	}
	dashboard.mode = ModeDashboard{}
	mode.openProjects(dashboard)
	dashboard.mode = mode
	wzAction(t, dashboard.TakeProjectDiscovery(), DashboardActionLoadMountHistory{})
	history := core.NewStateState()
	history.RememberProjectDirectory("local", "/work/discovered")
	dashboard.ApplyMountHistory(wzMountHistory(&history))
	dashboard.ApplyProjectCatalogStatus(core.ProjectCatalogStatus{
		State: core.ProjectCatalogStateFailed,
		Errors: []string{
			"missing native project: " + strings.Repeat("stale checkout; ", 100),
			"fatal: not a git repository",
		},
	})
	wizard := newWizard(dashboard)
	if !slices.ContainsFunc(wizard.projectPicker.entries, func(entry core.ProjectEntry) bool {
		return entry.Source == "/work/discovered"
	}) {
		t.Fatalf("entries %+v", wizard.projectPicker.entries)
	}
	if err := wizard.projectPicker.error; err == nil || *err != "Project discovery reported 2 errors. See daemon logs for details." {
		t.Fatalf("error %v", err)
	}
	terminal := newWizardsTerminal(120, 32)
	terminal.draw(dashboard)
	rendered := strings.Join(terminal.lines(), "\n")
	wzContains(t, rendered, "Project discovery reported 2 errors.")
	wzNotContains(t, rendered, "stale checkout")
	wzNotContains(t, rendered, "fatal:")
	activateProjectControl(dashboard, WizardControlProjectRetry)
	wzAction(t, dashboard.TakeProjectDiscovery(), DashboardActionRefreshProjects{Retry: true})
	if next := dashboard.TakeProjectDiscovery(); next != nil {
		t.Fatalf("a second discovery is pending: %s", wzDebug(next))
	}
}

// Tab moves keyboard focus onto the recent directories, so each one has to
// look different while it holds focus, including the one that is already
// the selected directory. Launch campaign finding C-2.
// Hard-won: 8e3e20d4: focused and unfocused recent rows rendered identically
func TestAFocusedRecentProjectIsDrawnDifferentlyFromItsUnfocusedSelf(t *testing.T) {
	styleOf := func(dashboard *DashboardState, label string) uv.Style {
		terminal := newWizardsTerminal(140, 40)
		terminal.draw(dashboard)
		at := point(bufferLines(terminal.buffer), label)
		return terminal.buffer.CellAt(at.X, at.Y).Style
	}
	labels := []string{"/work/newer", "/work/older"}
	dashboard := dashboardAtLocalProjectStep(labels...)
	unfocused := make([]uv.Style, len(labels))
	for i, label := range labels {
		unfocused[i] = styleOf(dashboard, label)
	}

	checked := 0
	for range 6 {
		readyKey(dashboard, key(tea.KeyTab))
		wizard := newWizard(dashboard)
		focused, ok := wizard.form.Focused()
		if !ok {
			continue
		}
		index, ok := focused.RecentProjectIndex()
		if !ok {
			continue
		}
		label := wizard.projectHistory[index]
		position := slices.Index(labels, label)
		if position < 0 {
			t.Fatalf("unknown recent directory %q", label)
		}
		focusedStyle := styleOf(dashboard, label)
		if focusedStyle.Equal(&unfocused[position]) {
			t.Fatalf("%s looks the same with and without focus", label)
		}
		checked++
	}
	if checked < 2 {
		t.Fatal("Tab reaches both recent directories")
	}
}

// targetStepDrawsATableAndOffersResizeKeysOnlyForSizedTargets: #1175: the
// target step is a table like the profile step, and the resize keys are
// offered only for a target that is sized.
func targetStepDrawsATableAndOffersResizeKeysOnlyForSizedTargets(t *testing.T) {
	configuration := config()
	configuration.Targets["bare"] = core.TargetTemplateLocalBare{}
	ids := slices.Sorted(maps.Keys(configuration.Targets))
	bare, podman := slices.Index(ids, "bare"), slices.Index(ids, "podman")
	dashboard := wzNewDashboard(configuration, core.NewStateState())
	dashboard.beginNew()
	draw := func(target int) string {
		wizard := newWizard(dashboard)
		wizard.step = WizardStepTarget
		wizard.target = target
		terminal := newWizardsTerminal(140, 32)
		terminal.draw(dashboard)
		lines := terminal.lines()
		captureRender(140, 32, lines)
		return strings.Join(lines, "\n")
	}

	raw := draw(bare)
	if !strings.Contains(raw, "TARGET") || !strings.Contains(raw, "STATUS") {
		t.Fatalf("no table heading:\n%s", raw)
	}
	wzNotContains(t, raw, "+ double")
	wzNotContains(t, raw, "fixed/default resources")

	sized := draw(podman)
	wzNotContains(t, sized, "+ double")
	if !strings.Contains(sized, "CPU") || !strings.Contains(sized, "MEM (GiB)") {
		t.Fatalf("no size fields:\n%s", sized)
	}
}

// wzClick presses and releases the left button at one cell.
func wzClick(dashboard *DashboardState, at uv.Position) {
	dashboard.HandleMouse(mouseAt(tea.MouseClickMsg{Button: tea.MouseLeft}, at))
	dashboard.HandleMouse(mouseAt(tea.MouseReleaseMsg{Button: tea.MouseLeft}, at))
}

func containerFieldsAreUnderlinedBeforeFocusAndClickableOnSmallTerminals(t *testing.T) {
	for _, size := range [][2]int{{140, 40}, {80, 24}, {60, 20}} {
		width, height := size[0], size[1]
		dashboard := wzNewDashboard(config(), core.NewStateState())
		readyOpenNewWizard(dashboard)
		readyKey(dashboard, key(tea.KeyEnter))
		terminal := newWizardsTerminal(width, height)
		terminal.draw(dashboard)
		buffer := terminal.buffer
		lines := bufferLines(buffer)
		captureRender(width, height, lines)
		captureDetail("cell style", "CPU and MEM (GiB) inputs are underlined before focus")
		y := wizardsRowOf(lines, "MEM (GiB)") + 1
		cpuX := point(lines, "CPU").X
		memoryX := point(lines, "MEM (GiB)").X
		for _, field := range []struct {
			x        int
			expected WizardControl
		}{
			{cpuX, WizardControlResourceCpu},
			{memoryX, WizardControlResourceMemory},
		} {
			if cell := buffer.CellAt(field.x, y); cell == nil || cell.Style.Underline == uv.UnderlineNone {
				t.Fatalf("%dx%d: the input is not underlined: %q", width, height, lines)
			}
			wzClick(dashboard, uv.Pos(field.x, y))
			wzFocusIs(t, &newWizard(dashboard).form, field.expected)
			if step := newWizard(dashboard).step; step != WizardStepTarget {
				t.Fatalf("step %v, want Target", step)
			}
		}
		replaceResourceText(dashboard, "12.5")
		capturedDrawn(dashboard, width, height)
		if allocation := newWizard(dashboard).resourceAllocation; !wzEqual(allocation,
			core.SessionResourceAllocation(core.SessionResourceAllocationContainer{Cpus: 4, MemoryBytes: (12 << 30) + (1 << 29)})) {
			t.Fatalf("allocation %s", wzDebugAllocation(allocation))
		}
		assertDialogSpacing(t, lines, "target", "Next")
	}
}

func TestContainerMemoryErrorsKeepTheDraftAndRefuseSubmissionAboveHostLimits(t *testing.T) {
	dashboard := wzNewDashboard(config(), core.NewStateState())
	dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{testCapacityTarget()})
	dashboard.applyDeploymentCapacity("local", wzCapacityUsage(16<<30, 8), nil, 0)
	setLocalContainerCapacity(dashboard,
		core.HostContainerSize{Cpus: 8, MemoryBytes: 16 << 30},
		core.HostContainerSize{Cpus: 0, MemoryBytes: 0},
		core.HostContainerSize{Cpus: 8, MemoryBytes: (16 << 30) * 3 / 5},
		true)
	readyOpenNewWizard(dashboard)
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyTab))
	replaceResourceText(dashboard, "9")
	if cpu := newWizard(dashboard).resourceEditor.cpu.Value(); cpu != "9" {
		t.Fatalf("CPU %q, want 9", cpu)
	}
	if err := newWizard(dashboard).sizingError; err == nil || !strings.Contains(*err, "CPU exceeds") {
		t.Fatalf("error %v", err)
	}
	replaceResourceText(dashboard, "4")
	readyKey(dashboard, key(tea.KeyTab))
	for _, text := range []string{"", "0", "NaN", "-1", "3", "20", "18446744073709551615"} {
		replaceResourceText(dashboard, text)
		if memory := newWizard(dashboard).resourceEditor.memory.Value(); memory != text {
			t.Fatalf("memory %q, want %q", memory, text)
		}
		if allocation := newWizard(dashboard).resourceAllocation; allocation != nil {
			t.Fatalf("%q: allocation %s, want none", text, wzDebugAllocation(allocation))
		}
		readyKey(dashboard, key(tea.KeyEnter))
		if step := newWizard(dashboard).step; step != WizardStepTarget {
			t.Fatalf("%q: step %v, want Target", text, step)
		}
	}
	replaceResourceText(dashboard, "8.5")
	if allocation := newWizard(dashboard).resourceAllocation; !wzEqual(allocation,
		core.SessionResourceAllocation(core.SessionResourceAllocationContainer{Cpus: 4, MemoryBytes: (8 << 30) + (1 << 29)})) {
		t.Fatalf("allocation %s", wzDebugAllocation(allocation))
	}
}

func TestInvalidResourceTextSurvivesBackToProfileAndReenteringTarget(t *testing.T) {
	dashboard := wzNewDashboard(config(), core.NewStateState())
	readyOpenNewWizard(dashboard)
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyTab))
	replaceResourceText(dashboard, "invalid")
	newWizard(dashboard).form.Focus(WizardControlBack)
	wzFocusIs(t, &newWizard(dashboard).form, WizardControlBack)
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	if step := newWizard(dashboard).step; step != WizardStepTarget {
		t.Fatalf("step %v, want Target", step)
	}
	if cpu := newWizard(dashboard).resourceEditor.cpu.Value(); cpu != "invalid" {
		t.Fatalf("CPU %q, want invalid", cpu)
	}
	if allocation := newWizard(dashboard).resourceAllocation; allocation != nil {
		t.Fatalf("allocation %s, want none", wzDebugAllocation(allocation))
	}
}

func TestLateHostLimitsPreserveTypedValuesAndBlockAnOversizedDraft(t *testing.T) {
	dashboard := wzNewDashboard(config(), core.NewStateState())
	readyOpenNewWizard(dashboard)
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyTab))
	replaceResourceText(dashboard, "6")
	readyKey(dashboard, key(tea.KeyEnter))
	readyKey(dashboard, key(tea.KeyEnter))
	if step := newWizard(dashboard).step; step != WizardStepReview {
		t.Fatalf("step %v, want Review", step)
	}
	dashboard.TakePrerequisiteCheck()
	dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{testCapacityTarget()})
	dashboard.applyDeploymentCapacity("local", wzCapacityUsage(64<<30, 4), nil, 0)
	setLocalContainerCapacity(dashboard,
		core.HostContainerSize{Cpus: 4, MemoryBytes: 64 << 30},
		core.HostContainerSize{Cpus: 0, MemoryBytes: 0},
		core.HostContainerSize{Cpus: 4, MemoryBytes: (64 << 30) * 4 / 5},
		true)
	wizard := newWizard(dashboard)
	if wizard.step != WizardStepTarget {
		t.Fatalf("step %v, want Target", wizard.step)
	}
	if cpu := wizard.resourceEditor.cpu.Value(); cpu != "6" {
		t.Fatalf("CPU %q, want 6", cpu)
	}
	if wizard.resourceAllocation != nil {
		t.Fatalf("allocation %s, want none", wzDebugAllocation(wizard.resourceAllocation))
	}
	if err := wizard.sizingError; err == nil || !strings.Contains(*err, "CPU exceeds") {
		t.Fatalf("error %v", err)
	}
	if wizard.remotePreflightInFlight {
		t.Fatal("the oversized draft is not checked")
	}
	readyKey(dashboard, key(tea.KeyEnter))
	if step := newWizard(dashboard).step; step != WizardStepTarget {
		t.Fatalf("step %v, want Target", step)
	}
	dashboard.applyDeploymentCapacity("local", wzCapacityUsage(64<<30, 8), nil, 1)
	setLocalContainerCapacity(dashboard,
		core.HostContainerSize{Cpus: 8, MemoryBytes: 64 << 30},
		core.HostContainerSize{Cpus: 0, MemoryBytes: 0},
		core.HostContainerSize{Cpus: 8, MemoryBytes: (64 << 30) * 4 / 5},
		true)
	if cpu := newWizard(dashboard).resourceEditor.cpu.Value(); cpu != "6" {
		t.Fatalf("CPU %q, want 6", cpu)
	}
	if allocation := newWizard(dashboard).resourceAllocation; !wzEqual(allocation,
		core.SessionResourceAllocation(core.SessionResourceAllocationContainer{Cpus: 6, MemoryBytes: 8 << 30})) {
		t.Fatalf("allocation %s", wzDebugAllocation(allocation))
	}
}

func singleRawConfig() core.Config {
	configuration := config()
	maps.DeleteFunc(configuration.Profiles, func(id string, _ core.HarnessProfile) bool { return id != "claude-1" })
	configuration.Targets = map[string]core.TargetTemplate{"local": core.TargetTemplateLocalBare{}}
	return configuration
}

func singleProfileAndRawTargetCreateFromProjectWithoutConfirm(t *testing.T) {
	dashboard := wzNewDashboard(singleRawConfig(), core.NewStateState())
	dashboard.beginNew()
	if step := newWizard(dashboard).step; step != WizardStepProjectDirectory {
		t.Fatalf("step %v, want ProjectDirectory", step)
	}
	screen := strings.Join(capturedDrawn(dashboard, 100, 30), "\n")
	wzContains(t, screen, "1/1")
	wzContains(t, screen, "Create")
	wzNotContains(t, screen, "Back")
	if mode, ok := dashboard.mode.(ModeNew); ok {
		mode.projectDirectory = chat.PathInputFromValue("/work/project")
	}
	if action := readyKey(dashboard, key(tea.KeyEnter)); !isValidateProjectDirectory(action) {
		t.Fatalf("action %s, want a project directory validation", wzDebug(action))
	}
	dashboard.ApplyResolvedProjectDirectory(dashboard.PathInputContext(), "/work/project", "/work/project",
		core.ManagedWorktreeOptions{Available: true, DefaultCreate: true}, false, nil)
	if step := newWizard(dashboard).step; step != WizardStepLaunching {
		t.Fatalf("step %v, want Launching", step)
	}
	wzNotContains(t, strings.Join(capturedDrawn(dashboard, 100, 30), "\n"), "Confirm")
	action, _ := dashboard.TakePrerequisiteCheck()
	captureDetail("action", "CreateSession with managed worktree enabled")
	if create, ok := action.(DashboardActionCreateSession); !ok || create.CreateManagedWorktree == nil || !*create.CreateManagedWorktree {
		t.Fatalf("action %s, want a creation with a managed worktree", wzDebug(action))
	}
	wzInDashboard(t, dashboard)
	if check, ok := dashboard.TakePrerequisiteCheck(); ok {
		t.Fatalf("a second check: %s", wzDebug(check))
	}
}

func singleProfileKeepsContainerSizingAndConfirm(t *testing.T) {
	configuration := config()
	maps.DeleteFunc(configuration.Profiles, func(id string, _ core.HarnessProfile) bool { return id != "claude-1" })
	dashboard := wzNewDashboard(configuration, core.NewStateState())
	readyOpenNewWizard(dashboard)
	if step := newWizard(dashboard).step; step != WizardStepTarget {
		t.Fatalf("step %v, want Target", step)
	}
	screen := strings.Join(capturedDrawn(dashboard, 120, 30), "\n")
	wzContains(t, screen, "1/3")
	wzNotContains(t, screen, "Back")
	readyKey(dashboard, key(tea.KeyEnter))
	if step := newWizard(dashboard).step; step != WizardStepBundle {
		t.Fatalf("step %v, want Bundle", step)
	}
	capturedDrawn(dashboard, 120, 30)
	readyKey(dashboard, key(tea.KeyEnter))
	if step := newWizard(dashboard).step; step != WizardStepReview {
		t.Fatalf("step %v, want Review", step)
	}
	capturedDrawn(dashboard, 120, 30)
}

// wzRawLocalSession points the dashboard's session-1 at a local directory
// on the `local` bare target.
func wzRawLocalSession(dashboard *DashboardState) *core.SessionRecord {
	session := dashboard.state.Mut().Sessions.GetMut("session-1")
	if session == nil {
		panic("no session-1")
	}
	session.ProjectDirectory = wzPtr("/work/project")
	session.TargetTemplateID = "local"
	return session
}

func singleProfileRawResumeStartsChecksWithoutConfirm(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.config.Set(singleRawConfig())
	wzRawLocalSession(dashboard)

	action := dashboard.BeginResumeFor("session-1")
	captureDetail("action", "PreflightResumeRepositories")
	if _, ok := action.(DashboardActionPreflightResumeRepositories); !ok {
		t.Fatalf("action %s", wzDebug(action))
	}
	if step := resumeWizard(dashboard).step; step != WizardStepLaunching {
		t.Fatalf("step %v, want Launching", step)
	}
	if !dashboard.resumePreflightInFlight() {
		t.Fatal("the resume check is running")
	}
	if check, ok := dashboard.TakePrerequisiteCheck(); ok {
		t.Fatalf("a second check: %s", wzDebug(check))
	}
	wzNotContains(t, strings.Join(capturedDrawn(dashboard, 100, 30), "\n"), "Confirm")
}

// wzIsExecutingMove is whether action is a Move that executes rather than
// prepares.
func wzIsExecutingMove(action DashboardAction) bool {
	move, ok := action.(DashboardActionMoveSession)
	return ok && move.PreparationRequestID == nil
}

func TestRawMoveKeepsReviewAndSessionPolicyWhenBothDestinationPickersAreSkipped(t *testing.T) {
	// DSH shows a stored Native as None, which is what it does there; an
	// untouched Move still retains the stored policy.
	for _, destination := range []core.HarnessKind{core.HarnessKindClaude, core.HarnessKindDsh} {
		for _, stored := range []core.SubagentPolicy{
			{Mode: core.SubagentPolicyNative},
			{Mode: core.SubagentPolicyMjolnir},
			{Mode: core.SubagentPolicyMjolnir, Straightforward: &core.SubagentChoice{Model: "gpt-6-luna", Effort: wzPtr("max")}},
		} {
			dashboard := dashboardWithSession(runningSession())
			dashboard.config.Set(singleRawConfig())
			profile := dashboard.config.Get().Profiles["claude-1"]
			profile.Kind = destination
			dashboard.config.Mut().Profiles["claude-1"] = profile
			session := wzRawLocalSession(dashboard)
			session.Subagents = wzPtr(stored)

			move, ok := dashboard.beginMove().(DashboardActionMoveSession)
			if !ok || move.PreparationRequestID == nil || move.Subagents != nil {
				t.Fatal("prepare retains session policy")
			}
			request := *move.PreparationRequestID
			if step := resumeWizard(dashboard).step; step != WizardStepReview {
				t.Fatalf("step %v, want Review", step)
			}
			if policy := resumeWizard(dashboard).subagents.policy; !wzEqual(policy, stored) {
				t.Fatalf("policy %s, want %s", wzDebugPolicy(policy), wzDebugPolicy(stored))
			}
			dashboard.takeSubagentDiscovery()
			if selected, want := resumeWizard(dashboard).subagents.selected(), stored.AsChoiceFor(destination); !wzEqual(selected, want) {
				t.Fatalf("selected %s, want %s", wzDebugPolicy(selected), wzDebugPolicy(want))
			}
			preparation := movePreparation()
			preparation.Active = false
			preparation.Selection.ProfileID = wzPtr("claude-1")
			preparation.Selection.TargetTemplateID = wzPtr("local")
			if !dashboard.ApplyMovePreparation(request, preparation) {
				t.Fatal("the preparation applies")
			}
			if check, _ := dashboard.TakePrerequisiteCheck(); wzIsExecutingMove(check) {
				t.Fatal("the review must not execute by itself")
			}
			capturedDrawn(dashboard, 100, 30)
			if executed, ok := readyKey(dashboard, key(tea.KeyEnter)).(DashboardActionMoveSession); !ok || executed.PreparationRequestID != nil || executed.Subagents != nil {
				t.Fatal("Enter executes the move with the session's policy")
			}
			if check, _ := dashboard.TakePrerequisiteCheck(); wzIsExecutingMove(check) {
				t.Fatal("the move executes once")
			}
		}
	}
}

func TestRawResumeFastPathFailureWaitsForExplicitRetry(t *testing.T) {
	session := stoppedSession()
	session.ProjectDirectory = wzPtr("/work/project")
	session.TargetTemplateID = "local"
	dashboard := dashboardWithSession(session)
	dashboard.config.Set(singleRawConfig())
	if _, ok := dashboard.BeginResumeFor("session-1").(DashboardActionPreflightResumeRepositories); !ok {
		t.Fatal("the fast path starts the resume check")
	}
	firstGeneration := dashboard.SessionPreflightGeneration()
	capturedDrawn(dashboard, 100, 30)
	dashboard.FailResumePreflight("network unavailable")
	screen := strings.Join(capturedDrawn(dashboard, 100, 30), "\n")
	wzContains(t, screen, "network unavailable")
	wzContains(t, screen, "Retry")
	wzNotContains(t, screen, "confirm")
	if check, ok := dashboard.TakePrerequisiteCheck(); ok {
		t.Fatalf("a failed check waits for Retry: %s", wzDebug(check))
	}
	if _, ok := readyKey(dashboard, key(tea.KeyEnter)).(DashboardActionPreflightResumeRepositories); !ok {
		t.Fatal("Retry starts the resume check again")
	}
	if dashboard.SessionPreflightGeneration() == firstGeneration {
		t.Fatal("Retry runs under a new generation")
	}
	if !dashboard.resumePreflightInFlight() {
		t.Fatal("the retried check is running")
	}
	if err := resumeWizard(dashboard).preflightError; err != nil {
		t.Fatalf("error %q survived the retry", *err)
	}
}

func TestMoveComboboxMouseSelectionPreservesRecordUntilExplicitCommitAndReprepares(t *testing.T) {
	mjolnir := core.SubagentPolicy{Mode: core.SubagentPolicyMjolnir}
	native := core.SubagentPolicy{Mode: core.SubagentPolicyNative}
	for _, size := range [][2]int{{120, 40}, {80, 32}, {60, 24}} {
		width, height := size[0], size[1]
		session := runningSession()
		session.ProjectDirectory = wzPtr("/work/project")
		session.TargetTemplateID = "local"
		session.Subagents = wzPtr(mjolnir)
		dashboard := dashboardWithSession(session)
		dashboard.config.Set(singleRawConfig())
		move, ok := dashboard.beginMove().(DashboardActionMoveSession)
		if !ok || move.PreparationRequestID == nil || move.Subagents != nil {
			t.Fatal("prepare")
		}
		preparation := movePreparation()
		preparation.Selection.ProfileID = wzPtr("claude-1")
		preparation.Selection.TargetTemplateID = wzPtr("local")
		dashboard.ApplyMovePreparation(*move.PreparationRequestID, preparation)
		resumeWizard(dashboard).form.Focus(WizardControlSubagents)
		lines := capturedDrawn(dashboard, width, height)
		label := point(lines, "Sub-agents")
		for _, kind := range []tea.MouseMsg{tea.MouseClickMsg{Button: tea.MouseLeft}, tea.MouseReleaseMsg{Button: tea.MouseLeft}} {
			dashboard.HandleMouse(mouseAt(kind, uv.Pos(label.X+12, label.Y)))
			capturedDrawn(dashboard, width, height)
		}
		if !resumeWizard(dashboard).subagents.combo.IsOpen(WizardControlSubagents) {
			t.Fatalf("%d: the combobox opens: %q", width, lines)
		}
		dashboard.HandleKey(key(tea.KeyUp))
		capturedDrawn(dashboard, width, height)
		dashboard.HandleKey(key(tea.KeyEscape))
		if policy := resumeWizard(dashboard).subagents.policy; !wzEqual(policy, mjolnir) {
			t.Fatalf("policy %s, want the recorded one", wzDebugPolicy(policy))
		}
		if resumeWizard(dashboard).preparation == nil {
			t.Fatal("dismissing preview keeps preparation")
		}
		lines = capturedDrawn(dashboard, width, height)
		label = point(lines, "Sub-agents")
		for _, kind := range []tea.MouseMsg{tea.MouseClickMsg{Button: tea.MouseLeft}, tea.MouseReleaseMsg{Button: tea.MouseLeft}} {
			dashboard.HandleMouse(mouseAt(kind, uv.Pos(label.X+12, label.Y)))
			capturedDrawn(dashboard, width, height)
		}
		lines = capturedDrawn(dashboard, width, height)
		choice := point(lines, "Native")
		for _, kind := range []tea.MouseMsg{tea.MouseClickMsg{Button: tea.MouseLeft}, tea.MouseReleaseMsg{Button: tea.MouseLeft}} {
			dashboard.HandleMouse(mouseAt(kind, uv.Pos(choice.X+1, choice.Y)))
			capturedDrawn(dashboard, width, height)
		}
		if policy := resumeWizard(dashboard).subagents.policy; !wzEqual(policy, native) {
			t.Fatalf("policy %s, want Native", wzDebugPolicy(policy))
		}
		if resumeWizard(dashboard).preparation != nil {
			t.Fatal("the committed choice drops the preparation")
		}
		if recorded, _ := dashboard.state.Get().Sessions.Get("session-1"); recorded.Subagents == nil || !wzEqual(*recorded.Subagents, mjolnir) {
			t.Fatal("draft does not change recorded policy")
		}
		check, _ := dashboard.TakePrerequisiteCheck()
		prepare, ok := check.(DashboardActionMoveSession)
		if !ok || prepare.Subagents == nil || !wzEqual(*prepare.Subagents, native) || prepare.PreparationRequestID == nil {
			t.Fatal("the committed Native choice must reach preparation")
		}
		// Hard-won: 5390a50d: the displayed Native choice was sealed as an omitted override.
		wrong := movePreparation()
		wrong.Selection.ProfileID = wzPtr("claude-1")
		wrong.Selection.TargetTemplateID = wzPtr("local")
		if !dashboard.ApplyMovePreparation(*prepare.PreparationRequestID, wrong) {
			t.Fatal("the preparation applies")
		}
		if mode, ok := dashboard.mode.(ModeResume); ok {
			mode.form.Focus(WizardControlSubmit)
		}
		retry, ok := readyKey(dashboard, key(tea.KeyEnter)).(DashboardActionMoveSession)
		if !ok || retry.Subagents == nil || !wzEqual(*retry.Subagents, native) || retry.PreparationRequestID == nil {
			t.Fatal("a preparation that lost Native must be prepared again")
		}
		wrong.Selection.Subagents = wzPtr(native)
		if !dashboard.ApplyMovePreparation(*retry.PreparationRequestID, wrong) {
			t.Fatal("the corrected preparation applies")
		}
		if mode, ok := dashboard.mode.(ModeResume); ok {
			mode.form.Focus(WizardControlSubmit)
		}
		executed, ok := readyKey(dashboard, key(tea.KeyEnter)).(DashboardActionMoveSession)
		if !ok || executed.Subagents == nil || !wzEqual(*executed.Subagents, native) || executed.PreparationRequestID != nil {
			t.Fatal("the move executes with Native")
		}
	}
}

// wzScenario is one golden scenario: its label and what it runs.
type wzScenario struct {
	label string
	run   func(*testing.T)
}

// wzCaptureGolden runs scenarios under capture and compares what they
// recorded with the named golden.
func wzCaptureGolden(t *testing.T, name string, scenarios []wzScenario) {
	t.Helper()
	takeGoldenOutput()
	for _, scenario := range scenarios {
		captureScenario(t, scenario.label, scenario.run)
	}
	core.AssertGolden(t, "testdata", name, takeGoldenOutput())
}

func TestGoldenProjectPicker(t *testing.T) {
	wzCaptureGolden(t, "project-picker", []wzScenario{
		{"recent_mount_list_filters_attached_sources_and_scrolls", TestRecentMountListFiltersAttachedSourcesAndScrolls},
		{"project_picker_skips_empty_saved_projects_and_offers_github_from_recent", projectPickerSkipsEmptySavedProjectsAndOffersGithubFromRecent},
		{"saved_projects_show_every_source_of_the_selection_and_stack_add_and_remove", savedProjectsShowEverySourceOfTheSelectionAndStackAddAndRemove},
		{"project_picker_url_enter_creates_one_repository_and_advances_to_review", projectPickerURLEnterCreatesOneRepositoryAndAdvancesToReview},
		{"project_picker_multiple_sources_wait_for_submit_and_remove_selected", projectPickerMultipleSourcesWaitForSubmitAndRemoveSelected},
		{"project_picker_github_search_chooses_the_keyboard_selected_result", projectPickerGithubSearchChoosesTheKeyboardSelectedResult},
		{"project_picker_multiple_discovery_selection_toggles_members_and_can_change_primary", projectPickerMultipleDiscoverySelectionTogglesMembersAndCanChangePrimary},
		{"project_picker_mouse_tabs_and_single_repository_choice_work_on_small_terminals", projectPickerMouseTabsAndSingleRepositoryChoiceWorkOnSmallTerminals},
		{"project_picker_url_or_path_submits_a_local_repository_path", projectPickerURLOrPathSubmitsALocalRepositoryPath},
		{"project_picker_results_remain_visible_with_multiple_selected_repositories", projectPickerResultsRemainVisibleWithMultipleSelectedRepositories},
		{"project_picker_mouse_multiple_toggle_and_url_add_keep_input_above_actions", projectPickerMouseMultipleToggleAndURLAddKeepInputAboveActions},
		{"directory_completion_lists_every_candidate_and_selects_with_keys", directoryCompletionListsEveryCandidateAndSelectsWithKeys},
		{"clicking_a_recent_project_fills_the_field_and_enter_validates_it", clickingARecentProjectFillsTheFieldAndEnterValidatesIt},
	})
}

func TestGoldenProfilePickerTable(t *testing.T) {
	wzCaptureGolden(t, "profile-picker-table", []wzScenario{
		{"new_session_profile_step_aligns_its_columns", newSessionProfileStepAlignsItsColumns},
		{"new_session_profile_step_shows_weekly_and_five_hour_percentages", newSessionProfileStepShowsWeeklyAndFiveHourPercentages},
		{"resume_profile_step_aligns_its_columns", resumeProfileStepAlignsItsColumns},
		{"a_missing_profile_is_left_out_of_the_profile_step_and_named", aMissingProfileIsLeftOutOfTheProfileStepAndNamed},
	})
}

func newSessionWizardKeepsItsFrameAcrossSteps(t *testing.T) {
	for _, size := range [][2]int{{140, 40}, {60, 22}} {
		width, height := size[0], size[1]
		dashboard := dashboardWithSession(stoppedSession())
		dashboard.beginNewWizard()
		var frameBounds *[4]int
		for _, step := range []WizardStep{
			WizardStepProfile,
			WizardStepTarget,
			WizardStepBundle,
			WizardStepNewBundle,
			WizardStepProjectDirectory,
			WizardStepReview,
			WizardStepMounts,
			WizardStepLaunching,
		} {
			wizard := newWizard(dashboard)
			wizard.step = step
			wizard.form = chat.NewDialog[WizardControl]()
			// Hard-won: ee7ee2012: wizard steps moved and resized the frame around their content.
			lines := capturedDrawn(dashboard, width, height)
			title := "New session"
			switch step {
			case WizardStepNewBundle:
				title = "Choose a project"
			case WizardStepMounts:
				title = "Add attached directory"
			case WizardStepLaunching:
				title = "Creating session"
			}
			top := wizardsRowOf(lines, title)
			row := []rune(lines[top])
			left := slices.Index(row, '╭')
			right := -1
			for index := len(row) - 1; index >= 0; index-- {
				if row[index] == '╮' {
					right = index
					break
				}
			}
			if left < 0 || right < 0 {
				t.Fatalf("%v: no frame corners on %q", step, lines[top])
			}
			bottom := -1
			for y := top + 1; y < len(lines); y++ {
				if cells := []rune(lines[y]); left < len(cells) && cells[left] == '╰' {
					bottom = y
					break
				}
			}
			if bottom < 0 {
				t.Fatalf("%v: no frame bottom: %q", step, lines)
			}
			bounds := [4]int{left, top, right - left + 1, bottom - top + 1}
			if frameBounds == nil {
				frameBounds = &bounds
			}
			if *frameBounds != bounds {
				t.Fatalf("%v: frame %v, want %v", step, bounds, *frameBounds)
			}
			if bounds[2] > 84 || bounds[3] > 24 {
				t.Fatalf("%v: frame %v is too large", step, bounds)
			}
		}
	}
}

// reviewWarnsOnceAndOffersASandboxedTarget: an agent using the Mjolnir
// guardian on a raw machine is warned about once, at the review, with the
// sandboxed target one press away; without one the warning says what is
// missing instead.
func reviewWarnsOnceAndOffersASandboxedTarget(t *testing.T) {
	build := func(withPodman bool) *DashboardState {
		configuration := config()
		configuration.Profiles = map[string]core.HarnessProfile{"kimi-1": wzKimiProfile()}
		podman := config().Targets["podman"]
		configuration.Targets = map[string]core.TargetTemplate{"localhost": core.TargetTemplateLocalBare{}}
		if withPodman {
			configuration.Targets["podman"] = podman
		}
		dashboard := wzNewDashboard(configuration, core.NewStateState())
		dashboard.beginNew()
		wizard := newWizard(dashboard)
		wizard.projectDirectory.SetValue("/home/me/project")
		wizard.target = 0 // localhost sorts before podman
		wizard.profileStepSkipped = false
		wizard.targetStepSkipped = false
		wizard.step = WizardStepReview
		return dashboard
	}
	draw := func(dashboard *DashboardState) string {
		terminal := newWizardsTerminal(140, 32)
		terminal.draw(dashboard)
		lines := terminal.lines()
		captureRender(140, 32, lines)
		return strings.Join(lines, "\n")
	}

	without := build(false)
	screen := draw(without)
	if count := strings.Count(screen, "runs directly on the host"); count != 1 {
		t.Fatalf("warned %d times:\n%s", count, screen)
	}
	wzContains(t, screen, "No compatible sandbox is available")

	with := build(true)
	screen = draw(with)
	if count := strings.Count(screen, "runs directly on the host"); count != 1 {
		t.Fatalf("warned %d times:\n%s", count, screen)
	}
	wzContains(t, screen, "Use podman instead (sandboxed)")
	// Activating the offer returns to the target step with the sandbox
	// chosen.
	wzClick(with, point(strings.Split(screen, "\n"), "Use podman instead"))
	screen = draw(with)
	if !strings.Contains(screen, "TARGET") || !strings.Contains(screen, "STATUS") {
		t.Fatalf("the offer opens the target step:\n%s", screen)
	}
	wizard := newWizard(with)
	if wizard.step != WizardStepTarget {
		t.Fatalf("step %v, want Target", wizard.step)
	}
	if wizard.target == 0 {
		t.Fatal("the sandbox is chosen")
	}
}

// Hard-won: 01a64d2e8: trimming blank lines could remove the sandbox button row.
// resumeReviewKeepsTheSandboxButtonAsItsLastRow: resuming on a bare target
// an agent using the Mjolnir guardian: the review carries the sandbox
// button, and no worktree, sub-agent or attachment control follows it, so
// its row is the last control row.
func resumeReviewKeepsTheSandboxButtonAsItsLastRow(t *testing.T) {
	session := stoppedSession()
	session.TargetTemplateID = "localhost"
	session.ProjectDirectory = wzPtr("/work/project")
	session.HarnessKind = core.HarnessKindKimi
	session.LastProfile = "kimi-1"
	dashboard := dashboardWithSession(session)
	dashboard.config.Set(standardLocalTargetsConfig())
	dashboard.config.Mut().Profiles = map[string]core.HarnessProfile{"kimi-1": wzKimiProfile()}
	action := dashboard.BeginResumeFor("session-1")
	captureAction(action)
	answerTargetChecks(dashboard, nil)
	localhost := slices.Index(slices.Sorted(maps.Keys(dashboard.config.Get().Targets)), "localhost")
	if localhost < 0 {
		t.Fatal("the fixture has localhost")
	}
	wizard := resumeWizard(dashboard)
	wizard.target = localhost
	wizard.profileStepSkipped = false
	wizard.targetStepSkipped = false
	wizard.step = WizardStepReview
	review := strings.Join(capturedDrawn(dashboard, 140, 40), "\n")
	wzContains(t, review, "sandboxed")
}

func TestGoldenWizardTargetStep(t *testing.T) {
	wzCaptureGolden(t, "wizard-target-step", []wzScenario{
		{"review_warns_once_and_offers_a_sandboxed_target", reviewWarnsOnceAndOffersASandboxedTarget},
		{"resume_review_keeps_the_sandbox_button_as_its_last_row", resumeReviewKeepsTheSandboxButtonAsItsLastRow},
		{"new_session_wizard_keeps_its_frame_across_steps", newSessionWizardKeepsItsFrameAcrossSteps},
		{"new_session_prefers_interactive_choices_over_newer_unattended_sessions", newSessionPrefersInteractiveChoicesOverNewerUnattendedSessions},
		{"new_session_skips_the_target_step_when_only_one_target_is_offered", newSessionSkipsTheTargetStepWhenOnlyOneTargetIsOffered},
		{"a_machine_without_a_runtime_keeps_the_target_step_and_says_why", aMachineWithoutARuntimeKeepsTheTargetStepAndSaysWhy},
		{"new_session_shows_the_target_step_when_a_second_target_may_be_chosen", newSessionShowsTheTargetStepWhenASecondTargetMayBeChosen},
		{"a_lone_container_target_keeps_its_step_for_sizing", aLoneContainerTargetKeepsItsStepForSizing},
		{"resume_skips_the_target_step_when_only_one_target_suits_the_session", resumeSkipsTheTargetStepWhenOnlyOneTargetSuitsTheSession},
		{"resume_shows_the_target_step_when_two_targets_suit_the_session", resumeShowsTheTargetStepWhenTwoTargetsSuitTheSession},
		{"move_skips_the_target_step_when_only_one_target_suits_the_session", moveSkipsTheTargetStepWhenOnlyOneTargetSuitsTheSession},
		{"move_shows_the_target_step_when_two_targets_suit_the_session", moveShowsTheTargetStepWhenTwoTargetsSuitTheSession},
		{"selecting_a_row_skips_the_hidden_target", selectingARowSkipsTheHiddenTarget},
		{"target_step_draws_a_table_and_offers_resize_keys_only_for_sized_targets", targetStepDrawsATableAndOffersResizeKeysOnlyForSizedTargets},
		{"container_fields_are_underlined_before_focus_and_clickable_on_small_terminals", containerFieldsAreUnderlinedBeforeFocusAndClickableOnSmallTerminals},
	})
}

func TestGoldenSingleChoiceFastPaths(t *testing.T) {
	wzCaptureGolden(t, "single-choice-fast-paths", []wzScenario{
		{"single_profile_and_raw_target_create_from_project_without_confirm", singleProfileAndRawTargetCreateFromProjectWithoutConfirm},
		{"single_profile_keeps_container_sizing_and_confirm", singleProfileKeepsContainerSizingAndConfirm},
		{"single_profile_raw_resume_starts_checks_without_confirm", singleProfileRawResumeStartsChecksWithoutConfirm},
	})
}

// wzCommit is a combobox commit of index on id.
func wzCommit(id WizardControl, index int) chat.Interaction[WizardControl] {
	return chat.InteractionComboBoxCommit[WizardControl]{ID: id, Index: index}
}

var (
	wzPolicyNative  = core.SubagentPolicy{Mode: core.SubagentPolicyNative}
	wzPolicyMjolnir = core.SubagentPolicy{Mode: core.SubagentPolicyMjolnir}
	wzPolicyNone    = core.SubagentPolicy{Mode: core.SubagentPolicyNone}
)

// wzPolicyIs fails t unless policy is want.
func wzPolicyIs(t testing.TB, policy, want core.SubagentPolicy) {
	t.Helper()
	if !wzEqual(policy, want) {
		t.Fatalf("policy %s, want %s", wzDebugPolicy(policy), wzDebugPolicy(want))
	}
}

func TestOracleChoiceStaysAvailableWhenDelegationIsNone(t *testing.T) {
	configuration := config()
	configuration.Subagents.Oracle = []core.SubagentChoice{{Model: "advisor"}}
	draft := newSubagentDraft(wzPolicyNone)
	profile := slices.Sorted(maps.Keys(configuration.Profiles))[0]
	draft.updateChoices(profile, &configuration)
	if !draft.oracleAvailable() {
		t.Fatal("the oracle is available")
	}
	if changed, ok := draft.apply(wzCommit(WizardControlOracle, 1)); !ok || !changed {
		t.Fatalf("the oracle commit applies: changed %t, handled %t", changed, ok)
	}
	if draft.oracle {
		t.Fatal("the commit turns the oracle off")
	}
	wzPolicyIs(t, draft.policy, wzPolicyNone)
	var overrideChoice *core.SubagentChoice
	if n := len(configuration.Subagents.Oracle); n > 0 {
		last := configuration.Subagents.Oracle[n-1]
		overrideChoice = &last
		configuration.Subagents.Oracle = configuration.Subagents.Oracle[:n-1]
	}
	overrideDraft := newSubagentDraft(wzPolicyNone).withOracle(true, overrideChoice)
	overrideDraft.updateChoices(profile, &configuration)
	if !overrideDraft.oracleAvailable() {
		t.Fatal("the session's own oracle choice keeps it available")
	}
	configuration.Subagents.Oracle = nil
	if overrideDraft.oracleChoice != nil {
		configuration.Subagents.Oracle = []core.SubagentChoice{*overrideDraft.oracleChoice}
	}
	state := core.NewStateState()
	if state.LastOracleChoices == nil {
		state.LastOracleChoices = map[string]bool{}
	}
	state.LastOracleChoices[profile] = false
	dashboard := wzNewDashboard(configuration, state)
	dashboard.beginNewWizard()
	if newWizard(dashboard).subagents.oracle {
		t.Fatal("the remembered off choice is kept")
	}
}

// wzRememberPolicy records policy as the last delegation choice for
// profile.
func wzRememberPolicy(dashboard *DashboardState, profile string, policy core.SubagentPolicy) {
	state := dashboard.state.Mut()
	if state.LastSubagentPolicies == nil {
		state.LastSubagentPolicies = map[string]core.SubagentPolicy{}
	}
	state.LastSubagentPolicies[profile] = policy
}

// wzSetProfileKind changes the harness of one configured profile.
func wzSetProfileKind(dashboard *DashboardState, id string, kind core.HarnessKind) {
	profile := dashboard.config.Get().Profiles[id]
	profile.Kind = kind
	dashboard.config.Mut().Profiles[id] = profile
}

// wzEnabledProfileIndex is where id sits among the enabled profiles.
func wzEnabledProfileIndex(dashboard *DashboardState, id string) int {
	index := 0
	for enabled := range dashboard.config.Get().EnabledProfiles() {
		if enabled == id {
			return index
		}
		index++
	}
	panic(fmt.Sprintf("profile %q is not enabled", id))
}

// wzCreateSubagents is the delegation the open new-session wizard would
// create with.
func wzCreateSubagents(t testing.TB, dashboard *DashboardState) *core.SubagentPolicy {
	t.Helper()
	create, ok := dashboard.createSessionActionWithoutClosing(newWizard(dashboard)).(DashboardActionCreateSession)
	if !ok {
		t.Fatal("create")
	}
	return create.Subagents
}

func TestGoldenCreateDelegationAvailabilityAndSubmission(t *testing.T) {
	var output strings.Builder
	for _, configured := range []bool{false, true} {
		dashboard := dashboardWithSession(stoppedSession())
		if configured {
			choice := core.SubagentChoice{Model: "model"}
			subagents := &dashboard.config.Mut().Subagents
			subagents.Primary = []core.SubagentChoice{choice}
			subagents.Straightforward = []core.SubagentChoice{choice}
			subagents.Subtle = []core.SubagentChoice{choice}
			subagents.Oracle = []core.SubagentChoice{{Model: "advisor", Effort: wzPtr("high")}}
		}
		dashboard.beginNewWizard()
		newWizard(dashboard).step = WizardStepReview
		dashboard.takeSubagentDiscovery()
		want := wzPolicyNative
		if configured {
			want = wzPolicyMjolnir
		}
		wzPolicyIs(t, newWizard(dashboard).subagents.policy, want)
		screen := strings.Join(drawn(dashboard, 140, 40), "\n")
		wzContains(t, screen, "Sub-agents")
		if !configured {
			wzContains(t, screen, "Set up Mjolnir sub-agents")
		}
		fmt.Fprintf(&output, "=== configured=%t ===\n%s\n", configured, screen)
		if subagents := wzCreateSubagents(t, dashboard); subagents != nil {
			t.Fatalf("an untouched draft submits no delegation, got %s", wzDebugPolicy(*subagents))
		}
		if configured {
			newWizard(dashboard).form.Focus(WizardControlOrchestrators)
			drawn(dashboard, 140, 40)
			dashboard.HandleKey(key(' '))
			dashboard.takeSubagentDiscovery()
			policy := wzCreateSubagents(t, dashboard)
			if policy == nil {
				t.Fatal("orchestrator opt-in must submit the delegation policy")
			}
			if !policy.OrchestratorsEnabled() {
				t.Fatalf("policy %s enables no orchestrators", wzDebugPolicy(*policy))
			}
			screen := strings.Join(drawn(dashboard, 140, 40), "\n")
			wzContains(t, screen, "Orchestrators")
			fmt.Fprintf(&output, "=== orchestrators enabled ===\n%s\n", screen)
		}
		// None is available even without configured Mjolnir models.
		newWizard(dashboard).form.Focus(WizardControlSubagents)
		drawn(dashboard, 140, 40)
		dashboard.HandleKey(key(tea.KeyEnter))
		dashboard.HandleKey(key(tea.KeyEnd))
		screen = strings.Join(drawn(dashboard, 140, 40), "\n")
		fmt.Fprintf(&output, "=== configured=%t, selecting None ===\n%s\n", configured, screen)
		dashboard.HandleKey(key(tea.KeyEnter))
		dashboard.takeSubagentDiscovery()
		action := dashboard.createSessionActionWithoutClosing(newWizard(dashboard))
		if create, ok := action.(DashboardActionCreateSession); !ok || create.Subagents == nil || !wzEqual(*create.Subagents, wzPolicyNone) {
			t.Fatalf("action %s, want a creation with None", wzDebug(action))
		}
		fmt.Fprintf(&output, "action: %s\n", wzDebug(action))
		// The remembered selection must survive opening another draft.
		wzRememberPolicy(dashboard, "", wzPolicyNone)
		dashboard.beginNewWizard()
		dashboard.takeSubagentDiscovery()
		wzPolicyIs(t, newWizard(dashboard).subagents.policy, wzPolicyNone)
	}
	// DSH has no sub-agents of its own, so Native is not offered. Create
	// starts on a remembered Mjolnir only when it is configured, and on None
	// otherwise, and submits what it shows.
	for _, scenario := range []struct {
		remembered core.SubagentPolicy
		configured bool
		expected   core.SubagentPolicy
	}{
		{wzPolicyNative, true, wzPolicyNone},
		{wzPolicyMjolnir, true, wzPolicyMjolnir},
		{wzPolicyMjolnir, false, wzPolicyNone},
	} {
		dashboard := dashboardWithSession(stoppedSession())
		wzSetProfileKind(dashboard, "codex-1", core.HarnessKindDsh)
		if scenario.configured {
			dashboard.config.Mut().Subagents.Straightforward = []core.SubagentChoice{{Model: "model"}}
		}
		wzRememberPolicy(dashboard, "", scenario.remembered)
		dashboard.beginNewWizard()
		dsh := wzEnabledProfileIndex(dashboard, "codex-1")
		wizard := newWizard(dashboard)
		wizard.SetProfile(dsh)
		wizard.step = WizardStepReview
		dashboard.takeSubagentDiscovery()
		wzPolicyIs(t, newWizard(dashboard).subagents.selected(), scenario.expected)
		if policies := newWizard(dashboard).subagents.policies(); !slices.Equal(policies, []string{"Mjolnir", "None"}) {
			t.Fatalf("policies %q", policies)
		}
		newWizard(dashboard).form.Focus(WizardControlSubagents)
		drawn(dashboard, 140, 40)
		dashboard.HandleKey(key(tea.KeyEnter))
		screen := strings.Join(drawn(dashboard, 140, 40), "\n")
		fmt.Fprintf(&output, "=== DSH, remembered %s, configured=%t ===\n%s\n",
			wzDebugPolicy(scenario.remembered), scenario.configured, screen)
		dashboard.HandleKey(key(tea.KeyEnter))
		if subagents := wzCreateSubagents(t, dashboard); subagents == nil || !wzEqual(*subagents, scenario.expected) {
			t.Fatalf("submitted %v, want %s", subagents, wzDebugPolicy(scenario.expected))
		}
	}
	core.AssertGolden(t, "testdata", "create-delegation", output.String())
}

func TestMoveUnconfiguredDelegationRetainsNoneUntilChanged(t *testing.T) {
	session := runningSession()
	session.ProjectDirectory = wzPtr("/work/project")
	session.TargetTemplateID = "local"
	session.Subagents = wzPtr(wzPolicyNone)
	dashboard := dashboardWithSession(session)
	dashboard.config.Set(singleRawConfig())
	dashboard.beginMove()
	dashboard.takeSubagentDiscovery()
	wizard := resumeWizard(dashboard)
	wzPolicyIs(t, wizard.subagents.policy, wzPolicyNone)
	if reason, ok := wizard.subagents.unavailableReason(); !ok || !strings.Contains(reason, "Settings") {
		t.Fatalf("reason %q (%t)", reason, ok)
	}
	if wizard.subagents.selectPolicy(1) {
		t.Fatal("unconfigured Mjolnir cannot be chosen")
	}
	if change := resumeWizard(dashboard).subagentChange(dashboard); change != nil {
		t.Fatalf("change %s, want none", wzDebugPolicy(*change))
	}
	wzContains(t, strings.Join(drawn(dashboard, 140, 40), "\n"), "Set up Mjolnir sub-agents")
}

func TestDelegationRejectsDisabledMjolnirOnCreateAndMove(t *testing.T) {
	draft := newSubagentDraft(wzPolicyNative)
	configuration := config()
	draft.updateChoices("claude-1", &configuration)
	if draft.selectPolicy(1) {
		t.Fatal("unconfigured Mjolnir cannot be chosen")
	}
	if reason, ok := draft.unavailableReason(); !ok || !strings.Contains(reason, "Settings") {
		t.Fatalf("reason %q (%t)", reason, ok)
	}
	setKind := func(kind core.HarnessKind) {
		profile := configuration.Profiles["claude-1"]
		profile.Kind = kind
		configuration.Profiles["claude-1"] = profile
	}
	setKind(core.HarnessKindKimi)
	draft.updateChoices("claude-1", &configuration)
	if draft.selectPolicy(1) {
		t.Fatal("Kimi cannot use Mjolnir")
	}
	if reason, ok := draft.unavailableReason(); !ok || !strings.Contains(reason, "Claude, Codex, DSH, or OpenCode") {
		t.Fatalf("reason %q (%t)", reason, ok)
	}
	// DSH lists Mjolnir and None only; unconfigured Mjolnir stays disabled.
	setKind(core.HarnessKindDsh)
	draft.updateChoices("claude-1", &configuration)
	if policies := draft.policies(); !slices.Equal(policies, []string{"Mjolnir", "None"}) {
		t.Fatalf("policies %q", policies)
	}
	if enabled := draft.enabledPolicies(); !slices.Equal(enabled, []bool{false, true}) {
		t.Fatalf("enabled %v", enabled)
	}
	wzPolicyIs(t, draft.selected(), wzPolicyNone)
	if draft.selectPolicy(0) {
		t.Fatal("unconfigured Mjolnir cannot be chosen")
	}
	if draft.selectPolicy(1) {
		t.Fatal("Native shown as None is already None")
	}
	wzPolicyIs(t, draft.selected(), wzPolicyNone)
}

// wzMjolnirWith is Mjolnir delegation with model as the straightforward
// choice.
func wzMjolnirWith(model string) core.SubagentPolicy {
	return core.SubagentPolicy{Mode: core.SubagentPolicyMjolnir, Straightforward: &core.SubagentChoice{Model: model}}
}

// Hard-won: P2 review: changing Create's profile stranded remembered Mjolnir on an unsupported harness.
func TestCreateDelegationTracksProfileAvailabilityInBothDirections(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	choice := core.SubagentChoice{Model: "model"}
	dashboard.config.Mut().Subagents.Straightforward = []core.SubagentChoice{choice}
	dashboard.config.Mut().Subagents.Subtle = []core.SubagentChoice{choice}
	wzSetProfileKind(dashboard, "claude-1", core.HarnessKindGrok)
	wzRememberPolicy(dashboard, "codex-1", wzMjolnirWith("codex-model"))
	wzRememberPolicy(dashboard, "claude-1", wzMjolnirWith("grok-model"))
	dashboard.beginNewWizard()
	supported := wzEnabledProfileIndex(dashboard, "codex-1")
	unsupported := wzEnabledProfileIndex(dashboard, "claude-1")
	for _, step := range []struct {
		profile  int
		expected core.SubagentPolicy
	}{
		{supported, wzMjolnirWith("codex-model")},
		{unsupported, wzPolicyNative},
		{supported, wzMjolnirWith("codex-model")},
	} {
		newWizard(dashboard).SetProfile(step.profile)
		dashboard.takeSubagentDiscovery()
		wizard := newWizard(dashboard)
		wzPolicyIs(t, wizard.subagents.policy, step.expected)
		if message, ok := wizard.subagents.error(); ok {
			t.Fatalf("error %q", message)
		}
	}
	// A same-profile explicit choice survives availability refreshes.
	// Switching profiles loads that destination's saved policy.
	newWizard(dashboard).SetProfile(unsupported)
	dashboard.takeSubagentDiscovery()
	wizard := newWizard(dashboard)
	wizard.subagents.apply(wzCommit(WizardControlSubagents, 0))
	wizard.SetProfile(supported)
	dashboard.takeSubagentDiscovery()
	wzPolicyIs(t, newWizard(dashboard).subagents.policy, wzMjolnirWith("codex-model"))
}

// Hard-won: P2 review: Create did not reconcile delegation after global availability changed.
func TestCreateDelegationTracksConfigAvailabilityWithoutOverwritingExplicitChoices(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.beginNewWizard()
	supported := wzEnabledProfileIndex(dashboard, "codex-1")
	wizard := newWizard(dashboard)
	wizard.SetProfile(supported)
	wizard.step = WizardStepReview
	choice := core.SubagentChoice{Model: "model"}
	subagents := func() *core.SubagentConfig { return &dashboard.config.Mut().Subagents }
	subagents().Straightforward = []core.SubagentChoice{choice}
	subagents().Subtle = []core.SubagentChoice{choice}
	dashboard.SetProfileCapabilities(core.ProfileCapabilitiesSnapshot{})
	wzPolicyIs(t, newWizard(dashboard).subagents.policy, wzPolicyMjolnir)
	// Subtle is optional: straightforward alone keeps Mjolnir available.
	subagents().Subtle = nil
	dashboard.SetProfileCapabilities(core.ProfileCapabilitiesSnapshot{})
	wzPolicyIs(t, newWizard(dashboard).subagents.policy, wzPolicyMjolnir)
	// Subtle alone is never enough.
	subagents().Straightforward = nil
	subagents().Subtle = []core.SubagentChoice{choice}
	dashboard.SetProfileCapabilities(core.ProfileCapabilitiesSnapshot{})
	wzPolicyIs(t, newWizard(dashboard).subagents.policy, wzPolicyNative)
	if message, ok := newWizard(dashboard).subagents.error(); ok {
		t.Fatalf("error %q", message)
	}
	if selection := newWizard(dashboard).subagents.createSelection(); selection != nil {
		t.Fatalf("selection %s, want none", wzDebugPolicy(*selection))
	}
	subagents().Straightforward = []core.SubagentChoice{choice}
	dashboard.SetProfileCapabilities(core.ProfileCapabilitiesSnapshot{})
	wizard = newWizard(dashboard)
	wzPolicyIs(t, wizard.subagents.policy, wzPolicyMjolnir)
	if selection := wizard.subagents.createSelection(); selection != nil {
		t.Fatalf("selection %s, want none", wzDebugPolicy(*selection))
	}
	wizard.subagents.apply(wzCommit(WizardControlSubagents, 1))
	subagents().Straightforward = nil
	dashboard.SetProfileCapabilities(core.ProfileCapabilitiesSnapshot{})
	wzPolicyIs(t, newWizard(dashboard).subagents.policy, wzPolicyNative)
	subagents().Straightforward = []core.SubagentChoice{choice}
	wzRememberPolicy(dashboard, "", wzPolicyNative)
	dashboard.SetProfileCapabilities(core.ProfileCapabilitiesSnapshot{})
	wzPolicyIs(t, newWizard(dashboard).subagents.policy, wzPolicyMjolnir)
	if selection := newWizard(dashboard).subagents.createSelection(); selection == nil || !wzEqual(*selection, wzPolicyMjolnir) {
		t.Fatalf("selection %v, want Mjolnir", selection)
	}
}

// wzWorkerRelation is a worker sub-agent relation of child under parent.
func wzWorkerRelation(child *core.SessionRecord, parentID string) core.SubagentMetadata {
	return core.SubagentMetadata{
		Kind:            core.SubagentKindWorker,
		ChildSessionID:  child.ID,
		ParentSessionID: parentID,
		TaskName:        "Inspect parser",
		ProfileID:       child.LastProfile,
		RequestKey:      "request-1",
		CreatedAt:       child.CreatedAt,
	}
}

// wzAddSubagent records child as a sub-agent of the relation's parent.
func wzAddSubagent(dashboard *DashboardState, child core.SessionRecord, relation core.SubagentMetadata) {
	state := *dashboard.state.Get()
	state.EnvironmentOwners.Insert(child.ID, relation.ParentSessionID)
	state.Sessions.Insert(child.ID, child)
	state.Subagents.Insert(child.ID, relation)
	dashboard.SetState(state)
}

// "New session with these settings" on a sub-agent that works in its
// parent's managed worktree must start a top-level session that asks for a
// worktree of its own in the parent's source directory (never the parent's
// worktree path), takes delegation and review from the owning session (the
// child's own are fixed by the launch), and replaces a profile that can no
// longer be used instead of refusing.
func TestNewSessionLikeAChildInAManagedWorktreeIsTopLevelWithItsOwnWorktree(t *testing.T) {
	parent := legacyManagedSession(runningSession())
	parent.TargetTemplateID = "localhost"
	// Mjolnir delegation with no sub-agent model configured, and an oracle
	// with no oracle model: the copy cannot keep either.
	parent.Subagents = wzPtr(wzPolicyMjolnir)
	parent.Oracle = true
	parent.Review = wzPtr(core.SessionReview(core.SessionReviewOff{}))
	parent.NoGithubCredentials = true
	worktreeRoot := *parent.ProjectDirectory
	child := runningSession()
	child.ID = "child-session"
	child.TargetTemplateID = "localhost"
	child.ProjectDirectory = parent.ProjectDirectory
	child.ManagedWorktree = nil
	child.Subagents = wzPtr(wzPolicyNone)
	child.Oracle = false
	child.Review = nil
	child.NoGithubCredentials = true
	// Size overrides are a container's; a bare runtime has none to carry.
	child.LastProfile = "retired-profile"
	relation := wzWorkerRelation(&child, parent.ID)
	dashboard := dashboardWithSession(parent)
	dashboard.config.Mut().Targets["localhost"] = core.TargetTemplateLocalBare{}
	wzAddSubagent(dashboard, child, relation)

	create, ok := dashboard.beginNewSessionLike(child.ID).(DashboardActionCreateSession)
	if !ok {
		t.Fatal("the copy creates a session without a dialog")
	}
	if create.ProjectDirectory == nil || *create.ProjectDirectory != "/srv/project" {
		t.Fatalf("project directory %v, want /srv/project", create.ProjectDirectory)
	}
	if *create.ProjectDirectory == worktreeRoot {
		t.Fatal("the copy must not reuse the parent's worktree")
	}
	if create.CreateManagedWorktree == nil || !*create.CreateManagedWorktree {
		t.Fatal("the copy asks for a worktree of its own")
	}
	if create.Subagents == nil || !wzEqual(*create.Subagents, wzPolicyNative) {
		t.Fatalf("subagents %v, want Native", create.Subagents)
	}
	if create.Review == nil || !wzEqual(*create.Review, core.SessionReview(core.SessionReviewOff{})) {
		t.Fatalf("review %v, want Off", create.Review)
	}
	// The profile the wizard itself would open on.
	if profile, ok := dashboard.nthOfferedProfile(dashboard.defaultNewProfileIndex()); !ok || profile != create.ProfileID {
		t.Fatalf("profile %q, want the wizard's default %q (%t)", create.ProfileID, profile, ok)
	}
	if !create.Carried.NoGithubCredentials {
		t.Fatal("the copy keeps no-GitHub-credentials")
	}
	if create.Carried.Note == nil {
		t.Fatal("the replaced profile is reported")
	}
	note := *create.Carried.Note
	if !strings.Contains(note, "retired-profile") || !strings.Contains(note, create.ProfileID) {
		t.Fatalf("note %q", note)
	}
	if !strings.Contains(note, "Sub-agents: Mjolnir is not available") || !strings.Contains(note, "so this session uses Native.") {
		t.Fatalf("note %q", note)
	}
	if !strings.Contains(note, "Oracle is not available") || !strings.Contains(note, "starts without it.") {
		t.Fatalf("note %q", note)
	}
}

// "New session with these settings" carries the owner's requested
// allocation, including when the selected session borrows its owner's
// container.
func TestNewSessionLikeCarriesTheContainerOwnersSize(t *testing.T) {
	source := runningSession()
	source.ResourceAllocation = core.SessionResourceAllocationContainer{Cpus: 4, MemoryBytes: 6 << 30}
	dashboard := dashboardWithSession(source)

	create, ok := dashboard.beginNewSessionLike(source.ID).(DashboardActionCreateSession)
	if !ok {
		t.Fatal("the copy creates a session without a dialog")
	}
	if !wzEqual(create.ResourceAllocation, source.ResourceAllocation) {
		t.Fatalf("allocation %s, want %s", wzDebugAllocation(create.ResourceAllocation), wzDebugAllocation(source.ResourceAllocation))
	}

	// Hard-won: 6ddd61a06: a sub-agent's copy read the child's own size,
	// which is not updated when its parent's container is resized.
	child := runningSession()
	child.ID = "resized-child"
	child.ProjectDirectory = source.ProjectDirectory
	wzAddSubagent(dashboard, child, wzWorkerRelation(&child, source.ID))
	create, ok = dashboard.beginNewSessionLike(child.ID).(DashboardActionCreateSession)
	if !ok {
		t.Fatal("the copy of a sub-agent creates a session without a dialog")
	}
	if !wzEqual(create.ResourceAllocation, source.ResourceAllocation) {
		t.Fatalf("allocation %s, want the owner's %s", wzDebugAllocation(create.ResourceAllocation), wzDebugAllocation(source.ResourceAllocation))
	}
}

// A copied launch skips the new-session key, so it must wait for the host's
// Git identity check the way a fast-start launch does, then run as copied.
// Hard-won: aa2206c29: the copied launch bypassed the Git identity gate.
func TestNewSessionLikeWaitsForTheGitIdentityCheck(t *testing.T) {
	source := legacyManagedSession(runningSession())
	source.TargetTemplateID = "localhost"
	dashboard := dashboardWithSession(source)
	dashboard.config.Mut().Targets["localhost"] = core.TargetTemplateLocalBare{}
	dashboard.RequireGitIdentityCheck()

	wzAction(t, dashboard.beginNewSessionLike(source.ID), DashboardActionCheckGitIdentity{})
	dashboard.ApplyGitIdentityCheck(client.GitIdentity{
		Name:         wzPtr("Ada"),
		Email:        wzPtr("ada@example.test"),
		PerDirectory: false,
	}, nil)
	if _, ok := dashboard.TakeGitIdentityFollowup().(DashboardActionCreateSession); !ok {
		t.Fatal("the copied launch runs once the identity is known")
	}
}
