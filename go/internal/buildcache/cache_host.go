// Port of mj-build-cache/src/cache_host.rs.
//
// Running commands on the machine that hosts a container target.
//
// A container target's caches live on the host that runs the container
// engine, not on the machine running the controller. This is the one place
// that knows how to reach that host for a local or SSH target, shared by the
// Git clone cache and the mbx build cache.
//
// A host is a machine, not a runtime: local Podman and local Docker are the
// same computer, so they share one identity here and one inspection of it.

package buildcache

import (
	"sync"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// homeLifetime is how long a resolved login home is reused. A home does not
// move while a screen is open, and a burst of completions must not probe it
// per keystroke.
const homeLifetime = 600 * time.Second

// resolvedHome is one successfully resolved login home and when it was
// resolved.
type resolvedHome struct {
	recorded time.Time
	home     string
}

// homes holds successfully resolved login homes, keyed by CacheHost.Key.
var homes = struct {
	sync.Mutex
	byHost map[string]resolvedHome
}{byHost: map[string]resolvedHome{}}

// homeLabel is `$0` for the shell that prints a remote home.
const homeLabel = "hel-home"

// CacheHost is the machine that hosts a container target's caches (Rust
// enum CacheHost). SSH is nil for the machine running the controller (Rust
// CacheHost::Local), and names the connection otherwise (CacheHost::Ssh).
// Rust's `ssh()` accessor is the field itself.
type CacheHost struct {
	SSH *core.SshTarget
}

// IsLocal reports whether this is the machine running the controller (Rust
// `matches!(host, CacheHost::Local)`).
func (h CacheHost) IsLocal() bool { return h.SSH == nil }

// ResolveTargetInputPath resolves the login home on the machine that owns an
// editable path.
func ResolveTargetInputPath(target core.TargetTemplate, path string, executor core.CommandExecutor) (string, error) {
	needsHome, err := core.NeedsHome(path)
	if err != nil {
		return "", err
	}
	if !needsHome {
		return path, nil
	}
	host, err := CacheHostForPathTarget(target)
	if err != nil {
		return "", err
	}
	home, err := host.Home(executor)
	if err != nil {
		return "", err
	}
	return core.ExpandHome(path, &home)
}

// HostPlatform is a cache host's operating system, which decides which of
// its sessions can use what the host itself has installed.
type HostPlatform int

const (
	// HostPlatformLinux: containers run on the host's own kernel, so they can
	// mount its caches and run its mbx.
	HostPlatformLinux HostPlatform = iota
	// HostPlatformDarwin: bare sessions run on the Mac itself and use its
	// native tools. Its containers run Linux in a virtual machine, which can
	// neither run a macOS executable nor rely on the Mac's filesystems for
	// shared locks.
	HostPlatformDarwin
	// HostPlatformWindows: a Windows controller. It has no `uname` to ask,
	// runs no bare workers, and its container engine runs in a VM this
	// machine's paths do not reach.
	HostPlatformWindows
)

// Platform asks the host itself, not the controller or a container on that
// host.
func (h CacheHost) Platform(executor core.CommandExecutor) (HostPlatform, error) {
	panic("unported: mj-build-cache/src/cache_host.rs CacheHost::platform")
}

// CacheHostForTarget is the host whose caches a runtime target's containers
// mount (Rust CacheHost::for_target), or false for a target that has none.
func CacheHostForTarget(target *core.TargetsTargetTemplate) (CacheHost, bool) {
	switch target.Kind {
	case core.TargetsTargetTemplateLocalPodman, core.TargetsTargetTemplateLocalDocker,
		core.TargetsTargetTemplateAppleContainer:
		return CacheHost{}, true
	case core.TargetsTargetTemplateSshPodman, core.TargetsTargetTemplateSshDocker,
		core.TargetsTargetTemplateSshAppleContainer:
		ssh := target.SSH.Clone()
		return CacheHost{SSH: &ssh}, true
	}
	// The host's caches are directories a container mounts; a sandbox is a
	// VM and cannot, so it has no cache host. Bare targets have none either.
	return CacheHost{}, false
}

// CacheHostForMachine is the host a configured machine is, or false for a
// machine that has no standing host: an EC2 launch template creates an
// instance per session, so nothing persists between them to cache.
func CacheHostForMachine(machine core.Machine) (CacheHost, bool) {
	switch machine := machine.(type) {
	case core.MachineLocal:
		return CacheHost{}, true
	case core.MachineSsh:
		ssh := core.SshTargetFromSshConnection(&machine.SSH)
		return CacheHost{SSH: &ssh}, true
	}
	return CacheHost{}, false
}

// CacheHostForPathTarget is the host that owns a path a target's runtime
// reads. Unlike CacheHostForTarget, every target kind has one: a bare
// target's paths live on its machine, and an EC2 template's editable paths
// are this machine's, because no instance exists before a session starts.
func CacheHostForPathTarget(target core.TargetTemplate) (CacheHost, error) {
	panic("unported: mj-build-cache/src/cache_host.rs CacheHost::for_path_target")
}

// CacheHostForPathMachine is the host that owns a path on a configured
// machine. An EC2 machine has no standing instance, so its editable paths
// are this machine's.
func CacheHostForPathMachine(machine core.Machine) (CacheHost, error) {
	if machine, ok := machine.(core.MachineSsh); ok {
		return cacheHostForSSHInput(machine.SSH)
	}
	return CacheHost{}, nil
}

// cacheHostForSSHInput is an SSH host reached with the identity file as this
// machine reads it: `~` in a configured key path is the controller user's
// home.
func cacheHostForSSHInput(ssh core.SshConnection) (CacheHost, error) {
	if ssh.IdentityFile != nil {
		expanded, err := core.ExpandLocal(*ssh.IdentityFile)
		if err != nil {
			return CacheHost{}, err
		}
		ssh.IdentityFile = &expanded
	}
	target := core.SshTargetFromSshConnection(&ssh)
	return CacheHost{SSH: &target}, nil
}

// Home is the login home on this host, cached for ten minutes per host. Only
// a successful answer is cached; a host that was unreachable is retried.
func (h CacheHost) Home(executor core.CommandExecutor) (string, error) {
	panic("unported: mj-build-cache/src/cache_host.rs CacheHost::home")
}

// Key is an identity for this host, used to key cached host-side answers.
// Every runtime on one machine shares it.
func (h CacheHost) Key() string {
	if h.SSH == nil {
		return "local"
	}
	return "ssh:" + h.SSH.Destination
}

// Command runs remote, an argv, on the host.
func (h CacheHost) Command(remote []string, purpose string) core.CommandSpec {
	var command core.CommandSpec
	if h.SSH == nil {
		command = core.NewCommandSpec(remote[0], append([]string{}, remote[1:]...))
	} else {
		command = core.SSHCommandOwned(core.SshTrafficClassControl, h.SSH, remote)
	}
	return command.WithPurpose(purpose)
}

// ShellCommand runs script through `sh -c` on the host. label becomes `$0`,
// and arguments become `$1` onwards.
func (h CacheHost) ShellCommand(script, label string, arguments []string, purpose string) core.CommandSpec {
	remote := append([]string{"sh", "-c", script, label}, arguments...)
	return h.Command(remote, purpose)
}
