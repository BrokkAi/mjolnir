// Port of mj-core/src/config/daemon.rs.
//
// Where the daemon process runs: [daemon] in config.toml.

package core

import "encoding/json"

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
	panic("unported: mj-core/src/config/daemon.rs DaemonConfig::load_placement")
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
