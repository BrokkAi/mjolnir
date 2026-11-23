// Port of mj-targets/src/targets.rs.
//
// Declarative execution plans for Hel session targets.
//
// Plans deliberately contain argv vectors instead of local shell strings. A
// shell is used only at the SSH boundary, where OpenSSH necessarily sends a
// command string; every remotely supplied argument is POSIX-quoted there.
//
// Rust re-exports all of mj_core::targets from this module; Go callers use
// package core for those items. Name collisions: image_refresh_plan is
// ImageRefreshPlanFor beside the ImageRefreshPlan type.

package targets

import (
	_ "embed"
	"fmt"
	"maps"
	"path/filepath"
	"reflect"
	"slices"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// PodmanDocumentationURL and DockerDocumentationURL are the published copies
// of `docs/PODMAN.md` and `docs/DOCKER.md` (`docs/scripts/sync-podman.mjs`
// copies them into the site at `docs/astro.config.mjs`'s `site`). Messages
// link these because an installed user has no repository checkout.
const PodmanDocumentationURL = "https://mjolnir.brokk.ai/podman/"
const DockerDocumentationURL = "https://mjolnir.brokk.ai/docker/"

// `mj doctor` prints a self-contained setup page that quotes these two pages
// in full. They are embedded here, beside the paths that name them.

// PodmanDocumentation is the rootless Podman postconditions page, verbatim.
//
//go:embed data/docs/PODMAN.md
var PodmanDocumentation string

// DockerDocumentation is the Docker postconditions page, verbatim.
//
//go:embed data/docs/DOCKER.md
var DockerDocumentation string

// podmanMinimumVersion is the oldest Podman (major, minor) Mjolnir supports.
var podmanMinimumVersion = [2]uint32{4, 0}

// AgentWorkingDirectoryAt resolves the primary repository's directory on a
// target. Session indexing uses the same rule as API file exports so both
// surfaces name the agent's actual working directory.
func AgentWorkingDirectoryAt(workspaceRoot string, destination string) string {
	return TargetJoin(workspaceRoot, destination)
}

// TargetJoin joins a relative path onto a target-side root.
//
// Target paths are POSIX text whatever this daemon runs on, so the
// components are spelled with `/` here rather than by the host's separator.
// Only normal components are kept, as Rust's Path::components yields them.
func TargetJoin(root string, relative string) string {
	path := strings.TrimRight(root, "/")
	relative = filepath.ToSlash(relative[len(filepath.VolumeName(relative)):])
	for _, part := range strings.Split(relative, "/") {
		if part == "" || part == "." || part == ".." {
			continue
		}
		path += "/" + part
	}
	return path
}

// EffectiveTargetImages is the target's base image followed by its distinct
// bundle images for this platform. An unknown platform (nil) can only
// resolve the base image.
func EffectiveTargetImages(config *core.Config, targetID string, platform *string) []string {
	target, ok := config.Targets[targetID]
	if !ok {
		return nil
	}
	container := target.Container()
	if container == nil {
		return nil
	}
	images := []string{container.Image}
	if platform != nil {
		// Rust's BTreeMap visits bundles in order.
		for _, bundleID := range slices.Sorted(maps.Keys(config.Bundles)) {
			if image, ok := config.SessionImage(&bundleID, targetID, *platform); ok && !slices.Contains(images, image) {
				images = append(images, image)
			}
		}
	}
	return images
}

type ImageRefreshPlan struct {
	Pulls []core.ImageRefresh
	// KeepImages includes images whose pull policy is Never. The host may
	// already have those images, and their targets still need them.
	KeepImages map[string]struct{}
	// MbxCacheBudget is the machine's `build_cache.max_total_size`, which
	// also bounds a Nitro host's proxy cache. Nil means mbx's automatic
	// budget.
	MbxCacheBudget *string
}

// ImageRefreshPlanFor plans downloads, once per (host, image, platform).
// Targets that share an image take the more eager policy. Supply one host's
// targets when using the keep set to reconcile its image store (Rust:
// image_refresh_plan).
func ImageRefreshPlanFor(config *core.Config, targetIDs []string, nativePlatform *string) ImageRefreshPlan {
	plan := ImageRefreshPlan{KeepImages: map[string]struct{}{}}
	for _, targetID := range targetIDs {
		target, ok := config.Targets[targetID]
		if !ok {
			continue
		}
		host, container, ok := core.TargetTemplateImageHost(target)
		if !ok {
			continue
		}
		platform := container.Platform
		if platform == nil {
			platform = nativePlatform
		}
		if container.BuildCache != nil && container.BuildCache.MaxTotalSize != nil {
			budget := *container.BuildCache.MaxTotalSize
			plan.MbxCacheBudget = &budget
		}
		for _, image := range EffectiveTargetImages(config, targetID, platform) {
			plan.KeepImages[image] = struct{}{}
			refresh, ok := core.ImageRefreshFor(host, image, platform, container.PullPolicy)
			if !ok {
				continue
			}
			index := slices.IndexFunc(plan.Pulls, func(entry core.ImageRefresh) bool {
				return reflect.DeepEqual(entry.Host, refresh.Host) && entry.Image == refresh.Image &&
					reflect.DeepEqual(entry.Platform, refresh.Platform)
			})
			if index >= 0 {
				plan.Pulls[index].When = max(plan.Pulls[index].When, refresh.When)
			} else {
				plan.Pulls = append(plan.Pulls, refresh)
			}
		}
	}
	return plan
}

// UnwrappedRemoteLine is the command an SSH remote line runs, without the
// macOS system PATH prefix every remote line carries.
func UnwrappedRemoteLine(remote string) string {
	prefix := core.RemoteCommandLine(nil) + " "
	rest, ok := strings.CutPrefix(remote, prefix)
	if !ok {
		panic(fmt.Sprintf("%s does not run under the macOS system PATH", core.RustDebugString(remote)))
	}
	return rest
}
