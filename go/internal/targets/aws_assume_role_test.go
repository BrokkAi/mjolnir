// Port of the tests in mj-targets/src/aws_assume_role.rs.

package targets

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const assumeRoleSession = "0123456789abcdef0123456789abcdef"

func assumeRoleTestRole() core.AwsRoleConfig {
	return core.AwsRoleConfig{RoleARN: "arn:aws:iam::123456789012:role/mj-bedrock"}
}

func assumeRoleTestSource(role core.AwsRoleConfig) core.AwsCredentialSource {
	return core.AwsCredentialSource{
		Environment: map[string]string{"AWS_PROFILE": "work"},
		Role:        &role,
		Mints:       true,
	}
}

// recordedAssumeRoleOutput is the recorded `aws sts assume-role` output with
// its expiration moved to lifetime after issued, because credentials
// expired in the past are refused. Everything else is exactly what the CLI
// printed.
func recordedAssumeRoleOutput(t testing.TB, issued time.Time, lifetime time.Duration) []byte {
	t.Helper()
	var output map[string]any
	if err := json.Unmarshal([]byte(TestFixturesAssumeRole), &output); err != nil {
		t.Fatal(err)
	}
	output["Credentials"].(map[string]any)["Expiration"] = issued.Add(lifetime).UTC().Format("2006-01-02T15:04:05") + "+00:00"
	encoded, err := json.Marshal(output)
	if err != nil {
		t.Fatal(err)
	}
	return encoded
}

// assumeRoleTestClock is a test clock the executor and the cache share.
type assumeRoleTestClock struct {
	mu  sync.Mutex
	now time.Time
}

func newAssumeRoleTestClock() *assumeRoleTestClock {
	return &assumeRoleTestClock{now: time.Now()}
}

func (c *assumeRoleTestClock) Now() time.Time {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.now
}

func (c *assumeRoleTestClock) advance(by time.Duration) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.now = c.now.Add(by)
}

// assumeRoleFakeAws plays the AWS CLI: grants lifetime, counts calls, can fail.
type assumeRoleFakeAws struct {
	core.CommandExecutorDefaults
	t        testing.TB
	clock    *assumeRoleTestClock
	lifetime time.Duration
	calls    atomic.Int64
	mu       sync.Mutex
	commands []core.CommandSpec
	stderr   *string
}

func newAssumeRoleFakeAws(t testing.TB, clock *assumeRoleTestClock, lifetime time.Duration) *assumeRoleFakeAws {
	return &assumeRoleFakeAws{t: t, clock: clock, lifetime: lifetime}
}

func (a *assumeRoleFakeAws) setStderr(stderr *string) {
	a.mu.Lock()
	defer a.mu.Unlock()
	a.stderr = stderr
}

func (a *assumeRoleFakeAws) recorded() []core.CommandSpec {
	a.mu.Lock()
	defer a.mu.Unlock()
	return slices.Clone(a.commands)
}

func (a *assumeRoleFakeAws) PauseDeadline() *core.DeadlinePause { return core.DeadlinePauseNone() }

func (a *assumeRoleFakeAws) CancellationRequestedWithoutDeadline() bool {
	return a.CancellationRequested()
}

func (a *assumeRoleFakeAws) ExecuteWithoutDeadline(command *core.CommandSpec) (core.CommandOutput, error) {
	return a.Execute(command)
}

func (a *assumeRoleFakeAws) ExecuteWithStdinWithoutDeadline(command *core.CommandSpec, input io.Reader) (core.CommandOutput, error) {
	return a.ExecuteWithStdin(command, input)
}

func (a *assumeRoleFakeAws) ExecuteCleanup(command *core.CommandSpec) (core.CommandOutput, error) {
	return a.Execute(command)
}

func (a *assumeRoleFakeAws) ExecuteCleanupWithTimeout(command *core.CommandSpec, _ time.Duration) (core.CommandOutput, error) {
	return a.ExecuteCleanup(command)
}

func (a *assumeRoleFakeAws) Execute(command *core.CommandSpec) (core.CommandOutput, error) {
	if !slices.Equal(command.Args[:2], []string{"sts", "assume-role"}) {
		panic(fmt.Sprintf("unexpected command %q", command.Args))
	}
	if !command.SensitiveOutput {
		panic("the assume-role command's output must be sensitive")
	}
	a.calls.Add(1)
	a.mu.Lock()
	a.commands = append(a.commands, *command)
	stderr := a.stderr
	a.mu.Unlock()
	if stderr != nil {
		return core.CommandOutput{Status: 254, Stdout: []byte("private-key-material"), Stderr: []byte(*stderr)}, nil
	}
	return core.CommandOutput{Status: 0, Stdout: recordedAssumeRoleOutput(a.t, a.clock.Now(), a.lifetime), Stderr: []byte{}}, nil
}

func assumeRoleCache(clock *assumeRoleTestClock) *ScopedAwsCredentials {
	return scopedAwsCredentialsWithClock(clock.Now)
}

func assumeRoleEncode(t testing.TB, credentials core.AwsCredentials) string {
	t.Helper()
	encoded, err := credentials.Encode()
	if err != nil {
		t.Fatal(err)
	}
	return string(encoded)
}

func TestCredentialsAreReusedUntilHalfTheirLifetimeHasPassed(t *testing.T) {
	for _, lifetime := range []time.Duration{900 * time.Second, 3600 * time.Second} {
		clock := newAssumeRoleTestClock()
		aws := newAssumeRoleFakeAws(t, clock, lifetime)
		cache := assumeRoleCache(clock)
		source := assumeRoleTestSource(assumeRoleTestRole())
		mint := func() core.AwsCredentials {
			credentials, err := cache.CredentialsFor(assumeRoleSession, &source, aws)
			if err != nil {
				t.Fatal(err)
			}
			return credentials
		}
		first := mint()
		// AWS reports whole seconds, so the granted lifetime can be a
		// fraction of a second short.
		near := func(wait time.Duration, ok bool, expected time.Duration) bool {
			return ok && wait <= expected && expected-wait < time.Second
		}
		if wait, ok := cache.RenewIn(assumeRoleSession, &source); !near(wait, ok, lifetime/2) {
			t.Fatalf("renew in %v (%v), want about %v", wait, ok, lifetime/2)
		}
		clock.advance(lifetime/2 - time.Second)
		if assumeRoleEncode(t, mint()) != assumeRoleEncode(t, first) {
			t.Fatal("fresh credentials were not reused")
		}
		if calls := aws.calls.Load(); calls != 1 {
			t.Fatalf("calls = %d, want 1", calls)
		}
		// The renewal is scheduled for the moment reuse stops.
		if wait, ok := cache.RenewIn(assumeRoleSession, &source); !near(wait, ok, time.Second) {
			t.Fatalf("renew in %v (%v), want about 1s", wait, ok)
		}
		// Half the granted lifetime has passed: the next pass renews, and no
		// renewal is scheduled for "now", which would loop without waiting.
		clock.advance(time.Second)
		if wait, ok := cache.RenewIn(assumeRoleSession, &source); ok {
			t.Fatalf("renew in %v, want none", wait)
		}
		mint()
		if calls := aws.calls.Load(); calls != 2 {
			t.Fatalf("calls = %d, want 2", calls)
		}
	}
}

// The signing key is the controller's own: when it cannot be read, the
// failure says so, and does not tell the person to give sessions their
// unscoped AWS credentials instead.
func TestASigningKeyFailureIsNotReportedAsAFailedRoleAssumption(t *testing.T) {
	clock := newAssumeRoleTestClock()
	aws := newAssumeRoleFakeAws(t, clock, 3600*time.Second)
	cache := assumeRoleCache(clock)
	source := assumeRoleTestSource(assumeRoleTestRole())
	SigningKeyUnavailable.Store(true)
	_, err := cache.CredentialsFor(assumeRoleSession, &source, aws)
	SigningKeyUnavailable.Store(false)
	if err == nil {
		t.Fatal("a mint without the signing key succeeded")
	}
	reported := err.Error()
	if !strings.Contains(reported, "signing key") {
		t.Fatal(reported)
	}
	if strings.Contains(reported, "Could not assume") || strings.Contains(reported, "unscoped") {
		t.Fatal(reported)
	}
}

// Verification uses the key derived once per process: after the first use,
// the context-aware path reads it from memory instead of loading it again.
func TestVerificationKeepsTheDerivedKeyForLaterUse(t *testing.T) {
	clock := newAssumeRoleTestClock()
	aws := newAssumeRoleFakeAws(t, clock, 3600*time.Second)
	source := assumeRoleTestSource(assumeRoleTestRole())
	issued, err := assumeRoleCache(clock).CredentialsFor(assumeRoleSession, &source, aws)
	if err != nil {
		t.Fatal(err)
	}
	scope := *issued.Scope()
	encoded, err := issued.Encode()
	if err != nil {
		t.Fatal(err)
	}
	fingerprint := core.AwsCredentialsSnapshot(encoded).Fingerprint
	authentic, err := ScopeIsAuthentic(context.Background(), &scope, fingerprint)
	if err != nil {
		t.Fatal(err)
	}
	if !authentic {
		t.Fatal("the controller's own record does not verify")
	}
	if fixtureScopeKeyStore.cached() == nil {
		t.Fatal("the derived key was not kept")
	}
	forged := scope
	forged.RoleArn += "x"
	authentic, err = ScopeIsAuthentic(context.Background(), &forged, fingerprint)
	if err != nil {
		t.Fatal(err)
	}
	if authentic {
		t.Fatal("a forged record verifies")
	}
}

func TestEachSessionHasItsOwnCredentialsAndSessionName(t *testing.T) {
	clock := newAssumeRoleTestClock()
	aws := newAssumeRoleFakeAws(t, clock, 3600*time.Second)
	cache := assumeRoleCache(clock)
	source := assumeRoleTestSource(assumeRoleTestRole())
	other := "fedcba9876543210fedcba9876543210"
	for _, session := range []string{assumeRoleSession, other, assumeRoleSession} {
		if _, err := cache.CredentialsFor(session, &source, aws); err != nil {
			t.Fatal(err)
		}
	}
	var names []string
	for _, command := range aws.recorded() {
		at := slices.Index(command.Args, "--role-session-name")
		names = append(names, command.Args[at+1])
	}
	if want := []string{"mj-" + assumeRoleSession, "mj-" + other}; !slices.Equal(names, want) {
		t.Fatalf("session names = %q, want %q", names, want)
	}
}

func TestAFailedRenewalIsAnErrorAndKeepsTheEntryUntilItExpires(t *testing.T) {
	clock := newAssumeRoleTestClock()
	aws := newAssumeRoleFakeAws(t, clock, 900*time.Second)
	cache := assumeRoleCache(clock)
	source := assumeRoleTestSource(assumeRoleTestRole())
	if _, err := cache.CredentialsFor(assumeRoleSession, &source, aws); err != nil {
		t.Fatal(err)
	}
	clock.advance(450 * time.Second)
	denied := TestFixturesAccessDenied
	aws.setStderr(&denied)
	_, err := cache.CredentialsFor(assumeRoleSession, &source, aws)
	if err == nil {
		t.Fatal("a refused renewal succeeded")
	}
	reported := err.Error()
	// The person is told what AWS said and how to proceed; the helper's other
	// output (here, key material on stdout) is never repeated.
	if !strings.Contains(reported, "AccessDenied") {
		t.Fatal(reported)
	}
	// The remedy is the trust-policy fix only; the scope is never offered for
	// removal, which would widen access.
	if !strings.Contains(reported, "named in the role's trust policy") && !strings.Contains(reported, "name it in the role's trust policy") {
		t.Fatal(reported)
	}
	for _, absent := range []string{"remove [profiles.", "unscoped", "private-key-material"} {
		if strings.Contains(reported, absent) {
			t.Fatal(reported)
		}
	}
	// AWS recovered: the next pass renews without needing the old entry.
	aws.setStderr(nil)
	if _, err := cache.CredentialsFor(assumeRoleSession, &source, aws); err != nil {
		t.Fatal(err)
	}
	if calls := aws.calls.Load(); calls != 3 {
		t.Fatalf("calls = %d, want 3", calls)
	}
}

func assumeRoleEntryCount(cache *ScopedAwsCredentials) int {
	cache.mu.Lock()
	defer cache.mu.Unlock()
	return len(cache.entries)
}

func TestExpiredEntriesAreDroppedByThePassAndNotByEveryCall(t *testing.T) {
	clock := newAssumeRoleTestClock()
	aws := newAssumeRoleFakeAws(t, clock, 900*time.Second)
	cache := assumeRoleCache(clock)
	source := assumeRoleTestSource(assumeRoleTestRole())
	if _, err := cache.CredentialsFor(assumeRoleSession, &source, aws); err != nil {
		t.Fatal(err)
	}
	if count := assumeRoleEntryCount(cache); count != 1 {
		t.Fatalf("entries = %d, want 1", count)
	}
	clock.advance(901 * time.Second)
	if _, err := cache.CredentialsFor("fedcba9876543210fedcba9876543210", &source, aws); err != nil {
		t.Fatal(err)
	}
	// A call scans nothing but its own key...
	if count := assumeRoleEntryCount(cache); count != 2 {
		t.Fatalf("entries = %d, want 2", count)
	}
	// ...the pass drops what has expired.
	cache.SweepExpired()
	if count := assumeRoleEntryCount(cache); count != 1 {
		t.Fatalf("entries = %d, want 1", count)
	}
	if cache.Holds(assumeRoleSession) {
		t.Fatal("the expired session's entry survived the pass")
	}
}

func TestConcurrentCallersForOneSessionShareOneMint(t *testing.T) {
	clock := newAssumeRoleTestClock()
	aws := newAssumeRoleFakeAws(t, clock, 3600*time.Second)
	cache := assumeRoleCache(clock)
	source := assumeRoleTestSource(assumeRoleTestRole())
	var group sync.WaitGroup
	for range 8 {
		group.Add(1)
		go func() {
			defer group.Done()
			if _, err := cache.CredentialsFor(assumeRoleSession, &source, aws); err != nil {
				t.Error(err)
			}
		}()
	}
	group.Wait()
	if calls := aws.calls.Load(); calls != 1 {
		t.Fatalf("calls = %d, want 1", calls)
	}
}

func TestAChangedCredentialSourceMintsAgain(t *testing.T) {
	clock := newAssumeRoleTestClock()
	aws := newAssumeRoleFakeAws(t, clock, 3600*time.Second)
	cache := assumeRoleCache(clock)
	source := assumeRoleTestSource(assumeRoleTestRole())
	for range 2 {
		if _, err := cache.CredentialsFor(assumeRoleSession, &source, aws); err != nil {
			t.Fatal(err)
		}
	}
	if calls := aws.calls.Load(); calls != 1 {
		t.Fatalf("calls = %d, want 1", calls)
	}
	// The same AWS profile name now resolves through another config file.
	source.Environment["AWS_CONFIG_FILE"] = "/elsewhere/config"
	if _, err := cache.CredentialsFor(assumeRoleSession, &source, aws); err != nil {
		t.Fatal(err)
	}
	if calls := aws.calls.Load(); calls != 2 {
		t.Fatalf("calls = %d, want 2", calls)
	}
	if got, ok := aws.recorded()[1].Env["AWS_CONFIG_FILE"]; !ok || got != "/elsewhere/config" {
		t.Fatalf("AWS_CONFIG_FILE = %q (%v)", got, ok)
	}
}

func TestTheSweepLeavesAnExpiredEntryACallerStillHolds(t *testing.T) {
	clock := newAssumeRoleTestClock()
	aws := newAssumeRoleFakeAws(t, clock, 900*time.Second)
	cache := assumeRoleCache(clock)
	source := assumeRoleTestSource(assumeRoleTestRole())
	if _, err := cache.CredentialsFor(assumeRoleSession, &source, aws); err != nil {
		t.Fatal(err)
	}
	// A racing caller has taken the cell and waits for its lock.
	held := cache.takeCell(newScopedKey(assumeRoleSession, source.Role, source.Environment), false)
	clock.advance(901 * time.Second)
	cache.SweepExpired()
	if count := assumeRoleEntryCount(cache); count != 1 {
		t.Fatalf("held cells stay: entries = %d", count)
	}
	// That caller finds the expired entry in its own cell and mints again,
	// into the cell the cache still holds.
	cache.releaseCell(held)
	if _, err := cache.CredentialsFor(assumeRoleSession, &source, aws); err != nil {
		t.Fatal(err)
	}
	if count := assumeRoleEntryCount(cache); count != 1 {
		t.Fatalf("entries = %d, want 1", count)
	}
	if calls := aws.calls.Load(); calls != 2 {
		t.Fatalf("calls = %d, want 2", calls)
	}
	// Once nobody holds it and it has expired, a pass removes it with no mint
	// at all: no expired secret outlives the periodic sync.
	clock.advance(901 * time.Second)
	cache.SweepExpired()
	if count := assumeRoleEntryCount(cache); count != 0 {
		t.Fatalf("entries = %d, want 0", count)
	}
	if calls := aws.calls.Load(); calls != 2 {
		t.Fatalf("calls = %d, want 2", calls)
	}
}

func TestTheRecordedAssumeRoleOutputBecomesProcessFormatCredentials(t *testing.T) {
	clock := newAssumeRoleTestClock()
	output := recordedAssumeRoleOutput(t, clock.Now(), 900*time.Second)
	credentials, err := core.AwsCredentialsFromAssumeRole("work", output)
	if err != nil {
		t.Fatal(err)
	}
	var encoded map[string]any
	if err := json.Unmarshal([]byte(assumeRoleEncode(t, credentials)), &encoded); err != nil {
		t.Fatal(err)
	}
	if encoded["Version"] != float64(1) {
		t.Fatalf("Version = %v", encoded["Version"])
	}
	if encoded["AccessKeyId"] != "ASIAEXAMPLEEXAMPLE00" {
		t.Fatalf("AccessKeyId = %v", encoded["AccessKeyId"])
	}
	if encoded["SessionToken"] != "example-session-token" {
		t.Fatalf("SessionToken = %v", encoded["SessionToken"])
	}
	if encoded["Profile"] != "work" {
		t.Fatalf("Profile = %v", encoded["Profile"])
	}
	// The CLI prints the offset as +00:00 rather than Z.
	if expiration, _ := encoded["Expiration"].(string); !strings.HasSuffix(expiration, "+00:00") {
		t.Fatalf("Expiration = %v", encoded["Expiration"])
	}
	// Expired output is refused, and the error never repeats a secret.
	expired := recordedAssumeRoleOutput(t, clock.Now().Add(-3600*time.Second), 900*time.Second)
	_, err = core.AwsCredentialsFromAssumeRole("work", expired)
	if err == nil {
		t.Fatal("expired credentials were accepted")
	}
	if strings.Contains(err.Error(), "example-secret") {
		t.Fatal(err)
	}
}

// The command's output is sensitive: the notice holds the error code and
// the operation, both identifier-shaped, and never free text from the CLI or
// from a helper it ran.
func TestAwsCliFailuresReportOnlyTheErrorCodeAndOperation(t *testing.T) {
	denied := TestFixturesAccessDenied
	// A multiline stderr: a helper's lines before and after, one of them a
	// token-like secret, around the CLI's own error.
	stderr := fmt.Sprintf("credential helper: token sk-live-0123456789abcdef\n%s\nhelper trace: AKIAIOSFODNN7EXAMPLE secret-access-key\n", denied)
	if message := assumeRoleFailure([]byte(stderr), 254); message != "AWS refused AssumeRole: AccessDenied" {
		t.Fatal(message)
	}
	// Another operation is named; a range check has none.
	other := "aws: [ERROR]: An error occurred (ThrottlingException) when calling the GetCallerIdentity operation: token sk-live-0123456789abcdef\n"
	if message := assumeRoleFailure([]byte(other), 254); message != "AWS refused AssumeRole: ThrottlingException (operation GetCallerIdentity)" {
		t.Fatal(message)
	}
	rangeCheck := "\naws: [ERROR]: An error occurred (ParamValidation): Parameter validation failed:\nInvalid value for parameter DurationSeconds, value: 100, valid min value: 900\n"
	if message := assumeRoleFailure([]byte(rangeCheck), 252); message != "AWS refused AssumeRole: ParamValidation" {
		t.Fatal(message)
	}
	// A code or operation that is not identifier-shaped is not repeated.
	spoofed := "An error occurred (sk-live-0123456789abcdef!) when calling the AssumeRole operation: x\nAn error occurred (AccessDenied) when calling the Assume Role/secret operation: x\n"
	if message := assumeRoleFailure([]byte(spoofed), 255); strings.Contains(message, "sk-live") || strings.Contains(message, "secret") {
		t.Fatal(message)
	}
	helper := "Traceback: secret-token-value"
	if message := assumeRoleFailure([]byte(helper), 255); strings.Contains(message, "secret-token-value") || !strings.Contains(message, "255") {
		t.Fatal(message)
	}
}

func TestGoldenAssumeRoleCommandLines(t *testing.T) {
	full := assumeRoleTestRole()
	policy := `{"Version":"2012-10-17","Statement":[]}`
	full.SessionPolicy = &policy
	full.SessionPolicyARNs = []string{
		"arn:aws:iam::aws:policy/AmazonBedrockReadOnly",
		"arn:aws:iam::123456789012:policy/team-boundary",
	}
	duration := uint32(900)
	full.DurationSeconds = &duration
	full.SourceIdentity = true
	environment := map[string]string{"AWS_PROFILE": "work"}
	var rendered strings.Builder
	for _, labelled := range []struct {
		label string
		role  core.AwsRoleConfig
	}{{"default policy", assumeRoleTestRole()}, {"everything configured", full}} {
		command := AssumeRoleCommand(&labelled.role, "work", SessionName(assumeRoleSession), environment)
		fmt.Fprintf(&rendered, "# %s\n%s %s\n", labelled.label, command.Program, strings.Join(command.Args, "\n  "))
	}
	core.AssertGolden(t, "testdata", "aws-assume-role-command", rendered.String())
}
