//go:build linux

// Port of the tests of mj-worker/src/microvm/replace_root.rs.

package worker

import (
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

const microvmOldImage = "1111111111111111111111111111111111111111111111111111111111111111"
const microvmNewImage = "2222222222222222222222222222222222222222222222222222222222222222"

// microvmCopy is the plain copy the tests give apply in place of a reflink.
func microvmCopy(source, destination string) error {
	return microvmCopyFile(source, destination)
}

// microvmReplaceRootHost is a VM named `t1` on microvmOldImage with a
// workspace volume, and microvmNewImage known under the reference
// `agent:new`.
func microvmReplaceRootHost(t *testing.T) MicrovmLayout {
	t.Helper()
	layout := NewMicrovmLayout(t.TempDir())
	if err := layout.Ensure(); err != nil {
		t.Fatal(err)
	}
	for _, image := range []struct{ id, content string }{{microvmOldImage, "old image"}, {microvmNewImage, "new image"}} {
		microvmMustMkdirAll(t, layout.Image(image.id))
		microvmMustWrite(t, filepath.Join(layout.Image(image.id), "root.ext4"), []byte(image.content))
		microvmMustWrite(t, filepath.Join(layout.Image(image.id), "config.json"), []byte("{}"))
	}
	refs := MicrovmImageRefs{Refs: map[string]string{"agent:new": microvmNewImage, "agent:old": microvmOldImage}}
	if err := refs.Write(layout); err != nil {
		t.Fatal(err)
	}
	microvmMustMkdirAll(t, layout.VM("t1"))
	microvmMustWrite(t, filepath.Join(layout.VM("t1"), microvmReplaceRootRoot), []byte("old root with session edits"))
	state := MicrovmVmState{
		Format:         MicrovmStateFormat,
		Jailed:         true,
		ID:             strings.Repeat("a", 64),
		Name:           "t1",
		Created:        "2026-11-11T00:00:00Z",
		ImageReference: "agent:old",
		ImageID:        microvmOldImage,
		Labels:         map[string]string{"dev.mj.session": "s1"},
		VCPUs:          2,
		NanoCPUs:       2_000_000_000,
		MemoryBytes:    4 << 30,
		Env:            []string{},
		User:           "0:0",
		WorkingDir:     "/workspace",
		Entrypoint:     []string{},
		Cmd:            []string{"sleep", "infinity"},
		Volumes: []MicrovmVmVolume{{
			Name:        "t1-workspace",
			Destination: "/workspace",
			ReadWrite:   true,
			Chown:       true,
		}},
		Slot:        0,
		EngineBuild: "test",
	}
	if err := state.Write(layout); err != nil {
		t.Fatal(err)
	}
	return layout
}

func microvmAccept(MicrovmLayout, string, string, string) error { return nil }

func microvmUpgradeLabels() map[string]string {
	return map[string]string{"dev.mj.container-upgrade": "op1"}
}

func microvmRootDisk(t *testing.T, layout MicrovmLayout) string {
	t.Helper()
	return microvmReadText(t, filepath.Join(layout.VM("t1"), microvmReplaceRootRoot))
}

func microvmMustRecord(t *testing.T, layout MicrovmLayout) microvmReplaceRootRecord {
	t.Helper()
	record, err := microvmReadReplaceRootRecord(layout, "t1")
	if err != nil || record == nil {
		t.Fatalf("record: %+v, %v", record, err)
	}
	return *record
}

func microvmMustState(t *testing.T, layout MicrovmLayout) MicrovmVmState {
	t.Helper()
	state, err := MicrovmVmStateRead(layout, "t1")
	if err != nil || state == nil {
		t.Fatalf("state: %+v, %v", state, err)
	}
	return *state
}

// microvmCrashAfter runs the swap's steps, in order, so a test can stop
// after any of them the way a crash would.
func microvmCrashAfter(t *testing.T, layout MicrovmLayout, steps int) {
	t.Helper()
	directory := layout.VM("t1")
	record := microvmMustRecord(t, layout)
	if steps >= 1 {
		if err := microvmCopy(MicrovmRootImage(layout, record.ImageID), filepath.Join(directory, microvmReplaceRootNext)); err != nil {
			t.Fatal(err)
		}
	}
	if steps >= 2 {
		if err := os.Rename(filepath.Join(directory, microvmReplaceRootRoot), filepath.Join(directory, microvmReplaceRootPrevious)); err != nil {
			t.Fatal(err)
		}
	}
	if steps >= 3 {
		if err := os.Rename(filepath.Join(directory, microvmReplaceRootNext), filepath.Join(directory, microvmReplaceRootRoot)); err != nil {
			t.Fatal(err)
		}
	}
}

// A crash between any two steps of the swap is finished by a second apply,
// which then changes nothing further, and release keeps the new root only.
func TestApplyFinishesAfterACrashBetweenAnyTwoSteps(t *testing.T) {
	for steps := 0; steps <= 3; steps++ {
		layout := microvmReplaceRootHost(t)
		if _, err := microvmReplaceRootStageChecked(layout, "t1", "op1", "agent:new", microvmUpgradeLabels(), microvmAccept); err != nil {
			t.Fatal(err)
		}
		microvmCrashAfter(t, layout, steps)
		seen, err := MicrovmReplaceRootObserved(layout, "t1", microvmMustState(t, layout))
		if err != nil {
			t.Fatal(err)
		}
		record := microvmMustRecord(t, layout)
		if (seen.ID == record.NewID) != (steps >= 2) {
			t.Fatalf("after %d steps the observer sees id %s", steps, seen.ID)
		}
		id, err := microvmReplaceRootApplyStopped(layout, "t1", "op1", microvmCopy)
		if err != nil {
			t.Fatalf("after %d steps: %v", steps, err)
		}
		state := microvmMustState(t, layout)
		if state.ID != id {
			t.Fatalf("after %d steps: state id %s, apply said %s", steps, state.ID, id)
		}
		if id == strings.Repeat("a", 64) {
			t.Fatal("the VM kept its id")
		}
		if state.ImageID != microvmNewImage || state.ImageReference != "agent:new" {
			t.Fatalf("image %s %s", state.ImageID, state.ImageReference)
		}
		if state.Labels["dev.mj.session"] != "s1" || state.Labels["dev.mj.container-upgrade"] != "op1" {
			t.Fatalf("labels %v", state.Labels)
		}
		if len(state.Volumes) != 1 {
			t.Fatalf("volumes %v", state.Volumes)
		}
		if got := microvmRootDisk(t, layout); got != "new image" {
			t.Fatalf("after %d steps the root disk holds %q", steps, got)
		}
		if got := microvmReadText(t, filepath.Join(layout.VM("t1"), microvmReplaceRootPrevious)); got != "old root with session edits" {
			t.Fatal(got)
		}
		again, err := microvmReplaceRootApplyStopped(layout, "t1", "op1", microvmCopy)
		if err != nil || again != id {
			t.Fatalf("second apply: %s, %v", again, err)
		}

		if err := MicrovmReplaceRootRelease(layout, "t1", "op1"); err != nil {
			t.Fatal(err)
		}
		if microvmExists(filepath.Join(layout.VM("t1"), microvmReplaceRootPrevious)) || microvmExists(filepath.Join(layout.VM("t1"), microvmReplaceRootNext)) {
			t.Fatal("release left a root disk behind")
		}
		if record, err := microvmReadReplaceRootRecord(layout, "t1"); err != nil || record != nil {
			t.Fatalf("record after release: %+v, %v", record, err)
		}
		if err := MicrovmReplaceRootRelease(layout, "t1", "op1"); err != nil {
			t.Fatal(err)
		}
		if got := microvmRootDisk(t, layout); got != "new image" {
			t.Fatal(got)
		}
	}
}

// A release before the swap (a cancelled upgrade) drops the pin and leaves
// the VM alone; a release of a swap that was interrupted is refused,
// because the old root disk is then the only one.
func TestReleaseKeepsTheOnlyRootDiskOfAnInterruptedSwap(t *testing.T) {
	layout := microvmReplaceRootHost(t)
	if _, err := microvmReplaceRootStageChecked(layout, "t1", "op1", "agent:new", microvmUpgradeLabels(), microvmAccept); err != nil {
		t.Fatal(err)
	}
	pinned, err := MicrovmPinnedImages(layout)
	if err != nil || !pinned[microvmNewImage] {
		t.Fatalf("pinned %v, %v", pinned, err)
	}
	microvmCrashAfter(t, layout, 2)
	err = MicrovmReplaceRootRelease(layout, "t1", "op1")
	if err == nil || !strings.Contains(err.Error(), "interrupted") {
		t.Fatalf("%v", err)
	}
	if !microvmExists(filepath.Join(layout.VM("t1"), microvmReplaceRootPrevious)) {
		t.Fatal("release removed the only root disk")
	}

	layout = microvmReplaceRootHost(t)
	if _, err := microvmReplaceRootStageChecked(layout, "t1", "op1", "agent:new", microvmUpgradeLabels(), microvmAccept); err != nil {
		t.Fatal(err)
	}
	if err := MicrovmReplaceRootRelease(layout, "t1", "op1"); err != nil {
		t.Fatal(err)
	}
	if pinned, err := MicrovmPinnedImages(layout); err != nil || len(pinned) != 0 {
		t.Fatalf("pinned %v, %v", pinned, err)
	}
	if got := microvmRootDisk(t, layout); got != "old root with session edits" {
		t.Fatal(got)
	}
}

// Stage is repeatable for its own operation and refuses another one's
// image or a second operation while one is in progress.
func TestStageIsIdempotentAndRefusesASecondOperation(t *testing.T) {
	layout := microvmReplaceRootHost(t)
	first, err := microvmReplaceRootStageChecked(layout, "t1", "op1", "agent:new", microvmUpgradeLabels(), microvmAccept)
	if err != nil {
		t.Fatal(err)
	}
	record := microvmMustRecord(t, layout)
	if again, err := microvmReplaceRootStageChecked(layout, "t1", "op1", "agent:new", microvmUpgradeLabels(), microvmAccept); err != nil || again != first {
		t.Fatalf("restage: %s, %v", again, err)
	}
	if after := microvmMustRecord(t, layout); !reflect.DeepEqual(after, record) {
		t.Fatalf("restage rewrote the record: %+v", after)
	}
	if _, err := microvmReplaceRootStageChecked(layout, "t1", "op1", "agent:old", microvmUpgradeLabels(), microvmAccept); err == nil {
		t.Fatal("op1 staged a second image")
	}
	_, err = microvmReplaceRootStageChecked(layout, "t1", "op2", "agent:new", microvmUpgradeLabels(), microvmAccept)
	if err == nil || !strings.Contains(err.Error(), "in progress") {
		t.Fatalf("%v", err)
	}
	if _, err := microvmReplaceRootApplyStopped(layout, "t1", "op2", microvmCopy); err == nil {
		t.Fatal("op2 applied op1's replacement")
	}
	if _, err := microvmReplaceRootStageChecked(layout, "absent", "op1", "agent:new", microvmUpgradeLabels(), microvmAccept); err == nil {
		t.Fatal("staged a missing container")
	}
}
