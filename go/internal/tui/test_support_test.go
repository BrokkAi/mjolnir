// Port of mj-tui/src/test_support.rs.

// Shared fixtures for the dashboard unit tests.
//
// crossterm's KeyEvent is a tea.KeyPressMsg here and its MouseEvent a
// tea.MouseMsg. A mouse event's kind (crossterm's MouseEventKind) is passed
// as a prototype message of the matching type carrying its button, such as
// tea.MouseClickMsg{Button: tea.MouseLeft} for Down(Left) or
// tea.MouseWheelMsg{Button: tea.MouseWheelUp} for ScrollUp; its position is
// ignored.

package tui

import (
	"encoding/json"
	"fmt"
	"maps"
	"runtime"
	"slices"
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

// Existing dashboard fixtures describe probe inputs and outcomes. Adapt them
// to the same host rows the daemon publishes to the dashboard.
func (d *DashboardState) setDeploymentCapacityTargets(targets []core.DeploymentCapacityTarget) {
	capacities := make([]client.RuntimeHostCapacity, 0, len(targets))
	for index := range targets {
		capacities = append(capacities, client.RuntimeHostCapacity{
			Reading:    core.NewHostCapacityRow(&targets[index], &core.TargetCapacityState{}, nil),
			Refreshing: false,
		})
	}
	d.SetHostCapacities(capacities)
}

// fixtureCapacityDetails is the capacity details in key order, as Rust's
// BTreeMap values() yields them.
func (d *DashboardState) fixtureCapacityDetails() []client.RuntimeHostCapacity {
	capacities := make([]client.RuntimeHostCapacity, 0, len(d.capacityDetails))
	for _, key := range slices.Sorted(maps.Keys(d.capacityDetails)) {
		capacities = append(capacities, d.capacityDetails[key])
	}
	return capacities
}

// applyDeploymentCapacity applies one probe outcome (Rust's
// `Result<Option<DeploymentCapacityUsage>, String>`: failure set for Err,
// usage nil for Ok(None)).
func (d *DashboardState) applyDeploymentCapacity(targetID string, usage *core.DeploymentCapacityUsage, failure *string, sampledAt uint64) {
	capacities := d.fixtureCapacityDetails()
	var detail *client.RuntimeHostCapacity
	for index := range capacities {
		if capacities[index].Reading.ID == targetID {
			detail = &capacities[index]
			break
		}
	}
	if detail == nil {
		panic(fmt.Sprintf("no capacity row for %s", targetID))
	}
	detail.Refreshing = false
	reading := &detail.Reading
	if failure == nil {
		sampled := sampledAt
		reading.SampledAtEpochSeconds = &sampled
		reading.ProbeError = nil
		reading.HasReading = usage != nil
		reading.OnDemand = usage == nil
		reading.CPUPercent = nil
		reading.MemoryUsedBytes = 0
		reading.MemoryTotalBytes = 0
		reading.LogicalCores = 0
		if usage != nil {
			reading.CPUPercent = usage.CPUPercent
			reading.MemoryUsedBytes = usage.MemoryUsedBytes
			reading.MemoryTotalBytes = usage.MemoryTotalBytes
			reading.LogicalCores = usage.LogicalCores
		}
	} else {
		reading.OnDemand = false
		probeError := *failure
		reading.ProbeError = &probeError
	}
	d.SetHostCapacities(capacities)
}

func (d *DashboardState) setTestCapacityStorage(views []core.TargetStorageView) {
	capacities := d.fixtureCapacityDetails()
	for index := range capacities {
		detail := &capacities[index]
		filesystems := []core.HostFilesystemRow{}
		for _, view := range views {
			if view.Host != detail.Reading.Host {
				continue
			}
			for _, filesystem := range view.Filesystems {
				filesystems = append(filesystems, core.HostFilesystemRow{
					Host:           view.Host,
					Mount:          filesystem.Mount,
					AvailableBytes: filesystem.AvailableBytes,
					TotalBytes:     filesystem.TotalBytes,
					Condition:      filesystem.Condition,
				})
			}
		}
		detail.Reading.Filesystems = filesystems
	}
	d.SetTargetStorage(views)
	d.SetHostCapacities(capacities)
}

// key is a press of code with no modifiers held. code is a character or one
// of Bubble Tea's key codes (tea.KeyEnter, tea.KeyUp, ...); characters carry
// their text as a terminal reports them.
func key(code rune) tea.KeyPressMsg {
	press := tea.KeyPressMsg{Code: code}
	if code < tea.KeyExtended && unicode.IsPrint(code) {
		press.Text = string(code)
	}
	return press
}

// bufferLines is the drawn buffer as one string per row.
func bufferLines(buffer *uv.Buffer) []string {
	lines := chat.BufferLines(buffer)
	pinNotificationAges(lines)
	return lines
}

func assertDialogSpacing(t testing.TB, lines []string, title, action string) {
	t.Helper()
	cells := make([][]rune, len(lines))
	for index, line := range lines {
		cells[index] = []rune(line)
	}
	top := -1
	for index, line := range lines {
		if strings.Contains(line, title) {
			top = index
			break
		}
	}
	if top < 0 {
		t.Fatal("dialog title")
	}
	titleColumn := utf8.RuneCountInString(lines[top][:strings.Index(lines[top], title)])
	left := -1
	for column := titleColumn - 1; column >= 0; column-- {
		if cells[top][column] == '╭' || cells[top][column] == '┌' {
			left = column
			break
		}
	}
	if left < 0 {
		t.Fatal("left border")
	}
	right := -1
	for column := titleColumn; column < len(cells[top]); column++ {
		if cells[top][column] == '╮' || cells[top][column] == '┐' {
			right = column
			break
		}
	}
	if right < 0 {
		t.Fatal("right border")
	}
	bottom := -1
	for row := top + 1; row < len(cells); row++ {
		if left < len(cells[row]) && (cells[row][left] == '╰' || cells[row][left] == '└') {
			bottom = row
			break
		}
	}
	if bottom < 0 {
		t.Fatal("bottom border")
	}
	text := strings.Join(lines, "\n")
	for _, row := range []int{top + 1, bottom - 2} {
		for _, cell := range cells[row][left+1 : right] {
			if cell != ' ' {
				t.Fatalf("row %d:\n%s", row, text)
			}
		}
	}
	if !strings.Contains(lines[bottom-1], action) {
		t.Fatalf("buttons touch bottom border:\n%s", text)
	}
	for _, row := range cells[top+1 : bottom] {
		if row[left+1] != ' ' {
			t.Fatalf("left padding:\n%s", text)
		}
		if row[right-1] != ' ' {
			t.Fatalf("right padding:\n%s", text)
		}
	}
}

// drawn is the whole dashboard surface drawn into a terminal of the given
// size, as one string per row.
//
// Rust draws through render::render, the combined surface with no
// conversation under a pinned NO_COLOR; this draws the same surface the same
// way.
func drawn(dashboard *DashboardState, width, height int) []string {
	dashboard.quotaProviders.settle()
	buffer := uv.NewBuffer(width, height)
	frame := chat.NewFrame(buffer)
	withNoColorOverrideForTest(true, func() struct{} {
		renderCombinedForTest(frame, dashboard, map[string]*chat.ActiveChat{}, map[PaneId]string{}, false)
		return struct{}{}
	})
	lines := bufferLines(buffer)
	pinSpinnerGlyphs(lines)
	return lines
}

// pinNotificationAges: notification ages use the wall clock in production.
// Goldens retain the header and its spacing while replacing just that
// changing value.
func pinNotificationAges(lines []string) {
	headers := []string{"│? ", "│! ", "|? ", "|! "}
	for index, line := range lines {
		// A menu can cover the title; recognize the card header itself.
		end := strings.Index(line, "s ago")
		if end < 0 {
			continue
		}
		start := len(strings.TrimRightFunc(line[:end], func(ch rune) bool { return ch >= '0' && ch <= '9' }))
		if start == end || strings.Count(line[:start], " · ") < 2 {
			continue
		}
		hasHeader := false
		for _, header := range headers {
			if strings.Contains(line[:start], header) {
				hasHeader = true
				break
			}
		}
		if !hasHeader {
			continue
		}
		replacement := "<age>" + strings.Repeat(" ", end-start)
		lines[index] = line[:start] + replacement + line[end+5:]
	}
}

// pinnedSpinnerReplacer replaces the compact Scan spinner's frames.
var pinnedSpinnerReplacer = strings.NewReplacer("⠁", "?", "⠂", "?", "⠄", "?", "⡀", "?")

// pinSpinnerGlyphs replaces the compact Scan spinner's frames with `?`.
// Spinners take their frame from a process-wide clock (chat's spinner
// elapsed time), so a golden would otherwise record whichever frame was
// showing when it drew.
func pinSpinnerGlyphs(lines []string) {
	for index, line := range lines {
		lines[index] = pinnedSpinnerReplacer.Replace(line)
	}
}

// point is the cell position of the first drawn row containing label, as
// the column where the label starts and the row it is on.
func point(lines []string, label string) uv.Position {
	for row, line := range lines {
		if strings.Contains(line, label) {
			return uv.Pos(cellColumn(line, label), row)
		}
	}
	panic(fmt.Sprintf("missing %q: %#v", label, lines))
}

// mouseAt is a mouse event of kind at one cell position, with no modifiers
// held.
func mouseAt(kind tea.MouseMsg, at uv.Position) tea.MouseMsg {
	mouse := kind.Mouse()
	mouse.X, mouse.Y, mouse.Mod = at.X, at.Y, 0
	return mouseLike(kind, mouse)
}

// mouseLike is mouse as a message of kind's type.
func mouseLike(kind tea.MouseMsg, mouse tea.Mouse) tea.MouseMsg {
	switch kind.(type) {
	case tea.MouseClickMsg:
		return tea.MouseClickMsg(mouse)
	case tea.MouseReleaseMsg:
		return tea.MouseReleaseMsg(mouse)
	case tea.MouseWheelMsg:
		return tea.MouseWheelMsg(mouse)
	default:
		return tea.MouseMotionMsg(mouse)
	}
}

// cellColumn is the column of needle within a drawn row, counted in cells
// rather than bytes.
func cellColumn(line, needle string) int {
	index := strings.Index(line, needle)
	if index < 0 {
		panic(fmt.Sprintf("missing %s in %q", needle, line))
	}
	return utf8.RuneCountInString(line[:index])
}

// summaryTextCell reports whether a drawn summary cell holds session text
// rather than the rule fill.
func summaryTextCell(cell *uv.Cell) bool {
	return strings.TrimSpace(cell.Content) != "" && cell.Content != "─"
}

func ctrlKey(character rune) tea.KeyPressMsg {
	modifier := tea.ModCtrl
	if runtime.GOOS == "darwin" {
		modifier = tea.ModSuper
	}
	return tea.KeyPressMsg{Code: character, Mod: modifier}
}

// altKey is an Alt chord, for the composer's readline keys. Alt is the same
// modifier everywhere, so this must not go through ctrlKey, which reports
// Super on macOS.
func altKey(character rune) tea.KeyPressMsg {
	return tea.KeyPressMsg{Code: character, Mod: tea.ModAlt}
}

// prefixKey is the default prefix key exactly as a terminal delivers it.
//
// Deliberately literal rather than ctrlKey: that helper reports Super on
// macOS, and the prefix is matched as Control on every platform.
func prefixKey() tea.KeyPressMsg {
	return tea.KeyPressMsg{Code: 'b', Mod: tea.ModCtrl}
}

// route presses a sequence of keys through the prefix router and dispatches
// what it decides exactly as the mj-cli event loop does, so a test drives
// the same path the live terminal does.
func route(dashboard *DashboardState, keys []tea.KeyMsg) DashboardAction {
	var action DashboardAction = DashboardActionNone{}
	for _, pressed := range keys {
		switch decided := dashboard.RouteBoundKey(pressed).(type) {
		case KeyRouteCommand:
			if !dashboard.CommandAllowedNow(decided.ID) {
				continue
			}
			if decided.ID == CommandIdSwitchWorkspace {
				if decided.Index != nil {
					action = dashboard.SelectWorkspaceIndex(*decided.Index)
				} else {
					action = DashboardActionNone{}
				}
			} else {
				action = dashboard.DispatchCommand(decided.ID)
			}
		case KeyRouteConsumed:
		case KeyRouteForward:
			action = dashboard.HandleKey(pressed)
		}
	}
	return action
}

// chordKeys are the key presses that run a command through its first live
// binding.
func chordKeys(dashboard *DashboardState, id CommandId) []tea.KeyMsg {
	action := spec(id).action
	if action == nil {
		panic(fmt.Sprintf("%v has no bindable action", id))
	}
	bindings := dashboard.Keybinds().Bindings(*action)
	if len(bindings) == 0 {
		panic(fmt.Sprintf("%v is not bound to any key", id))
	}
	binding := bindings[0]
	pressed := comboKeyEvent(binding.Combo)
	if binding.Trigger == core.TriggerPrefix {
		return []tea.KeyMsg{comboKeyEvent(dashboard.Keybinds().Prefix), pressed}
	}
	return []tea.KeyMsg{pressed}
}

// chord runs a command the way a person does: its first live binding,
// through the router, dispatched as mj-cli would.
func chord(dashboard *DashboardState, id CommandId) DashboardAction {
	return route(dashboard, chordKeys(dashboard, id))
}

func openNewSessionWizard(dashboard *DashboardState) DashboardAction {
	return chord(dashboard, CommandIdNewSessionWizard)
}

func openPalette(dashboard *DashboardState) DashboardAction {
	return chord(dashboard, CommandIdPalette)
}

func mouseIn(kind tea.MouseMsg, area uv.Rectangle) tea.MouseMsg {
	return mouseAt(kind, uv.Pos(area.Min.X+1, area.Min.Y+1))
}

// mouseAtRow is like mouseIn, but clicks a specific row within area instead
// of always the top-left cell, so tests can hit tail lines further down a
// multi-line hitbox.
func mouseAtRow(kind tea.MouseMsg, area uv.Rectangle, rowOffset int) tea.MouseMsg {
	return mouseAt(kind, uv.Pos(area.Min.X+1, area.Min.Y+rowOffset))
}

// config is the dashboard tests' configuration: three profiles, one
// project, and a local Podman target.
func config() core.Config {
	// SetupDialog sizes itself from the NO_COLOR-aware summary before
	// drawing.
	pinNoColorOverrideForTest(true)
	config := core.DefaultConfig()
	config.Version = core.ConfigVersion
	config.Profiles = map[string]core.HarnessProfile{
		"claude-1": core.NewHarnessProfile(core.HarnessKindClaude, "/profiles/claude"),
		"codex-1":  core.NewHarnessProfile(core.HarnessKindCodex, "/profiles/codex"),
		"codex-2":  core.NewHarnessProfile(core.HarnessKindCodex, "/profiles/codex-two"),
	}
	github := "BrokkAi/hel"
	config.Bundles = map[string]core.ProjectBundle{
		"hel": {
			PrimaryRepo: "hel",
			Repositories: []core.ProjectRepository{{
				ID:          "hel",
				Github:      &github,
				Destination: "hel",
			}},
		},
	}
	config.Targets = map[string]core.TargetTemplate{
		"podman": core.TargetTemplateLocalPodman{
			ContainerTemplate: core.ContainerTemplate{
				Image:            "ubuntu:24.04",
				PullPolicy:       core.ImagePullPolicyAuto,
				WorkspaceStorage: core.PodmanWorkspaceStoragePodmanVolume{},
			},
		},
	}
	return config
}

func stoppedSession() core.SessionRecord {
	native := "native-1"
	acpTitle := "ACP pretty name"
	return core.SessionRecord{
		Oracle:           true,
		WorkspaceID:      core.DefaultWorkspaceID,
		ID:               "session-1",
		Title:            "Raise the dead",
		HarnessKind:      core.HarnessKindCodex,
		LastProfile:      "codex-1",
		BundleID:         "hel",
		TargetTemplateID: "podman",
		AdditionalMounts: []core.AdditionalMount{},
		State:            core.SessionStateStopped,
		NativeSessionID:  &native,
		ACPSessionTitle:  &acpTitle,
		CreatedAt:        "2026-08-09T00:00:00Z",
		UpdatedAt:        "2026-08-09T01:00:00Z",
		Checkpoint: &core.CheckpointMetadata{
			ArchivePath:   "sessions/session-1.hel.zip",
			Sha256:        strings.Repeat("a", 64),
			CreatedAt:     "2026-08-09T01:00:00Z",
			EventFrontier: 2,
		},
	}
}

// runningSession is the same fixture as stoppedSession, but live. Dashboard
// interactions such as rename and the container editor only apply to a
// session that is actually on the dashboard.
func runningSession() core.SessionRecord {
	session := stoppedSession()
	session.Oracle = true
	session.Project = nil
	session.State = core.SessionStateRunning
	return session
}

func legacyManagedSession(session core.SessionRecord) core.SessionRecord {
	root := "/srv/project/.mj/worktrees/" + session.ID
	session.ProjectDirectory = &root
	baseCommit := strings.Repeat("1", 40)
	session.ManagedWorktree = &core.ManagedWorktree{
		Kind:                   core.ManagedCheckoutKindWorktree,
		SourceProjectDirectory: "/srv/project",
		SourceRepository:       "/srv/project",
		WorktreeRoot:           root,
		Branch:                 "mj/" + session.ID,
		Target:                 core.ManagedWorktreeTargetLocal{},
		BaseCommit:             &baseCommit,
	}
	return session
}

// question is a form question the agent is waiting on, as the daemon
// projects it.
func question(sessionID string) core.ElicitationRequest {
	params, err := json.Marshal(map[string]any{
		"mode":            "form",
		"sessionId":       sessionID,
		"message":         "Choose a path",
		"requestedSchema": map[string]any{"type": "object", "properties": map[string]any{"path": map[string]any{"type": "string"}}},
	})
	if err != nil {
		panic(err)
	}
	request, err := core.ElicitationRequestFromACPParams(sessionID+"-question", params)
	if err != nil {
		panic(fmt.Sprintf("valid test question: %v", err))
	}
	return request
}

// resumeCandidatesFor is what the daemon's ResumeCandidates answer holds for
// state: the inactive top-level records, and what every record says about
// native sessions and local checkouts.
func resumeCandidatesFor(config *core.Config, state *core.StateState) client.ResumeCandidates {
	var candidates client.ResumeCandidates
	for id, record := range state.Sessions.All() {
		if record.NativeSessionID != nil {
			candidates.AdoptedNativeSessions = append(candidates.AdoptedNativeSessions,
				client.DaemonPair[core.HarnessKind, string]{First: record.HarnessKind, Second: *record.NativeSessionID})
		}
		if checkout := record.ManagedWorktree; checkout != nil {
			if _, local := checkout.Target.(core.ManagedWorktreeTargetLocal); local {
				candidates.LocalCheckoutRoots = append(candidates.LocalCheckoutRoots, checkout.WorktreeRoot)
			}
		}
		if !record.State.IsActive() && !state.IsSubagentSession(id) {
			candidates.Candidates = append(candidates.Candidates, client.ResumeCandidateOf(record, config))
		}
	}
	return candidates
}

// openResumeDialog opens the resume dialog and answers its request for
// stopped sessions from the dashboard's own records, as the daemon would.
func openResumeDialog(dashboard *DashboardState, discoveryID uint64, profiles []ImportProfileOption) {
	dashboard.ShowResumeDialog(discoveryID, profiles)
	candidates := resumeCandidatesFor(dashboard.config.Get(), dashboard.state.Get())
	if !dashboard.ApplyResumeCandidates(discoveryID, candidates, nil) {
		panic("the resume dialog accepts its candidates")
	}
}

// focusResumeHelRows moves the open resume dialog to the Mjolnir tab with
// its list focused. The dialog opens on the running sessions with the list
// itself focused, and a stopped record is one tab to the right of them.
func focusResumeHelRows(dashboard *DashboardState) {
	if dialog, ok := dashboard.mode.(ModeResumeDialog); ok {
		dialog.tab = ResumeTabHel
		dialog.form.Focus(ResumeFocusSessions)
	}
	dashboard.RebuildResumeRows()
}

// openResumeWizard reaches the resume wizard the way the UI does: open the
// resume dialog, move to the Mjolnir tab, then activate the first row, which
// is the session's own stopped record.
func openResumeWizard(dashboard *DashboardState) DashboardAction {
	openResumeDialog(dashboard, 1, nil)
	focusResumeHelRows(dashboard)
	return dashboard.HandleKey(key(tea.KeyEnter))
}

func dashboardWithSession(session core.SessionRecord) *DashboardState {
	session.UpdatedAt = "2026-08-09T01:00:00Z"
	sessionID := session.ID
	state := core.StateState{Version: core.StateVersion}
	state.EnvironmentOwners.Insert(sessionID, sessionID)
	state.Sessions.Insert(session.ID, session)
	dashboard := NewDashboardState(config(), state, map[string]client.ProfileQuota{})
	publishFixtureVerdicts(dashboard)
	dashboard.quotaProviders.settle()
	// This fixture represents a conversation already opened by the host.
	serveAttention(dashboard, []string{sessionID}, core.SessionAttentionIdle)
	dashboard.navigation.assign(dashboard.FocusedPane(), &sessionID)
	return dashboard
}

// fixtureNeedsPerson reports the served attentions that ask for a person.
func fixtureNeedsPerson(attention core.SessionAttention) bool {
	switch attention {
	case core.SessionAttentionNeedsInput, core.SessionAttentionImportant, core.SessionAttentionUnread,
		core.SessionAttentionFailed, core.SessionAttentionUnreachable:
		return true
	}
	return false
}

func publishFixtureVerdicts(dashboard *DashboardState) {
	var verdicts core.SnapshotMap[string, client.SessionVerdict]
	config := dashboard.config.Get()
	for id, record := range dashboard.state.Get().Sessions.All() {
		active := record.State.IsActive()
		transition, transitioning := record.State.TransitionKind()
		verdict := client.SessionVerdict{LiveDashboard: active}
		if transitioning && record.LastError == nil {
			kind := transition
			verdict.Transition = &kind
		}
		if transitioning && record.LastError != nil &&
			(transition == core.SessionTransitionKindSuspending || transition == core.SessionTransitionKindDestroying) {
			kind := transition
			verdict.TransitionFailure = &kind
		}
		served, hasServed := dashboard.servedAttention.Get().Get(id)
		verdict.NeedsPerson = dashboard.verdict(id).NeedsPerson || (hasServed && fixtureNeedsPerson(served))
		offerIf := func(ready bool, otherwise client.SessionOffer) client.SessionOffer {
			if ready {
				return client.SessionOfferReady
			}
			return otherwise
		}
		verdict.Stop = offerIf(active, client.SessionOfferHidden)
		verdict.Restart = client.SessionOfferReady
		verdict.MoveSession = offerIf(active, client.SessionOfferHidden)
		verdict.ChangeWorkspace = offerIf(len(dashboard.workspaceIDs()) > 1, client.SessionOfferNoOtherWorkspace)
		target, configured := config.Targets[record.TargetTemplateID]
		verdict.ContainerSettings = offerIf(configured && core.IsContainerTarget(target), client.SessionOfferHidden)
		if active {
			verdict.Resume = client.SessionOfferActive
		} else {
			verdict.Resume = client.SessionOfferReady
		}
		verdicts.Insert(id, verdict)
	}
	dashboard.SetSessionVerdicts(verdicts)
}

// setWorking marks a session's turn as running, so it reads as working.
func setWorking(dashboard *DashboardState, sessionID string) {
	verdict := dashboard.verdict(sessionID)
	verdict.Interrupt = true
	verdict.ActiveWork = true
	verdict.InterruptAll = true
	verdict.InterruptCount++
	verdict.Unfinished = true
	verdict.Working = true
	dashboard.sessionVerdicts.Mut().Insert(sessionID, verdict)
	if parent, ok := dashboard.subagentParentFor(sessionID); ok {
		parentVerdict := dashboard.verdict(parent)
		parentVerdict.InterruptAll = true
		parentVerdict.InterruptCount++
		parentVerdict.WorkingChildren++
		parentVerdict.UnfinishedChildren++
		dashboard.sessionVerdicts.Mut().Insert(parent, parentVerdict)
	}
	serveAttention(dashboard, []string{sessionID}, core.SessionAttentionWorking)
	detail, ok := (*dashboard.sessionDetails.Mut())[sessionID]
	if !ok {
		panic("the session has details")
	}
	started := uint64(1)
	detail.currentTurnStartedAt = &started
	startedMs := int64(1)
	detail.activity.ReportedState = &core.ActivityState{Kind: core.ActivityStateTurn, StartedAtMs: &startedMs}
}

// serveAttention supplies the daemon publication independently of the raw
// transcript fixture.
func serveAttention(dashboard *DashboardState, ids []string, attention core.SessionAttention) {
	for _, id := range ids {
		verdict := dashboard.verdict(id)
		verdict.NeedsPerson = fixtureNeedsPerson(attention)
		dashboard.sessionVerdicts.Mut().Insert(id, verdict)
		dashboard.servedAttention.Mut().Insert(id, attention)
	}
}

// dashboardWithOneSubagent is a dashboard showing one running parent
// session with one running sub-agent. It returns the parent's id.
func dashboardWithOneSubagent() (*DashboardState, string) {
	parent := runningSession()
	child := runningSession()
	child.ID = "child-session"
	child.NotificationsIgnored = true
	relation := core.SubagentMetadata{
		Kind:            core.SubagentKindWorker,
		ChildSessionID:  child.ID,
		ParentSessionID: parent.ID,
		TaskName:        "Inspect parser",
		ProfileID:       child.LastProfile,
		RequestKey:      "request-1",
		CreatedAt:       child.CreatedAt,
	}
	dashboard := dashboardWithSession(parent)
	state := *dashboard.state.Get()
	state.Sessions.Insert(child.ID, child)
	state.Subagents.Insert(child.ID, relation)
	dashboard.SetState(state)
	publishFixtureVerdicts(dashboard)
	return dashboard, parent.ID
}

// dashboardWithFinishedNativeChild is a dashboard with one running parent
// and one finished harness-native child, as R10 saw one: Codex's "Review
// calc", completed, availability unknown, with its transcript in the store.
// It returns the parent's id and the child's presentation id.
func dashboardWithFinishedNativeChild() (*DashboardState, string, string) {
	parent := runningSession()
	dashboard := dashboardWithSession(parent)
	agent := core.NativeAgent{
		Availability:   core.NativeAgentAvailabilityUnknown,
		OwnerSessionID: parent.ID,
		SessionID:      "01a0d8d0-76ad-7e03-917d-8416c910f32c",
		Name:           "Review calc",
		Task:           "Delegated task for Review calc",
		Capabilities:   core.NativeAgentCapabilities{Cancel: true, Close: false},
		State:          core.NativeAgentStateCompleted,
	}
	id := agent.ViewID()
	projection := core.MaterializedSessionEmpty(id)
	projection.Transcript = []*core.TranscriptItem{
		thought(87, "Reading calc.py"),
		agentMessage(153, "calc.py adds and subtracts correctly."),
	}
	dashboard.SetNativeAgents([]core.NativeAgentView{{
		GenerationOrdinal: 81,
		PendingInputTimes: map[string]int64{},
		Agent:             agent,
		Projection:        projection,
	}})
	return dashboard, parent.ID, id
}

// precisionStorage is precision-3260 as the daemon's storage owner reports
// it: `/` holds the worker roots and profile homes, `~/Projects` is a
// filesystem of its own.
func precisionStorage(rootFree, projectsFree uint64) core.TargetStorageView {
	home := "/home/jonathan"
	sampled := uint64(1)
	return core.TargetStorageViewEvaluate(
		"precision-3260",
		&home,
		[]core.FilesystemSpace{
			{
				Mount:          "/",
				AvailableBytes: rootFree,
				TotalBytes:     500 << 30,
				ReservedBytes:  25 << 30,
				Paths: []string{
					"/home/jonathan/.local/share/hel/workers",
					"/home/jonathan/.local/share/hel/profiles",
					"/tmp",
				},
			},
			{
				Mount:          "/home/jonathan/Projects",
				AvailableBytes: projectsFree,
				TotalBytes:     1000 << 30,
				ReservedBytes:  0,
				Paths:          []string{"/home/jonathan/Projects"},
			},
		},
		&sampled,
		func(*core.FilesystemSpace) *string { return nil },
		nil,
	)
}

// precisionSession is a running session on precision-3260 whose managed
// clone is under `~/Projects`.
func precisionSession() core.SessionRecord {
	session := runningSession()
	session.Target = core.TargetLocatorSshBare{
		Host:      "precision-3260",
		Workspace: "/home/jonathan/Projects/app/.mj/clones/" + session.ID,
	}
	return session
}

func testCapacityTarget() core.DeploymentCapacityTarget {
	return core.DeploymentCapacityTarget{
		ID:                "local",
		Host:              "local",
		TargetIDs:         []string{"podman"},
		Kind:              core.DeploymentCapacityKindHost,
		Local:             true,
		Probes:            nil,
		LocalStoragePaths: nil,
		ProbeError:        nil,
	}
}

func transcriptItem(position uint64, body core.TranscriptBody) *core.TranscriptItem {
	atMs := int64(position) * 1_000
	item := &core.TranscriptItem{
		StableID:        fmt.Sprintf("item-%d", position),
		Position:        position,
		CreatedAtMs:     atMs,
		LastChangedAtMs: atMs,
		Body:            body,
	}
	if _, agent := body.(core.TranscriptBodyAgent); agent {
		latest := position
		item.LatestContentEventOrdinal = &latest
	}
	return item
}

// fixtureTextChunk is an ACP content chunk carrying text, as the transcript
// stores it.
func fixtureTextChunk(text string) json.RawMessage {
	chunk, err := json.Marshal(map[string]any{"content": map[string]any{"type": "text", "text": text}})
	if err != nil {
		panic(err)
	}
	return chunk
}

func agentMessage(position uint64, text string) *core.TranscriptItem {
	return transcriptItem(position, core.TranscriptBodyAgent{
		Chunks:    []json.RawMessage{fixtureTextChunk(text)},
		Streaming: false,
	})
}

func thought(position uint64, text string) *core.TranscriptItem {
	return transcriptItem(position, core.TranscriptBodyThought{
		Chunks:    []json.RawMessage{fixtureTextChunk(text)},
		Streaming: false,
	})
}

func workInterruption(position uint64) *core.TranscriptItem {
	item := transcriptItem(position, core.TranscriptBodySystem{Text: "Work interrupted"})
	item.StableID = fmt.Sprintf("%s%d", core.WorkInterruptedItemPrefix, position)
	return item
}

func materializedSessionFor(sessionID string, transcript []*core.TranscriptItem) core.MaterializedSession {
	frontier := uint64(0)
	for _, item := range transcript {
		frontier = max(frontier, item.Position)
	}
	session := core.MaterializedSessionEmpty(sessionID)
	session.AppliedEventOrdinal = frontier
	if frontier > 0 {
		session.AppliedEventDigest = strings.Repeat("a", 64)
	}
	session.Execution = core.MaterializedExecutionStateRunning{StartedAtMs: 100_000}
	lastActivity := int64(100_000)
	if len(transcript) > 0 {
		lastActivity = transcript[0].LastChangedAtMs
		for _, item := range transcript {
			lastActivity = max(lastActivity, item.LastChangedAtMs)
		}
	}
	session.LastActivityAtMs = &lastActivity
	session.Transcript = transcript
	return session
}

func applyMaterializedTranscript(dashboard *DashboardState, transcript []*core.TranscriptItem) {
	applyMaterializedTranscriptFor(dashboard, "session-1", transcript)
}

func applyMaterializedTranscriptFor(dashboard *DashboardState, sessionID string, transcript []*core.TranscriptItem) {
	materialized := materializedSessionFor(sessionID, transcript)
	dashboard.ApplyMaterializedSession(&materialized)
}

// numberedConversation is a conversation of count numbered exchanges, so
// preview scroll assertions can name the message they expect to see. Agent
// chunks coalesce unless separated, so each pairs with its own prompt.
func numberedConversation(count uint64) []*core.TranscriptItem {
	var items []*core.TranscriptItem
	for index := range count {
		content, err := json.Marshal(map[string]any{"type": "text", "text": fmt.Sprintf("question %d", index)})
		if err != nil {
			panic(err)
		}
		items = append(items,
			transcriptItem(index*2+1, core.TranscriptBodyUser{Content: []json.RawMessage{content}}),
			agentMessage(index*2+2, fmt.Sprintf("answer %d", index)),
		)
	}
	return items
}

func operation(kind SessionOperationKind, stage *core.ProvisionStage) SessionOperationDisplay {
	var activeStages []ProvisionStageStart
	if stage != nil {
		activeStages = []ProvisionStageStart{{Stage: *stage, StartedAt: 1_000}}
	}
	return SessionOperationDisplay{
		kind:                  kind,
		operationID:           nil,
		cancellable:           true,
		startedAtEpochSeconds: 1_000,
		placeholder:           nil,
		activeStages:          activeStages,
		resumeDestination:     nil,
	}
}

// availabilityAnswer is the launch verdicts this rendering fixture receives
// from the feed.
func availabilityAnswer(dashboard *DashboardState, uninstalled, unreachable []string) client.RuntimeAvailability {
	config := dashboard.config.Get()
	answer := client.RuntimeAvailability{
		Profiles:               map[string]client.AvailabilityVerdict{},
		Targets:                map[string]client.AvailabilityVerdict{},
		MachinesWithoutRuntime: map[string]string{},
		UninstalledEngines:     map[string]struct{}{},
	}
	for id := range config.EnabledProfiles() {
		answer.Profiles[id] = client.AvailabilityVerdict{Kind: client.AvailabilityVerdictAvailable}
	}
	var missingKinds []string
	for _, id := range uninstalled {
		missingKinds = append(missingKinds, core.RuntimeKindName(config.Targets[id]))
	}
	for id, target := range config.EnabledTargets() {
		kind := core.RuntimeKindName(target)
		localEngine := false
		switch target.(type) {
		case core.TargetTemplateLocalDocker, core.TargetTemplateLocalPodman, core.TargetTemplateAppleContainer:
			localEngine = true
		}
		missing := false
		for _, missingKind := range missingKinds {
			if missingKind == kind {
				missing = true
			}
		}
		verdict := client.AvailabilityVerdict{Kind: client.AvailabilityVerdictAvailable}
		if localEngine && missing {
			answer.UninstalledEngines[id] = struct{}{}
			reason := kind + " is not installed here."
			if !config.IsDefaultTarget(id) {
				text := id + ": " + reason
				answer.TargetNotes = append(answer.TargetNotes, client.AvailabilityNote{Full: text, Text: text, Routine: false})
			}
			verdict = client.AvailabilityVerdict{Kind: client.AvailabilityVerdictMissing, Reason: reason}
		}
		answer.Targets[id] = verdict
	}
	for _, machine := range unreachable {
		if _, known := config.Machines[machine]; !known {
			continue
		}
		publishUnreachableMachine(
			dashboard,
			&answer,
			machine,
			fmt.Sprintf("`ssh %s true` failed: Connection timed out.", machine),
			fmt.Sprintf("Check that %s is up.", machine),
			true,
		)
	}
	return answer
}

func publishUnreachableMachine(dashboard *DashboardState, answer *client.RuntimeAvailability, machine, detail, remediation string, routine bool) {
	var ids []string
	for id := range dashboard.config.Get().MachineRuntimes(machine) {
		ids = append(ids, id)
	}
	summary := "SSH to " + machine + " failed"
	if routine {
		summary = machine + " did not answer"
	}
	suffix := detail + " " + remediation
	for _, id := range ids {
		answer.Targets[id] = client.AvailabilityVerdict{
			Kind:   client.AvailabilityVerdictMissing,
			Reason: summary + ". " + suffix,
		}
	}
	var text string
	if len(ids) == 0 {
		answer.MachinesWithoutRuntime[machine] = "it did not answer. " + suffix
		text = summary + "."
	} else {
		text = summary + "; not listing " + strings.Join(ids, ", ") + "."
	}
	full := text + " " + suffix
	note := client.AvailabilityNote{Text: full, Full: full, Routine: routine}
	if routine {
		note.Text = text
	}
	answer.TargetNotes = append(answer.TargetNotes, note)
}

func answerAvailability(dashboard *DashboardState, answer func(*DashboardState) client.RuntimeAvailability) {
	availability := answer(dashboard)
	dashboard.SetRuntimeConfig(*dashboard.config.Get(), core.NewSharedSnapshot(availability))
}

// serveFormCard publishes authoritative form provenance, as the daemon
// runtime feed does.
func serveFormCard(dashboard *DashboardState, owner string, holder *string, request *core.ElicitationRequest) {
	form := client.CardFormElicitation(request, 0)
	var cards core.SnapshotMap[string, client.NotificationCard]
	cards.Insert(owner, client.NotificationCard{
		SinceMs: 0,
		Badge:   core.NotificationKindNeedsInput,
		Entries: nil,
		Holder:  holder,
		Form:    &form,
	})
	dashboard.SetNotificationCards(&cards)
}
