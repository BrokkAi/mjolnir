// Port of mj-core/src/aws_credentials.rs.

// Selected AWS credentials transported privately from controller to worker.
//
// The module's free functions and constants carry an `AwsCredentials`
// prefix (AwsCredentialsState for aws_credentials::state, ...) because their
// Rust names are too generic for the flat package.

package core

import (
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"maps"
	"os"
	"slices"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

const (
	AwsCredentialsSharedCredentialsFile = "aws-credentials"
	AwsCredentialsConfigFile            = "aws-config"
	AwsCredentialsEmptyCredentialsFile  = "aws-empty-credentials"
	// AwsCredentialsScopeFile is beside the secret: which role and session
	// Mjolnir issued it for.
	AwsCredentialsScopeFile = "aws-scope.json"
)

// AwsCredentialsFiles is every file the AWS credential provider keeps in a
// session's home. One list, for staging, refresh and removal alike.
var AwsCredentialsFiles = [4]string{
	AwsCredentialsSharedCredentialsFile,
	AwsCredentialsConfigFile,
	AwsCredentialsEmptyCredentialsFile,
	AwsCredentialsScopeFile,
}

// AwsCredentialScope is which credentials Mjolnir issued, for what, until
// when. The worker keeps it beside the secret, so a controller that has lost
// its memory (a restart) can still tell credentials it issued for a
// session's role from anything else.
type AwsCredentialScope struct {
	RoleArn string `json:"role_arn"`
	// RoleSessionName is the STS role session name, `mj-<session id>`.
	RoleSessionName string `json:"role_session_name"`
	// SessionPolicyFingerprint is the fingerprint of the effective session
	// policy the credentials were issued under. A narrower policy configured
	// since makes them broader than configured.
	SessionPolicyFingerprint string `json:"session_policy_fingerprint"`
	// SourceIdentity is the AWS `SourceIdentity` set when the credentials
	// were issued: the role session name, or nil when `source_identity` was
	// off. Credentials issued before it was enabled lack the attribution
	// configured now.
	SourceIdentity       *string `json:"source_identity"`
	ExpiresAtEpochMillis int64   `json:"expires_at_epoch_millis"`
	// Tag is a keyed MAC over the other fields and the secret's fingerprint,
	// made by the controller with a key only it holds. The worker's home is
	// writable by the session, so without it the session could write a
	// record that vouches for any credentials; the controller trusts a
	// record only when this verifies.
	Tag string `json:"tag"`
}

// SignedPayload is the bytes the tag authenticates: every field but the
// tag, and the fingerprint of the secret the record describes, so a tag
// cannot be moved to other credentials. It is serde_json's encoding of the
// tuple, byte for byte.
func (s *AwsCredentialScope) SignedPayload(secretFingerprint string) []byte {
	var sourceIdentity any
	if s.SourceIdentity != nil {
		sourceIdentity = *s.SourceIdentity
	}
	payload, err := jsonvalue.Marshal([]any{
		"mjolnir-aws-scope-v1",
		secretFingerprint,
		s.RoleArn,
		s.RoleSessionName,
		s.SessionPolicyFingerprint,
		sourceIdentity,
		s.ExpiresAtEpochMillis,
	})
	if err != nil {
		panic("a tuple of strings and numbers serializes: " + err.Error())
	}
	return payload
}

// awsStoredScope is what the worker stores: the scope, bound to the secret
// it describes.
type awsStoredScope struct {
	Scope             AwsCredentialScope `json:"scope"`
	SecretFingerprint string             `json:"secret_fingerprint"`
}

// AwsCredentialSource is the controller environment used by the AWS CLI to
// resolve one profile. Its String form names the environment's variables
// but never their values.
type AwsCredentialSource struct {
	Environment map[string]string
	// Role is the role each session assumes instead of receiving the host's
	// own credentials. Part of the source, and so of the sync target's
	// identity: a configuration change makes obsolete queued work compare
	// unequal.
	Role *AwsRoleConfig
	// Mints is whether credentials are minted for the role. A role is
	// configured for every session of the profile, but only a session that
	// receives synced credentials gets any: for a profile that does not
	// authenticate with the AWS credential chain nothing is minted.
	// Credentials a session holds that do not match the role's scope are
	// still withdrawn; role-scoped credentials it already holds stay until
	// they expire.
	Mints bool
	// Selection is where the harness's own configuration may name the AWS
	// profile the role-scoped mint runs as (Codex's provider table). Only the
	// pointer is part of the source and of the sync target's identity; the
	// file is read each time a credential is minted, because an edit to it
	// does not rebuild the target.
	Selection *AwsProfileSelection
}

// Equal compares sources by value, as Rust's PartialEq does.
func (s *AwsCredentialSource) Equal(other *AwsCredentialSource) bool {
	panic("unported: mj-core/src/aws_credentials.rs AwsCredentialSource PartialEq")
}

// String is Rust's Debug form, which leaves the environment's values out.
func (s AwsCredentialSource) String() string {
	names := slices.Sorted(maps.Keys(s.Environment))
	role := "None"
	if s.Role != nil {
		role = fmt.Sprintf("Some(%q)", s.Role.RoleArn)
	}
	return fmt.Sprintf("AwsCredentialSource { environment_names: %q, role: %s }", names, role)
}

// GoString keeps %#v from printing the environment's values.
func (s AwsCredentialSource) GoString() string { return s.String() }

// AwsProfileSelection is the configuration file that may select the AWS
// profile.
type AwsProfileSelection struct {
	Home string
	// Overlay is the Codex profile overlay the profile runs with, if any.
	Overlay *string
}

// AwsCredentials is an AWS CLI `process` export plus the selected profile
// name. All but the profile are secrets: String and GoString never print
// them (Rust gives the type no Debug at all).
type AwsCredentials struct {
	version         uint32
	accessKeyID     string
	secretAccessKey string
	sessionToken    *string
	expiration      *string
	profile         string
	// scope is not part of the credential file the SDK reads; it travels
	// beside it.
	scope *AwsCredentialScope
}

// awsCredentialsWire is the serde form of AwsCredentials (PascalCase keys,
// optional members skipped when absent, Profile defaulting to "default").
type awsCredentialsWire struct {
	Version         uint32  `json:"Version"`
	AccessKeyID     string  `json:"AccessKeyId"`
	SecretAccessKey string  `json:"SecretAccessKey"`
	SessionToken    *string `json:"SessionToken,omitempty"`
	Expiration      *string `json:"Expiration,omitempty"`
	Profile         *string `json:"Profile,omitempty"`
}

// MarshalJSON writes serde's encoding byte for byte: members in declaration
// order, with serde_json's string escapes.
func (c AwsCredentials) MarshalJSON() ([]byte, error) {
	object := jsonvalue.Object{
		{Key: "Version", Value: int64(c.version)},
		{Key: "AccessKeyId", Value: c.accessKeyID},
		{Key: "SecretAccessKey", Value: c.secretAccessKey},
	}
	if c.sessionToken != nil {
		object = append(object, jsonvalue.Member{Key: "SessionToken", Value: *c.sessionToken})
	}
	if c.expiration != nil {
		object = append(object, jsonvalue.Member{Key: "Expiration", Value: *c.expiration})
	}
	object = append(object, jsonvalue.Member{Key: "Profile", Value: c.profile})
	return jsonvalue.Marshal(object)
}

// UnmarshalJSON reads serde's encoding; a missing Profile is "default".
func (c *AwsCredentials) UnmarshalJSON(data []byte) error {
	var wire awsCredentialsWire
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	profile := awsCredentialsDefaultProfile()
	if wire.Profile != nil {
		profile = *wire.Profile
	}
	*c = AwsCredentials{
		version:         wire.Version,
		accessKeyID:     wire.AccessKeyID,
		secretAccessKey: wire.SecretAccessKey,
		sessionToken:    wire.SessionToken,
		expiration:      wire.Expiration,
		profile:         profile,
	}
	return nil
}

// String names only the profile.
func (c AwsCredentials) String() string { return "AwsCredentials { profile: " + c.profile + " }" }

// GoString keeps %#v from printing the secrets.
func (c AwsCredentials) GoString() string { return c.String() }

func awsCredentialsDefaultProfile() string { return "default" }

func AwsCredentialsFromExport(profile string, bytes []byte) (AwsCredentials, error) {
	credentials, err := AwsCredentialsDecode(bytes)
	if err != nil {
		return AwsCredentials{}, err
	}
	credentials.profile = profile
	if err := credentials.validate(); err != nil {
		return AwsCredentials{}, err
	}
	return credentials, nil
}

// AwsCredentialsFromAssumeRole reads credentials from `aws sts assume-role
// --output json`, whose output nests the same four fields under
// `Credentials`.
func AwsCredentialsFromAssumeRole(profile string, bytes []byte) (AwsCredentials, error) {
	panic("unported: mj-core/src/aws_credentials.rs AwsCredentials::from_assume_role")
}

// IssuedFor is these credentials, recorded as issued by Mjolnir for roleArn
// and the STS session roleSessionName.
func (c AwsCredentials) IssuedFor(roleArn, roleSessionName, sessionPolicyFingerprint string, sourceIdentity *string) (AwsCredentials, error) {
	expiresAt, ok := c.ExpiresAtEpochMillis()
	if !ok {
		return AwsCredentials{}, errors.New("issued AWS credentials have no expiration")
	}
	c.scope = &AwsCredentialScope{
		RoleArn:                  roleArn,
		RoleSessionName:          roleSessionName,
		SessionPolicyFingerprint: sessionPolicyFingerprint,
		SourceIdentity:           sourceIdentity,
		ExpiresAtEpochMillis:     expiresAt,
	}
	return c, nil
}

// SignScope authenticates the scope with sign, given the scope's fields and
// the fingerprint of the secret that will be installed.
func (c AwsCredentials) SignScope(sign func(scope *AwsCredentialScope, secretFingerprint string) (string, error)) (AwsCredentials, error) {
	encoded, err := c.Encode()
	if err != nil {
		return AwsCredentials{}, err
	}
	fingerprint := CredentialFingerprint(encoded)
	if c.scope != nil {
		scope := *c.scope
		tag, err := sign(&scope, fingerprint)
		if err != nil {
			return AwsCredentials{}, err
		}
		scope.Tag = tag
		c.scope = &scope
	}
	return c, nil
}

func (c AwsCredentials) Scope() *AwsCredentialScope { return c.scope }

// Profile is the AWS profile these credentials are installed under: the
// section name in the session's AWS configuration.
func (c AwsCredentials) Profile() string { return c.profile }

// WithProfile is these credentials, installed under another profile name.
// The name is part of the secret, so this is done before the scope is
// signed.
func (c AwsCredentials) WithProfile(profile string) (AwsCredentials, error) {
	if !AwsCredentialsValidProfileName(profile) {
		return AwsCredentials{}, fmt.Errorf("AWS profile name %q is not valid", profile)
	}
	c.profile = profile
	return c, nil
}

// WithScope attaches a scope read from the wire.
func (c AwsCredentials) WithScope(scope *AwsCredentialScope) AwsCredentials {
	c.scope = scope
	return c
}

// ExpiresAtEpochMillis is when these credentials stop working, in
// milliseconds since the Unix epoch. False for credentials that do not
// expire.
func (c AwsCredentials) ExpiresAtEpochMillis() (int64, bool) {
	if c.expiration == nil {
		return 0, false
	}
	return credentialRFC3339Millis(*c.expiration)
}

func AwsCredentialsDecode(bytes []byte) (AwsCredentials, error) {
	if len(bytes) > MaxCredentialBytes {
		return AwsCredentials{}, errors.New("AWS credential export is too large")
	}
	var credentials AwsCredentials
	// A detailed decoding error can include an offending secret value.
	if err := json.Unmarshal(bytes, &credentials); err != nil {
		return AwsCredentials{}, errors.New("AWS credential export is not valid process-format JSON")
	}
	if err := credentials.validate(); err != nil {
		return AwsCredentials{}, err
	}
	return credentials, nil
}

func (c AwsCredentials) Encode() ([]byte, error) {
	if err := c.validate(); err != nil {
		return nil, err
	}
	encoded, err := c.MarshalJSON()
	if err != nil {
		return nil, fmt.Errorf("encode AWS credentials: %w", err)
	}
	return encoded, nil
}

func (c AwsCredentials) validate() error {
	panic("unported: mj-core/src/aws_credentials.rs AwsCredentials::validate")
}

// Configuration is the AWS configuration naming the credential process. The
// native process provider honors expiration and reloads renewed
// credentials. A static INI entry can be cached forever by an AWS SDK.
func (c AwsCredentials) Configuration(targetHome string) ([]byte, error) {
	panic("unported: mj-core/src/aws_credentials.rs AwsCredentials::configuration")
}

// Install installs the complete provider, in staging or at its final home.
// The JSON is published last, after the files the SDK needs to resolve it.
func (c AwsCredentials) Install(home, targetHome string) (CredentialSnapshot, error) {
	panic("unported: mj-core/src/aws_credentials.rs AwsCredentials::install")
}

// AwsCredentialsRemove withdraws the session's AWS credentials. The
// credential file is the secret; without it the native process provider
// finds nothing to use, and a later install writes every file again.
func AwsCredentialsRemove(home string) (CredentialSnapshot, error) {
	panic("unported: mj-core/src/aws_credentials.rs remove")
}

func removeIfPresent(path string) error {
	if err := os.Remove(path); err != nil && !errors.Is(err, fs.ErrNotExist) {
		return fmt.Errorf("remove %s: %w", path, err)
	}
	return nil
}

// AwsCredentialsValidProfileName is whether name can be the section name of
// an AWS configuration (`[profile <name>]`). The one rule: launch validates
// the credentials it stages with it, and the name read back from a worker is
// held to it too, so staging and reading accept the same names. (It allows
// spaces and `@`; it does not say whether the AWS CLI trims a name with
// leading or trailing whitespace.)
func AwsCredentialsValidProfileName(name string) bool {
	// Rust's char::is_control is the Cc category: C0, DEL and C1.
	return name != "" && !strings.ContainsFunc(name, func(c rune) bool {
		return c < 0x20 || (c >= 0x7f && c <= 0x9f) || c == '[' || c == ']'
	})
}

// AwsCredentialsStagedProfile is the AWS profile this session was staged
// with: the section name in its AWS configuration. Role-scoped credentials
// are installed under it, so a renewal keeps the name the session's
// environment selects. The session can edit the file, which is safe here
// only because what is installed under the name is role-scoped: a changed
// name changes where those credentials are written, not what they allow.
// False when nothing was staged or no section has a valid name.
func AwsCredentialsStagedProfile(home string) (string, bool, error) {
	panic("unported: mj-core/src/aws_credentials.rs staged_profile")
}

// AwsCredentialsScope is the scope Mjolnir recorded for the credentials in
// home, when the record describes the secret that is there now.
func AwsCredentialsScope(home string) (*AwsCredentialScope, error) {
	panic("unported: mj-core/src/aws_credentials.rs scope")
}

func AwsCredentialsSnapshot(bytes []byte) CredentialSnapshot {
	return CredentialSnapshot{Present: true, Fingerprint: CredentialFingerprint(bytes)}
}

func AwsCredentialsState(home string) (CredentialSnapshot, error) {
	if user, ok := userHome(); ok {
		return AwsCredentialsStateForUser(home, &user)
	}
	return AwsCredentialsStateForUser(home, nil)
}

// userHome is the home directory of the user this process runs as, which a
// relative profile path in the session's AWS configuration is relative to.
func userHome() (string, bool) {
	return os.LookupEnv("HOME")
}

// AwsCredentialsStateForUser is AwsCredentialsState for a user whose home
// directory is userHome.
func AwsCredentialsStateForUser(home string, userHome *string) (CredentialSnapshot, error) {
	panic("unported: mj-core/src/aws_credentials.rs state_for_user")
}

// configurationNames is whether the AWS configuration actual is the one for
// profile and the secret at secret. Launch writes the secret's path as the
// target will see it, which for an SSH worker is relative to the user's
// home, while the worker has resolved its own home to an absolute path: the
// two spellings of one path are equal, so this resolves a relative one
// against userHome and compares the paths, not their text. This is the one
// place the configured path is compared.
func configurationNames(actual []byte, profile, secret string, userHome *string) bool {
	panic("unported: mj-core/src/aws_credentials.rs configuration_names")
}

// configuredSecretPath is the path in a `credential_process` command written
// by AwsCredentials.Configuration: `cat '<absolute path>'`, or for a path
// relative to the user's home, `sh -c 'exec cat "$HOME"/'<path>”`.
func configuredSecretPath(command string) (string, bool) {
	panic("unported: mj-core/src/aws_credentials.rs configured_secret_path")
}

// awsCredentialsSamePath reports two spellings of one path: equal once
// symlinks are resolved where the files exist, and otherwise equal component
// by component. (Rust's aws_credentials::same_path; config_loading.go
// already has a samePath.)
func awsCredentialsSamePath(left, right string) bool {
	panic("unported: mj-core/src/aws_credentials.rs same_path")
}
