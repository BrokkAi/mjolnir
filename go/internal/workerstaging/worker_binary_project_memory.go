// Port of mj-worker-staging/src/worker_binary/project_memory.rs.

package workerstaging

import (
	"errors"
	"fmt"
	"os"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ClaudeDisableAutoMemoryEnv is Claude Code's switch for its auto-memory
// directory and memory reminder.
const ClaudeDisableAutoMemoryEnv = "CLAUDE_CODE_DISABLE_AUTO_MEMORY"

// ProjectMemoryLaunch is the project memory configuration of a launch whose
// workspace is primary plus additional repository paths (Rust's
// `&(String, Vec<String>)`). bundle and parentWorktree are nil when absent.
func ProjectMemoryLaunch(session *core.SessionRecord, bundle *core.ProjectBundle, primary string, additional []string, targetProfileHome string, parentWorktree *core.ManagedWorktree) (core.ProjectMemoryLaunchConfig, error) {
	// A sub-agent records its parent's checkout as its project directory.
	// For an isolated parent that is the parent's own clone, not the
	// project, so the child takes the parent's checkout for its identity and
	// shares the parent's memory (R10-4).
	var identity core.ProjectMemoryIdentity
	var err error
	if session.Project != nil {
		identity, err = session.Project.MemoryIdentity()
	} else {
		identity, err = core.ProjectMemoryIdentityForLegacySession(session, bundle, parentWorktree)
	}
	if err != nil {
		return core.ProjectMemoryLaunchConfig{}, err
	}
	projectKey, err := core.ProjectMemoryIdentityKey(identity)
	if err != nil {
		return core.ProjectMemoryLaunchConfig{}, err
	}
	replicaSlug := ProjectMemoryReplicaSlug(projectKey, session.ID)
	projectRoot := core.Join(targetProfileHome, pathJoin("projects", replicaSlug))
	root := core.Join(projectRoot, "memory")
	baselineRoot := core.Join(projectRoot, ".hel-memory-baseline")
	repositoryRoots := map[string]string{}
	if bundle != nil {
		primaryRepository := bundle.Primary()
		if primaryRepository == nil {
			return core.ProjectMemoryLaunchConfig{}, errors.New("bundle primary is missing")
		}
		repositories := []core.ProjectRepository{*primaryRepository}
		for _, repository := range bundle.Repositories {
			if repository.ID != bundle.PrimaryRepo {
				repositories = append(repositories, repository)
			}
		}
		targetRoots := append([]string{primary}, additional...)
		for index, repository := range repositories {
			if index >= len(targetRoots) {
				break
			}
			repositoryRoots[repository.ID] = targetRoots[index]
		}
	}
	return core.ProjectMemoryLaunchConfig{
		ProjectKey:      projectKey,
		Root:            root,
		BaselineRoot:    baselineRoot,
		RepositoryRoots: repositoryRoots,
		MCPDelivery:     core.ProjectMemoryMcpDeliveryAcp,
	}, nil
}

func ProjectMemoryReplicaSlug(projectKey, sessionID string) string {
	return fmt.Sprintf("hel-%s-%s", projectKey[:16], sessionID)
}

func ProjectMemoryMCPDelivery(harness core.HarnessKind, target *core.TargetsTargetLocator) core.ProjectMemoryMcpDelivery {
	if harness == core.HarnessKindKimi && target.Kind != core.TargetsTargetLocatorLocalBare {
		return core.ProjectMemoryMcpDeliveryHarnessProfile
	}
	return core.ProjectMemoryMcpDeliveryAcp
}

func CanonicalMemoryRoot(projectKey string) string {
	return pathJoin(core.DataDir(), "projects", projectKey, "memory")
}

// StageMemoryReplica copies the canonical memory into the stage as the
// replica and its baseline, and returns their paths within the stage.
func StageMemoryReplica(memory *core.ProjectMemoryLaunchConfig, targetProfileHome, profileStage string) ([2]string, error) {
	canonical, err := core.ResolveCanonicalRoot(CanonicalMemoryRoot(memory.ProjectKey))
	if err != nil {
		return [2]string{}, err
	}
	if err := os.MkdirAll(canonical, 0o777); err != nil {
		return [2]string{}, rustIOError(err)
	}
	replica, ok := pathStripPrefix(memory.Root, targetProfileHome)
	if !ok {
		return [2]string{}, errStripPrefix
	}
	baseline, ok := pathStripPrefix(memory.BaselineRoot, targetProfileHome)
	if !ok {
		return [2]string{}, errStripPrefix
	}
	if err := CopyProfileEntry(canonical, pathJoin(profileStage, replica)); err != nil {
		return [2]string{}, err
	}
	if err := CopyProfileEntry(canonical, pathJoin(profileStage, baseline)); err != nil {
		return [2]string{}, err
	}
	return [2]string{replica, baseline}, nil
}

// errStripPrefix is Rust's StripPrefixError, as it displays.
var errStripPrefix = errors.New("prefix not found")

// StagedMemoryIndex reads the canonical-memory snapshot already copied into
// this launch's stage; false when it has no index.
func StagedMemoryIndex(memory *core.ProjectMemoryLaunchConfig, targetProfileHome, profileStage string) (string, bool, error) {
	relative, ok := pathStripPrefix(memory.Root, targetProfileHome)
	if !ok {
		return "", false, errStripPrefix
	}
	store := core.NewProjectMemoryStore(pathJoin(profileStage, relative))
	index, err := store.StartupIndex()
	if err != nil || index == nil {
		return "", false, err
	}
	return *index, true, nil
}
