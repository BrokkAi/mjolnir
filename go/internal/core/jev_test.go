// Port of the tests in mj-core/src/jev.rs.

package core

import (
	"encoding/json"
	"os"
	"strconv"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

func TestExactInputAndFinalActionAreCorrelatedWithoutExposingInputsInLists(t *testing.T) {
	dir := t.TempDir()
	log, err := DecisionLogOpen(dir)
	if err != nil {
		t.Fatal(err)
	}
	attempt := log.Start("s", "activity", "Who needs to act?", "Current conversation and live facts")
	defer attempt.Release()
	input := `{"text":"résumé 🛠","tools_in_flight":["shell"],"omitted":true}`
	answer := "Finished"
	attempt.Update(&answer, json.RawMessage(`{"request":`+input+`}`))
	attempt.Finish("stale", "New output arrived; mj kept the runtime status.")
	list, err := JevRead(dir, "s", nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(list.Decisions) != 1 {
		t.Fatalf("decisions = %d", len(list.Decisions))
	}
	if list.Decisions[0].Technical != nil {
		t.Fatalf("list exposes technical %s", list.Decisions[0].Technical)
	}
	id := attempt.ID()
	detail, err := JevRead(dir, "s", &id)
	if err != nil {
		t.Fatal(err)
	}
	technical, err := jsonvalue.Parse(detail.Decisions[0].Technical)
	if err != nil {
		t.Fatal(err)
	}
	request, _ := technical.(jsonvalue.Object).Get("request")
	want, _ := jsonvalue.Parse([]byte(input))
	if !jsonvalue.Equal(request, want) {
		t.Fatalf("request = %v", request)
	}
	if detail.Decisions[0].Status != "stale" {
		t.Fatalf("status = %q", detail.Decisions[0].Status)
	}
	other, err := JevRead(dir, "other", nil)
	if err != nil || len(other.Decisions) != 0 {
		t.Fatalf("other = %+v, %v", other, err)
	}
}

func TestCancellationRotationRestartAndPartialWritesAreHonest(t *testing.T) {
	dir := t.TempDir()
	log, err := DecisionLogOpen(dir)
	if err != nil {
		t.Fatal(err)
	}
	attempt := log.Start("s", "continuation", "Work left?", "Earlier instructions")
	id := attempt.ID()
	// Rust drops the last owner here.
	attempt.Release()
	page, err := JevRead(dir, "s", &id)
	if err != nil {
		t.Fatal(err)
	}
	record := page.Decisions[0]
	if record.Status != "cancelled" {
		t.Fatalf("status = %q", record.Status)
	}
	// Each record occupies its own segment with this deliberately small limit.
	for i := 0; i < 8; i++ {
		record.ID = strconv.Itoa(i)
		if err := jevAppend(dir, &record, 1); err != nil {
			t.Fatal(err)
		}
	}
	page, err = jevReadFiles(dir, "s", nil)
	if err != nil || len(page.Decisions) != 4 {
		t.Fatalf("decisions = %d, %v", len(page.Decisions), err)
	}
	page, err = jevReadFiles(dir, "s", &id)
	if err != nil || len(page.Decisions) != 0 {
		t.Fatalf("decisions = %d, %v", len(page.Decisions), err)
	}
	record.Status = "pending"
	record.Owner = "previous-process"
	if err := jevAppend(dir, &record, jevSegmentBytes); err != nil {
		t.Fatal(err)
	}
	file, err := os.OpenFile(jevSegment(dir, 0), os.O_APPEND|os.O_WRONLY, 0)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := file.Write([]byte("{broken")); err != nil {
		t.Fatal(err)
	}
	file.Close()
	page, err = jevReadFiles(dir, "s", &record.ID)
	if err != nil {
		t.Fatal(err)
	}
	if page.Decisions[0].Status != "interrupted" {
		t.Fatalf("status = %q", page.Decisions[0].Status)
	}
	if len(page.Warnings) != 1 {
		t.Fatalf("warnings = %q", page.Warnings)
	}
}
