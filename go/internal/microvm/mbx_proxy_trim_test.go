//go:build linux

// Port of the tests of mj-microvm/src/mbx_proxy/trim.rs.

package microvm

import (
	"bytes"
	"io"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"golang.org/x/sys/unix"
)

func TestMbxProxyTrimKeepsLiveReaderAndRemovesOldest(t *testing.T) {
	cache := t.TempDir()
	older := filepath.Join(cache, strings.Repeat("a", 64))
	newer := filepath.Join(cache, strings.Repeat("b", 64))
	stale := filepath.Join(cache, microvmMbxStagingPrefix+"abandoned")
	active := filepath.Join(cache, microvmMbxStagingPrefix+"active")
	directory := filepath.Join(cache, strings.Repeat("c", 64))
	contents := bytes.Repeat([]byte{0xa5}, 128*1024)
	for _, path := range []string{older, newer, stale, active} {
		microvmMustWrite(t, path, contents)
	}
	microvmMustMkdirAll(t, directory)
	reader, err := os.Open(older)
	if err != nil {
		t.Fatal(err)
	}
	defer reader.Close()
	past := unix.NsecToTimespec(time.Now().Add(-7200 * time.Second).UnixNano())
	omit := unix.Timespec{Nsec: unix.UTIME_OMIT}
	if err := unix.UtimesNanoAt(unix.AT_FDCWD, older, []unix.Timespec{past, omit}, 0); err != nil {
		t.Fatal(err)
	}
	if err := unix.UtimesNanoAt(unix.AT_FDCWD, stale, []unix.Timespec{omit, past}, 0); err != nil {
		t.Fatal(err)
	}
	budget := uint64(len(contents))
	freed, err := microvmMbxTrim(cache, &budget)
	if err != nil {
		t.Fatal(err)
	}
	if freed != 2*budget {
		t.Fatalf("freed %d", freed)
	}
	if microvmExists(older) || microvmExists(stale) {
		t.Fatal("the oldest entry or the abandoned staging file survived")
	}
	if !microvmExists(newer) || !microvmExists(active) {
		t.Fatal("trim removed the newer entry or the active staging file")
	}
	if info, err := os.Stat(directory); err != nil || !info.IsDir() {
		t.Fatalf("the directory went: %v", err)
	}
	read, err := io.ReadAll(reader)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(read, contents) {
		t.Fatal("the live reader lost its contents")
	}
	if freed, err := microvmMbxTrim(cache, &budget); err != nil || freed != 0 {
		t.Fatalf("second trim freed %d: %v", freed, err)
	}
}
