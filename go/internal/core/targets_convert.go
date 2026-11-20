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
	panic("unported: mj-core/src/targets/convert.rs path_text")
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
	panic("unported: mj-core/src/targets/convert.rs From<&SshConnection> for SshTarget")
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
	panic("unported: mj-core/src/targets/convert.rs config::TargetTemplate::image_host")
}

// TargetsPodmanWorkspaceStorageFromPodmanWorkspaceStorage converts the
// configured workspace storage (Rust: From<&PodmanWorkspaceStorage> for
// targets::PodmanWorkspaceStorage).
func TargetsPodmanWorkspaceStorageFromPodmanWorkspaceStorage(storage PodmanWorkspaceStorage) TargetsPodmanWorkspaceStorage {
	panic("unported: mj-core/src/targets/convert.rs From<&PodmanWorkspaceStorage>")
}

// TargetsPodmanWorkspaceLocatorFromPodmanWorkspaceLocator converts the
// stored workspace locator (Rust: From<&PodmanWorkspaceLocator> for
// targets::PodmanWorkspaceLocator).
func TargetsPodmanWorkspaceLocatorFromPodmanWorkspaceLocator(storage PodmanWorkspaceLocator) TargetsPodmanWorkspaceLocator {
	panic("unported: mj-core/src/targets/convert.rs From<&PodmanWorkspaceLocator>")
}

// PodmanWorkspaceLocatorFromTargetsPodmanWorkspaceLocator converts back to
// the stored workspace locator (Rust: From<targets::PodmanWorkspaceLocator>
// for PodmanWorkspaceLocator).
func PodmanWorkspaceLocatorFromTargetsPodmanWorkspaceLocator(storage TargetsPodmanWorkspaceLocator) PodmanWorkspaceLocator {
	panic("unported: mj-core/src/targets/convert.rs From<targets::PodmanWorkspaceLocator>")
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
	panic("unported: mj-core/src/targets/convert.rs TryFrom<StoredTarget>")
}

// TargetsTargetLocatorFromRecordedTarget converts a recorded target (Rust:
// TryFrom<RecordedTarget> for targets::TargetLocator).
func TargetsTargetLocatorFromRecordedTarget(stored RecordedTarget) (TargetsTargetLocator, error) {
	panic("unported: mj-core/src/targets/convert.rs TryFrom<RecordedTarget>")
}

// LocatorKindName is the stored locator's target kind, spelled as the
// configured TargetTemplate's kind name spells it so a mismatch names both
// sides the same way.
func LocatorKindName(locator TargetLocator) string {
	panic("unported: mj-core/src/targets/convert.rs locator_kind_name")
}

// LocatorNeedsConnection: local resource locators contain everything needed
// to reach the resource.
func LocatorNeedsConnection(locator TargetLocator) bool {
	panic("unported: mj-core/src/targets/convert.rs locator_needs_connection")
}
