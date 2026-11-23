// Port of mj-worker/src/acp/muse_usage.rs: its tests module.

package worker

import (
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"
	acpunstable "github.com/BrokkAi/acp-go/schema/unstable"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func museUsageStandard(t *testing.T) core.TokenUsage {
	report := acpunstable.Usage{TotalTokens: 130, InputTokens: 100, OutputTokens: 30}
	report.Meta = museUsageMeta(t, `{"mjolnir.dev/usage-scope": "turn"}`)
	return core.TokenUsageFromACP(core.HarnessKindMuse, report)
}

func museUsageMeta(t *testing.T, text string) jsonvalue.Object {
	object, ok := acpFeaturesJSON(t, text).(jsonvalue.Object)
	if !ok {
		t.Fatal("object fixture")
	}
	return object
}

func TestAPartialMuseReportKeepsTheFieldsItDoesCarry(t *testing.T) {
	extras := museUsageMeta(t, `{"muse": {"modelCalls": 2, "apiDurationMs": null}}`)
	details := MuseUsageAttachProviderDetails(museUsageStandard(t), extras).ProviderDetails
	if details == nil {
		t.Fatal("provider details")
	}
	if details.ModelCalls == nil || *details.ModelCalls != 2 {
		t.Fatalf("model calls %v", details.ModelCalls)
	}
	if details.APIDurationMs != nil {
		t.Fatalf("api duration %v", *details.APIDurationMs)
	}
	if len(details.ModelUsage) != 0 {
		t.Fatalf("model usage %v", details.ModelUsage)
	}
}
