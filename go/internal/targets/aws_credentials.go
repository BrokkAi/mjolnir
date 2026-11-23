// Port of mj-targets/src/aws_credentials.rs.
//
// Resolve the host AWS chain once, then provision only its selected
// identity.
//
// Name collisions: credentials_withheld is CredentialsWithheldFor beside
// the CredentialsWithheld type.

package targets

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func hasCredentialSource(environment map[string]string) bool {
	for _, key := range []string{
		"AWS_ACCESS_KEY_ID",
		"AWS_SECRET_ACCESS_KEY",
		"AWS_BEARER_TOKEN_BEDROCK",
		"AWS_CONFIG_FILE",
		"AWS_SHARED_CREDENTIALS_FILE",
		"AWS_WEB_IDENTITY_TOKEN_FILE",
		"AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
		"AWS_CONTAINER_CREDENTIALS_FULL_URI",
	} {
		if environment[key] != "" {
			return true
		}
	}
	value, ok := environment["AWS_EC2_METADATA_DISABLED"]
	return ok && value == "false"
}

// SessionHost is where a session runs, as far as AWS credentials care.
// Built from a session's locator or from a configured target template; both
// matches are exhaustive, so a new target kind must decide here.
type SessionHost int

const (
	// SessionHostRawLocal is the controller's own machine and user: the
	// session reads the host's AWS files itself.
	SessionHostRawLocal SessionHost = iota
	// SessionHostEc2 is an EC2 instance, which uses its instance role.
	SessionHostEc2
	// SessionHostIsolated is a container or a remote machine, which has no
	// AWS credentials unless the controller delivers them.
	SessionHostIsolated
)

// SessionHostFromTargetLocator is Rust's From<&TargetLocator> for
// SessionHost.
func SessionHostFromTargetLocator(locator *core.TargetsTargetLocator) SessionHost {
	switch locator.Kind {
	case core.TargetsTargetLocatorLocalBare:
		return SessionHostRawLocal
	case core.TargetsTargetLocatorAwsEc2:
		return SessionHostEc2
	default:
		return SessionHostIsolated
	}
}

// SessionHostFromTargetTemplate is Rust's From<&config::TargetTemplate> for
// SessionHost.
func SessionHostFromTargetTemplate(template core.TargetTemplate) SessionHost {
	switch template.(type) {
	case core.TargetTemplateLocalBare:
		return SessionHostRawLocal
	case core.TargetTemplateAwsEc2:
		return SessionHostEc2
	default:
		return SessionHostIsolated
	}
}

// CredentialsWithheld is why a session on this profile and host does not
// receive AWS credentials from the controller. One definition of the
// question, used by launch, the sync and `mj doctor`. Explicit credentials
// in the target's or the profile's environment stay with their owner.
type CredentialsWithheld int

const (
	// CredentialsWithheldNotBedrock: the profile does not authenticate with
	// the AWS credential chain.
	CredentialsWithheldNotBedrock CredentialsWithheld = iota
	CredentialsWithheldRawLocal
	CredentialsWithheldEc2
	// CredentialsWithheldTargetEnvironment: the target's environment names
	// an AWS profile or credential source.
	CredentialsWithheldTargetEnvironment
	// CredentialsWithheldProfileEnvironment: the profile's environment names
	// an AWS credential source.
	CredentialsWithheldProfileEnvironment
)

// WhyARoleCannotApply is what to say about a `[profiles.<id>.aws]` table
// that therefore cannot apply, in terms of what the session uses instead.
func (w CredentialsWithheld) WhyARoleCannotApply() string {
	switch w {
	case CredentialsWithheldNotBedrock:
		return "the profile does not authenticate with the AWS credential chain (Amazon Bedrock)"
	case CredentialsWithheldRawLocal:
		return "raw local sessions read the host's own AWS files directly, so a role cannot limit them"
	case CredentialsWithheldEc2:
		return "EC2 sessions use the instance's own role, which a role here cannot limit"
	case CredentialsWithheldTargetEnvironment:
		return "the target's environment sets its own AWS profile or credentials, which take precedence"
	default:
		return "the profile's environment sets its own AWS credentials, which take precedence"
	}
}

// CredentialsWithheldFor is why a session on this profile and host does not
// receive AWS credentials; false means it does (Rust: credentials_withheld).
func CredentialsWithheldFor(profile *core.HarnessProfile, host SessionHost, targetEnvironment map[string]string) (CredentialsWithheld, bool) {
	panic("unported: mj-targets/src/aws_credentials.rs credentials_withheld")
}

// IsBedrock reports whether sessions on this profile authenticate with the
// AWS credential chain: CredentialsWithheldFor (so launch, the unused-role
// notice and the sync's decision to mint) and `mj doctor` ask it. Other code
// that reads AuthScheme for other reasons does not. A profile whose
// configuration cannot be read is unknown, and with a role it is treated as
// Bedrock (see unreadableProvider). It decides whether credentials are
// minted, never whether a configured role withdraws the ones a session
// holds.
func IsBedrock(profile *core.HarnessProfile) bool {
	panic("unported: mj-targets/src/aws_credentials.rs is_bedrock")
}

// unreadableProvider is why the profile's Codex configuration cannot be
// read, when a role is configured and it cannot. Unreadable is unknown, not
// "not Bedrock": the worker still runs the configuration it was staged with,
// so a profile with a role is treated as Bedrock until its configuration can
// be read, and an error is reported rather than the session's AWS
// credentials left unmanaged.
func unreadableProvider(profile *core.HarnessProfile) (string, bool) {
	panic("unported: mj-targets/src/aws_credentials.rs unreadable_provider")
}

func ReceivesSyncedCredentials(profile *core.HarnessProfile, host SessionHost, targetEnvironment map[string]string) bool {
	_, withheld := CredentialsWithheldFor(profile, host, targetEnvironment)
	return !withheld
}

// UnusedRoleNotice is what to tell the person when a profile has a
// `[profiles.<id>.aws]` table that cannot apply to a session on this target,
// and why. The session runs without it rather than being refused.
func UnusedRoleNotice(profileID string, profile *core.HarnessProfile, host SessionHost, targetEnvironment map[string]string) (string, bool) {
	panic("unported: mj-targets/src/aws_credentials.rs unused_role_notice")
}

// Source is the AWS credential source for a session; nil when explicit
// target credentials or an EC2 instance role stay target-owned.
func Source(profile *core.HarnessProfile, backend *core.TargetsTargetLocator, targetEnvironment map[string]string) (*core.AwsCredentialSource, error) {
	panic("unported: mj-targets/src/aws_credentials.rs source")
}

// SyncSource is Source for the credential sync.
//
// A profile without a role is the unscoped path, unchanged from before role
// scoping: the same source launch builds, resolved when the targets are
// built.
//
// A profile with a role is the role-scoped path. Every session of the
// profile that Mjolnir may have staged credentials into is managed, whether
// or not it still receives any: a role configured for a profile means no
// session of it keeps host credentials, so the sync withdraws what does not
// match the role's scope, and Mints says whether it also mints. The source
// carries a pointer to the harness configuration that may select the host's
// AWS profile; the mint reads it when it runs (Resolve), because an edit to
// that file does not rebuild the targets, and a file that cannot be read
// fails that cycle closed. Raw local and EC2 sessions have no credentials of
// Mjolnir's to withdraw.
func SyncSource(profile *core.HarnessProfile, backend *core.TargetsTargetLocator, targetEnvironment map[string]string) (*core.AwsCredentialSource, error) {
	panic("unported: mj-targets/src/aws_credentials.rs sync_source")
}

// Resolve is the source with the AWS profile the harness's configuration
// selects, read now. The role-scoped mint calls this each time. An error
// means the configuration cannot be read: nothing can be minted for that
// attempt.
func Resolve(source *core.AwsCredentialSource) (core.AwsCredentialSource, error) {
	panic("unported: mj-targets/src/aws_credentials.rs resolve")
}

// SelectedSource is the environment the AWS CLI runs in to resolve this
// profile's identity on the controller host: the profile's own environment
// with the AWS profile it selects, read now. Callers have already decided
// that the profile receives synced credentials.
func SelectedSource(profile *core.HarnessProfile) (core.AwsCredentialSource, error) {
	base := baseSource(profile)
	source, err := Resolve(&base)
	if err != nil {
		return core.AwsCredentialSource{}, err
	}
	// Resolved, so nothing is left to read later.
	source.Selection = nil
	return source, nil
}

// baseSource is the profile's own environment with the AWS profile it
// names, before the harness's configuration is consulted.
func baseSource(profile *core.HarnessProfile) core.AwsCredentialSource {
	panic("unported: mj-targets/src/aws_credentials.rs base_source")
}

func Export(source *core.AwsCredentialSource, executor core.CommandExecutor) (core.AwsCredentials, error) {
	panic("unported: mj-targets/src/aws_credentials.rs export")
}
