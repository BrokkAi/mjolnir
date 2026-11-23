// Port of mj-core/src/config/machines.rs.
//
// Machines (the hosts sessions run on) and the stored form of the runtimes
// that run on them.
//
// The configuration file keeps these apart. [machines.<id>] describes a host:
// this computer (always present, always called `local`), an SSH host, or an
// EC2 launch template. Each machine owns its SSH connection details, the
// remote directory bare runtimes keep workspaces in, and the mbx build cache
// every runtime on that machine shares. [targets.<id>] describes a runtime --
// `bare`, `podman`, `docker`, `apple-container` or `nitro` -- and names the
// machine it runs on.
//
// In memory nothing is split: every consumer still matches on the fused
// TargetTemplate, which pairs a host with a runtime. This file is the only
// place that joins a stored runtime to its machine (ResolveTarget) and takes a
// fused target back apart (StoredTargetOf).

package core

import (
	"encoding/json"
	"fmt"
	"log/slog"
	"slices"
	"strings"
)

// LocalMachineID is the id of the machine Mjolnir itself runs on. It always
// exists, whether or not the file has a [machines.local] table.
const LocalMachineID = "local"

func localMachineID() string { return LocalMachineID }

func isLocalMachineID(id string) bool { return id == LocalMachineID }

// Machine is one host that runtimes run on (tag = "kind", kebab-case). Fields
// of this type are read with UnmarshalMachineJSON.
type Machine interface {
	isMachine()
	// KindName is the `kind` spelling used in the configuration file.
	KindName() string
	// BuildCache is the build cache every runtime on this machine shares,
	// when it has overrides of its own.
	BuildCache() *TargetBuildCache
	validate(id string) error
	MarshalJSON() ([]byte, error)
}

// MachineLocal is this computer. Only the id `local` may hold it.
type MachineLocal struct {
	TargetBuildCache *TargetBuildCache
}

type MachineSsh struct {
	SSH SshConnection
	// WorkspacePrefix is where bare runtimes on this machine keep their
	// workspaces, relative to the login home.
	WorkspacePrefix  string
	TargetBuildCache *TargetBuildCache
}

// MachineAwsEc2 is an EC2 launch template. Instances from it run a bare
// harness only.
type MachineAwsEc2 struct {
	AWSProfile            *string          `json:"aws_profile,omitempty"`
	Region                string           `json:"region"`
	LaunchTemplate        string           `json:"launch_template"`
	LaunchTemplateVersion *string          `json:"launch_template_version,omitempty"`
	SSHUser               string           `json:"ssh_user"`
	AddressSource         AwsAddressSource `json:"address_source"`
	IdentityFile          *string          `json:"identity_file,omitempty"`
	SSHArgs               []string         `json:"ssh_args,omitempty"`
}

func machineKindName(m Machine) string {
	switch m.(type) {
	case MachineLocal:
		return "local"
	case MachineSsh:
		return "ssh"
	case MachineAwsEc2:
		return "aws-ec2"
	}
	panic(fmt.Sprintf("unknown machine %T", m))
}

func machineBuildCache(m Machine) *TargetBuildCache {
	switch m := m.(type) {
	case MachineLocal:
		return m.TargetBuildCache
	case MachineSsh:
		return m.TargetBuildCache
	}
	// An EC2 instance is created per session, so it shares no cache.
	return nil
}

// machineSetBuildCache replaces the build cache of the machine machine holds
// (Rust's Machine::set_build_cache); an EC2 machine has none.
func machineSetBuildCache(machine *Machine, value *TargetBuildCache) {
	switch m := (*machine).(type) {
	case MachineLocal:
		m.TargetBuildCache = value
		*machine = m
	case MachineSsh:
		m.TargetBuildCache = value
		*machine = m
	}
}

func machineValidate(m Machine, id string) error {
	if err := ValidateID("machine", id); err != nil {
		return err
	}
	switch m := m.(type) {
	case MachineLocal:
		if !isLocalMachineID(id) {
			return fmt.Errorf("machine %s is this machine, which is always called %s",
				debugString(id), debugString(LocalMachineID))
		}
		if m.TargetBuildCache != nil {
			return m.TargetBuildCache.validate(id)
		}
		return nil
	case MachineSsh:
		if isLocalMachineID(id) {
			return fmt.Errorf("machine %s is reserved for this machine", debugString(LocalMachineID))
		}
		if err := m.SSH.validate(id); err != nil {
			return err
		}
		if err := validateWorkspacePrefix("machine "+id, m.WorkspacePrefix); err != nil {
			return err
		}
		if m.TargetBuildCache != nil {
			return m.TargetBuildCache.validate(id)
		}
		return nil
	case MachineAwsEc2:
		if isLocalMachineID(id) {
			return fmt.Errorf("machine %s is reserved for this machine", debugString(LocalMachineID))
		}
		if configBlank(m.Region) || configBlank(m.LaunchTemplate) || configBlank(m.SSHUser) {
			return fmt.Errorf("AWS machine %s requires region, launch_template, and ssh_user", debugString(id))
		}
		if m.AWSProfile != nil && *m.AWSProfile == "" ||
			m.LaunchTemplateVersion != nil && *m.LaunchTemplateVersion == "" {
			return fmt.Errorf("AWS machine %s contains an empty optional value", debugString(id))
		}
		return nil
	}
	panic(fmt.Sprintf("unknown machine %T", m))
}

func marshalMachineJSON(m Machine) ([]byte, error) {
	var object serdeObject
	object.field("kind", m.KindName())
	switch m := m.(type) {
	case MachineLocal:
		object.fieldUnless(isDefaultTargetBuildCache(m.TargetBuildCache), "build_cache", m.TargetBuildCache)
	case MachineSsh:
		object.flatten(m.SSH)
		object.field("workspace_prefix", m.WorkspacePrefix)
		object.fieldUnless(isDefaultTargetBuildCache(m.TargetBuildCache), "build_cache", m.TargetBuildCache)
	case MachineAwsEc2:
		object.fieldUnless(m.AWSProfile == nil, "aws_profile", m.AWSProfile)
		object.field("region", m.Region)
		object.field("launch_template", m.LaunchTemplate)
		object.fieldUnless(m.LaunchTemplateVersion == nil, "launch_template_version", m.LaunchTemplateVersion)
		object.field("ssh_user", m.SSHUser)
		object.field("address_source", m.AddressSource)
		object.fieldUnless(m.IdentityFile == nil, "identity_file", m.IdentityFile)
		object.fieldUnless(len(m.SSHArgs) == 0, "ssh_args", m.SSHArgs)
	}
	return object.finish()
}

// UnmarshalMachineJSON reads a Machine.
func UnmarshalMachineJSON(data []byte) (Machine, error) {
	kind, err := serdeTag(data, "kind")
	if err != nil {
		return nil, err
	}
	fields, err := decodeObject(data)
	if err != nil {
		return nil, err
	}
	buildCache := func() (*TargetBuildCache, error) {
		raw, ok := fields["build_cache"]
		if !ok {
			return nil, nil
		}
		return unmarshalTargetBuildCacheJSON(raw)
	}
	switch kind {
	case "local":
		cache, err := buildCache()
		return MachineLocal{TargetBuildCache: cache}, err
	case "ssh":
		var connection SshConnection
		if err := json.Unmarshal(data, &connection); err != nil {
			return nil, err
		}
		value := MachineSsh{SSH: connection, WorkspacePrefix: defaultNamedMachinePrefix()}
		if prefix, ok := fields["workspace_prefix"]; ok {
			if err := json.Unmarshal(prefix, &value.WorkspacePrefix); err != nil {
				return nil, err
			}
		}
		value.TargetBuildCache, err = buildCache()
		return value, err
	case "aws-ec2":
		if err := requireFields(data, "region", "launch_template", "ssh_user"); err != nil {
			return nil, err
		}
		value := MachineAwsEc2{AddressSource: AwsAddressSourcePublicDNS}
		err := json.Unmarshal(data, &value)
		return value, err
	}
	return nil, serdeUnknownVariant(kind, "local", "ssh", "aws-ec2")
}

// validateMachines validates every machine in machines, plus the checks that
// only make sense across the whole collection.
func validateMachines(machines map[string]Machine) error {
	ids := sortedKeys(machines)
	for _, id := range ids {
		if err := machines[id].validate(id); err != nil {
			return err
		}
	}
	type sshMachine struct {
		id         string
		connection SshConnection
	}
	var ssh []sshMachine
	for _, id := range ids {
		if machine, ok := machines[id].(MachineSsh); ok {
			ssh = append(ssh, sshMachine{id, machine.SSH})
		}
	}
	for index, current := range ssh {
		for _, other := range ssh[:index] {
			if sshConnectionEqual(other.connection, current.connection) {
				return fmt.Errorf("machines %s and %s describe the same host",
					debugString(other.id), debugString(current.id))
			}
		}
	}
	return nil
}

// StoredTarget is one runtime as the configuration file stores it: a kind,
// the machine it runs on, and the settings that belong to the runtime rather
// than the host (tag = "kind", kebab-case). Fields of this type are read with
// UnmarshalStoredTargetJSON.
type StoredTarget interface {
	isStoredTarget()
	// Machine is the machine this runtime runs on.
	Machine() string
	container() *ContainerTemplate
	MarshalJSON() ([]byte, error)
}

type StoredTargetBare struct {
	MachineID string
	// Permissions is only meaningful on an SSH machine; a bare runtime on
	// this machine always uses the configured approvals.
	Permissions *PermissionMode
}

type StoredTargetPodman struct {
	MachineID         string
	ContainerTemplate ContainerTemplate
}

type StoredTargetDocker struct {
	MachineID         string
	ContainerTemplate ContainerTemplate
}

type StoredTargetAppleContainer struct {
	MachineID         string
	ContainerTemplate ContainerTemplate
}

// StoredTargetNitro is an AWS Nitro Sandbox: a microVM per session on an EC2
// host reached over SSH. Only an SSH machine can hold it, so the machine is
// required.
type StoredTargetNitro struct {
	MachineID         string
	ContainerTemplate ContainerTemplate
}

// storedTargetKinds is the five runtime kinds a version 11 configuration may
// name.
var storedTargetKinds = [5]string{"bare", "podman", "docker", "apple-container", "nitro"}

func storedTargetMachine(s StoredTarget) string {
	switch s := s.(type) {
	case StoredTargetBare:
		return s.MachineID
	case StoredTargetPodman:
		return s.MachineID
	case StoredTargetDocker:
		return s.MachineID
	case StoredTargetAppleContainer:
		return s.MachineID
	case StoredTargetNitro:
		return s.MachineID
	}
	panic(fmt.Sprintf("unknown stored target %T", s))
}

func storedTargetContainer(s StoredTarget) *ContainerTemplate {
	var container ContainerTemplate
	switch s := s.(type) {
	case StoredTargetPodman:
		container = s.ContainerTemplate
	case StoredTargetDocker:
		container = s.ContainerTemplate
	case StoredTargetAppleContainer:
		container = s.ContainerTemplate
	case StoredTargetNitro:
		container = s.ContainerTemplate
	default:
		return nil
	}
	return &container
}

func storedTargetKind(s StoredTarget) string {
	switch s.(type) {
	case StoredTargetBare:
		return "bare"
	case StoredTargetPodman:
		return "podman"
	case StoredTargetDocker:
		return "docker"
	case StoredTargetAppleContainer:
		return "apple-container"
	case StoredTargetNitro:
		return "nitro"
	}
	panic(fmt.Sprintf("unknown stored target %T", s))
}

func marshalStoredTargetJSON(s StoredTarget) ([]byte, error) {
	var object serdeObject
	object.field("kind", storedTargetKind(s))
	_, nitro := s.(StoredTargetNitro)
	object.fieldUnless(!nitro && isLocalMachineID(s.Machine()), "machine", s.Machine())
	switch s := s.(type) {
	case StoredTargetBare:
		object.fieldUnless(s.Permissions == nil, "permissions", s.Permissions)
	default:
		object.flatten(*s.container())
	}
	return object.finish()
}

// UnmarshalStoredTargetJSON reads a StoredTarget.
func UnmarshalStoredTargetJSON(data []byte) (StoredTarget, error) {
	kind, err := serdeTag(data, "kind")
	if err != nil {
		return nil, err
	}
	fields, err := decodeObject(data)
	if err != nil {
		return nil, err
	}
	machine := localMachineID()
	if raw, ok := fields["machine"]; ok {
		if err := json.Unmarshal(raw, &machine); err != nil {
			return nil, err
		}
	} else if kind == "nitro" {
		return nil, missingField("machine")
	}
	container := func() (ContainerTemplate, error) {
		var value ContainerTemplate
		err := json.Unmarshal(data, &value)
		return value, err
	}
	switch kind {
	case "bare":
		value := StoredTargetBare{MachineID: machine}
		if raw, ok := fields["permissions"]; ok && string(raw) != "null" {
			var mode PermissionMode
			if err := json.Unmarshal(raw, &mode); err != nil {
				return nil, err
			}
			value.Permissions = &mode
		}
		return value, nil
	case "podman":
		value, err := container()
		return StoredTargetPodman{MachineID: machine, ContainerTemplate: value}, err
	case "docker":
		value, err := container()
		return StoredTargetDocker{MachineID: machine, ContainerTemplate: value}, err
	case "apple-container":
		value, err := container()
		return StoredTargetAppleContainer{MachineID: machine, ContainerTemplate: value}, err
	case "nitro":
		value, err := container()
		return StoredTargetNitro{MachineID: machine, ContainerTemplate: value}, err
	}
	return nil, serdeUnknownVariant(kind, storedTargetKinds[:]...)
}

// machineFor is the machine a runtime names, with `local` supplied even when
// the file has no [machines.local] table.
func machineFor(id, machineID string, machines map[string]Machine) (Machine, error) {
	if machine, ok := machines[machineID]; ok {
		return machine, nil
	}
	if isLocalMachineID(machineID) {
		return MachineLocal{}, nil
	}
	return nil, fmt.Errorf("target %s names machine %s, which is not defined", debugString(id), debugString(machineID))
}

// ResolveTarget joins a stored runtime to its machine, producing the fused
// target every other part of Mjolnir works with.
func ResolveTarget(id string, target StoredTarget, machines map[string]Machine) (TargetTemplate, error) {
	machineID := target.Machine()
	machine, err := machineFor(id, machineID, machines)
	if err != nil {
		return nil, err
	}
	if container := target.container(); container != nil && !isDefaultTargetBuildCache(container.BuildCache) {
		return nil, fmt.Errorf("target %s sets build_cache, which now belongs to [machines.%s]", debugString(id), machineID)
	}
	withCache := func(container ContainerTemplate) ContainerTemplate {
		container.BuildCache = cloneTargetBuildCache(machine.BuildCache())
		return container
	}
	ec2Only := func() error {
		return fmt.Errorf("target %s: an EC2 machine runs a bare harness only", debugString(id))
	}
	switch target := target.(type) {
	case StoredTargetBare:
		switch machine := machine.(type) {
		case MachineLocal:
			if target.Permissions != nil {
				return nil, fmt.Errorf("target %s sets permissions, which only applies to a bare runtime on an SSH machine", debugString(id))
			}
			return TargetTemplateLocalBare{}, nil
		case MachineSsh:
			permissions := PermissionModeGuardian
			if target.Permissions != nil {
				permissions = *target.Permissions
			}
			return TargetTemplateSshBare{SSH: machine.SSH, Permissions: permissions, WorkspacePrefix: machine.WorkspacePrefix}, nil
		case MachineAwsEc2:
			if target.Permissions != nil {
				return nil, fmt.Errorf("target %s sets permissions, which only applies to a bare runtime on an SSH machine", debugString(id))
			}
			return TargetTemplateAwsEc2{
				AWSProfile:            cloneStringPointer(machine.AWSProfile),
				Region:                machine.Region,
				LaunchTemplate:        machine.LaunchTemplate,
				LaunchTemplateVersion: cloneStringPointer(machine.LaunchTemplateVersion),
				SSHUser:               machine.SSHUser,
				AddressSource:         machine.AddressSource,
				IdentityFile:          cloneStringPointer(machine.IdentityFile),
				SSHArgs:               slices.Clone(machine.SSHArgs),
			}, nil
		}
	case StoredTargetPodman:
		switch machine := machine.(type) {
		case MachineLocal:
			return TargetTemplateLocalPodman{ContainerTemplate: withCache(target.ContainerTemplate)}, nil
		case MachineSsh:
			return TargetTemplateSshPodman{SSH: machine.SSH, ContainerTemplate: withCache(target.ContainerTemplate)}, nil
		case MachineAwsEc2:
			return nil, ec2Only()
		}
	case StoredTargetDocker:
		switch machine := machine.(type) {
		case MachineLocal:
			return TargetTemplateLocalDocker{ContainerTemplate: withCache(target.ContainerTemplate)}, nil
		case MachineSsh:
			return TargetTemplateSshDocker{SSH: machine.SSH, ContainerTemplate: withCache(target.ContainerTemplate)}, nil
		case MachineAwsEc2:
			return nil, ec2Only()
		}
	case StoredTargetAppleContainer:
		switch machine := machine.(type) {
		case MachineLocal:
			return TargetTemplateAppleContainer{ContainerTemplate: withCache(target.ContainerTemplate)}, nil
		case MachineSsh:
			return TargetTemplateSshAppleContainer{SSH: machine.SSH, ContainerTemplate: withCache(target.ContainerTemplate)}, nil
		case MachineAwsEc2:
			return nil, ec2Only()
		}
	case StoredTargetNitro:
		switch machine := machine.(type) {
		case MachineLocal:
			return nil, fmt.Errorf("target %s: a Nitro sandbox runs on an SSH machine, not on this machine", debugString(id))
		case MachineSsh:
			return TargetTemplateSshNitro{SSH: machine.SSH, ContainerTemplate: withCache(target.ContainerTemplate)}, nil
		case MachineAwsEc2:
			return nil, ec2Only()
		}
	}
	panic(fmt.Sprintf("unknown stored target %T on machine %T", target, machine))
}

// machineIDFor is the id of the machine in machines equal to wanted,
// inserting it under a collision-free id derived from base when it is not
// there yet.
func machineIDFor(machines map[string]Machine, wanted Machine, base string) string {
	for _, id := range sortedKeys(machines) {
		if machineEqual(machines[id], wanted) {
			return id
		}
	}
	id := UniqueConfigID(machines, base)
	machines[id] = wanted
	return id
}

// localMachineFor is the id of this machine, making sure machines holds it
// and that it owns the build cache settings a local container runtime
// carried.
func localMachineFor(machines map[string]Machine, buildCache *TargetBuildCache, targetID string) string {
	entry, ok := machines[LocalMachineID]
	if !ok {
		entry = MachineLocal{}
	}
	foldBuildCache(&entry, buildCache, LocalMachineID, targetID)
	machines[LocalMachineID] = entry
	return LocalMachineID
}

// sshMachineID is the id of the SSH machine in machines reaching ssh,
// creating it when absent. At most one machine may hold a given connection,
// so the connection alone identifies it.
func sshMachineID(machines map[string]Machine, ssh SshConnection, workspacePrefix *string, buildCache *TargetBuildCache, targetID string) string {
	id, found := findSSHMachine(machines, ssh)
	if !found {
		base := targetID
		if ValidateID("machine", ssh.Host) == nil {
			base = ssh.Host
		}
		id = UniqueConfigID(machines, base)
		prefix := defaultNamedMachinePrefix()
		if workspacePrefix != nil {
			prefix = *workspacePrefix
		}
		machines[id] = MachineSsh{SSH: ssh, WorkspacePrefix: prefix}
	}
	machine := machines[id]
	if current, ok := machine.(MachineSsh); ok && workspacePrefix != nil &&
		current.WorkspacePrefix != *workspacePrefix {
		slog.Warn("keeping the workspace directory already set for this machine and ignoring the one on this runtime",
			"machine", id, "target", targetID)
	}
	foldBuildCache(&machine, buildCache, id, targetID)
	machines[id] = machine
	return id
}

// foldBuildCache moves a container runtime's build cache overrides onto its
// machine, which owns them now. The first non-default setting for a machine
// wins.
func foldBuildCache(machine *Machine, buildCache *TargetBuildCache, machineID, targetID string) {
	if buildCache == nil || buildCache.IsDefault() {
		return
	}
	existing := (*machine).BuildCache()
	switch {
	case existing == nil:
		machineSetBuildCache(machine, cloneTargetBuildCache(buildCache))
	case !targetBuildCacheEqual(*existing, *buildCache):
		slog.Warn("keeping the build cache settings already set for this machine and ignoring the ones on this runtime",
			"machine", machineID, "target", targetID)
	}
}

// StoredTargetOf takes a fused target apart (Rust's stored_target): it
// returns the runtime as the file stores it, and makes sure machines holds
// the host it runs on.
//
// This is both what saving does and how a configuration written before
// version 11 is migrated, because a legacy [targets.<id>] table parses into
// exactly the same fused TargetTemplate.
func StoredTargetOf(id string, target TargetTemplate, machines map[string]Machine) StoredTarget {
	withoutCache := func(container ContainerTemplate) ContainerTemplate {
		container.BuildCache = nil
		return container
	}
	switch target := target.(type) {
	case TargetTemplateLocalBare:
		return StoredTargetBare{MachineID: LocalMachineID}
	case TargetTemplateLocalPodman:
		return StoredTargetPodman{
			MachineID:         localMachineFor(machines, target.ContainerTemplate.BuildCache, id),
			ContainerTemplate: withoutCache(target.ContainerTemplate),
		}
	case TargetTemplateLocalDocker:
		return StoredTargetDocker{
			MachineID:         localMachineFor(machines, target.ContainerTemplate.BuildCache, id),
			ContainerTemplate: withoutCache(target.ContainerTemplate),
		}
	case TargetTemplateAppleContainer:
		return StoredTargetAppleContainer{
			MachineID:         localMachineFor(machines, target.ContainerTemplate.BuildCache, id),
			ContainerTemplate: withoutCache(target.ContainerTemplate),
		}
	case TargetTemplateSshBare:
		prefix := target.WorkspacePrefix
		permissions := target.Permissions
		return StoredTargetBare{
			MachineID:   sshMachineID(machines, target.SSH, &prefix, nil, id),
			Permissions: &permissions,
		}
	case TargetTemplateSshPodman:
		return StoredTargetPodman{
			MachineID:         sshMachineID(machines, target.SSH, nil, target.ContainerTemplate.BuildCache, id),
			ContainerTemplate: withoutCache(target.ContainerTemplate),
		}
	case TargetTemplateSshDocker:
		return StoredTargetDocker{
			MachineID:         sshMachineID(machines, target.SSH, nil, target.ContainerTemplate.BuildCache, id),
			ContainerTemplate: withoutCache(target.ContainerTemplate),
		}
	case TargetTemplateSshAppleContainer:
		return StoredTargetAppleContainer{
			MachineID:         sshMachineID(machines, target.SSH, nil, target.ContainerTemplate.BuildCache, id),
			ContainerTemplate: withoutCache(target.ContainerTemplate),
		}
	case TargetTemplateSshNitro:
		return StoredTargetNitro{
			MachineID:         sshMachineID(machines, target.SSH, nil, target.ContainerTemplate.BuildCache, id),
			ContainerTemplate: withoutCache(target.ContainerTemplate),
		}
	case TargetTemplateAwsEc2:
		machine, _ := awsMachine(target)
		return StoredTargetBare{MachineID: machineIDFor(machines, machine, id)}
	}
	panic(fmt.Sprintf("unknown target template %T", target))
}

// awsMachine is the EC2 machine an EC2 target launches from. Two EC2 targets
// are on the same machine exactly when every launch setting matches.
func awsMachine(target TargetTemplate) (Machine, bool) {
	ec2, ok := target.(TargetTemplateAwsEc2)
	if !ok {
		return nil, false
	}
	return MachineAwsEc2(ec2), true
}

// findSSHMachine is the id of the SSH machine reaching ssh. At most one
// machine may hold a given connection, so the connection alone identifies it.
func findSSHMachine(machines map[string]Machine, ssh SshConnection) (string, bool) {
	for _, id := range sortedKeys(machines) {
		if machine, ok := machines[id].(MachineSsh); ok && sshConnectionEqual(machine.SSH, ssh) {
			return id, true
		}
	}
	return "", false
}

// TargetMachineID is the id of the machine in machines a fused target runs
// on, matched the way saving matches it, or false when no machine there holds
// it yet. Every local runtime runs on `local`, which exists whether or not
// machines names it.
func TargetMachineID(target TargetTemplate, machines map[string]Machine) (string, bool) {
	switch target := target.(type) {
	case TargetTemplateLocalBare, TargetTemplateLocalPodman, TargetTemplateLocalDocker, TargetTemplateAppleContainer:
		return LocalMachineID, true
	case TargetTemplateSshBare:
		return findSSHMachine(machines, target.SSH)
	case TargetTemplateSshPodman:
		return findSSHMachine(machines, target.SSH)
	case TargetTemplateSshDocker:
		return findSSHMachine(machines, target.SSH)
	case TargetTemplateSshAppleContainer:
		return findSSHMachine(machines, target.SSH)
	case TargetTemplateSshNitro:
		return findSSHMachine(machines, target.SSH)
	case TargetTemplateAwsEc2:
		wanted, _ := awsMachine(target)
		for _, id := range sortedKeys(machines) {
			if machineEqual(machines[id], wanted) {
				return id, true
			}
		}
		return "", false
	}
	panic(fmt.Sprintf("unknown target template %T", target))
}

// RuntimeKindName is the runtime half of a fused target: `bare`, `podman`,
// `docker`, `apple-container` or `nitro`, as [targets.<id>] spells its
// `kind`.
func RuntimeKindName(target TargetTemplate) string {
	switch target.(type) {
	case TargetTemplateLocalBare, TargetTemplateSshBare, TargetTemplateAwsEc2:
		return "bare"
	case TargetTemplateLocalPodman, TargetTemplateSshPodman:
		return "podman"
	case TargetTemplateLocalDocker, TargetTemplateSshDocker:
		return "docker"
	case TargetTemplateAppleContainer, TargetTemplateSshAppleContainer:
		return "apple-container"
	case TargetTemplateSshNitro:
		return "nitro"
	}
	panic(fmt.Sprintf("unknown target template %T", target))
}

// legacyKindAdvice is the version 11 spelling a configuration written before
// the split should use, named in the error a version 11 file gets for an old
// `kind`: the new kind and what to name as its machine.
func legacyKindAdvice(kind string) (string, string, bool) {
	const ssh = "the id of an [machines.<id>] entry with kind = \"ssh\""
	switch kind {
	case "local-bare":
		return "bare", LocalMachineID, true
	case "local-podman":
		return "podman", LocalMachineID, true
	case "local-docker":
		return "docker", LocalMachineID, true
	case "ssh-bare":
		return "bare", ssh, true
	case "ssh-podman":
		return "podman", ssh, true
	case "ssh-docker":
		return "docker", ssh, true
	case "aws-ec2":
		return "bare", "the id of an [machines.<id>] entry with kind = \"aws-ec2\"", true
	}
	return "", "", false
}

func (MachineLocal) isMachine()  {}
func (MachineSsh) isMachine()    {}
func (MachineAwsEc2) isMachine() {}

func (m MachineLocal) KindName() string              { return machineKindName(m) }
func (m MachineLocal) BuildCache() *TargetBuildCache { return machineBuildCache(m) }
func (m MachineLocal) validate(id string) error      { return machineValidate(m, id) }
func (m MachineLocal) MarshalJSON() ([]byte, error)  { return marshalMachineJSON(m) }

func (m MachineSsh) KindName() string              { return machineKindName(m) }
func (m MachineSsh) BuildCache() *TargetBuildCache { return machineBuildCache(m) }
func (m MachineSsh) validate(id string) error      { return machineValidate(m, id) }
func (m MachineSsh) MarshalJSON() ([]byte, error)  { return marshalMachineJSON(m) }

func (m MachineAwsEc2) KindName() string              { return machineKindName(m) }
func (m MachineAwsEc2) BuildCache() *TargetBuildCache { return machineBuildCache(m) }
func (m MachineAwsEc2) validate(id string) error      { return machineValidate(m, id) }
func (m MachineAwsEc2) MarshalJSON() ([]byte, error)  { return marshalMachineJSON(m) }

func (StoredTargetBare) isStoredTarget()           {}
func (StoredTargetPodman) isStoredTarget()         {}
func (StoredTargetDocker) isStoredTarget()         {}
func (StoredTargetAppleContainer) isStoredTarget() {}
func (StoredTargetNitro) isStoredTarget()          {}

func (s StoredTargetBare) Machine() string               { return storedTargetMachine(s) }
func (s StoredTargetBare) container() *ContainerTemplate { return storedTargetContainer(s) }
func (s StoredTargetBare) MarshalJSON() ([]byte, error)  { return marshalStoredTargetJSON(s) }

func (s StoredTargetPodman) Machine() string               { return storedTargetMachine(s) }
func (s StoredTargetPodman) container() *ContainerTemplate { return storedTargetContainer(s) }
func (s StoredTargetPodman) MarshalJSON() ([]byte, error)  { return marshalStoredTargetJSON(s) }

func (s StoredTargetDocker) Machine() string               { return storedTargetMachine(s) }
func (s StoredTargetDocker) container() *ContainerTemplate { return storedTargetContainer(s) }
func (s StoredTargetDocker) MarshalJSON() ([]byte, error)  { return marshalStoredTargetJSON(s) }

func (s StoredTargetAppleContainer) Machine() string               { return storedTargetMachine(s) }
func (s StoredTargetAppleContainer) container() *ContainerTemplate { return storedTargetContainer(s) }
func (s StoredTargetAppleContainer) MarshalJSON() ([]byte, error)  { return marshalStoredTargetJSON(s) }

func (s StoredTargetNitro) Machine() string               { return storedTargetMachine(s) }
func (s StoredTargetNitro) container() *ContainerTemplate { return storedTargetContainer(s) }
func (s StoredTargetNitro) MarshalJSON() ([]byte, error)  { return marshalStoredTargetJSON(s) }

// configBlank is Rust's `value.trim().is_empty()`.
func configBlank(value string) bool { return strings.TrimSpace(value) == "" }

// sshConnectionEqual is SshConnection's PartialEq: an absent list equals an
// empty one, as Rust's empty Vec does.
func sshConnectionEqual(a, b SshConnection) bool {
	return a.Host == b.Host && configOptionalEqual(a.User, b.User) && configOptionalEqual(a.IdentityFile, b.IdentityFile) &&
		slices.Equal(a.ExtraArgs, b.ExtraArgs)
}

// targetBuildCacheEqual is TargetBuildCache's PartialEq.
func targetBuildCacheEqual(a, b TargetBuildCache) bool {
	return configOptionalEqual(a.Enabled, b.Enabled) && configOptionalEqual(a.Directory, b.Directory) &&
		configOptionalEqual(a.ToolsDirectory, b.ToolsDirectory) && configOptionalEqual(a.MaxTotalSize, b.MaxTotalSize) &&
		configOptionalEqual(a.Remote, b.Remote) && configOptionalEqual(a.RemoteRegion, b.RemoteRegion) &&
		configOptionalEqual(a.Scheduler.CPUs, b.Scheduler.CPUs) && configOptionalEqual(a.Scheduler.Memory, b.Scheduler.Memory)
}

func optionalTargetBuildCacheEqual(a, b *TargetBuildCache) bool {
	if a == nil || b == nil {
		return a == b
	}
	return targetBuildCacheEqual(*a, *b)
}

func cloneTargetBuildCache(value *TargetBuildCache) *TargetBuildCache {
	if value == nil {
		return nil
	}
	copied := *value
	return &copied
}

// configOptionalEqual is Option<T>'s PartialEq on pointers.
func configOptionalEqual[T comparable](a, b *T) bool {
	if a == nil || b == nil {
		return a == b
	}
	return *a == *b
}

// machineEqual is Machine's PartialEq.
func machineEqual(a, b Machine) bool {
	switch a := a.(type) {
	case MachineLocal:
		b, ok := b.(MachineLocal)
		return ok && optionalTargetBuildCacheEqual(a.TargetBuildCache, b.TargetBuildCache)
	case MachineSsh:
		b, ok := b.(MachineSsh)
		return ok && sshConnectionEqual(a.SSH, b.SSH) && a.WorkspacePrefix == b.WorkspacePrefix &&
			optionalTargetBuildCacheEqual(a.TargetBuildCache, b.TargetBuildCache)
	case MachineAwsEc2:
		b, ok := b.(MachineAwsEc2)
		return ok && configOptionalEqual(a.AWSProfile, b.AWSProfile) && a.Region == b.Region &&
			a.LaunchTemplate == b.LaunchTemplate && configOptionalEqual(a.LaunchTemplateVersion, b.LaunchTemplateVersion) &&
			a.SSHUser == b.SSHUser && a.AddressSource == b.AddressSource &&
			configOptionalEqual(a.IdentityFile, b.IdentityFile) && slices.Equal(a.SSHArgs, b.SSHArgs)
	}
	return false
}
