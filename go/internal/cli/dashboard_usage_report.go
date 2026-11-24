// Port of mj-cli/src/dashboard/usage_report.rs.
//
// One cancellable report reader, replaced when the modal or filters change.

package cli

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"time"

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

// usageReportReadTimeout bounds one read of the report.
const usageReportReadTimeout = 20 * time.Second

// usageReportRereadInterval is how long a successful read stands before the
// next one.
const usageReportRereadInterval = 30 * time.Second

// sync starts a reader for the dashboard's current usage request, replacing
// one for an earlier request. It reads every 30 seconds until a read fails.
func (l *usageReportLoader) sync(dashboard *tui.DashboardState, updates *core.Unbounded[dashboardIoUpdate]) {
	generation, query, requested := dashboard.SessionUsageRequest()
	if (l.generation == nil && !requested) || (l.generation != nil && requested && *l.generation == generation) {
		return
	}
	l.cancel()
	if !requested {
		return
	}
	l.generation = &generation
	task := spawnDashboardTask(func(ctx context.Context) {
		for {
			usage, err := readUsageReport(ctx, query)
			if ctx.Err() != nil {
				// Replaced or cancelled: an aborted reader reports nothing.
				return
			}
			failed := err != nil
			if err != nil {
				slog.Warn("usage report unavailable", "error", err.Error())
			}
			report("session usage", updates, dashboardIoUpdate(dashboardIoUpdateSessionUsageFinished{
				generation: generation,
				report:     usage,
				err:        err,
			}))
			// Errors wait for an explicit Refresh; successful reads are
			// serial.
			if failed {
				return
			}
			if !sleepUntil(ctx, time.Now().Add(usageReportRereadInterval)) {
				return
			}
		}
	})
	l.task = task.abort
}

// readUsageReport is one read of the report on its own task, bounded by
// usageReportReadTimeout. A panicking read is reported as a failure.
func readUsageReport(ctx context.Context, query core.SessionUsageQuery) (core.SessionUsageReport, error) {
	type read struct {
		report core.SessionUsageReport
		err    error
	}
	readCtx, cancel := context.WithTimeout(ctx, usageReportReadTimeout)
	defer cancel()
	child := core.Spawn(func() read {
		apiClient, err := apiClientConnect(readCtx)
		if err != nil {
			return read{err: err}
		}
		report, err := apiClient.usageReport(readCtx, &query)
		return read{report: report, err: err}
	})
	select {
	case <-child.Done():
	case <-readCtx.Done():
		if ctx.Err() == nil {
			return core.SessionUsageReport{}, errors.New("usage request timed out")
		}
		return core.SessionUsageReport{}, ctx.Err()
	}
	result, joinErr := child.Join(context.Background())
	if joinErr != nil {
		return core.SessionUsageReport{}, fmt.Errorf("usage reader failed: %w", joinErr)
	}
	return result.report, result.err
}

func (l *usageReportLoader) cancel() {
	if l.task != nil {
		l.task()
	}
	l.task = nil
	l.generation = nil
}
