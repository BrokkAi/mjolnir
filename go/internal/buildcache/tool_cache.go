// Port of mj-build-cache/src/tool_cache.rs.
//
// Machine storage shared by native build tools, independently of mbx.

package buildcache

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ToolCachePrepare places the native build-tool caches for a session on the
// target templateID names, adding their mounts to mounts (Rust
// tool_cache::prepare). Publish all mounts with their placement, or none: a
// cache failure must not leave an unrecorded mount or prevent the requested
// session starting, so it is reported as a notice and the result is nil.
func ToolCachePrepare(config *core.Config, templateID string, mounts *[]core.AdditionalMount, executor core.CommandExecutor) *core.ToolCachePlacement {
	panic("unported: mj-build-cache/src/tool_cache.rs prepare")
}

func prepareChecked(config *core.Config, templateID string, mounts *[]core.AdditionalMount, executor core.CommandExecutor) (*core.ToolCachePlacement, error) {
	panic("unported: mj-build-cache/src/tool_cache.rs prepare_checked")
}
