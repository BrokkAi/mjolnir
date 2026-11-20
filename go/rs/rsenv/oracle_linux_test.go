//go:build linux

package rsenv

import (
	"encoding/hex"
	"encoding/json"
	"math/rand/v2"
	"os"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rsio"
	"github.com/BrokkAi/mjolnir/go/rs/rspath"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// The differential test asks the real std::env (go/port/oracle, module
// std_path) and holds this package to its answers.
//
// The oracle is a separate process: it starts with the environment and the
// current directory of this test, and both sides then change them with the
// same requests in the same order. The corpus is recorded on Linux, where
// temp_dir falls back to /tmp and getenv is glibc's, and replayed on Linux.
// What depends on the machine (the constants, the home directory from the
// user database) is asked only under MJ_ORACLE=live.

type envRequest struct {
	Op     string   `json:"op"`
	S      *string  `json:"s,omitempty"`
	Parts  []string `json:"parts,omitempty"`
	K      string   `json:"k,omitempty"`
	V      string   `json:"v,omitempty"`
	P      string   `json:"p,omitempty"`
	Tmpdir *string  `json:"tmpdir,omitempty"`
	Home   *string  `json:"home,omitempty"`
}

// A variable the test sets before the oracle starts, so that both processes
// hold it. (A name or value that is not UTF-8 cannot be set this way: the
// oracle helper runs cargo first, which panics on it in env::vars. Those are
// set by env_set_get requests instead.)
const (
	oracleEqName  = "MJ_RSENV_ORACLE_EQ"
	oracleEqValue = "B=C"
)

func toHex(s string) string { return hex.EncodeToString([]byte(s)) }

func fromHex(t *testing.T, s string) string {
	t.Helper()
	bytes, err := hex.DecodeString(s)
	if err != nil {
		t.Fatalf("oracle request holds bad hexadecimal %q: %v", s, err)
	}
	return string(bytes)
}

func optionalHex(o rs.Option[string]) any {
	if value, ok := o.Get(); ok {
		return toHex(value)
	}
	return nil
}

func hexPointer(s string) *string {
	encoded := toHex(s)
	return &encoded
}

// agree returns the text when every way the Go code has of producing it
// gives the same text, and otherwise all of them, so that the case differs
// from Rust's answer and is reported.
func agree(texts ...string) string {
	for _, text := range texts[1:] {
		if text != texts[0] {
			return "the Go forms disagree: " + strings.Join(texts, " | ")
		}
	}
	return texts[0]
}

// caught runs f and returns the message of the panic it raised, if any.
func caught(f func()) (message string, panicked bool) {
	defer func() {
		if recovered := recover(); recovered != nil {
			message, panicked = rs.PanicInfoOf(recovered).Message, true
		}
	}()
	f()
	return "", false
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

func envGetReply(key string) map[string]any {
	reply := map[string]any{"var_os": optionalHex(VarOs(key))}
	if value, err := Var(key); err != nil {
		reply["var"] = map[string]any{"err": agree(err.Error(), rs.ToString(err)), "debug": rs.DebugString(err)}
	} else {
		reply["var"] = map[string]any{"ok": value}
	}
	return reply
}

func envSetGetReply(key, value string) any {
	if message, panicked := caught(func() { SetVar(key, value) }); panicked {
		return map[string]any{"set_panic": message}
	}
	afterSet := envGetReply(key)
	listed := false
	for name, held := range VarsOs().All() {
		if name == key && held == value {
			listed = true
			break
		}
	}
	var removed any
	if message, panicked := caught(func() { RemoveVar(key) }); panicked {
		removed = map[string]any{"remove_panic": message}
	} else {
		removed = envGetReply(key)
	}
	return map[string]any{"set": afterSet, "listed": listed, "removed": removed}
}

// answerEnvRequest answers requests on the Go side. scratch is a directory
// for the directory absolute_deleted_cwd deletes; it is chosen before the
// requests change TMPDIR.
func answerEnvRequest(t *testing.T, scratch string) func(json.RawMessage) any {
	return func(raw json.RawMessage) any {
		var request envRequest
		if err := json.Unmarshal(raw, &request); err != nil {
			t.Fatalf("oracle request does not decode: %v\n%s", err, raw)
		}
		switch request.Op {
		case "split_paths":
			parts := make([]any, 0)
			for part := range SplitPaths(fromHex(t, *request.S)).All() {
				parts = append(parts, toHex(part))
			}
			return parts
		case "join_paths":
			parts := make([]string, 0, len(request.Parts))
			for _, part := range request.Parts {
				parts = append(parts, fromHex(t, part))
			}
			joined, err := JoinPaths(parts)
			if err != nil {
				return map[string]any{"err": agree(err.Error(), rs.ToString(err)), "debug": rs.DebugString(err)}
			}
			return map[string]any{"ok": toHex(joined)}
		case "var_error":
			var err VarError = &VarError_NotPresent{}
			if request.S != nil {
				err = &VarError_NotUnicode{F0: fromHex(t, *request.S)}
			}
			return map[string]any{"display": agree(err.Error(), rs.ToString(err)), "debug": rs.DebugString(err)}
		case "env_get":
			return envGetReply(fromHex(t, request.K))
		case "env_set_get":
			return envSetGetReply(fromHex(t, request.K), fromHex(t, request.V))
		case "env_remove":
			if message, panicked := caught(func() { RemoveVar(fromHex(t, request.K)) }); panicked {
				return map[string]any{"panic": message}
			}
			return map[string]any{"ok": true}
		case "chdir":
			if err := SetCurrentDir(fromHex(t, request.P)); err != nil {
				return ioErrorReply(t, err)
			}
			current, err := CurrentDir()
			if err != nil {
				return ioErrorReply(t, err)
			}
			return map[string]any{"ok": toHex(current)}
		case "absolute_deleted_cwd":
			// From a directory that no longer exists: getcwd fails.
			gone, err := os.MkdirTemp(scratch, "mj-rsenv-gone-")
			if err != nil {
				t.Fatal(err)
			}
			if err := os.Chdir(gone); err != nil {
				t.Fatal(err)
			}
			if err := os.Remove(gone); err != nil {
				t.Fatal(err)
			}
			reply := map[string]any{}
			if absolute, err := rspath.Absolute(fromHex(t, request.P)); err != nil {
				reply["absolute"] = ioErrorReply(t, err)
			} else {
				reply["absolute"] = map[string]any{"ok": toHex(absolute)}
			}
			if current, err := CurrentDir(); err != nil {
				reply["current_dir"] = ioErrorReply(t, err)
			} else {
				reply["current_dir"] = map[string]any{"ok": toHex(current)}
			}
			if err := os.Chdir("/"); err != nil {
				t.Fatal(err)
			}
			return reply
		case "temp_dir":
			if request.Tmpdir != nil {
				SetVar("TMPDIR", fromHex(t, *request.Tmpdir))
			} else {
				RemoveVar("TMPDIR")
			}
			return toHex(TempDir())
		case "home_dir":
			if request.Home != nil {
				SetVar("HOME", fromHex(t, *request.Home))
			} else {
				RemoveVar("HOME")
			}
			return optionalHex(HomeDir())
		case "consts":
			separators := make([]bool, 0)
			for _, c := range []rune{'/', '\\', ':', '.', 'a', '\u2215'} {
				separators = append(separators, rspath.IsSeparator(c))
			}
			return map[string]any{
				"ARCH":               ConstsArch,
				"FAMILY":             ConstsFamily,
				"OS":                 ConstsOs,
				"DLL_PREFIX":         ConstsDllPrefix,
				"DLL_SUFFIX":         ConstsDllSuffix,
				"DLL_EXTENSION":      ConstsDllExtension,
				"EXE_SUFFIX":         ConstsExeSuffix,
				"EXE_EXTENSION":      ConstsExeExtension,
				"MAIN_SEPARATOR":     string(rspath.MainSeparator),
				"MAIN_SEPARATOR_STR": rspath.MainSeparatorStr,
				"is_separator":       separators,
			}
		case "current_exe":
			exe, err := CurrentExe()
			if err != nil {
				return ioErrorReply(t, err)
			}
			args0 := ArgsOs().Next()
			return map[string]any{
				"is_absolute": rspath.IsAbsolute(exe),
				"file_name_is_args0_file_name": rspath.FileName(exe) ==
					rs.OptionAndThen(args0, rspath.FileName),
			}
		default:
			t.Fatalf("unknown oracle operation %q", request.Op)
			return nil
		}
	}
}

var envPieces = []string{
	":", ":", "::", "/", "/usr", "bin", "a", "é", "日本", " ", "\xff", "\xc3", "\xed\xa0\x80", ";", "\"", "\\",
	"=", ".", "..", "\t", "\n", "~", "$HOME", "%PATH%", "🚀",
}

func envGenerate(yield func(any)) {
	rng := rand.New(rand.NewPCG(0x6d6a2d727365, 0x6e76))
	random := func(pieces []string, most int) string {
		var b strings.Builder
		for range rng.IntN(most + 1) {
			b.WriteString(pieces[rng.IntN(len(pieces))])
		}
		return b.String()
	}
	optional := func(s string) *string { return hexPointer(s) }

	// Fixed cases first.
	for _, s := range []string{
		"", ":", "::", ":::", "/", "/:", ":/", "/:/usr/local", "a::b", ":a:", "/bin:/usr/bin:", "\xff:\xfe",
		"é:日本", "a;b", `"a:b"`, `c:\;d`, strings.Repeat("/x:", 40), strings.Repeat(":", 70),
	} {
		yield(envRequest{Op: "split_paths", S: optional(s)})
	}
	for _, parts := range [][]string{
		{}, {""}, {"", ""}, {"/bin"}, {"/bin", "/usr/bin", "/usr/local/bin"}, {"", "/bin", "", "", "/usr/bin", ""},
		{"/te:st"}, {"a", ":"}, {":"}, {"a;b", `c"d`}, {"\xff", "\xfe"}, {"é", "日本"}, {"/ok", "/bad:", "/ok"},
		{strings.Repeat("x", 300), strings.Repeat("y", 300)},
	} {
		encoded := make([]string, 0, len(parts))
		for _, part := range parts {
			encoded = append(encoded, toHex(part))
		}
		yield(envRequest{Op: "join_paths", Parts: encoded})
	}
	yield(envRequest{Op: "var_error"})
	for _, s := range []string{"", "x", "\xff", "a\xffb", "é", "\x00", "\"quoted\"", "tab\there", "\xed\xa0\x80", "\u200b"} {
		yield(envRequest{Op: "var_error", S: optional(s)})
	}

	// Reading variables this test set, and ones nobody sets.
	for _, k := range []string{
		oracleEqName, oracleEqName + "=B", oracleEqName + "=", oracleEqName + "=B=", oracleEqName + "=B=C",
		"MJ_RSENV_ORACLE_ABSENT", "", "=", "=" + oracleEqName,
		"MJ_RSENV\x00ORACLE", "\xff", strings.Repeat("K", 1000),
	} {
		yield(envRequest{Op: "env_get", K: toHex(k)})
	}

	// Setting, reading back and removing.
	keys := []string{
		"MJ_RSENV_SET_A", "", "=", "=A", "A=B", "A\x00B", "\xff\xfe", "é", "MJ RSENV SPACE",
		"MJ_RSENV_" + strings.Repeat("L", 1000),
	}
	values := []string{"", "x", "a=b", "=", "\xff", "a\x00b", "日本", strings.Repeat("v", 10000), "\n", " lead"}
	for index, k := range keys {
		for _, v := range []string{values[index%len(values)], values[(index+3)%len(values)]} {
			yield(envRequest{Op: "env_set_get", K: toHex(k), V: toHex(v)})
		}
	}
	for _, v := range values {
		yield(envRequest{Op: "env_set_get", K: toHex("MJ_RSENV_SET_B"), V: toHex(v)})
	}
	for _, k := range []string{"", "=", "A=B", "=A", "a\x00", "MJ_RSENV_ORACLE_ABSENT", "\xff"} {
		yield(envRequest{Op: "env_remove", K: toHex(k)})
	}

	// The current directory: from a deleted one, then from the root on.
	yield(envRequest{Op: "absolute_deleted_cwd", P: toHex("a")})
	yield(envRequest{Op: "absolute_deleted_cwd", P: toHex("/a/../b/")})
	for _, p := range []string{
		"/", "/usr", "..", ".", "usr", "/usr/", "//", "/usr//.", "", "/nonexistent-mj-rsenv", "nonexistent-mj-rsenv",
		"a\x00b", "/etc/passwd", "/etc/passwd/", "/", "\xff",
	} {
		yield(envRequest{Op: "chdir", P: toHex(p)})
	}

	for _, value := range []*string{nil, optional(""), optional("/x"), optional("relative"), optional("\xff"), optional("/tmp/a b/")} {
		yield(envRequest{Op: "temp_dir", Tmpdir: value})
	}
	for _, value := range []string{"/h", "relative", "\xff", "/home/MountainView", " "} {
		yield(envRequest{Op: "home_dir", Home: optional(value)})
	}
	yield(envRequest{Op: "current_exe"})

	// Then seeded random ones.
	for range 300 {
		yield(envRequest{Op: "split_paths", S: optional(random(envPieces, 8))})
	}
	for range 300 {
		parts := make([]string, rng.IntN(5))
		for index := range parts {
			parts[index] = toHex(random(envPieces, 4))
		}
		yield(envRequest{Op: "join_paths", Parts: parts})
	}
	for range 100 {
		yield(envRequest{Op: "var_error", S: optional(random(envPieces, 6))})
	}
	for range 100 {
		yield(envRequest{Op: "env_set_get", K: toHex("MJ_RSENV_RANDOM" + random(envPieces, 3)), V: toHex(random(envPieces, 6))})
	}

	if os.Getenv("MJ_ORACLE") == "live" {
		// What depends on the machine: the build target, and the home
		// directory of the user database entry (HOME unset or empty).
		yield(envRequest{Op: "consts"})
		yield(envRequest{Op: "home_dir"})
		yield(envRequest{Op: "home_dir", Home: optional("")})
	}
}

func TestDiffAgainstRust(t *testing.T) {
	start, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	saved := map[string]rs.Option[string]{}
	for _, name := range []string{"TMPDIR", "HOME", oracleEqName} {
		saved[name] = VarOs(name)
	}
	defer func() {
		if err := os.Chdir(start); err != nil {
			t.Error(err)
		}
		for name, value := range saved {
			if v, ok := value.Get(); ok {
				SetVar(name, v)
			} else {
				RemoveVar(name)
			}
		}
	}()

	SetVar(oracleEqName, oracleEqValue)

	oracle.Check(t, "std_path", "std_env_linux", envGenerate, answerEnvRequest(t, t.TempDir()))
}
