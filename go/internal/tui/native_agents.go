// Port of mj-tui/src/native_agents.rs.

package tui

import (
	"cmp"
	"fmt"
	"reflect"
	"slices"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// NativeAgentPane is a harness-native sub-agent's read-only conversation.
type NativeAgentPane struct {
	Agent          core.NativeAgent
	transcript     *chat.TranscriptSnapshot
	projection     core.MaterializedSession
	generation     uint64
	loadingHistory bool
	hasMore        bool
	scroll         int
	Stopping       bool
}

func (d *DashboardState) IsNativeAgent(id string) bool {
	_, ok := (*d.nativeAgents.Get())[id]
	return ok
}

func (d *DashboardState) subagentParentFor(id string) (string, bool) {
	if pane, ok := (*d.nativeAgents.Get())[id]; ok {
		return pane.Agent.ParentViewID(), true
	}
	if agent := d.state.Get().Subagents.GetShared(id); agent != nil {
		return agent.ParentSessionID, true
	}
	return "", false
}

func (d *DashboardState) WorkingSubagentCountFor(parent string) int {
	return d.verdict(parent).WorkingChildren
}

func (d *DashboardState) SubagentCountFor(parent string) int {
	return len(d.managedChildIDsOfKind(parent, core.SubagentKindWorker)) +
		len((*d.nativeByParent.Get())[parent])
}

func (d *DashboardState) SetNativeAgents(views []core.NativeAgentView) {
	snapshot := core.NewSnapshotMap[string, core.NativeAgentView]()
	for _, view := range views {
		snapshot.Insert(view.Agent.ViewID(), view)
	}
	d.SetNativeAgentSnapshot(snapshot)
}

// nativeAgentViewEqual is NativeAgentView's derived PartialEq.
func nativeAgentViewEqual(a, b *core.NativeAgentView) bool { return reflect.DeepEqual(a, b) }

// nativeIndexRemove removes id from the set at key, dropping the set once it
// is empty.
func nativeIndexRemove(index map[string]map[string]struct{}, key, id string) {
	ids, ok := index[key]
	if !ok {
		return
	}
	delete(ids, id)
	if len(ids) == 0 {
		delete(index, key)
	}
}

// nativeIndexInsert adds id to the set at key, creating the set.
func nativeIndexInsert(index map[string]map[string]struct{}, key, id string) {
	ids, ok := index[key]
	if !ok {
		ids = map[string]struct{}{}
		index[key] = ids
	}
	ids[id] = struct{}{}
}

func (d *DashboardState) SetNativeAgentSnapshot(views core.SnapshotMap[string, core.NativeAgentView]) {
	type change struct {
		id   string
		view *core.NativeAgentView
	}
	var changes []change
	for id, view := range d.nativeSources.Changes(views, nativeAgentViewEqual) {
		changes = append(changes, change{id: id, view: view})
	}
	if len(changes) == 0 {
		return
	}
	d.nativeSources = views
	if d.nativeRunningByParent == nil {
		d.nativeRunningByParent = map[string]map[string]struct{}{}
	}
	if d.nativeByOwner == nil {
		d.nativeByOwner = map[string]map[string]struct{}{}
	}
	for _, change := range changes {
		id := change.id
		if old, ok := (*d.nativeAgents.Get())[id]; ok {
			parent := old.Agent.ParentViewID()
			nativeIndexRemove(*d.nativeByParent.Mut(), parent, id)
			nativeIndexRemove(d.nativeRunningByParent, parent, id)
			nativeIndexRemove(d.nativeByOwner, old.Agent.OwnerSessionID, id)
		}
		if change.view == nil {
			d.state.Mut().Sessions.Remove(id)
			delete(*d.nativeAgents.Mut(), id)
			delete(*d.sessionDetails.Mut(), id)
			delete(*d.projectSources.Mut(), id)
			continue
		}
		view := *change.view
		byParent := d.nativeByParent.Mut()
		if *byParent == nil {
			*byParent = map[string]map[string]struct{}{}
		}
		nativeIndexInsert(*byParent, view.Agent.ParentViewID(), id)
		nativeIndexInsert(d.nativeByOwner, view.Agent.OwnerSessionID, id)
		if view.Agent.State == core.NativeAgentStateRunning {
			nativeIndexInsert(d.nativeRunningByParent, view.Agent.ParentViewID(), id)
		}
		// These records exist only in the presentation model. The controller
		// never sees a native child as a provisionable session.
		if row, ok := d.state.Get().Sessions.Get(view.Agent.OwnerSessionID); ok {
			row.ID = id
			// Native forms notify through a managed session's card.
			row.NotificationsIgnored = true
			row.Title = view.Agent.Name
			override := fmt.Sprintf("%s · %s", view.Agent.Name, view.Agent.LifecycleLabel())
			row.SessionTitleOverride = &override
			row.ACPSessionTitle = nil
			if view.Agent.State == core.NativeAgentStateRunning {
				row.State = core.SessionStateRunning
			} else {
				row.State = core.SessionStateStopped
			}
			row.LastError = nil
			d.state.Mut().Sessions.Insert(id, row)
		}
		agents := d.nativeAgents.Mut()
		if *agents == nil {
			*agents = map[string]*NativeAgentPane{}
		}
		previous, found := (*agents)[id]
		delete(*agents, id)
		var pane *NativeAgentPane
		if found && previous.generation == view.GenerationOrdinal {
			if previous.projection.AppliedEventOrdinal != view.Projection.AppliedEventOrdinal {
				oldItems := previous.projection.Transcript
				previous.projection = view.Projection
				previous.projection.Transcript = slices.Clone(view.Projection.Transcript)
				mergeNativeItems(&previous.projection, oldItems)
				previous.transcript = chat.TranscriptSnapshotFromMaterialized(&previous.projection)
			}
			previous.Agent = view.Agent
			pane = previous
		} else {
			projection := view.Projection
			projection.Transcript = slices.Clone(view.Projection.Transcript)
			pane = &NativeAgentPane{
				transcript: chat.TranscriptSnapshotFromMaterialized(&projection),
				projection: projection,
				generation: view.GenerationOrdinal,
				Agent:      view.Agent,
				hasMore:    true,
			}
		}
		pane.Stopping = pane.Stopping && pane.Agent.State == core.NativeAgentStateRunning
		d.setNativeRowExcerpt(id, pane.projection.Transcript)
		(*d.nativeAgents.Mut())[id] = pane
	}
	d.clampSelections()
}

// setNativeRowExcerpt gives a native child's row the excerpt other rows get
// from their stored summary. A native child has no summary of its own, so
// without this its row read "No messages yet" whatever it had said (R10-2).
func (d *DashboardState) setNativeRowExcerpt(id string, transcript []*core.TranscriptItem) {
	none := MaterializedProjectionCache{}
	details := d.sessionDetails.Mut()
	if *details == nil {
		*details = map[string]*SessionDetail{}
	}
	detail, ok := (*details)[id]
	if !ok {
		detail = &SessionDetail{}
		(*details)[id] = detail
	}
	detail.lastAgentMessage = nil
	if found := lastAgentMessage(transcript, 0, &none); found != nil {
		text := found.text
		detail.lastAgentMessage = &text
	}
	detail.latestAgentActivityAfterLastUser = nil
	if found := latestAgentActivity(transcript, 0, &none); found != nil {
		text := found.text
		detail.latestAgentActivityAfterLastUser = &text
	}
	detail.lastAgentMessageFollowsLastUser = detail.lastAgentMessage != nil
}

// NativeAgentHistoryLoaded applies a page of older native-agent history; err
// is Rust's Err(String), nil for Ok(page).
func (d *DashboardState) NativeAgentHistoryLoaded(owner string, child string, page core.NativeAgentHistoryPage, err error) {
	pane, ok := (*d.nativeAgents.Mut())[core.ViewID(owner, child)]
	if !ok {
		return
	}
	pane.loadingHistory = false
	switch {
	case err != nil:
		d.SetNotice(fmt.Sprintf("Could not load native agent history: %v", err))
	case page.GenerationOrdinal == pane.generation:
		pane.hasMore = page.HasMore
		mergeNativeItems(&pane.projection, page.Items)
		pane.transcript = chat.TranscriptSnapshotFromMaterialized(&pane.projection)
	}
}

// NativeAgentStopFinished applies the result of a native-agent stop; err is
// Rust's Err(String), nil for Ok.
func (d *DashboardState) NativeAgentStopFinished(owner string, child string, err error) {
	if pane, ok := (*d.nativeAgents.Mut())[core.ViewID(owner, child)]; ok && err != nil {
		pane.Stopping = false
	}
	if err != nil {
		d.SetNotice(fmt.Sprintf("Could not stop native agent: %v", err))
	}
}

// nativeAgentKey handles a key for a native agent's row or pane; nil when the
// key is not the pane's.
func (d *DashboardState) nativeAgentKey(key tea.KeyMsg) DashboardAction {
	var id string
	var ok bool
	switch {
	case d.focus == FocusSessions:
		if filter := *d.sessionsFilter.Get(); filter != nil && filter.Editing {
			return nil
		}
		id, ok = d.SelectedSessionID()
	case d.focus == FocusPrompt:
		id, ok = d.CurrentSessionID()
	}
	if !ok {
		return nil
	}
	hasChildren := d.SubagentCountFor(id) > 0
	pane, ok := (*d.nativeAgents.Mut())[id]
	if !ok {
		return nil
	}
	pressed := key.Key()
	code := chat.KeyPressCode(tea.KeyPressMsg(pressed))
	plain := pressed.Mod&^(tea.ModCapsLock|tea.ModNumLock|tea.ModScrollLock) == 0
	switch {
	case code == tea.KeyPgUp:
		pane.scroll += 20
		if !pane.loadingHistory && pane.hasMore {
			pane.loadingHistory = true
			var before *client.DaemonPair[uint64, string]
			if len(pane.projection.Transcript) > 0 {
				first := pane.projection.Transcript[0]
				before = &client.DaemonPair[uint64, string]{First: first.Position, Second: first.StableID}
			}
			return DashboardActionLoadNativeAgentHistory{
				Owner:  pane.Agent.OwnerSessionID,
				Child:  pane.Agent.SessionID,
				Before: before,
			}
		}
	case code == tea.KeyPgDown:
		pane.scroll = max(pane.scroll-20, 0)
	case code == tea.KeyEnd:
		pane.scroll = 0
	case code == 'p' && plain:
		parent := pane.Agent.ParentViewID()
		next := d.childWorkspaceFor(parent)
		d.switchChildScope(next)
		d.SelectActiveSession(parent)
		return DashboardActionOpen{SessionID: parent}
	case (code == tea.KeyRight || code == tea.KeyEnter) && hasChildren:
		d.OpenSubagentWorkspace(id)
	// With no children of its own, Enter on the row opens the child's
	// conversation, as it does for any other row (R10-2).
	case code == tea.KeyEnter && d.focus == FocusSessions:
		return DashboardActionOpen{SessionID: id}
	case code == tea.KeyRight || code == tea.KeyEnter:
	case code == 's' && plain && pane.Agent.Capabilities.Cancel &&
		pane.Agent.State == core.NativeAgentStateRunning && !pane.Stopping:
		pane.Stopping = true
		return DashboardActionStopNativeAgent{
			Owner: pane.Agent.OwnerSessionID,
			Child: pane.Agent.SessionID,
		}
	default:
		return nil
	}
	return DashboardActionNone{}
}

func (d *DashboardState) renderNativeAgent(frame *chat.Frame, paneID PaneId, id string, transcriptArea uv.Rectangle, promptArea uv.Rectangle) {
	children := ""
	if d.SubagentCountFor(id) > 0 {
		children = " · Enter: children"
	}
	pane, ok := (*d.nativeAgents.Get())[id]
	if !ok {
		return
	}
	agent := &pane.Agent
	title := childPaneTitle(d, paneID, transcriptArea.Dx(), agent.Name,
		[]string{agent.ActivityLabel(), agent.Availability.Label()})
	tools := chat.ToolDisplayFromConfig(&d.config.Get().Advanced)
	pane, ok = (*d.nativeAgents.Mut())[id]
	if !ok {
		return
	}
	block := chat.NewBlock().WithBorders(chat.BordersAll).WithTitle(title)
	inner := block.Inner(transcriptArea)
	frame.RenderWidget(block, transcriptArea)
	lines, scroll := pane.transcript.RichTailScrolled(inner.Dx(), inner.Dy(), pane.scroll, tools)
	pane.scroll = scroll
	if len(pane.projection.Transcript) == 0 {
		frame.RenderWidget(chat.NewParagraph(chat.RawText("No child transcript available yet.")), inner)
	} else {
		frame.RenderWidget(chat.NewParagraph(chat.TextFrom(lines...)), inner)
	}
	var controls string
	switch {
	case pane.Stopping:
		controls = "Suspending…"
	case pane.Agent.Capabilities.Cancel && pane.Agent.State == core.NativeAgentStateRunning:
		controls = fmt.Sprintf("s: stop · PgUp/PgDn: scroll%s · p: parent", children)
	default:
		controls = fmt.Sprintf("PgUp/PgDn: scroll%s · p: parent · controlled by parent", children)
	}
	frame.RenderWidget(
		chat.NewParagraph(chat.RawText(pane.Agent.Task+"\n"+controls)).
			WithBlock(chat.NewBlock().WithBorders(chat.BordersAll).WithTitle(chat.RawLine("Native agent"))),
		promptArea,
	)
}

// mergeNativeItems merges older transcript items into the projection, by
// stable id, ordered by position then stable id.
func mergeNativeItems(projection *core.MaterializedSession, older []*core.TranscriptItem) {
	items := make(map[string]*core.TranscriptItem, len(older)+len(projection.Transcript))
	for _, item := range older {
		items[item.StableID] = item
	}
	for _, item := range projection.Transcript {
		items[item.StableID] = item
	}
	merged := make([]*core.TranscriptItem, 0, len(items))
	for _, item := range items {
		merged = append(merged, item)
	}
	slices.SortFunc(merged, func(a, b *core.TranscriptItem) int {
		if c := cmp.Compare(a.Position, b.Position); c != 0 {
			return c
		}
		return cmp.Compare(a.StableID, b.StableID)
	})
	projection.Transcript = merged
}
