// Port of mj-core/src/config/daemon.rs.
//
// Where the daemon process runs: [daemon] in config.toml.

package core

import (
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"slices"
	"strings"

	"github.com/BurntSushi/toml"
)

// DaemonConfig is the [daemon] table, which selects where the daemon runs.
//
//	[daemon]
//	runtime = "podman"          # "host" (default), "podman", "docker" or "ssh"
//	image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"
//	mounts = ["/srv/projects"]  # optional host paths, mounted explicitly
//
// Image and Mounts apply only to a container runtime. A container otherwise
// has no host filesystem access; its data lives in a named volume.
type DaemonConfig struct {
	Runtime DaemonRuntime `json:"runtime" toml:"runtime"`
	// Host is the SSH destination, using the user's OpenSSH configuration.
	Host *string `json:"host,omitempty" toml:"host,omitempty"`
	// Instance is the remote instance; nil means the client's selected
	// instance.
	Instance *string  `json:"instance,omitempty" toml:"instance,omitempty"`
	SSHArgs  []string `json:"ssh_args,omitempty" toml:"ssh_args,omitempty"`
	// RemoteCommand is the remote executable; nil means `mj` on the login
	// shell PATH. Read it with RemoteCommandOrDefault.
	RemoteCommand *string `json:"remote_command,omitempty" toml:"remote_command,omitempty"`
	// Image is the daemon container's image. Nil is this build's agent-dev
	// image, which carries the harnesses local sessions run.
	Image *string `json:"image,omitempty" toml:"image,omitempty"`
	// Mounts is host directories explicitly mounted into the daemon container
	// at the same path. Nothing is mounted by default; the daemon's own data
	// lives in a named volume.
	Mounts []string `json:"mounts,omitempty" toml:"mounts,omitempty"`
}

// DefaultDaemonConfig is DaemonConfig's Default.
func DefaultDaemonConfig() DaemonConfig { return DaemonConfig{Runtime: DaemonRuntimeHost} }

func (d DaemonConfig) MarshalJSON() ([]byte, error) {
	var object serdeObject
	object.fieldUnless(d.Runtime.IsDefault(), "runtime", d.Runtime)
	object.fieldUnless(d.Host == nil, "host", d.Host)
	object.fieldUnless(d.Instance == nil, "instance", d.Instance)
	object.fieldUnless(len(d.SSHArgs) == 0, "ssh_args", d.SSHArgs)
	object.fieldUnless(d.RemoteCommand == nil, "remote_command", d.RemoteCommand)
	object.fieldUnless(d.Image == nil, "image", d.Image)
	object.fieldUnless(len(d.Mounts) == 0, "mounts", d.Mounts)
	return object.finish()
}

func (d *DaemonConfig) UnmarshalJSON(data []byte) error {
	type plain DaemonConfig
	value := plain(DefaultDaemonConfig())
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	*d = DaemonConfig(value)
	return nil
}

// DaemonConfigLoadPlacement reads only client-owned placement. Other tables
// are deliberately not deserialized: profiles and credentials belong to the
// daemon machine.
func DaemonConfigLoadPlacement(path string) (DaemonConfig, error) {
	contents, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return DefaultDaemonConfig(), nil
	}
	if err != nil {
		return DaemonConfig{}, fmt.Errorf("read placement %s: %w", path, err)
	}
	var document map[string]any
	metadata, err := toml.Decode(string(contents), &document)
	if err != nil {
		return DaemonConfig{}, fmt.Errorf("parse placement %s: %w", path, err)
	}
	placement := DefaultDaemonConfig()
	if table, ok := document["daemon"]; ok {
		data, err := tomlToJSON(table)
		if err == nil {
			err = json.Unmarshal(data, &placement)
		}
		if err != nil {
			return DaemonConfig{}, fmt.Errorf("parse placement %s: %w", path, err)
		}
	}
	// The other tables belong to the daemon, so only `[daemon]` keys are
	// this reader's to refuse; an SSH client reads `[daemon]` nowhere else.
	var unread []string
	for _, key := range metadata.Keys() {
		key := settingPath(key)
		if len(key) == 2 && key[0] == "daemon" && !daemonConfigKeys[key[1]] {
			unread = append(unread, strings.Join(key, "."))
		}
	}
	if err := refuseUnreadKeys(path, unread); err != nil {
		return DaemonConfig{}, err
	}
	return placement, nil
}

// DaemonConfigIsSetIn reports whether the file has a `[daemon]` table at
// all. A placement written as `runtime = "host"` equals the default once
// loaded, so only the file can tell an explicit choice from none.
func DaemonConfigIsSetIn(path string) (bool, error) {
	contents, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return false, nil
	}
	if err != nil {
		return false, fmt.Errorf("read placement %s: %w", path, err)
	}
	document, err := parseTOMLDocument(string(contents))
	if err != nil {
		return false, fmt.Errorf("parse placement %s: %w", path, err)
	}
	_, ok := document["daemon"]
	return ok, nil
}

// ReplacePlacement writes this placement as the file's `[daemon]` table,
// provided the file still holds expected: nil for no `[daemon]` table at all
// (an explicit `runtime = "host"` is a table, though it loads as the
// default), else the placement it loads as. The check and the write happen
// under one hold of the config lock, so two launches that each decided from
// the same file cannot both win: the second is refused instead of silently
// replacing the first. Every other table, comment and the order of the file
// stay as they are, and the rest of the file is not loaded: an SSH client's
// config belongs to the daemon machine and may hold tables this build must
// not reinterpret.
func (d DaemonConfig) ReplacePlacement(path string, expected *DaemonConfig) error {
	lock, err := configLockAcquire(path)
	if err != nil {
		return err
	}
	defer lock.Close()
	if err := configEnsureWritable(path); err != nil {
		return err
	}
	set, err := DaemonConfigIsSetIn(path)
	if err != nil {
		return err
	}
	var current *DaemonConfig
	if set {
		placement, err := DaemonConfigLoadPlacement(path)
		if err != nil {
			return err
		}
		current = &placement
	}
	if !daemonConfigOptionEqual(current, expected) {
		change := "it is no longer set"
		if current != nil {
			change = "it is now set"
			if current.Host != nil {
				change = "it now names " + *current.Host
			}
		}
		return fmt.Errorf("the daemon placement in %s changed while this was running: %s", path, change)
	}
	existing, err := os.ReadFile(path)
	if err != nil && !errors.Is(err, fs.ErrNotExist) {
		return fmt.Errorf("read placement %s: %w", path, err)
	}
	document, err := parseTomlEditDocument(string(existing))
	if err != nil {
		return fmt.Errorf("parse placement %s: %w", path, err)
	}
	// Only the non-default keys serialize, so the default placement removes
	// the table instead of leaving an empty one behind.
	serialized, err := marshalTOML(d)
	if err != nil {
		return fmt.Errorf("serialize daemon placement: %w", err)
	}
	keys, err := parseTomlEditDocument(serialized)
	if err != nil {
		return fmt.Errorf("reparse daemon placement: %w", err)
	}
	if len(keys.root.keys()) == 0 {
		document.root.remove("daemon")
	} else {
		// A file made here starts the way every file Mjolnir writes does.
		// Without the line, switching `runtime` back to `host` by hand would
		// leave a file the local daemon refuses to load.
		if strings.TrimSpace(string(existing)) == "" {
			tomlEditSet(document.root, "version", &tomlEditValue{kind: tomlEditInteger, repr: fmt.Sprint(ConfigVersion)})
		}
		tomlEditSet(document.root, "daemon", cloneTomlEditTable(keys.root))
	}
	return AtomicWrite(path, []byte(document.String()))
}

// daemonConfigOptionEqual is PartialEq for Option<&DaemonConfig>.
func daemonConfigOptionEqual(a, b *DaemonConfig) bool {
	if a == nil || b == nil {
		return a == b
	}
	return a.Runtime == b.Runtime && configOptionalEqual(a.Host, b.Host) && configOptionalEqual(a.Instance, b.Instance) &&
		slices.Equal(a.SSHArgs, b.SSHArgs) && configOptionalEqual(a.RemoteCommand, b.RemoteCommand) &&
		configOptionalEqual(a.Image, b.Image) && slices.Equal(a.Mounts, b.Mounts)
}

// RemoteInstance is the instance the remote `mj` runs as: the configured
// one, else the client's own. False is the remote's unnamed instance. It
// must not become `--instance default`, which selects `instances/default`, a
// different store from the one a plain `mj` on that machine uses.
func (d DaemonConfig) RemoteInstance() (string, bool) {
	if d.Instance != nil {
		return *d.Instance, true
	}
	return InstanceName()
}

// daemonConfigKeys is every key DaemonConfig reads. A deeper key belongs to
// the value of one of these, which reports a wrong shape itself.
var daemonConfigKeys = map[string]bool{
	"runtime": true, "host": true, "instance": true, "ssh_args": true,
	"remote_command": true, "image": true, "mounts": true,
}

// RemoteCommandOrDefault is Rust's DaemonConfig::remote_command (renamed: the
// field has that name in Go).
func (d DaemonConfig) RemoteCommandOrDefault() string {
	if d.RemoteCommand == nil {
		return "mj"
	}
	return *d.RemoteCommand
}

func (d DaemonConfig) IsDefault() bool {
	return d.Runtime == DaemonRuntimeHost && d.Host == nil && d.Instance == nil && len(d.SSHArgs) == 0 &&
		d.RemoteCommand == nil && d.Image == nil && len(d.Mounts) == 0
}

// DaemonRuntime is where `mj daemon-run` runs. Podman and Docker run it in a
// long-lived per-instance container of that engine. The container is its own
// machine: its data is a named volume, its network is private, and the only
// host directories it sees are the ones named in DaemonConfig.Mounts. Local
// sessions' workers run inside it.
type DaemonRuntime string

const (
	// DaemonRuntimeHost is a detached process on this computer. The default.
	DaemonRuntimeHost   DaemonRuntime = "host"
	DaemonRuntimePodman DaemonRuntime = "podman"
	DaemonRuntimeDocker DaemonRuntime = "docker"
	DaemonRuntimeSsh    DaemonRuntime = "ssh"
)

func (r DaemonRuntime) IsDefault() bool { return r == DaemonRuntimeHost }

// Engine is the container engine program, or false for host and SSH
// placement.
func (r DaemonRuntime) Engine() (string, bool) {
	switch r {
	case DaemonRuntimePodman:
		return "podman", true
	case DaemonRuntimeDocker:
		return "docker", true
	}
	return "", false
}

func (r *DaemonRuntime) UnmarshalText(text []byte) error {
	switch DaemonRuntime(text) {
	case DaemonRuntimeHost, DaemonRuntimePodman, DaemonRuntimeDocker, DaemonRuntimeSsh:
		*r = DaemonRuntime(text)
		return nil
	}
	return serdeUnknownVariant(string(text), "host", "podman", "docker", "ssh")
}
