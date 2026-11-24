// Port of mj-import/src/project_catalog.rs.

// Daemon-owned project discovery. Reads are immediate; refreshes coalesce
// and run off the event loop. The durable catalog owns canonical project
// IDs.

package mjimport

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"path/filepath"
	"reflect"
	"slices"
	"sort"
	"sync"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// Snapshot resolves a configured bundle's repository identities, and with
// launch also its network sources.
func Snapshot(bundle *core.ProjectBundle, executor core.CommandExecutor, launch bool) (core.ProjectBundleSnapshot, error) {
	project := core.ProjectBundleSnapshot{
		Bundle: core.ProjectBundle{
			PrimaryRepo:  bundle.PrimaryRepo,
			Repositories: slices.Clone(bundle.Repositories),
		},
		Identities:     map[string]core.RepositoryIdentity{},
		NetworkSources: map[string]core.NetworkGitSource{},
	}
	for index := range bundle.Repositories {
		repository := &bundle.Repositories[index]
		var identity core.RepositoryIdentity
		if repository.Github != nil {
			identity = core.RepositoryIdentityFromRemote(*repository.Github)
			if identity == nil {
				return core.ProjectBundleSnapshot{}, errors.New("invalid project remote")
			}
		} else {
			if repository.Local == nil {
				return core.ProjectBundleSnapshot{}, errors.New("project source is missing")
			}
			var err error
			identity, err = core.LocalIdentity(*repository.Local, executor)
			if err != nil {
				return core.ProjectBundleSnapshot{}, err
			}
		}
		project.Identities[repository.ID] = identity
		if launch {
			source, err := core.ResolveRepository(repository, executor)
			if err != nil {
				return core.ProjectBundleSnapshot{}, err
			}
			project.NetworkSources[repository.ID] = source
		}
	}
	if _, err := project.Key(); err != nil {
		return core.ProjectBundleSnapshot{}, err
	}
	return project, nil
}

func hasNoRecordedCheckoutDirectory(state *core.StateState, session *core.SessionRecord) bool {
	checkout, err := state.Checkout(session.ID)
	if err != nil {
		return false
	}
	_, ok := checkout.ProjectDirectory()
	return !ok
}

// ResolveDirectory resolves a directory on a bare target, returning the
// host it lives on ("local" for this machine) and its checkout.
func ResolveDirectory(target core.TargetTemplate, path string, executor core.CommandExecutor) (string, core.ResolvedDirectory, error) {
	switch template := target.(type) {
	case core.TargetTemplateLocalBare:
		resolved, err := core.ResolveLocalDirectory(path, executor)
		if err != nil {
			return "", core.ResolvedDirectory{}, err
		}
		return "local", resolved, nil
	case core.TargetTemplateSshBare:
		host, ok := core.ProjectHistoryHost(target)
		if !ok {
			return "", core.ResolvedDirectory{}, errors.New("project host is missing")
		}
		resolved, err := core.ResolveDirectory(path, &host, &targets.RemoteGitExecutor{
			Executor: executor,
			SSH:      core.SshTargetFromSshConnection(&template.SSH),
		})
		if err != nil {
			return "", core.ResolvedDirectory{}, err
		}
		return host, resolved, nil
	}
	return "", core.ResolvedDirectory{}, errors.New("directory projects require a bare target")
}

// DirectorySnapshot is the one-repository project of a resolved directory.
func DirectorySnapshot(resolved *core.ResolvedDirectory) core.ProjectBundleSnapshot {
	id := SetupStyleID(resolved.Identity.Name())
	local := resolved.CheckoutRoot
	project := core.ProjectBundleSnapshot{
		Bundle: core.ProjectBundle{
			PrimaryRepo: id,
			Repositories: []core.ProjectRepository{{
				ID:          id,
				Local:       &local,
				Destination: id,
			}},
		},
		Identities:     map[string]core.RepositoryIdentity{id: resolved.Identity},
		NetworkSources: map[string]core.NetworkGitSource{},
	}
	if resolved.PlainDirectory {
		project.PlainDirectories = []string{id}
	}
	return project
}

// AcceptDirectory resolves directory on target and records it in the
// catalog, returning the project's id and snapshot.
func AcceptDirectory(target core.TargetTemplate, directory string, executor core.CommandExecutor) (string, core.ProjectBundleSnapshot, error) {
	host, resolved, err := ResolveDirectory(target, directory, executor)
	if err != nil {
		return "", core.ProjectBundleSnapshot{}, err
	}
	return AcceptResolvedDirectory(host, directory, resolved)
}

// AcceptResolvedDirectory records an already resolved directory in the
// catalog, returning the project's id and snapshot.
func AcceptResolvedDirectory(host string, directory string, resolved core.ResolvedDirectory) (string, core.ProjectBundleSnapshot, error) {
	project := DirectorySnapshot(&resolved)
	id, err := store.StoreDirectoryProject(&project)
	if err != nil {
		return "", core.ProjectBundleSnapshot{}, err
	}
	if err := store.StoreProjectLocation(&core.ProjectLocation{
		Host:           host,
		Directory:      directory,
		CheckoutRoot:   resolved.CheckoutRoot,
		RepositoryRoot: resolved.RepositoryRoot,
		Identity:       resolved.Identity,
		SeenAt:         core.FormatChronoRFC3339(time.Now()),
	}); err != nil {
		return "", core.ProjectBundleSnapshot{}, err
	}
	return id, project, nil
}

// projectBundlesEqual is Rust's derived PartialEq on ProjectBundle.
func projectBundlesEqual(left, right *core.ProjectBundle) bool {
	return left.PrimaryRepo == right.PrimaryRepo &&
		len(left.Repositories) == len(right.Repositories) &&
		(len(left.Repositories) == 0 || reflect.DeepEqual(left.Repositories, right.Repositories))
}

var errProjectDiscoveryCancelled = errors.New("project discovery cancelled")

// reconcileConfig resolves changed TOML inputs only. Previously accepted
// definitions are used for unchanged inputs, including sources temporarily
// unavailable offline. Failures that discovery reports go to failures.
func reconcileConfig(config *core.Config, executor core.CommandExecutor, failures *[]string) error {
	catalog, err := store.ReadProjectCatalog()
	if err != nil {
		return err
	}
	state, err := store.LoadState()
	if err != nil {
		return err
	}
	sessionCount := func(id string) int {
		count := 0
		for session := range state.Sessions.Values() {
			if session.BundleID == id {
				count++
			}
		}
		return count
	}
	configured := make([]string, 0, len(config.Bundles))
	for id := range config.Bundles {
		configured = append(configured, id)
	}
	counts := map[string]int{}
	for _, id := range configured {
		counts[id] = sessionCount(id)
	}
	sort.Slice(configured, func(left, right int) bool {
		a, b := configured[left], configured[right]
		if counts[a] != counts[b] {
			return counts[a] > counts[b]
		}
		return a < b
	})
	needsLaunch := func(id string) bool {
		for session := range state.Sessions.Values() {
			if session.BundleID == id && session.Project == nil && hasNoRecordedCheckoutDirectory(&state, session) {
				return true
			}
		}
		return false
	}
	for _, id := range configured {
		bundle := config.Bundles[id]
		if executor.CancellationRequested() {
			return errProjectDiscoveryCancelled
		}
		var existing *core.SavedProject
		for index := range catalog.Projects {
			saved := &catalog.Projects[index]
			if saved.BundleID == id && projectBundlesEqual(&saved.Project.Bundle, &bundle) {
				existing = saved
				break
			}
		}
		var project core.ProjectBundleSnapshot
		var resolveErr error
		if existing != nil && (len(existing.Project.NetworkSources) != 0 || !needsLaunch(id)) {
			project = existing.Project
		} else {
			project, resolveErr = Snapshot(&bundle, executor, needsLaunch(id))
		}
		var unavailable core.RepositoryUnavailable
		switch {
		case resolveErr == nil:
			cataloged := false
			for index := range catalog.Projects {
				if catalog.Projects[index].BundleID == id {
					cataloged = true
					break
				}
			}
			if !cataloged {
				var sessions []*core.SessionRecord
				for session := range state.Sessions.Values() {
					if session.BundleID == id && hasNoRecordedCheckoutDirectory(&state, session) {
						sessions = append(sessions, session)
					}
				}
				if err := migrateMemory(&project, &bundle, sessions); err != nil {
					return err
				}
			}
			if _, err := store.StoreCatalogProject(id, &project, true); err != nil {
				return err
			}
		case errors.As(resolveErr, &unavailable):
			slog.Debug("configured project is unavailable", "project", id, "error", resolveErr)
		default:
			*failures = append(*failures, fmt.Sprintf("Project %s: %v", id, resolveErr))
		}
	}
	// Aliases are durable before deleting duplicate TOML entries. Compare
	// the fresh definition so concurrent manual edits are never removed.
	aliases, err := store.PendingProjectAliases()
	if err != nil {
		return err
	}
	for _, alias := range aliases {
		if executor.CancellationRequested() {
			return errProjectDiscoveryCancelled
		}
		if err := consolidateAlias(&alias); err != nil {
			return err
		}
	}
	return nil
}

// consolidateAlias removes one durable alias's duplicate configured bundle,
// holding upgrade admission until the alias is finished.
func consolidateAlias(alias *store.ProjectAlias) error {
	commit, err := core.UpgradeActivityUnlessDraining("project consolidation config")
	if err != nil {
		return err
	}
	defer commit.Release()
	_, _, err = core.ConfigUpdate(func(fresh *core.Config) (struct{}, error) {
		current, ok := fresh.Bundles[alias.BundleID]
		if !ok || !projectBundlesEqual(&current, &alias.Project.Bundle) {
			return struct{}{}, nil
		}
		if _, ok := fresh.Bundles[alias.CanonicalID]; !ok {
			view, err := store.ReadProjectCatalog()
			if err != nil {
				return struct{}{}, err
			}
			var saved *core.SavedProject
			for index := range view.Projects {
				if view.Projects[index].BundleID == alias.CanonicalID {
					saved = &view.Projects[index]
					break
				}
			}
			if saved == nil {
				return struct{}{}, errors.New("alias canonical project is missing")
			}
			fresh.Bundles[alias.CanonicalID] = saved.Project.Bundle
		}
		delete(fresh.Bundles, alias.BundleID)
		fresh.Images.ConsolidateBundle(alias.BundleID, alias.CanonicalID)
		return struct{}{}, nil
	})
	if err != nil {
		return err
	}
	return store.FinishProjectAlias(alias.BundleID)
}

// discoverLocal records a local directory found in history in the catalog.
// Discovery never writes `config.toml`: saved bundles are the ones the user
// created, and a discovered project becomes one only when the user chooses
// it.
//
// Discovery stays strict about Git. Any directory a harness recorded is not
// necessarily a project - a person may have run an agent from a scratch
// folder - so only a checkout is listed automatically. A plain folder is
// still openable: the project-directory dialog accepts one when a person
// names it, which is a deliberate choice rather than a guess from history.
func discoverLocal(path string, executor core.CommandExecutor) error {
	resolved, err := core.ResolveLocalDirectory(path, executor)
	if err != nil {
		return err
	}
	if resolved.PlainDirectory {
		return core.RepositoryUnavailableNotRepository{Path: path}
	}
	_, _, err = AcceptDirectory(core.TargetTemplateLocalBare{}, path, executor)
	return err
}

// historicalDiscoveryError is the error to report for one historical
// directory, or nil when it succeeded or is merely unavailable.
func historicalDiscoveryError(result error, path string) *string {
	if result == nil {
		return nil
	}
	var unavailable core.RepositoryUnavailable
	if errors.As(result, &unavailable) {
		slog.Debug("historical project is unavailable", "directory", path, "error", result)
		return nil
	}
	message := fmt.Sprintf("%s: %v", path, result)
	return &message
}

// catalogCanonicalize is Rust's fs::canonicalize.
func catalogCanonicalize(path string) (string, error) {
	absolute, err := filepath.Abs(path)
	if err != nil {
		return "", err
	}
	return filepath.EvalSymlinks(absolute)
}

func refresh(executor core.CommandExecutor, retry bool) ([]string, error) {
	var errs []string
	config, err := core.ConfigLoad()
	if err != nil {
		return nil, err
	}
	if err := reconcileConfig(&config, executor, &errs); err != nil {
		return nil, err
	}
	type seededHome struct{ harness, home string }
	homes := map[seededHome]bool{}
	profileIDs := make([]string, 0, len(config.Profiles))
	for id := range config.Profiles {
		profileIDs = append(profileIDs, id)
	}
	sort.Strings(profileIDs)
	for _, id := range profileIDs {
		profile := config.Profiles[id]
		if !profile.Enabled {
			continue
		}
		home, err := catalogCanonicalize(profile.Home)
		if err != nil {
			home = profile.Home
		}
		encoded, err := json.Marshal(profile.Kind)
		if err != nil {
			return nil, err
		}
		harness := string(encoded)
		key := seededHome{harness, home}
		if homes[key] {
			continue
		}
		homes[key] = true
		seeded, err := store.ProjectSeeded(harness, home)
		if err != nil {
			return nil, err
		}
		if seeded {
			continue
		}
		if executor.CancellationRequested() {
			return nil, errProjectDiscoveryCancelled
		}
		seed, err := RecentProjectDirectories(profile.Kind, home, 10, executor)
		if err != nil {
			errs = append(errs, fmt.Sprintf("%s profile %s: %v", profile.Kind.DisplayName(), home, err))
			continue
		}
		for _, failure := range seed.Errors {
			message := failure[1]
			if err := store.SeedFailure(harness, home, failure[0], true, &message); err != nil {
				return nil, err
			}
			errs = append(errs, message)
		}
		for _, path := range seed.Directories {
			if executor.CancellationRequested() {
				return nil, errProjectDiscoveryCancelled
			}
			if message := historicalDiscoveryError(discoverLocal(path, executor), path); message != nil {
				if err := store.SeedFailure(harness, home, path, false, message); err != nil {
					return nil, err
				}
				errs = append(errs, *message)
			}
		}
		if executor.CancellationRequested() {
			return nil, errProjectDiscoveryCancelled
		}
		if err := store.FinishProjectSeed(harness, home); err != nil {
			return nil, err
		}
	}
	if retry {
		failures, err := store.SeedFailures()
		if err != nil {
			return nil, err
		}
		for _, failure := range failures {
			if executor.CancellationRequested() {
				return nil, errProjectDiscoveryCancelled
			}
			var result error
			if failure.SourceFile {
				var kind core.HarnessKind
				if err := json.Unmarshal([]byte(failure.Harness), &kind); err != nil {
					return nil, err
				}
				directory, err := NativeProjectDirectory(failure.Directory, kind, executor)
				if err == nil {
					err = discoverLocal(directory, executor)
				}
				result = err
			} else {
				result = discoverLocal(failure.Directory, executor)
			}
			message := historicalDiscoveryError(result, failure.Directory)
			if message != nil {
				errs = append(errs, *message)
			}
			if err := store.SeedFailure(failure.Harness, failure.Home, failure.Directory, failure.SourceFile, message); err != nil {
				return nil, err
			}
		}
	}
	for {
		changes, err := store.ProjectDiscoveryChanges(retry)
		if err != nil {
			return nil, err
		}
		if len(changes) == 0 {
			break
		}
		// Failed candidates are visited once per explicit retry, not in
		// every batch of the same pass.
		for _, change := range changes {
			if executor.CancellationRequested() {
				return nil, errProjectDiscoveryCancelled
			}
			discoveredDirectory := change.Directory
			result := discoverChange(&config, &change, executor, &discoveredDirectory)
			if executor.CancellationRequested() {
				return nil, errProjectDiscoveryCancelled
			}
			message := historicalDiscoveryError(result, discoveredDirectory)
			if message != nil {
				errs = append(errs, *message)
			}
			if err := store.FinishProjectDiscovery(change.Sequence, message); err != nil {
				return nil, err
			}
		}
		if retry || len(changes) < 128 {
			break
		}
	}
	stored, err := store.DiscoveryErrors()
	if err != nil {
		return nil, err
	}
	errs = append(errs, stored...)
	sort.Strings(errs)
	return slices.Compact(errs), nil
}

// discoverChange binds one session recorded by Mjolnir to its project,
// setting discoveredDirectory to the directory it resolved.
func discoverChange(config *core.Config, change *store.ProjectDiscoveryChange, executor core.CommandExecutor, discoveredDirectory *string) error {
	storedState, err := store.LoadState()
	if err != nil {
		return err
	}
	known, hasKnown := storedState.Sessions.Get(change.SessionID)
	path := change.Directory
	if checkout, err := storedState.Checkout(change.SessionID); err == nil {
		switch checkout := checkout.(type) {
		case core.CheckoutManagedWorktree:
			path = checkout.Worktree.SourceProjectDirectory
		case core.CheckoutAttached:
			path = checkout.Path
		}
	}
	*discoveredDirectory = path
	var recorded core.TargetTemplate
	if hasKnown && known.TargetRuntime != nil {
		switch connection := known.TargetRuntime.Connection.(type) {
		case core.TargetConnectionLocal:
			recorded = core.TargetTemplateLocalBare{}
		case core.TargetConnectionSsh:
			recorded = core.TargetTemplateSshBare{
				SSH:             connection.SSH,
				Permissions:     core.PermissionModeGuardian,
				WorkspacePrefix: "/tmp/mj-workspaces",
			}
		}
	}
	target := recorded
	if target == nil {
		configured, ok := config.Targets[change.TargetID]
		if !ok {
			return errors.New("historical project target is unavailable")
		}
		target = configured
	}
	var id string
	var project core.ProjectBundleSnapshot
	if hasKnown && known.Project != nil {
		id, project = known.BundleID, *known.Project
	} else {
		id, project, err = AcceptDirectory(target, path, executor)
		if err != nil {
			return err
		}
	}
	if hasKnown && known.Project == nil {
		if err := migrateMemory(&project, nil, []*core.SessionRecord{&known}); err != nil {
			return err
		}
	}
	if err := store.BindSessionProject(change.SessionID, id, &project); err != nil {
		return err
	}
	if _, ok := target.(core.TargetTemplateLocalBare); ok {
		return discoverLocal(path, executor)
	}
	return nil
}

// migrateMemory merges the project memory of a configured bundle's legacy
// key and of sessions' legacy keys into project's canonical store.
func migrateMemory(project *core.ProjectBundleSnapshot, bundle *core.ProjectBundle, sessions []*core.SessionRecord) error {
	identity, err := project.MemoryIdentity()
	if err != nil {
		return err
	}
	canonical, err := core.ProjectMemoryIdentityKey(identity)
	if err != nil {
		return err
	}
	root := func(key string) string {
		return filepath.Join(core.DataDir(), "projects", key, "memory")
	}
	var legacy []string
	if bundle != nil {
		primaryRepository := bundle.Primary()
		if primaryRepository == nil {
			return errors.New("legacy primary is missing")
		}
		primary, err := core.RepositoryMemoryIdentityFromConfigured(primaryRepository)
		if err != nil {
			return err
		}
		members := make([]core.RepositoryMemoryIdentity, 0, len(bundle.Repositories))
		for index := range bundle.Repositories {
			member, err := core.RepositoryMemoryIdentityFromConfigured(&bundle.Repositories[index])
			if err != nil {
				return err
			}
			members = append(members, member)
		}
		key, err := core.ProjectMemoryIdentityKey(core.NewProjectMemoryIdentityBundle(primary, members))
		if err != nil {
			return err
		}
		legacy = append(legacy, key)
	}
	for _, session := range sessions {
		identity, err := core.ProjectMemoryIdentityForLegacySession(session, bundle, nil)
		if err != nil {
			return err
		}
		key, err := core.ProjectMemoryIdentityKey(identity)
		if err != nil {
			return err
		}
		legacy = append(legacy, key)
	}
	for _, key := range legacy {
		conflicts, err := core.MergeCanonicalStores(root(key), root(canonical))
		if err != nil {
			return err
		}
		for _, conflict := range conflicts {
			slog.Warn("project memory merge preserved conflicting documents", "conflict", conflict)
		}
	}
	return nil
}

// Catalog coalesces refresh requests and serves the catalog view. Its zero
// value is ready to use (Rust: Catalog::default()).
type Catalog struct {
	requested atomic.Bool
	retry     atomic.Bool
	// notify holds at most one pending wake-up, as tokio's Notify stores
	// one permit for notify_one. Created on first use.
	notifyOnce sync.Once
	notify     chan struct{}
	// mu guards status.
	mu     sync.Mutex
	status core.ProjectCatalogStatus
}

func (c *Catalog) wake() chan struct{} {
	c.notifyOnce.Do(func() { c.notify = make(chan struct{}, 1) })
	return c.notify
}

// Request asks for a refresh; retry also revisits durable failures.
func (c *Catalog) Request(retry bool) {
	if retry {
		c.retry.Store(true)
	}
	c.requested.Store(true)
	c.mu.Lock()
	c.status = core.ProjectCatalogStatus{State: core.ProjectCatalogStateRefreshing}
	c.mu.Unlock()
	select {
	case c.wake() <- struct{}{}:
	default:
	}
}

// View is the stored catalog with network URLs made displayable, and the
// current refresh status.
func (c *Catalog) View() (core.ProjectCatalogView, error) {
	view, err := store.ReadProjectCatalog()
	if err != nil {
		return core.ProjectCatalogView{}, err
	}
	for index := range view.Projects {
		sources := view.Projects[index].Project.NetworkSources
		for key, source := range sources {
			source.FetchURL = core.DisplayNetworkURL(source.FetchURL)
			source.PushURLs = core.DisplayNetworkURLs(source.PushURLs)
			sources[key] = source
		}
	}
	c.mu.Lock()
	view.Status = core.ProjectCatalogStatus{State: c.status.State, Errors: slices.Clone(c.status.Errors)}
	c.mu.Unlock()
	return view, nil
}

// refreshOutcome is one refresh's result as its goroutine reports it.
type refreshOutcome struct {
	errors []string
	err    error
}

// runRefresh runs one refresh on its own goroutine and reports its outcome,
// a panic included, on the returned channel.
func runRefresh(executor core.CommandExecutor, retry bool) <-chan refreshOutcome {
	done := make(chan refreshOutcome, 1)
	go func() {
		defer func() {
			if recovered := recover(); recovered != nil {
				done <- refreshOutcome{err: fmt.Errorf("project discovery task panicked: %v", recovered)}
			}
		}()
		errs, err := refresh(executor, retry)
		done <- refreshOutcome{errors: errs, err: err}
	}()
	return done
}

// projectDiscoveryCleanup is how long shutdown waits for a cancelled
// refresh before leaving it to finish on its own; unfinished discovery
// resumes at the next startup.
const projectDiscoveryCleanup = 2 * time.Second

// projectDiscoveryDeadline bounds each refresh, so a wedged child becomes a
// reported failure.
const projectDiscoveryDeadline = 120 * time.Second

// Run refreshes the catalog whenever one is requested until ctx (the
// daemon's shutdown) is cancelled. It reconciles durable failures once at
// startup: an upgraded resolver or restored checkout must recover without
// requiring a manual retry.
func (c *Catalog) Run(ctx context.Context) {
	c.Request(true)
	for {
		select {
		case <-ctx.Done():
			return
		case <-c.wake():
		}
		if !c.requested.Swap(false) {
			continue
		}
		retry := c.retry.Swap(false)
		cancelled := new(atomic.Bool)
		executor := core.NewCancellableProcessExecutor(cancelled).WithDeadline(projectDiscoveryDeadline)
		done := runRefresh(executor, retry)
		var outcome refreshOutcome
		select {
		case outcome = <-done:
		case <-ctx.Done():
			cancelled.Store(true)
			timer := time.NewTimer(projectDiscoveryCleanup)
			defer timer.Stop()
			select {
			case outcome := <-done:
				if outcome.err != nil {
					slog.Warn("project discovery stopped during cancellation", "error", outcome.err)
				}
			case <-timer.C:
				slog.Warn("project discovery cleanup exceeded two seconds; unfinished discovery will resume at startup")
				go func() {
					if outcome := <-done; outcome.err != nil {
						slog.Warn("cancelled project discovery failed", "error", outcome.err)
					}
				}()
			}
			return
		}
		status := core.ProjectCatalogStatus{State: core.ProjectCatalogStateReady}
		switch {
		case outcome.err != nil:
			status = core.ProjectCatalogStatus{State: core.ProjectCatalogStateFailed, Errors: []string{outcome.err.Error()}}
		case len(outcome.errors) != 0:
			status = core.ProjectCatalogStatus{State: core.ProjectCatalogStateFailed, Errors: outcome.errors}
		}
		if status.State == core.ProjectCatalogStateFailed {
			for _, message := range status.Errors {
				slog.Warn("project discovery failed", "error", message)
			}
		}
		c.mu.Lock()
		if c.requested.Load() {
			c.status = core.ProjectCatalogStatus{State: core.ProjectCatalogStateRefreshing}
		} else {
			c.status = status
		}
		c.mu.Unlock()
	}
}
