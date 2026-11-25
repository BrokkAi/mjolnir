// Port of mj-cli/src/dashboard/usage_report.rs.
//
// One cancellable report reader, replaced when the modal or filters change.

package cli

import (
	"context"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// usageReportLoader owns the reader of the usage report the dashboard asks
// for. The zero value is Rust's Default.
type usageReportLoader struct {
	// generation is the request the reader serves; nil when none.
	generation *uint64
	// task aborts the reader (Rust's AbortOnDropHandle); nil when none.
	task context.CancelFunc
}

// sync starts a reader for the dashboard's current usage request, replacing
// one for an earlier request. It reads every 30 seconds until a read fails.
func (l *usageReportLoader) sync(dashboard *tui.DashboardState, updates *core.Unbounded[dashboardIoUpdate]) {
	panic("unported: mj-cli/src/dashboard/usage_report.rs UsageReportLoader::sync")
}

func (l *usageReportLoader) cancel() {
	if l.task != nil {
		l.task()
	}
	l.task = nil
	l.generation = nil
}
