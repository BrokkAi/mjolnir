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
	"log/slog"
	"math"
	"os"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"unicode"
	"unicode/utf8"

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
	// `container exists` is Podman-specific. Both engines use inspect's exit 1
	// for absence; any other failure remains an error rather than a fallback.
	command := host.Command([]string{host.Engine(), "container", "inspect", name}, "inspect retained session container")
	output, err := executor.Execute(&command)
	if err != nil {
		return nil, err
	}
	missing := preflightASCIILower(preflightLossy(output.Stderr))
	if output.Status != 0 &&
		(strings.Contains(missing, "no such container") ||
			strings.Contains(missing, "no such object") ||
			strings.Contains(missing, "no container with name or id")) {
		return nil, nil
	}
	if output.Status != 0 {
		return nil, fmt.Errorf("inspect retained container %s: %s", name, preflightLossy(output.Stderr))
	}
	var records []inspection
	if err := json.Unmarshal(output.Stdout, &records); err != nil {
		return nil, fmt.Errorf("decode container inspection: %w", err)
	}
	if len(records) != 1 {
		return nil, errors.New("container inspection did not identify exactly one container")
	}
	return &records[0], nil
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
	host, name, err := hostAndName(locator)
	if err != nil {
		return RetainedContainer{}, err
	}
	record, err := inspect(&host, name, executor)
	if err != nil {
		return RetainedContainer{}, err
	}
	if record == nil {
		return RetainedContainer{}, errors.New("the existing container is missing; Restart and upgrade cannot reconstruct its filesystem")
	}
	if err := owned(record, sessionID); err != nil {
		return RetainedContainer{}, err
	}
	entrypoint := record.Config.Entrypoint
	command := record.Config.Cmd
	if !((len(entrypoint) == 0 &&
		(slices.Equal(command, []string{"sh", "-c", keepaliveScript}) ||
			slices.Equal(command, []string{"sleep", "infinity"}))) ||
		(slices.Equal(entrypoint, []string{"sh"}) && slices.Equal(command, []string{"-c", keepaliveScript}))) {
		return RetainedContainer{}, storageUnavailable("container upgrade cannot safely restart this custom container entrypoint; retain the current environment")
	}
	nonempty := func(value any) bool {
		switch value := value.(type) {
		case nil:
			return false
		case []any:
			return len(value) != 0
		case map[string]any:
			return len(value) != 0
		default:
			return true
		}
	}
	hostConfig := &record.HostConfig
	capabilitiesKept := true
	for _, capability := range hostConfig.CapAdd {
		if !slices.Contains(containerCapabilities(host.Engine()), strings.TrimPrefix(capability, "CAP_")) {
			capabilitiesKept = false
		}
	}
	optionsKept := true
	for _, option := range hostConfig.SecurityOpt {
		if !slices.Contains(containerSecurityOpts(host.Engine()), option) {
			optionsKept = false
		}
	}
	if hostConfig.Privileged ||
		hostConfig.ReadonlyRootfs ||
		nonempty(hostConfig.Devices) ||
		nonempty(hostConfig.PortBindings) ||
		hostConfig.UsernsMode != "" ||
		!capabilitiesKept ||
		len(hostConfig.CapDrop) != 0 ||
		!optionsKept ||
		strings.HasPrefix(hostConfig.NetworkMode, "container:") {
		return RetainedContainer{}, storageUnavailable("storage-preserving upgrades do not support this container's device, privilege, port, or namespace overrides; the existing container is unchanged")
	}
	for _, assignment := range record.Config.Env {
		if err := validateRetainedEnvironment(assignment); err != nil {
			return RetainedContainer{}, err
		}
	}
	if hostConfig.AutoRemove {
		return RetainedContainer{}, storageUnavailable("cannot preserve storage of an auto-remove container")
	}
	if strings.TrimLeft(record.Name, "/") != name {
		return RetainedContainer{}, errors.New("container locator must name its managed container")
	}
	mounts := make([]retainedMount, 0, len(record.Mounts))
	for _, mount := range record.Mounts {
		unsupportedOption := slices.ContainsFunc(mount.Options, func(option string) bool {
			return option == "O" || option == "z" || option == "Z" || option == "noexec" ||
				strings.Contains(option, "overlay") || strings.Contains(option, "idmap")
		})
		if !((mount.Kind == "volume" || mount.Kind == "bind") &&
			!unsupportedOption &&
			(mount.Propagation == "" || mount.Propagation == "private" || mount.Propagation == "rprivate")) {
			return RetainedContainer{}, storageUnavailable(fmt.Sprintf("cannot preserve transient or unsupported mount at %s (%s)", mount.Destination, mount.Kind))
		}
		source := mount.Source
		if mount.Kind == "volume" {
			source = mount.Name
		}
		if source == "" || strings.ContainsAny(source, ",\n") || strings.ContainsAny(mount.Destination, ",\n") {
			return RetainedContainer{}, fmt.Errorf("mount at %s cannot be represented safely for replacement", mount.Destination)
		}
		mounts = append(mounts, retainedMount{
			Kind:        mount.Kind,
			Source:      source,
			Destination: mount.Destination,
			Writable:    mount.Writable,
		})
	}
	slices.SortStableFunc(mounts, func(left, right retainedMount) int {
		return strings.Compare(left.Destination, right.Destination)
	})
	retainedPath := func(path string) bool {
		return slices.ContainsFunc(mounts, func(mount retainedMount) bool {
			return mount.Writable && upgradePathStartsWith(path, mount.Destination)
		})
	}
	if !retainedPath(workspace) {
		return RetainedContainer{}, storageUnavailable(fmt.Sprintf("cannot preserve workspace: %s lives in the container writable layer; use independent workspace storage before upgrading", workspace))
	}
	const controlRoot = "/var/lib/hel"
	var control *controlState
	if !(retainedPath(workerRoot) && retainedPath(core.Join(controlRoot, "profiles"))) {
		if !upgradePathStartsWith(workerRoot, controlRoot) {
			return RetainedContainer{}, errors.New("cannot transfer this worker's unsupported control-state layout")
		}
		base := workspace
		if retainedPath("/tmp") {
			base = "/tmp"
		}
		excludes := []string{}
		for _, mount := range mounts {
			relative, ok := upgradePathStripPrefix(mount.Destination, controlRoot)
			if ok && relative != "" {
				excludes = append(excludes, "./"+core.Text(relative))
			}
		}
		control = &controlState{
			Root:             controlRoot,
			StagingDirectory: core.Text(core.Join(base, ".mj-container-upgrade-"+operationID)),
			Excludes:         excludes,
		}
	}
	if control != nil {
		probe := core.LocatorCommand(core.SshTrafficClassControl, locator, []string{"sh", "-c", "tar --help 2>&1"}).
			WithPurpose("verify control-state transfer support")
		output, err := executor.Execute(&probe)
		if err != nil {
			return RetainedContainer{}, err
		}
		help := preflightLossy(output.Stdout)
		supported := output.Status == 0
		for _, option := range []string{"--one-file-system", "--unlink-first", "--strip-components", "--no-recursion", "--occurrence"} {
			if !strings.Contains(help, option) {
				supported = false
			}
		}
		if !supported {
			return RetainedContainer{}, storageUnavailable("this container lacks the tar support needed to preserve worker state; the source is unchanged")
		}
	}
	cachedCommand := host.Command([]string{host.Engine(), "image", "inspect", "--format", "{{.Id}}", targetImage}, "inspect selected upgrade image")
	cached, err := executor.Execute(&cachedCommand)
	if err != nil {
		return RetainedContainer{}, err
	}
	var preparedImage *string
	if cached.Status == 0 {
		text, err := upgradeUTF8(cached.Stdout)
		if err != nil {
			return RetainedContainer{}, err
		}
		id := strings.TrimSpace(text)
		preparedImage = &id
	} else {
		message := strings.ToLower(preflightLossy(cached.Stderr))
		if !(strings.Contains(message, "no such image") ||
			strings.Contains(message, "image not known") ||
			strings.Contains(message, "image not found")) {
			return RetainedContainer{}, fmt.Errorf("cannot inspect selected image: %s", message)
		}
	}
	currentImage := record.Config.Image
	if selected, ok := record.Config.Labels["dev.mj.container-image"]; ok {
		currentImage = selected
	} else if record.ImageName != "" {
		currentImage = record.ImageName
	}
	var selectedImageID *string
	if preparedImage != nil {
		selected := *preparedImage
		selectedImageID = &selected
	}
	var pidsLimit int64
	if hostConfig.PidsLimit != nil {
		pidsLimit = *hostConfig.PidsLimit
	}
	environment := record.Config.Env
	if environment == nil {
		environment = []string{}
	}
	return RetainedContainer{
		Locator:              upgradeCloneLocator(locator),
		SourceID:             record.ID,
		Name:                 name,
		BackupName:           name + "-upgrade-" + operationID,
		currentImageID:       record.Image,
		CurrentImage:         currentImage,
		TargetImage:          targetImage,
		controlState:         control,
		ControlStateRestored: false,
		selectedImageID:      selectedImageID,
		PreparedImage:        preparedImage,
		ImagePinReserved:     false,
		OperationID:          operationID,
		mounts:               mounts,
		user:                 record.Config.User,
		environment:          environment,
		workingDirectory:     record.Config.WorkingDir,
		memory:               hostConfig.Memory,
		nanoCpus:             hostConfig.NanoCpus,
		pidsLimit:            pidsLimit,
		networkMode:          hostConfig.NetworkMode,
	}, nil
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
	overlaps := func(a, b string) bool { return upgradePathStartsWith(a, b) || upgradePathStartsWith(b, a) }
	protect := func(destination string) error {
		for _, path := range []string{workspace, "/var/lib/hel"} {
			if overlaps(destination, path) {
				return mountConflict(destination, path)
			}
		}
		return nil
	}
	containsMount := func(mounts []core.AdditionalMount, mount core.AdditionalMount) bool {
		return slices.ContainsFunc(mounts, func(candidate core.AdditionalMount) bool {
			return upgradeSameMount(candidate, mount)
		})
	}
	for _, mount := range applied {
		if containsMount(change.Mounts, mount) {
			continue
		}
		index := slices.IndexFunc(r.mounts, func(retained retainedMount) bool {
			return retained.Destination == core.Text(mount.Destination) &&
				retained.Kind == "bind" &&
				retained.Source == core.Text(mount.Source) &&
				retained.Writable == (mount.Access == core.MountAccessRw)
		})
		if index < 0 {
			return fmt.Errorf("the attached directory at %s no longer matches the running container", mount.Destination)
		}
		if err := protect(mount.Destination); err != nil {
			return err
		}
		if r.controlState != nil && upgradePathStartsWith(r.controlState.StagingDirectory, mount.Destination) {
			return mountConflict(mount.Destination, "the worker staging directory")
		}
		r.mounts = slices.Delete(r.mounts, index, index+1)
	}
	for _, mount := range change.Mounts {
		if containsMount(applied, mount) {
			continue
		}
		if err := protect(mount.Destination); err != nil {
			return err
		}
		for _, retained := range r.mounts {
			if overlaps(mount.Destination, retained.Destination) {
				return mountConflict(mount.Destination, retained.Destination)
			}
		}
		source := core.Text(mount.Source)
		destination := core.Text(mount.Destination)
		if strings.ContainsAny(source, ",\n") || strings.ContainsAny(destination, ",\n") {
			return fmt.Errorf("mount at %s cannot be represented safely for replacement", destination)
		}
		r.mounts = append(r.mounts, retainedMount{
			Kind:        "bind",
			Source:      source,
			Destination: destination,
			Writable:    mount.Access == core.MountAccessRw,
		})
	}
	if change.ResourceAllocation != nil {
		memory := core.AllocationMemory(change.ResourceAllocation)
		if memory > math.MaxInt64 {
			return errors.New("out of range integral type conversion attempted")
		}
		r.memory = int64(memory)
		cpus := core.AllocationCpus(change.ResourceAllocation)
		if cpus > math.MaxInt64 {
			return errors.New("out of range integral type conversion attempted")
		}
		if cpus > math.MaxInt64/1_000_000_000 {
			return errors.New("container CPU limit is too large")
		}
		r.nanoCpus = int64(cpus) * 1_000_000_000
	} else {
		var memory int64
		if template.Memory != nil {
			limit, err := core.ContainerMemoryLimit(*template.Memory)
			if err != nil {
				return err
			}
			memory = limit
		}
		r.memory = memory
		var nanoCpus int64
		if template.CPUs != nil {
			cpus, err := upgradeParseFloat(*template.CPUs)
			if err != nil {
				return err
			}
			nanos := cpus * 1e9
			if !(nanos >= 0 && nanos < float64(math.MaxInt64)) {
				return fmt.Errorf("invalid container CPU limit %s", core.RustDebugString(*template.CPUs))
			}
			nanoCpus = int64(math.Round(nanos))
		}
		r.nanoCpus = nanoCpus
	}
	prepared := r.currentImageID
	r.PreparedImage = &prepared
	selected := prepared
	r.selectedImageID = &selected
	r.TargetImage = r.CurrentImage
	return nil
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
	state := r.controlState
	if state == nil {
		return nil
	}
	id, err := core.NewCommandID("capture")
	if err != nil {
		return err
	}
	next := fmt.Sprintf("%s/state.tar.%s", state.StagingDirectory, id)
	args := []string{
		"sh",
		"-c",
		`set -eu; mkdir -p -- "$1"; chmod 700 -- "$1"; shift; exec "$@"`,
		"mj-container-control-state",
		state.StagingDirectory,
		"tar",
		"--one-file-system",
	}
	for _, path := range state.Excludes {
		args = append(args, "--exclude", path)
	}
	args = append(args, "-C", state.Root, "-cf", next, ".")
	capture := core.LocatorCommand(core.SshTrafficClassBulk, &r.Locator, args).
		WithPurpose("capture stopped worker journal and harness state")
	output, err := executor.Execute(&capture)
	if err != nil {
		return err
	}
	if output.Status != 0 {
		return fmt.Errorf("cannot transfer worker control state; source and storage retained: %s", preflightLossy(output.Stderr))
	}
	seal := core.LocatorCommand(core.SshTrafficClassBulk, &r.Locator, []string{
		"mv",
		"--",
		next,
		state.StagingDirectory + "/state.tar",
	}).WithPurpose("seal private control-state transfer")
	output, err = executor.Execute(&seal)
	if err != nil {
		return err
	}
	if output.Status != 0 {
		return errors.New("cannot seal worker control-state transfer; source retained")
	}
	return nil
}

// RestoreControlState extracts the captured control tree into the
// replacement, once.
func (r *RetainedContainer) RestoreControlState(executor core.CommandExecutor) error {
	state := r.controlState
	if state == nil {
		return nil
	}
	// A disconnected engine client may leave its extractor alive. The lock
	// and durable completion marker make every later extractor wait and then
	// skip, even after the new worker starts accepted work. Capture prefixes
	// every member with './'. Strip that component so --unlink-first never
	// tries to unlink the root '.' directory, then restore just that
	// directory's metadata without extracting files.
	const script = `set -eu; state=$1; root=$2; exec flock -x "$state/restore.lock" sh -c 'set -eu; [ ! -f "$1/restore-complete" ] || exit 0; mkdir -p -- "$2"; tar --unlink-first --strip-components=1 -C "$2" -xf "$1/state.tar"; tar --no-recursion --occurrence=1 -C "$2" -xf "$1/state.tar" .; touch "$1/restore-complete"' mj-control-restore "$state" "$root"`
	restore := core.LocatorCommand(core.SshTrafficClassBulk, &r.Locator, []string{
		"sh",
		"-c",
		script,
		"mj-container-control-state",
		state.StagingDirectory,
		state.Root,
	}).WithPurpose("transfer preserved worker journal and harness state")
	output, err := executor.Execute(&restore)
	if err != nil {
		return err
	}
	if output.Status != 0 {
		return fmt.Errorf("cannot restore private worker state; source and all mounts retained: %s", preflightLossy(output.Stderr))
	}
	return nil
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
	if !r.ImagePinReserved {
		return nil
	}
	host, _, err := hostAndName(&r.Locator)
	if err != nil {
		return err
	}
	if host.Kind == core.ImageHostSshNitro {
		return r.nitroRelease(&host, executor)
	}
	pin := r.imagePin()
	inspectPin := host.Command([]string{host.Engine(), "image", "inspect", "--format", "{{.Id}}", pin}, "inspect private upgrade image pin")
	inspected, err := executor.Execute(&inspectPin)
	if err != nil {
		return err
	}
	if inspected.Status != 0 {
		message := preflightASCIILower(preflightLossy(inspected.Stderr))
		if !(strings.Contains(message, "no such image") ||
			strings.Contains(message, "image not known") ||
			strings.Contains(message, "image not found")) {
			return fmt.Errorf("cannot inspect private upgrade image pin: %s", message)
		}
		return nil
	}
	text, err := upgradeUTF8(inspected.Stdout)
	if err != nil {
		return err
	}
	image := strings.TrimSpace(text)
	if r.PreparedImage != nil && trimSha256Prefixes(image) != trimSha256Prefixes(*r.PreparedImage) {
		return errors.New("private upgrade image pin changed ownership")
	}
	var args []string
	if host.Engine() == "podman" {
		args = []string{"untag", image, pin}
	} else {
		args = []string{"image", "rm", "--force", "--no-prune", pin}
	}
	_, err = checked(&host, args, executor)
	return err
}

// Prepare pulls and pins the target image, and checks it can receive the
// control state, before anything is stopped.
func (r *RetainedContainer) Prepare(executor core.CommandExecutor) error {
	host, _, err := hostAndName(&r.Locator)
	if err != nil {
		return err
	}
	if r.PreparedImage == nil {
		if err := r.pullTargetImage(&host, executor); err != nil {
			return err
		}
	}
	if host.Kind == core.ImageHostSshNitro {
		return r.nitroStage(&host, executor)
	}
	if _, err := checked(&host, []string{"image", "tag", *r.PreparedImage, r.imagePin()}, executor); err != nil {
		return err
	}
	if r.controlState != nil {
		user := r.user
		if user == "" {
			user = "0"
		}
		if _, err := checked(&host, []string{
			"run",
			"--rm",
			"--entrypoint",
			"sh",
			"--user",
			user,
			*r.PreparedImage,
			"-c",
			"command -v tar >/dev/null && command -v flock >/dev/null && mkdir -p /var/lib/hel && test -w /var/lib/hel",
		}, executor); err != nil {
			return err
		}
	}
	return nil
}

// pullTargetImage downloads the target image and records its immutable
// identity, holding the image's pull gate for the download (Rust holds the
// guard for the rest of the `if` block in Prepare).
func (r *RetainedContainer) pullTargetImage(host *core.ImageHost, executor core.CommandExecutor) error {
	lock := ImagePullMutex(host, r.TargetImage)
	pull, err := HoldImagePull(lock, executor.CancellationRequested, func() {})
	if err != nil {
		return err
	}
	defer pull.Release()
	if _, err := checked(host, []string{"pull", r.TargetImage}, executor); err != nil {
		return err
	}
	output, err := checked(host, []string{"image", "inspect", "--format", "{{.Id}}", r.TargetImage}, executor)
	if err != nil {
		return err
	}
	text, err := upgradeUTF8(output.Stdout)
	if err != nil {
		return err
	}
	id := strings.TrimSpace(text)
	if id == "" {
		return errors.New("image preparation returned no immutable image identity")
	}
	r.PreparedImage = &id
	return nil
}

// Replace is idempotent after the durable replacement boundary. The stopped
// source remains a rollback resource until the new worker is ready.
func (r *RetainedContainer) Replace(sessionID string, executor core.CommandExecutor) error {
	host, _, err := hostAndName(&r.Locator)
	if err != nil {
		return err
	}
	if r.PreparedImage == nil {
		return errors.New("container image has not been prepared")
	}
	image := *r.PreparedImage
	if host.Kind == core.ImageHostSshNitro {
		if err := r.nitroReplace(&host, sessionID, executor); err != nil {
			return err
		}
		return InitializeContainerHome(&r.Locator, executor)
	}
	current, err := inspect(&host, r.Name, executor)
	if err != nil {
		return err
	}
	if current != nil {
		if err := owned(current, sessionID); err != nil {
			return err
		}
		if operation, ok := current.Config.Labels["dev.mj.container-upgrade"]; ok && operation == r.OperationID {
			if !current.State.Running {
				if _, err := checked(&host, []string{"start", r.Name}, executor); err != nil {
					return err
				}
			}
			return InitializeContainerHome(&r.Locator, executor)
		}
		if current.ID != r.SourceID {
			return errors.New("container identity changed since upgrade confirmation; refusing replacement")
		}
		if current.State.Running {
			if err := checkedContainerStateChange(&host, "kill", r.SourceID, containerStateGoalNotRunning, executor); err != nil {
				return err
			}
		}
		if _, err := checked(&host, []string{"rename", r.SourceID, r.BackupName}, executor); err != nil {
			return err
		}
	}
	if r.ControlStateRestored {
		return errors.New("replacement container is missing after its worker state was restored; retaining the source and refusing to replay older work")
	}
	backup, err := inspect(&host, r.BackupName, executor)
	if err != nil {
		return err
	}
	if backup == nil {
		return errors.New("retained source container disappeared; refusing to create another worker")
	}
	if err := owned(backup, sessionID); err != nil {
		return err
	}
	if backup.ID != r.SourceID || backup.State.Running {
		return errors.New("retained source identity or stopped state changed")
	}
	if err := core.ReachTestHook("container_upgrade_source_retained_before_replacement"); err != nil {
		return err
	}
	for _, mount := range r.mounts {
		if mount.Kind == "volume" {
			if _, err := checked(&host, []string{"volume", "inspect", mount.Source}, executor); err != nil {
				return err
			}
		}
	}
	args := []string{
		"run",
		"--detach",
		"--name",
		r.Name,
		"--pull=never",
		"--init",
		"--stop-signal=KILL",
	}
	for _, capability := range containerCapabilities(host.Engine()) {
		args = append(args, "--cap-add="+capability)
	}
	for _, option := range containerSecurityOpts(host.Engine()) {
		args = append(args, "--security-opt", option)
	}
	args = append(args, managedResourceIdentityArgs(managedResourceKindContainer, sessionID)...)
	args = append(args,
		"--label", "dev.mj.container-upgrade="+r.OperationID,
		"--label", "dev.mj.container-image="+r.TargetImage,
	)
	if r.user != "" {
		args = append(args, "--user", r.user)
	}
	if r.workingDirectory != "" {
		args = append(args, "--workdir", r.workingDirectory)
	}
	// The CLI reads environment files on its own host, without changing
	// HOME or putting values in argv. SSH hosts read their forwarded stdin.
	var input []byte
	for _, assignment := range r.environment {
		if err := validateRetainedEnvironment(assignment); err != nil {
			return err
		}
		input = append(input, assignment...)
		input = append(input, '\n')
	}
	var environmentFile *os.File
	// dropEnvironment removes the private environment file, as dropping
	// Rust's NamedTempFile does. Repeating it is safe.
	dropEnvironment := func() {
		if environmentFile != nil {
			_ = environmentFile.Close()
			_ = os.Remove(environmentFile.Name())
		}
	}
	defer dropEnvironment()
	switch host.Kind {
	case core.ImageHostSshPodman, core.ImageHostSshDocker:
	default:
		file, err := os.CreateTemp("", ".tmp")
		if err != nil {
			return fmt.Errorf("create private container environment file: %w", err)
		}
		environmentFile = file
		if _, err := file.Write(input); err != nil {
			return fmt.Errorf("write private container environment: %w", err)
		}
	}
	environmentPath := "/dev/stdin"
	if environmentFile != nil {
		environmentPath = environmentFile.Name()
	}
	args = append(args, "--env-file", environmentPath)
	if r.networkMode != "" {
		args = append(args, "--network", r.networkMode)
	}
	// The source's limits were enforceable when it was created; the host may
	// have stopped delegating their controllers since.
	var unenforceable []CgroupLimit
	if (host.Kind == core.ImageHostLocalPodman || host.Kind == core.ImageHostSshPodman) && (r.memory > 0 || r.nanoCpus > 0) {
		var ssh *core.SshTarget
		if host.Kind == core.ImageHostSshPodman {
			ssh = host.SSH
		}
		limits, err := ProbePodmanUnenforceableLimits(ssh, executor)
		if err != nil {
			slog.Warn("could not read Podman's cgroup controllers; keeping container limits", "error", err.Error())
		} else {
			unenforceable = limits
		}
	}
	var dropped []CgroupLimit
	for _, candidate := range []struct {
		limit CgroupLimit
		set   bool
	}{{CgroupLimitMemory, r.memory > 0}, {CgroupLimitCPU, r.nanoCpus > 0}} {
		if candidate.set && slices.Contains(unenforceable, candidate.limit) {
			dropped = append(dropped, candidate.limit)
		}
	}
	if len(dropped) != 0 {
		executor.NotifyNotice(UnenforceableLimitsDetail(dropped) + " " + CgroupDelegationRemediation)
	}
	if r.memory > 0 && !slices.Contains(dropped, CgroupLimitMemory) {
		args = append(args, "--memory", strconv.FormatInt(r.memory, 10))
	}
	if r.nanoCpus > 0 && !slices.Contains(dropped, CgroupLimitCPU) {
		args = append(args, "--cpus", strconv.FormatFloat(float64(r.nanoCpus)/1e9, 'f', -1, 64))
	}
	args = append(args, "--pids-limit", strconv.FormatInt(r.pidsLimit, 10))
	for _, mount := range r.mounts {
		readonly := ""
		if !mount.Writable {
			readonly = ",readonly"
		}
		args = append(args, "--mount", fmt.Sprintf("type=%s,source=%s,target=%s%s", mount.Kind, mount.Source, mount.Destination, readonly))
	}
	args = append(args, "--entrypoint", "sh", image, "-c", keepaliveScript)
	args = append([]string{host.Engine()}, args...)
	command := host.Command(args, "start replacement on retained storage")
	if environmentFile == nil {
		command = command.WithSensitiveStdin(input)
	}
	result, err := executor.Execute(&command)
	if err != nil {
		return err
	}
	dropEnvironment()
	environmentFile = nil
	if result.Status != 0 {
		detail := preflightLossy(result.Stderr)
		failure := fmt.Errorf("replacement start failed; source and all storage retained: %s", strings.TrimRightFunc(detail, unicode.IsSpace))
		for _, mount := range r.mounts {
			if mount.Kind == "bind" &&
				(strings.Contains(detail, fmt.Sprintf("statfs %s: no such file or directory", mount.Source)) ||
					strings.Contains(detail, fmt.Sprintf("bind source path does not exist: %s", mount.Source))) {
				return core.RefusalPrecondition(fmt.Sprintf(
					"The bind source for %s is missing. Recreate the source to retry this replacement, or remove the mount before the next container creation.",
					mount.Destination)).Wrap(failure)
			}
		}
		return failure
	}
	return InitializeContainerHome(&r.Locator, executor)
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
	host, _, err := hostAndName(&r.Locator)
	if err != nil {
		return err
	}
	replacement, err := inspect(&host, r.Name, executor)
	if err != nil {
		return err
	}
	if replacement == nil {
		return errors.New("replacement container disappeared")
	}
	if err := owned(replacement, sessionID); err != nil {
		return err
	}
	if operation, ok := replacement.Config.Labels["dev.mj.container-upgrade"]; !ok || operation != r.OperationID {
		return errors.New("replacement ownership changed")
	}
	if host.Kind == core.ImageHostSshNitro {
		if err := r.nitroRelease(&host, executor); err != nil {
			return err
		}
	} else {
		backup, err := inspect(&host, r.BackupName, executor)
		if err != nil {
			return err
		}
		if backup != nil {
			if err := owned(backup, sessionID); err != nil {
				return err
			}
			if backup.ID != r.SourceID || backup.State.Running {
				return errors.New("refusing to retire a changed or running source")
			}
			if err := checkedContainerStateChange(&host, "rm", r.SourceID, containerStateGoalAbsent, executor); err != nil {
				return err
			}
		}
	}
	if state := r.controlState; state != nil {
		cleanup := core.LocatorCommand(core.SshTrafficClassControl, &r.Locator, []string{
			"sh",
			"-c",
			`set -eu; [ -f "$1/restore-complete" ] || exit 0; rm -f -- "$1/state.tar" "$1"/state.tar.capture-*`,
			"mj-control-transfer-cleanup",
			state.StagingDirectory,
		}).WithPurpose("retire completed private control-state transfer")
		output, err := executor.Execute(&cleanup)
		if err != nil {
			return err
		}
		if output.Status != 0 {
			return errors.New("cannot retire completed private control-state transfer")
		}
	}
	return nil
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
	current, err := inspect(host, r.Name, executor)
	if err != nil {
		return err
	}
	if current == nil {
		return errors.New("the session's sandbox is missing; refusing to create another worker over its volumes")
	}
	if err := owned(current, sessionID); err != nil {
		return err
	}
	if operation, ok := current.Config.Labels["dev.mj.container-upgrade"]; ok && operation == r.OperationID {
		if !current.State.Running {
			if _, err := checked(host, []string{"start", r.Name}, executor); err != nil {
				return err
			}
		}
		return nil
	}
	if current.ID != r.SourceID {
		return errors.New("container identity changed since upgrade confirmation; refusing replacement")
	}
	if r.ControlStateRestored {
		return errors.New("replacement container is missing after its worker state was restored; retaining the source and refusing to replay older work")
	}
	if current.State.Running {
		// The worker is already stopped. A stop gives the guest the chance to
		// flush the volumes, which hold the captured control state.
		if _, err := checked(host, []string{"stop", "--time", "0", r.Name}, executor); err != nil {
			return err
		}
	}
	if err := core.ReachTestHook("container_upgrade_source_retained_before_replacement"); err != nil {
		return err
	}
	if _, err := checked(host, r.nitroVerb("apply", nil), executor); err != nil {
		return err
	}
	_, err = checked(host, []string{"start", r.Name}, executor)
	return err
}

type containerStateGoal int

const (
	containerStateGoalNotRunning containerStateGoal = iota
	containerStateGoalAbsent
)

func checkedContainerStateChange(host *core.ImageHost, verb string, containerID string, goal containerStateGoal, executor core.CommandExecutor) error {
	// Podman exits nonzero when a dead exec session's conmon never wrote its
	// exit file, even though the container did stop; the state decides.
	command := host.Command([]string{host.Engine(), verb, containerID}, "upgrade container while retaining storage")
	output, err := executor.Execute(&command)
	if err != nil {
		return err
	}
	inspected, err := inspect(host, containerID, executor)
	if err != nil {
		return err
	}
	var achieved bool
	switch goal {
	case containerStateGoalNotRunning:
		achieved = inspected == nil || !inspected.State.Running
	case containerStateGoalAbsent:
		achieved = inspected == nil
	}
	if achieved {
		warning := strings.TrimSpace(preflightLossy(output.Stderr))
		if output.Status != 0 || warning != "" {
			slog.Warn("container state-change command succeeded by post-command state",
				"container_id", containerID, "verb", verb, "status", output.Status, "stderr", warning)
		}
		return nil
	}
	return fmt.Errorf("container upgrade failed; storage retained: %s", preflightLossy(output.Stderr))
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
	command := host.Command([]string{host.Engine(), "volume", "inspect", name}, "inspect retained legacy volume")
	output, err := executor.Execute(&command)
	if err != nil {
		return false, err
	}
	if output.Status != 0 {
		message := preflightASCIILower(preflightLossy(output.Stderr))
		if !(strings.Contains(message, "no such volume") || strings.Contains(message, "no volume with name")) {
			return false, fmt.Errorf("cannot establish retained volume absence: %s; checkpoint and storage retained", message)
		}
		return false, nil
	}
	var records []any
	if err := json.Unmarshal(output.Stdout, &records); err != nil {
		return false, err
	}
	if len(records) == 0 {
		return false, errors.New("volume inspection is empty")
	}
	record, _ := records[0].(map[string]any)
	labels, _ := record["Labels"].(map[string]any)
	recordName, nameOK := record["Name"].(string)
	managed, managedOK := labels["dev.mj.managed"].(string)
	owner, ownerOK := labels["dev.mj.session"].(string)
	if !(nameOK && recordName == name && managedOK && managed == "true" && ownerOK && owner == session) {
		return false, errors.New("legacy volume ownership changed; refusing checkpoint restore over retained storage")
	}
	return true, nil
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
	host, name, err := hostAndName(locator)
	if err != nil {
		return false, err
	}
	existing, err := inspect(&host, name, executor)
	if err != nil {
		return false, err
	}
	if existing != nil {
		return false, errors.New("legacy recovery target already exists")
	}
	var storage *core.TargetsPodmanWorkspaceLocator
	switch locator.Kind {
	case core.TargetsTargetLocatorLocalPodman, core.TargetsTargetLocatorSshPodman:
		storage = &locator.WorkspaceStorage
	}
	var restoreRepositories bool
	switch {
	case storage != nil && storage.EffectiveKind() == core.TargetsPodmanWorkspaceLocatorVolume:
		if requireExistingWorkspace {
			exists, err := retainedVolumeExists(&host, storage.Name, sessionID, executor)
			if err != nil {
				return false, err
			}
			if !exists {
				return false, errors.New("retained workspace disappeared before replacement; no storage was created or removed; retry Resume")
			}
			restoreRepositories = false
		} else {
			existed, err := retainLegacyVolume(&host, storage.Name, sessionID, executor)
			if err != nil {
				return false, err
			}
			restoreRepositories = !existed
		}
	case storage != nil && storage.EffectiveKind() == core.TargetsPodmanWorkspaceLocatorHostPath:
		command := host.Command([]string{
			"sh",
			"-c",
			`test -d "$1"`,
			"mj-retained-workspace",
			storage.Path,
		}, "find surviving helper workspace")
		result, err := executor.Execute(&command)
		if err != nil {
			return false, err
		}
		if result.Status != 0 {
			return false, errors.New("legacy helper workspace is absent or inaccessible; checkpoint retained; restore the helper resource before retrying")
		}
		restoreRepositories = false
	default:
		var chosen *core.AdditionalMount
		chosenDepth := -1
		for i := range mounts {
			mount := &mounts[i]
			if !(upgradePathStartsWith(workspace, mount.Destination) || upgradePathStartsWith(mount.Destination, workspace)) {
				continue
			}
			// Rust's max_by_key keeps the last of equally deep mounts.
			if depth := upgradePathComponentCount(mount.Destination); depth >= chosenDepth {
				chosen, chosenDepth = mount, depth
			}
		}
		if chosen == nil {
			restoreRepositories = true
			break
		}
		if chosen.Access != core.MountAccessRw {
			return false, errors.New("legacy workspace uses an unsupported read-only or transient attachment; checkpoint retained")
		}
		// A surviving repository mounted inside the workspace also forbids a
		// whole-workspace archive restore.
		source := chosen.Source
		if rest, ok := upgradePathStripPrefix(workspace, chosen.Destination); ok {
			source = core.Join(chosen.Source, rest)
		}
		command := host.Command([]string{"test", "-d", source}, "verify surviving legacy workspace attachment")
		result, err := executor.Execute(&command)
		if err != nil {
			return false, err
		}
		if result.Status != 0 {
			return false, errors.New("retained legacy workspace attachment is inaccessible; refusing checkpoint restore over its storage")
		}
		restoreRepositories = false
	}
	temporary := ContainerTemporaryVolumeName(name)
	if _, err := retainLegacyVolume(&host, temporary, sessionID, executor); err != nil {
		return false, err
	}
	// Captured historical mounts are retained; provisioning's destructive
	// error traps and repository clone plan are deliberately absent.
	args, err := containerRunArgs(host.Engine(), template, name, sessionID, mounts, storage, workspace, false)
	if err != nil {
		return false, err
	}
	if _, err := checked(&host, args, executor); err != nil {
		return false, err
	}
	return restoreRepositories, nil
}

// upgradeCloneLocator copies a locator without sharing its pointers or
// slices.
func upgradeCloneLocator(locator *core.TargetsTargetLocator) core.TargetsTargetLocator {
	clone := *locator
	clone.WorkspaceStorage.Helper = slices.Clone(locator.WorkspaceStorage.Helper)
	if locator.BorrowedFrom != nil {
		borrowed := *locator.BorrowedFrom
		clone.BorrowedFrom = &borrowed
	}
	if locator.SSH != nil {
		ssh := locator.SSH.Clone()
		clone.SSH = &ssh
	}
	if locator.WorkerID != nil {
		worker := *locator.WorkerID
		clone.WorkerID = &worker
	}
	return clone
}

// upgradeSameMount is Rust's derived AdditionalMount equality, which compares
// the paths by component.
func upgradeSameMount(a, b core.AdditionalMount) bool {
	return a.Access == b.Access &&
		slices.Equal(upgradePathComponents(a.Source), upgradePathComponents(b.Source)) &&
		slices.Equal(upgradePathComponents(a.Destination), upgradePathComponents(b.Destination))
}

// upgradePathComponents is Rust's `Path::components` for a target path: the
// root, then each name, with empty and `.` components after the first
// dropped. Target paths are POSIX; a Windows controller also splits on `\`.
func upgradePathComponents(path string) []string {
	separator := func(r rune) bool { return r == '/' || (runtime.GOOS == "windows" && r == '\\') }
	var components []string
	rest := path
	if rest != "" && separator(rune(rest[0])) {
		components = append(components, "/")
		rest = rest[1:]
	}
	for index, part := range strings.FieldsFunc(rest, separator) {
		if part == "." && (index != 0 || len(components) != 0 || !strings.HasPrefix(rest, ".")) {
			continue
		}
		components = append(components, part)
	}
	return components
}

// upgradePathComponentCount is `Path::components().count()`.
func upgradePathComponentCount(path string) int {
	return len(upgradePathComponents(path))
}

// upgradePathStartsWith is Rust's `Path::starts_with`: whether base is a
// component prefix of path.
func upgradePathStartsWith(path, base string) bool {
	left, right := upgradePathComponents(path), upgradePathComponents(base)
	return len(right) <= len(left) && slices.Equal(left[:len(right)], right)
}

// upgradePathStripPrefix is Rust's `Path::strip_prefix`: the components of
// path after base, joined with `/`, or false when base is not a component
// prefix of path.
func upgradePathStripPrefix(path, base string) (string, bool) {
	if !upgradePathStartsWith(path, base) {
		return "", false
	}
	left, right := upgradePathComponents(path), upgradePathComponents(base)
	return strings.Join(left[len(right):], "/"), true
}

// upgradeUTF8 is Rust's `String::from_utf8`.
func upgradeUTF8(data []byte) (string, error) {
	if !utf8.Valid(data) {
		index := 0
		for index < len(data) {
			r, size := utf8.DecodeRune(data[index:])
			if r == utf8.RuneError && size <= 1 {
				break
			}
			index += size
		}
		return "", fmt.Errorf("invalid utf-8 sequence of 1 bytes from index %d", index)
	}
	return string(data), nil
}

// upgradeParseFloat is Rust's `str::parse::<f64>`, which takes decimal
// notation, `inf`, `infinity` and `nan`, but not Go's hexadecimal floats or
// digit separators.
func upgradeParseFloat(text string) (float64, error) {
	if strings.ContainsAny(text, "_xX") {
		return 0, errors.New("invalid float literal")
	}
	value, err := strconv.ParseFloat(text, 64)
	if err != nil && !errors.Is(err, strconv.ErrRange) {
		return 0, errors.New("invalid float literal")
	}
	return value, nil
}
