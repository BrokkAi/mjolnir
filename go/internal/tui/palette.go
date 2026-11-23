// Port of mj-tui/src/palette.rs.
//
// The searchable command palette: every command that applies right now, in
// one searchable list.
//
// The palette is a renderer over the action registry (actions.go). It
// groups what it lists the way the user is standing: the selected session's
// own commands first, under a heading naming that session, then the focused
// pane's, then the ones that answer from anywhere. Nothing here decides what
// a command does — [DashboardState.dispatchCommand] does that, so the
// palette, the footer, and the keyboard cannot disagree.
//
// It replaces the old session edit dialog, which existed only because the
// footer had no room for three more hints.
package tui

import (
	"cmp"
	"fmt"
	"math"
	"slices"
	"strings"
	"unicode"
	"unicode/utf8"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// PaletteEntry is one row of the palette: a command and whether it can be
// run.
//
// Blocked entries are listed greyed with their reason rather than dropped,
// because a list that hides what does not apply leaves the reader unable to
// tell "there is no such command" from "not here, not now".
type PaletteEntry struct {
	id           CommandId
	availability Availability
	// recent lists the entry under the Recent heading because it was run
	// lately, ahead of its own group.
	recent bool
}

// CommandPalette is the open palette.
//
// Rust's RefCell form is a plain field mutated through the palette's
// pointer.
type CommandPalette struct {
	// query is what the user has typed. Filtering is by label, then
	// description.
	query chat.TextInput
	// entries are the entries the query matches, in the order they are
	// drawn.
	entries []PaletteEntry
	// selected is the index into entries of the highlighted row.
	selected int
	// rankedFor is the query entries were built for, so a rebuild can tell
	// a new search from the same search rebuilt because availability moved.
	rankedFor   string
	form        chat.Dialog[PaletteControl]
	sessionOnly bool
	// sessionID is the session the menu acts on; nil when there is none.
	sessionID *string
	// sessionTitle is the session menu's title; nil for the command palette.
	sessionTitle *string
	// workspacePopup is the workspace list open on the "Change workspace"
	// row, if any.
	workspacePopup *WorkspacePopup
}

// Clone copies the palette, its entries and its form (Rust's derived
// Clone).
func (p *CommandPalette) Clone() CommandPalette {
	clone := *p
	clone.entries = slices.Clone(p.entries)
	clone.form = p.form.Clone()
	clone.sessionID = workspacesCloneString(p.sessionID)
	clone.sessionTitle = workspacesCloneString(p.sessionTitle)
	if p.workspacePopup != nil {
		popup := p.workspacePopup.Clone()
		clone.workspacePopup = &popup
	}
	return clone
}

// Equal compares two palettes field by field (Rust's derived PartialEq).
func (p *CommandPalette) Equal(other *CommandPalette) bool {
	samePopup := (p.workspacePopup == nil) == (other.workspacePopup == nil) &&
		(p.workspacePopup == nil || p.workspacePopup.Equal(other.workspacePopup))
	return p.query.Equal(&other.query) &&
		slices.Equal(p.entries, other.entries) &&
		p.selected == other.selected &&
		p.rankedFor == other.rankedFor &&
		p.form.Equal(&other.form) &&
		p.sessionOnly == other.sessionOnly &&
		workspacesStringsEqual(p.sessionID, other.sessionID) &&
		workspacesStringsEqual(p.sessionTitle, other.sessionTitle) &&
		samePopup
}

// PaletteControl identifies the palette's controls.
type PaletteControl int

const (
	PaletteControlQuery PaletteControl = iota
	PaletteControlCommands
	PaletteControlRun
	// PaletteControlWorkspace is the combobox anchored on the "Change
	// workspace" row.
	PaletteControlWorkspace
	// PaletteControlCopySessionId is the copy button in a session menu's top
	// right.
	PaletteControlCopySessionId
)

func (p *CommandPalette) prepare() {
	form := &p.form
	form.BeginUpdate()
	if !p.sessionOnly {
		form.DeclareWithEnabled(PaletteControlQuery, chat.ControlKindTextField{}, true)
	}
	form.DeclareWithEnabled(
		PaletteControlCommands,
		chat.ControlKindChoiceList{Len: len(p.entries), Selected: p.selected},
		len(p.entries) > 0,
	)
	if !p.sessionOnly {
		form.DeclareWithEnabled(PaletteControlRun, chat.ControlKindButton{}, p.selectedReady())
	}
	if p.workspacePopup != nil {
		form.DeclareWithEnabled(PaletteControlWorkspace, p.workspacePopup.kind(), true)
	}
	form.SetMenu(true)
	form.SetListActivation(PaletteControlCommands, chat.ListActivationSingleClick)
	if p.sessionOnly {
		form.EndFrame(PaletteControlCommands)
	} else {
		form.EndFrame(PaletteControlQuery)
	}
}

// selectedReady reports whether the highlighted entry can run now.
func (p *CommandPalette) selectedReady() bool {
	return p.selected >= 0 && p.selected < len(p.entries) &&
		p.entries[p.selected].availability == Availability(AvailabilityReady{})
}

var sessionMenuCommands = []CommandId{
	CommandIdChangedFiles,
	CommandIdDismissNotification,
	CommandIdOpenWaitingQuestion,
	CommandIdIgnoreNotifications,
	CommandIdRenameSession,
	CommandIdPinSession,
	CommandIdChangeWorkspace,
	CommandIdContainerSettings,
	CommandIdMoveSession,
	CommandIdSuspendSession,
	CommandIdRestartSession,
	CommandIdUpgradeContainer,
	CommandIdInterruptAll,
	CommandIdDestroySession,
}

// nativeAgentMenuCommands is the menu of a harness-native child. Its
// parent's harness owns it, and Mjolnir keeps no record of its own for it to
// rename, pin, restart or destroy, so reading its conversation is what
// applies (R10-2).
var nativeAgentMenuCommands = []CommandId{CommandIdOpenSession}

func sessionMenuEntries(dashboard *DashboardState) []PaletteEntry {
	commands := sessionMenuCommands
	if id, ok := dashboard.commandSessionID(); ok && dashboard.IsNativeAgent(id) {
		commands = nativeAgentMenuCommands
	}
	var entries []PaletteEntry
	for _, id := range commands {
		availability := spec(id).available(dashboard)
		if availability != Availability(AvailabilityHidden{}) {
			entries = append(entries, PaletteEntry{id: id, availability: availability, recent: false})
		}
	}
	return entries
}

// paletteSessionFacts is Rust's palette::session_facts, prefixed because
// session_view's SessionFacts type may take the unprefixed Go name. It lists
// facts about the session that its row does not show and that change what
// the menu's actions do: how it delegates, what it has checked out, and the
// container settings it carries into its next container.
func paletteSessionFacts(dashboard *DashboardState, session *core.SessionRecord) []chat.Line {
	var facts []chat.Line
	// The bare top line reads like the session row's info: no field labels.
	detail := (*dashboard.sessionDetails.Get())[session.ID]
	var top []string
	if detail != nil && detail.currentModel != nil {
		top = append(top, *detail.currentModel)
	}
	if detail != nil && detail.currentEffort != nil {
		top = append(top, *detail.currentEffort)
	}
	top = append(top, session.LastProfile)
	facts = append(facts, chat.RawLine(strings.Join(top, " · ")))
	fact := func(label, value string) chat.Line {
		return chat.LineFrom(
			chat.StyledSpan(fmt.Sprintf("%-11s", label), chat.Muted()),
			chat.RawSpan(value),
		)
	}

	var policy core.SubagentPolicy
	if session.Subagents != nil {
		policy = *session.Subagents
	}
	var delegation string
	switch policy.Mode {
	case core.SubagentPolicyMjolnir:
		label := func(name string, choice *core.SubagentChoice) (string, bool) {
			if choice == nil {
				return "", false
			}
			effort := ""
			if choice.Effort != nil {
				effort = ", " + *choice.Effort
			}
			return name + ": " + choice.Model + effort, true
		}
		var overrides []string
		if text, ok := label("straightforward", policy.Straightforward); ok {
			overrides = append(overrides, text)
		}
		if text, ok := label("subtle", policy.Subtle); ok {
			overrides = append(overrides, text)
		}
		if len(overrides) == 0 {
			delegation = "Mjolnir"
		} else {
			delegation = "Mjolnir (" + strings.Join(overrides, "; ") + ")"
		}
	case core.SubagentPolicyNone:
		delegation = "None"
	default:
		delegation = "Native"
	}
	if total := dashboard.SubagentCountFor(session.ID); total > 0 {
		delegation += fmt.Sprintf(" · %d of %d working", dashboard.WorkingSubagentCountFor(session.ID), total)
	}
	facts = append(facts, fact("Sub-agents", delegation))

	// A full or low filesystem the session writes to changes what every
	// write-bearing action can do.
	for _, filesystem := range dashboard.sessionFilesystems(session.ID) {
		var condition string
		switch filesystem.Filesystem.Condition {
		case core.StorageConditionLow:
			condition = "disk low"
		case core.StorageConditionFull:
			condition = "disk full"
		default:
			continue
		}
		facts = append(facts, fact("Disk", condition+": "+filesystem.View.Explanation(filesystem.Filesystem)))
	}

	checkout := session.RecordCheckout()
	if worktree := checkout.ManagedWorktree(); worktree != nil {
		kind := "Worktree"
		if worktree.Kind == core.ManagedCheckoutKindClone {
			kind = "Clone"
		}
		facts = append(facts, fact("Checkout", kind+" of "+worktree.SourceProjectDirectory))
		branch := worktree.Branch
		if worktree.BaseCommit != nil {
			branch = worktree.Branch + " from " + shortCommit(*worktree.BaseCommit)
		}
		facts = append(facts, fact("Branch", branch))
	} else if directory, ok := checkout.ProjectDirectory(); ok {
		facts = append(facts, fact("Checkout", directory+" (in place)"))
	} else if exact := session.Checkout; exact != nil {
		facts = append(facts, fact("Checkout", exact.RepositoryID+" at "+shortCommit(exact.Commit)))
		if exact.Branch != nil {
			facts = append(facts, fact("Branch", *exact.Branch))
		}
	}

	cpu := cpuSummary(dashboard, session.ID)
	cpuLine := []chat.Span{
		chat.StyledSpan(fmt.Sprintf("%-11s", "CPU"), chat.Muted()),
		cpu.mainSpan(),
	}
	if cpu.detail != nil && cpu.tree {
		facts = append(facts, chat.LineFrom(cpuLine...))
		facts = append(facts, chat.LineFrom(
			chat.RawSpan(strings.Repeat(" ", 11)),
			chat.StyledSpan("("+*cpu.detail+")", chat.Muted()),
		))
	} else {
		if cpu.detail != nil {
			cpuLine = append(cpuLine, chat.StyledSpan(" ("+*cpu.detail+")", chat.Muted()))
		}
		facts = append(facts, chat.LineFrom(cpuLine...))
	}

	var container []string
	if allocation, ok := session.ResourceAllocation.(core.SessionResourceAllocationContainer); ok {
		container = append(container, fmt.Sprintf("%d CPUs", allocation.Cpus))
		container = append(container, memoryGibText(allocation.MemoryBytes)+" GiB memory")
	}
	if len(session.AdditionalMounts) > 0 {
		container = append(container, counted(len(session.AdditionalMounts), "mount", "mounts"))
	}
	if session.BuildCache != nil {
		container = append(container, "build cache")
	}
	if len(container) > 0 {
		facts = append(facts, fact("Container", strings.Join(container, " · ")))
	}
	return facts
}

// shortCommit is the first 10 characters of a commit ID, as `git log
// --oneline` shows it.
func shortCommit(commit string) string {
	if len(commit) >= 10 && (len(commit) == 10 || utf8.RuneStart(commit[10])) {
		return commit[:10]
	}
	return commit
}

// commandDescription is what the palette says a command does, for the
// session it would act on.
//
// Open shows a harness-native child's or a stopped sub-agent's conversation
// without a composer, so it must not promise that you can type in it
// (R11-4).
func commandDescription(dashboard *DashboardState, id CommandId) string {
	if id == CommandIdOpenSession {
		if session, ok := dashboard.commandSessionID(); ok &&
			(dashboard.IsNativeAgent(session) || dashboard.IsStoppedSubagent(session)) {
			return "Show the selected agent's conversation (read-only)."
		}
	}
	return spec(id).description
}

func firstReady(entries []PaletteEntry) int {
	return max(slices.IndexFunc(entries, func(entry PaletteEntry) bool {
		return entry.availability == Availability(AvailabilityReady{})
	}), 0)
}

func sessionMenuLabel(dashboard *DashboardState, palette *CommandPalette, id CommandId) string {
	switch id {
	// Opens a second choice, so it carries the menu's submenu glyph.
	case CommandIdChangeWorkspace:
		return "Change Workspace " + chat.ThemeGlyphs().Navigate
	// One row that flips with the session's preference.
	case CommandIdIgnoreNotifications:
		if palette.sessionID != nil {
			if session := dashboard.state.Get().Sessions.GetShared(*palette.sessionID); session != nil && session.NotificationsIgnored {
				return "Stop ignoring"
			}
		}
		return "Ignore"
	// One row that flips with the pin state, like Ignore's.
	case CommandIdPinSession:
		if palette.sessionID != nil {
			if _, pinned := dashboard.PinID(*palette.sessionID); pinned {
				return "Unpin"
			}
		}
		return "Pin…"
	// Names the child the row's own status line names.
	case CommandIdOpenWaitingQuestion:
		if palette.sessionID != nil {
			if kind, ok := dashboard.waitingChildKind(*palette.sessionID); ok && kind == core.SubagentKindReviewer {
				return "Open reviewer question"
			}
		}
		return "Open sub-agent question"
	case CommandIdOpenSession:
		return "Open"
	case CommandIdRenameSession:
		return "Rename…"
	case CommandIdMoveSession:
		return "Move…"
	case CommandIdSuspendSession:
		return "Suspend…"
	case CommandIdRestartSession:
		return "Restart"
	case CommandIdUpgradeContainer:
		return "Upgrade container and restart…"
	case CommandIdInterruptAll:
		return "Interrupt all (parent + sub-agents)…"
	case CommandIdDestroySession:
		return "Destroy…"
	default:
		return spec(id).label
	}
}

// headingFor is the heading printed above the group an entry opens.
//
// The selected session's group is headed by the session's own name, because
// "Stop" means nothing without saying what it stops.
func headingFor(dashboard *DashboardState, scope Scope) string {
	if scope == ScopeSession {
		if session := dashboard.commandSession(); session != nil {
			if dashboard.goMode != nil {
				return dashboard.GoConversationTitle(session.ID)
			}
			return sessionName(session)
		}
	}
	return scope.heading()
}

// paneScope is the pane group the palette lists after the selected
// session's.
//
// The composer is not a pane, but every Sessions-pane command answers from
// it after the prefix, so typing in a conversation lists the Sessions pane's
// group rather than none at all.
func paneScope(focus Focus) Scope {
	switch focus {
	case FocusWorkspaces:
		return ScopeGlobal
	case FocusTargets:
		return ScopeTargets
	case FocusQuota:
		return ScopeQuota
	case FocusNotifications:
		return ScopeNotifications
	default:
		return ScopeSessions
	}
}

// scopeOrder is the groups the palette walks, in the order it prints them.
func scopeOrder(dashboard *DashboardState) []Scope {
	order := []Scope{ScopeSession, paneScope(dashboard.focus)}
	for _, scope := range []Scope{ScopeSetup, ScopePane, ScopeSettings, ScopeGlobal} {
		if !slices.Contains(order, scope) {
			order = append(order, scope)
		}
	}
	return order
}

// matchScore is how well query matches a command, higher being better, or
// false when it does not match at all.
//
// A label that starts with the query beats everything, then a label that
// carries the query as one run of characters — `rend` in "rendering" —
// because that is what a reader means by a match. Below those the query's
// characters need only appear in the label in order (so `cre` finds "Create
// session" and `mvs` finds "Move session"), scored by how many land on the
// start of a word and how many sit next to each other; letters scattered
// across three words are the weakest kind of label hit and must not outrank
// a run. A query found only in the description ranks last, so a word from
// the description still finds the command without outranking a label hit.
func matchScore(label, description, query string) (uint32, bool) {
	query = strings.ToLower(query)
	labelLower := strings.ToLower(label)
	if query == "" {
		return 0, true
	}
	if strings.HasPrefix(labelLower, query) {
		return 10_000, true
	}
	if index := strings.Index(labelLower, query); index >= 0 {
		// A run that starts a word reads as the word the person typed, so
		// it comes before one buried inside another word.
		previous, size := utf8.DecodeLastRuneInString(labelLower[:index])
		if size == 0 || !paletteIsAlphanumeric(previous) {
			return 5_000 + 30, true
		}
		return 5_000, true
	}
	if score, ok := subsequenceScore(labelLower, query); ok {
		return 1_000 + score, true
	}
	if strings.Contains(strings.ToLower(description), query) {
		return 100, true
	}
	return 0, false
}

// paletteIsAlphanumeric is Rust's char::is_alphanumeric: alphabetic or
// numeric.
func paletteIsAlphanumeric(character rune) bool {
	return unicode.IsLetter(character) || unicode.IsNumber(character)
}

// subsequenceScore is the in-order match of query in text, scored for word
// starts and adjacency, or false when a character of the query never
// appears.
func subsequenceScore(text, query string) (uint32, bool) {
	var score uint32
	previousIndex := -1
	previousChar := ' '
	offset := 0
	for _, wanted := range query {
		for {
			if offset >= len(text) {
				return 0, false
			}
			found, size := utf8.DecodeRuneInString(text[offset:])
			index := offset
			offset += size
			if found == wanted {
				if !paletteIsAlphanumeric(previousChar) {
					score += 30
				}
				if previousIndex >= 0 && previousIndex+utf8.RuneLen(previousChar) == index {
					score += 20
				}
				previousIndex = index
				previousChar = found
				break
			}
			previousChar = found
		}
	}
	// Fewer leftover characters means a tighter match.
	leftover := min(max(len(text)-len(query), 0), 10)
	return score + uint32(10-leftover), true
}

// rank orders the entries by [matchScore], keeping registry order among
// equals so the groups stay together when the query is broad.
func rank(entries []PaletteEntry, query string) []PaletteEntry {
	if query == "" {
		return entries
	}
	type scoredEntry struct {
		score uint32
		entry PaletteEntry
	}
	var scored []scoredEntry
	for _, entry := range entries {
		command := spec(entry.id)
		if score, ok := matchScore(command.label, command.description, query); ok {
			scored = append(scored, scoredEntry{score: score, entry: entry})
		}
	}
	slices.SortStableFunc(scored, func(a, b scoredEntry) int {
		return cmp.Compare(b.score, a.score)
	})
	ranked := make([]PaletteEntry, 0, len(scored))
	for _, item := range scored {
		ranked = append(ranked, item.entry)
	}
	return ranked
}

// paletteEntries is every command the palette would list for query, in
// drawing order.
//
// Hidden commands are left out entirely, as are the commands
// [hiddenFromPalette] names because a visible dashboard control already runs
// them.
func paletteEntries(dashboard *DashboardState, query string) []PaletteEntry {
	var entries []PaletteEntry
	for _, scope := range scopeOrder(dashboard) {
		for index := range commands {
			command := &commands[index]
			if command.scope != scope || hiddenFromPalette(command.id) {
				continue
			}
			availability := command.available(dashboard)
			if availability == Availability(AvailabilityHidden{}) {
				continue
			}
			entries = append(entries, PaletteEntry{id: command.id, availability: availability, recent: false})
		}
	}
	if query == "" {
		// What was run lately leads an unfiltered list; the same commands
		// stay in their own groups below, so nothing moves out of place.
		var recent []PaletteEntry
		for _, id := range dashboard.recentCommands {
			if index := slices.IndexFunc(entries, func(entry PaletteEntry) bool { return entry.id == id }); index >= 0 {
				entry := entries[index]
				entry.recent = true
				recent = append(recent, entry)
			}
		}
		return append(recent, entries...)
	}
	return rank(entries, query)
}

// beginPalette opens the palette over the dashboard.
func (d *DashboardState) beginPalette() {
	entries := paletteEntries(d, "")
	palette := &CommandPalette{
		query:       chat.NewTextInput(),
		entries:     entries,
		selected:    0,
		rankedFor:   "",
		form:        chat.NewDialog[PaletteControl](),
		sessionOnly: false,
	}
	if id, ok := d.commandSessionID(); ok {
		palette.sessionID = &id
	}
	palette.prepare()
	d.mode = ModePalette{palette}
}

func (d *DashboardState) beginSessionPalette() {
	var sessionID, sessionTitle *string
	if id, ok := d.commandSessionID(); ok {
		sessionID = &id
	}
	if session := d.commandSession(); session != nil {
		var title string
		if d.goMode != nil {
			title = d.GoConversationTitle(session.ID)
		} else {
			title = sessionName(session)
		}
		sessionTitle = &title
	}
	entries := sessionMenuEntries(d)
	palette := &CommandPalette{
		query:        chat.NewTextInput(),
		selected:     firstReady(entries),
		entries:      entries,
		rankedFor:    "",
		form:         chat.NewDialog[PaletteControl](),
		sessionOnly:  true,
		sessionID:    sessionID,
		sessionTitle: sessionTitle,
	}
	palette.prepare()
	d.mode = ModePalette{palette}
}

// beginSessionPaletteFor opens a row's menu for that session without
// changing the conversation.
func (d *DashboardState) beginSessionPaletteFor(sessionID string) {
	saved := d.commandSessionOverride
	d.commandSessionOverride = &sessionID
	d.beginSessionPalette()
	d.commandSessionOverride = saved
}

// rebuildPaletteEntries refreshes query matches and availability.
//
// A new query is a new list, so the cursor starts on its top match: Enter
// runs the row the cursor is on, and a cursor that followed the command it
// happened to sit on — the one the Recent group leads with, say — would run
// a command the ranking had moved to the bottom. A rebuild under the same
// query is only availability moving, so there the cursor stays put.
func (d *DashboardState) rebuildPaletteEntries() {
	palette, ok := d.mode.(ModePalette)
	if !ok {
		return
	}
	query := palette.query.Value()
	sessionOnly := palette.sessionOnly
	sessionID := workspacesCloneString(palette.sessionID)
	oldSelected := palette.selected
	var oldID *CommandId
	if oldSelected >= 0 && oldSelected < len(palette.entries) {
		id := palette.entries[oldSelected].id
		oldID = &id
	}
	savedOverride := d.commandSessionOverride
	if sessionOnly {
		d.commandSessionOverride = sessionID
	}
	var entries []PaletteEntry
	if sessionOnly {
		entries = sessionMenuEntries(d)
	} else {
		entries = paletteEntries(d, query)
	}
	d.commandSessionOverride = savedOverride
	palette, ok = d.mode.(ModePalette)
	if !ok {
		return
	}
	sameQuery := palette.rankedFor == query
	if sameQuery && slices.Equal(palette.entries, entries) {
		return
	}
	ready := func(index int) bool {
		return index >= 0 && index < len(entries) && entries[index].availability == Availability(AvailabilityReady{})
	}
	switch {
	case sessionOnly:
		anchor := min(oldSelected, max(len(entries)-1, 0))
		if oldID != nil {
			if index := slices.IndexFunc(entries, func(entry PaletteEntry) bool { return entry.id == *oldID }); index >= 0 {
				anchor = index
			}
		}
		selected := -1
		if ready(anchor) {
			selected = anchor
		}
		for index := anchor + 1; selected < 0 && index < len(entries); index++ {
			if ready(index) {
				selected = index
			}
		}
		for index := min(anchor, len(entries)) - 1; selected < 0 && index >= 0; index-- {
			if ready(index) {
				selected = index
			}
		}
		palette.selected = max(selected, 0)
	case sameQuery:
		selected := 0
		if palette.selected >= 0 && palette.selected < len(palette.entries) {
			id := palette.entries[palette.selected].id
			if index := slices.IndexFunc(entries, func(entry PaletteEntry) bool { return entry.id == id }); index >= 0 {
				selected = index
			}
		}
		palette.selected = selected
	default:
		palette.selected = 0
	}
	palette.rankedFor = query
	palette.form.CancelPointer()
	palette.entries = entries
	palette.prepare()
}

func (d *DashboardState) handlePaletteEvent(event tea.Msg) DashboardAction {
	if palette, ok := d.mode.(ModePalette); ok && palette.workspacePopup != nil {
		return d.handleWorkspacePopupEvent(event)
	}
	palette, ok := d.mode.(ModePalette)
	if !ok {
		return DashboardActionNone{}
	}
	// Search palettes let arrows browse results while typing remains in the
	// query.
	var browse *tea.KeyPressMsg
	if press, ok := event.(tea.KeyPressMsg); ok && palette.form.IsFocused(PaletteControlQuery) {
		code := chat.KeyPressCode(press)
		control := press.Mod&^(tea.ModCapsLock|tea.ModNumLock|tea.ModScrollLock) == tea.ModCtrl
		switch {
		case code == tea.KeyUp || code == tea.KeyDown:
			browse = &tea.KeyPressMsg{Code: code}
		case code == 'p' && control:
			browse = &tea.KeyPressMsg{Code: tea.KeyUp}
		case code == 'n' && control:
			browse = &tea.KeyPressMsg{Code: tea.KeyDown}
		// The query is a text field, so Ctrl-U and Ctrl-D stay readline's
		// kill-to-line-start and delete-forward while there is text to edit.
		// The list borrows them for paging only once the query is empty and
		// they would do nothing.
		case (code == 'd' || code == 'u') && control && palette.query.IsEmpty():
			browse = &press
		}
	}
	var interaction chat.Interaction[PaletteControl]
	if browse != nil {
		form := &palette.form
		form.Focus(PaletteControlCommands)
		result := form.Handle(*browse)
		form.Focus(PaletteControlQuery)
		d.lastEventConsumed = result.Consumed
		interaction = result.Action
	} else {
		result := palette.form.Handle(event)
		d.lastEventConsumed = result.Consumed
		interaction = result.Action
	}
	switch interaction := interaction.(type) {
	case chat.InteractionCancel[PaletteControl]:
		d.CancelModal()
	case chat.InteractionActivate[PaletteControl]:
		if interaction.ID == PaletteControlCopySessionId {
			sessionID := workspacesCloneString(palette.sessionID)
			d.mode = ModeDashboard{}
			if sessionID == nil {
				return DashboardActionNone{}
			}
			return DashboardActionCopySessionId{SessionID: *sessionID}
		}
		if interaction.ID != PaletteControlQuery && interaction.ID != PaletteControlCommands && interaction.ID != PaletteControlRun {
			break
		}
		if selected, ok := palette.form.Selected(PaletteControlCommands); ok {
			palette.selected = selected
		}
		if palette.selected < 0 || palette.selected >= len(palette.entries) {
			return DashboardActionNone{}
		}
		entry := palette.entries[palette.selected]
		if blocked, ok := entry.availability.(AvailabilityBlocked); ok {
			d.notices.Set(fmt.Sprintf("%s is unavailable: %s.", spec(entry.id).label, blocked.Reason))
			return DashboardActionNone{}
		}
		if entry.id == CommandIdChangeWorkspace {
			// The choice opens on the row, inside the palette.
			d.rememberCommand(entry.id)
			d.openWorkspacePopup()
			return DashboardActionNone{}
		}
		if spec(entry.id).scope == ScopeSession {
			if palette.sessionID == nil || !d.state.Get().Sessions.ContainsKey(*palette.sessionID) {
				d.SetNotice("This session is no longer available.")
				return DashboardActionNone{}
			}
			id := *palette.sessionID
			d.commandSessionOverride = &id
		}
		d.mode = ModeDashboard{}
		// Everything run from here is listed under Recent, including
		// commands that also have a pane key.
		d.rememberCommand(entry.id)
		action := d.runAvailableCommand(entry.id)
		d.commandSessionOverride = nil
		return action
	case chat.InteractionEdit[PaletteControl]:
		if interaction.ID == PaletteControlQuery {
			chat.TextFieldApply(&palette.query, interaction.Edit)
			d.rebuildPaletteEntries()
		}
	case chat.InteractionSelect[PaletteControl]:
		if interaction.ID == PaletteControlCommands && palette.selected != interaction.Index {
			palette.selected = interaction.Index
		}
	}
	return DashboardActionNone{}
}

// PaletteLine is one drawn row: either a group heading or a command.
type PaletteLine interface {
	isPaletteLine()
}

// PaletteLineHeading is a group heading.
type PaletteLineHeading string

// PaletteLineBlank is an empty row between session-menu sections.
type PaletteLineBlank struct{}

// PaletteLineCommand is the entry's index into entries, so the highlight
// can be placed.
type PaletteLineCommand int

func (PaletteLineHeading) isPaletteLine() {}
func (PaletteLineBlank) isPaletteLine()   {}
func (PaletteLineCommand) isPaletteLine() {}

// paletteLines is the rows the palette draws, with a heading wherever the
// group changes.
func paletteLines(dashboard *DashboardState, palette *CommandPalette) []PaletteLine {
	var lines []PaletteLine
	if palette.sessionOnly {
		section := -1
		for index, entry := range palette.entries {
			var next int
			switch entry.id {
			case CommandIdOpenSession, CommandIdChangedFiles, CommandIdDismissNotification,
				CommandIdOpenWaitingQuestion, CommandIdIgnoreNotifications:
				next = 0
			case CommandIdRenameSession, CommandIdPinSession, CommandIdChangeWorkspace:
				next = 1
			case CommandIdContainerSettings, CommandIdMoveSession, CommandIdSuspendSession,
				CommandIdRestartSession, CommandIdUpgradeContainer, CommandIdInterruptAll,
				CommandIdDestroySession:
				next = 2
			default:
				continue
			}
			if section != next {
				// An empty row sets one section off from the last.
				if section >= 0 {
					lines = append(lines, PaletteLineBlank{})
				}
				switch next {
				case 0:
					lines = append(lines, PaletteLineHeading("Content"))
				case 1:
					lines = append(lines, PaletteLineHeading("Organize"))
				default:
					lines = append(lines, PaletteLineHeading("Lifecycle"))
				}
				section = next
			}
			lines = append(lines, PaletteLineCommand(index))
		}
		return lines
	}
	// A nil group is the Recent group, which has no scope of its own; the
	// first row always opens a group.
	started := false
	var previous *Scope
	for index, entry := range palette.entries {
		var group *Scope
		if !entry.recent {
			scope := spec(entry.id).scope
			group = &scope
		}
		if !started || (previous == nil) != (group == nil) || (group != nil && *previous != *group) {
			if group != nil {
				lines = append(lines, PaletteLineHeading(headingFor(dashboard, *group)))
			} else {
				lines = append(lines, PaletteLineHeading("Recent"))
			}
			previous = group
			started = true
		}
		lines = append(lines, PaletteLineCommand(index))
	}
	return lines
}

func renderPalette(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, palette *CommandPalette, surfaces *chat.FrameSurfaces) {
	lines := paletteLines(dashboard, palette)
	var facts []chat.Line
	if palette.sessionOnly && palette.sessionID != nil &&
		// A harness-native child's row is presentation only; it has no
		// record of its own to describe.
		!dashboard.IsNativeAgent(*palette.sessionID) {
		if session := dashboard.state.Get().Sessions.GetShared(*palette.sessionID); session != nil {
			facts = paletteSessionFacts(dashboard, session)
		}
	}
	// The facts are followed by one blank row that sets them apart from the
	// commands.
	factsHeight := 0
	if len(facts) > 0 {
		factsHeight = len(facts) + 1
	}
	// The popup grows with the complete list. The shared modal helper clamps
	// it to the usable terminal bounds when the list cannot fit.
	extraHeight := factsHeight + 5
	popupHeight := min(max(len(lines)+extraHeight, 5), math.MaxUint16)
	popup := centeredModal(frame, surfaces, 72, popupHeight, area)
	inner := chat.Modal().Inner(popup)
	var rows []uv.Rectangle
	if palette.sessionOnly {
		// One blank row of margin at the top and at the bottom.
		rows = chat.VerticalLayout(
			chat.ConstraintLength(1),
			chat.ConstraintLength(min(factsHeight, math.MaxUint16)),
			chat.ConstraintMin(1),
			chat.ConstraintLength(1),
			chat.ConstraintLength(1),
		).Split(inner)
	} else {
		rows = chat.VerticalLayout(
			chat.ConstraintLength(1),
			chat.ConstraintMin(1),
			chat.ConstraintLength(1),
			chat.ConstraintLength(1),
		).Split(inner)
	}
	listRow, descriptionRow := rows[1], rows[2]
	if palette.sessionOnly {
		listRow, descriptionRow = rows[2], rows[3]
	}
	if len(facts) > 0 {
		factsRow := uv.Rect(
			rows[1].Min.X+2,
			rows[1].Min.Y,
			max(rows[1].Dx()-2, 0),
			max(rows[1].Dy()-1, 0),
		)
		frame.RenderWidget(chat.NewParagraph(chat.TextFrom(facts...)), factsRow)
	}

	form := &palette.form
	form.BeginFrame()
	form.SetBounds(popup)
	title := chat.ThemeGlyphs().Spark + " Commands"
	if palette.sessionTitle != nil {
		title = FitSessionName(*palette.sessionTitle, max(popup.Dx()-8, 0))
	}
	titleLine := dismissibleModalTitle(&form.Form, popup, title, chat.Title(true), true)
	outer := chat.Modal().WithTitle(titleLine)
	if !palette.sessionOnly {
		outer = outer.WithTitle(
			chat.StyledLine(" "+counted(len(palette.entries), "command", "commands")+" ", chat.Muted()).RightAligned(),
		)
	}
	outer = outer.WithTitleBottom(chat.DialogShellHints(true))
	frame.RenderWidget(outer, popup)
	if palette.sessionOnly {
		// The session id copies from an ordinary button in the body's first
		// row, at the right. That row is the menu's top margin: the title
		// sits on the border above it and the facts start in the row below,
		// so the button cannot overlap either. It leaves the command list to
		// lifecycle and content work.
		enabled := spec(CommandIdCopySessionId).available(dashboard) == Availability(AvailabilityReady{})
		form.RenderActionsAligned(
			frame,
			rows[0],
			[]chat.ButtonSpec[PaletteControl]{{ID: PaletteControlCopySessionId, Label: "Copy session ID", Enabled: enabled}},
			chat.RowAlignRight,
		)
	} else {
		chat.TextFieldRender(frame, rows[0], &palette.query, &form.Form, PaletteControlQuery)
		if palette.query.IsEmpty() {
			frame.RenderWidget(
				chat.StyledLine("Search commands"+chat.ThemeGlyphs().Ellipsis, chat.Muted().Italic()),
				rows[0],
			)
		}
	}

	// Keep the rail outside the list's registered area. Besides leaving the
	// command text untouched, this means clicking the scrollbar cannot be
	// interpreted as clicking a command row.
	listArea := uv.Rect(listRow.Min.X, listRow.Min.Y, max(listRow.Dx()-1, 0), listRow.Dy())
	scrollbarArea := uv.Rect(
		listRow.Min.X+listArea.Dx(),
		listRow.Min.Y,
		max(listRow.Dx()-listArea.Dx(), 0),
		listRow.Dy(),
	)
	width := max(listArea.Dx()-1, 0)
	rowMap := make([]*int, 0, len(lines))
	enabled := make([]bool, 0, len(lines))
	items := make([]chat.Line, 0, len(lines))
	for _, line := range lines {
		switch line := line.(type) {
		case PaletteLineHeading:
			rowMap = append(rowMap, nil)
			enabled = append(enabled, false)
			heading := truncateToCells(string(line), max(width-4, 0), TruncatePlain)
			ruleWidth := max(width-(utf8.RuneCountInString(heading)+4), 0)
			items = append(items, chat.LineFrom(
				chat.StyledSpan("  "+heading+"  ", chat.Title(true)),
				chat.StyledSpan(strings.Repeat(chat.ThemeGlyphs().Rule, ruleWidth), chat.Border(false)),
			))
		case PaletteLineBlank:
			rowMap = append(rowMap, nil)
			enabled = append(enabled, false)
			items = append(items, chat.Line{})
		case PaletteLineCommand:
			index := int(line)
			rowMap = append(rowMap, &index)
			entry := palette.entries[index]
			ready := entry.availability == Availability(AvailabilityReady{})
			enabled = append(enabled, ready)
			command := spec(entry.id)
			keys := strings.Join(dashboard.keyLabels(entry.id), " / ")
			// The same form help uses: its own sentence after a dash,
			// dimmed, not a lowercase fragment in brackets.
			reason := ""
			if blocked, ok := entry.availability.(AvailabilityBlocked); ok {
				reason = " — " + sentence(blocked.Reason)
			}
			selected := index == palette.selected
			// Keep command labels aligned and reserve a visible gap before
			// right-aligned shortcuts, even when a chord has several keys.
			keys = truncateToCells(keys, max(width-4, 0)/2, TruncatePlain)
			keyWidth := chat.RawLine(keys).Width()
			labelWidth := max(width-(keyWidth+4), 0)
			label := command.label
			if palette.sessionOnly {
				label = sessionMenuLabel(dashboard, palette, entry.id)
			}
			text := truncateToCells(label+reason, labelWidth, TruncatePlain)
			var style chat.Style
			switch {
			case !ready:
				style = chat.Muted()
			case selected:
				style = chat.Style{}.Foreground(chat.ThemePalette().Text).Bold()
			default:
				style = chat.Style{}.Foreground(chat.ThemePalette().Text)
			}
			padding := max(labelWidth-chat.RawLine(text).Width(), 0) + 2
			split := len(text)
			if strings.HasPrefix(text, label) {
				split = len(label)
			}
			labelText, reasonText := text[:split], text[split:]
			marker := "  "
			if selected {
				marker = chat.ThemeGlyphs().Selected
			}
			keyColor := chat.ThemePalette().Muted
			if ready {
				keyColor = chat.ThemePalette().Secondary
			}
			items = append(items, chat.LineFrom(
				chat.StyledSpan(marker, chat.Title(true)),
				chat.StyledSpan(labelText, style),
				chat.StyledSpan(reasonText, style.Dim()),
				chat.RawSpan(strings.Repeat(" ", padding)),
				chat.StyledSpan(keys, chat.Style{}.Foreground(keyColor)),
			))
		}
	}
	if len(items) == 0 {
		frame.RenderWidget(chat.RawLine("No matching command"), listArea)
		form.Register(PaletteControlCommands, chat.ControlKindChoiceList{Len: 0, Selected: 0}, listArea, false)
	} else {
		chat.ChoiceListRenderWithRows(
			frame,
			listArea,
			items,
			palette.selected,
			rowMap,
			enabled,
			&form.Form,
			PaletteControlCommands,
		)
	}
	renderSessionScrollbar(
		frame,
		scrollbarArea,
		len(items),
		form.ListOffset(PaletteControlCommands),
		max(listArea.Dy(), 1),
	)
	anchorRow, anchored := 0, false
	if palette.workspacePopup != nil {
		line := slices.IndexFunc(lines, func(line PaletteLine) bool {
			command, ok := line.(PaletteLineCommand)
			return ok && palette.entries[command].id == CommandIdChangeWorkspace
		})
		if row := line - form.ListOffset(PaletteControlCommands); line >= 0 && row >= 0 && row < listArea.Dy() {
			anchorRow, anchored = listArea.Min.Y+row, true
		}
	}
	if !palette.sessionOnly {
		form.RenderActions(
			frame,
			rows[3],
			[]chat.ButtonSpec[PaletteControl]{{ID: PaletteControlRun, Label: "Run", Enabled: palette.selectedReady()}},
		)
	}
	// Drawn after every other control so the list lies over the rows below.
	if palette.workspacePopup != nil && anchored {
		row := uv.Rect(listArea.Min.X+2, anchorRow, max(listArea.Dx()-2, 0), 1)
		palette.workspacePopup.render(frame, area, row, palette.sessionOnly, form)
	}
	if palette.sessionOnly {
		form.EndFrame(PaletteControlCommands)
	} else {
		form.EndFrame(PaletteControlQuery)
	}

	description := "Try a command name or a word from its description."
	if palette.selected >= 0 && palette.selected < len(palette.entries) {
		description = commandDescription(dashboard, palette.entries[palette.selected].id)
	}
	frame.RenderWidget(chat.NewParagraph(chat.RawText(description)).WithStyle(chat.Muted()), descriptionRow)
}
