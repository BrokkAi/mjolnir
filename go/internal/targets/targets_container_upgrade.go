// Port of mj-targets/src/targets/container_upgrade.rs.

// Container replacement has no storage-destruction capability. In
// particular, none of these commands use provisioning traps, close plans, or
// `rm -v`.

package targets

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// RetainedContainer is a confirmed storage-preserving container upgrade: the
// source container's identity and the settings its replacement is created
// with. It is persisted as JSON (serde field names, declaration order).
type RetainedContainer struct {
	Locator              core.TargetsTargetLocator
	SourceID             string
	Name                 string
	BackupName           string
	CurrentImage         string
	currentImageID       string
	TargetImage          string
	PreparedImage        *string
	ImagePinReserved     bool
	selectedImageID      *string
	controlState         *controlState
	ControlStateRestored bool
	OperationID          string
	mounts               []retainedMount
	user                 string
	environment          []string
	workingDirectory     string
	memory               int64
	nanoCpus             int64
	pidsLimit            int64
	networkMode          string
}

// retainedContainerWire is RetainedContainer's serialized shape, in serde
// field order.
type retainedContainerWire struct {
	Locator              core.TargetsTargetLocator `json:"locator"`
	SourceID             string                    `json:"source_id"`
	Name                 string                    `json:"name"`
	BackupName           string                    `json:"backup_name"`
	CurrentImage         string                    `json:"current_image"`
	CurrentImageID       string                    `json:"current_image_id"`
	TargetImage          string                    `json:"target_image"`
	PreparedImage        *string                   `json:"prepared_image"`
	ImagePinReserved     bool                      `json:"image_pin_reserved"`
	SelectedImageID      *string                   `json:"selected_image_id"`
	ControlState         *controlState             `json:"control_state"`
	ControlStateRestored bool                      `json:"control_state_restored"`
	OperationID          string                    `json:"operation_id"`
	Mounts               []retainedMount           `json:"mounts"`
	User                 string                    `json:"user"`
	Environment          []string                  `json:"environment"`
	WorkingDirectory     string                    `json:"working_directory"`
	Memory               int64                     `json:"memory"`
	NanoCpus             int64                     `json:"nano_cpus"`
	PidsLimit            int64                     `json:"pids_limit"`
	NetworkMode          string                    `json:"network_mode"`
}

// MarshalJSON writes serde's shape, private fields included, with empty
// lists as [].
func (r RetainedContainer) MarshalJSON() ([]byte, error) {
	wire := retainedContainerWire{
		Locator: r.Locator, SourceID: r.SourceID, Name: r.Name, BackupName: r.BackupName,
		CurrentImage: r.CurrentImage, CurrentImageID: r.currentImageID, TargetImage: r.TargetImage,
		PreparedImage: r.PreparedImage, ImagePinReserved: r.ImagePinReserved,
		SelectedImageID: r.selectedImageID, ControlState: r.controlState,
		ControlStateRestored: r.ControlStateRestored, OperationID: r.OperationID,
		Mounts: r.mounts, User: r.user, Environment: r.environment,
		WorkingDirectory: r.workingDirectory, Memory: r.memory, NanoCpus: r.nanoCpus,
		PidsLimit: r.pidsLimit, NetworkMode: r.networkMode,
	}
	if wire.Mounts == nil {
		wire.Mounts = []retainedMount{}
	}
	if wire.Environment == nil {
		wire.Environment = []string{}
	}
	return json.Marshal(wire)
}

// UnmarshalJSON reads serde's shape: every field except the Options is
// required and may not be null.
func (r *RetainedContainer) UnmarshalJSON(data []byte) error {
	required := []string{
		"locator", "source_id", "name", "backup_name", "current_image", "current_image_id",
		"target_image", "image_pin_reserved", "control_state_restored", "operation_id", "mounts",
		"user", "environment", "working_directory", "memory", "nano_cpus", "pids_limit", "network_mode",
	}
	if _, err := containerUpgradeCheckFields(data, required, required); err != nil {
		return err
	}
	var wire retainedContainerWire
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	*r = RetainedContainer{
		Locator: wire.Locator, SourceID: wire.SourceID, Name: wire.Name, BackupName: wire.BackupName,
		CurrentImage: wire.CurrentImage, currentImageID: wire.CurrentImageID, TargetImage: wire.TargetImage,
		PreparedImage: wire.PreparedImage, ImagePinReserved: wire.ImagePinReserved,
		selectedImageID: wire.SelectedImageID, controlState: wire.ControlState,
		ControlStateRestored: wire.ControlStateRestored, OperationID: wire.OperationID,
		mounts: wire.Mounts, user: wire.User, environment: wire.Environment,
		workingDirectory: wire.WorkingDirectory, memory: wire.Memory, nanoCpus: wire.NanoCpus,
		pidsLimit: wire.PidsLimit, networkMode: wire.NetworkMode,
	}
	return nil
}

type controlState struct {
	Root             string   `json:"root"`
	StagingDirectory string   `json:"staging_directory"`
	Excludes         []string `json:"excludes"`
}

func (c controlState) MarshalJSON() ([]byte, error) {
	type plain controlState
	if c.Excludes == nil {
		c.Excludes = []string{}
	}
	return json.Marshal(plain(c))
}

func (c *controlState) UnmarshalJSON(data []byte) error {
	required := []string{"root", "staging_directory", "excludes"}
	if _, err := containerUpgradeCheckFields(data, required, required); err != nil {
		return err
	}
	type plain controlState
	var decoded plain
	if err := json.Unmarshal(data, &decoded); err != nil {
		return err
	}
	*c = controlState(decoded)
	return nil
}

type retainedMount struct {
	Kind        string `json:"kind"`
	Source      string `json:"source"`
	Destination string `json:"destination"`
	Writable    bool   `json:"writable"`
}

func (m *retainedMount) UnmarshalJSON(data []byte) error {
	required := []string{"kind", "source", "destination", "writable"}
	if _, err := containerUpgradeCheckFields(data, required, required); err != nil {
		return err
	}
	type plain retainedMount
	var decoded plain
	if err := json.Unmarshal(data, &decoded); err != nil {
		return err
	}
	*m = retainedMount(decoded)
	return nil
}

// inspection is the parts of `container inspect` an upgrade reads (serde
// rename_all = "PascalCase"; Id also accepts "ID").
type inspection struct {
	ID         string         `json:"Id"`
	Name       string         `json:"Name"`
	Image      string         `json:"Image"`
	ImageName  string         `json:"ImageName"`
	Config     inspectConfig  `json:"Config"`
	HostConfig inspectHost    `json:"HostConfig"`
	Mounts     []inspectMount `json:"Mounts"`
	State      inspectState   `json:"State"`
}

func (i *inspection) UnmarshalJSON(data []byte) error {
	fields, err := containerUpgradeCheckFields(data,
		[]string{"Name", "Image", "Config", "HostConfig", "Mounts", "State"},
		[]string{"Id", "ID", "Name", "Image", "ImageName", "Config", "HostConfig", "Mounts", "State"})
	if err != nil {
		return err
	}
	_, id := fields["Id"]
	_, alias := fields["ID"]
	if !id && !alias {
		return errors.New("missing field `Id`")
	}
	type plain inspection
	var decoded plain
	if err := json.Unmarshal(data, &decoded); err != nil {
		return err
	}
	*i = inspection(decoded)
	return nil
}

type inspectConfig struct {
	Image      string            `json:"Image"`
	Labels     map[string]string `json:"Labels"`
	User       string            `json:"User"`
	Env        []string          `json:"Env"`
	WorkingDir string            `json:"WorkingDir"`
	Cmd        []string          `json:"Cmd"`
	Entrypoint []string          `json:"Entrypoint"`
}

func (c *inspectConfig) UnmarshalJSON(data []byte) error {
	if _, err := containerUpgradeCheckFields(data,
		[]string{"Image"},
		[]string{"Image", "Labels", "User", "Env", "WorkingDir"}); err != nil {
		return err
	}
	type plain inspectConfig
	var decoded plain
	if err := json.Unmarshal(data, &decoded); err != nil {
		return err
	}
	*c = inspectConfig(decoded)
	return nil
}

// inspectHost is HostConfig; every field defaults when missing (serde
// default on the struct).
type inspectHost struct {
	AutoRemove     bool     `json:"AutoRemove"`
	Memory         int64    `json:"Memory"`
	NanoCpus       int64    `json:"NanoCpus"`
	PidsLimit      *int64   `json:"PidsLimit"`
	NetworkMode    string   `json:"NetworkMode"`
	Privileged     bool     `json:"Privileged"`
	ReadonlyRootfs bool     `json:"ReadonlyRootfs"`
	Devices        any      `json:"Devices"`
	PortBindings   any      `json:"PortBindings"`
	UsernsMode     string   `json:"UsernsMode"`
	SecurityOpt    []string `json:"SecurityOpt"`
	CapAdd         []string `json:"CapAdd"`
	CapDrop        []string `json:"CapDrop"`
}

func (h *inspectHost) UnmarshalJSON(data []byte) error {
	if _, err := containerUpgradeCheckFields(data, nil,
		[]string{"AutoRemove", "Memory", "NanoCpus", "NetworkMode", "Privileged", "ReadonlyRootfs", "UsernsMode"}); err != nil {
		return err
	}
	type plain inspectHost
	var decoded plain
	if err := json.Unmarshal(data, &decoded); err != nil {
		return err
	}
	*h = inspectHost(decoded)
	return nil
}

type inspectMount struct {
	Kind        string   `json:"Type"`
	Name        string   `json:"Name"`
	Source      string   `json:"Source"`
	Destination string   `json:"Destination"`
	Writable    bool     `json:"RW"`
	Options     []string `json:"Options"`
	Propagation string   `json:"Propagation"`
}

func (m *inspectMount) UnmarshalJSON(data []byte) error {
	if _, err := containerUpgradeCheckFields(data,
		[]string{"Type", "Source", "Destination", "RW"},
		[]string{"Type", "Name", "Source", "Destination", "RW", "Options", "Propagation"}); err != nil {
		return err
	}
	type plain inspectMount
	var decoded plain
	if err := json.Unmarshal(data, &decoded); err != nil {
		return err
	}
	*m = inspectMount(decoded)
	return nil
}

type inspectState struct {
	Running bool `json:"Running"`
}

func (s *inspectState) UnmarshalJSON(data []byte) error {
	required := []string{"Running"}
	if _, err := containerUpgradeCheckFields(data, required, required); err != nil {
		return err
	}
	type plain inspectState
	var decoded plain
	if err := json.Unmarshal(data, &decoded); err != nil {
		return err
	}
	*s = inspectState(decoded)
	return nil
}

// containerUpgradeCheckFields makes the checks serde's derived Deserialize
// makes that encoding/json does not: each required field is present, and a
// field that is not an Option is not null. It returns the object's members.
func containerUpgradeCheckFields(data []byte, required []string, nonNull []string) (map[string]json.RawMessage, error) {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil {
		return nil, err
	}
	if fields == nil {
		return nil, errors.New("invalid type: null, expected a struct")
	}
	for _, name := range required {
		if _, ok := fields[name]; !ok {
			return nil, fmt.Errorf("missing field `%s`", name)
		}
	}
	for _, name := range nonNull {
		if raw, ok := fields[name]; ok && bytes.Equal(bytes.TrimSpace(raw), []byte("null")) {
			return nil, fmt.Errorf("invalid type: null, expected a value for field `%s`", name)
		}
	}
	return fields, nil
}

// validateRetainedEnvironment: the engine env-file parser has a 64 KiB line
// buffer. A total environment may exceed that, but an individual assignment
// must fit before acceptance.
func validateRetainedEnvironment(assignment string) error {
	name, value, ok := strings.Cut(assignment, "=")
	if !ok {
		return errors.New("container environment is not an assignment")
	}
	if err := core.ValidateInheritedEnvironmentName(name); err != nil {
		return err
	}
	if strings.ContainsAny(value, "\n\r") || len(assignment) >= 64*1024-1 {
		return errors.New("this container has a multiline or oversized environment entry that the engine env-file format cannot preserve; the existing container is unchanged")
	}
	return nil
}

func hostAndName(locator *core.TargetsTargetLocator) (core.ImageHost, string, error) {
	if core.IsBorrowed(locator) {
		return core.ImageHost{}, "", errors.New("a borrowed container must be upgraded through its owning session")
	}
	sshHost := func(kind core.ImageHostKind) core.ImageHost {
		ssh := locator.SSH.Clone()
		return core.ImageHost{Kind: kind, SSH: &ssh}
	}
	switch locator.Kind {
	case core.TargetsTargetLocatorLocalPodman:
		return core.ImageHost{Kind: core.ImageHostLocalPodman}, locator.ContainerID, nil
	case core.TargetsTargetLocatorLocalDocker:
		return core.ImageHost{Kind: core.ImageHostLocalDocker}, locator.ContainerID, nil
	case core.TargetsTargetLocatorSshPodman:
		return sshHost(core.ImageHostSshPodman), locator.ContainerID, nil
	case core.TargetsTargetLocatorSshDocker:
		return sshHost(core.ImageHostSshDocker), locator.ContainerID, nil
	case core.TargetsTargetLocatorSshNitro:
		return sshHost(core.ImageHostSshNitro), locator.ContainerID, nil
	}
	return core.ImageHost{}, "", errors.New("storage-preserving container upgrades support owned Podman, Docker and Nitro sandbox containers; this target is unsupported")
}

// inspect reads the named container, or nil when the engine says it does
// not exist.
func inspect(host *core.ImageHost, name string, executor core.CommandExecutor) (*inspection, error) {
	panic("unported: mj-targets/src/targets/container_upgrade.rs inspect")
}

func owned(record *inspection, sessionID string) error {
	managed, managedOK := record.Config.Labels["dev.mj.managed"]
	session, sessionOK := record.Config.Labels["dev.mj.session"]
	if !managedOK || managed != "true" || !sessionOK || session != sessionID {
		return errors.New("refusing to replace a foreign container")
	}
	return nil
}

func storageUnavailable(message string) core.Refusal {
	return core.RefusalPrecondition(message).WithCode("container-storage-not-preserved")
}

// InspectRetainedContainer inspects the session's container and plans its
// storage-preserving replacement with targetImage, refusing anything the
// replacement could not reproduce. workspace and workerRoot are target-side
// paths.
func InspectRetainedContainer(locator *core.TargetsTargetLocator, sessionID string, targetImage string, operationID string, workspace string, workerRoot string, executor core.CommandExecutor) (RetainedContainer, error) {
	panic("unported: mj-targets/src/targets/container_upgrade.rs inspect_retained_container")
}

func mountConflict(destination string, retained string) core.Refusal {
	return core.RefusalPrecondition(fmt.Sprintf("attachment %s overlaps %s", destination, retained)).
		WithCode("container-mount-conflict")
}

// trimSha256Prefixes is Rust's `trim_start_matches("sha256:")`, which
// removes every leading repetition.
func trimSha256Prefixes(id string) string {
	for strings.HasPrefix(id, "sha256:") {
		id = id[len("sha256:"):]
	}
	return id
}

// ReplacesSettings reports whether this is a settings change on the same
// image: explicit image upgrades only accept a different immutable image.
func (r *RetainedContainer) ReplacesSettings() bool {
	return r.selectedImageID != nil && trimSha256Prefixes(*r.selectedImageID) == trimSha256Prefixes(r.currentImageID)
}

// ImageChanged reports whether the prepared image differs from the source's.
func (r *RetainedContainer) ImageChanged() bool {
	return r.PreparedImage != nil && trimSha256Prefixes(*r.PreparedImage) != trimSha256Prefixes(r.currentImageID)
}

// ApplySettings turns this plan into a settings change on the current image:
// applied are the attachments the running container has, change the ones
// and the allocation the replacement gets. workspace is a target-side path.
func (r *RetainedContainer) ApplySettings(applied []core.AdditionalMount, change *core.ContainerSettingsChange, workspace string, template *core.ContainerTemplate) error {
	panic("unported: mj-targets/src/targets/container_upgrade.rs RetainedContainer::apply_settings")
}

// SettingsPreview is Preview for a settings change.
func (r *RetainedContainer) SettingsPreview(sessionID string, settings core.ContainerSettingsChange) core.ContainerUpgradePreview {
	preview := r.Preview(sessionID)
	preview.Settings = &settings
	preview.Available = true
	return preview
}

// Preview is what the upgrade confirmation shows.
func (r *RetainedContainer) Preview(sessionID string) core.ContainerUpgradePreview {
	available := r.CurrentImage != r.TargetImage
	if r.PreparedImage != nil {
		available = trimSha256Prefixes(*r.PreparedImage) != trimSha256Prefixes(r.currentImageID)
	}
	retainedMounts := make([]string, 0, len(r.mounts))
	for _, mount := range r.mounts {
		retainedMounts = append(retainedMounts, mount.Destination)
	}
	return core.ContainerUpgradePreview{
		Warning:           core.ContainerUpgradeWarning,
		Settings:          nil,
		SessionID:         sessionID,
		SourceContainerID: r.SourceID,
		CurrentImage:      r.CurrentImage,
		TargetImage:       r.TargetImage,
		CurrentImageID:    r.currentImageID,
		TargetImageID:     r.selectedImageID,
		Available:         available,
		RetainedMounts:    retainedMounts,
	}
}

// CaptureControlState copies only the stopped worker's private control tree.
// Every independent mount beneath it is excluded, including build caches.
func (r *RetainedContainer) CaptureControlState(executor core.CommandExecutor) error {
	panic("unported: mj-targets/src/targets/container_upgrade.rs RetainedContainer::capture_control_state")
}

// RestoreControlState extracts the captured control tree into the
// replacement, once.
func (r *RetainedContainer) RestoreControlState(executor core.CommandExecutor) error {
	panic("unported: mj-targets/src/targets/container_upgrade.rs RetainedContainer::restore_control_state")
}

// SourceIsCurrent reports whether the container under this plan's name is
// still the confirmed source.
func (r *RetainedContainer) SourceIsCurrent(executor core.CommandExecutor) (bool, error) {
	host, _, err := hostAndName(&r.Locator)
	if err != nil {
		return false, err
	}
	record, err := inspect(&host, r.Name, executor)
	if err != nil {
		return false, err
	}
	return record != nil && record.ID == r.SourceID, nil
}

func (r *RetainedContainer) imagePin() string {
	return fmt.Sprintf("localhost/mj-container-upgrade/%s:%s", r.Name, r.OperationID)
}

// ReleaseImagePin drops the private tag that keeps the prepared image from
// being pruned.
func (r *RetainedContainer) ReleaseImagePin(executor core.CommandExecutor) error {
	panic("unported: mj-targets/src/targets/container_upgrade.rs RetainedContainer::release_image_pin")
}

// Prepare pulls and pins the target image, and checks it can receive the
// control state, before anything is stopped.
func (r *RetainedContainer) Prepare(executor core.CommandExecutor) error {
	panic("unported: mj-targets/src/targets/container_upgrade.rs RetainedContainer::prepare")
}

// Replace is idempotent after the durable replacement boundary. The stopped
// source remains a rollback resource until the new worker is ready.
func (r *RetainedContainer) Replace(sessionID string, executor core.CommandExecutor) error {
	panic("unported: mj-targets/src/targets/container_upgrade.rs RetainedContainer::replace")
}

// RetireBackupForDestroy removes the stopped source a destroy no longer
// needs.
func (r *RetainedContainer) RetireBackupForDestroy(sessionID string, executor core.CommandExecutor) error {
	host, _, err := hostAndName(&r.Locator)
	if err != nil {
		return err
	}
	if host.Kind == core.ImageHostSshNitro {
		// The previous root disk is part of the VM, which Destroy removes.
		return nil
	}
	backup, err := inspect(&host, r.BackupName, executor)
	if err != nil || backup == nil {
		return err
	}
	if err := owned(backup, sessionID); err != nil {
		return err
	}
	if backup.ID != r.SourceID || backup.State.Running {
		return errors.New("refusing to retire a changed or running backup")
	}
	return checkedContainerStateChange(&host, "rm", r.SourceID, containerStateGoalAbsent, executor)
}

// Finish retires the source and the control-state transfer once the
// replacement's worker is ready.
func (r *RetainedContainer) Finish(sessionID string, executor core.CommandExecutor) error {
	panic("unported: mj-targets/src/targets/container_upgrade.rs RetainedContainer::finish")
}

// The Nitro engine replaces the root disk of the one VM in place. Two VMs
// may not list a volume (two kernels would corrupt its ext4 image), so the
// Podman sequence of renaming the source and creating a second container on
// its volumes has no engine equivalent. The engine's `replace-root` verbs
// keep the same guarantees: the old root disk stays beside the new one until
// `release`, the staged image is pinned against `image prune`, every step is
// repeatable, and the VM's id changes at the swap, so SourceIsCurrent and
// the label check in Replace tell source from replacement as they do for
// Podman.

func (r *RetainedContainer) nitroVerb(verb string, extra []string) []string {
	args := []string{"container", "replace-root", verb, "--operation", r.OperationID}
	args = append(args, extra...)
	return append(args, r.Name)
}

// nitroStage pins the prepared image in the VM's record. Unlike the Podman
// tag, the pin cannot outlive the VM. The engine also checks the image's root
// filesystem for what the control-state transfer needs (the Podman
// preflight's `run --rm` probe) and refuses before anything is stopped.
func (r *RetainedContainer) nitroStage(host *core.ImageHost, executor core.CommandExecutor) error {
	if r.PreparedImage == nil {
		return errors.New("container image has not been prepared")
	}
	var extra []string
	for _, label := range []string{"dev.mj.container-upgrade=" + r.OperationID, "dev.mj.container-image=" + r.TargetImage} {
		extra = append(extra, "--label", label)
	}
	args := append(r.nitroVerb("stage", extra), trimSha256Prefixes(*r.PreparedImage))
	_, err := checked(host, args, executor)
	return err
}

// nitroRelease ends the operation: drops the previous root disk once the
// replacement is swapped in, or just the pin when it never was. Repeating it
// is safe.
func (r *RetainedContainer) nitroRelease(host *core.ImageHost, executor core.CommandExecutor) error {
	_, err := checked(host, r.nitroVerb("release", nil), executor)
	return err
}

func (r *RetainedContainer) nitroReplace(host *core.ImageHost, sessionID string, executor core.CommandExecutor) error {
	panic("unported: mj-targets/src/targets/container_upgrade.rs RetainedContainer::nitro_replace")
}

type containerStateGoal int

const (
	containerStateGoalNotRunning containerStateGoal = iota
	containerStateGoalAbsent
)

func checkedContainerStateChange(host *core.ImageHost, verb string, containerID string, goal containerStateGoal, executor core.CommandExecutor) error {
	panic("unported: mj-targets/src/targets/container_upgrade.rs checked_container_state_change")
}

func checked(host *core.ImageHost, args []string, executor core.CommandExecutor) (core.CommandOutput, error) {
	args = append([]string{host.Engine()}, args...)
	command := host.Command(args, "upgrade container while retaining storage")
	output, err := executor.Execute(&command)
	if err != nil {
		return core.CommandOutput{}, err
	}
	if output.Status != 0 {
		return core.CommandOutput{}, fmt.Errorf("container upgrade failed; storage retained: %s", preflightLossy(output.Stderr))
	}
	return output, nil
}

// ContainerUpgradeTestPlan is a confirmed upgrade of a local Podman session
// container, for tests.
func ContainerUpgradeTestPlan(sessionID string) RetainedContainer {
	name, err := core.ResourceName(sessionID)
	if err != nil {
		panic(err)
	}
	prepared := "sha256:target"
	selected := "sha256:target"
	return RetainedContainer{
		Locator: core.TargetsTargetLocator{
			Kind:        core.TargetsTargetLocatorLocalPodman,
			ContainerID: name,
			WorkspaceStorage: core.TargetsPodmanWorkspaceLocator{
				Kind: core.TargetsPodmanWorkspaceLocatorVolume,
				Name: name + "-workspace",
			},
		},
		SourceID:         "source-container",
		Name:             name,
		BackupName:       name + "-upgrade-upgrade-test",
		CurrentImage:     "agent-dev:2.51.0",
		currentImageID:   "sha256:source",
		TargetImage:      "agent-dev:2.52.0",
		PreparedImage:    &prepared,
		ImagePinReserved: false,
		selectedImageID:  &selected,
		controlState: &controlState{
			Root:             "/var/lib/hel",
			StagingDirectory: "/tmp/.mj-container-upgrade-test",
			Excludes:         []string{},
		},
		ControlStateRestored: false,
		OperationID:          "upgrade-test",
		mounts: []retainedMount{{
			Kind:        "volume",
			Source:      name + "-workspace",
			Destination: "/workspace",
			Writable:    true,
		}},
		user:             "0:0",
		environment:      []string{},
		workingDirectory: "/workspace",
		memory:           0,
		nanoCpus:         0,
		pidsLimit:        16384,
		networkMode:      "bridge",
	}
}

// retainedVolumeExists proves absence before creating, and ownership before
// reattaching. Engine failure never turns surviving workspace data into a
// checkpoint restore.
func retainedVolumeExists(host *core.ImageHost, name string, session string, executor core.CommandExecutor) (bool, error) {
	panic("unported: mj-targets/src/targets/container_upgrade.rs retained_volume_exists")
}

func retainLegacyVolume(host *core.ImageHost, name string, session string, executor core.CommandExecutor) (bool, error) {
	existed, err := retainedVolumeExists(host, name, session, executor)
	if err != nil || existed {
		return existed, err
	}
	if _, err := checked(host, []string{"volume", "create", "--label=dev.mj.managed=true", "--label=dev.mj.session=" + session, name}, executor); err != nil {
		return false, err
	}
	exists, err := retainedVolumeExists(host, name, session, executor)
	if err != nil {
		return false, err
	}
	if !exists {
		return false, errors.New("cannot verify created volume ownership")
	}
	return false, nil
}

// RetainedWorkspaceVolumeExists is read-only evidence for Resume; the
// replacement uses this same volume owner.
func RetainedWorkspaceVolumeExists(locator *core.TargetsTargetLocator, session string, executor core.CommandExecutor) (bool, error) {
	switch locator.Kind {
	case core.TargetsTargetLocatorLocalPodman, core.TargetsTargetLocatorSshPodman:
		if locator.WorkspaceStorage.EffectiveKind() == core.TargetsPodmanWorkspaceLocatorVolume {
			host, _, err := hostAndName(locator)
			if err != nil {
				return false, err
			}
			return retainedVolumeExists(&host, locator.WorkspaceStorage.Name, session, executor)
		}
	}
	return false, nil
}

// RecoverRemovedContainer is recovery only for a shipped recreation that
// already removed its source. Existing workspace storage wins over the
// archive, even if it contains files the archive never covered. No failure
// cleanup removes any storage. It reports whether the repositories must be
// restored from the checkpoint.
func RecoverRemovedContainer(locator *core.TargetsTargetLocator, template *core.TargetsContainerTemplate, sessionID string, mounts []core.AdditionalMount, workspace string, requireExistingWorkspace bool, executor core.CommandExecutor) (bool, error) {
	panic("unported: mj-targets/src/targets/container_upgrade.rs recover_removed_container")
}
