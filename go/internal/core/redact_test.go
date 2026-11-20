// Port of the tests in mj-core/src/redact.rs.

package core

import "testing"

func TestRedactionRemovesTokenRunsAndKeepsOrdinaryText(t *testing.T) {
	got := RedactSecrets("Bearer eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiIxMjM0In0.c2lnbmF0dXJl, key sk-proj-ABCDEFGHIJKLMNOPQRSTUVWX")
	if got != "Bearer [redacted], key [redacted]" {
		t.Fatalf("got %q", got)
	}
	ordinary := "2026-10-05T12:00:00Z WARN codex_login::auth: POST https://auth.openai.com/oauth/token returned 401"
	if got := RedactSecrets(ordinary); got != ordinary {
		t.Fatalf("got %q", got)
	}
}
