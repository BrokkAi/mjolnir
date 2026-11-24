// Port of mj-worker-staging/src/worker_binary/project_memory.rs.

package workerstaging

import (
	"fmt"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ClaudeDisableAutoMemoryEnv is Claude Code's switch for its auto-memory
// directory and memory reminder.
const ClaudeDisableAutoMemoryEnv = "CLAUDE_CODE_DISABLE_AUTO_MEMORY"

// ProjectMemoryLaunch is the project memory configuration of a launch whose
// workspace is primary plus additional repository paths (Rust's
// `&(String, Vec<String>)`). bundle and parentWorktree are nil when absent.
func ProjectMemoryLaunch(session *core.SessionRecord, bundle *core.ProjectBundle, primary string, additional []string, targetProfileHome string, parentWorktree *core.ManagedWorktree) (core.ProjectMemoryLaunchConfig, error) {
	panic("unported: mj-worker-staging/src/worker_binary/project_memory.rs project_memory_launch")
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
	panic("unported: mj-worker-staging/src/worker_binary/project_memory.rs stage_memory_replica")
}

// StagedMemoryIndex reads the canonical-memory snapshot already copied into
// this launch's stage; false when it has no index.
func StagedMemoryIndex(memory *core.ProjectMemoryLaunchConfig, targetProfileHome, profileStage string) (string, bool, error) {
	panic("unported: mj-worker-staging/src/worker_binary/project_memory.rs staged_memory_index")
}
