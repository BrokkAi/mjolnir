//go:build unix

package rspath

import (
	"encoding/json"
	"os"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs/rsio"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// std::path::absolute reads the current directory, so it is compared with
// Rust on the platform family it runs on, from directories every Unix has.

type absoluteRequest struct {
	Op  string `json:"op"`
	P   string `json:"p"`
	Cwd string `json:"cwd,omitempty"`
}

// ioErrorReply is what the oracle says about a std::io::Error.
func ioErrorReply(t *testing.T, err error) map[string]any {
	t.Helper()
	failed, ok := err.(*rsio.Error)
	if !ok {
		t.Fatalf("not an io::Error: %T %v", err, err)
	}
	reply := map[string]any{"err": failed.Error(), "kind": failed.Kind().DebugString(), "os": nil}
	if code, ok := failed.RawOsError().Get(); ok {
		reply["os"] = code
	}
	return reply
}

func absoluteReply(t *testing.T, path string) any {
	t.Helper()
	absolute, err := Absolute(path)
	if err != nil {
		return ioErrorReply(t, err)
	}
	return map[string]any{"ok": toHex(absolute)}
}

func TestDiffAbsoluteAgainstRust(t *testing.T) {
	start, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	defer func() {
		if err := os.Chdir(start); err != nil {
			t.Fatal(err)
		}
	}()

	paths := []string{
		"", "a", "a/b", "./a", "../a", ".", "..", "./", "../", "a/./b/../c", "a//b/", "a/", "\xff", "a/\xff/",
		"/", "//", "///", "/.", "/..", "//.", "/a/b/c", "/a//b/c", "//a/b/c", "///a/b/c", "/a/b/c/", "/a/b/c//",
		"/a/./b/../c/.././..", "//a", "//a/", "///a/", "/./a", "/../a", "é/日本", "a b",
	}
	oracle.Check(t, "std_path", "std_path_absolute_unix", func(yield func(any)) {
		for _, directory := range []string{"/", "/usr"} {
			for _, path := range paths {
				yield(absoluteRequest{Op: "absolute", P: toHex(path), Cwd: toHex(directory)})
			}
		}
		yield(absoluteRequest{Op: "absolute_deleted_cwd", P: toHex("a")})
		yield(absoluteRequest{Op: "absolute_deleted_cwd", P: toHex("/a/../b/")})
	}, func(raw json.RawMessage) any {
		var request absoluteRequest
		if err := json.Unmarshal(raw, &request); err != nil {
			t.Fatalf("oracle request does not decode: %v\n%s", err, raw)
		}
		path := fromHex(t, request.P)
		switch request.Op {
		case "absolute":
			if err := os.Chdir(fromHex(t, request.Cwd)); err != nil {
				t.Fatal(err)
			}
			return absoluteReply(t, path)
		case "absolute_deleted_cwd":
			// From a directory that no longer exists: getcwd fails.
			gone, err := os.MkdirTemp("", "mj-rspath-gone-")
			if err != nil {
				t.Fatal(err)
			}
			if err := os.Chdir(gone); err != nil {
				t.Fatal(err)
			}
			if err := os.Remove(gone); err != nil {
				t.Fatal(err)
			}
			reply := map[string]any{"absolute": absoluteReply(t, path)}
			if current, err := getcwd(); err != nil {
				reply["current_dir"] = ioErrorReply(t, err)
			} else {
				reply["current_dir"] = map[string]any{"ok": toHex(current)}
			}
			if err := os.Chdir("/"); err != nil {
				t.Fatal(err)
			}
			return reply
		default:
			t.Fatalf("unknown oracle operation %q", request.Op)
			return nil
		}
	})
}
