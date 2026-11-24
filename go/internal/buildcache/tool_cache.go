// Port of mj-build-cache/src/tool_cache.rs.
//
// Machine storage shared by native build tools, independently of mbx.

package buildcache

import (
	"errors"
	"fmt"
	"log/slog"
	"maps"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// ToolCachePrepare places the native build-tool caches for a session on the
// target templateID names, adding their mounts to mounts (Rust
// tool_cache::prepare). Publish all mounts with their placement, or none: a
// cache failure must not leave an unrecorded mount or prevent the requested
// session starting, so it is reported as a notice and the result is nil.
func ToolCachePrepare(config *core.Config, templateID string, mounts *[]core.AdditionalMount, executor core.CommandExecutor) *core.ToolCachePlacement {
	// Publish all mounts with their placement, or none. A cache failure must
	// not leave an unrecorded mount or prevent the requested session starting.
	prepared := append([]core.AdditionalMount{}, (*mounts)...)
	cache, err := prepareChecked(config, templateID, &prepared, executor)
	if err != nil {
		slog.Warn(fmt.Sprintf("native build caches unavailable: %s", alternate(err)), "target", templateID)
		executor.NotifyNotice(fmt.Sprintf("Native build caches are unavailable: %s. This session will start without them.", alternate(err)))
		return nil
	}
	*mounts = prepared
	return cache
}

func prepareChecked(config *core.Config, templateID string, mounts *[]core.AdditionalMount, executor core.CommandExecutor) (*core.ToolCachePlacement, error) {
	template, ok := config.Targets[templateID]
	if !ok {
		return nil, errors.New("cache target disappeared")
	}
	// Apple's runtime mounts supplemental directories read-only from its own
	// VM, so there is no host directory to share, locally or over SSH; EC2
	// copies the directory instead of mounting it.
	switch template.(type) {
	case core.TargetTemplateAwsEc2, core.TargetTemplateAppleContainer, core.TargetTemplateSshAppleContainer,
		// A sandbox is a VM with no way to mount a host directory.
		core.TargetTemplateSshNitro:
		return nil, nil
	}
	// Use the same mapping as configuration serialization, including implicit
	// local machines and raw SSH runtimes whose template has no cache fields.
	machines := maps.Clone(config.Machines)
	if machines == nil {
		machines = map[string]core.Machine{}
	}
	stored := core.StoredTargetOf(templateID, template, machines)
	var settings *core.TargetBuildCache
	if machine, ok := machines[stored.Machine()]; ok {
		settings = machine.BuildCache()
	}
	if settings != nil && settings.Enabled != nil && !*settings.Enabled {
		return nil, nil
	}
	host, err := CacheHostForPathTarget(template)
	if err != nil {
		return nil, err
	}
	var container bool
	switch template.(type) {
	case core.TargetTemplateLocalBare, core.TargetTemplateSshBare:
	default:
		container = true
	}
	// A container shares these directories through a bind mount, which needs
	// the host's own kernel. A bare session on a Mac runs on the Mac itself.
	// Other hosts keep native defaults.
	platform, err := host.Platform(executor)
	if err != nil {
		return nil, err
	}
	switch {
	case platform == HostPlatformLinux:
	case platform == HostPlatformDarwin && !container:
	default:
		return nil, nil
	}
	home, err := host.Home(executor)
	if err != nil {
		return nil, err
	}
	var directory string
	if settings != nil && settings.ToolsDirectory != nil {
		directory = *settings.ToolsDirectory
	} else {
		directory = core.Join(home, ".cache/mjolnir/build")
	}
	nxHome := core.Join(home, ".nx")
	nxDestination := nxHome
	if container {
		containerHome := "/home/hel"
		if template := template.Container(); template != nil {
			if value, ok := template.Environment.Resolved()["HOME"]; ok {
				containerHome = value
			}
		}
		nxDestination = core.Join(containerHome, ".nx")
	}
	for _, pair := range [][2]string{{directory, directory}, {nxHome, nxDestination}} {
		directory, destination := pair[0], pair[1]
		if container {
			for _, mount := range *mounts {
				if pathStartsWith(mount.Destination, destination) || pathStartsWith(destination, mount.Destination) {
					return nil, fmt.Errorf("an attached directory overlaps native build cache %s", directory)
				}
			}
		}
		command := host.Command([]string{"mkdir", "-p", "-m", "700", "--", directory}, "create shared native build cache")
		output, err := executor.Execute(&command)
		if err != nil {
			return nil, err
		}
		if output.Status != 0 {
			return nil, fmt.Errorf("create native build cache: %s", lossy(output.Stderr))
		}
		filesystems, err := targets.ProbeFilesystemTypes(host.SSH, []string{directory}, executor)
		if err != nil {
			return nil, err
		}
		if len(filesystems) == 0 {
			return nil, errors.New("native cache filesystem probe returned no result")
		}
		if reason, ok := core.OverlayUnsupportedFilesystem(filesystems[0]); ok &&
			(reason == "network filesystem" || reason == "FUSE filesystem") {
			return nil, fmt.Errorf("native build cache %s needs a local filesystem; configure build_cache.tools_directory on this machine", directory)
		}
		if container {
			*mounts = append(*mounts, core.AdditionalMount{
				Source:      directory,
				Destination: destination,
				Access:      core.MountAccessRw,
			})
		}
	}
	return &core.ToolCachePlacement{
		Host:      host.Key(),
		Directory: directory,
		NxHome:    nxDestination,
	}, nil
}
