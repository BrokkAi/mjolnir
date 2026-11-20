// Port of mj-core/src/targets/convert.rs.
//
// Conversions from the stored target types to the execution-plan types.
//
// Two families of target types exist on purpose. The configuration (config)
// and the session store (state) hold what is written to the configuration
// file and the session store: a kebab-case serde tag and path values. This
// module's targets (the Targets* types) hold what an execution plan needs: a
// snake_case tag and string paths, because every path here ends up as an
// argv element or inside a POSIX-quoted remote command string.
//
// Path text crosses into an execution plan here and nowhere else.

package core

import "fmt"

// pathText is the single place path text crosses into an execution plan.
func pathText(path string) string {
	return Text(path)
}

// TargetConversionErrorKind names a TargetConversionError variant.
type TargetConversionErrorKind int

const (
	// TargetConversionErrorRecordedKindMismatch carries Locator and Recorded.
	TargetConversionErrorRecordedKindMismatch TargetConversionErrorKind = iota
	TargetConversionErrorInvalidRecordedConnection
	// TargetConversionErrorKindMismatch: the locator's target kind is not the
	// template's target kind. Carries Locator and Template.
	TargetConversionErrorKindMismatch
	// TargetConversionErrorSshHostMismatch: both sides are SSH targets of the
	// same kind, but name different hosts. Carries Locator and Template.
	TargetConversionErrorSshHostMismatch
	// TargetConversionErrorMissingAwsAddress: an EC2 locator was stored
	// before its instance reported an address.
	TargetConversionErrorMissingAwsAddress
)

// TargetConversionError says why a stored locator and its target template
// cannot describe one target.
//
// A session records its locator and its template independently, so a
// configuration edit can leave the pair disagreeing. Each variant names what
// disagreed rather than collapsing into one message.
type TargetConversionError struct {
	Kind     TargetConversionErrorKind
	Locator  string
	Recorded string
	Template string
}

func (e *TargetConversionError) Error() string {
	switch e.Kind {
	case TargetConversionErrorRecordedKindMismatch:
		return fmt.Sprintf("session target kind %s differs from recorded kind %s", e.Locator, e.Recorded)
	case TargetConversionErrorInvalidRecordedConnection:
		return "recorded target connection has the wrong kind"
	case TargetConversionErrorKindMismatch:
		return fmt.Sprintf("session locator/template mismatch: locator is %s, template is %s", e.Locator, e.Template)
	case TargetConversionErrorSshHostMismatch:
		// %q matches Rust's {:?} for the printable host names this carries.
		return fmt.Sprintf("session locator/template SSH host mismatch: locator is %q, template is %q", e.Locator, e.Template)
	default:
		return "AWS locator has no address"
	}
}

// SSHArgsWithIdentity is the OpenSSH arguments that keep `ssh`
// non-interactive.
//
// Mjolnir drives ssh from a TUI; a host-key or password prompt would steal
// the terminal and wedge provisioning. `BatchMode` fails fast instead of
// prompting, and `accept-new` trusts a first-seen host key (fresh EC2
// instances are always first-seen) while still rejecting changed keys.
//
// OpenSSH uses the first value it sees for an option, so the user's
// `extra_args` come first and override these defaults: a machine with
// `-o StrictHostKeyChecking=yes` gets strict checking. Settings in
// `ssh_config` cannot override them, because command-line options always
// win over the config file.
func SSHArgsWithIdentity(args []string, identity *string) []string {
	result := append([]string{}, args...)
	result = append(result,
		"-o", "BatchMode=yes",
		"-o", "StrictHostKeyChecking=accept-new",
		"-o", "ConnectTimeout=15",
	)
	if identity != nil {
		result = append(result, "-i", pathText(*identity))
	}
	return result
}

// SshTargetFromSshConnection is the execution-plan form of a configured SSH
// connection (Rust: From<&SshConnection> for targets::SshTarget).
func SshTargetFromSshConnection(ssh *SshConnection) SshTarget {
	destination := ssh.Host
	if ssh.User != nil {
		destination = *ssh.User + "@" + ssh.Host
	}
	return SshTarget{Destination: destination, SSHArgs: SSHArgsWithIdentity(ssh.ExtraArgs, ssh.IdentityFile)}
}

// TargetTemplateImageHost is the host that downloads this configured
// target's image, with the container settings that name the image. false
// for targets that run no image (Rust: config::TargetTemplate::image_host,
// a free function here because the configured TargetTemplate belongs to the
// config port).
//
// This mirrors TargetsTargetTemplate.ImageHost for the stored form of a
// target, which is what the daemon reads when it decides what to download;
// the two template families never share a type.
func TargetTemplateImageHost(template TargetTemplate) (ImageHost, *ContainerTemplate, bool) {
	withSSH := func(kind ImageHostKind, ssh SshConnection, container ContainerTemplate) (ImageHost, *ContainerTemplate, bool) {
		target := SshTargetFromSshConnection(&ssh)
		return ImageHost{Kind: kind, SSH: &target}, &container, true
	}
	switch template := template.(type) {
	case TargetTemplateLocalPodman:
		return ImageHost{Kind: ImageHostLocalPodman}, &template.ContainerTemplate, true
	case TargetTemplateLocalDocker:
		return ImageHost{Kind: ImageHostLocalDocker}, &template.ContainerTemplate, true
	case TargetTemplateAppleContainer:
		return ImageHost{Kind: ImageHostAppleContainer}, &template.ContainerTemplate, true
	case TargetTemplateSshPodman:
		return withSSH(ImageHostSshPodman, template.SSH, template.ContainerTemplate)
	case TargetTemplateSshDocker:
		return withSSH(ImageHostSshDocker, template.SSH, template.ContainerTemplate)
	case TargetTemplateSshAppleContainer:
		return withSSH(ImageHostSshAppleContainer, template.SSH, template.ContainerTemplate)
	case TargetTemplateSshNitro:
		return withSSH(ImageHostSshNitro, template.SSH, template.ContainerTemplate)
	}
	return ImageHost{}, nil, false
}

// TargetsPodmanWorkspaceStorageFromPodmanWorkspaceStorage converts the
// configured workspace storage (Rust: From<&PodmanWorkspaceStorage> for
// targets::PodmanWorkspaceStorage).
func TargetsPodmanWorkspaceStorageFromPodmanWorkspaceStorage(storage PodmanWorkspaceStorage) TargetsPodmanWorkspaceStorage {
	switch storage := storage.(type) {
	case PodmanWorkspaceStorageHostHelper:
		return TargetsPodmanWorkspaceStorage{
			Kind:   TargetsPodmanWorkspaceStorageHostHelper,
			Root:   pathText(storage.Root),
			Helper: append([]string{}, storage.Helper...),
		}
	case PodmanWorkspaceStorageContainerLayer:
		return TargetsPodmanWorkspaceStorage{Kind: TargetsPodmanWorkspaceStorageContainerLayer}
	default:
		// PodmanVolume, the configured default.
		return TargetsPodmanWorkspaceStorage{Kind: TargetsPodmanWorkspaceStoragePodmanVolume}
	}
}

// TargetsPodmanWorkspaceLocatorFromPodmanWorkspaceLocator converts the
// stored workspace locator (Rust: From<&PodmanWorkspaceLocator> for
// targets::PodmanWorkspaceLocator).
func TargetsPodmanWorkspaceLocatorFromPodmanWorkspaceLocator(storage PodmanWorkspaceLocator) TargetsPodmanWorkspaceLocator {
	switch storage := storage.(type) {
	case PodmanWorkspaceLocatorVolume:
		return TargetsPodmanWorkspaceLocator{Kind: TargetsPodmanWorkspaceLocatorVolume, Name: storage.Name}
	case PodmanWorkspaceLocatorHostPath:
		return TargetsPodmanWorkspaceLocator{
			Kind:     TargetsPodmanWorkspaceLocatorHostPath,
			Path:     pathText(storage.Path),
			Helper:   append([]string{}, storage.Helper...),
			Resource: storage.Resource,
		}
	default:
		// ContainerLayer, the stored default.
		return TargetsPodmanWorkspaceLocator{Kind: TargetsPodmanWorkspaceLocatorContainerLayer}
	}
}

// PodmanWorkspaceLocatorFromTargetsPodmanWorkspaceLocator converts back to
// the stored workspace locator (Rust: From<targets::PodmanWorkspaceLocator>
// for PodmanWorkspaceLocator).
func PodmanWorkspaceLocatorFromTargetsPodmanWorkspaceLocator(storage TargetsPodmanWorkspaceLocator) PodmanWorkspaceLocator {
	switch storage.EffectiveKind() {
	case TargetsPodmanWorkspaceLocatorVolume:
		return PodmanWorkspaceLocatorVolume{Name: storage.Name}
	case TargetsPodmanWorkspaceLocatorHostPath:
		return PodmanWorkspaceLocatorHostPath{Path: storage.Path, Helper: storage.Helper, Resource: storage.Resource}
	default:
		return PodmanWorkspaceLocatorContainerLayer{}
	}
}

// TargetsStoredTarget is one session's stored target: what the store holds,
// plus the configuration and the session identity the stored form
// deliberately does not repeat (Rust: targets::StoredTarget; prefixed
// because config/machines also has a StoredTarget).
//
// An SSH locator records only the host and an EC2 locator only the instance
// and its resolved address, while an execution plan needs the whole `ssh`
// invocation, the AWS profile and region, and the session's workspace path.
type TargetsStoredTarget struct {
	Locator   TargetLocator
	Template  TargetTemplate
	SessionID string
}

// RecordedTarget is a provisioned target paired with its durable access
// settings.
type RecordedTarget struct {
	Locator   TargetLocator
	Runtime   *TargetRuntimeSettings
	SessionID string
}

// TargetsTargetLocatorFromStoredTarget converts a stored target (Rust:
// TryFrom<StoredTarget> for targets::TargetLocator).
func TargetsTargetLocatorFromStoredTarget(stored TargetsStoredTarget) (TargetsTargetLocator, error) {
	if LocatorKindName(stored.Locator) != stored.Template.KindName() {
		return TargetsTargetLocator{}, &TargetConversionError{
			Kind:     TargetConversionErrorKindMismatch,
			Locator:  LocatorKindName(stored.Locator),
			Template: stored.Template.KindName(),
		}
	}
	runtime := TargetRuntimeSettingsFromTemplate(stored.Template)
	return TargetsTargetLocatorFromRecordedTarget(RecordedTarget{
		Locator:   stored.Locator,
		Runtime:   &runtime,
		SessionID: stored.SessionID,
	})
}

// TargetsTargetLocatorFromRecordedTarget converts a recorded target (Rust:
// TryFrom<RecordedTarget> for targets::TargetLocator).
func TargetsTargetLocatorFromRecordedTarget(stored RecordedTarget) (TargetsTargetLocator, error) {
	locator, runtime := stored.Locator, stored.Runtime
	if runtime != nil {
		if LocatorKindName(locator) != runtime.Kind {
			return TargetsTargetLocator{}, &TargetConversionError{
				Kind:     TargetConversionErrorRecordedKindMismatch,
				Locator:  LocatorKindName(locator),
				Recorded: runtime.Kind,
			}
		}
		if _, local := runtime.Connection.(TargetConnectionLocal); !LocatorNeedsConnection(locator) && !local {
			return TargetsTargetLocator{}, &TargetConversionError{Kind: TargetConversionErrorInvalidRecordedConnection}
		}
	}
	ssh := func(host string) (*SshTarget, error) {
		var connection TargetConnectionSsh
		ok := false
		if runtime != nil {
			connection, ok = runtime.Connection.(TargetConnectionSsh)
		}
		if !ok {
			return nil, &TargetConversionError{Kind: TargetConversionErrorInvalidRecordedConnection}
		}
		if host != connection.SSH.Host {
			return nil, &TargetConversionError{
				Kind:     TargetConversionErrorSshHostMismatch,
				Locator:  host,
				Template: connection.SSH.Host,
			}
		}
		target := SshTargetFromSshConnection(&connection.SSH)
		return &target, nil
	}
	cloned := func(value *string) *string {
		if value == nil {
			return nil
		}
		copied := *value
		return &copied
	}
	switch locator := locator.(type) {
	case TargetLocatorLocalBare:
		return TargetsTargetLocator{Kind: TargetsTargetLocatorLocalBare, WorkerRoot: pathText(locator.WorkerRoot)}, nil
	case TargetLocatorLocalPodman:
		return TargetsTargetLocator{
			Kind:             TargetsTargetLocatorLocalPodman,
			ContainerID:      locator.ContainerID,
			WorkspaceStorage: TargetsPodmanWorkspaceLocatorFromPodmanWorkspaceLocator(locator.WorkspaceStorage),
			BorrowedFrom:     cloned(locator.BorrowedFrom),
		}, nil
	case TargetLocatorLocalDocker:
		return TargetsTargetLocator{Kind: TargetsTargetLocatorLocalDocker, ContainerID: locator.ContainerID, BorrowedFrom: cloned(locator.BorrowedFrom)}, nil
	case TargetLocatorAppleContainer:
		return TargetsTargetLocator{Kind: TargetsTargetLocatorAppleContainer, ContainerID: locator.ContainerID, BorrowedFrom: cloned(locator.BorrowedFrom)}, nil
	case TargetLocatorSshBare:
		target, err := ssh(locator.Host)
		if err != nil {
			return TargetsTargetLocator{}, err
		}
		return TargetsTargetLocator{Kind: TargetsTargetLocatorSshBare, SSH: target, Workspace: pathText(locator.Workspace), WorkerID: cloned(locator.WorkerID)}, nil
	case TargetLocatorSshPodman:
		target, err := ssh(locator.Host)
		if err != nil {
			return TargetsTargetLocator{}, err
		}
		return TargetsTargetLocator{
			Kind:             TargetsTargetLocatorSshPodman,
			SSH:              target,
			ContainerID:      locator.ContainerID,
			WorkspaceStorage: TargetsPodmanWorkspaceLocatorFromPodmanWorkspaceLocator(locator.WorkspaceStorage),
			BorrowedFrom:     cloned(locator.BorrowedFrom),
		}, nil
	case TargetLocatorSshDocker:
		target, err := ssh(locator.Host)
		if err != nil {
			return TargetsTargetLocator{}, err
		}
		return TargetsTargetLocator{Kind: TargetsTargetLocatorSshDocker, SSH: target, ContainerID: locator.ContainerID, BorrowedFrom: cloned(locator.BorrowedFrom)}, nil
	case TargetLocatorSshAppleContainer:
		target, err := ssh(locator.Host)
		if err != nil {
			return TargetsTargetLocator{}, err
		}
		return TargetsTargetLocator{Kind: TargetsTargetLocatorSshAppleContainer, SSH: target, ContainerID: locator.ContainerID, BorrowedFrom: cloned(locator.BorrowedFrom)}, nil
	case TargetLocatorSshNitro:
		target, err := ssh(locator.Host)
		if err != nil {
			return TargetsTargetLocator{}, err
		}
		return TargetsTargetLocator{Kind: TargetsTargetLocatorSshNitro, SSH: target, ContainerID: locator.ContainerID, BorrowedFrom: cloned(locator.BorrowedFrom)}, nil
	case TargetLocatorAwsEc2:
		var connection TargetConnectionAws
		ok := false
		if runtime != nil {
			connection, ok = runtime.Connection.(TargetConnectionAws)
		}
		if !ok {
			return TargetsTargetLocator{}, &TargetConversionError{Kind: TargetConversionErrorInvalidRecordedConnection}
		}
		if locator.Address == nil {
			return TargetsTargetLocator{}, &TargetConversionError{Kind: TargetConversionErrorMissingAwsAddress}
		}
		return TargetsTargetLocator{
			Kind:       TargetsTargetLocatorAwsEc2,
			Profile:    connection.Profile,
			Region:     connection.Region,
			InstanceID: locator.InstanceID,
			SSH: &SshTarget{
				Destination: connection.SSHUser + "@" + *locator.Address,
				SSHArgs:     SSHArgsWithIdentity(connection.SSHArgs, connection.IdentityFile),
			},
			Workspace: AWSWorkspace(stored.SessionID),
		}, nil
	}
	panic(fmt.Sprintf("unknown target locator %T", locator))
}

// LocatorKindName is the stored locator's target kind, spelled as the
// configured TargetTemplate's kind name spells it so a mismatch names both
// sides the same way.
func LocatorKindName(locator TargetLocator) string {
	switch locator.(type) {
	case TargetLocatorLocalBare:
		return "local-bare"
	case TargetLocatorLocalPodman:
		return "local-podman"
	case TargetLocatorLocalDocker:
		return "local-docker"
	case TargetLocatorAppleContainer:
		return "apple-container"
	case TargetLocatorAwsEc2:
		return "aws-ec2"
	case TargetLocatorSshBare:
		return "ssh-bare"
	case TargetLocatorSshPodman:
		return "ssh-podman"
	case TargetLocatorSshDocker:
		return "ssh-docker"
	case TargetLocatorSshAppleContainer:
		return "ssh-apple-container"
	case TargetLocatorSshNitro:
		return "ssh-nitro"
	}
	panic(fmt.Sprintf("unknown target locator %T", locator))
}

// LocatorNeedsConnection: local resource locators contain everything needed
// to reach the resource.
func LocatorNeedsConnection(locator TargetLocator) bool {
	switch locator.(type) {
	case TargetLocatorSshBare, TargetLocatorSshPodman, TargetLocatorSshDocker,
		TargetLocatorSshAppleContainer, TargetLocatorSshNitro, TargetLocatorAwsEc2:
		return true
	}
	return false
}
