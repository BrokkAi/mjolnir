package rs

import (
	"io"
	"os"
	"regexp"
	"strings"
	"testing"
)

// captureStderr runs f with os.Stderr going to a pipe and returns what f
// wrote there.
func captureStderr(t *testing.T, f func()) string {
	t.Helper()
	reader, writer, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	saved := os.Stderr
	os.Stderr = writer
	done := make(chan string)
	go func() {
		data, _ := io.ReadAll(reader)
		done <- string(data)
	}()
	func() {
		defer func() { os.Stderr = saved }()
		f()
	}()
	writer.Close()
	return <-done
}

// The default hook writes what std::panicking::default_hook of Rust 1.99.1
// writes: the thread's name and its OS thread id, the location, the message,
// and the note about RUST_BACKTRACE after the first panic only.
func TestPanicDefaultHookText(t *testing.T) {
	if _, set := os.LookupEnv("RUST_BACKTRACE"); set {
		t.Skip("RUST_BACKTRACE is set: the hook prints a backtrace instead of the note")
	}
	hookMu.Lock()
	firstPanic = true
	hookMu.Unlock()

	first := captureStderr(t, func() {
		defaultPanicHook(&PanicInfo{Message: "boom\nsecond line", File: "src/main.go", Line: 12, Thread: "main"})
	})
	want := regexp.MustCompile("^\nthread 'main' \\(([0-9]+)\\) panicked at src/main.go:12:\nboom\nsecond line\n" +
		"note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n$")
	match := want.FindStringSubmatch(first)
	if match == nil {
		t.Fatalf("first report:\n%q", first)
	}
	if match[1] == "0" {
		t.Errorf("no OS thread id in %q", first)
	}

	second := captureStderr(t, func() {
		defaultPanicHook(&PanicInfo{Message: "again", File: "x.go", Line: 1})
	})
	if !regexp.MustCompile("^\nthread '<unnamed>' \\([0-9]+\\) panicked at x.go:1:\nagain\n$").MatchString(second) {
		t.Fatalf("second report:\n%q", second)
	}
	if strings.Contains(second, "note:") {
		t.Errorf("the note is printed after the first panic only: %q", second)
	}
}
