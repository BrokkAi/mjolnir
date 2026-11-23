//go:build linux

// Port of the tests of mj-worker/src/microvm/state.rs.

package worker

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"strings"
	"sync"
	"sync/atomic"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

func microvmSampleState(name string) MicrovmVmState {
	return MicrovmVmState{
		Format:         MicrovmStateFormat,
		ID:             strings.Repeat("a", 64),
		Name:           name,
		Created:        "2026-11-10T00:00:00Z",
		ImageReference: "ghcr.io/example/image:latest",
		ImageID:        strings.Repeat("b", 64),
		Labels:         map[string]string{"dev.mj.session": "s1"},
		VCPUs:          2,
		NanoCPUs:       2_000_000_000,
		MemoryBytes:    4 << 30,
		Env:            []string{"A=1"},
		User:           "0:0",
		WorkingDir:     "/workspace",
		Entrypoint:     []string{},
		Cmd:            []string{"sleep", "infinity"},
		Volumes: []MicrovmVmVolume{{
			Name:        "t1-ws",
			Destination: "/workspace",
			ReadWrite:   true,
			Chown:       false,
		}},
		Slot:        3,
		EngineBuild: "test",
		Jailed:      true,
	}
}

// microvmStateObject is the state as serde_json::to_value holds it.
func microvmStateObject(t *testing.T, state MicrovmVmState) jsonvalue.Object {
	t.Helper()
	encoded, err := json.Marshal(state)
	if err != nil {
		t.Fatal(err)
	}
	value, err := jsonvalue.Parse(encoded)
	if err != nil {
		t.Fatal(err)
	}
	return value.(jsonvalue.Object)
}

func TestStateWrittenByThisBuildReadsBackAndANewerFormatIsRefused(t *testing.T) {
	layout := NewMicrovmLayout(t.TempDir())
	if err := layout.Ensure(); err != nil {
		t.Fatal(err)
	}
	microvmMustMkdirAll(t, layout.VM("t1"))
	state := microvmSampleState("t1")
	if err := state.Write(layout); err != nil {
		t.Fatal(err)
	}
	read, err := MicrovmVmStateRead(layout, "t1")
	if err != nil {
		t.Fatal(err)
	}
	if read == nil || !reflect.DeepEqual(*read, state) {
		t.Fatalf("read back %+v", read)
	}
	if absent, err := MicrovmVmStateRead(layout, "absent"); err != nil || absent != nil {
		t.Fatalf("absent: %+v, %v", absent, err)
	}

	// A format 1 file, from before the jailer, reads back as not jailed.
	older := microvmStateObject(t, state)
	older.Set("format", int64(1))
	older.Delete("jailed")
	encoded, err := jsonvalue.Marshal(older)
	if err != nil {
		t.Fatal(err)
	}
	parsed, err := MicrovmVmStateParse(encoded)
	if err != nil {
		t.Fatal(err)
	}
	if parsed.Format != 1 || parsed.Jailed {
		t.Fatalf("format %d, jailed %v", parsed.Format, parsed.Jailed)
	}

	// A file from a future engine: unknown fields are not ours to drop.
	newer := microvmStateObject(t, state)
	newer.Set("format", int64(MicrovmStateFormat+1))
	newer.Set("quantum_flux", true)
	encoded, err = jsonvalue.Marshal(newer)
	if err != nil {
		t.Fatal(err)
	}
	microvmMustWrite(t, MicrovmVmStatePath(layout, "t1"), encoded)
	_, err = MicrovmVmStateRead(layout, "t1")
	if err == nil || !strings.Contains(err.Error(), "newer mj-worker") || !strings.Contains(err.Error(), fmt.Sprintf("format %d", MicrovmStateFormat+1)) {
		t.Fatalf("%v", err)
	}
}

func TestDeletingALockFileWhileHeldNeverLetsTwoHoldersIn(t *testing.T) {
	layout := NewMicrovmLayout(t.TempDir())
	if err := layout.Ensure(); err != nil {
		t.Fatal(err)
	}
	var inside atomic.Int64
	var workers sync.WaitGroup
	failures := make(chan error, 8)
	for worker := range 8 {
		workers.Add(1)
		go func() {
			defer workers.Done()
			for round := range 150 {
				lock, err := layout.LockVM("racing")
				if err != nil {
					failures <- err
					return
				}
				// A holder on an unlinked file would see another inside.
				if previous := inside.Add(1) - 1; previous != 0 {
					failures <- fmt.Errorf("two holders")
					lock.Drop()
					return
				}
				runtime.Gosched()
				// Deleting the file is the holder's last act: a locker that
				// arrives after it creates a new file and may enter at once.
				inside.Add(-1)
				if (worker+round)%3 == 0 {
					if err := layout.ForgetVMLock("racing", lock); err != nil {
						failures <- err
						lock.Drop()
						return
					}
				}
				lock.Drop()
			}
		}()
	}
	workers.Wait()
	close(failures)
	for err := range failures {
		t.Fatal(err)
	}
	lock, err := layout.LockVM("racing")
	if err != nil {
		t.Fatal(err)
	}
	if err := layout.ForgetVMLock("racing", lock); err != nil {
		t.Fatal(err)
	}
	lock.Drop()
	if _, err := os.Stat(filepath.Join(layout.Locks(), "vm-racing.lock")); err == nil {
		t.Fatal("the lock file outlived its last holder")
	}
}
