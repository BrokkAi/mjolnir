// Port of mj-core/src/credentials/tests.rs.

package core

import (
	"context"
	"encoding/base64"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"

	acp "github.com/BrokkAi/acp-go"
	"github.com/BrokkAi/acp-go/jsonvalue"
	acpschema "github.com/BrokkAi/acp-go/schema"
)

// credentialsCompact is text as serde_json::to_vec writes the same value:
// compact, members in the order written.
func credentialsCompact(t *testing.T, text string) []byte {
	t.Helper()
	value, err := jsonvalue.Parse([]byte(text))
	if err != nil {
		t.Fatal(err)
	}
	encoded, err := jsonvalue.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	return encoded
}

func claudeCredentials(t *testing.T, expiresAt int64) []byte {
	return credentialsCompact(t, fmt.Sprintf(`{
		"claudeAiOauth": {
			"accessToken": "access",
			"refreshToken": "refresh",
			"expiresAt": %d,
			"refreshTokenExpiresAt": %d,
			"scopes": ["user:inference"],
			"subscriptionType": "max",
			"rateLimitTier": "default"
		}
	}`, expiresAt, expiresAt+1_000))
}

// codexFixtureExpirySeconds is the `exp` of the access token every Codex
// fixture carries.
const codexFixtureExpirySeconds = 1_785_901_860

// codexAccessToken is a JWT shaped like a Codex access token: header, `exp`
// claim, and a signature nothing here verifies.
func codexAccessToken(t *testing.T, expirySeconds int64) string {
	segment := func(text string) string {
		return base64.RawURLEncoding.EncodeToString(credentialsCompact(t, text))
	}
	return segment(`{"alg":"RS256","typ":"JWT"}`) + "." +
		segment(fmt.Sprintf(`{"exp":%d,"sub":"account"}`, expirySeconds)) + ".signature-is-never-checked"
}

func codexCredentials(t *testing.T, lastRefresh string) []byte {
	return credentialsCompact(t, fmt.Sprintf(`{
		"auth_mode": "chatgpt",
		"tokens": {
			"access_token": %q,
			"refresh_token": "refresh",
			"id_token": "id",
			"account_id": "account"
		},
		"last_refresh": %q
	}`, codexAccessToken(t, codexFixtureExpirySeconds), lastRefresh))
}

func kimiCredentials(t *testing.T, expiresAt int64) []byte {
	return credentialsCompact(t, fmt.Sprintf(`{
		"access_token": "access",
		"refresh_token": "refresh",
		"expires_at": %d,
		"expires_in": 900,
		"scope": "all",
		"token_type": "Bearer"
	}`, expiresAt))
}

func grokCredentials(t *testing.T, expiries ...string) []byte {
	grants := jsonvalue.Object{}
	for index, expiresAt := range expiries {
		grants = append(grants, jsonvalue.Member{
			Key: fmt.Sprintf("https://auth.x.ai::grant-%d", index),
			Value: jsonvalue.Object{
				{Key: "key", Value: "access"},
				{Key: "auth_mode", Value: "oidc"},
				{Key: "refresh_token", Value: "refresh"},
				{Key: "expires_at", Value: expiresAt},
				{Key: "oidc_issuer", Value: "https://auth.x.ai"},
			},
		})
	}
	encoded, err := jsonvalue.Marshal(grants)
	if err != nil {
		t.Fatal(err)
	}
	return encoded
}

func credentialsSnapshot(fingerprint string, freshness *int64) CredentialSnapshot {
	return CredentialSnapshot{Present: true, Fingerprint: fingerprint, FreshnessEpochMs: freshness}
}

func credentialsInt(value int64) *int64 { return &value }

func credentialsString(value string) *string { return &value }

func credentialsExpectMillis(t *testing.T, label string, got int64, ok bool, want *int64) {
	t.Helper()
	switch {
	case want == nil && ok:
		t.Fatalf("%s: got %d, want none", label, got)
	case want != nil && !ok:
		t.Fatalf("%s: got none, want %d", label, *want)
	case want != nil && got != *want:
		t.Fatalf("%s: got %d, want %d", label, got, *want)
	}
}

func TestGrokFreshnessReadsTheLatestRFC3339GrantExpiry(t *testing.T) {
	freshness, ok := CredentialFreshness(HarnessKindGrok, grokCredentials(t, "2026-08-17T02:19:01.724226598Z"))
	credentialsExpectMillis(t, "one grant", freshness, ok, credentialsInt(1_786_933_141_724))
	// A home may hold several grants; the newest expiry decides freshness.
	freshness, ok = CredentialFreshness(HarnessKindGrok, grokCredentials(t,
		"2026-08-17T02:19:01.724226598Z",
		"2026-08-17T04:19:01.724226598Z"))
	credentialsExpectMillis(t, "two grants", freshness, ok, credentialsInt(1_786_940_341_724))
	// Non-UTC offsets normalize to the same instant.
	freshness, ok = CredentialFreshness(HarnessKindGrok, grokCredentials(t, "2026-08-16T22:19:01.724226598-04:00"))
	credentialsExpectMillis(t, "offset", freshness, ok, credentialsInt(1_786_933_141_724))
}

// Hard-won: c94c68c0: single-use OAuth refresh tokens made concurrent container turns fail; this checks token-derived expiry used for proactive refresh
func TestEveryHarnessReportsExpiryOnlyWhereHelCanRefreshAheadOfIt(t *testing.T) {
	type fixture struct {
		kind      HarnessKind
		bytes     []byte
		freshness *int64
		expiry    *int64
	}
	fixtures := []fixture{
		{HarnessKindClaude, claudeCredentials(t, 1_755_000_000_000), credentialsInt(1_755_000_000_000), credentialsInt(1_755_000_000_000)},
		{HarnessKindCodex, codexCredentials(t, "2026-08-05T02:51:00.864587231Z"), credentialsInt(1_785_898_260_864), credentialsInt(codexFixtureExpirySeconds * 1_000)},
		{HarnessKindKimi, kimiCredentials(t, 1_755_000_000), credentialsInt(1_755_000_000_000), credentialsInt(1_755_000_000_000)},
		{HarnessKindGrok, grokCredentials(t, "2026-08-17T02:19:01.724226598Z"), credentialsInt(1_786_933_141_724), nil},
		// OpenCode's logins live in its database; the staged export orders
		// nothing, even when it carries an OAuth expiry.
		{HarnessKindOpenCode, []byte(`[{"id":"cred_1","integrationID":"anthropic","label":"OAuth","active":true,"value":{"type":"oauth","refresh":"r","access":"a","expires":1756000000000}}]`), nil, nil},
		{HarnessKindMuse, []byte(`{"schema_version":1,"providers":{"meta":{"access_token":"a","refresh_token":"r","expires_at":1756000000,"mechanism":"oauth"}}}`), credentialsInt(1_756_000_000_000), credentialsInt(1_756_000_000_000)},
		{HarnessKindAntigravity, []byte(`{"token":{"access_token":"a","refresh_token":"r","expiry":"2026-08-17T02:19:01.724226598Z"}}`), credentialsInt(1_786_933_141_724), nil},
		// DSH has no login; a file in its place orders nothing.
		{HarnessKindDsh, claudeCredentials(t, 1_755_000_000_000), nil, nil},
	}
	for _, kind := range HarnessKindAll {
		var found *fixture
		for index := range fixtures {
			if fixtures[index].kind == kind {
				found = &fixtures[index]
			}
		}
		if found == nil {
			t.Fatalf("%s needs a credential fixture", kind)
		}
		freshness, ok := CredentialFreshness(kind, found.bytes)
		credentialsExpectMillis(t, string(kind)+" freshness", freshness, ok, found.freshness)
		expiry, ok := CredentialExpiry(kind, found.bytes)
		credentialsExpectMillis(t, string(kind)+" expiry", expiry, ok, found.expiry)
	}
}

func TestUnreadableAccessTokensReportNoExpiry(t *testing.T) {
	for _, accessToken := range []string{
		`"not-a-jwt"`,
		`"header.not-base64!!.signature"`,
		fmt.Sprintf(`"header.%s.signature"`, base64.RawURLEncoding.EncodeToString([]byte(`{"sub":"only"}`))),
		`null`,
	} {
		bytes := credentialsCompact(t, fmt.Sprintf(`{
			"auth_mode": "chatgpt",
			"tokens": { "access_token": %s },
			"last_refresh": "2026-08-05T02:51:00.864587231Z"
		}`, accessToken))
		if expiry, ok := CredentialExpiry(HarnessKindCodex, bytes); ok {
			t.Fatalf("%s: expiry %d", accessToken, expiry)
		}
	}
	if _, ok := CredentialExpiry(HarnessKindCodex, []byte("not json")); ok {
		t.Fatal("not json has no expiry")
	}
	if _, ok := CredentialExpiry(HarnessKindClaude, []byte("{}")); ok {
		t.Fatal("an empty login has no expiry")
	}
}

func TestUnparseableCredentialsReportNoFreshness(t *testing.T) {
	for _, kind := range HarnessKindAll {
		if _, ok := CredentialFreshness(kind, []byte("not json")); ok {
			t.Fatalf("%s: not json", kind)
		}
		if _, ok := CredentialFreshness(kind, []byte("{}")); ok {
			t.Fatalf("%s: {}", kind)
		}
	}
	if _, ok := CredentialFreshness(HarnessKindCodex, codexCredentials(t, "yesterday")); ok {
		t.Fatal("yesterday is no time")
	}
}

func TestPayloadValidationRejectsEmptyOversizedAndMalformedDocuments(t *testing.T) {
	if ValidateCredentialPayload(HarnessKindClaude, []byte("")) == nil {
		t.Fatal("empty")
	}
	if ValidateCredentialPayload(HarnessKindClaude, []byte("not json")) == nil {
		t.Fatal("not json")
	}
	if ValidateCredentialPayload(HarnessKindClaude, []byte(strings.Repeat("a", MaxCredentialBytes+1))) == nil {
		t.Fatal("oversized")
	}
	if err := ValidateCredentialPayload(HarnessKindClaude, claudeCredentials(t, 1)); err != nil {
		t.Fatal(err)
	}
}

func TestIdenticalOrAbsentCopiesNeedNoSync(t *testing.T) {
	absent := CredentialSnapshotAbsent()
	if action := Reconcile(&absent, &absent); action != SyncActionNone {
		t.Fatalf("absent: %v", action)
	}
	canonical, session := credentialsSnapshot("a", credentialsInt(2)), credentialsSnapshot("a", credentialsInt(1))
	if action := Reconcile(&canonical, &session); action != SyncActionNone {
		t.Fatalf("identical: %v", action)
	}
}

// Hard-won: 4e5d6566: stale session copies caused live sessions to die after another copy consumed a single-use refresh token
func TestTheFresherCopyWinsAndAKnownTimeBeatsAnUnknownOne(t *testing.T) {
	for _, entry := range []struct {
		canonical, session CredentialSnapshot
		want               SyncAction
	}{
		{credentialsSnapshot("a", credentialsInt(2)), credentialsSnapshot("b", credentialsInt(1)), SyncActionPush},
		{credentialsSnapshot("a", credentialsInt(1)), credentialsSnapshot("b", credentialsInt(2)), SyncActionPull},
		{credentialsSnapshot("a", credentialsInt(1)), credentialsSnapshot("b", nil), SyncActionPush},
		{credentialsSnapshot("a", nil), credentialsSnapshot("b", credentialsInt(1)), SyncActionPull},
	} {
		if action := Reconcile(&entry.canonical, &entry.session); action != entry.want {
			t.Fatalf("%+v %+v: %v, want %v", entry.canonical, entry.session, action, entry.want)
		}
	}
}

func TestDifferingCopiesWithoutAnyFreshnessAreLeftAlone(t *testing.T) {
	canonical, session := credentialsSnapshot("a", nil), credentialsSnapshot("b", nil)
	if action := Reconcile(&canonical, &session); action != SyncActionNone {
		t.Fatalf("no freshness: %v", action)
	}
	canonical, session = credentialsSnapshot("a", credentialsInt(5)), credentialsSnapshot("b", credentialsInt(5))
	if action := Reconcile(&canonical, &session); action != SyncActionNone {
		t.Fatalf("equal freshness: %v", action)
	}
}

func TestCanonicalWriteIsOwnerOnlyAndReplacesThePreviousFile(t *testing.T) {
	path := HarnessAuthenticationMarker(HarnessKindKimi, t.TempDir())
	if err := WriteCredentialFile(HarnessKindKimi, path, kimiCredentials(t, 1)); err != nil {
		t.Fatal(err)
	}
	if err := WriteCredentialFile(HarnessKindKimi, path, kimiCredentials(t, 2)); err != nil {
		t.Fatal(err)
	}
	snapshot, bytes, err := ReadCredentialFile(HarnessKindKimi, path)
	if err != nil {
		t.Fatal(err)
	}
	if !snapshot.Present || string(bytes) != string(kimiCredentials(t, 2)) {
		t.Fatalf("read %+v %s", snapshot, bytes)
	}
	if runtime.GOOS != "windows" {
		info, err := os.Stat(path)
		if err != nil {
			t.Fatal(err)
		}
		if mode := info.Mode().Perm(); mode != 0o600 {
			t.Fatalf("mode %o", mode)
		}
	}
}

func TestCanonicalWriteRefusesASymlinkedDestination(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("Unix only")
	}
	home := t.TempDir()
	elsewhere := filepath.Join(home, "elsewhere.json")
	if err := os.WriteFile(elsewhere, []byte("{}"), 0o666); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(home, "auth.json")
	if err := os.Symlink(elsewhere, path); err != nil {
		t.Fatal(err)
	}
	err := WriteCredentialFile(HarnessKindCodex, path, codexCredentials(t, "2026-01-01T00:00:00Z"))
	if err == nil || !strings.Contains(err.Error(), "symbolic link") {
		t.Fatalf("error %v", err)
	}
	if body, _ := os.ReadFile(elsewhere); string(body) != "{}" {
		t.Fatalf("elsewhere %s", body)
	}
}

func TestAMissingCredentialFileReadsAsAnAbsentSnapshot(t *testing.T) {
	path := HarnessAuthenticationMarker(HarnessKindCodex, t.TempDir())
	snapshot, bytes, err := ReadCredentialFile(HarnessKindCodex, path)
	if err != nil {
		t.Fatal(err)
	}
	if snapshot.Present || len(bytes) != 0 {
		t.Fatalf("read %+v %q", snapshot, bytes)
	}
}

func TestAuthFailurePhrasesMatchAndNearMissesDoNot(t *testing.T) {
	for _, entry := range []struct {
		kind HarnessKind
		text string
		want bool
	}{
		// Hard-won: #1508: Codex ACP session-load refusals bypassed login classification.
		{HarnessKindCodex, `ACP protocol failed: Internal error: "load ACP session 01a1c46e-48d2-7ef0-a803-ce37deb2e2d8: Internal error: {\"details\": \"workspace routing discovery unauthorized (401)\"}"`, true},
		{HarnessKindClaude, "Error: OAuth session expired and could not be refreshed", true},
		{HarnessKindCodex, `{"error":{"type":"authentication_error"}}`, true},
		{HarnessKindKimi, "invalid_grant: refresh token rejected", true},
		{HarnessKindKimi, "OAuthUnauthorizedError: The provided authorization grant is invalid", true},
		{HarnessKindCodex, "error=INVALID_GRANT", true},
		{HarnessKindClaude, "the OAuth session expired last week, but we refreshed it", false},
		{HarnessKindClaude, "authentication succeeded", false},
		{HarnessKindCodex, "docs_src/authentication_error_status_code/tutorial001_an_py310.py", false},
		{HarnessKindCodex, "someauthentication_error", false},
		{HarnessKindCodex, "invalid_grant_result", false},
	} {
		if got := AuthFailureSignature(entry.kind, entry.text); got != entry.want {
			t.Fatalf("%q: %v, want %v", entry.text, got, entry.want)
		}
	}
}

func credentialsRelayEvent(observation RelayObservation) RelayEvent {
	return RelayEvent{
		Format:         RelayEventFormatV1,
		Ordinal:        1,
		PreviousDigest: RelayEventGenesisDigest,
		Digest:         strings.Repeat("a", 64),
		RecordedAtMs:   1,
		Observation:    observation,
	}
}

func credentialsTextBlock(text string) acpschema.ContentBlock {
	return acpschema.ContentBlock{Kind: acpschema.ContentBlockKindText, Text: &acpschema.TextContent{Text: text}}
}

func credentialsSyncReason(event RelayEvent) (CredentialSyncReason, bool) {
	return RelayEventCredentialSyncReason(&event)
}

// R14-1: when its refresh token is rejected, Codex fails the prompt with this
// error (reverify-14 cli/011, cli/013). Neither its sentence nor its
// `codexErrorInfo` counted as an auth failure, so no credential
// reconciliation ran and no `mj login` notice appeared.
// Hard-won: 1fb50fe0: R14-1 Codex refresh refusals skipped credential reconciliation and produced a duplicate unreadable warning
func TestACodexRefreshFailureIsAnAuthFailure(t *testing.T) {
	const sentence = "Your access token could not be refreshed. Please log out and sign in again."
	failedTurn := func(data string) RelayEvent {
		diagnostic := TurnDiagnosticFromACP(&acp.RPCError{Code: -32603, Message: "Internal error", Data: []byte(data)})
		return credentialsRelayEvent(RelayObservationCommandCompleted{
			CommandID: "prompt-1",
			Outcome:   RelayCommandOutcomePrompt{StopReason: "error", Diagnostic: &diagnostic},
		})
	}

	if !AuthFailureSignature(HarnessKindCodex, sentence) {
		t.Fatal("the sentence is an auth failure")
	}
	// The turn that failed this way reports it, whether or not a warning
	// repeats the sentence.
	if reason, ok := credentialsSyncReason(failedTurn(fmt.Sprintf(`{"message": %q, "codexErrorInfo": "unauthorized"}`, sentence))); !ok || reason != CredentialSyncReasonAuthenticationFailure {
		t.Fatalf("failed turn: %q %v", reason, ok)
	}
	// The error kind alone is enough when Codex words the sentence
	// differently.
	if reason, ok := credentialsSyncReason(failedTurn(`{"message": "Your session ended.", "codexErrorInfo": "unauthorized"}`)); !ok || reason != CredentialSyncReasonAuthenticationFailure {
		t.Fatalf("error kind: %q %v", reason, ok)
	}

	// Near misses: agent prose about an HTTP 401, another thing that could
	// not be refreshed, and a turn that failed for another reason.
	if AuthFailureSignature(HarnessKindCodex, "The endpoint returns 401 Unauthorized until the header is set.") {
		t.Fatal("prose about a 401")
	}
	if AuthFailureSignature(HarnessKindCodex, "The page could not be refreshed.") {
		t.Fatal("a page that could not be refreshed")
	}
	if reason, ok := credentialsSyncReason(failedTurn(`{"codexErrorInfo": "responseStreamDisconnected"}`)); ok {
		t.Fatalf("a disconnected stream: %q", reason)
	}
	if reason, ok := credentialsSyncReason(failedTurn(`{"message": "You’ve hit your usage limit.", "codexErrorInfo": "usageLimitExceeded"}`)); ok {
		t.Fatalf("a usage limit: %q", reason)
	}
}

func TestOnlyHarnessObservationsRequestCredentialSync(t *testing.T) {
	event := credentialsRelayEvent
	if !EventsReportAuthFailure(HarnessKindClaude, []RelayEvent{event(RelayObservationWarning{
		Message: "OAuth session expired and could not be refreshed",
	})}) {
		t.Fatal("a harness warning")
	}
	if !EventsReportAuthFailure(HarnessKindClaude, []RelayEvent{event(RelayObservationSessionUpdate{
		Update: acpschema.SessionUpdate{
			Kind:              acpschema.SessionUpdateKindAgentMessageChunk,
			AgentMessageChunk: &acpschema.ContentChunk{Content: credentialsTextBlock("Please run /login to continue")},
		},
	})}) {
		t.Fatal("agent text")
	}
	if !EventsReportAuthFailure(HarnessKindCodex, []RelayEvent{event(RelayObservationWarning{
		Message: PromptAuthRequiredMarker + ": codex",
	})}) {
		t.Fatal("the auth_required marker")
	}

	observedFalsePositive := "docs_src/authentication_error_status_code/tutorial001_an_py310.py"
	exitCode := uint32(0)
	if EventsReportAuthFailure(HarnessKindCodex, []RelayEvent{
		event(RelayObservationSessionUpdate{Update: acpschema.SessionUpdate{
			Kind:           acpschema.SessionUpdateKindToolCallUpdate,
			ToolCallUpdate: &acpschema.ToolCallUpdate{ToolCallID: "call-1", Title: &observedFalsePositive},
		}}),
		event(RelayObservationSessionUpdate{Update: acpschema.SessionUpdate{
			Kind:     acpschema.SessionUpdateKindToolCall,
			ToolCall: &acpschema.ToolCall{ToolCallID: "call-2", Title: observedFalsePositive},
		}}),
		event(RelayObservationSessionUpdate{Update: acpschema.SessionUpdate{
			Kind:              acpschema.SessionUpdateKindAgentThoughtChunk,
			AgentThoughtChunk: &acpschema.ContentChunk{Content: credentialsTextBlock(observedFalsePositive)},
		}}),
		event(RelayObservationSessionUpdate{Update: acpschema.SessionUpdate{
			Kind:             acpschema.SessionUpdateKindUserMessageChunk,
			UserMessageChunk: &acpschema.ContentChunk{Content: credentialsTextBlock("explain authentication_error")},
		}}),
		event(RelayObservationTerminalOutput{
			TerminalID: "terminal-1",
			Output:     observedFalsePositive,
			ExitCode:   &exitCode,
		}),
	}) {
		t.Fatal("tool titles, thoughts, user text and terminal output are not the harness reporting")
	}
	if EventsReportAuthFailure(HarnessKindClaude, []RelayEvent{event(RelayObservationCommandInterrupted{
		CommandID: "command-1",
		Command:   RelayCommandKindPrompt,
		Message:   "invalid_grant",
	})}) {
		t.Fatal("an interrupted command")
	}
	if EventsReportAuthFailure(HarnessKindClaude, []RelayEvent{event(RelayObservationCommandQueued{
		CommandID:   "command-1",
		Command:     RelayCommandPrompt{Prompt: []acpschema.ContentBlock{credentialsTextBlock("explain invalid_grant")}},
		CreatedAtMs: 1,
	})}) {
		t.Fatal("a queued prompt")
	}
	if reason, ok := credentialsSyncReason(event(RelayObservationWarning{Message: PromptEmptyResponseMarker})); !ok || reason != CredentialSyncReasonEmptyPromptResponse {
		t.Fatalf("empty response: %q %v", reason, ok)
	}
}

// A bridge that sends typed failures reports a rejected login outside a turn
// as an `access` record, not as text a signature could match.
func TestATypedLoginFailureRecordAsksForACredentialSync(t *testing.T) {
	record := func(category, title string) RelayEvent {
		meta := jsonvalue.Object{{Key: "jetbrains", Value: jsonvalue.Object{{Key: "air", Value: jsonvalue.Object{
			{Key: "version", Value: int64(1)},
			{Key: "sessionFailure", Value: jsonvalue.Object{
				{Key: "id", Value: "session:error:1"},
				{Key: "revision", Value: int64(1)},
				{Key: "category", Value: category},
				{Key: "severity", Value: "error"},
				{Key: "title", Value: title},
				{Key: "actions", Value: []any{}},
			}},
		}}}}}
		return credentialsRelayEvent(RelayObservationSessionUpdate{Update: acpschema.SessionUpdate{
			Kind:              acpschema.SessionUpdateKindSessionInfoUpdate,
			SessionInfoUpdate: &acpschema.SessionInfoUpdate{Meta: meta},
		}})
	}
	if reason, ok := credentialsSyncReason(record("access", "Sign in again.")); !ok || reason != CredentialSyncReasonAuthenticationFailure {
		t.Fatalf("access record: %q %v", reason, ok)
	}
	if reason, ok := credentialsSyncReason(record("service", "Codex is overloaded.")); ok {
		t.Fatalf("service record: %q", reason)
	}
}

const credentialsZaiConfig = "model_provider = \"zai\"\n" +
	"[model_providers.zai]\n" +
	"base_url = \"https://api.z.ai/api/v1\"\n" +
	"env_key = \"ZAI_API_KEY\"\n" +
	"wire_api = \"responses\"\n"

func TestAnAPIKeyProfileReportsThatItHasNoInteractiveLogin(t *testing.T) {
	home := t.TempDir()
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte(credentialsZaiConfig), 0o666); err != nil {
		t.Fatal(err)
	}
	profile := NewHarnessProfile(HarnessKindCodex, home)
	profile.Environment = EnvironmentFromMap(map[string]string{"ZAI_API_KEY": "key"})
	_, _, err := LoginCommand(&profile)
	if err == nil {
		t.Fatal("API-key profiles have no login")
	}
	if !strings.Contains(err.Error(), "ZAI_API_KEY") {
		t.Fatal(err)
	}
}

func TestAQueuedPeriodicSyncIsNotQueuedTwice(t *testing.T) {
	var queue []SyncTrigger
	Enqueue(&queue, SyncTrigger{ProfileID: "work"})
	Enqueue(&queue, SyncTrigger{ProfileID: "work"})
	Enqueue(&queue, SyncTrigger{ProfileID: "work", Cause: &CredentialSyncCause{
		SessionID: "session",
		Ordinal:   1,
		Reason:    CredentialSyncReasonEmptyPromptResponse,
	}})
	if len(queue) != 2 {
		t.Fatalf("queue %d", len(queue))
	}
	if queue[1].Cause == nil || queue[1].Cause.SessionID != "session" {
		t.Fatalf("second %+v", queue[1])
	}
}

func credentialsMode(t *testing.T, path string) os.FileMode {
	t.Helper()
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	return info.Mode().Perm()
}

func TestGithubTokensAreValidatedFingerprintedAndRemovedSafely(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("Unix only")
	}
	if _, err := ValidateGithubToken([]byte("")); err == nil {
		t.Fatal("empty")
	}
	if _, err := ValidateGithubToken([]byte("contains whitespace")); err == nil {
		t.Fatal("whitespace")
	}
	if _, err := ValidateGithubToken([]byte(strings.Repeat("x", MaxGithubTokenBytes+1))); err == nil {
		t.Fatal("oversized")
	}

	path := filepath.Join(t.TempDir(), "github-token")
	installed, err := WriteGithubToken(path, []byte("controller-token"))
	if err != nil {
		t.Fatal(err)
	}
	if installed != GithubTokenSnapshotOf("controller-token") {
		t.Fatalf("installed %+v", installed)
	}
	if mode := credentialsMode(t, path); mode != 0o600 {
		t.Fatalf("mode %o", mode)
	}
	state, token, err := ReadGithubToken(path)
	if err != nil {
		t.Fatal(err)
	}
	if state != installed || token == nil || *token != "controller-token" {
		t.Fatalf("read %+v %v", state, token)
	}

	if err := RemoveGithubToken(path); err != nil {
		t.Fatal(err)
	}
	if err := RemoveGithubToken(path); err != nil {
		t.Fatal(err)
	}
	if state, _, err := ReadGithubToken(path); err != nil || state != GithubTokenSnapshotAbsent() {
		t.Fatalf("read %+v %v", state, err)
	}
}

func TestClaudeSetupTokensRoundTripThroughAnOwnerOnlyProfileDirectory(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("Unix only")
	}
	if _, err := ValidateClaudeOauthToken([]byte("")); err == nil {
		t.Fatal("empty")
	}
	if _, err := ValidateClaudeOauthToken([]byte("contains whitespace")); err == nil {
		t.Fatal("whitespace")
	}
	if _, err := ValidateClaudeOauthToken([]byte(strings.Repeat("x", MaxClaudeOauthTokenBytes+1))); err == nil {
		t.Fatal("oversized")
	}

	directory := filepath.Join(t.TempDir(), "profiles", "claude")
	path := filepath.Join(directory, "claude-oauth-token")
	if _, ok, err := ReadClaudeOauthToken(path); err != nil || ok {
		t.Fatalf("absent: %v %v", ok, err)
	}

	if err := WriteClaudeOauthToken(path, []byte("sk-ant-oat01-example\n")); err != nil {
		t.Fatal(err)
	}
	if token, ok, err := ReadClaudeOauthToken(path); err != nil || !ok || token != "sk-ant-oat01-example" {
		t.Fatalf("read %q %v %v", token, ok, err)
	}
	if mode := credentialsMode(t, path); mode != 0o600 {
		t.Fatalf("file mode %o", mode)
	}
	if mode := credentialsMode(t, directory); mode != 0o700 {
		t.Fatalf("directory mode %o", mode)
	}

	// Rotating the token replaces the file in place.
	if err := WriteClaudeOauthToken(path, []byte("sk-ant-oat01-second")); err != nil {
		t.Fatal(err)
	}
	if token, ok, err := ReadClaudeOauthToken(path); err != nil || !ok || token != "sk-ant-oat01-second" {
		t.Fatalf("read %q %v %v", token, ok, err)
	}
}

func TestClaudeSetupTokenReadsAndWritesRefuseSymlinkDestinations(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("Unix only")
	}
	root := t.TempDir()
	elsewhere := filepath.Join(root, "elsewhere")
	if err := os.WriteFile(elsewhere, []byte("keep"), 0o666); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(root, "claude-oauth-token")
	if err := os.Symlink(elsewhere, path); err != nil {
		t.Fatal(err)
	}
	if WriteClaudeOauthToken(path, []byte("sk-ant-oat01-example")) == nil {
		t.Fatal("write through a symlink")
	}
	if _, _, err := ReadClaudeOauthToken(path); err == nil {
		t.Fatal("read through a symlink")
	}
	if body, _ := os.ReadFile(elsewhere); string(body) != "keep" {
		t.Fatalf("elsewhere %q", body)
	}
}

func TestAProfileSetupTokenLivesBesideTheConfigurationNotInTheProfileHome(t *testing.T) {
	path := ClaudeOauthTokenPath("claude-max")
	if !strings.HasSuffix(path, filepath.Join("profiles", "claude-max", "claude-oauth-token")) {
		t.Fatalf("path %s", path)
	}
	if !strings.HasPrefix(path, ConfigDir()+string(filepath.Separator)) {
		t.Fatalf("path %s outside %s", path, ConfigDir())
	}
}

func TestGithubTokenInstallAndRemoveRefuseSymlinkDestinations(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("Unix only")
	}
	root := t.TempDir()
	elsewhere := filepath.Join(root, "elsewhere")
	if err := os.WriteFile(elsewhere, []byte("keep"), 0o666); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(root, "github-token")
	if err := os.Symlink(elsewhere, path); err != nil {
		t.Fatal(err)
	}
	if _, err := WriteGithubToken(path, []byte("controller-token")); err == nil {
		t.Fatal("write through a symlink")
	}
	if RemoveGithubToken(path) == nil {
		t.Fatal("remove through a symlink")
	}
	if body, _ := os.ReadFile(elsewhere); string(body) != "keep" {
		t.Fatalf("elsewhere %q", body)
	}
}

func credentialsRefusal(rejected *RejectedLogins, profileID string) *string {
	if reason, ok := rejected.Refusal(profileID); ok {
		return &reason
	}
	return nil
}

// #1160: after the first child of a profile failed to sign in, ten more
// children were spawned on it and died the same way. A sync that answers an
// auth failure with nothing fresher to push shows the profile's own login is
// the one refused, and it stays refused until the login file changes.
// Hard-won: 1b077959: ten more children reused a profile after its login had already been refused
func TestARefusedLoginIsKnownUntilTheLoginFileChanges(t *testing.T) {
	home := t.TempDir()
	marker := HarnessAuthenticationMarker(HarnessKindCodex, home)
	if err := os.WriteFile(marker, codexCredentials(t, "2026-09-25T20:00:00.000Z"), 0o666); err != nil {
		t.Fatal(err)
	}
	profile := NewHarnessProfile(HarnessKindCodex, home)
	result := func(reason CredentialSyncReason, outcomes []CredentialSyncOutcome, failure *string) *CredentialSyncResult {
		return &CredentialSyncResult{
			ProfileID: "codex4",
			Trigger:   &CredentialSyncCause{SessionID: "child-1", Ordinal: 1, Reason: reason},
			Failure:   failure,
			Outcomes:  outcomes,
		}
	}
	matched, _, err := ReadCredentialFile(HarnessKindCodex, marker)
	if err != nil {
		t.Fatal(err)
	}
	reached := func(actions []CredentialSyncAction, failure *string) []CredentialSyncOutcome {
		unchanged := matched
		return []CredentialSyncOutcome{{
			UnchangedLogin: &unchanged,
			SessionID:      "child-1",
			Actions:        actions,
			Failure:        failure,
		}}
	}
	pushed := func() []CredentialSyncOutcome {
		return reached([]CredentialSyncAction{CredentialSyncActionPushed}, nil)
	}
	// The session was reconciled, and its copy already matched the profile's.
	nothingPushed := func() []CredentialSyncOutcome {
		return reached([]CredentialSyncAction{CredentialSyncActionSkillsPushed}, nil)
	}
	var rejected RejectedLogins
	// A fresher login was pushed to the session: the profile's own is fine.
	rejected.Observe(result(CredentialSyncReasonAuthenticationFailure, pushed(), nil), &profile)
	if refusal := credentialsRefusal(&rejected, "codex4"); refusal != nil {
		t.Fatalf("pushed: %s", *refusal)
	}
	// The sync failed, or never reached the session, so it compared nothing.
	rejected.Observe(result(CredentialSyncReasonAuthenticationFailure, nil, credentialsString("sync task stopped")), &profile)
	rejected.Observe(result(CredentialSyncReasonAuthenticationFailure, reached(nil, credentialsString("worker unreachable")), nil), &profile)
	rejected.Observe(result(CredentialSyncReasonAuthenticationFailure, nil, nil), &profile)
	// The session's whole sync failed (its connection) although an AWS
	// result was merged beside it: nothing was compared either.
	unchanged := matched
	rejected.Observe(result(CredentialSyncReasonAuthenticationFailure, []CredentialSyncOutcome{{
		UnchangedLogin: &unchanged,
		SessionID:      "child-1",
		Actions:        []CredentialSyncAction{CredentialSyncActionSkillsPushed},
		StepFailures:   []StepFailure{{Step: SyncStepSession, Detail: "worker unreachable"}},
	}}, nil), &profile)
	if refusal := credentialsRefusal(&rejected, "codex4"); refusal != nil {
		t.Fatalf("nothing compared: %s", *refusal)
	}
	// An empty answer is not an auth failure.
	rejected.Observe(result(CredentialSyncReasonEmptyPromptResponse, nothingPushed(), nil), &profile)
	if refusal := credentialsRefusal(&rejected, "codex4"); refusal != nil {
		t.Fatalf("empty answer: %s", *refusal)
	}

	// Nothing fresher to push: the profile's login is the one refused.
	rejected.Observe(result(CredentialSyncReasonAuthenticationFailure, nothingPushed(), nil), &profile)
	refusal := credentialsRefusal(&rejected, "codex4")
	if refusal == nil || *refusal != "the login is no longer valid; run `mj login --profile codex4` and spawn again" {
		t.Fatalf("refusal %v", refusal)
	}
	if refusal := credentialsRefusal(&rejected, "codex3"); refusal != nil {
		t.Fatalf("another profile: %s", *refusal)
	}

	// `mj login` rewrites the file.
	if err := os.WriteFile(marker, codexCredentials(t, "2026-09-25T22:49:05.000Z"), 0o666); err != nil {
		t.Fatal(err)
	}
	if refusal := credentialsRefusal(&rejected, "codex4"); refusal != nil {
		t.Fatalf("after login: %s", *refusal)
	}

	// A profile that signs in with an API key has no login to redo.
	provider := t.TempDir()
	if err := os.WriteFile(filepath.Join(provider, "config.toml"), []byte(credentialsZaiConfig), 0o666); err != nil {
		t.Fatal(err)
	}
	apiKey := profile
	apiKey.Home = provider
	var apiKeyRejected RejectedLogins
	apiKeyRejected.Observe(result(CredentialSyncReasonAuthenticationFailure, nothingPushed(), nil), &apiKey)
	if refusal := credentialsRefusal(&apiKeyRejected, "codex4"); refusal != nil {
		t.Fatalf("API key: %s", *refusal)
	}
}

// Hard-won: #1547: a delayed auth signal marked a renewed Claude login refused.
func TestAClaudeRefusalIsRecordedForTheFailedTokenOnlyWhileItIsCurrent(t *testing.T) {
	home := t.TempDir()
	marker := HarnessAuthenticationMarker(HarnessKindClaude, home)
	login := func(token string) []byte {
		return credentialsCompact(t, fmt.Sprintf(`{"claudeAiOauth": {"accessToken": %q, "expiresAt": 1}}`, token))
	}
	fingerprint := func(token string) string {
		value, ok := ClaudeAccessTokenFingerprint(login(token))
		if !ok {
			t.Fatal("a token fingerprint")
		}
		return value
	}
	if err := os.WriteFile(marker, login("renewed"), 0o666); err != nil {
		t.Fatal(err)
	}
	profile := NewHarnessProfile(HarnessKindClaude, home)
	// What a sync of the failed session saw: its copy already equals the
	// profile's, so there is nothing to push.
	observed := func(failed *string) *CredentialSyncResult {
		snapshot, bytes, err := ReadCredentialFile(HarnessKindClaude, marker)
		if err != nil {
			t.Fatal(err)
		}
		cause := &CredentialSyncCause{SessionID: "child", Ordinal: 1, Reason: CredentialSyncReasonAuthenticationFailure}
		if failed != nil {
			cause.LoginFingerprint = credentialsString(fingerprint(*failed))
		}
		outcome := CredentialSyncOutcome{SessionID: "child", UnchangedLogin: &snapshot, Actions: []CredentialSyncAction{}}
		if current, ok := ClaudeAccessTokenFingerprint(bytes); ok {
			outcome.CurrentAccessToken = &current
		}
		return &CredentialSyncResult{ProfileID: "claude6", Trigger: cause, Outcomes: []CredentialSyncOutcome{outcome}}
	}
	var rejected RejectedLogins
	// The turn failed on a token that was renewed before the sync ran.
	stale := observed(credentialsString("revoked"))
	if verdict, ok := stale.AuthenticationVerdict(); !ok || verdict != AuthenticationVerdictLoginRenewed {
		t.Fatalf("verdict %v %v", verdict, ok)
	}
	rejected.Observe(stale, &profile)
	if refusal := credentialsRefusal(&rejected, "claude6"); refusal != nil {
		t.Fatalf("stale: %s", *refusal)
	}
	// A failure that names no token proves nothing about the current login.
	rejected.Observe(observed(nil), &profile)
	if refusal := credentialsRefusal(&rejected, "claude6"); refusal != nil {
		t.Fatalf("no token: %s", *refusal)
	}

	// The current token itself was refused.
	rejected.Observe(observed(credentialsString("renewed")), &profile)
	if credentialsRefusal(&rejected, "claude6") == nil {
		t.Fatal("the current token was refused")
	}
	// A later renewal ends it without anything else being recorded.
	if err := os.WriteFile(marker, login("renewed again"), 0o666); err != nil {
		t.Fatal(err)
	}
	if refusal := credentialsRefusal(&rejected, "claude6"); refusal != nil {
		t.Fatalf("renewed: %s", *refusal)
	}
	// A late signal for the first refused token does not bring it back.
	rejected.Observe(observed(credentialsString("renewed")), &profile)
	if refusal := credentialsRefusal(&rejected, "claude6"); refusal != nil {
		t.Fatalf("late signal: %s", *refusal)
	}
}

// Hard-won: #1485: observing an old failed sync marked a refreshed host login refused.
func TestARefusedLoginObservationCannotRejectALaterHostRefresh(t *testing.T) {
	home := t.TempDir()
	marker := HarnessAuthenticationMarker(HarnessKindCodex, home)
	previous := codexCredentials(t, "2026-09-25T20:00:00.000Z")
	if err := os.WriteFile(marker, previous, 0o666); err != nil {
		t.Fatal(err)
	}
	observed := CredentialSnapshotOf(HarnessKindCodex, previous)
	result := &CredentialSyncResult{
		ProfileID: "codex4",
		Trigger:   &CredentialSyncCause{SessionID: "failed-session", Ordinal: 1, Reason: CredentialSyncReasonAuthenticationFailure},
		Outcomes: []CredentialSyncOutcome{{
			SessionID:      "failed-session",
			UnchangedLogin: &observed,
			Actions:        []CredentialSyncAction{},
		}},
	}
	// The host refreshes after the sync made its observation, before
	// delivery.
	if err := os.WriteFile(marker, codexCredentials(t, "2026-09-25T22:49:05.000Z"), 0o666); err != nil {
		t.Fatal(err)
	}
	profile := NewHarnessProfile(HarnessKindCodex, home)
	var rejected RejectedLogins
	rejected.Observe(result, &profile)
	if refusal := credentialsRefusal(&rejected, "codex4"); refusal != nil {
		t.Fatal(*refusal)
	}
}

// #1132: Kimi's refusal of its stored OAuth token, as its journal states it:
// the turn's error message, and the agent's error line with the error name.
// A sub-agent child whose turn failed this way failed on its login.
// Hard-won: 8d4f80ad: Kimi journalled an OAuth refusal but Mjolnir reported prompt_unanswered and suggested resending
func TestAKimiTokenRefusalIsAnAuthFailure(t *testing.T) {
	const message = "Stored token for \"kimi-code\" was rejected; re-login required."
	if !AuthFailureSignature(HarnessKindKimi, message) {
		t.Fatal("the message")
	}
	if !AuthFailureSignature(HarnessKindKimi, "OAuthUnauthorizedError: "+message) {
		t.Fatal("the error line")
	}
	provider, err := jsonvalue.Parse(credentialsCompact(t, fmt.Sprintf(`{
		"code": "internal",
		"message": %q,
		"name": "OAuthUnauthorizedError",
		"retryable": false
	}`, message)))
	if err != nil {
		t.Fatal(err)
	}
	diagnostic := TurnDiagnosticFromProvider(provider)
	if diagnostic == nil {
		t.Fatal("a diagnostic")
	}
	if !TurnDiagnosticReportsAuthFailure(diagnostic) {
		t.Fatal("the diagnostic reports an auth failure")
	}
	accepted, start := uint64(1), uint64(1)
	turn := MaterializedTurnOutcome{
		Diagnostic:        diagnostic,
		CommandID:         "prompt-1",
		AcceptedOrdinal:   &accepted,
		TurnStartPosition: &start,
		CompletedOrdinal:  2,
		CompletedAtMs:     2,
		Outcome:           TurnOutcomeKindCompleted{StopReason: "error"},
	}
	if !TurnFailedOnLogin(&turn) {
		t.Fatal("the turn failed on its login")
	}
	if AuthFailureSignature(HarnessKindKimi, "The token was rejected by the linter; see the log.") {
		t.Fatal("a linter's rejection")
	}
}

// claudeLogin is a Claude login with the given expiry whose refresh token is
// refresh, or none, as a harness copy holds it.
func claudeLogin(t *testing.T, expiresAt int64, refresh *string) []byte {
	parsed, err := jsonvalue.Parse(claudeCredentials(t, expiresAt))
	if err != nil {
		t.Fatal(err)
	}
	document := parsed.(jsonvalue.Object)
	member, _ := document.Get("claudeAiOauth")
	oauth := member.(jsonvalue.Object)
	if refresh != nil {
		oauth.Set("refreshToken", *refresh)
	} else {
		oauth.SwapDelete("refreshToken")
	}
	document.Set("claudeAiOauth", oauth)
	encoded, err := jsonvalue.Marshal(document)
	if err != nil {
		t.Fatal(err)
	}
	return encoded
}

func TestAClaudeHarnessCopyOmitsTheRefreshTokenAndNothingElse(t *testing.T) {
	host := claudeLogin(t, 1_000, credentialsString("host-refresh"))
	copied := CredentialCopyForHarness(HarnessKindClaude, host)
	parsed, err := jsonvalue.Parse(copied)
	if err != nil {
		t.Fatal(err)
	}
	if _, ok := jvPointer(parsed, "/claudeAiOauth/refreshToken"); ok {
		t.Fatal("the copy holds no refresh token")
	}
	if token, _ := jvString(jvPointer(parsed, "/claudeAiOauth/accessToken")); token != "access" {
		t.Fatalf("access token %q", token)
	}
	if expiry, _ := jvInt64(jvPointer(parsed, "/claudeAiOauth/expiresAt")); expiry != 1_000 {
		t.Fatalf("expiry %d", expiry)
	}
	if scope, _ := jvString(jvPointer(parsed, "/claudeAiOauth/scopes/0")); scope != "user:inference" {
		t.Fatalf("scope %q", scope)
	}
	// The copy is stable, so equal inputs have equal fingerprints.
	if string(copied) != string(CredentialCopyForHarness(HarnessKindClaude, host)) {
		t.Fatal("the copy is stable")
	}

	// `""` means "login ended" to Claude Code, so it is never written, and an
	// existing one is not turned into a missing key.
	ended := claudeLogin(t, 1_000, credentialsString(""))
	if string(CredentialCopyForHarness(HarnessKindClaude, ended)) != string(ended) {
		t.Fatal("an ended login is unchanged")
	}
	// Other harnesses and unreadable documents keep their bytes.
	codex := codexCredentials(t, "2026-01-01T00:00:00Z")
	if string(CredentialCopyForHarness(HarnessKindCodex, codex)) != string(codex) {
		t.Fatal("Codex is unchanged")
	}
	if string(CredentialCopyForHarness(HarnessKindClaude, []byte("synthetic credentials"))) != "synthetic credentials" {
		t.Fatal("an unreadable document is unchanged")
	}
}

// #1490: a copy without the refresh token never takes the profile's login
// from it, however late it expires, and a copy staged before that rule, which
// still holds the refresh token, is replaced at the same expiry.
func TestClaudeInstallsNeverDropTheHostsRefreshTokenAndConvergeCopies(t *testing.T) {
	ctx := context.Background()
	directory := t.TempDir()
	host := filepath.Join(directory, "host", ".credentials.json")
	session := filepath.Join(directory, "session", ".credentials.json")
	for _, path := range []string{host, session} {
		if err := os.MkdirAll(filepath.Dir(path), 0o777); err != nil {
			t.Fatal(err)
		}
	}
	read := func(path string) string {
		t.Helper()
		body, err := os.ReadFile(path)
		if err != nil {
			t.Fatal(err)
		}
		return string(body)
	}
	install := func(path string, bytes []byte, session bool) CredentialInstall {
		t.Helper()
		var outcome CredentialInstall
		var err error
		if session {
			outcome, err = InstallSessionCredentials(ctx, HarnessKindClaude, path, bytes)
		} else {
			outcome, err = InstallCredentialsIfNewer(ctx, HarnessKindClaude, path, bytes)
		}
		if err != nil {
			t.Fatal(err)
		}
		return outcome
	}
	withToken := claudeLogin(t, 1_000, credentialsString("host-refresh"))
	stripped := CredentialCopyForHarness(HarnessKindClaude, withToken)
	if err := os.WriteFile(host, withToken, 0o666); err != nil {
		t.Fatal(err)
	}

	// (b) A later-expiring copy without the token does not replace the host.
	laterStripped := claudeLogin(t, 9_000, nil)
	if install(host, laterStripped, false).Installed {
		t.Fatal("a stripped copy replaced the host")
	}
	if read(host) != string(withToken) {
		t.Fatal("the host keeps its login")
	}
	// A later copy that carries its own login does.
	ownLogin := claudeLogin(t, 9_000, credentialsString("session-refresh"))
	if !install(host, ownLogin, false).Installed {
		t.Fatal("a later login replaces the host")
	}
	if err := os.WriteFile(host, withToken, 0o666); err != nil {
		t.Fatal(err)
	}
	// A host without a login (after a logout) is not handed a stripped copy
	// nothing can renew; a copy with its own login still signs it in.
	signedOut := filepath.Join(directory, "signed-out", ".credentials.json")
	if install(signedOut, laterStripped, false).Installed {
		t.Fatal("a stripped copy signed a host in")
	}
	if _, err := os.Stat(signedOut); err == nil {
		t.Fatal("nothing was written")
	}
	if !install(signedOut, ownLogin, false).Installed {
		t.Fatal("a copy with its own login signs the host in")
	}

	// (a) A copy holding the host's refresh token at the same expiry is
	// replaced by the stripped copy, but only under the session-copy rule.
	if err := os.WriteFile(session, withToken, 0o666); err != nil {
		t.Fatal(err)
	}
	if install(session, stripped, false).Installed {
		t.Fatal("the canonical rule does not strip a copy")
	}
	if !install(session, stripped, true).Installed {
		t.Fatal("the session rule strips a copy")
	}
	if read(session) != string(stripped) {
		t.Fatal("the session holds the stripped copy")
	}

	// An older delivery never replaces a copy that expires later (it may be a
	// login of its own that the daemon has yet to pull), nor does a delivery
	// that carries a token at the same expiry.
	newerOwn := claudeLogin(t, 2_000, credentialsString("session-refresh"))
	if err := os.WriteFile(session, newerOwn, 0o666); err != nil {
		t.Fatal(err)
	}
	for _, incoming := range [][]byte{stripped, withToken} {
		if install(session, incoming, true).Installed {
			t.Fatal("an older delivery replaced a later copy")
		}
		if read(session) != string(newerOwn) {
			t.Fatal("the session keeps its later copy")
		}
	}
}

// Hard-won: d633dfdbd: signing in a custom Antigravity home used the default account.
func TestAntigravityLoginKeepsTheDefaultAccountUntouched(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("Unix only")
	}
	root := t.TempDir()
	defaultHome := filepath.Join(root, ".gemini")
	selected := filepath.Join(root, "other Google account")
	marker := HarnessKindAntigravity.CredentialFileName()
	if err := os.MkdirAll(filepath.Join(defaultHome, "antigravity-cli"), 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(defaultHome, marker), []byte("default-account"), 0o666); err != nil {
		t.Fatal(err)
	}
	// A CLI-side fake uses agy's documented argument, and otherwise its
	// default home; the Mjolnir-only environment variable cannot select it.
	shim := filepath.Join(root, "agy")
	if err := os.WriteFile(shim, []byte(`#!/bin/sh
selected="$HOME/.gemini"
while [ "$#" -gt 0 ]; do
    case "$1" in --gemini-dir) shift; selected="$1" ;; esac
    shift
done
mkdir -p "$selected/antigravity-cli"
printf signed-in > "$selected/antigravity-cli/antigravity-oauth-token"
`), 0o755); err != nil {
		t.Fatal(err)
	}
	profile := NewHarnessProfile(HarnessKindAntigravity, selected)
	program, args, err := LoginCommand(&profile)
	if err != nil {
		t.Fatal(err)
	}
	command := exec.Command(filepath.Join(root, program), args...)
	command.Env = append(os.Environ(), "HOME="+root)
	if _, err := RunInherited(command); err != nil {
		t.Fatal(err)
	}
	if !command.ProcessState.Success() {
		t.Fatalf("status %v", command.ProcessState)
	}
	if body, _ := os.ReadFile(filepath.Join(defaultHome, marker)); string(body) != "default-account" {
		t.Fatalf("default account %q", body)
	}
	if body, _ := os.ReadFile(filepath.Join(selected, marker)); string(body) != "signed-in" {
		t.Fatalf("selected account %q", body)
	}
}

// Hard-won: #1581: registration could restore Pending after the worker's matching report.
func TestDeliveryRegistrationConsumesAMatchingHeldLoginInEitherOrder(t *testing.T) {
	target := CredentialSyncTarget{
		SessionID:   "child",
		ProfileID:   "helper",
		Harness:     HarnessKindClaude,
		ProfileHome: "profile",
		SkillsScope: SkillsScopeLocalhost,
		Spec:        NewCommandSpec("unused", []string{}),
	}
	const current = "renewed"
	for _, reportFirst := range []bool{false, true} {
		var rejected RejectedLogins
		profile, session := target.ProfileID, target.SessionID
		publication := func(held string) *WorkerPublication {
			value := NewWorkerPublication(&held, nil, 0)
			return &value
		}
		rejected.ObserveWorker(profile, session, publication("old"))
		if reportFirst {
			// Reproduce the order in which publication won the old race.
			// There is no second worker report to undo registration.
			rejected.ObserveWorker(profile, session, publication(current))
		}
		rejected.StartDelivery(&target, current)
		if !reportFirst {
			if state := rejected.DeliveryState(profile, session); state != LoginDeliveryState(LoginDeliveryStatePending{}) {
				t.Fatalf("state %#v", state)
			}
			rejected.ObserveWorker(profile, session, publication(current))
		}
		if held, ok := rejected.HeldLoginFingerprint(profile, session); !ok || held != current {
			t.Fatalf("held %q %v", held, ok)
		}
		if _, ok := rejected.DeliveryState(profile, session).(LoginDeliveryStateConcluded); !ok {
			t.Fatalf("state %#v", rejected.DeliveryState(profile, session))
		}
		// A later attempt after the actor retires needs fresh evidence.
		rejected.RetainHeldLogins(func(string) bool { return false })
		rejected.StartDelivery(&target, current)
		if state := rejected.DeliveryState(profile, session); state != LoginDeliveryState(LoginDeliveryStatePending{}) {
			t.Fatalf("state %#v", state)
		}
	}
}

func claudeLoginWithToken(t *testing.T, token string) []byte {
	return credentialsCompact(t, fmt.Sprintf(`{
		"claudeAiOauth": {
			"accessToken": %q,
			"refreshToken": "refresh",
			"expiresAt": 4000000000000
		}
	}`, token))
}

// Hard-won: #1600: a failure during a renewal was judged against the login being replaced and called refused
func TestARejectedLoginIsJudgedAgainstTheLoginTheRenewerWrites(t *testing.T) {
	ctx := context.Background()
	home := t.TempDir()
	marker := HarnessAuthenticationMarker(HarnessKindClaude, home)
	var store HarnessLoginStore = HarnessLoginStoreFile{Marker: marker}
	if err := os.WriteFile(marker, claudeLoginWithToken(t, "old"), 0o666); err != nil {
		t.Fatal(err)
	}
	rejected, _ := ClaudeAccessTokenFingerprint(claudeLoginWithToken(t, "old"))
	// The decision: is the rejected token still the file's current one?
	decide := func(wait time.Duration) *bool {
		decision, ok := ReadLoginUnderNativeLock(ctx, HarnessKindClaude, &store, wait, func() *bool {
			current, err := LoginFingerprintReadFile(HarnessKindClaude, marker)
			token, isToken := current.(LoginFingerprintAccessToken)
			if err != nil || !isToken {
				return nil
			}
			same := token.Value == rejected
			return &same
		})
		if !ok {
			return nil
		}
		return decision
	}

	// Another daemon is renewing: it holds the native lock until the new
	// login is written.
	renewer, err := CredentialWriteGuardAcquire(ctx, HarnessKindClaude, marker)
	if err != nil {
		t.Fatal(err)
	}
	// While the lock is held, the answer is unknown, not "refused".
	if decision := decide(200 * time.Millisecond); decision != nil {
		t.Fatalf("decision %v while the renewer holds the lock", *decision)
	}
	decision := Spawn(func() *bool { return decide(10 * time.Second) })
	time.Sleep(300 * time.Millisecond)
	if err := os.WriteFile(marker, claudeLoginWithToken(t, "new"), 0o666); err != nil {
		t.Fatal(err)
	}
	renewer.Release()

	answer, err := decision.Join(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if answer == nil || *answer {
		t.Fatalf("answer %v", answer)
	}
}
