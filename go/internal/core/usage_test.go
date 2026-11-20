// Port of the tests in mj-core/src/usage.rs.

package core

import (
	"testing"

	acp "github.com/BrokkAi/acp-go/schema/unstable"
)

func usageTestReport() acp.Usage {
	return acp.Usage{TotalTokens: 100, InputTokens: 80, OutputTokens: 20}
}

func usageTestDeclared(harness HarnessKind, scope string) UsageScope {
	report := usageTestReport()
	report.Meta = acp.NewMeta(map[string]any{"mjolnir.dev/usage-scope": scope})
	return TokenUsageFromACP(harness, report).Scope
}

func TestCodexScopeDistinguishesCompletePartialAndLegacyReports(t *testing.T) {
	if legacy := TokenUsageFromACP(HarnessKindCodex, usageTestReport()); legacy.Scope != UsageScopeLastRequest {
		t.Fatal(legacy.Scope)
	}
	for _, c := range []struct {
		declared string
		expected UsageScope
	}{
		{"turn", UsageScopeTurn},
		{"unspecified", UsageScopeUnspecified},
		{"future_scope", UsageScopeUnspecified},
	} {
		report := usageTestReport()
		report.Meta = acp.NewMeta(map[string]any{"mjolnir.dev/usage-scope": c.declared})
		result := TokenUsageFromACP(HarnessKindCodex, report)
		if result.Scope != c.expected || result.TotalTokens != 100 {
			t.Fatalf("%s: got %+v", c.declared, result)
		}
	}
}

func TestMuseCountsAWholeTurnOnlyWhenTheAdapterDeclaresIt(t *testing.T) {
	if got := TokenUsageFromACP(HarnessKindMuse, usageTestReport()).Scope; got != UsageScopeUnspecified {
		t.Fatal(got)
	}
	for scope, want := range map[string]UsageScope{
		"turn":         UsageScopeTurn,
		"unspecified":  UsageScopeUnspecified,
		"future_scope": UsageScopeUnspecified,
	} {
		if got := usageTestDeclared(HarnessKindMuse, scope); got != want {
			t.Fatalf("%s: got %s", scope, got)
		}
	}
}

func TestADeclaredScopeOverridesTheClaudeDefault(t *testing.T) {
	if got := usageTestDeclared(HarnessKindClaude, "turn"); got != UsageScopeTurn {
		t.Fatal(got)
	}
	if got := usageTestDeclared(HarnessKindClaude, "unspecified"); got != UsageScopeUnspecified {
		t.Fatal(got)
	}
}
