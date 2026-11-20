// Port of the tests in mj-core/src/diagnostic.rs.

package core

import (
	"strings"
	"testing"

	acp "github.com/BrokkAi/acp-go"
	"github.com/BrokkAi/acp-go/jsonvalue"
)

// diagnosticInternalError is the ACP crate's `Error::internal_error().data(data)`.
func diagnosticInternalError(data string) *acp.RPCError {
	return &acp.RPCError{Code: -32603, Message: "Internal error", Data: []byte(data)}
}

func diagnosticParse(t *testing.T, text string) any {
	t.Helper()
	value, err := jsonvalue.Parse([]byte(text))
	if err != nil {
		t.Fatal(err)
	}
	return value
}

// Launch finding J-25: Codex reports an exhausted plan as a JSON-RPC internal
// error whose data names `codexErrorInfo: usageLimitExceeded`. That is a
// usage limit, whatever its sentence says, and the sentence is the
// diagnostic's message.
// Hard-won: cd2f1b6d: Codex quota failures appeared as raw JSON and repeated warnings
func TestACodexUsageLimitErrorIsAUsageLimit(t *testing.T) {
	diagnostic := TurnDiagnosticFromACP(diagnosticInternalError(`{
		"message": "You’ve hit your usage limit. Visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again at Sep 29th, 2026 10:20 PM.",
		"codexErrorInfo": "usageLimitExceeded"
	}`))
	if !diagnostic.IsUsageLimit() {
		t.Fatalf("%+v", diagnostic)
	}
	if !strings.HasPrefix(diagnostic.Message, "You’ve hit your usage limit") {
		t.Fatalf("message %q", diagnostic.Message)
	}
	if diagnostic.Code == nil || *diagnostic.Code != "usageLimitExceeded" {
		t.Fatalf("code %v", diagnostic.Code)
	}

	other := TurnDiagnosticFromACP(diagnosticInternalError(`{
		"message": "stream disconnected before completion",
		"codexErrorInfo": "responseStreamDisconnected"
	}`))
	if other.IsUsageLimit() {
		t.Fatal("a disconnected stream is not a usage limit")
	}
}

func TestOnlyExplicitExhaustionIsQuotaAndDetailsSurvive(t *testing.T) {
	diagnostic := TurnDiagnosticFromProvider(diagnosticParse(t, `{
		"code":"provider.auth_error", "message":"You have reached your five-hour usage limit. Resets at 23:00 UTC.",
		"details":{"statusCode":403,"resetAt":"23:00 UTC"}
	}`))
	if diagnostic == nil {
		t.Fatal("a diagnostic")
	}
	if !diagnostic.IsUsageLimit() {
		t.Fatal("explicit exhaustion is a usage limit")
	}
	if diagnostic.HTTPStatus == nil || *diagnostic.HTTPStatus != 403 {
		t.Fatalf("status %v", diagnostic.HTTPStatus)
	}
	if diagnostic.ResetAt == nil || *diagnostic.ResetAt != "23:00 UTC" {
		t.Fatalf("reset %v", diagnostic.ResetAt)
	}
	for _, message := range []string{
		"Forbidden",
		"Invalid API key",
		"Unable to read quota",
		"Usage limit service unavailable",
	} {
		ordinary := *diagnostic
		ordinary.Message = message
		if ordinary.IsUsageLimit() {
			t.Fatal(message)
		}
	}
}

func diagnosticRecord(t *testing.T, category string, actions []string, title string) *SessionFailure {
	t.Helper()
	actionValues := make([]any, len(actions))
	for index, action := range actions {
		actionValues[index] = action
	}
	failure := jsonvalue.Object{
		{Key: "id", Value: "turn:error"},
		{Key: "revision", Value: int64(1)},
		{Key: "category", Value: category},
		{Key: "severity", Value: "error"},
		{Key: "title", Value: title},
		{Key: "actions", Value: actionValues},
	}
	meta := jsonvalue.Object{
		{Key: "quota", Value: jsonvalue.Object{}},
		{Key: "jetbrains", Value: jsonvalue.Object{{Key: "air", Value: jsonvalue.Object{
			{Key: "version", Value: int64(1)},
			{Key: "sessionFailure", Value: failure},
		}}}},
	}
	record := SessionFailureFromMeta(meta)
	if record == nil {
		t.Fatal("a failure record")
	}
	return record
}

// Issue 1217: the failed turn's record, as codex-acp sends it.
// Hard-won: 03e1b5ac: Issue 1217 made a failed Codex sub-agent turn count as finished
func TestATypedProviderFailureKeepsTheProvidersSentenceAndStatus(t *testing.T) {
	failure := diagnosticRecord(t, "service", []string{"retry"},
		`{"type":"error","error":{"message":"model 'gpt-6-luna' is not enabled in rustponsesapi","type":"invalid_request_error","param":null,"code":null},"status":400}`)
	if !failure.IsError() {
		t.Fatal("an error record")
	}
	diagnostic := TurnDiagnosticFromSessionFailure(failure)
	if diagnostic.Message != "model 'gpt-6-luna' is not enabled in rustponsesapi" {
		t.Fatalf("message %q", diagnostic.Message)
	}
	if diagnostic.HTTPStatus == nil || *diagnostic.HTTPStatus != 400 {
		t.Fatalf("status %v", diagnostic.HTTPStatus)
	}
	if diagnostic.Code == nil || *diagnostic.Code != "service" {
		t.Fatalf("code %v", diagnostic.Code)
	}
	if diagnostic.IsUsageLimit() {
		t.Fatal("not a usage limit")
	}
}

// The record drops Codex's error kind; the two kinds Mjolnir acts on come
// back from the bridge's category and actions.
// Hard-won: 03e1b5ac: Issue 1217 made a failed Codex sub-agent turn count as finished
func TestATypedUsageLimitOrLoginFailureKeepsTheKindMjolnirActsOn(t *testing.T) {
	exhausted := TurnDiagnosticFromSessionFailure(diagnosticRecord(t, "limit", nil, "You've hit your usage limit."))
	if !exhausted.IsUsageLimit() {
		t.Fatal("exhausted is a usage limit")
	}
	throttled := TurnDiagnosticFromSessionFailure(diagnosticRecord(t, "limit", []string{"retry"}, "Rate limit reached for requests."))
	if throttled.IsUsageLimit() {
		t.Fatal("a rate limit is not a usage limit")
	}
	login := TurnDiagnosticFromSessionFailure(diagnosticRecord(t, "access", []string{"login"}, "Your access token could not be refreshed."))
	if !TurnDiagnosticReportsAuthFailure(&login) {
		t.Fatal("a login failure")
	}
}

func TestAWarningRecordIsNotAFailedTurnAndOtherMetaIsNotARecord(t *testing.T) {
	meta := diagnosticParse(t, `{"jetbrains": {"air": {"sessionFailure": {
		"id": "turn:error", "category": "connection", "severity": "warning",
		"title": "Reconnecting... 1/5", "actions": []}}}}`).(jsonvalue.Object)
	record := SessionFailureFromMeta(meta)
	if record == nil || record.IsError() {
		t.Fatalf("a warning record: %+v", record)
	}
	goal := diagnosticParse(t, `{"jetbrains": {"air": {"goal": null}}}`).(jsonvalue.Object)
	if SessionFailureFromMeta(goal) != nil {
		t.Fatal("other meta is not a record")
	}
	if SessionFailureFromMeta(nil) != nil {
		t.Fatal("no meta is not a record")
	}
}
