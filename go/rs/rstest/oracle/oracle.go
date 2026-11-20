// Package oracle compares the Go port with the Rust code it is a port of.
//
// The Rust side is the program in go/port/oracle, which runs the real Rust
// libraries on requests read from its standard input. A Go test describes
// requests, says how the Go code answers one, and Check holds the two answers
// against each other.
//
// Rust's answers are recorded in testdata/oracle/<corpus>.jsonl beside the
// test, so `go test` needs no Rust toolchain. The environment variable
// MJ_ORACLE changes that:
//
//	MJ_ORACLE=live     also start the Rust program, generate fresh requests
//	                   and compare with what Rust answers now
//	MJ_ORACLE=record   rewrite the corpus from the Rust program
//
// A test with no recorded corpus fails; it never passes by comparing nothing.
package oracle

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"sync"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Case is one request and what Rust answered.
type Case struct {
	Request json.RawMessage `json:"request"`
	Reply   json.RawMessage `json:"reply"`
}

// Generate produces requests for the oracle. Each value passed to yield is
// encoded as one JSON request. Generation must be deterministic (a fixed
// seed), so a recorded corpus is reproducible.
type Generate func(yield func(request any))

// Check compares the Go port with Rust.
//
// module is the oracle module (the Cargo feature in go/port/oracle). corpus
// names the recorded file, testdata/oracle/<corpus>.jsonl in the test's
// package directory. generate produces the requests. answer is the Go side:
// it receives a request and returns the reply the Go code gives, as a value
// that encodes to the JSON Rust replies with. Use Catch inside answer when
// the Rust side may reply with a panic.
func Check(t *testing.T, module, corpus string, generate Generate, answer func(request json.RawMessage) any) {
	t.Helper()
	path := filepath.Join("testdata", "oracle", corpus+".jsonl")
	mode := os.Getenv("MJ_ORACLE")

	if mode == "record" {
		cases := askRust(t, module, generate)
		writeCorpus(t, path, cases)
	}

	cases := readCorpus(t, path)
	if mode == "live" {
		cases = append(cases, askRust(t, module, generate)...)
	}
	if len(cases) == 0 {
		t.Fatalf("oracle corpus %s holds no cases", path)
	}

	failures := 0
	for index, c := range cases {
		got, err := json.Marshal(answer(c.Request))
		if err != nil {
			t.Fatalf("case %d: Go reply does not encode: %v\nrequest: %s", index, err, c.Request)
		}
		if !sameJSON(got, c.Reply) {
			failures++
			if failures <= 20 {
				t.Errorf("case %d differs from Rust\nrequest: %s\nrust:    %s\ngo:      %s", index, c.Request, c.Reply, got)
			}
		}
	}
	if failures > 20 {
		t.Errorf("%d more cases differ from Rust", failures-20)
	}
}

// Catch runs f and returns its result, or, when f panics, the reply the Rust
// oracle gives for a panic: {"panic": "<message>"}.
func Catch(f func() any) (reply any) {
	defer func() {
		if recovered := recover(); recovered != nil {
			reply = map[string]string{"panic": rs.PanicInfoOf(recovered).Message}
		}
	}()
	return f()
}

// sameJSON compares two JSON texts as values, keeping numbers exact.
func sameJSON(a, b []byte) bool {
	return reflect.DeepEqual(decode(a), decode(b))
}

func decode(text []byte) any {
	decoder := json.NewDecoder(bytes.NewReader(text))
	decoder.UseNumber()
	var value any
	if err := decoder.Decode(&value); err != nil {
		return fmt.Sprintf("<not JSON: %v: %s>", err, text)
	}
	return value
}

func readCorpus(t *testing.T, path string) []Case {
	t.Helper()
	file, err := os.Open(path)
	if err != nil {
		t.Fatalf("no recorded oracle corpus at %s (%v); record it with MJ_ORACLE=record", path, err)
	}
	defer file.Close()
	var cases []Case
	reader := bufio.NewReader(file)
	for {
		line, err := reader.ReadBytes('\n')
		if len(bytes.TrimSpace(line)) > 0 {
			var c Case
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

func writeCorpus(t *testing.T, path string, cases []Case) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	var out bytes.Buffer
	for _, c := range cases {
		line, err := json.Marshal(c)
		if err != nil {
			t.Fatal(err)
		}
		out.Write(line)
		out.WriteByte('\n')
	}
	if err := os.WriteFile(path, out.Bytes(), 0o644); err != nil {
		t.Fatal(err)
	}
}

// askRust sends every generated request to the Rust oracle and returns the
// cases with its replies.
func askRust(t *testing.T, module string, generate Generate) []Case {
	t.Helper()
	if generate == nil {
		return nil
	}
	var requests [][]byte
	generate(func(request any) {
		encoded, err := json.Marshal(request)
		if err != nil {
			t.Fatalf("oracle request does not encode: %v", err)
		}
		requests = append(requests, encoded)
	})

	binary := build(t, module)
	command := exec.Command(binary, module)
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
		t.Fatalf("start oracle: %v", err)
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

	cases := make([]Case, 0, len(requests))
	reader := bufio.NewReaderSize(stdout, 1<<20)
	for index, request := range requests {
		line, err := reader.ReadBytes('\n')
		if err != nil {
			t.Fatalf("oracle %s stopped after %d of %d replies: %v", module, index, len(requests), err)
		}
		reply := bytes.TrimSpace(line)
		var probe map[string]json.RawMessage
		if json.Unmarshal(reply, &probe) == nil {
			if message, failed := probe["oracle_error"]; failed {
				t.Fatalf("oracle %s could not serve request %s: %s", module, request, message)
			}
		}
		cases = append(cases, Case{Request: request, Reply: append(json.RawMessage(nil), reply...)})
	}
	if err := <-writeDone; err != nil {
		t.Fatalf("write oracle requests: %v", err)
	}
	if err := command.Wait(); err != nil {
		t.Fatalf("oracle %s: %v", module, err)
	}
	return cases
}

var buildOnce sync.Map // module -> *sync.Once

// build compiles the oracle with one module and returns the program's path.
func build(t *testing.T, module string) string {
	t.Helper()
	directory := filepath.Join(RepoRoot(t), "go", "port", "oracle")
	binary := filepath.Join(directory, "target", module, "release", "oracle")
	once, _ := buildOnce.LoadOrStore(module, new(sync.Once))
	var buildErr error
	once.(*sync.Once).Do(func() {
		command := exec.Command("cargo", "build", "--release", "--quiet",
			"--features", module, "--target-dir", filepath.Join("target", module))
		command.Dir = directory
		output, err := command.CombinedOutput()
		if err != nil {
			buildErr = fmt.Errorf("build oracle module %s: %v\n%s", module, err, output)
		}
	})
	if buildErr != nil {
		t.Fatal(buildErr)
	}
	if _, err := os.Stat(binary); err != nil {
		t.Fatalf("oracle module %s was not built: %v", module, err)
	}
	return binary
}

// RepoRoot returns the repository root: the directory holding go.mod.
func RepoRoot(t testing.TB) string {
	t.Helper()
	directory, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	for {
		if _, err := os.Stat(filepath.Join(directory, "go.mod")); err == nil {
			return directory
		}
		parent := filepath.Dir(directory)
		if parent == directory {
			t.Fatal("no go.mod above the working directory")
		}
		directory = parent
	}
}
