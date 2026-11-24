// Port of mj-import/src/project_catalog.rs.

// Daemon-owned project discovery. Reads are immediate; refreshes coalesce
// and run off the event loop. The durable catalog owns canonical project
// IDs.

package mjimport

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"slices"
	"sync"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
)

// Snapshot resolves a configured bundle's repository identities, and with
// launch also its network sources.
func Snapshot(bundle *core.ProjectBundle, executor core.CommandExecutor, launch bool) (core.ProjectBundleSnapshot, error) {
	panic("unported: mj-import/src/project_catalog.rs snapshot")
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
	panic("unported: mj-import/src/project_catalog.rs resolve_directory")
}

// DirectorySnapshot is the one-repository project of a resolved directory.
func DirectorySnapshot(resolved *core.ResolvedDirectory) core.ProjectBundleSnapshot {
	panic("unported: mj-import/src/project_catalog.rs directory_snapshot")
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

// reconcileConfig resolves changed TOML inputs only. Previously accepted
// definitions are used for unchanged inputs, including sources temporarily
// unavailable offline. Failures that discovery reports go to errors.
func reconcileConfig(config *core.Config, executor core.CommandExecutor, errors *[]string) error {
	panic("unported: mj-import/src/project_catalog.rs reconcile_config")
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

func refresh(executor core.CommandExecutor, retry bool) ([]string, error) {
	panic("unported: mj-import/src/project_catalog.rs refresh")
}

// migrateMemory merges the project memory of a configured bundle's legacy
// key and of sessions' legacy keys into project's canonical store.
func migrateMemory(project *core.ProjectBundleSnapshot, bundle *core.ProjectBundle, sessions []*core.SessionRecord) error {
	panic("unported: mj-import/src/project_catalog.rs migrate_memory")
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

// Run refreshes the catalog whenever one is requested until ctx (the
// daemon's shutdown) is cancelled. It reconciles durable failures once at
// startup: an upgraded resolver or restored checkout must recover without
// requiring a manual retry.
func (c *Catalog) Run(ctx context.Context) {
	panic("unported: mj-import/src/project_catalog.rs Catalog::run")
}
