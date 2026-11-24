// Port of mj-cli/src/dashboard/read_receipts.rs tests.

package cli

import (
	"errors"
	"reflect"
	"testing"
)

// expectReceipt checks one acknowledge or complete answer: the frontier to
// save now, or none (want nil).
func expectReceipt(t *testing.T, through uint64, ok bool, want *uint64) {
	t.Helper()
	if want == nil {
		if ok {
			t.Fatalf("started a save of %d", through)
		}
		return
	}
	if !ok || through != *want {
		t.Fatalf("save %d, %v; want %d", through, ok, *want)
	}
}

func receiptOf(through uint64) *uint64 { return &through }

func TestIndependentSessionsSaveConcurrentlyAndUpdatesCoalesce(t *testing.T) {
	var receipts readReceipts
	through, ok := receipts.acknowledge("a", 3)
	expectReceipt(t, through, ok, receiptOf(3))
	through, ok = receipts.acknowledge("b", 8)
	expectReceipt(t, through, ok, receiptOf(8))
	through, ok = receipts.acknowledge("a", 5)
	expectReceipt(t, through, ok, nil)
	through, ok = receipts.acknowledge("a", 4)
	expectReceipt(t, through, ok, nil)
	through, ok = receipts.complete("a", 3, nil)
	expectReceipt(t, through, ok, receiptOf(5))
	through, ok = receipts.complete("b", 8, nil)
	expectReceipt(t, through, ok, nil)
	if !receipts.isSaving() {
		t.Fatal("a is still saving")
	}
	through, ok = receipts.complete("a", 5, nil)
	expectReceipt(t, through, ok, nil)
	if receipts.isSaving() {
		t.Fatal("nothing is saving")
	}
	through, ok = receipts.acknowledge("a", 4)
	expectReceipt(t, through, ok, nil)
}

func TestFailedSavesRetryBoundedlyAndRemainAvailableForShutdown(t *testing.T) {
	var receipts readReceipts
	through, ok := receipts.acknowledge("a", 3)
	expectReceipt(t, through, ok, receiptOf(3))
	through, ok = receipts.complete("a", 0, errors.New("offline"))
	expectReceipt(t, through, ok, receiptOf(3))
	through, ok = receipts.complete("a", 0, errors.New("offline"))
	expectReceipt(t, through, ok, nil)
	if receipts.isSaving() {
		t.Fatal("nothing is saving")
	}
	if failure := receipts.failures["a"]; failure != "offline" {
		t.Fatalf("failure %q", failure)
	}
	if retries := receipts.retryFailed(); !reflect.DeepEqual(retries, []readReceiptRetry{{sessionID: "a", through: 3}}) {
		t.Fatalf("retries %+v", retries)
	}
	if !receipts.isSaving() {
		t.Fatal("the retry is saving")
	}
	through, ok = receipts.complete("a", 3, nil)
	expectReceipt(t, through, ok, nil)
	if len(receipts.failures) != 0 {
		t.Fatalf("failures %v", receipts.failures)
	}
}
