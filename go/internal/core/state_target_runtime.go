// Port of mj-core/src/state/target_runtime.rs.
//
// Settings belonging to an already selected target, independent of its
// template.

package core

import (
	"bytes"
	"encoding/json"
	"fmt"
	"maps"
	"slices"
)

type TargetRuntimeSettings struct {
	Kind            string            `json:"kind"`
	Connection      TargetConnection  `json:"connection"`
	ExecutionPolicy ExecutionPolicy   `json:"execution_policy"`
	Environment     map[string]string `json:"environment"`
	// ToolCache is the shared native build caches selected when this
	// environment was created. Container mounts cannot follow subsequent
	// machine configuration edits.
	ToolCache *ToolCachePlacement `json:"tool_cache,omitempty"`
	// ContainerSettings is the immutable per-session selection used to
	// create this container. Nil on legacy containers: their applied
	// configuration is unknown.
	ContainerSettings *ContainerSettings `json:"container_settings,omitempty"`
}

func (s *TargetRuntimeSettings) UnmarshalJSON(data []byte) error {
	type plain TargetRuntimeSettings
	aux := struct {
		*plain
		Connection stateUnionJSON[TargetConnection] `json:"connection"`
	}{plain: (*plain)(s)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	if aux.Connection.V == nil {
		return fmt.Errorf("missing field `connection`")
	}
	s.Connection = aux.Connection.V
	return nil
}

// IsYolo reports whether the harness runs without approvals here: the
// recorded execution policy the worker launch enforces, so containers and
// SSH bare targets with permissions = "yolo" both qualify. The recorded
// policy is used instead of the template so a session keeps the mode it was
// launched with.
func (s *TargetRuntimeSettings) IsYolo() bool {
	return s.ExecutionPolicy == ExecutionPolicyUnconstrained
}

// ContainerSettings: desired settings and the applied creation snapshot
// share one representation.
type ContainerSettings struct {
	AdditionalMounts   []AdditionalMount         `json:"additional_mounts"`
	ResourceAllocation SessionResourceAllocation `json:"resource_allocation"`
	// DefaultImage is which image the container was created from, as far as
	// upgrades care: non-nil to a non-nil string for the default image of
	// the Mjolnir that created it, non-nil to nil for a configured image, and
	// nil for a container recorded before Mjolnir tracked this. Old
	// release-image strings remain readable. An explicit upgrade can use the
	// retained agent-dev tag for sessions created on that default. This
	// snapshot never authorizes container replacement.
	DefaultImage **string `json:"default_image,omitempty"`
}

func (s ContainerSettings) MarshalJSON() ([]byte, error) {
	type plain ContainerSettings
	if s.AdditionalMounts == nil {
		s.AdditionalMounts = []AdditionalMount{}
	}
	return json.Marshal(plain(s))
}

func (s *ContainerSettings) UnmarshalJSON(data []byte) error {
	type plain ContainerSettings
	aux := struct {
		*plain
		ResourceAllocation stateUnionJSON[SessionResourceAllocation] `json:"resource_allocation"`
		DefaultImage       json.RawMessage                           `json:"default_image"`
	}{plain: (*plain)(s)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	s.ResourceAllocation = aux.ResourceAllocation.V
	s.DefaultImage = nil
	if aux.DefaultImage != nil {
		image, err := recordedDefaultImage(aux.DefaultImage)
		if err != nil {
			return err
		}
		s.DefaultImage = image
	}
	return nil
}

// recordedDefaultImage: a present default_image, null included, is a
// recorded answer; only a missing one is unknown.
func recordedDefaultImage(raw json.RawMessage) (**string, error) {
	var image *string
	if string(bytes.TrimSpace(raw)) != "null" {
		var text string
		if err := json.Unmarshal(raw, &text); err != nil {
			return nil, err
		}
		image = &text
	}
	return &image, nil
}

// ContainerSettingsUpgradeSupported is a candidate for an explicit engine
// inspection; this grants no authority to replace the environment or infer
// what image it actually runs.
func ContainerSettingsUpgradeSupported(session *SessionRecord) bool {
	if !containerSettingsOwnsContainer(session) {
		return false
	}
	switch session.Target.(type) {
	case TargetLocatorAppleContainer, TargetLocatorSshAppleContainer:
		return false
	}
	return true
}

func ContainerSettingsRequested(session *SessionRecord) ContainerSettings {
	settings := ContainerSettings{
		AdditionalMounts:   slices.Clone(session.AdditionalMounts),
		ResourceAllocation: session.ResourceAllocation,
	}
	// A container on the default image follows the default to the one this
	// Mjolnir names; a configured image stays as it was.
	if applied := containerSettingsAppliedTo(session); applied != nil && applied.DefaultImage != nil {
		var image *string
		if *applied.DefaultImage != nil {
			family := defaultFamilyImage(**applied.DefaultImage)
			image = &family
		}
		settings.DefaultImage = &image
	}
	return settings
}

// ContainerSettingsApplied is the snapshot to record for a container just
// created from image.
func ContainerSettingsApplied(session *SessionRecord, image string) ContainerSettings {
	settings := ContainerSettingsRequested(session)
	marker := ContainerSettingsDefaultMarker(image)
	settings.DefaultImage = &marker
	return settings
}

// ContainerSettingsDefaultMarker is the default_image marker for a container
// running image: the image when it is the session default, nil for a
// configured image.
func ContainerSettingsDefaultMarker(image string) *string {
	if image == DefaultSessionImage {
		return &image
	}
	return nil
}

// ContainerSettingsUpgradeImage is the image an explicit upgrade replaces
// this session's container with. Sessions created on the old default follow
// the retained agent-dev tag when the target resolves to the session default,
// preserving their user, home and tools.
func ContainerSettingsUpgradeImage(session *SessionRecord, resolved string) string {
	applied := containerSettingsAppliedTo(session)
	if applied != nil && applied.DefaultImage != nil && *applied.DefaultImage != nil && resolved == DefaultSessionImage {
		return defaultFamilyImage(**applied.DefaultImage)
	}
	return resolved
}

func containerSettingsAppliedTo(session *SessionRecord) *ContainerSettings {
	if session.TargetRuntime == nil {
		return nil
	}
	return session.TargetRuntime.ContainerSettings
}

func containerSettingsOwnsContainer(session *SessionRecord) bool {
	if session.Target == nil {
		return false
	}
	switch session.Target.(type) {
	case TargetLocatorLocalPodman, TargetLocatorLocalDocker, TargetLocatorAppleContainer,
		TargetLocatorSshPodman, TargetLocatorSshDocker, TargetLocatorSshAppleContainer,
		TargetLocatorSshNitro:
		return session.Target.OwnsContainer()
	}
	return false
}

type ToolCachePlacement struct {
	Host      string `json:"host"`
	Directory string `json:"directory"`
	// NxHome: Nx 23.2+ owns the cache and its database together under ~/.nx.
	NxHome string `json:"nx_home"`
}

// TargetConnection is how to reach a selected target. Tag "kind", kebab-case.
type TargetConnection interface {
	isTargetConnection()
}

type TargetConnectionLocal struct{}

type TargetConnectionSsh struct {
	SSH SshConnection `json:"ssh"`
}

type TargetConnectionAws struct {
	Profile      string   `json:"profile"`
	Region       string   `json:"region"`
	SSHUser      string   `json:"ssh_user"`
	IdentityFile *string  `json:"identity_file"`
	SSHArgs      []string `json:"ssh_args"`
}

func (TargetConnectionLocal) isTargetConnection() {}
func (TargetConnectionSsh) isTargetConnection()   {}
func (TargetConnectionAws) isTargetConnection()   {}

func (TargetConnectionLocal) MarshalJSON() ([]byte, error) {
	return stateMarshalTagged("kind", "local", nil)
}

func (v TargetConnectionSsh) MarshalJSON() ([]byte, error) {
	type plain TargetConnectionSsh
	return stateMarshalTagged("kind", "ssh", plain(v))
}

func (v TargetConnectionAws) MarshalJSON() ([]byte, error) {
	type plain TargetConnectionAws
	if v.SSHArgs == nil {
		v.SSHArgs = []string{}
	}
	return stateMarshalTagged("kind", "aws", plain(v))
}

func unmarshalTargetConnection(data []byte) (TargetConnection, error) {
	tag, err := stateTag(data, "kind")
	if err != nil {
		return nil, err
	}
	switch tag {
	case "local":
		return TargetConnectionLocal{}, nil
	case "ssh":
		return stateVariant[TargetConnectionSsh, TargetConnection](data)
	case "aws":
		return stateVariant[TargetConnectionAws, TargetConnection](data)
	}
	return nil, fmt.Errorf("unknown variant `%s`", tag)
}

// WithCurrentSSHOptions is these recorded settings with the SSH options
// current now gives the same target, or nil when there is nothing to take
// from it.
//
// A session's recorded access pins where its worker lives: the host, the
// login user, and the port. The other options (a key, a ControlPath,
// keepalives, host-key policy) say how to connect, so they follow the
// machine's current configuration, as a resume does since J-21. Settings of
// another kind, or that reach another place, are not taken.
func (s *TargetRuntimeSettings) WithCurrentSSHOptions(current *TargetRuntimeSettings) *TargetRuntimeSettings {
	recordedConnection, ok := s.Connection.(TargetConnectionSsh)
	if !ok {
		return nil
	}
	nowConnection, ok := current.Connection.(TargetConnectionSsh)
	if !ok {
		return nil
	}
	recorded, now := recordedConnection.SSH, nowConnection.SSH
	if s.Kind != current.Kind || sshConnectionEqual(recorded, now) {
		return nil
	}
	location := func(ssh *SshConnection) ManagedWorktreeTargetSsh {
		destination := ssh.Host
		if ssh.User != nil {
			destination = *ssh.User + "@" + ssh.Host
		}
		return ManagedWorktreeTargetSsh{Destination: destination, SSHArgs: slices.Clone(ssh.ExtraArgs)}
	}
	if !location(&recorded).SameLocation(location(&now)) {
		return nil
	}
	refreshed := *s
	refreshed.Environment = maps.Clone(s.Environment)
	connection := recorded
	connection.IdentityFile = now.IdentityFile
	connection.ExtraArgs = slices.Clone(now.ExtraArgs)
	refreshed.Connection = TargetConnectionSsh{SSH: connection}
	return &refreshed
}

// TargetRuntimeSettingsFromTemplate is From<&TargetTemplate> for
// TargetRuntimeSettings (and, through its address, for the Arc form).
func TargetRuntimeSettingsFromTemplate(template TargetTemplate) TargetRuntimeSettings {
	var connection TargetConnection = TargetConnectionLocal{}
	switch t := template.(type) {
	case TargetTemplateSshBare:
		connection = TargetConnectionSsh{SSH: t.SSH}
	case TargetTemplateSshPodman:
		connection = TargetConnectionSsh{SSH: t.SSH}
	case TargetTemplateSshDocker:
		connection = TargetConnectionSsh{SSH: t.SSH}
	case TargetTemplateSshAppleContainer:
		connection = TargetConnectionSsh{SSH: t.SSH}
	case TargetTemplateSshNitro:
		connection = TargetConnectionSsh{SSH: t.SSH}
	case TargetTemplateAwsEc2:
		profile := "default"
		if t.AWSProfile != nil {
			profile = *t.AWSProfile
		}
		connection = TargetConnectionAws{
			Profile:      profile,
			Region:       t.Region,
			SSHUser:      t.SSHUser,
			IdentityFile: t.IdentityFile,
			SSHArgs:      slices.Clone(t.SSHArgs),
		}
	}
	environment := map[string]string{}
	if container := template.Container(); container != nil {
		environment = maps.Clone(container.Environment.Resolved())
		if environment == nil {
			environment = map[string]string{}
		}
	}
	return TargetRuntimeSettings{
		Kind:            template.KindName(),
		Connection:      connection,
		ExecutionPolicy: template.ExecutionPolicy(),
		Environment:     environment,
	}
}

// MarshalJSON writes an absent environment as an empty map, as serde
// writes a BTreeMap.
func (s TargetRuntimeSettings) MarshalJSON() ([]byte, error) {
	type plain TargetRuntimeSettings
	if s.Environment == nil {
		s.Environment = map[string]string{}
	}
	return json.Marshal(plain(s))
}

// defaultFamilyImage normalizes recorded defaults to the current session
// default or the retained agent-dev tag.
func defaultFamilyImage(recorded string) string {
	if recorded == DefaultSessionImage {
		return DefaultSessionImage
	}
	return LegacyDefaultContainerImage
}
