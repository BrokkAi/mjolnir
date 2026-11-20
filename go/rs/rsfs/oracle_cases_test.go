//go:build unix

package rsfs_test

import (
	"encoding/json"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"testing"

	"golang.org/x/sys/unix"

	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// corpusFor names the corpus of this system: the answers hold the text of
// operating system errors and depend on the C library and on flag values,
// so a corpus belongs to one system and architecture. The test is skipped
// where none was recorded and none is being made, and when it runs as root,
// because the corpus was recorded by an ordinary user and permissions do
// not stop root.
func corpusFor(t *testing.T, name string) string {
	t.Helper()
	corpus := name + "_" + runtime.GOOS + "_" + runtime.GOARCH
	mode := os.Getenv("MJ_ORACLE")
	if mode != "record" && mode != "live" {
		if _, err := os.Stat(filepath.Join("testdata", "oracle", corpus+".jsonl.gz")); err != nil {
			t.Skipf("no oracle corpus was recorded on %s/%s; record one there with MJ_ORACLE=record", runtime.GOOS, runtime.GOARCH)
		}
	}
	if os.Geteuid() == 0 {
		t.Skip("the corpus was recorded by an unprivileged user; root is not refused where it is")
	}
	return corpus
}

// rng is splitmix64, so that the requests do not depend on the Go release.
type rng struct{ state uint64 }

func (r *rng) next() uint64 {
	r.state += 0x9e3779b97f4a7c15
	z := r.state
	z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9
	z = (z ^ (z >> 27)) * 0x94d049bb133111eb
	return z ^ (z >> 31)
}

func (r *rng) intn(n int) int { return int(r.next() % uint64(n)) }

func pick[T any](r *rng, options ...T) T { return options[r.intn(len(options))] }

// st is a step: the action and its fields. p, q and data given as text are
// sent as their hexadecimal bytes.
func st(do string, fields ...any) map[string]any {
	step := map[string]any{"do": do}
	for i := 0; i < len(fields); i += 2 {
		key := fields[i].(string)
		value := fields[i+1]
		if text, ok := value.(string); ok && (key == "p" || key == "q" || key == "data") {
			value = hexOf([]byte(text))
		}
		step[key] = value
	}
	return step
}

func fsScript(umask int, steps ...map[string]any) map[string]any {
	list := make([]any, len(steps))
	for i, step := range steps {
		list[i] = step
	}
	return map[string]any{"op": "fs", "umask": umask, "steps": list}
}

// pathSteps applies every path function to p.
func pathSteps(p string) []map[string]any {
	return []map[string]any{
		st("read", "p", p), st("read_to_string", "p", p), st("metadata", "p", p),
		st("symlink_metadata", "p", p), st("exists", "p", p), st("try_exists", "p", p),
		st("path_exists", "p", p), st("path_is_file", "p", p), st("path_is_dir", "p", p),
		st("path_is_symlink", "p", p), st("read_link", "p", p), st("canonicalize", "p", p),
		st("read_dir", "p", p), st("debug_file_type", "p", p), st("file_open", "p", p, "f", "x"),
		st("close", "f", "x"),
	}
}

// mutatingSteps applies every function that changes the file system to p,
// in an order that keeps the later ones meaningful.
func mutatingSteps(p string) []map[string]any {
	return []map[string]any{
		st("set_permissions", "p", p, "mode", 0o644), st("set_readonly", "p", p, "readonly", true),
		st("chown_none", "p", p), st("copy", "p", p, "q", "copied"), st("hard_link", "p", p, "q", "linked"),
		st("rename", "p", p, "q", "renamed"), st("remove_file", "p", p), st("remove_dir", "p", p),
		st("remove_dir_all", "p", p), st("create_dir", "p", p), st("create_dir_all", "p", p),
		st("write", "p", p, "data", "w"), st("file_create", "p", p, "f", "x"), st("close", "f", "x"),
		st("file_create_new", "p", p, "f", "x"), st("close", "f", "x"),
	}
}

// withoutStep is the steps without those of one action.
func withoutStep(list []map[string]any, action string) []map[string]any {
	var out []map[string]any
	for _, step := range list {
		if step["do"] != action {
			out = append(out, step)
		}
	}
	return out
}

func steps(groups ...[]map[string]any) []map[string]any {
	var out []map[string]any
	for _, group := range groups {
		out = append(out, group...)
	}
	return out
}

const (
	oNonblock = unix.O_NONBLOCK
	oNofollow = unix.O_NOFOLLOW
)

func generateFs(yield func(any)) {
	// Paths that do not name anything, in every function.
	yield(fsScript(0o022, steps(pathSteps("missing"), pathSteps("missing/x"), pathSteps("$ROOT/missing"))...))
	yield(fsScript(0o022, steps(mutatingSteps("missing/x"))...))
	// The empty path, and paths holding a NUL byte.
	yield(fsScript(0o022, steps(pathSteps(""), mutatingSteps(""))...))
	yield(fsScript(0o022, steps(pathSteps("a\x00b"), mutatingSteps("a\x00b"),
		[]map[string]any{st("write", "p", "ok", "data", "x"), st("copy", "p", "ok", "q", "c\x00d"),
			st("rename", "p", "ok", "q", "c\x00d"), st("hard_link", "p", "ok", "q", "c\x00d"),
			st("symlink", "p", "ok", "q", "c\x00d"), st("symlink", "p", "o\x00k", "q", "link")})...))

	// A file, and functions that want a directory.
	yield(fsScript(0o022, steps(
		[]map[string]any{st("write", "p", "a.txt", "data", "hello\n")},
		pathSteps("a.txt"), pathSteps("a.txt/"), pathSteps("a.txt/x"), pathSteps("a.txt/."), pathSteps("a.txt/.."),
		[]map[string]any{
			st("create_dir", "p", "a.txt"), st("create_dir_all", "p", "a.txt"), st("create_dir_all", "p", "a.txt/x/y"),
			st("remove_dir", "p", "a.txt"), st("remove_dir_all", "p", "a.txt"), st("path_exists", "p", "a.txt"),
			st("write", "p", "a.txt/x", "data", "x"), st("create_dir", "p", "a.txt/x"),
		})...))

	// A directory, and functions that want a file.
	yield(fsScript(0o022, steps(
		[]map[string]any{st("create_dir", "p", "d"), st("write", "p", "d/f", "data", "f")},
		pathSteps("d"), pathSteps("d/"), pathSteps("d/."), pathSteps("d/.."), pathSteps("$ROOT/d"),
		[]map[string]any{
			st("create_dir", "p", "d"), st("remove_file", "p", "d"), st("remove_dir", "p", "d"),
			st("write", "p", "d", "data", "x"), st("file_create", "p", "d", "f", "x"),
			st("open", "p", "d", "write", true, "f", "x"), st("open", "p", "d", "read", true, "f", "x"),
			st("file_read", "f", "x", "n", 4), st("file_read", "f", "x", "n", 0), st("file_metadata", "f", "x"),
			st("file_read_to_end", "f", "x"), st("close", "f", "x"),
			st("copy", "p", "d", "q", "e"), st("copy", "p", "d/f", "q", "d"), st("hard_link", "p", "d", "q", "e"),
			st("rename", "p", "d/f", "q", "d"), st("remove_dir_all", "p", "d"), st("path_exists", "p", "d"),
		})...))

	// create_dir_all on the forms of a path.
	yield(fsScript(0o022,
		st("create_dir_all", "p", "x/y/z"), st("create_dir_all", "p", "x/y/z"), st("create_dir_all", "p", "x//y///w/"),
		st("create_dir_all", "p", "x/./v/../u"), st("create_dir_all", "p", "."), st("create_dir_all", "p", ".."),
		st("create_dir_all", "p", "/"), st("create_dir_all", "p", "$ROOT/abs/olute"), st("create_dir_all", "p", "./rel/"),
		st("read_dir", "p", "x"), st("read_dir", "p", "x/y"), st("read_dir", "p", "."), st("create_dir", "p", "x/y"),
		st("create_dir", "p", "new/"), st("create_dir", "p", "new2/."), st("path_is_dir", "p", "x/v"),
		st("dir_builder", "p", "b1/b2", "recursive", false), st("dir_builder", "p", "b1/b2", "recursive", true, "mode", 0o750),
		st("metadata", "p", "b1"), st("metadata", "p", "b1/b2"), st("dir_builder", "p", "b3", "mode", 0o711),
		st("metadata", "p", "b3"), st("dir_builder", "p", "b3", "recursive", true), st("dir_builder", "p", "b3"),
	))

	// Directories that are not empty, and what removes them.
	yield(fsScript(0o022,
		st("create_dir_all", "p", "t/a/b/c"), st("write", "p", "t/a/f", "data", "1"), st("write", "p", "t/a/b/g", "data", "2"),
		st("write", "p", "outside", "data", "keep"), st("symlink", "p", "../../outside", "q", "t/a/link"),
		st("create_dir", "p", "kept"), st("write", "p", "kept/x", "data", "kept"), st("symlink", "p", "$ROOT/kept", "q", "t/abslink"),
		st("remove_dir", "p", "t"), st("remove_dir", "p", "t/a/b/c"), st("remove_dir_all", "p", "t"),
		st("path_exists", "p", "t"), st("read", "p", "outside"), st("read", "p", "kept/x"),
		st("symlink", "p", "kept", "q", "klink"), st("remove_dir_all", "p", "klink"), st("path_exists", "p", "kept/x"),
		st("path_is_symlink", "p", "klink"), st("remove_dir_all", "p", "kept/x"), st("remove_dir_all", "p", "kept"),
		st("remove_dir_all", "p", "kept"), st("create_dir_all", "p", "dot/inner"), st("write", "p", "dot/f", "data", "f"),
		st("remove_dir_all", "p", "dot/."), st("read_dir", "p", "dot"), st("remove_dir_all", "p", "dot/inner/.."),
		st("path_exists", "p", "dot"),
	))

	// Permissions: what an ordinary user is refused.
	yield(fsScript(0o022,
		st("write", "p", "secret", "data", "s"), st("set_permissions", "p", "secret", "mode", 0),
		st("read", "p", "secret"), st("file_open", "p", "secret", "f", "x"), st("write", "p", "secret", "data", "t"),
		st("copy", "p", "secret", "q", "copy"), st("metadata", "p", "secret"), st("set_permissions", "p", "secret", "mode", 0o600),
		st("read", "p", "secret"), st("set_readonly", "p", "secret", "readonly", true), st("write", "p", "secret", "data", "t"),
		st("open", "p", "secret", "append", true, "f", "x"), st("set_readonly", "p", "secret", "readonly", false),
		st("metadata", "p", "secret"),
		st("create_dir_all", "p", "locked/inner"), st("write", "p", "locked/file", "data", "f"),
		st("set_permissions", "p", "locked", "mode", 0),
		st("read_dir", "p", "locked"), st("metadata", "p", "locked/file"), st("exists", "p", "locked/file"),
		st("try_exists", "p", "locked/file"), st("path_exists", "p", "locked/file"), st("canonicalize", "p", "locked/file"),
		st("canonicalize", "p", "locked/missing"), st("canonicalize", "p", "locked/.."), st("remove_dir_all", "p", "locked"),
		st("create_dir_all", "p", "locked/more"), st("set_permissions", "p", "locked", "mode", 0o500),
		st("remove_dir_all", "p", "locked"), st("read_dir", "p", "locked"), st("write", "p", "locked/new", "data", "n"),
		st("set_permissions", "p", "locked", "mode", 0o300), st("read_dir", "p", "locked"),
		st("canonicalize", "p", "locked/file"), st("canonicalize", "p", "locked/inner/"), st("remove_dir_all", "p", "locked"),
		st("set_permissions", "p", "locked", "mode", 0o700), st("remove_dir_all", "p", "locked"), st("path_exists", "p", "locked"),
		st("chown_self", "p", "secret"), st("chown_none", "p", "secret"), st("lchown_self", "p", "secret"),
		st("chown_root", "p", "secret"), st("chown_root", "p", "missing"),
	))

	// The umask and the modes files and directories are created with.
	for _, umask := range []int{0, 0o022, 0o077, 0o777} {
		yield(fsScript(umask,
			st("write", "p", "w", "data", "x"), st("metadata", "p", "w"), st("create_dir", "p", "d"), st("metadata", "p", "d"),
			st("open", "p", "m", "write", true, "create", true, "mode", 0o741, "f", "x"), st("file_metadata", "f", "x"),
			st("close", "f", "x"), st("open", "p", "m2", "write", true, "create_new", true, "mode", 0o4755, "f", "x"),
			st("metadata", "p", "m2"), st("dir_builder", "p", "e", "mode", 0o1777), st("metadata", "p", "e"),
			st("copy", "p", "m", "q", "mcopy"), st("metadata", "p", "mcopy"),
		))
	}

	// Every combination of the open options, on a file that exists and on
	// one that does not.
	for _, exists := range []bool{true, false} {
		var list []map[string]any
		for bits := 0; bits < 64; bits++ {
			path := "o" + strconv.Itoa(bits)
			if exists {
				list = append(list, st("write", "p", path, "data", "existing"))
			}
			list = append(list,
				st("open", "p", path, "f", "x", "read", bits&1 != 0, "write", bits&2 != 0, "append", bits&4 != 0,
					"truncate", bits&8 != 0, "create", bits&16 != 0, "create_new", bits&32 != 0),
				st("file_write", "f", "x", "data", "new"), st("file_seek", "f", "x", "whence", "start", "offset", 0),
				st("file_read", "f", "x", "n", 32), st("close", "f", "x"), st("read", "p", path))
		}
		yield(fsScript(0o022, list...))
	}

	// One open file, through every File operation.
	yield(fsScript(0o022,
		st("file_create", "p", "f", "f", "f"), st("file_write", "f", "f", "data", "hello world"),
		st("file_stream_position", "f", "f"), st("file_seek", "f", "f", "whence", "start", "offset", 0),
		st("file_read", "f", "f", "n", 3), st("file_read", "f", "f", "n", 0), st("file_write", "f", "f", "data", ""),
		st("file_read_exact", "f", "f", "n", 4), st("file_read_exact", "f", "f", "n", 50),
		st("file_seek", "f", "f", "whence", "current", "offset", -4), st("file_read_to_end", "f", "f", "prefix", hexOf([]byte("p:"))),
		st("file_seek", "f", "f", "whence", "end", "offset", -5), st("file_read_to_string", "f", "f", "prefix", "s:"),
		st("file_seek", "f", "f", "whence", "current", "offset", -100), st("file_seek", "f", "f", "whence", "start", "offset", uint64(1)<<63),
		st("file_seek", "f", "f", "whence", "end", "offset", 100), st("file_stream_position", "f", "f"),
		st("file_stream_len_matches", "f", "f"), st("file_write", "f", "f", "data", "!"), st("file_rewind", "f", "f"),
		st("file_read_to_end", "f", "f"), st("file_set_len", "f", "f", "len", 5), st("file_metadata", "f", "f"),
		st("file_set_len", "f", "f", "len", 1<<20), st("file_metadata", "f", "f"), st("file_set_len", "f", "f", "len", uint64(1)<<63),
		st("file_set_len", "f", "f", "len", ^uint64(0)), st("file_sync_all", "f", "f"), st("file_sync_data", "f", "f"),
		st("file_flush", "f", "f"), st("file_is_terminal", "f", "f"), st("file_cloexec", "f", "f"), st("file_debug", "f", "f"),
		st("file_try_clone", "f", "f", "as", "g"), st("file_cloexec", "f", "g"), st("file_seek", "f", "g", "whence", "start", "offset", 1),
		st("file_stream_position", "f", "f"), st("file_debug", "f", "g"), st("file_set_permissions", "f", "f", "mode", 0o640),
		st("file_metadata", "f", "g"), st("file_fchown_self", "f", "f"),
		st("file_set_modified", "f", "f", "secs", 1_000_000_000, "nanos", 123_456_789), st("file_modified", "f", "f"),
		st("file_set_times", "f", "f", "asecs", 1_500_000_000, "ananos", 7, "secs", 1_600_000_000, "nanos", 999_999_999),
		st("file_accessed", "f", "f"), st("file_modified", "f", "f"),
		st("file_set_times", "f", "f", "secs", -1, "nanos", 500_000_000), st("file_modified", "f", "f"),
		st("file_set_times", "f", "f", "secs", -86_400, "nanos", 0), st("file_modified", "f", "f"),
		st("file_set_times", "f", "f"), st("file_modified", "f", "f"),
		st("file_write_at", "f", "f", "data", "XY", "offset", 2), st("file_read_at", "f", "f", "n", 8, "offset", 0),
		st("file_read_exact_at", "f", "f", "n", 3, "offset", 1), st("file_read_exact_at", "f", "f", "n", 20, "offset", 1),
		st("file_write_all_at", "f", "f", "data", "tail", "offset", 30), st("file_read_at", "f", "f", "n", 8, "offset", 1<<40),
		st("file_read_at", "f", "f", "n", 8, "offset", uint64(1)<<63), st("file_write_at", "f", "f", "data", "x", "offset", uint64(1)<<63),
		st("file_stream_position", "f", "f"), st("metadata", "p", "f"), st("read", "p", "f"),
		st("file_open", "p", "f", "f", "r"), st("file_write", "f", "r", "data", "no"), st("file_write_all", "f", "r", "data", "no"),
		st("file_set_len", "f", "r", "len", 0), st("file_debug", "f", "r"),
		st("open", "p", "f", "write", true, "f", "w"), st("file_read", "f", "w", "n", 4), st("file_read", "f", "w", "n", 0),
		st("file_read_to_end", "f", "w"), st("file_read_to_string", "f", "w"), st("file_debug", "f", "w"),
		st("open", "p", "f", "append", true, "f", "a"), st("file_write", "f", "a", "data", "+app"),
		st("file_seek", "f", "a", "whence", "start", "offset", 0), st("file_write", "f", "a", "data", "+more"),
		st("file_stream_position", "f", "a"), st("read", "p", "f"), st("file_debug", "f", "a"),
	))

	// Text that is not UTF-8.
	yield(fsScript(0o022,
		st("write", "p", "bad", "data", "ok \xff\xfe bad"), st("read_to_string", "p", "bad"), st("read", "p", "bad"),
		st("write", "p", "cut", "data", "caf\xc3"), st("read_to_string", "p", "cut"),
		st("write", "p", "good", "data", "caf\xc3\xa9 \xe2\x82\xac"), st("read_to_string", "p", "good"),
		st("file_open", "p", "bad", "f", "x"), st("file_read_to_string", "f", "x", "prefix", "before"),
		st("file_seek", "f", "x", "whence", "start", "offset", 0), st("file_read_to_end", "f", "x"),
		st("write", "p", "empty", "data", ""), st("read_to_string", "p", "empty"), st("read", "p", "empty"),
	))

	// Locks between two handles of one file.
	yield(fsScript(0o022,
		st("write", "p", "l", "data", "lock"), st("file_open", "p", "l", "f", "a"), st("file_open", "p", "l", "f", "b"),
		st("file_try_lock", "f", "a"), st("file_try_lock", "f", "b"), st("file_try_lock_shared", "f", "b"),
		st("file_unlock", "f", "a"), st("file_try_lock_shared", "f", "a"), st("file_try_lock_shared", "f", "b"),
		st("file_try_lock", "f", "b"), st("file_unlock", "f", "a"), st("file_unlock", "f", "b"), st("file_lock", "f", "b"),
		st("file_try_lock", "f", "a"), st("file_unlock", "f", "b"), st("file_lock_shared", "f", "a"),
		st("file_try_lock", "f", "b"), st("file_unlock", "f", "a"), st("file_unlock", "f", "a"),
		st("file_try_clone", "f", "a", "as", "c"), st("file_try_lock", "f", "a"), st("file_try_lock", "f", "c"),
		st("try_lock_error_debug", "code", 11), st("try_lock_error_debug", "code", 9), st("try_lock_error_debug", "code", 0),
	))

	// Copies.
	yield(fsScript(0o022,
		st("write", "p", "src", "data", "source"), st("set_permissions", "p", "src", "mode", 0o751),
		st("copy", "p", "src", "q", "dst"), st("metadata", "p", "dst"), st("read", "p", "dst"),
		st("write", "p", "old", "data", "a much longer old file"), st("set_permissions", "p", "old", "mode", 0o600),
		st("copy", "p", "src", "q", "old"), st("metadata", "p", "old"), st("read", "p", "old"),
		st("create_dir", "p", "dir"), st("copy", "p", "dir", "q", "x"), st("copy", "p", "src", "q", "dir"),
		st("symlink", "p", "src", "q", "link"), st("copy", "p", "link", "q", "fromlink"), st("symlink_metadata", "p", "fromlink"),
		st("symlink", "p", "nowhere", "q", "dangling"), st("copy", "p", "dangling", "q", "x2"), st("copy", "p", "src", "q", "dangling"),
		st("read", "p", "nowhere"), st("write", "p", "empty", "data", ""), st("copy", "p", "empty", "q", "empty2"),
		st("metadata", "p", "empty2"), st("copy", "p", "src", "q", "src"), st("read", "p", "src"),
		st("write", "p", "big", "data", map[string]any{"lcg": 7, "len": 200_000}), st("copy", "p", "big", "q", "big2"),
		st("read_show", "p", "big2"), st("metadata", "p", "big2"), st("copy", "p", "big", "q", "missing/dir/x"),
		st("set_permissions", "p", "src", "mode", 0o200), st("copy", "p", "src", "q", "unreadable"),
	))

	// Renames.
	yield(fsScript(0o022,
		st("write", "p", "f", "data", "f"), st("write", "p", "g", "data", "g"), st("create_dir_all", "p", "d/sub"),
		st("create_dir", "p", "empty"), st("create_dir", "p", "full"), st("write", "p", "full/x", "data", "x"),
		st("rename", "p", "f", "q", "f2"), st("rename", "p", "g", "q", "f2"), st("read", "p", "f2"),
		st("rename", "p", "f2", "q", "d"), st("rename", "p", "d", "q", "f2"), st("rename", "p", "d", "q", "empty"),
		st("rename", "p", "full", "q", "empty"), st("rename", "p", "empty", "q", "empty/sub"), st("rename", "p", "full", "q", "full"),
		st("rename", "p", "missing", "q", "x"), st("rename", "p", "f2", "q", "missing/x"), st("rename", "p", "f2", "q", "f2/"),
		st("rename", "p", "empty/", "q", "moved/"), st("read_dir", "p", "."),
	))

	// Hard links.
	yield(fsScript(0o022,
		st("write", "p", "f", "data", "f"), st("hard_link", "p", "f", "q", "h"), st("metadata", "p", "f"),
		st("hard_link", "p", "f", "q", "h"), st("create_dir", "p", "d"), st("hard_link", "p", "d", "q", "dh"),
		st("symlink", "p", "f", "q", "s"), st("hard_link", "p", "s", "q", "sh"), st("symlink_metadata", "p", "sh"),
		st("symlink", "p", "gone", "q", "s2"), st("hard_link", "p", "s2", "q", "s2h"), st("symlink_metadata", "p", "s2h"),
		st("remove_file", "p", "f"), st("read", "p", "h"), st("metadata", "p", "h"),
	))

	// Symbolic links.
	yield(fsScript(0o022, steps(
		[]map[string]any{
			st("write", "p", "target", "data", "t"), st("create_dir", "p", "dir"),
			st("symlink", "p", "target", "q", "rel"), st("symlink", "p", "$ROOT/target", "q", "abs"),
			st("symlink", "p", "dir", "q", "dlink"), st("symlink", "p", "dir/", "q", "dslash"),
			st("symlink", "p", "nowhere", "q", "dangling"), st("symlink", "p", "loop2", "q", "loop1"),
			st("symlink", "p", "loop1", "q", "loop2"), st("symlink", "p", "self", "q", "self"),
			st("symlink", "p", "..", "q", "dir/up"), st("symlink", "p", ".", "q", "here"), st("symlink", "p", "", "q", "emptylink"),
			st("symlink", "p", "target", "q", "rel"), st("symlink", "p", "target", "q", "dir"),
			st("symlink", "p", "target/x", "q", "through"),
		},
		// The size of a link is the length of its target, and the target of
		// abs holds the script's directory, whose length differs between
		// processes: abs is not lstat'ed.
		pathSteps("rel"), withoutStep(pathSteps("abs"), "symlink_metadata"), pathSteps("dlink"), pathSteps("dlink/"),
		pathSteps("dslash"),
		pathSteps("dangling"), pathSteps("dangling/"), pathSteps("loop1"), pathSteps("loop1/x"), pathSteps("self"),
		pathSteps("dir/up"), pathSteps("dir/up/"), pathSteps("here/here/rel"), pathSteps("through"),
		[]map[string]any{
			st("open", "p", "rel", "read", true, "custom_flags", oNofollow, "f", "x"),
			st("open", "p", "target", "read", true, "custom_flags", oNofollow, "f", "x"), st("close", "f", "x"),
			st("open", "p", "dangling", "write", true, "create", true, "f", "x"), st("close", "f", "x"),
			st("read", "p", "nowhere"), st("open", "p", "rel", "write", true, "create_new", true, "f", "x"),
			st("write", "p", "loop1", "data", "x"), st("remove_file", "p", "dlink"), st("path_is_dir", "p", "dir"),
			st("remove_file", "p", "loop1"), st("canonicalize", "p", "loop2"), st("symlink_metadata", "p", "emptylink"),
			st("debug_file_type", "p", "rel"), st("debug_file_type", "p", "dir"), st("debug_file_type", "p", "target"),
		})...))

	// Chains of symbolic links around the C libraries' limits (musl stops
	// at 40 links, glibc after 40, Linux after 40).
	{
		list := []map[string]any{st("write", "p", "t", "data", "t")}
		for i := 45; i >= 0; i-- {
			target := "c" + strconv.Itoa(i+1)
			if i == 45 {
				target = "t"
			}
			list = append(list, st("symlink", "p", target, "q", "c"+strconv.Itoa(i)))
		}
		for _, i := range []int{0, 3, 4, 5, 6, 7, 8, 30, 45} {
			c := "c" + strconv.Itoa(i)
			list = append(list, st("canonicalize", "p", c), st("metadata", "p", c), st("read", "p", c),
				st("canonicalize", "p", c+"/"), st("try_exists", "p", c))
		}
		yield(fsScript(0o022, list...))
	}

	// canonicalize on the forms of a path.
	{
		list := []map[string]any{
			st("create_dir_all", "p", "a/b/c"), st("write", "p", "a/f", "data", "f"), st("symlink", "p", "b/c", "q", "a/lc"),
			st("symlink", "p", "$ROOT/a", "q", "la"), st("symlink", "p", "../a/f", "q", "a/b/lf"),
			st("symlink", "p", "/", "q", "lroot"), st("symlink", "p", "//", "q", "lroot2"), st("symlink", "p", "a//b/", "q", "lb"),
		}
		for _, p := range []string{".", "./", "a/..", "a/b/../..", "../../../../../../../../..", "/", "//", "///", "$ROOT",
			"$ROOT/", "$ROOT//a//b//", "$ROOT/./a/./b/.", "$ROOT/a/b/../../a", "a", "a/", "a/.", "a/..", "a/b/c/../../..",
			"a/f", "a/f/", "a/f/.", "a/f/..", "a/missing/..", "a/missing/../f", "a/lc", "a/lc/..", "a/lc/../..",
			"la", "la/b", "la/../la/f", "a/b/lf", "a/b/lf/", "lroot", "lroot/.", "lroot2", "lroot2/", "lb", "lb/c/..",
			"missing", "missing/", "./missing", "a/f/missing", "$ROOT/missing/x/..",
			strings.Repeat("x", 255), strings.Repeat("x", 256), "a/" + strings.Repeat("y", 300) + "/..",
			strings.Repeat("a/b/../", 600), strings.Repeat("./", 2100) + "a", strings.Repeat("z/", 2100)} {
			list = append(list, st("canonicalize", "p", p), st("path_canonicalize", "p", p))
		}
		yield(fsScript(0o022, list...))
	}

	// Long names and long paths.
	{
		long := strings.Repeat("n", 255)
		var deep []string
		for i := 0; i < 15; i++ {
			deep = append(deep, strings.Repeat(string(rune('a'+i)), 200))
		}
		deepPath := strings.Join(deep, "/")
		yield(fsScript(0o022,
			st("write", "p", long, "data", "x"), st("read", "p", long), st("write", "p", long+"n", "data", "x"),
			st("create_dir", "p", long+"n"), st("metadata", "p", long+"n"), st("canonicalize", "p", long),
			st("create_dir_all", "p", deepPath), st("write", "p", deepPath+"/f", "data", "deep"), st("canonicalize", "p", deepPath+"/f"),
			st("symlink", "p", deepPath, "q", "deeplink"), st("canonicalize", "p", "deeplink/f"), st("read", "p", "deeplink/f"),
			st("create_dir_all", "p", deepPath+"/"+deepPath), st("create_dir_all", "p", strings.Repeat("q/", 2050)),
			st("read", "p", strings.Repeat("q/", 2050)+"f"), st("remove_dir_all", "p", deep[0]), st("path_exists", "p", deep[0]),
		))
	}

	// Directory listings with every kind of entry.
	yield(fsScript(0o022,
		st("create_dir", "p", "d"), st("write", "p", "d/file", "data", "f"), st("create_dir", "p", "d/sub"),
		st("symlink", "p", "file", "q", "d/lfile"), st("symlink", "p", "sub", "q", "d/lsub"), st("symlink", "p", "gone", "q", "d/ldangling"),
		st("mkfifo", "p", "d/fifo", "mode", 0o644), st("socket", "p", "d/sock"), st("write", "p", "d/.hidden", "data", "h"),
		st("write", "p", "d/n\xffo\xfe", "data", "not utf-8"), st("write", "p", "d/caf\xc3\xa9", "data", "utf-8"),
		st("read_dir", "p", "d"), st("path_read_dir", "p", "d/"), st("read_dir", "p", "$ROOT/d"), st("read_dir", "p", "d/sub"),
		st("debug_read_dir", "p", "d"), st("metadata", "p", "d/fifo"), st("metadata", "p", "d/sock"),
		st("debug_file_type", "p", "d/fifo"), st("debug_file_type", "p", "d/sock"), st("debug_metadata_shape", "p", "d/fifo"),
		st("read_dir", "p", "d/file"), st("read_dir", "p", "d/lsub"), st("read_dir", "p", "d/ldangling"),
		st("open", "p", "d/fifo", "read", true, "custom_flags", oNonblock, "f", "r"), st("file_read", "f", "r", "n", 8),
		st("open", "p", "d/fifo", "write", true, "custom_flags", oNonblock, "f", "w"), st("file_read", "f", "r", "n", 8),
		st("file_write", "f", "w", "data", "through"), st("file_read", "f", "r", "n", 4), st("file_read_to_end", "f", "r"),
		st("file_metadata", "f", "r"), st("file_debug", "f", "r"), st("close", "f", "w"), st("file_read", "f", "r", "n", 8),
		st("open", "p", "d/fifo", "write", true, "custom_flags", oNonblock, "f", "w2"), st("close", "f", "r"),
		st("open", "p", "d/fifo", "write", true, "custom_flags", oNonblock, "f", "w3"),
		st("copy", "p", "d/sock", "q", "x"), st("copy", "p", "d/fifo", "q", "x"), st("remove_dir_all", "p", "d"),
		st("path_exists", "p", "d"),
	))

	// Many entries: more than one buffer of directory entries.
	{
		list := []map[string]any{st("create_dir", "p", "many")}
		for i := 0; i < 300; i++ {
			list = append(list, st("write", "p", "many/entry-"+strconv.Itoa(i)+"-"+strings.Repeat("x", i%40), "data", ""))
		}
		list = append(list, st("read_dir", "p", "many"), st("remove_dir_all", "p", "many"), st("path_exists", "p", "many"))
		yield(fsScript(0o022, list...))
	}

	// The Debug forms.
	{
		var list []map[string]any
		for _, mode := range []int{0, 0o644, 0o100644, 0o100755, 0o40755, 0o41777, 0o40776, 0o41776, 0o120777, 0o104755,
			0o102755, 0o106000, 0o101000, 0o20666, 0o60660, 0o10644, 0o140755, 0o170777, 0o7777, 0o100000, 0o200000644} {
			list = append(list, st("debug_permissions", "mode", mode))
		}
		list = append(list, st("debug_open_options"), st("debug_open_options", "read", true, "write", true, "append", true,
			"truncate", true, "create", true, "create_new", true, "mode", 0o600, "custom_flags", -1),
			st("debug_open_options", "write", true, "mode", 0o100644, "custom_flags", oNofollow),
			st("debug_dir_builder"), st("debug_dir_builder", "recursive", true, "mode", 0o40700),
			st("debug_file_times"), st("debug_file_times", "secs", 1_000_000_000, "nanos", 5),
			st("debug_file_times", "secs", -2, "nanos", 999_999_999, "asecs", 0, "ananos", 0),
			st("write", "p", "f", "data", "f"), st("create_dir", "p", "d"), st("symlink", "p", "f", "q", "l"),
			st("debug_metadata_shape", "p", "f"), st("debug_metadata_shape", "p", "d"), st("debug_metadata_shape", "p", "l"),
			st("debug_read_dir", "p", "."), st("debug_read_dir", "p", "$ROOT"), st("debug_read_dir", "p", "missing"))
		yield(fsScript(0o022, list...))
	}

	// Seeded random scripts over a few names.
	random := &rng{state: 0x5eed_f5}
	// No name reaches above the script's directory, and neither can a link
	// made from these names: the scripts change nothing outside it, and
	// their answers depend only on what they created.
	names := []string{"a", "b", "d", "d/a", "d/e", "d/e/f", "l", "a/x", "", ".", "d/", "$ROOT/b", "l/a", "d/.."}
	for script := 0; script < 120; script++ {
		var list []map[string]any
		for i := 0; i < 24; i++ {
			p, q := pick(random, names...), pick(random, names...)
			handle := pick(random, "f", "g")
			data := pick(random, "", "x", "hello\n", "\xff\xfe", strings.Repeat("r", 300))
			switch random.intn(30) {
			case 0:
				list = append(list, st("write", "p", p, "data", data))
			case 1:
				list = append(list, st("read", "p", p))
			case 2:
				list = append(list, st("read_to_string", "p", p))
			case 3:
				list = append(list, st("create_dir", "p", p))
			case 4:
				list = append(list, st("create_dir_all", "p", p))
			case 5:
				list = append(list, st("remove_file", "p", p))
			case 6:
				list = append(list, st("remove_dir", "p", p))
			case 7:
				list = append(list, st("remove_dir_all", "p", p))
			case 8:
				list = append(list, st("rename", "p", p, "q", q))
			case 9:
				list = append(list, st("copy", "p", p, "q", q))
			case 10:
				list = append(list, st("hard_link", "p", p, "q", q))
			case 11:
				list = append(list, st("symlink", "p", p, "q", q))
			case 12:
				list = append(list, st("metadata", "p", p))
			case 13:
				list = append(list, st("symlink_metadata", "p", p))
			case 14:
				list = append(list, st("canonicalize", "p", p))
			case 15:
				list = append(list, st("read_dir", "p", p))
			case 16:
				list = append(list, st("try_exists", "p", p))
			case 17:
				list = append(list, st("read_link", "p", p))
			case 18:
				list = append(list, st("set_permissions", "p", p, "mode", pick(random, 0o644, 0o755, 0o400, 0o100)))
			case 19:
				list = append(list, st("open", "p", p, "f", handle, "read", random.intn(2) == 0, "write", random.intn(2) == 0,
					"append", random.intn(4) == 0, "truncate", random.intn(4) == 0, "create", random.intn(2) == 0,
					"create_new", random.intn(6) == 0))
			case 20:
				list = append(list, st("file_write", "f", handle, "data", data))
			case 21:
				list = append(list, st("file_read", "f", handle, "n", pick(random, 0, 1, 7, 1000)))
			case 22:
				list = append(list, st("file_seek", "f", handle, "whence", pick(random, "start", "current", "end"),
					"offset", pick(random, 0, 1, 3, 100)))
			case 23:
				list = append(list, st("file_read_to_end", "f", handle))
			case 24:
				list = append(list, st("file_set_len", "f", handle, "len", pick(random, 0, 2, 10)))
			case 25:
				list = append(list, st("file_metadata", "f", handle))
			case 26:
				list = append(list, st("close", "f", handle))
			case 27:
				list = append(list, st("file_try_lock", "f", handle))
			case 28:
				list = append(list, st("path_is_dir", "p", p), st("path_is_file", "p", p))
			case 29:
				list = append(list, st("dir_builder", "p", p, "recursive", random.intn(2) == 0, "mode", pick(random, 0o700, 0o755)))
			}
			// A handle that was never opened is an oracle error; open one first.
			if last := list[len(list)-1]; strings.HasPrefix(textOf(last["do"]), "file_") {
				list = append(list[:len(list)-1], st("open", "p", "handle-"+handle, "f", handle, "read", true, "write", true,
					"create", true), last)
			}
		}
		yield(fsScript(pick(random, 0o022, 0o077, 0), list...))
	}
}

// generateFsLive adds what depends on the file system holding the test's
// temporary directory, and so is compared only live: whether it records
// creation times.
func generateFsLive(yield func(any)) {
	generateFs(yield)
	if os.Getenv("MJ_ORACLE") != "live" {
		return
	}
	yield(fsScript(0o022,
		st("write", "p", "f", "data", "f"), st("create_dir", "p", "d"), st("symlink", "p", "f", "q", "l"),
		st("created", "p", "f"), st("created", "p", "d"), st("created", "p", "l"), st("symlink_created", "p", "l"),
		st("created", "p", "missing"), st("file_open", "p", "f", "f", "x"), st("file_created", "f", "x"),
		st("debug_metadata_shape", "p", "f"),
	))
}

func answerRsfs(request json.RawMessage) any {
	object := decodeRequest(request)
	switch textOf(object["op"]) {
	case "fs":
		return answerFs(object)
	}
	panic("unknown op " + textOf(object["op"]))
}

func TestDiffAgainstRust(t *testing.T) {
	oracle.Check(t, "std_io", corpusFor(t, "rsfs"), generateFsLive, answerRsfs)
}
