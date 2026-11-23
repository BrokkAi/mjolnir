// Port of mj-tui/src/go.rs.

// The optional fast-start workflow. All IO remains in the CLI supervisor.

package tui

import (
	"fmt"
	"maps"
	"slices"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const simpleTargetID = "localhost"

// GoMode is the fast-start workflow's state for one workspace.
type GoMode struct {
	WorkspaceID   *string
	LastSessionID *string
	Directory     string
	Recipe        *core.GoRecipe
	SaveAsDefault bool
	// Simple: `mj simple` fixes the target, checkout, review, and key
	// prefix. The startup state machine uses this marker rather than
	// inferring those invariants from a recipe that a normal `mj go` user
	// may edit.
	Simple bool
}

// RememberedGoLaunch is what RememberGoLaunch hands the host to save: the
// directory the recipe belongs to, the recipe, and whether it becomes the
// default.
type RememberedGoLaunch struct {
	Directory     string
	Recipe        core.GoRecipe
	SaveAsDefault bool
}

// SetLaunchProjectDirectory records the repository mj was started in, so a
// new local session starts with it as its project directory.
func (d *DashboardState) SetLaunchProjectDirectory(directory *string) {
	d.launchProjectDirectory = directory
}

func (d *DashboardState) RegisterGoWorkspaces(modes []GoMode) {
	for _, mode := range modes {
		if mode.WorkspaceID == nil {
			continue
		}
		if d.goWorkspaces == nil {
			d.goWorkspaces = map[string]GoMode{}
		}
		d.goWorkspaces[*mode.WorkspaceID] = mode
	}
}

func (d *DashboardState) switchGoWorkspace(workspaceID *string) {
	if mode := d.goMode; mode != nil && mode.WorkspaceID != nil {
		mode.LastSessionID = nil
		if selected, ok := d.SelectedSessionID(); ok {
			mode.LastSessionID = &selected
		}
		if d.goWorkspaces == nil {
			d.goWorkspaces = map[string]GoMode{}
		}
		d.goWorkspaces[*mode.WorkspaceID] = *mode
	}
	d.goMode = nil
	if workspaceID != nil {
		if mode, ok := d.goWorkspaces[*workspaceID]; ok {
			d.goMode = &mode
		}
	}
}

// SetGoContext records where a session's checkout is and on which branch
// (First and Second), or why that could not be read.
func (d *DashboardState) SetGoContext(sessionID string, result client.DaemonResult[client.DaemonPair[string, string]]) {
	if d.goContexts == nil {
		d.goContexts = map[string]client.DaemonResult[client.DaemonPair[string, string]]{}
	}
	d.goContexts[sessionID] = result
}

// BeginGo starts the fast-start workflow. startup is the session the daemon
// chose for this workspace (DaemonClient.GoStartupSession): the remembered
// one while it is still eligible, otherwise the newest.
func (d *DashboardState) BeginGo(mode GoMode, setup bool, startup *core.SessionRecord) DashboardAction {
	if mode.Simple {
		d.simpleMode = true
		d.prefixPending = false
		d.keybinds = core.SimpleKeybinds()
	}
	needsRemotePath := false
	if recipe := mode.Recipe; recipe != nil {
		if target, ok := d.config.Get().Targets[recipe.TargetID]; ok {
			_, sshBare := target.(core.TargetTemplateSshBare)
			needsRemotePath = sshBare && recipe.ProjectDirectory == nil
		}
	}
	d.goMode = &mode
	d.FocusPrompt()
	if d.simpleMode {
		if startup != nil && !goSimpleStartupEligible(startup) {
			startup = nil
		}
		if startup != nil {
			sessionID := startup.ID
			d.SelectActiveSession(sessionID)
			return d.openSelectedSession()
		}
		if d.goMode != nil && d.goMode.Recipe != nil {
			return DashboardActionGoLaunch{Recipe: *d.goMode.Recipe}
		}
		d.SetFailureNotice("Simple mode has no launch recipe; run `mj simple` again.")
		return DashboardActionNone{}
	}
	switch {
	case setup || needsRemotePath:
		return d.changeGoSetup()
	case startup != nil:
		sessionID := startup.ID
		if startup.State == core.SessionStateStopped {
			// The host fetched the stopped record and its verdict on demand.
			return d.BeginResumeFor(sessionID)
		}
		d.SelectActiveSession(sessionID)
		return d.openSelectedSession()
	default:
		return d.beginNew()
	}
}

// goSimpleStartupEligible says whether simple mode may reopen startup: a
// live local session in its own worktree that is never reviewed.
func goSimpleStartupEligible(session *core.SessionRecord) bool {
	if !session.State.IsActive() || session.TargetTemplateID != simpleTargetID {
		return false
	}
	if session.CreateManagedWorktree == nil || !*session.CreateManagedWorktree {
		return false
	}
	if session.Review == nil {
		return false
	}
	_, off := (*session.Review).(core.SessionReviewOff)
	return off
}

func (d *DashboardState) GoConversationTitle(sessionID string) string {
	session := d.state.Get().Sessions.GetShared(sessionID)
	if session == nil {
		return "Conversation"
	}
	if session.HasGivenName() {
		return session.ListedTitle()
	}
	sessions := []*core.SessionRecord{}
	for _, other := range d.state.Get().Sessions.All() {
		if other.WorkspaceID == session.WorkspaceID {
			sessions = append(sessions, other)
		}
	}
	slices.SortStableFunc(sessions, func(a, b *core.SessionRecord) int { return a.CompareByCreation(b) })
	number := max(slices.IndexFunc(sessions, func(other *core.SessionRecord) bool { return other.ID == sessionID }), 0) + 1
	return fmt.Sprintf("Conversation %d", number)
}

func (d *DashboardState) GoMode() *GoMode {
	return d.goMode
}

func (d *DashboardState) SimpleMode() bool {
	return d.simpleMode
}

func (d *DashboardState) changeGoSetup() DashboardAction {
	action := d.beginNewWizard()
	mode := d.goMode
	if mode == nil {
		return action
	}
	revealProfile := -1
	if wizard, ok := d.mode.(ModeNew); ok {
		// The local source is explicit. A remote bare path is requested
		// separately after target selection, never guessed from this path.
		wizard.projectDirectory = chat.PathInputFromValue(mode.Directory)
		if recipe := mode.Recipe; recipe != nil {
			config := d.config.Get()
			wizard.profile = 0
			position := 0
			for id := range config.EnabledProfiles() {
				if id == recipe.ProfileID {
					wizard.profile = position
					break
				}
				position++
			}
			wizard.focusedProvider = nil
			revealProfile = wizard.profile
			wizard.target = max(slices.Index(slices.Sorted(maps.Keys(config.Targets)), recipe.TargetID), 0)
			wizard.createManagedWorktree = recipe.CreateManagedWorktree != nil && *recipe.CreateManagedWorktree
			wizard.resourceAllocation = recipe.ResourceAllocation
			wizard.mounts.mounts = slices.Clone(recipe.AdditionalMounts)
		}
	}
	if revealProfile >= 0 {
		provider := newWizardProviderForProfile(d, revealProfile)
		if wizard, ok := d.mode.(ModeNew); ok && provider != nil {
			if wizard.expandedProviders == nil {
				wizard.expandedProviders = map[core.QuotaProvider]struct{}{}
			}
			wizard.expandedProviders[provider] = struct{}{}
		}
	}
	return action
}

func (d *DashboardState) GoLaunchAction(workspaceID string, recipe core.GoRecipe) DashboardAction {
	oracle := d.state.Get().LastOracleChoiceFor(recipe.ProfileID)
	// Normalize explicit choices from old recipes and let admission resolve
	// omission.
	var subagents *core.SubagentPolicy
	if recipe.Subagents != nil {
		policy := *recipe.Subagents
		if profile, ok := d.config.Get().Profiles[recipe.ProfileID]; ok {
			policy = recipe.Subagents.AsChoiceFor(profile.Kind)
		}
		subagents = &policy
	}
	bundleID := ""
	if recipe.BundleID != nil {
		bundleID = *recipe.BundleID
	} else {
		projectDirectory := ""
		if recipe.ProjectDirectory != nil {
			projectDirectory = *recipe.ProjectDirectory
		}
		bundleID = core.RawProjectContextID(projectDirectory)
	}
	var review *core.SessionReview
	if d.simpleMode {
		var off core.SessionReview = core.SessionReviewOff{}
		review = &off
	}
	return DashboardActionCreateSession{
		RememberContainerSize: false,
		WorkspaceID:           workspaceID,
		ProfileID:             recipe.ProfileID,
		Provider:              nil,
		TargetTemplateID:      recipe.TargetID,
		BundleID:              bundleID,
		ProjectDirectory:      recipe.ProjectDirectory,
		CreateManagedWorktree: recipe.CreateManagedWorktree,
		Subagents:             subagents,
		Oracle:                &oracle,
		OracleChoice:          nil,
		Review:                review,
		AdditionalMounts:      recipe.AdditionalMounts,
		ResourceAllocation:    recipe.ResourceAllocation,
		Carried:               CarriedLaunchSettings{},
	}
}

// RememberGoLaunch records a launch's settings as its workspace's fast-start
// recipe and returns what the host should save, or nil when the launch is
// not one to remember.
func (d *DashboardState) RememberGoLaunch(action DashboardAction) *RememberedGoLaunch {
	if d.simpleMode {
		return nil
	}
	create, ok := action.(DashboardActionCreateSession)
	if !ok {
		return nil
	}
	if create.ProfileID == "" {
		// Fast-start recipes store a concrete profile id; do not overwrite
		// them with an empty id from Auto or provider-balanced launches.
		return nil
	}
	var bundleID *string
	if create.ProjectDirectory == nil {
		id := create.BundleID
		bundleID = &id
	}
	recipe := core.GoRecipe{
		ProfileID:             create.ProfileID,
		TargetID:              create.TargetTemplateID,
		BundleID:              bundleID,
		ProjectDirectory:      create.ProjectDirectory,
		CreateManagedWorktree: create.CreateManagedWorktree,
		Subagents:             create.Subagents,
		AdditionalMounts:      slices.Clone(create.AdditionalMounts),
		ResourceAllocation:    create.ResourceAllocation,
	}
	if mode := d.goMode; mode != nil && mode.WorkspaceID != nil && *mode.WorkspaceID == create.WorkspaceID {
		stored := recipe
		mode.Recipe = &stored
		return &RememberedGoLaunch{Directory: mode.Directory, Recipe: recipe, SaveAsDefault: mode.SaveAsDefault}
	}
	mode, ok := d.goWorkspaces[create.WorkspaceID]
	if !ok {
		return nil
	}
	stored := recipe
	mode.Recipe = &stored
	d.goWorkspaces[create.WorkspaceID] = mode
	return &RememberedGoLaunch{Directory: mode.Directory, Recipe: recipe, SaveAsDefault: mode.SaveAsDefault}
}

func (d *DashboardState) goContext() []string {
	mode := d.goMode
	if mode == nil {
		return []string{}
	}
	lines := []string{
		core.GoPreferencesDirectoryLabel(mode.Directory),
		fmt.Sprintf("Source: %s", mode.Directory),
	}
	if session := d.selectedSession(); session != nil {
		lines[0] = fmt.Sprintf("%s · %s · %s", lines[0], session.LastProfile, session.TargetTemplateID)
		sharing := "isolated checkout"
		if checkout, err := d.state.Get().Checkout(session.ID); err == nil {
			switch checkout.(type) {
			case core.CheckoutManagedWorktree:
				sharing = "separate checkout"
			case core.CheckoutAttached, core.CheckoutBorrowed:
				sharing = "shared folder"
			}
		}
		switch context, ok := d.goContexts[session.ID]; {
		case ok && context.Err == nil:
			lines = append(lines, fmt.Sprintf("Working: %s · branch: %s · %s", context.Ok.First, context.Ok.Second, sharing))
		case ok:
			lines = append(lines, fmt.Sprintf("Working location unavailable: %s", *context.Err))
		default:
			lines = append(lines, fmt.Sprintf("Checking working location… · %s", sharing))
		}
	} else if recipe := mode.Recipe; recipe != nil {
		lines[0] = fmt.Sprintf("%s · %s · %s", lines[0], recipe.ProfileID, recipe.TargetID)
	}
	return lines
}
