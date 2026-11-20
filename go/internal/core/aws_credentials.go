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
	"math"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"unicode"
	"unicode/utf8"

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
	return credentialsValueEqual(reflect.ValueOf(*s), reflect.ValueOf(*other))
}

// String is Rust's Debug form, which leaves the environment's values out.
func (s AwsCredentialSource) String() string {
	names := slices.Sorted(maps.Keys(s.Environment))
	role := "None"
	if s.Role != nil {
		role = fmt.Sprintf("Some(%q)", s.Role.RoleARN)
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

// UnmarshalJSON reads serde's encoding: keys match exactly, Version,
// AccessKeyId and SecretAccessKey are required, a null optional member is
// absent, and a missing Profile is "default".
func (c *AwsCredentials) UnmarshalJSON(data []byte) error {
	value, err := jsonvalue.Parse(data)
	if err != nil {
		return err
	}
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return errors.New("invalid type: expected struct AwsCredentials")
	}
	required := func(name string) (any, error) {
		member, ok := object.Get(name)
		if !ok {
			return nil, fmt.Errorf("missing field `%s`", name)
		}
		return member, nil
	}
	text := func(name string, member any) (string, error) {
		value, ok := member.(string)
		if !ok {
			return "", fmt.Errorf("invalid type for `%s`, expected a string", name)
		}
		return value, nil
	}
	optional := func(name string) (*string, error) {
		member, ok := object.Get(name)
		if !ok || member == nil {
			return nil, nil
		}
		value, err := text(name, member)
		if err != nil {
			return nil, err
		}
		return &value, nil
	}
	versionMember, err := required("Version")
	if err != nil {
		return err
	}
	version, ok := jvUint64(versionMember, true)
	if !ok || version > math.MaxUint32 {
		return errors.New("invalid value for `Version`, expected u32")
	}
	decoded := AwsCredentials{version: uint32(version), profile: awsCredentialsDefaultProfile()}
	for _, field := range []struct {
		name  string
		value *string
	}{{"AccessKeyId", &decoded.accessKeyID}, {"SecretAccessKey", &decoded.secretAccessKey}} {
		member, err := required(field.name)
		if err != nil {
			return err
		}
		if *field.value, err = text(field.name, member); err != nil {
			return err
		}
	}
	if decoded.sessionToken, err = optional("SessionToken"); err != nil {
		return err
	}
	if decoded.expiration, err = optional("Expiration"); err != nil {
		return err
	}
	if member, ok := object.Get("Profile"); ok {
		if decoded.profile, err = text("Profile", member); err != nil {
			return err
		}
	}
	*c = decoded
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
	if len(bytes) > MaxCredentialBytes {
		return AwsCredentials{}, errors.New("AWS assume-role output is too large")
	}
	// Serde's detailed errors can include an offending secret value.
	invalid := errors.New("AWS assume-role output is not the expected JSON")
	output, err := jsonvalue.Parse(bytes)
	if err != nil {
		return AwsCredentials{}, invalid
	}
	issued, ok := jvGet(output, "Credentials")
	if !ok {
		return AwsCredentials{}, invalid
	}
	if _, isObject := issued.(jsonvalue.Object); !isObject {
		return AwsCredentials{}, invalid
	}
	field := func(name string) (string, bool) { return jvString(jvGet(issued, name)) }
	accessKeyID, ok1 := field("AccessKeyId")
	secretAccessKey, ok2 := field("SecretAccessKey")
	sessionToken, ok3 := field("SessionToken")
	expiration, ok4 := field("Expiration")
	if !ok1 || !ok2 || !ok3 || !ok4 {
		return AwsCredentials{}, invalid
	}
	credentials := AwsCredentials{
		version:         1,
		accessKeyID:     accessKeyID,
		secretAccessKey: secretAccessKey,
		sessionToken:    &sessionToken,
		expiration:      &expiration,
		profile:         profile,
	}
	if err := credentials.validate(); err != nil {
		return AwsCredentials{}, err
	}
	return credentials, nil
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
	if c.version != 1 {
		return errors.New("unsupported AWS credential export version")
	}
	if !AwsCredentialsValidProfileName(c.profile) {
		return errors.New("AWS profile name cannot be represented in a shared credentials file")
	}
	validSecret := func(value string) bool {
		return value != "" && !strings.ContainsFunc(value, unicode.IsSpace)
	}
	if !validSecret(c.accessKeyID) || !validSecret(c.secretAccessKey) ||
		(c.sessionToken != nil && !validSecret(*c.sessionToken)) {
		return errors.New("AWS credential export contains missing or invalid credentials")
	}
	if c.expiration != nil {
		expiry, ok := ParseChronoRFC3339(*c.expiration)
		if !ok {
			return errors.New("AWS credential export has an invalid expiration")
		}
		if expiry.UnixMilli() <= EpochMillis() {
			return errors.New("AWS credential export has expired; renew the selected AWS login")
		}
	}
	return nil
}

// Configuration is the AWS configuration naming the credential process. The
// native process provider honors expiration and reloads renewed
// credentials. A static INI entry can be cached forever by an AWS SDK.
func (c AwsCredentials) Configuration(targetHome string) ([]byte, error) {
	if err := c.validate(); err != nil {
		return nil, err
	}
	path := Join(targetHome, AwsCredentialsSharedCredentialsFile)
	quoted := PosixQuote(Text(path))
	var command string
	if IsAbsolute(path) {
		command = "cat " + quoted
	} else {
		// SSH profile roots are relative to the target user's home;
		// credential_process itself runs in the project directory.
		script := `exec cat "$HOME"/` + quoted
		command = "sh -c " + PosixQuote(script)
	}
	return []byte(fmt.Sprintf("[profile %s]\ncredential_process = %s\n", c.profile, command)), nil
}

// Install installs the complete provider, in staging or at its final home.
// The JSON is published last, after the files the SDK needs to resolve it.
func (c AwsCredentials) Install(home, targetHome string) (CredentialSnapshot, error) {
	bytes, err := c.Encode()
	if err != nil {
		return CredentialSnapshot{}, err
	}
	configuration, err := c.Configuration(targetHome)
	if err != nil {
		return CredentialSnapshot{}, err
	}
	if err := AtomicWrite(filepath.Join(home, AwsCredentialsConfigFile), configuration); err != nil {
		return CredentialSnapshot{}, err
	}
	if err := AtomicWrite(filepath.Join(home, AwsCredentialsEmptyCredentialsFile), []byte{}); err != nil {
		return CredentialSnapshot{}, err
	}
	// The scope record goes first, so the secret is never published without
	// the record of what it is; credentials Mjolnir did not issue leave no
	// record, and a stale one is removed.
	if c.scope != nil {
		record, err := awsSerdeJSON(awsStoredScope{Scope: *c.scope, SecretFingerprint: CredentialFingerprint(bytes)})
		if err != nil {
			return CredentialSnapshot{}, fmt.Errorf("encode AWS credential scope: %w", err)
		}
		if err := AtomicWrite(filepath.Join(home, AwsCredentialsScopeFile), record); err != nil {
			return CredentialSnapshot{}, err
		}
	} else if err := removeIfPresent(filepath.Join(home, AwsCredentialsScopeFile)); err != nil {
		return CredentialSnapshot{}, err
	}
	if err := AtomicWrite(filepath.Join(home, AwsCredentialsSharedCredentialsFile), bytes); err != nil {
		return CredentialSnapshot{}, err
	}
	return AwsCredentialsSnapshot(bytes), nil
}

// AwsCredentialsRemove withdraws the session's AWS credentials. The
// credential file is the secret; without it the native process provider
// finds nothing to use, and a later install writes every file again.
func AwsCredentialsRemove(home string) (CredentialSnapshot, error) {
	if err := removeIfPresent(filepath.Join(home, AwsCredentialsSharedCredentialsFile)); err != nil {
		return CredentialSnapshot{}, err
	}
	if err := removeIfPresent(filepath.Join(home, AwsCredentialsScopeFile)); err != nil {
		return CredentialSnapshot{}, err
	}
	return AwsCredentialsState(home)
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
	bytes, err := os.ReadFile(filepath.Join(home, AwsCredentialsConfigFile))
	if errors.Is(err, fs.ErrNotExist) {
		return "", false, nil
	}
	if err == nil && !utf8.Valid(bytes) {
		err = errors.New("stream did not contain valid UTF-8")
	}
	if err != nil {
		return "", false, fmt.Errorf("read session AWS configuration: %w", err)
	}
	for _, line := range awsRustLines(string(bytes)) {
		name, ok := strings.CutPrefix(strings.TrimSpace(line), "[profile ")
		if !ok {
			continue
		}
		if name, ok = strings.CutSuffix(name, "]"); ok && AwsCredentialsValidProfileName(name) {
			return name, true, nil
		}
	}
	return "", false, nil
}

// AwsCredentialsScope is the scope Mjolnir recorded for the credentials in
// home, when the record describes the secret that is there now.
func AwsCredentialsScope(home string) (*AwsCredentialScope, error) {
	record, hasRecord, err := awsReadSessionFile(home, AwsCredentialsScopeFile)
	if err != nil {
		return nil, err
	}
	secret, hasSecret, err := awsReadSessionFile(home, AwsCredentialsSharedCredentialsFile)
	if err != nil {
		return nil, err
	}
	if !hasRecord || !hasSecret {
		return nil, nil
	}
	stored, ok := awsDecodeStoredScope(record)
	if !ok || stored.SecretFingerprint != CredentialFingerprint(secret) {
		return nil, nil
	}
	return &stored.Scope, nil
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
	bytes, err := os.ReadFile(filepath.Join(home, AwsCredentialsSharedCredentialsFile))
	if errors.Is(err, fs.ErrNotExist) {
		return CredentialSnapshotAbsent(), nil
	}
	if err != nil {
		return CredentialSnapshot{}, fmt.Errorf("read session AWS credentials: %w", err)
	}
	// Matching JSON alone does not make the provider usable. Worker upgrades
	// can introduce these launch paths without restaging the profile. Invalid
	// or expired JSON must likewise allow reconciliation to replace it.
	credentials, err := AwsCredentialsDecode(bytes)
	if err != nil {
		return CredentialSnapshotAbsent(), nil
	}
	configuration, ok, err := awsReadSessionFile(home, AwsCredentialsConfigFile)
	if err != nil {
		return CredentialSnapshot{}, err
	}
	if !ok || !configurationNames(configuration, credentials.Profile(), filepath.Join(home, AwsCredentialsSharedCredentialsFile), userHome) {
		return CredentialSnapshotAbsent(), nil
	}
	empty, ok, err := awsReadSessionFile(home, AwsCredentialsEmptyCredentialsFile)
	if err != nil {
		return CredentialSnapshot{}, err
	}
	if !ok || len(empty) != 0 {
		return CredentialSnapshotAbsent(), nil
	}
	return AwsCredentialsSnapshot(bytes), nil
}

// configurationNames is whether the AWS configuration actual is the one for
// profile and the secret at secret. Launch writes the secret's path as the
// target will see it, which for an SSH worker is relative to the user's
// home, while the worker has resolved its own home to an absolute path: the
// two spellings of one path are equal, so this resolves a relative one
// against userHome and compares the paths, not their text. This is the one
// place the configured path is compared.
func configurationNames(actual []byte, profile, secret string, userHome *string) bool {
	if !utf8.Valid(actual) {
		return false
	}
	lines := awsRustLines(string(actual))
	if len(lines) != 2 {
		return false
	}
	if lines[0] != "[profile "+profile+"]" {
		return false
	}
	command, ok := strings.CutPrefix(lines[1], "credential_process = ")
	if !ok {
		return false
	}
	configured, ok := configuredSecretPath(command)
	if !ok {
		return false
	}
	resolved := configured
	if !filepath.IsAbs(configured) {
		if userHome == nil {
			return false
		}
		resolved = filepath.Join(*userHome, configured)
	}
	return awsCredentialsSamePath(resolved, secret)
}

// configuredSecretPath is the path in a `credential_process` command written
// by AwsCredentials.Configuration: `cat '<absolute path>'`, or for a path
// relative to the user's home, `sh -c 'exec cat "$HOME"/'<path>”`.
func configuredSecretPath(command string) (string, bool) {
	unquote := func(text string) (string, bool) {
		text, ok := strings.CutPrefix(text, "'")
		if !ok {
			return "", false
		}
		text, ok = strings.CutSuffix(text, "'")
		if !ok {
			return "", false
		}
		return strings.ReplaceAll(text, `'\''`, "'"), true
	}
	if path, ok := strings.CutPrefix(command, "cat "); ok {
		return unquote(path)
	}
	quotedScript, ok := strings.CutPrefix(command, "sh -c ")
	if !ok {
		return "", false
	}
	script, ok := unquote(quotedScript)
	if !ok {
		return "", false
	}
	path, ok := strings.CutPrefix(script, `exec cat "$HOME"/`)
	if !ok {
		return "", false
	}
	return unquote(path)
}

// awsCredentialsSamePath reports two spellings of one path: equal once
// symlinks are resolved where the files exist, and otherwise equal component
// by component. (Rust's aws_credentials::same_path; config_loading.go
// already has a samePath.)
func awsCredentialsSamePath(left, right string) bool {
	resolved := func(path string) string {
		if canonical, err := credentialsCanonicalize(path); err == nil {
			return canonical
		}
		return awsPathComponents(path)
	}
	return resolved(left) == resolved(right)
}

// awsPathComponents is path rebuilt from its components, as Rust's
// `path.components().collect::<PathBuf>()` does: repeated separators, a
// trailing separator and `.` after the start go, and `..` stays.
func awsPathComponents(path string) string {
	volume := filepath.VolumeName(path)
	rest := path[len(volume):]
	rooted := rest != "" && os.IsPathSeparator(rest[0])
	parts := []string{}
	for index, part := range strings.FieldsFunc(rest, func(c rune) bool { return c < 0x80 && os.IsPathSeparator(byte(c)) }) {
		if part == "." && (index > 0 || rooted) {
			continue
		}
		parts = append(parts, part)
	}
	joined := strings.Join(parts, string(filepath.Separator))
	if rooted {
		joined = string(filepath.Separator) + joined
	}
	return volume + joined
}

// awsRustLines is Rust's str::lines: lines split at `\n`, each without a
// trailing `\r`, and no empty last line for a final newline.
func awsRustLines(text string) []string {
	lines := []string{}
	for text != "" {
		line, rest, found := strings.Cut(text, "\n")
		if found {
			line = strings.TrimSuffix(line, "\r")
		}
		lines = append(lines, line)
		text = rest
	}
	return lines
}

// awsReadSessionFile reads one of the session's AWS files; false when it is
// missing.
func awsReadSessionFile(home, name string) ([]byte, bool, error) {
	bytes, err := os.ReadFile(filepath.Join(home, name))
	if errors.Is(err, fs.ErrNotExist) {
		return nil, false, nil
	}
	if err != nil {
		return nil, false, fmt.Errorf("read session AWS %s: %w", name, err)
	}
	return bytes, true, nil
}

// awsDecodeStoredScope reads a stored scope record as serde does: every
// field but `source_identity` and `tag` is required, with its type.
func awsDecodeStoredScope(record []byte) (awsStoredScope, bool) {
	value, err := jsonvalue.Parse(record)
	if err != nil {
		return awsStoredScope{}, false
	}
	if _, ok := value.(jsonvalue.Object); !ok {
		return awsStoredScope{}, false
	}
	fingerprint, ok := jvString(jvGet(value, "secret_fingerprint"))
	if !ok {
		return awsStoredScope{}, false
	}
	scope, ok := jvGet(value, "scope")
	if !ok {
		return awsStoredScope{}, false
	}
	if _, isObject := scope.(jsonvalue.Object); !isObject {
		return awsStoredScope{}, false
	}
	roleArn, ok1 := jvString(jvGet(scope, "role_arn"))
	roleSessionName, ok2 := jvString(jvGet(scope, "role_session_name"))
	policy, ok3 := jvString(jvGet(scope, "session_policy_fingerprint"))
	expiresAt, ok4 := jvInt64(jvGet(scope, "expires_at_epoch_millis"))
	if !ok1 || !ok2 || !ok3 || !ok4 {
		return awsStoredScope{}, false
	}
	stored := awsStoredScope{SecretFingerprint: fingerprint, Scope: AwsCredentialScope{
		RoleArn:                  roleArn,
		RoleSessionName:          roleSessionName,
		SessionPolicyFingerprint: policy,
		ExpiresAtEpochMillis:     expiresAt,
	}}
	if member, ok := jvGet(scope, "source_identity"); ok && member != nil {
		identity, isString := member.(string)
		if !isString {
			return awsStoredScope{}, false
		}
		stored.Scope.SourceIdentity = &identity
	}
	if member, ok := jvGet(scope, "tag"); ok {
		tag, isString := member.(string)
		if !isString {
			return awsStoredScope{}, false
		}
		stored.Scope.Tag = tag
	}
	return stored, true
}

// awsSerdeJSON encodes value as serde_json writes it: Go's encoding first,
// then rewritten without Go's HTML and line-separator escapes.
func awsSerdeJSON(value any) ([]byte, error) {
	encoded, err := json.Marshal(value)
	if err != nil {
		return nil, err
	}
	parsed, err := jsonvalue.Parse(encoded)
	if err != nil {
		return nil, err
	}
	return jsonvalue.Marshal(parsed)
}
