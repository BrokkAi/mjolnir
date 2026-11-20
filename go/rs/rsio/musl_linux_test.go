//go:build linux

package rsio_test

import (
	"bufio"
	"bytes"
	"compress/gzip"
	"encoding/json"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"runtime"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs/rsio"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// Mjolnir's worker is linked against musl, whose strerror words many errors
// differently from glibc's. The oracle helper builds the Rust oracle for the
// host, which is glibc; the cases here are answered by an oracle built for
// the musl target instead, and compared with rsio.LinuxLibc set to musl.
//
// The corpus is testdata/oracle/<name>_linux_musl.jsonl.gz, in the helper's
// format. MJ_ORACLE=record rewrites it, and needs the Rust target
// <arch>-unknown-linux-musl (`rustup target add`); without it the recording
// is skipped and the test replays what is there.

type muslCase struct {
	Request json.RawMessage `json:"request"`
	Reply   json.RawMessage `json:"reply"`
}

// MuslCheck compares answer with an oracle built against musl, with
// rsio.LinuxLibc set to musl for the duration.
func muslCheck(t *testing.T, corpus string, generate oracle.Generate, answer func(request json.RawMessage) any) {
	t.Helper()
	path := filepath.Join("testdata", "oracle", corpus+"_linux_musl.jsonl.gz")
	mode := os.Getenv("MJ_ORACLE")

	var cases []muslCase
	if mode == "record" || mode == "live" {
		if fresh, ok := askMuslOracle(t, generate); ok {
			cases = fresh
			if mode == "record" {
				writeMuslCorpus(t, path, cases)
			}
		}
	}
	if mode != "record" || cases == nil {
		cases = append(readMuslCorpus(t, path), cases...)
	}
	if len(cases) == 0 {
		t.Fatalf("oracle corpus %s holds no cases", path)
	}

	previous := rsio.LinuxLibc
	rsio.LinuxLibc = rsio.Libc_Musl
	defer func() { rsio.LinuxLibc = previous }()

	failures := 0
	for index, c := range cases {
		got, err := json.Marshal(answer(c.Request))
		if err != nil {
			t.Fatalf("case %d: Go reply does not encode: %v\nrequest: %s", index, err, c.Request)
		}
		if !reflect.DeepEqual(decodeJSON(got), decodeJSON(c.Reply)) {
			failures++
			if failures <= 20 {
				t.Errorf("case %d differs from Rust on musl\nrequest: %s\nrust:    %s\ngo:      %s", index, c.Request, c.Reply, got)
			}
		}
	}
	if failures > 20 {
		t.Errorf("%d more cases differ from Rust on musl", failures-20)
	}
}

func decodeJSON(text []byte) any {
	decoder := json.NewDecoder(bytes.NewReader(text))
	decoder.UseNumber()
	var value any
	if err := decoder.Decode(&value); err != nil {
		return "<not JSON: " + err.Error() + ">"
	}
	return value
}

func muslTarget() string {
	if runtime.GOARCH == "arm64" {
		return "aarch64-unknown-linux-musl"
	}
	return "x86_64-unknown-linux-musl"
}

// askMuslOracle builds the oracle for the musl target and asks it every
// generated request. It reports false when the oracle cannot be built here.
func askMuslOracle(t *testing.T, generate oracle.Generate) ([]muslCase, bool) {
	t.Helper()
	directory := filepath.Join(oracle.RepoRoot(t), "go", "port", "oracle")
	build := exec.Command("cargo", "build", "--release", "--quiet", "--features", "std_io",
		"--target", muslTarget(), "--target-dir", filepath.Join("target", "std_io_musl"))
	build.Dir = directory
	if output, err := build.CombinedOutput(); err != nil {
		t.Logf("the oracle was not built for %s, so the musl corpus is replayed as recorded: %v\n%s", muslTarget(), err, output)
		return nil, false
	}
	binary := filepath.Join(directory, "target", "std_io_musl", muslTarget(), "release", "oracle")

	var requests [][]byte
	generate(func(request any) {
		encoded, err := json.Marshal(request)
		if err != nil {
			t.Fatalf("oracle request does not encode: %v", err)
		}
		requests = append(requests, encoded)
	})

	command := exec.Command(binary, "std_io")
	command.Stderr = os.Stderr
	stdin, err := command.StdinPipe()
	if err != nil {
		t.Fatal(err)
	}
	stdout, err := command.StdoutPipe()
	if err != nil {
		t.Fatal(err)
	}
	if err := command.Start(); err != nil {
		t.Fatalf("start the musl oracle: %v", err)
	}
	// Requests are written while replies are read: a pipe holds 64KB, and
	// writing everything first would block both processes.
	writeDone := make(chan error, 1)
	go func() {
		writer := bufio.NewWriter(stdin)
		for _, request := range requests {
			writer.Write(request)
			writer.WriteByte('\n')
		}
		err := writer.Flush()
		stdin.Close()
		writeDone <- err
	}()

	cases := make([]muslCase, 0, len(requests))
	reader := bufio.NewReaderSize(stdout, 1<<20)
	for index, request := range requests {
		line, err := reader.ReadBytes('\n')
		if err != nil {
			t.Fatalf("the musl oracle stopped after %d of %d replies: %v", index, len(requests), err)
		}
		reply := bytes.TrimSpace(line)
		var probe map[string]json.RawMessage
		if json.Unmarshal(reply, &probe) == nil {
			if message, failed := probe["oracle_error"]; failed {
				t.Fatalf("the musl oracle could not serve request %s: %s", request, message)
			}
		}
		cases = append(cases, muslCase{Request: request, Reply: append(json.RawMessage(nil), reply...)})
	}
	if err := <-writeDone; err != nil {
		t.Fatalf("write oracle requests: %v", err)
	}
	if err := command.Wait(); err != nil {
		t.Fatalf("the musl oracle: %v", err)
	}
	return cases, true
}

func readMuslCorpus(t *testing.T, path string) []muslCase {
	t.Helper()
	file, err := os.Open(path)
	if err != nil {
		t.Fatalf("no recorded oracle corpus at %s (%v); record it with MJ_ORACLE=record", path, err)
	}
	defer file.Close()
	decompressed, err := gzip.NewReader(file)
	if err != nil {
		t.Fatalf("%s: %v", path, err)
	}
	var cases []muslCase
	reader := bufio.NewReader(decompressed)
	for {
		line, err := reader.ReadBytes('\n')
		if len(bytes.TrimSpace(line)) > 0 {
			var c muslCase
			if decodeErr := json.Unmarshal(line, &c); decodeErr != nil {
				t.Fatalf("%s: line %d: %v", path, len(cases)+1, decodeErr)
			}
			cases = append(cases, c)
		}
		if err == io.EOF {
			return cases
		}
		if err != nil {
			t.Fatalf("%s: %v", path, err)
		}
	}
}

func writeMuslCorpus(t *testing.T, path string, cases []muslCase) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	var out bytes.Buffer
	compressor, err := gzip.NewWriterLevel(&out, gzip.BestCompression)
	if err != nil {
		t.Fatal(err)
	}
	for _, c := range cases {
		line, err := json.Marshal(c)
		if err != nil {
			t.Fatal(err)
		}
		compressor.Write(line)
		compressor.Write([]byte{'\n'})
	}
	if err := compressor.Close(); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, out.Bytes(), 0o644); err != nil {
		t.Fatal(err)
	}
}

// TestDiffAgainstRustMusl holds the error texts against Rust linked with
// musl: every errno, and the forms of io::Error built on one.
func TestDiffAgainstRustMusl(t *testing.T) {
	muslCheck(t, "rsio", func(yield func(any)) {
		for code := -3; code <= 200; code++ {
			yield(map[string]any{"op": "os_error", "code": code})
		}
		for _, code := range []int{255, 4095, 65536, 2147483647, -2147483648} {
			yield(map[string]any{"op": "os_error", "code": code})
		}
		for index, kind := range stableKinds {
			yield(map[string]any{"op": "custom_os", "kind": kind, "code": []int{2, 5, 12, 16, 24, 36, 40, 0, 999}[index%9]})
		}
	}, answerIo)
}
