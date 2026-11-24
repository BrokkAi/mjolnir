// Port of mj-cli/src/dashboard/summary_reads.rs tests.

package cli

import (
	"fmt"
	"runtime"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// summaryReadTasks is every read task reads owns right now.
func summaryReadTasks(reads *summaryReads) []*dashboardTask {
	var tasks []*dashboardTask
	for _, read := range reads.reads {
		if read.task != nil {
			tasks = append(tasks, read.task)
		}
	}
	return tasks
}

func TestLeavingAFamilyCancelsQueuedReadsAndRejectsPreviousVisits(t *testing.T) {
	reads := newSummaryReads()
	// Hold all network admission while a large family queues its reads.
	// Cancelled tasks must disappear without ever consulting a daemon.
	for range cap(reads.permits) {
		reads.permits <- struct{}{}
	}
	updates := core.NewUnbounded[dashboardIoUpdate]()
	wanted := map[string]uint64{}
	for id := range 1200 {
		wanted[fmt.Sprintf("child-%d", id)] = 0
	}
	item := tui.NotificationReadRequest{
		SessionID:         "notification",
		Source:            tui.NotificationReadSourceItem{Value: "outside-tail"},
		SelectionRevision: 1,
	}
	reads.reconcile(wanted, &item, updates)
	// Rust's Arc counts every task still holding the semaphore; Go has no
	// count, so the test keeps each task it started and waits for them all.
	spawned := summaryReadTasks(reads)
	oldItem := reads.reads[readKeyMessage{request: item}].generation
	old := reads.reads[readKeySummary{id: "child-0"}].generation
	runtime.Gosched()
	if removed := reads.reconcile(map[string]uint64{}, nil, updates); len(removed) != 1200 {
		t.Fatalf("removed %d summaries", len(removed))
	}
	revisited := item
	revisited.SelectionRevision = 2
	reads.reconcile(map[string]uint64{"child-0": 0}, &revisited, updates)
	spawned = append(spawned, summaryReadTasks(reads)...)
	currentItem := reads.reads[readKeyMessage{request: revisited}].generation
	if reads.finishItem(item, oldItem) {
		t.Fatal("the previous visit's item finished")
	}
	if reads.finishItem(revisited, oldItem) {
		t.Fatal("the previous visit's generation finished the new item")
	}
	if !reads.finishItem(revisited, currentItem) {
		t.Fatal("the current item did not finish")
	}
	current := reads.reads[readKeySummary{id: "child-0"}].generation
	if reads.finish("child-0", old) {
		t.Fatal("a previous visit cannot finish the new read")
	}
	if !reads.finish("child-0", current) {
		t.Fatal("the current read did not finish")
	}
	// Only the owner and the held permits may remain: every task has ended.
	deadline := time.NewTimer(2 * time.Second)
	defer deadline.Stop()
	for _, task := range spawned {
		select {
		case <-task.done():
		case <-deadline.C:
			t.Fatal("leaving a family must cancel its queued network work")
		}
	}
	if update, ok, closed := updates.TryRecv(); ok || closed {
		t.Fatalf("a cancelled read reported %#v (closed %v)", update, closed)
	}
	for range cap(reads.permits) {
		<-reads.permits
	}
}
