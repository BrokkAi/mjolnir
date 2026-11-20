// Port of mj-core/src/credentials.rs.

// Harness credential interpretation and convergence.
//
// Hel clones a profile's harness credentials into every session's isolated
// home. Rotating OAuth refresh tokens make those copies diverge: the first
// copy to refresh invalidates the grant every other copy still holds. This
// module owns the one interpretation of credential files Hel has, plus the
// background service that reconciles the controller-side canonical copy with
// each live session's copy in both directions.
//
// The same reconcile loop also pushes each profile's synced skills trees
// (see skills) into live sessions. Skills are not secrets and do not rotate,
// so they converge in one direction only: the canonical home wins.
//
// Credential bytes travel only in worker request and response frames. They
// never enter the durable event stream or a checkpoint archive. Fingerprints
// and freshness timestamps are not secret and may appear in logs.

package core

import (
	"context"
	"crypto/sha256"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"
)

// MaxCredentialBytes bounds a credential file. Credential files are small
// JSON or YAML documents. The cap keeps a hostile or corrupt worker from
// making the controller buffer an arbitrary payload.
const MaxCredentialBytes = 1024 * 1024

// MaxGithubTokenBytes bounds a GitHub token. GitHub tokens are opaque but
// small. Keeping a separate, tight bound avoids treating the live CLI
// credential as a general-purpose secret transport.
const MaxGithubTokenBytes = 4 * 1024

// SyncInterval is how often the coordinator reconciles every profile with
// its live sessions.
const SyncInterval = 60 * time.Second

func CredentialFingerprint(bytes []byte) string {
	sum := sha256.Sum256(bytes)
	return LowerHex(sum[:])
}

// ClaudeOauthLoginPath is the staged OAuth login, excluding setup tokens and
// API-key/Bedrock launches.
func ClaudeOauthLoginPath(environment map[string]string) (string, bool) {
	panic("unported: mj-core/src/credentials.rs claude_oauth_login_path")
}

// ClaudeAccessTokenFingerprint is the identity of the access token itself.
// Stripping a refresh token or rewriting the JSON cannot authorize recovery
// of a prompt that used the same login.
func ClaudeAccessTokenFingerprint(bytes []byte) (string, bool) {
	panic("unported: mj-core/src/credentials.rs claude_access_token_fingerprint")
}

type GithubTokenSnapshot struct {
	Present     bool
	Fingerprint string
}

func GithubTokenSnapshotAbsent() GithubTokenSnapshot {
	return GithubTokenSnapshot{}
}

func GithubTokenSnapshotOf(token string) GithubTokenSnapshot {
	return GithubTokenSnapshot{Present: true, Fingerprint: CredentialFingerprint([]byte(token))}
}

// validateOpaqueToken is the one reading of an opaque single-line secret,
// shared by every token file Hel stores so the limits and the rejections
// cannot drift apart.
func validateOpaqueToken(label string, limit int, bytes []byte) (string, error) {
	if len(bytes) == 0 {
		return "", fmt.Errorf("%s is empty", label)
	}
	if len(bytes) > limit {
		return "", fmt.Errorf("%s is above the %d byte limit", label, limit)
	}
	if !utf8.Valid(bytes) {
		return "", fmt.Errorf("%s is not valid UTF-8", label)
	}
	token := strings.TrimSpace(string(bytes))
	if token == "" || strings.ContainsFunc(token, unicode.IsSpace) {
		return "", fmt.Errorf("%s must be non-empty and contain no whitespace", label)
	}
	return token, nil
}

// refuseSymlinkedToken refuses a token file that is a symbolic link.
// Following one would let whatever created it choose where the secret is
// read from or written to.
func refuseSymlinkedToken(label, path string) error {
	if info, err := os.Lstat(path); err == nil && info.Mode()&fs.ModeSymlink != 0 {
		return fmt.Errorf("%s destination %s is a symbolic link", label, path)
	}
	return nil
}

func ValidateGithubToken(bytes []byte) (string, error) {
	return validateOpaqueToken("GitHub token", MaxGithubTokenBytes, bytes)
}

// ReadGithubToken reads the token at path; a missing file is an absent
// snapshot and a nil token.
func ReadGithubToken(path string) (GithubTokenSnapshot, *string, error) {
	if err := refuseSymlinkedToken("GitHub token", path); err != nil {
		return GithubTokenSnapshot{}, nil, err
	}
	bytes, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return GithubTokenSnapshotAbsent(), nil, nil
	}
	if err != nil {
		return GithubTokenSnapshot{}, nil, fmt.Errorf("read GitHub token %s: %w", path, err)
	}
	token, err := ValidateGithubToken(bytes)
	if err != nil {
		return GithubTokenSnapshot{}, nil, err
	}
	return GithubTokenSnapshotOf(token), &token, nil
}

func WriteGithubToken(path string, bytes []byte) (GithubTokenSnapshot, error) {
	token, err := ValidateGithubToken(bytes)
	if err != nil {
		return GithubTokenSnapshot{}, err
	}
	if err := refuseSymlinkedToken("GitHub token", path); err != nil {
		return GithubTokenSnapshot{}, err
	}
	if err := AtomicWriteExisting(path, []byte(token+"\n")); err != nil {
		return GithubTokenSnapshot{}, err
	}
	return GithubTokenSnapshotOf(token), nil
}

func RemoveGithubToken(path string) error {
	if err := refuseSymlinkedToken("GitHub token", path); err != nil {
		return err
	}
	if err := os.Remove(path); err != nil && !errors.Is(err, fs.ErrNotExist) {
		return fmt.Errorf("remove GitHub token %s: %w", path, err)
	}
	return nil
}

// MaxClaudeOauthTokenBytes bounds a Claude setup token, which is opaque and
// single-line, like the GitHub token.
const MaxClaudeOauthTokenBytes = 4 * 1024

// ClaudeOauthTokenEnv is the variable Claude Code reads a long-lived OAuth
// token from. It takes precedence over the `/login` credentials file and is
// honoured by the Agent SDK, so a worker started with it never touches the
// rotating grant.
const ClaudeOauthTokenEnv = "CLAUDE_CODE_OAUTH_TOKEN"

// ClaudeOauthTokenPath is where a profile's long-lived Claude setup token
// lives on the controller.
//
// It sits beside the configuration rather than inside the profile home, so
// profile staging never copies it into a session home or a container.
func ClaudeOauthTokenPath(profileID string) string {
	return filepath.Join(ConfigDir(), "profiles", profileID, "claude-oauth-token")
}

func ValidateClaudeOauthToken(bytes []byte) (string, error) {
	return validateOpaqueToken("Claude setup token", MaxClaudeOauthTokenBytes, bytes)
}

// ReadClaudeOauthToken is the stored setup token, or false when the profile
// has none.
func ReadClaudeOauthToken(path string) (string, bool, error) {
	if err := refuseSymlinkedToken("Claude setup token", path); err != nil {
		return "", false, err
	}
	bytes, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return "", false, nil
	}
	if err != nil {
		return "", false, fmt.Errorf("read Claude setup token %s: %w", path, err)
	}
	token, err := ValidateClaudeOauthToken(bytes)
	if err != nil {
		return "", false, err
	}
	return token, true, nil
}

func WriteClaudeOauthToken(path string, bytes []byte) error {
	panic("unported: mj-core/src/credentials.rs write_claude_oauth_token")
}

// CredentialFreshness is epoch milliseconds describing how current a
// credential copy is. Higher wins when two copies of the same grant differ.
//
// Key structure confirmed against real files on a developer machine:
//   - Claude `~/.claude/.credentials.json`: `{ "claudeAiOauth": {
//     "accessToken", "refreshToken", "expiresAt", "refreshTokenExpiresAt",
//     "scopes", "subscriptionType", "rateLimitTier" } }`. `expiresAt` is a
//     13-digit number, so already epoch milliseconds.
//   - Codex `~/.codex/auth.json`: `{ "auth_mode", "tokens": {
//     "access_token", "refresh_token", "id_token", "account_id" },
//     "last_refresh" }`. `last_refresh` is an RFC3339 string with fractional
//     seconds and a `Z` suffix.
//   - Kimi `~/.kimi-code/credentials/kimi-code.json`: `{ "access_token",
//     "refresh_token", "expires_at", "expires_in", "scope", "token_type" }`.
//     `expires_at` is a 10-digit number, so epoch seconds.
//   - Grok `~/.grok/auth.json`: an object keyed by `"<issuer>::<uuid>"`, each
//     value holding `{ "key", "refresh_token", "expires_at", ... }`.
//     `expires_at` is an RFC3339 string with nanosecond precision and a `Z`
//     suffix. A file may hold several grants, so the latest expiry wins.
//
// Anything unparseable is unknown rather than a guess.
func CredentialFreshness(kind HarnessKind, bytes []byte) (int64, bool) {
	panic("unported: mj-core/src/credentials.rs credential_freshness")
}

// CredentialExpiry is epoch milliseconds at which the stored access token
// stops working, for the harnesses Hel can refresh ahead of that deadline.
//
// This is not CredentialFreshness. Freshness orders two copies of the same
// grant; expiry says when the grant runs out. Claude states the same number
// for both, but Codex orders copies by `last_refresh` and expires by the
// `exp` claim of the access token in `tokens.access_token`. Grok has no
// proactive-refresh path, so it reports nothing here.
//
// Anything unparseable is unknown rather than a guess.
func CredentialExpiry(kind HarnessKind, bytes []byte) (int64, bool) {
	panic("unported: mj-core/src/credentials.rs credential_expiry")
}

// CopiesOmitRefreshToken is whether the credential copies this host hands to
// a harness process omit the refresh token, so the daemon stays the grant's
// only refresher.
//
// Claude Code refreshes five minutes before `expiresAt`, under a lock that
// covers only its own config directory. Every copy that carries the rotating
// single-use refresh token can therefore spend it, and a refresh inside a
// copy nobody pulls back leaves the host holding a spent token (#1490).
// Without the key, Claude Code never refreshes or writes the file and
// rereads a replaced one by modification time.
//
// The same on every host. Where the host's own login lives is
// HarnessKind.LoginStore's business (on macOS it is the Keychain, which the
// sync reads and the refresher renews); the copies are files either way.
func (kind HarnessKind) CopiesOmitRefreshToken() bool {
	return kind == HarnessKindClaude
}

// CredentialCopyForHarness is the bytes of a Claude credential copy without
// `claudeAiOauth.refreshToken`. The key is removed, never set to `""`, which
// Claude Code reads as "login ended"; an existing `""` is left alone. Callers
// decide whether a copy is stripped (HarnessKind.CopiesOmitRefreshToken).
// Other harnesses, and a document that is not Claude's JSON shape, are
// returned unchanged.
func CredentialCopyForHarness(kind HarnessKind, bytes []byte) []byte {
	panic("unported: mj-core/src/credentials.rs credential_copy_for_harness")
}

// CredentialHoldsRefreshToken is whether a Claude credential carries a
// refresh token its holder could spend. An empty string is the "login ended"
// marker, not a token. Other harnesses report none here.
func CredentialHoldsRefreshToken(kind HarnessKind, bytes []byte) bool {
	panic("unported: mj-core/src/credentials.rs credential_holds_refresh_token")
}

// museLoginExpiryMs reads Muse's Meta login access-token expiry, recorded in
// epoch seconds. A refresh moves it forward, so it both orders copies and
// says when the login runs out. A stored API key has none.
func museLoginExpiryMs(json any) (int64, bool) {
	panic("unported: mj-core/src/credentials.rs muse_login_expiry_ms")
}

// CredentialFreshnessMs reads CredentialFreshness out of this harness's own
// credential JSON (a jsonvalue value).
func (kind HarnessKind) CredentialFreshnessMs(json any) (int64, bool) {
	panic("unported: mj-core/src/credentials.rs HarnessKind::credential_freshness_ms")
}

// CredentialExpiryMs reads CredentialExpiry out of this harness's own
// credential JSON (a jsonvalue value).
func (kind HarnessKind) CredentialExpiryMs(json any) (int64, bool) {
	panic("unported: mj-core/src/credentials.rs HarnessKind::credential_expiry_ms")
}

// NativeLoginCommand is the harness CLI's own interactive login command.
//
// Verified against the locally installed CLIs with `--help`: `codex login`,
// `claude auth login` (there is no bare `claude login`), `kimi login`,
// `grok login`, `muse login`, and `opencode auth login`.
func (kind HarnessKind) NativeLoginCommand() (string, []string) {
	panic("unported: mj-core/src/credentials.rs HarnessKind::native_login_command")
}

// jwtExpiryMillis is the `exp` claim of a JWT, in epoch milliseconds.
//
// Only the payload is read, and only for its expiry. The signature is the
// issuer's business; Hel never accepts the token on anyone's behalf, so there
// is nothing here for a forged claim to unlock beyond an early refresh.
func jwtExpiryMillis(token string) (int64, bool) {
	panic("unported: mj-core/src/credentials.rs jwt_expiry_millis")
}

// credentialRFC3339Millis parses an RFC 3339 timestamp as chrono's
// parse_from_rfc3339 does, in epoch milliseconds.
func credentialRFC3339Millis(text string) (int64, bool) {
	panic("unported: mj-core/src/credentials.rs chrono::DateTime::parse_from_rfc3339")
}

// ValidateCredentialPayload rejects anything that is not a plausible
// credential document before it replaces a canonical file or lands in a
// session home.
func ValidateCredentialPayload(kind HarnessKind, bytes []byte) error {
	if len(bytes) == 0 {
		return errors.New("credential payload is empty")
	}
	if len(bytes) > MaxCredentialBytes {
		return fmt.Errorf("credential payload is %d bytes, above the %d byte limit", len(bytes), MaxCredentialBytes)
	}
	if !utf8.Valid(bytes) {
		return errors.New("credential payload is not valid UTF-8")
	}
	if !json.Valid(bytes) {
		return errors.New("credential payload is not JSON")
	}
	return nil
}

// LoginStatus is how a profile's harness login stands. It serializes as
// serde's externally tagged enum: `"SignedIn"`, or `{"NotRequired":"..."}`.
type LoginStatus interface {
	isLoginStatus()
	// Label is short text for a settings row, without a trailing period.
	Label() string
}

// LoginStatusSignedIn: a credential the harness can start from is present.
type LoginStatusSignedIn struct{}

// LoginStatusExpired: a credential is present but its recorded expiry has
// passed. The harness may still refresh it on the next turn.
type LoginStatusExpired struct{}

// LoginStatusSignedOut: this profile uses a harness login and no credential
// is present.
type LoginStatusSignedOut struct{}

// LoginStatusNotRequired: this profile authenticates without an interactive
// harness login.
type LoginStatusNotRequired struct{ Detail string }

// LoginStatusUnknown: the status could not be read, or the harness keeps it
// somewhere this process cannot inspect.
type LoginStatusUnknown struct{ Detail string }

func (LoginStatusSignedIn) isLoginStatus()    {}
func (LoginStatusExpired) isLoginStatus()     {}
func (LoginStatusSignedOut) isLoginStatus()   {}
func (LoginStatusNotRequired) isLoginStatus() {}
func (LoginStatusUnknown) isLoginStatus()     {}

func (LoginStatusSignedIn) Label() string      { return "signed in" }
func (LoginStatusExpired) Label() string       { return "credential expired" }
func (LoginStatusSignedOut) Label() string     { return "signed out" }
func (s LoginStatusNotRequired) Label() string { return s.Detail }
func (s LoginStatusUnknown) Label() string     { return s.Detail }

func (LoginStatusSignedIn) MarshalJSON() ([]byte, error)  { return []byte(`"SignedIn"`), nil }
func (LoginStatusExpired) MarshalJSON() ([]byte, error)   { return []byte(`"Expired"`), nil }
func (LoginStatusSignedOut) MarshalJSON() ([]byte, error) { return []byte(`"SignedOut"`), nil }

func (s LoginStatusNotRequired) MarshalJSON() ([]byte, error) {
	return json.Marshal(map[string]string{"NotRequired": s.Detail})
}

func (s LoginStatusUnknown) MarshalJSON() ([]byte, error) {
	return json.Marshal(map[string]string{"Unknown": s.Detail})
}

// UnmarshalLoginStatus decodes serde's externally tagged LoginStatus.
func UnmarshalLoginStatus(data []byte) (LoginStatus, error) {
	panic("unported: mj-core/src/credentials.rs LoginStatus deserialize")
}

type CredentialSnapshot struct {
	Present          bool
	Fingerprint      string
	FreshnessEpochMs *int64
}

func CredentialSnapshotAbsent() CredentialSnapshot {
	return CredentialSnapshot{}
}

func CredentialSnapshotOf(kind HarnessKind, bytes []byte) CredentialSnapshot {
	snapshot := CredentialSnapshot{Present: true, Fingerprint: CredentialFingerprint(bytes)}
	if freshness, ok := CredentialFreshness(kind, bytes); ok {
		snapshot.FreshnessEpochMs = &freshness
	}
	return snapshot
}

// Equal compares snapshots by value, as Rust's PartialEq does.
func (s CredentialSnapshot) Equal(other CredentialSnapshot) bool {
	if (s.FreshnessEpochMs == nil) != (other.FreshnessEpochMs == nil) {
		return false
	}
	if s.FreshnessEpochMs != nil && *s.FreshnessEpochMs != *other.FreshnessEpochMs {
		return false
	}
	return s.Present == other.Present && s.Fingerprint == other.Fingerprint
}

type SyncAction int

const (
	// SyncActionPush installs the canonical copy into the session.
	SyncActionPush SyncAction = iota
	// SyncActionPull adopts the session's copy as canonical.
	SyncActionPull
	// SyncActionNone: the copies already agree, or nothing can be decided.
	SyncActionNone
)

// Reconcile decides which way a profile's canonical copy and one session's
// copy converge.
//
// When both sides are present, differ, and neither reports freshness, Hel
// refuses to guess: file modification times are not a reliable proxy for
// grant age across a container boundary.
func Reconcile(canonical, session *CredentialSnapshot) SyncAction {
	switch {
	case !canonical.Present && !session.Present:
		return SyncActionNone
	case canonical.Present && !session.Present:
		return SyncActionPush
	case !canonical.Present && session.Present:
		return SyncActionPull
	case canonical.Fingerprint == session.Fingerprint:
		return SyncActionNone
	}
	switch c, s := canonical.FreshnessEpochMs, session.FreshnessEpochMs; {
	case c != nil && s != nil && *c > *s:
		return SyncActionPush
	case c != nil && s != nil && *s > *c:
		return SyncActionPull
	case c != nil && s == nil:
		return SyncActionPush
	case c == nil && s != nil:
		return SyncActionPull
	}
	return SyncActionNone
}

// ReadCredentialFile reads a credential file, treating "not there" as a
// snapshot rather than an error. A directory or unreadable file is an error
// worth surfacing.
func ReadCredentialFile(kind HarnessKind, path string) (CredentialSnapshot, []byte, error) {
	bytes, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return CredentialSnapshotAbsent(), []byte{}, nil
	}
	if err != nil {
		return CredentialSnapshot{}, nil, fmt.Errorf("read credentials %s: %w", path, err)
	}
	return CredentialSnapshotOf(kind, bytes), bytes, nil
}

// WriteCredentialFile replaces a credential file without exposing a partial
// write or widening its permissions. Refuses a symlinked destination so a
// compromised session home cannot redirect the write.
func WriteCredentialFile(kind HarnessKind, path string, bytes []byte) error {
	panic("unported: mj-core/src/credentials.rs write_credential_file")
}

// authFailurePhrases are full-phrase markers that a harness rejected the
// session's credentials. Kept tight on purpose: a false positive costs one
// redundant sync and one notice, but a noisy list would train operators to
// ignore both.
var authFailurePhrases = [9]string{
	"oauth session expired and could not be refreshed",
	"please run /login",
	"authorization grant is invalid",
	// Codex, when its refresh token is rejected (R14-1): "Your access token
	// could not be refreshed. Please log out and sign in again."
	"access token could not be refreshed",
	"log out and sign in again",
	// OpenAI, when it refuses an API key (#1160): "401 Unauthorized:
	// Incorrect API key provided: sk-...".
	"incorrect api key provided",
	// Kimi, when its stored OAuth token is refused (#1132): "Stored token for
	// \"kimi-code\" was rejected; re-login required." Its error name,
	// `OAuthUnauthorizedError`, is an identifier below.
	"re-login required",
	// Hel's own marker for a turn the bridge failed with ACP `auth_required`.
	// The bridge's wording ("Authentication required") is too generic to
	// match.
	"acp auth_required",
	// Codex ACP can refuse workspace routing while loading a parked session.
	"workspace routing discovery unauthorized (401)",
}

// authFailureIdentifiers are machine-readable authentication errors, which
// must match as complete identifiers. This avoids treating paths such as
// `authentication_error_status_code` as auth failures while still
// recognizing the codes in JSON and diagnostics.
var authFailureIdentifiers = [3]string{
	"authentication_error",
	"invalid_grant",
	"oauthunauthorizederror",
}

func containsASCIIIdentifier(text, identifier string) bool {
	isIdentifierByte := func(b byte) bool {
		return b >= '0' && b <= '9' || b >= 'a' && b <= 'z' || b >= 'A' && b <= 'Z' || b == '_'
	}
	for offset := 0; offset+len(identifier) <= len(text); {
		found := strings.Index(text[offset:], identifier)
		if found < 0 {
			return false
		}
		start := offset + found
		end := start + len(identifier)
		if (start == 0 || !isIdentifierByte(text[start-1])) && (end == len(text) || !isIdentifierByte(text[end])) {
			return true
		}
		offset = end
	}
	return false
}

func containsAuthFailureSignature(text string) bool {
	panic("unported: mj-core/src/credentials.rs contains_auth_failure_signature")
}

func AuthFailureSignature(kind HarnessKind, text string) bool {
	return containsAuthFailureSignature(text)
}

// TextReportsAuthFailure is AuthFailureSignature for text whose harness is
// not known, such as a failed turn's recorded reason.
func TextReportsAuthFailure(text string) bool {
	return containsAuthFailureSignature(text)
}

// authFailureErrorKinds are error kinds a harness names beside a failed turn
// when the provider rejected its login. Codex sends `codexErrorInfo:
// "unauthorized"` (R14-1), and a turn's diagnostic keeps that kind as its
// code. Matched against the code only, never against free text, where "401
// Unauthorized" is common.
var authFailureErrorKinds = [1]string{AuthFailureKind}

// TurnDiagnosticReportsAuthFailure is whether a failed turn's diagnostic
// says the provider rejected the session's login: by its error kind, or by
// an auth failure phrase in its message. A usage limit is never one, as in
// the worker's warning label.
func TurnDiagnosticReportsAuthFailure(diagnostic *TurnDiagnostic) bool {
	panic("unported: mj-core/src/credentials.rs turn_diagnostic_reports_auth_failure")
}

type CredentialSyncReason string

const (
	CredentialSyncReasonAuthenticationFailure CredentialSyncReason = "authentication_failure"
	CredentialSyncReasonEmptyPromptResponse   CredentialSyncReason = "empty_prompt_response"
)

// UnmarshalJSON accepts only the reasons serde accepts.
func (r *CredentialSyncReason) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return err
	}
	switch reason := CredentialSyncReason(text); reason {
	case CredentialSyncReasonAuthenticationFailure, CredentialSyncReasonEmptyPromptResponse:
		*r = reason
		return nil
	}
	return fmt.Errorf("unknown variant `%s`, expected `authentication_failure` or `empty_prompt_response`", text)
}

// CredentialSyncSignal decodes with serde's deny_unknown_fields.
type CredentialSyncSignal struct {
	Ordinal uint64               `json:"ordinal"`
	Reason  CredentialSyncReason `json:"reason"`
	// LoginFingerprint is the hash of the access token the failed turn used
	// (the turn diagnostic's `login_fingerprint`). nil when the failure
	// carries none: a warning, a worker that predates the field, or a harness
	// without a token hash.
	//
	// Never serialized: an older client's copy of this struct denies unknown
	// fields and would fail to decode the session view. The daemon passes it
	// in-process only.
	LoginFingerprint *string `json:"-"`
}

// UnmarshalJSON refuses unknown fields, as serde's deny_unknown_fields does.
func (s *CredentialSyncSignal) UnmarshalJSON(data []byte) error {
	panic("unported: mj-core/src/credentials.rs CredentialSyncSignal deserialize")
}

type CredentialSyncCause struct {
	SessionID string
	Ordinal   uint64
	Reason    CredentialSyncReason
	// LoginFingerprint is the failed turn's access-token hash, as in
	// CredentialSyncSignal.
	LoginFingerprint *string
}

// CredentialSyncCauseForFailedTurn is the cause a turn that failed on its
// login gives, or nil.
func CredentialSyncCauseForFailedTurn(sessionID string, turn *MaterializedTurnOutcome) *CredentialSyncCause {
	panic("unported: mj-core/src/credentials.rs CredentialSyncCause::for_failed_turn")
}

// ReadLoginUnderNativeLock runs read while holding the native lock every
// writer of the profile's login takes (the renewer holds it for its whole
// exchange with the provider), so a renewal in progress finishes first and
// read sees the login it wrote. False when the lock could not be taken
// within lockWait: the answer is unknown, never a reading of a login that is
// about to change.
func ReadLoginUnderNativeLock[T any](ctx context.Context, kind HarnessKind, store *HarnessLoginStore, lockWait time.Duration, read func() T) (T, bool) {
	panic("unported: mj-core/src/credentials.rs read_login_under_native_lock")
}

// AuthenticationVerdict is what an authentication failure means, decided
// once against the profile's current login (#1547). Every consumer (the
// push, the refusal, the notice) reads this answer instead of comparing
// logins on its own snapshot.
type AuthenticationVerdict int

const (
	// AuthenticationVerdictLoginRenewed: the failed token is no longer the
	// profile's: the login was renewed under the session. The current login
	// is pushed and the turn retries.
	AuthenticationVerdictLoginRenewed AuthenticationVerdict = iota
	// AuthenticationVerdictCurrentLoginRefused: the failed token is still
	// the profile's current one: the provider refused the current login.
	AuthenticationVerdictCurrentLoginRefused
	// AuthenticationVerdictUnknown: either side's token identity is missing.
	// Never grounds for a refusal.
	AuthenticationVerdictUnknown
)

// ClassifyAuthenticationFailure answers: is the access token that failed
// still the profile's current access token?
func ClassifyAuthenticationFailure(failedToken, currentToken *string) AuthenticationVerdict {
	switch {
	case failedToken == nil || currentToken == nil:
		return AuthenticationVerdictUnknown
	case *failedToken == *currentToken:
		return AuthenticationVerdictCurrentLoginRefused
	default:
		return AuthenticationVerdictLoginRenewed
	}
}

// RelayEventLoginFingerprint is the failed turn's access-token hash, when
// this event is a failed prompt.
func RelayEventLoginFingerprint(event *RelayEvent) (string, bool) {
	panic("unported: mj-core/src/credentials.rs relay_event_login_fingerprint")
}

// RelayEventCredentialSyncReason detects a reason for immediate credential
// reconciliation only in relay observations originating from the harness:
// its warnings, its agent text, and the diagnostic of a turn it failed.
// Durable prompt commands are deliberately excluded, so user text cannot
// trigger a sync.
//
// The failed turn counts on its own because the worker leaves out a warning
// that only repeats the harness's last agent message (R14-1).
func RelayEventCredentialSyncReason(event *RelayEvent) (CredentialSyncReason, bool) {
	panic("unported: mj-core/src/credentials.rs relay_event_credential_sync_reason")
}

func EventsReportAuthFailure(kind HarnessKind, events []RelayEvent) bool {
	for index := range events {
		if reason, ok := RelayEventCredentialSyncReason(&events[index]); ok &&
			reason == CredentialSyncReasonAuthenticationFailure {
			return true
		}
	}
	return false
}

// LoginCommand builds the harness's own interactive login command for a
// profile.
//
// Verified against the locally installed CLIs with `--help`: `codex login`,
// `claude auth login` (there is no bare `claude login`), `kimi login`,
// `grok login`, and `muse login`.
func LoginCommand(profile *HarnessProfile) (string, []string, error) {
	panic("unported: mj-core/src/credentials.rs login_command")
}

// NativeLoginCommand is the harness CLI's own login command, ignoring how
// the profile actually authenticates.
//
// Setup discovery uses this to find an installed CLI before any profile
// exists, where "this profile needs no login" is not a useful answer: it
// only wants the program name to run `--version` against.
func NativeLoginCommand(profile *HarnessProfile) (string, []string) {
	panic("unported: mj-core/src/credentials.rs native_login_command")
}

// CredentialSyncTarget is one live session the coordinator may reconcile
// with its profile.
type CredentialSyncTarget struct {
	SessionID string
	ProfileID string
	Harness   HarnessKind
	// ProfileHome is the controller-side canonical home for the profile.
	ProfileHome string
	// SkipsLoginFileSync is true when the profile has no harness login file
	// to synchronize. Such a profile has no credential file to exchange with
	// the session, so only skills and the GitHub token are reconciled.
	SkipsLoginFileSync bool
	// AwsCredentials is the host AWS chain provisioned into this isolated
	// session, when selected.
	AwsCredentials *AwsCredentialSource
	// SyncGithubToken: local sessions use their host login unless a session
	// credential is configured.
	SyncGithubToken bool
	// Github is the controller-owned source for this session's GitHub
	// credential. Included in target identity so a reload invalidates
	// obsolete reconciliation.
	Github GithubConfig
	// SkillsScope is the target-derived availability of the host CLI skill.
	SkillsScope SkillsScope
	// Spec is the reconnect command for the session's worker proxy.
	Spec CommandSpec
}

// Equal compares targets by value, as Rust's PartialEq does.
func (t *CredentialSyncTarget) Equal(other *CredentialSyncTarget) bool {
	panic("unported: mj-core/src/credentials.rs CredentialSyncTarget PartialEq")
}

type CredentialSyncAction int

const (
	// CredentialSyncActionPushed: the canonical copy replaced the session's
	// copy.
	CredentialSyncActionPushed CredentialSyncAction = iota
	// CredentialSyncActionPulled: the session's fresher copy became
	// canonical.
	CredentialSyncActionPulled
	// CredentialSyncActionSkillsPushed: the canonical skills trees replaced
	// the session's trees. Skills sync is push-only: the controller-side
	// profile home stays authoritative.
	CredentialSyncActionSkillsPushed
	// CredentialSyncActionGithubTokenPushed: the controller's current GitHub
	// token replaced the session copy.
	CredentialSyncActionGithubTokenPushed
	// CredentialSyncActionGithubTokenRemoved: a stale session token was
	// removed because the controller has none.
	CredentialSyncActionGithubTokenRemoved
	// CredentialSyncActionAwsCredentialsPushed: the controller's AWS
	// credentials replaced the session's. This is not a harness login: a
	// worker holding a rejected login has nothing to retry with after it.
	CredentialSyncActionAwsCredentialsPushed
	// CredentialSyncActionAwsCredentialsRemoved: the session's AWS
	// credentials were withdrawn, because the profile requires role-scoped
	// credentials and none could be issued.
	CredentialSyncActionAwsCredentialsRemoved
	// CredentialSyncActionSlopcopLoginPushed: the host's slopcop login
	// replaced the session copy.
	CredentialSyncActionSlopcopLoginPushed
	// CredentialSyncActionSlopcopLoginRemoved: the session copy was removed
	// because the host is signed out.
	CredentialSyncActionSlopcopLoginRemoved
)

// SyncStep is one of the independent steps of one session's sync. Each
// fails on its own, and a result can say that a step is fine again only if
// it checked that step. Steps are ordered as declared.
type SyncStep int

const (
	// SyncStepSession is not a step: the session's whole sync failed (its
	// connection, say) before any step ran. Reported without a step.
	SyncStepSession SyncStep = iota
	// SyncStepAws is the role-scoped AWS credentials, renewed by the
	// session's own task.
	SyncStepAws
	// SyncStepAwsExport is the profile's one AWS export, reconciled with its
	// login pass.
	SyncStepAwsExport
	SyncStepHarnessLogin
	SyncStepSkills
	SyncStepGithubToken
	SyncStepSlopcop
)

// StepFailure is one step that failed while the others ran.
type StepFailure struct {
	Step SyncStep
	// Detail is what failed, beginning with the step's name.
	Detail string
}

type CredentialSyncOutcome struct {
	SessionID string
	// CurrentAccessToken is the hash of the access token in the profile's
	// login as this sync read it (Claude only). With the failed turn's hash
	// it decides what an authentication failure means; see
	// ClassifyAuthenticationFailure.
	CurrentAccessToken *string
	// UnchangedLogin is the snapshot that initially matched the failing
	// session without any credential mutation. Refusal evidence must refer to
	// this observation, never to a later reading of the profile's login.
	UnchangedLogin *CredentialSnapshot
	// Actions and Failure are Rust's `outcome: Result<Vec<_>, String>`:
	// every action taken for the session, or (Failure set) why the reconcile
	// failed. An empty action list means the copies already agreed.
	Actions []CredentialSyncAction
	Failure *string
	// StepFailures are steps of the sync that failed while the others ran,
	// such as the AWS credentials. Actions then still lists what the other
	// steps did, so their work is counted and reported.
	StepFailures []StepFailure
}

// FailureText is the step failures as one line, for a log or a test.
func (o *CredentialSyncOutcome) FailureText() (string, bool) {
	if len(o.StepFailures) == 0 {
		return "", false
	}
	details := make([]string, len(o.StepFailures))
	for index, failure := range o.StepFailures {
		details[index] = failure.Detail
	}
	return strings.Join(details, "; "), true
}

// CredentialSyncResult is reported to the UI loops only when something
// happened: an action was taken, a session failed, or an on-demand sync
// finished with nothing to do.
type CredentialSyncResult struct {
	ProfileID string
	// Trigger is the session event that asked for an immediate sync, when
	// any.
	Trigger *CredentialSyncCause
	// Failure: the whole reconcile stopped before it could report
	// per-session outcomes. Kept separate so the failure is reported, never
	// dropped.
	Failure *string
	// Partial: the result covers only the sessions in Outcomes (one session's
	// AWS renewal), not the whole profile: a failure it does not mention is
	// not thereby over.
	Partial  bool
	Outcomes []CredentialSyncOutcome
}

// CredentialSyncFailure is one failure CredentialSyncResult.Failures lists.
type CredentialSyncFailure struct {
	SessionID string
	// Step is nil for a session whose whole sync failed.
	Step   *SyncStep
	Detail string
}

// AuthenticationVerdict is what the triggering authentication failure
// meant, from the profile's login as this sync read it. False when the sync
// was not triggered by an authentication failure or did not reach the
// triggering session.
func (r *CredentialSyncResult) AuthenticationVerdict() (AuthenticationVerdict, bool) {
	panic("unported: mj-core/src/credentials.rs CredentialSyncResult::authentication_verdict")
}

func (r *CredentialSyncResult) PushedTo(sessionID string) bool {
	for _, outcome := range r.Outcomes {
		if outcome.SessionID == sessionID && outcome.Failure == nil &&
			slices.Contains(outcome.Actions, CredentialSyncActionPushed) {
			return true
		}
	}
	return false
}

// Failures is every failure, with the step it belongs to (nil for a session
// whose whole sync failed).
func (r *CredentialSyncResult) Failures() []CredentialSyncFailure {
	panic("unported: mj-core/src/credentials.rs CredentialSyncResult::failures")
}

// Actions is the number of sessions that took at least one action of any
// kind.
func (r *CredentialSyncResult) Actions() int {
	count := 0
	for _, outcome := range r.Outcomes {
		if outcome.Failure == nil && len(outcome.Actions) > 0 {
			count++
		}
	}
	return count
}

// CredentialSessions is the number of sessions whose harness credentials
// were pushed or pulled.
func (r *CredentialSyncResult) CredentialSessions() int {
	return r.countActions(func(action CredentialSyncAction) bool {
		return action == CredentialSyncActionPushed || action == CredentialSyncActionPulled
	})
}

// SkillsSessions is the number of sessions whose skills trees were replaced.
func (r *CredentialSyncResult) SkillsSessions() int {
	return r.countActions(func(action CredentialSyncAction) bool {
		return action == CredentialSyncActionSkillsPushed
	})
}

func (r *CredentialSyncResult) GithubTokenPushedSessions() int {
	return r.countActions(func(action CredentialSyncAction) bool {
		return action == CredentialSyncActionGithubTokenPushed
	})
}

func (r *CredentialSyncResult) GithubTokenRemovedSessions() int {
	return r.countActions(func(action CredentialSyncAction) bool {
		return action == CredentialSyncActionGithubTokenRemoved
	})
}

func (r *CredentialSyncResult) SlopcopLoginPushedSessions() int {
	return r.countActions(func(action CredentialSyncAction) bool {
		return action == CredentialSyncActionSlopcopLoginPushed
	})
}

func (r *CredentialSyncResult) SlopcopLoginRemovedSessions() int {
	return r.countActions(func(action CredentialSyncAction) bool {
		return action == CredentialSyncActionSlopcopLoginRemoved
	})
}

func (r *CredentialSyncResult) countActions(wanted func(CredentialSyncAction) bool) int {
	count := 0
	for _, outcome := range r.Outcomes {
		if outcome.Failure == nil && slices.ContainsFunc(outcome.Actions, wanted) {
			count++
		}
	}
	return count
}

// RejectedLogins is the profiles whose own stored login the provider has
// refused.
//
// The credential sync is what learns it: a session reported an auth failure,
// the sync reached that session, and the profile's copy was no fresher than
// the session's, so there was nothing to push that could help.
//
// A Claude refusal is recorded for exactly one access token: the failed
// turn's token, and only when it is still the profile's current token
// (ClassifyAuthenticationFailure). It ends when the profile's token differs,
// so a renewal, `mj login`, or a fresher copy pulled from another session
// clears it, and a failure of an older token never records one. Other
// harnesses report no token identity, so their refusal still holds while the
// whole login file is unchanged.
//
// The zero value is empty and ready to use.
type RejectedLogins struct {
	profiles   map[string]rejectedLoginProfile
	failures   map[string]map[uint64]*loginFailure
	deliveries map[string]*loginDelivery
	heldLogins map[string]workerView
}

// rejectedLoginProfile is the login a profile's refusal was recorded for:
// Rust's (HarnessKind, PathBuf, LoginFingerprint).
type rejectedLoginProfile struct {
	kind     HarnessKind
	marker   string
	identity LoginFingerprint
}

// LoginFingerprint is the identity of a login. It serializes as serde's
// snake_case externally tagged enum: `{"access_token":"..."}` or
// `{"file":"..."}`. Its variants are comparable, so two LoginFingerprint
// values compare with ==.
type LoginFingerprint interface {
	isLoginFingerprint()
}

type LoginFingerprintAccessToken struct{ Value string }

type LoginFingerprintFile struct{ Value string }

func (LoginFingerprintAccessToken) isLoginFingerprint() {}
func (LoginFingerprintFile) isLoginFingerprint()        {}

func (f LoginFingerprintAccessToken) MarshalJSON() ([]byte, error) {
	return json.Marshal(map[string]string{"access_token": f.Value})
}

func (f LoginFingerprintFile) MarshalJSON() ([]byte, error) {
	return json.Marshal(map[string]string{"file": f.Value})
}

// UnmarshalLoginFingerprint decodes serde's externally tagged
// LoginFingerprint.
func UnmarshalLoginFingerprint(data []byte) (LoginFingerprint, error) {
	panic("unported: mj-core/src/credentials.rs LoginFingerprint deserialize")
}

// LoginFingerprintReadFile is the identity of the login at marker, or nil
// when there is none.
func LoginFingerprintReadFile(kind HarnessKind, marker string) (LoginFingerprint, error) {
	panic("unported: mj-core/src/credentials.rs LoginFingerprint::read_file")
}

// LoginFailureVerdict is the one verdict on a failed turn. Its variants are
// comparable, so two verdicts compare with ==.
type LoginFailureVerdict interface {
	isLoginFailureVerdict()
}

type LoginFailureVerdictDeciding struct{}

type LoginFailureVerdictRefused struct{}

type LoginFailureVerdictRenewed struct{}

// LoginFailureVerdictRetryOwed: the login was renewed and the worker still
// owes the retry, and the new login is arriving or already held: its next
// prompt ends this failure.
type LoginFailureVerdictRetryOwed struct{}

type LoginFailureVerdictUnrefused struct{ Reason string }

func (LoginFailureVerdictDeciding) isLoginFailureVerdict()  {}
func (LoginFailureVerdictRefused) isLoginFailureVerdict()   {}
func (LoginFailureVerdictRenewed) isLoginFailureVerdict()   {}
func (LoginFailureVerdictRetryOwed) isLoginFailureVerdict() {}
func (LoginFailureVerdictUnrefused) isLoginFailureVerdict() {}

type loginFailure struct {
	profileID   string
	fingerprint *string
	// conclusion is nil until a sync concludes this failure.
	conclusion loginConclusion
}

type loginConclusion interface {
	verdict() LoginFailureVerdict
}

type loginConclusionRefused struct{ identity LoginFingerprint }

type loginConclusionDelivered struct{ renewed bool }

type loginConclusionRenewed struct{}

type loginConclusionUnrefused struct{ reason string }

func (loginConclusionRefused) verdict() LoginFailureVerdict { return LoginFailureVerdictRefused{} }

func (c loginConclusionDelivered) verdict() LoginFailureVerdict {
	if c.renewed {
		return LoginFailureVerdictRenewed{}
	}
	return LoginFailureVerdictUnrefused{Reason: "credential reconciliation refreshed this session's login"}
}

func (loginConclusionRenewed) verdict() LoginFailureVerdict { return LoginFailureVerdictRenewed{} }

func (c loginConclusionUnrefused) verdict() LoginFailureVerdict {
	return LoginFailureVerdictUnrefused{Reason: c.reason}
}

func (f *loginFailure) matches(profileID string, cause *CredentialSyncCause) bool {
	if f.profileID != profileID || (f.fingerprint == nil) != (cause.LoginFingerprint == nil) {
		return false
	}
	return f.fingerprint == nil || *f.fingerprint == *cause.LoginFingerprint
}

// LoginDeliveryState is the current profile login's delivery to a live
// session. Its variants are comparable.
type LoginDeliveryState interface {
	isLoginDeliveryState()
}

type LoginDeliveryStatePending struct{}

type LoginDeliveryStateConcluded struct{ Reason string }

func (LoginDeliveryStatePending) isLoginDeliveryState()   {}
func (LoginDeliveryStateConcluded) isLoginDeliveryState() {}

func loginDeliveryStateWorkerHoldsCurrentLogin() LoginDeliveryState {
	return LoginDeliveryStateConcluded{Reason: "worker reports holding the current login"}
}

// WorkerPublication is what a connected worker's latest publication carried.
// A worker that never reports a login (older than protocol 69, or its
// credential read failed) still publishes, with Held unknown.
type WorkerPublication struct {
	Held *string
	// RetryIntent is the rejected login the worker still owes a retry for.
	RetryIntent *string
	// ObservedThrough is the relay ordinal the published view had applied.
	ObservedThrough uint64
}

func NewWorkerPublication(held, retryIntent *string, observedThrough uint64) WorkerPublication {
	return WorkerPublication{Held: held, RetryIntent: retryIntent, ObservedThrough: observedThrough}
}

// workerView is a registry entry, which exists exactly while the worker is
// connected and has published since it connected; "nothing published yet"
// is the absent entry, and "published without a login" is held nil. The two
// never share a representation.
type workerView struct {
	profileID       string
	held            *string
	retryIntent     *string
	observedThrough uint64
}

type loginDelivery struct {
	target      CredentialSyncTarget
	fingerprint string
	state       LoginDeliveryState
	// mismatch: a worker that read back a different login already tried this
	// delivery. The same requested login is not reinstalled unless the held
	// copy changes.
	mismatch *CredentialSnapshot
}

// DeliveryState is the delivery state of the session's login, or nil.
func (r *RejectedLogins) DeliveryState(profileID, sessionID string) LoginDeliveryState {
	delivery, ok := r.deliveries[sessionID]
	if !ok || delivery.target.ProfileID != profileID {
		return nil
	}
	return delivery.state
}

// HeldLoginFingerprint is the worker's latest acknowledged login, from the
// same registry that decides if delivery is pending.
func (r *RejectedLogins) HeldLoginFingerprint(profileID, sessionID string) (string, bool) {
	held, ok := r.heldLogins[sessionID]
	if !ok || held.profileID != profileID || held.held == nil {
		return "", false
	}
	return *held.held, true
}

func (r *RejectedLogins) StartDelivery(target *CredentialSyncTarget, fingerprint string) {
	panic("unported: mj-core/src/credentials.rs RejectedLogins::start_delivery")
}

func (r *RejectedLogins) RecordDelivery(target *CredentialSyncTarget, fingerprint string, state LoginDeliveryState, mismatch *CredentialSnapshot) {
	if r.deliveries == nil {
		r.deliveries = map[string]*loginDelivery{}
	}
	r.deliveries[target.SessionID] = &loginDelivery{
		target: *target, fingerprint: fingerprint, state: state, mismatch: mismatch,
	}
}

func (r *RejectedLogins) DeliveryMismatch(target *CredentialSyncTarget, fingerprint string, held *CredentialSnapshot) bool {
	delivery, ok := r.deliveries[target.SessionID]
	return ok && delivery.target.Equal(target) && delivery.fingerprint == fingerprint &&
		delivery.mismatch != nil && delivery.mismatch.Equal(*held)
}

// ObserveWorker publishes and acknowledges under the same registry lock that
// registers deliveries. A report that arrives first is evidence for the next
// one. A nil publication means the worker disconnected or the actor retired.
//
// The worker's retry intent and the ordinal its view had applied arrive with
// the held login, from the one published view, so the hold's inputs (held
// login, retry intent, delivery state) share this registry's lock.
func (r *RejectedLogins) ObserveWorker(profileID, sessionID string, publication *WorkerPublication) {
	panic("unported: mj-core/src/credentials.rs RejectedLogins::observe_worker")
}

func (r *RejectedLogins) RetainHeldLogins(keep func(string) bool) {
	for id := range r.heldLogins {
		if !keep(id) {
			delete(r.heldLogins, id)
		}
	}
}

func (r *RejectedLogins) RetainDeliveries(exists func(string) bool) {
	for id := range r.deliveries {
		if !exists(id) {
			delete(r.deliveries, id)
		}
	}
	r.RetainHeldLogins(exists)
}

func (r *RejectedLogins) EndDelivery(target *CredentialSyncTarget, fingerprint *string, reason string) {
	delivery, ok := r.deliveries[target.SessionID]
	if ok && delivery.target.Equal(target) && (fingerprint == nil || delivery.fingerprint == *fingerprint) {
		delivery.state = LoginDeliveryStateConcluded{Reason: reason}
	}
}

// Accept records that the poller accepted a reconciliation for this
// failure. A later turn supersedes it even when the worker used the same
// token for both.
func (r *RejectedLogins) Accept(profileID string, cause *CredentialSyncCause) {
	panic("unported: mj-core/src/credentials.rs RejectedLogins::accept")
}

// WasAccepted: an authentication event can arrive as both a relay signal
// and a completed turn. Both have the same ordinal.
func (r *RejectedLogins) WasAccepted(sessionID string, ordinal uint64) bool {
	_, ok := r.failures[sessionID][ordinal]
	return ok
}

// Supersede: a preceding failure cannot be used to report a later turn.
// Warnings after a terminal failure have their own ordinals and leave it
// intact.
func (r *RejectedLogins) Supersede(sessionID string, ordinal uint64) {
	session, ok := r.failures[sessionID]
	if !ok {
		return
	}
	for previous := range session {
		if previous < ordinal {
			delete(session, previous)
		}
	}
	if len(session) == 0 {
		delete(r.failures, sessionID)
	}
}

// RetainFailures: a sync finishing after removal cannot revive a retired
// decision.
func (r *RejectedLogins) RetainFailures(keep func(string) bool) {
	for id := range r.failures {
		if !keep(id) {
			delete(r.failures, id)
		}
	}
	r.RetainDeliveries(keep)
}

// Observe takes in one finished sync of a configured profile.
func (r *RejectedLogins) Observe(result *CredentialSyncResult, profile *HarnessProfile) {
	r.Conclude(result, profile)
}

// Conclude records the reconciliation's own observation. A notice describes
// that observation, while a later projection revalidates a refusal against
// the login the profile holds then. A nil profile is one no longer
// configured; a nil verdict means the result concluded no accepted failure.
func (r *RejectedLogins) Conclude(result *CredentialSyncResult, profile *HarnessProfile) LoginFailureVerdict {
	panic("unported: mj-core/src/credentials.rs RejectedLogins::conclude")
}

func (r *RejectedLogins) syncConclusion(result *CredentialSyncResult, cause *CredentialSyncCause, profile *HarnessProfile) loginConclusion {
	panic("unported: mj-core/src/credentials.rs RejectedLogins::sync_conclusion")
}

// FailureVerdict is the one verdict on a failed turn. The worker's held
// login, its retry intent and the delivery state are all read from this
// registry, which records them under one lock, so a delivery conclusion is
// never paired with an older view. No evidence for the session is unknown,
// never a hold; actorConnected says evidence is on its way.
func (r *RejectedLogins) FailureVerdict(profileID string, cause *CredentialSyncCause, current LoginFingerprint, actorConnected bool) LoginFailureVerdict {
	panic("unported: mj-core/src/credentials.rs RejectedLogins::failure_verdict")
}

func (r *RejectedLogins) WakeVerdict(staged LoginFingerprint, current LoginFingerprint) LoginFailureVerdict {
	switch {
	case current == nil:
		return LoginFailureVerdictUnrefused{Reason: "the current profile login could not be identified"}
	case current == staged:
		return LoginFailureVerdictRefused{}
	default:
		return LoginFailureVerdictRenewed{}
	}
}

// Refusal is why a new session on this profile would fail to sign in, while
// its login is still the one the provider refused.
func (r *RejectedLogins) Refusal(profileID string) (string, bool) {
	panic("unported: mj-core/src/credentials.rs RejectedLogins::refusal")
}

type SyncTrigger struct {
	ProfileID string
	Cause     *CredentialSyncCause
}

// CredentialSyncHandle is the handle the UI loops keep. Publishing targets
// and asking for an immediate sync are both non-blocking.
type CredentialSyncHandle struct {
	Targets  *Watch[[]CredentialSyncTarget]
	Triggers *Unbounded[SyncTrigger]
}

func (h *CredentialSyncHandle) SetTargets(targets []CredentialSyncTarget) {
	panic("unported: mj-core/src/credentials.rs CredentialSyncHandle::set_targets")
}

// SyncProfileNow reconciles one profile now instead of waiting for the next
// cycle. False when the coordinator has stopped.
func (h *CredentialSyncHandle) SyncProfileNow(profileID string, cause *CredentialSyncCause) bool {
	panic("unported: mj-core/src/credentials.rs CredentialSyncHandle::sync_profile_now")
}

func ProfilesWithTargets(targets []CredentialSyncTarget) []string {
	profiles := []string{}
	for _, target := range targets {
		if !slices.Contains(profiles, target.ProfileID) {
			profiles = append(profiles, target.ProfileID)
		}
	}
	slices.Sort(profiles)
	return profiles
}

func Enqueue(queue *[]SyncTrigger, trigger SyncTrigger) {
	if trigger.Cause == nil && slices.ContainsFunc(*queue, func(queued SyncTrigger) bool {
		return queued.ProfileID == trigger.ProfileID
	}) {
		return
	}
	*queue = append(*queue, trigger)
}

// GithubAuthenticationEnvironment is the authentication variables understood
// by GitHub CLI on github.com and enterprise hosts.
var GithubAuthenticationEnvironment = [4]string{
	"GH_TOKEN",
	"GITHUB_TOKEN",
	"GH_ENTERPRISE_TOKEN",
	"GITHUB_ENTERPRISE_TOKEN",
}

// RemoveGithubAuthenticationEnvironment removes them: a credential-free
// launch does so after environment composition.
func RemoveGithubAuthenticationEnvironment(environment map[string]string) {
	for _, name := range GithubAuthenticationEnvironment {
		delete(environment, name)
	}
}
