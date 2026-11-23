// Port of mj-targets/src/aws_assume_role.rs.
//
// Scoped, short-lived AWS credentials for one session, minted with STS
// `AssumeRole` and cached like the GitHub App installation tokens.
//
// The controller uses the host's own AWS login (the same
// AwsCredentialSource the unscoped export uses) to assume the role the
// profile names. The session receives only the resulting temporary
// credentials, which are limited to that role and to the session policy
// passed with the request.
//
// Name collisions: the SCOPE_KEY and FIXTURE_SCOPE_KEY statics are
// scopeKeyStore and fixtureScopeKeyStore beside the scopeKey function.

package targets

import (
	"context"
	"crypto/hmac"
	"crypto/sha256"
	_ "embed"
	"encoding/json"
	"errors"
	"fmt"
	"hash"
	"maps"
	"slices"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"
	"unicode"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

type clock func() time.Time

// ScopedAwsCredentials is the cache of role-scoped credentials. One process
// shares this cache between session launch, resume and the periodic
// credential sync. Entries are rebuildable: a daemon restart empties the
// cache and each live session then mints once.
type ScopedAwsCredentials struct {
	now clock
	mu  sync.Mutex
	// entries is keyed by scopedKey.encode().
	entries map[string]*scopedCell
}

// scopedKey is what makes two requests the same credentials: the session
// (it is the role session name), the role and policies, and the environment
// the AWS CLI runs in, because that selects the host identity that assumes
// the role (the AWS profile, but also `AWS_CONFIG_FILE`, `HOME` and the
// like). No String method: the environment is not for logs.
type scopedKey struct {
	sessionID   string
	role        core.AwsRoleConfig
	environment map[string]string
}

// encode is the key's identity as a map key: equal keys encode equally.
func (k *scopedKey) encode() string {
	encoded, err := json.Marshal(struct {
		SessionID   string             `json:"session_id"`
		Role        core.AwsRoleConfig `json:"role"`
		Environment map[string]string  `json:"environment"`
	}{k.sessionID, k.role, k.environment})
	if err != nil {
		panic(fmt.Sprintf("encode an AWS credential cache key: %v", err))
	}
	return string(encoded)
}

// newScopedKey is the key of a request for this session and source, owning
// copies of the role and environment.
func newScopedKey(sessionID string, role *core.AwsRoleConfig, environment map[string]string) scopedKey {
	owned := *role
	owned.SessionPolicyARNs = slices.Clone(role.SessionPolicyARNs)
	return scopedKey{sessionID: sessionID, role: owned, environment: maps.Clone(environment)}
}

// takeCell is the cell for key, created when missing, counted as held by the
// caller until it calls releaseCell (Rust: cloning the Arc out of the map).
func (s *ScopedAwsCredentials) takeCell(key scopedKey, create bool) *scopedCell {
	s.mu.Lock()
	defer s.mu.Unlock()
	encoded := key.encode()
	cell := s.entries[encoded]
	if cell == nil {
		if !create {
			return nil
		}
		cell = &scopedCell{key: key}
		s.entries[encoded] = cell
	}
	cell.holders++
	return cell
}

// releaseCell is the end of a takeCell hold (Rust: dropping the Arc).
func (s *ScopedAwsCredentials) releaseCell(cell *scopedCell) {
	s.mu.Lock()
	defer s.mu.Unlock()
	cell.holders--
}

// scopedCell is one key's entry. mu is held while minting into it; holders
// counts the callers that have taken the cell from the map and not yet
// finished with it (Rust: the Arc's strong count above the map's own), and
// is guarded by the map's lock.
type scopedCell struct {
	key     scopedKey
	mu      sync.Mutex
	entry   *scopedEntry
	holders int
}

type scopedEntry struct {
	credentials core.AwsCredentials
	mintedAt    time.Time
	expiresAt   time.Time
}

// refreshAt is when to refresh: once half of the granted lifetime has
// elapsed. That is derived from the lifetime AWS granted, so it scales with
// `duration_seconds`, and it leaves at least half of the shortest STS
// lifetime (15 minutes) for retries. The one place the half-life is
// computed: reuse and renewal scheduling both ask it.
func (e *scopedEntry) refreshAt() time.Time {
	lifetime := e.expiresAt.Sub(e.mintedAt)
	if lifetime < 0 {
		lifetime = 0
	}
	return e.mintedAt.Add(lifetime / 2)
}

func (e *scopedEntry) freshAt(now time.Time) bool {
	return now.Before(e.refreshAt())
}

// SweepExpired drops expired credentials. Called once per periodic
// credential sync pass and not on each call, which would make a pass
// quadratic in its sessions: the daemon holds a session's credentials for at
// most their own lifetime after the session stops being synced, and no
// expired secret survives a pass.
func (s *ScopedAwsCredentials) SweepExpired() {
	now := s.now()
	s.mu.Lock()
	defer s.mu.Unlock()
	sweep(s.entries, now)
}

// scopedPeek reads the entry cached for this session and source, when there
// is one and nobody is minting into it (Rust: ScopedAwsCredentials::peek).
func scopedPeek[T any](s *ScopedAwsCredentials, sessionID string, source *core.AwsCredentialSource, read func(entry *scopedEntry, now time.Time) (T, bool)) (T, bool) {
	var zero T
	if source.Role == nil {
		return zero, false
	}
	now := s.now()
	cell := s.takeCell(newScopedKey(sessionID, source.Role, source.Environment), false)
	if cell == nil {
		return zero, false
	}
	defer s.releaseCell(cell)
	if !cell.mu.TryLock() {
		return zero, false
	}
	defer cell.mu.Unlock()
	if cell.entry == nil {
		return zero, false
	}
	return read(cell.entry, now)
}

// RenewIn is how long until the credentials cached for this session and
// source are no longer fresh and are due to be renewed; false when none are
// cached (or another caller is minting into the entry), or when they are due
// already: no wait is a reason to retry, not to renew again at once.
func (s *ScopedAwsCredentials) RenewIn(sessionID string, source *core.AwsCredentialSource) (time.Duration, bool) {
	return scopedPeek(s, sessionID, source, func(entry *scopedEntry, now time.Time) (time.Duration, bool) {
		wait := entry.refreshAt().Sub(now)
		return wait, wait > 0
	})
}

// RemainingValidity is how long the cached credentials stay valid; false
// when none are cached or they have expired.
func (s *ScopedAwsCredentials) RemainingValidity(sessionID string, source *core.AwsCredentialSource) (time.Duration, bool) {
	return scopedPeek(s, sessionID, source, func(entry *scopedEntry, now time.Time) (time.Duration, bool) {
		left := entry.expiresAt.Sub(now)
		return left, left > 0
	})
}

// Forget drops the credentials cached for this session and source, so the
// next request mints new ones: for a session whose credentials may have
// been refused, or lost from the worker. Blocking, like CredentialsFor: it
// waits for a mint in progress for the key.
func (s *ScopedAwsCredentials) Forget(sessionID string, source *core.AwsCredentialSource) {
	if source.Role == nil {
		return
	}
	cell := s.takeCell(newScopedKey(sessionID, source.Role, source.Environment), false)
	if cell == nil {
		return
	}
	defer s.releaseCell(cell)
	cell.mu.Lock()
	cell.entry = nil
	cell.mu.Unlock()
}

// Holds reports whether the cache holds credentials for the session.
func (s *ScopedAwsCredentials) Holds(sessionID string) bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	for _, cell := range s.entries {
		if cell.key.sessionID == sessionID {
			return true
		}
	}
	return false
}

var (
	sharedScopedOnce sync.Once
	sharedScoped     *ScopedAwsCredentials
)

func ScopedAwsCredentialsShared() *ScopedAwsCredentials {
	sharedScopedOnce.Do(func() { sharedScoped = scopedAwsCredentialsWithClock(time.Now) })
	return sharedScoped
}

func scopedAwsCredentialsWithClock(now clock) *ScopedAwsCredentials {
	return &ScopedAwsCredentials{now: now, entries: map[string]*scopedCell{}}
}

// CredentialsFor is credentials for one session, from the cache while
// enough of their lifetime remains and otherwise from a new `AssumeRole`
// call. Blocking: run it off the event loop. Callers for one key wait for
// the first caller's mint instead of minting again.
//
// A failed mint is an error even when the previous credentials have not yet
// expired. The session keeps what it has installed, and the caller reports
// the failure; nothing widens the scope.
func (s *ScopedAwsCredentials) CredentialsFor(sessionID string, source *core.AwsCredentialSource, executor core.CommandExecutor) (core.AwsCredentials, error) {
	role := source.Role
	if role == nil {
		return core.AwsCredentials{}, errors.New("no AWS role is configured for this profile")
	}
	hostProfile := selectedProfile(source)
	now := s.now()
	cell := s.takeCell(newScopedKey(sessionID, role, source.Environment), true)
	defer s.releaseCell(cell)
	cell.mu.Lock()
	defer cell.mu.Unlock()
	if cell.entry != nil && cell.entry.freshAt(now) {
		return cell.entry.credentials, nil
	}
	name := SessionName(sessionID)
	command := AssumeRoleCommand(role, hostProfile, name, source.Environment)
	// Only the call to AWS (and reading its answer) is the role's to
	// explain: its advice, including the unscoped fallback, does not apply
	// to a failure of the controller's own signing key.
	issued, err := mint(&command, hostProfile, executor)
	if err == nil {
		// Recorded beside the secret on the worker, so what Mjolnir issued
		// for this session's role is recognisable later.
		var sourceIdentity *string
		if role.SourceIdentity {
			sourceIdentity = &name
		}
		issued, err = issued.IssuedFor(role.RoleARN, name, role.SessionPolicyFingerprint(), sourceIdentity)
	}
	if err != nil {
		return core.AwsCredentials{}, fmt.Errorf("Could not assume AWS role %s for this session. The controller's AWS login (profile %s) must be allowed to assume it: give that login sts:AssumeRole on the role, and name it in the role's trust policy: %w",
			role.RoleARN, hostProfile, err)
	}
	// ...and authenticated with a key the session cannot compute.
	minted, err := issued.SignScope(SignScope)
	if err != nil {
		return core.AwsCredentials{}, fmt.Errorf("could not sign the session's AWS scope record with the controller's signing key: %w", err)
	}
	mintedAt := s.now()
	millis, ok := minted.ExpiresAtEpochMillis()
	if !ok || millis < 0 {
		return core.AwsCredentials{}, errors.New("AWS returned credentials without an expiration")
	}
	cell.entry = &scopedEntry{credentials: minted, mintedAt: mintedAt, expiresAt: time.UnixMilli(millis)}
	return minted, nil
}

// sweep drops entries whose credentials have expired, and empty cells nobody
// holds. A cell another caller holds (it is minting into it, or waiting to)
// is left alone, expired or not: removing it would let that caller mint into
// a cell the map no longer knows. The map lock is held, so nobody can newly
// take a cell while this counts them.
func sweep(entries map[string]*scopedCell, now time.Time) {
	for encoded, cell := range entries {
		if cell.holders > 0 {
			continue
		}
		if !cell.mu.TryLock() {
			continue
		}
		live := cell.entry != nil && cell.entry.expiresAt.After(now)
		cell.mu.Unlock()
		if !live {
			delete(entries, encoded)
		}
	}
}

func selectedProfile(source *core.AwsCredentialSource) string {
	if profile, ok := source.Environment["AWS_PROFILE"]; ok {
		return profile
	}
	return "default"
}

// scopeKeyCell holds a derived scope key once it is loaded. A load that
// fails leaves it empty, so the next use tries again.
type scopeKeyCell struct {
	mu  sync.Mutex
	key []byte
}

// scopeKeyStore holds the key that authenticates scope records, derived
// from the controller's private signing key (the phone cookie key, kept
// owner-only in the data directory and created once) so no new secret file
// is needed. Rotating that key, the documented sign-everyone-out gesture,
// also makes every record fail verification, which withdraws what a failed
// mint would otherwise keep: the safe direction.
//
// The key is derived once per process and kept: reading it can create the
// key file and block on its lock, so verification must not repeat that, and
// the context-aware path loads it off the caller's goroutine. A rotation
// therefore takes effect when the daemon restarts, as it does for phone
// cookies.
var scopeKeyStore scopeKeyCell

// fixtureScopeKeyStore is scopeKeyStore for tests and fixture processes.
var fixtureScopeKeyStore scopeKeyCell

func scopeKeyCache() (*scopeKeyCell, bool, error) {
	fixture := cfgTest || core.TestFixturesEnabled()
	if !fixture {
		return &scopeKeyStore, false, nil
	}
	if SigningKeyUnavailable.Load() {
		return nil, true, errors.New("the signing key is unreadable (test)")
	}
	return &fixtureScopeKeyStore, true, nil
}

// cached is the key the cell holds, or nil before it is loaded.
func (c *scopeKeyCell) cached() []byte {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.key
}

// scopeKey is blocking: it reads (or creates) the signing key on first use.
func scopeKey() ([]byte, error) {
	cache, fixture, err := scopeKeyCache()
	if err != nil {
		return nil, err
	}
	return loadScopeKey(cache, fixture)
}

func loadScopeKey(cache *scopeKeyCell, fixture bool) ([]byte, error) {
	if key := cache.cached(); key != nil {
		return key, nil
	}
	var root []byte
	if fixture {
		root = []byte("test signing key, not a secret")
	} else {
		key, err := LoadOrCreateCookieKey(CookieKeyPath())
		if err != nil {
			return nil, fmt.Errorf("read the controller's signing key: %w", err)
		}
		root = key
	}
	derive := hmac.New(sha256.New, root)
	derive.Write([]byte("mjolnir aws credential scope key v1"))
	derived := derive.Sum(nil)
	cache.mu.Lock()
	defer cache.mu.Unlock()
	if cache.key == nil {
		cache.key = derived
	}
	return cache.key, nil
}

// scopeKeyWithoutBlocking is the key for code that must not block: the
// first use is loaded on another goroutine, every later one is a read of the
// kept key.
func scopeKeyWithoutBlocking(ctx context.Context) ([]byte, error) {
	cache, fixture, err := scopeKeyCache()
	if err != nil {
		return nil, err
	}
	if key := cache.cached(); key != nil {
		return key, nil
	}
	type loaded struct {
		key []byte
		err error
	}
	done := make(chan loaded, 1)
	go func() {
		key, err := loadScopeKey(cache, fixture)
		done <- loaded{key, err}
	}()
	select {
	case result := <-done:
		return result.key, result.err
	case <-ctx.Done():
		return nil, fmt.Errorf("signing key load task stopped: %w", ctx.Err())
	}
}

// SigningKeyUnavailable lets a test make the signing key unreadable (Rust: a
// thread-local flag the test sets on its own thread).
var SigningKeyUnavailable atomic.Bool

func scopeMAC(key []byte, scope *core.AwsCredentialScope, secretFingerprint string) hash.Hash {
	mac := hmac.New(sha256.New, key)
	mac.Write(scope.SignedPayload(secretFingerprint))
	return mac
}

// SignScope is the tag for a scope record describing the secret with this
// fingerprint.
func SignScope(scope *core.AwsCredentialScope, secretFingerprint string) (string, error) {
	key, err := scopeKey()
	if err != nil {
		return "", err
	}
	mac := scopeMAC(key, scope, secretFingerprint)
	return core.LowerHex(mac.Sum(nil)), nil
}

// ScopeIsAuthentic reports whether a record the worker reports was made by
// this controller for the secret with this fingerprint. A record the session
// could have written itself does not verify. An error means the answer is
// unknown (the signing key could not be read), which is not the same as
// "not authentic": callers must not act on it as if the record were forged.
func ScopeIsAuthentic(ctx context.Context, scope *core.AwsCredentialScope, secretFingerprint string) (bool, error) {
	key, err := scopeKeyWithoutBlocking(ctx)
	if err != nil {
		return false, err
	}
	// Each pair of characters is one byte, as Rust's u8::from_str_radix reads
	// it (an optional leading '+'); an odd last character is ignored.
	tag := make([]byte, 0, len(scope.Tag)/2)
	for index := 0; index < len(scope.Tag)/2; index++ {
		digits := scope.Tag[index*2 : index*2+2]
		if len(digits) > 1 && digits[0] == '+' {
			digits = digits[1:]
		}
		value, err := strconv.ParseUint(digits, 16, 8)
		if err != nil {
			return false, nil
		}
		tag = append(tag, byte(value))
	}
	return hmac.Equal(scopeMAC(key, scope, secretFingerprint).Sum(nil), tag), nil
}

// stsDefaultDuration is what AWS documents as the default `DurationSeconds`
// of `AssumeRole` when the call sets none: one hour.
const stsDefaultDuration = 3600 * time.Second

// CredentialHalfLife is half the lifetime a role's credentials are granted,
// which is when they are renewed (scopedEntry.refreshAt). A renewal that
// keeps failing is retried no more slowly than this, because slower than
// this the credentials would run out between attempts.
func CredentialHalfLife(role *core.AwsRoleConfig) time.Duration {
	lifetime := stsDefaultDuration
	if role.DurationSeconds != nil {
		lifetime = time.Duration(*role.DurationSeconds) * time.Second
	}
	return lifetime / 2
}

// ForWorkerProfile is role-scoped credentials installed under the profile
// name the worker's configuration already has, which can differ from the
// host profile the mint ran as. The name is part of the secret, so the scope
// is signed again.
func ForWorkerProfile(credentials core.AwsCredentials, profile string) (core.AwsCredentials, error) {
	if credentials.Profile() == profile {
		return credentials, nil
	}
	renamed, err := credentials.WithProfile(profile)
	if err != nil {
		return core.AwsCredentials{}, err
	}
	signed, err := renamed.SignScope(SignScope)
	if err != nil {
		return core.AwsCredentials{}, fmt.Errorf("could not sign the session's AWS scope record with the controller's signing key: %w", err)
	}
	return signed, nil
}

// SessionName is the role session name: it appears in the assumed-role ARN
// of every CloudTrail event the session causes. A session id is 32 hex
// characters, so this fits STS's 64-character limit.
func SessionName(sessionID string) string {
	return "mj-" + sessionID
}

// AssumeRoleCommand is `aws sts assume-role` for one session, run with the
// host's own login.
func AssumeRoleCommand(role *core.AwsRoleConfig, hostProfile string, sessionName string, environment map[string]string) core.CommandSpec {
	args := []string{
		"sts", "assume-role",
		"--role-arn", role.RoleARN,
		"--role-session-name", sessionName,
	}
	if policy, ok := role.EffectiveSessionPolicy(); ok {
		args = append(args, "--policy", policy)
	}
	if len(role.SessionPolicyARNs) > 0 {
		args = append(args, "--policy-arns")
		for _, arn := range role.SessionPolicyARNs {
			args = append(args, "arn="+arn)
		}
	}
	if role.DurationSeconds != nil {
		args = append(args, "--duration-seconds", strconv.FormatUint(uint64(*role.DurationSeconds), 10))
	}
	if role.SourceIdentity {
		args = append(args, "--source-identity", sessionName)
	}
	args = append(args, "--output", "json", "--profile", hostProfile)
	command := core.NewCommandSpec("aws", args).WithPurpose("assume the profile's AWS role for a session")
	// The output holds the new secret keys, and a failing credential helper
	// can print secrets to either stream.
	command.SensitiveOutput = true
	command.Env = maps.Clone(environment)
	if command.Env == nil {
		command.Env = map[string]string{}
	}
	command.Env["AWS_CLI_AUTO_PROMPT"] = "off"
	return command
}

func mint(command *core.CommandSpec, hostProfile string, executor core.CommandExecutor) (core.AwsCredentials, error) {
	output, err := executor.Execute(command)
	if err != nil {
		return core.AwsCredentials{}, fmt.Errorf("the AWS CLI could not run; install AWS CLI v2 on the controller host: %w", err)
	}
	if output.Status != 0 {
		return core.AwsCredentials{}, errors.New(assumeRoleFailure(output.Stderr, output.Status))
	}
	return core.AwsCredentialsFromAssumeRole(hostProfile, output.Stdout)
}

// assumeRoleFailure is what to tell the person about a failed call. The
// command's output is sensitive (a custom credential helper can print
// secrets to either stream), so only identifier-shaped fields are taken from
// the AWS CLI's error line,
// `An error occurred (<Code>) when calling the <Operation> operation: ...`:
// the error code and the operation, each checked against `[A-Za-z0-9.]+`.
// The free-text message is dropped. A client-side range check prints no
// operation, only `An error occurred (ParamValidation): ...`.
func assumeRoleFailure(stderr []byte, status int) string {
	identifier := func(text string) bool {
		if text == "" {
			return false
		}
		for i := 0; i < len(text); i++ {
			b := text[i]
			if !('a' <= b && b <= 'z' || 'A' <= b && b <= 'Z' || '0' <= b && b <= '9' || b == '.') {
				return false
			}
		}
		return true
	}
	for _, line := range libLines(lossyText(stderr)) {
		line = strings.TrimSpace(line)
		line = strings.TrimPrefix(line, "aws: [ERROR]: ")
		after, ok := strings.CutPrefix(line, "An error occurred (")
		if !ok {
			continue
		}
		code, rest, ok := strings.Cut(after, ")")
		if !ok || !identifier(code) {
			continue
		}
		operation, hasOperation := "", false
		if calling, ok := strings.CutPrefix(strings.TrimLeftFunc(rest, unicode.IsSpace), "when calling the "); ok {
			if named, _, ok := strings.Cut(calling, " operation"); ok && identifier(named) {
				operation, hasOperation = named, true
			}
		}
		if hasOperation && operation != "AssumeRole" {
			return fmt.Sprintf("AWS refused AssumeRole: %s (operation %s)", code, operation)
		}
		return "AWS refused AssumeRole: " + code
	}
	return fmt.Sprintf("the AWS CLI exited with status %d (its output is not shown)", status)
}

// Probe is one real `AssumeRole`, for `mj doctor`: the same command a
// session's mint runs, bypassing the cache. The role session name (and
// `SourceIdentity`, when the profile sets it) has the shape of a real
// session's, `mj-` and 32 random lowercase hex digits, because a trust
// policy may allow only that shape and a doctor probe must not be refused
// for looking different.
func Probe(source *core.AwsCredentialSource, executor core.CommandExecutor) (core.AwsCredentials, error) {
	role := source.Role
	if role == nil {
		return core.AwsCredentials{}, errors.New("no AWS role is configured for this profile")
	}
	hostProfile := selectedProfile(source)
	sessionID, err := core.NewSessionID()
	if err != nil {
		return core.AwsCredentials{}, err
	}
	command := AssumeRoleCommand(role, hostProfile, SessionName(sessionID), source.Environment)
	return mint(&command, hostProfile, executor)
}

// Obtain is the unscoped export for a profile without a role, the scoped
// mint for one with a role. Launch calls this; the credential sync calls the
// same two functions, because it exports once per profile and mints once
// per session.
func Obtain(sessionID string, source *core.AwsCredentialSource, executor core.CommandExecutor) (core.AwsCredentials, error) {
	if source.Role != nil {
		return ScopedAwsCredentialsShared().CredentialsFor(sessionID, source, executor)
	}
	return Export(source, executor)
}

// The STS replies recorded for tests of role assumption, here and in the
// packages that drive it (Rust: aws_assume_role::test_fixtures).

// TestFixturesAssumeRole is a recorded successful `aws sts assume-role`
// reply.
//
//go:embed testdata/tests/fixtures/aws-sts-assume-role.json
var TestFixturesAssumeRole string

// TestFixturesAccessDenied is a recorded AccessDenied error from
// `aws sts assume-role`.
//
//go:embed testdata/tests/fixtures/aws-sts-assume-role-access-denied.txt
var TestFixturesAccessDenied string
