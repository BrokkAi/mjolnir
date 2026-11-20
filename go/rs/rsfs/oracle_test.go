//go:build unix

package rsfs_test

import (
	"bytes"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"net"
	"os"
	"sort"
	"strconv"
	"strings"
	"time"

	"golang.org/x/sys/unix"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rsfs"
	"github.com/BrokkAi/mjolnir/go/rs/rsio"
	"github.com/BrokkAi/mjolnir/go/rs/rspath"
)

// The differential tests run scripts of std::fs calls on both sides (the
// `fs` operation of the oracle module std_io, go/port/oracle/src/std_io.rs)
// and compare every step's value or error: kind, raw code, Display and
// Debug. A script runs in a fresh directory, which is the working directory
// while it runs, under a umask the request sets. Paths travel as hexadecimal
// bytes, with `$ROOT` standing for the script's directory.

// --- encoding ------------------------------------------------------------

func hexOf(data []byte) string { return hex.EncodeToString(data) }

func unhex(text string) []byte {
	data, err := hex.DecodeString(text)
	if err != nil {
		panic(err)
	}
	return data
}

// decodeBytes reads bytes of a request as the oracle does: hexadecimal
// text, or {"lcg": seed, "len": n} for pseudo-random bytes, or
// {"repeat": hex, "times": n}.
func decodeBytes(value any) []byte {
	switch value := value.(type) {
	case string:
		return unhex(value)
	case map[string]any:
		if seed, ok := value["lcg"]; ok {
			length := int(intOf(value["len"]))
			var alphabet []byte
			if text, ok := value["alphabet"].(string); ok {
				alphabet = unhex(text)
			}
			state := uint64(intOf(seed))
			out := make([]byte, 0, length)
			for i := 0; i < length; i++ {
				state = state*6364136223846793005 + 1442695040888963407
				b := byte(state >> 56)
				if len(alphabet) != 0 {
					b = alphabet[int(b)%len(alphabet)]
				}
				out = append(out, b)
			}
			return out
		}
		return bytes.Repeat(unhex(textOf(value["repeat"])), int(intOf(value["times"])))
	}
	return nil
}

// show writes bytes of a reply as the oracle does.
func show(data []byte) string {
	if len(data) <= 96 {
		return hexOf(data)
	}
	hash := uint64(0xcbf29ce484222325)
	for _, b := range data {
		hash ^= uint64(b)
		hash *= 0x100000001b3
	}
	return fmt.Sprintf("len:%d:fnv:%016x", len(data), hash)
}

func intOf(value any) int64 {
	switch value := value.(type) {
	case json.Number:
		if n, err := value.Int64(); err == nil {
			return n
		}
		n, err := strconv.ParseUint(value.String(), 10, 64)
		if err != nil {
			panic(err)
		}
		return int64(n)
	case nil:
		return 0
	}
	panic(fmt.Sprintf("not a number: %#v", value))
}

func uintOf(value any) uint64 {
	if number, ok := value.(json.Number); ok {
		n, err := strconv.ParseUint(number.String(), 10, 64)
		if err != nil {
			panic(err)
		}
		return n
	}
	return uint64(intOf(value))
}

func textOf(value any) string {
	text, _ := value.(string)
	return text
}

func flagOf(object map[string]any, key string) bool {
	value, _ := object[key].(bool)
	return value
}

func decodeRequest(request json.RawMessage) map[string]any {
	decoder := json.NewDecoder(bytes.NewReader(request))
	decoder.UseNumber()
	var object map[string]any
	if err := decoder.Decode(&object); err != nil {
		panic(err)
	}
	return object
}

// errJSON reports an error the way the oracle reports an io::Error.
func errJSON(err error) map[string]any {
	e := rsio.From(err)
	var raw any
	if code, ok := e.RawOsError().Get(); ok {
		raw = code
	}
	return map[string]any{
		"kind":      e.Kind().DebugString(),
		"kind_text": e.Kind().String(),
		"raw":       raw,
		"display":   e.Error(),
		"debug":     e.DebugString(),
	}
}

func resultJSON(value any, err error) map[string]any {
	if err != nil {
		return map[string]any{"err": errJSON(err)}
	}
	return map[string]any{"ok": value}
}

func unitJSON(err error) map[string]any { return resultJSON(nil, err) }

// --- the script ------------------------------------------------------------

type script struct {
	root      string
	files     map[string]*rsfs.File
	listeners []net.Listener
}

// stepPath is a path of a step: the hexadecimal bytes of the path, in which
// `$ROOT` at the start stands for the script's directory.
func (s *script) stepPath(step map[string]any, key string) string {
	raw := string(decodeBytes(step[key]))
	if rest, ok := strings.CutPrefix(raw, "$ROOT"); ok {
		return s.root + rest
	}
	return raw
}

// replyPath is a path in a reply, with the script's directory replaced by
// `$ROOT`.
func (s *script) replyPath(path string) string {
	if rest, ok := strings.CutPrefix(path, s.root); ok {
		return hexOf([]byte("$ROOT" + rest))
	}
	return hexOf([]byte(path))
}

func (s *script) rootRelative(text string) string {
	return strings.ReplaceAll(text, s.root, "$ROOT")
}

func normalizeFd(debug string) string {
	start := strings.Index(debug, "fd: ")
	if start < 0 {
		return debug
	}
	digits := start + 4
	end := digits
	for end < len(debug) && debug[end] >= '0' && debug[end] <= '9' {
		end++
	}
	return debug[:digits] + "FD" + debug[end:]
}

func fileTypeName(fileType rsfs.FileType) string {
	switch {
	case fileType.IsFile():
		return "file"
	case fileType.IsDir():
		return "dir"
	case fileType.IsSymlink():
		return "symlink"
	case fileType.IsFifo():
		return "fifo"
	case fileType.IsSocket():
		return "socket"
	case fileType.IsBlockDevice():
		return "block"
	case fileType.IsCharDevice():
		return "char"
	}
	return "other"
}

func metadataJSON(metadata rsfs.Metadata) map[string]any {
	out := map[string]any{
		"type":       fileTypeName(metadata.FileType()),
		"is_file":    metadata.IsFile(),
		"is_dir":     metadata.IsDir(),
		"is_symlink": metadata.IsSymlink(),
		"readonly":   metadata.Permissions().Readonly(),
	}
	// The size of a directory depends on the file system.
	if !metadata.IsDir() {
		out["len"] = metadata.Len()
	}
	out["mode"] = metadata.Mode()
	out["permissions_mode"] = metadata.Permissions().Mode()
	out["own_uid"] = metadata.Uid() == uint32(os.Geteuid())
	out["own_gid"] = metadata.Gid() == uint32(os.Getegid())
	if !metadata.IsDir() {
		out["nlink"] = metadata.Nlink()
		out["size"] = metadata.Size()
	}
	matches := false
	if modified, err := metadata.Modified(); err == nil && !modified.Before(time.Unix(0, 0)) {
		matches = modified.Unix() == metadata.Mtime() && int64(modified.Nanosecond()) == metadata.MtimeNsec()
	}
	out["mtime_matches"] = matches || metadata.Mtime() < 0
	return out
}

func timeJSON(t time.Time, err error) map[string]any {
	if err != nil {
		return map[string]any{"err": errJSON(err)}
	}
	epoch := time.Unix(0, 0)
	if !t.Before(epoch) {
		return map[string]any{"ok": map[string]any{"secs": t.Unix(), "nanos": t.Nanosecond()}}
	}
	before := epoch.Sub(t)
	return map[string]any{"ok": map[string]any{
		"before_secs":  int64(before / time.Second),
		"before_nanos": int64(before % time.Second),
	}}
}

func systemTime(step map[string]any, secs, nanos string) time.Time {
	return time.Unix(intOf(step[secs]), int64(uintOf(step[nanos]))).UTC()
}

func openOptions(step map[string]any) *rsfs.OpenOptions {
	options := rsfs.OpenOptionsNew()
	options.
		Read(flagOf(step, "read")).
		Write(flagOf(step, "write")).
		Append(flagOf(step, "append")).
		Truncate(flagOf(step, "truncate")).
		Create(flagOf(step, "create")).
		CreateNew(flagOf(step, "create_new"))
	if mode, ok := step["mode"]; ok {
		options.Mode(uint32(uintOf(mode)))
	}
	if flags, ok := step["custom_flags"]; ok {
		options.CustomFlags(int32(intOf(flags)))
	}
	return options
}

func tryLockJSON(err error) map[string]any {
	if err == nil {
		return map[string]any{"ok": nil}
	}
	display := err.Error()
	debug := rs.DebugString(err)
	switch err := err.(type) {
	case *rsfs.TryLockError_WouldBlock:
		return map[string]any{"would_block": true, "display": display, "debug": debug}
	case *rsfs.TryLockError_Error:
		return map[string]any{"err": errJSON(err.F0), "display": display, "debug": debug}
	}
	panic("not a TryLockError")
}

// answerFs runs an `fs` script.
func answerFs(object map[string]any) any {
	created := fmt.Sprintf("%s/mj-oracle-std-io-go-%d-%d", os.TempDir(), os.Getpid(), nextSerial())
	if err := os.MkdirAll(created, 0o777); err != nil {
		panic(err)
	}
	root, err := rsfs.Canonicalize(created)
	if err != nil {
		panic(err)
	}
	previousDir, err := os.Getwd()
	if err != nil {
		panic(err)
	}
	if err := os.Chdir(root); err != nil {
		panic(err)
	}
	previousUmask := unix.Umask(int(uintOf(object["umask"])))

	s := &script{root: root, files: map[string]*rsfs.File{}}
	results := []any{}
	var unknown any
	steps, _ := object["steps"].([]any)
	for _, raw := range steps {
		step := raw.(map[string]any)
		result, ok := s.step(step)
		if !ok {
			unknown = map[string]any{"oracle_error": "unknown request"}
			break
		}
		results = append(results, result)
	}
	for _, file := range s.files {
		file.Drop()
	}
	for _, listener := range s.listeners {
		listener.Close()
	}

	unix.Umask(previousUmask)
	if err := os.Chdir(previousDir); err != nil {
		panic(err)
	}
	makeRemovable(root)
	_ = os.RemoveAll(root)
	if unknown != nil {
		return unknown
	}
	return map[string]any{"results": results}
}

var serial int

func nextSerial() int {
	serial++
	return serial
}

// makeRemovable gives the owner every permission on every directory under
// path, so that a script which took permissions away leaves nothing behind.
func makeRemovable(path string) {
	var stat unix.Stat_t
	if unix.Lstat(path, &stat) != nil || stat.Mode&unix.S_IFMT != unix.S_IFDIR {
		return
	}
	_ = unix.Chmod(path, 0o700)
	entries, err := os.ReadDir(path)
	if err != nil {
		return
	}
	for _, entry := range entries {
		makeRemovable(path + "/" + entry.Name())
	}
}

func (s *script) step(step map[string]any) (any, bool) {
	p := s.stepPath(step, "p")
	q := s.stepPath(step, "q")
	name := textOf(step["f"])
	switch action := textOf(step["do"]); action {
	case "write":
		return unitJSON(rsfs.Write(p, decodeBytes(step["data"]))), true
	case "read":
		data, err := rsfs.Read(p)
		return resultJSON(hexOf(data), err), true
	case "read_show":
		data, err := rsfs.Read(p)
		return resultJSON(show(data), err), true
	case "read_to_string":
		text, err := rsfs.ReadToString(p)
		return resultJSON(text, err), true
	case "copy":
		n, err := rsfs.Copy(p, q)
		return resultJSON(n, err), true
	case "rename":
		return unitJSON(rsfs.Rename(p, q)), true
	case "remove_file":
		return unitJSON(rsfs.RemoveFile(p)), true
	case "remove_dir":
		return unitJSON(rsfs.RemoveDir(p)), true
	case "remove_dir_all":
		return unitJSON(rsfs.RemoveDirAll(p)), true
	case "create_dir":
		return unitJSON(rsfs.CreateDir(p)), true
	case "create_dir_all":
		return unitJSON(rsfs.CreateDirAll(p)), true
	case "hard_link":
		return unitJSON(rsfs.HardLink(p, q)), true
	case "read_link", "path_read_link":
		target, err := rsfs.ReadLink(p)
		if action == "path_read_link" {
			target, err = rsfs.PathReadLink(p)
		}
		if err != nil {
			return resultJSON(nil, err), true
		}
		return resultJSON(s.replyPath(target), nil), true
	case "canonicalize", "path_canonicalize":
		path, err := rsfs.Canonicalize(p)
		if action == "path_canonicalize" {
			path, err = rsfs.PathCanonicalize(p)
		}
		if err != nil {
			return resultJSON(nil, err), true
		}
		return resultJSON(s.replyPath(path), nil), true
	case "metadata", "symlink_metadata", "path_metadata", "path_symlink_metadata":
		var (
			metadata rsfs.Metadata
			err      error
		)
		switch action {
		case "metadata":
			metadata, err = rsfs.MetadataOf(p)
		case "symlink_metadata":
			metadata, err = rsfs.SymlinkMetadata(p)
		case "path_metadata":
			metadata, err = rsfs.PathMetadata(p)
		default:
			metadata, err = rsfs.PathSymlinkMetadata(p)
		}
		if err != nil {
			return resultJSON(nil, err), true
		}
		return resultJSON(metadataJSON(metadata), nil), true
	case "exists":
		exists, err := rsfs.Exists(p)
		return resultJSON(exists, err), true
	case "try_exists":
		exists, err := rsfs.PathTryExists(p)
		return resultJSON(exists, err), true
	case "path_exists":
		return rsfs.PathExists(p), true
	case "path_is_file":
		return rsfs.PathIsFile(p), true
	case "path_is_dir":
		return rsfs.PathIsDir(p), true
	case "path_is_symlink":
		return rsfs.PathIsSymlink(p), true
	case "read_dir", "path_read_dir":
		listing, err := rsfs.ReadDirOf(p)
		if action == "path_read_dir" {
			listing, err = rsfs.PathReadDir(p)
		}
		if err != nil {
			return map[string]any{"err": errJSON(err)}, true
		}
		type named struct {
			name  string
			value map[string]any
		}
		var entries []named
		for result := range listing.Iter() {
			entry, err := result.Value, result.Err
			if err != nil {
				return map[string]any{"entry_err": errJSON(err)}, true
			}
			var fileType, metadataType any
			if t, err := entry.FileType(); err == nil {
				fileType = fileTypeName(t)
			}
			if m, err := entry.Metadata(); err == nil {
				metadataType = fileTypeName(m.FileType())
			}
			entries = append(entries, named{entry.FileName(), map[string]any{
				"name":           hexOf([]byte(entry.FileName())),
				"path_is_joined": rspath.Eq(entry.Path(), rspath.Join(p, entry.FileName())),
				"type":           fileType,
				"metadata_type":  metadataType,
			}})
		}
		// The order of a directory depends on the file system.
		sort.Slice(entries, func(i, j int) bool { return entries[i].name < entries[j].name })
		out := []any{}
		for _, entry := range entries {
			out = append(out, entry.value)
		}
		return map[string]any{"ok": out}, true
	case "open", "file_open", "file_create", "file_create_new":
		var (
			file *rsfs.File
			err  error
		)
		switch action {
		case "open":
			file, err = openOptions(step).Open(p)
		case "file_open":
			file, err = rsfs.FileOpen(p)
		case "file_create":
			file, err = rsfs.FileCreate(p)
		default:
			file, err = rsfs.FileCreateNew(p)
		}
		if err != nil {
			return map[string]any{"err": errJSON(err)}, true
		}
		if old, ok := s.files[name]; ok {
			old.Drop()
		}
		s.files[name] = file
		return map[string]any{"ok": nil}, true
	case "close":
		if file, ok := s.files[name]; ok {
			file.Drop()
			delete(s.files, name)
		}
		return nil, true
	case "created", "symlink_created":
		metadata, err := rsfs.MetadataOf(p)
		if action == "symlink_created" {
			metadata, err = rsfs.SymlinkMetadata(p)
		}
		if err == nil {
			_, err = metadata.Created()
		}
		if err != nil {
			return map[string]any{"err": errJSON(err)}, true
		}
		return map[string]any{"ok": true}, true
	case "debug_file_type":
		metadata, err := rsfs.SymlinkMetadata(p)
		if err != nil {
			return resultJSON(nil, err), true
		}
		return resultJSON(rs.DebugString(metadata.FileType()), nil), true
	case "debug_open_options":
		return rs.DebugString(*openOptions(step)), true
	case "debug_file_times":
		times := rsfs.FileTimesNew()
		if _, ok := step["secs"]; ok {
			times = times.SetModified(systemTime(step, "secs", "nanos"))
		}
		if _, ok := step["asecs"]; ok {
			times = times.SetAccessed(systemTime(step, "asecs", "ananos"))
		}
		return rs.DebugString(times), true
	case "debug_read_dir":
		listing, err := rsfs.ReadDirOf(p)
		if err != nil {
			return map[string]any{"err": errJSON(err)}, true
		}
		listingDebug := rs.DebugString(listing)
		entries := []string{}
		for result := range listing.Iter() {
			if result.Err == nil {
				entries = append(entries, s.rootRelative(rs.DebugString(result.Value)))
			}
		}
		sort.Strings(entries)
		return map[string]any{"ok": s.rootRelative(listingDebug), "entries": entries}, true
	case "debug_metadata_shape":
		metadata, err := rsfs.SymlinkMetadata(p)
		if err != nil {
			return map[string]any{"err": errJSON(err)}, true
		}
		debug := rs.DebugString(metadata)
		cut := strings.Index(debug, ", modified")
		if cut < 0 {
			cut = len(debug)
		}
		names := []any{}
		for _, field := range []string{"modified", "accessed", "created"} {
			if strings.Contains(debug, field+": SystemTime") {
				names = append(names, field)
			}
		}
		return map[string]any{"ok": debug[:cut], "times": names, "ends": strings.HasSuffix(debug, ", .. }")}, true
	case "symlink":
		return unitJSON(rsfs.Symlink(p, q)), true
	case "set_permissions":
		return unitJSON(rsfs.SetPermissions(p, rsfs.PermissionsFromMode(uint32(uintOf(step["mode"]))))), true
	case "set_readonly":
		metadata, err := rsfs.MetadataOf(p)
		if err != nil {
			return map[string]any{"err": errJSON(err)}, true
		}
		permissions := metadata.Permissions()
		permissions.SetReadonly(flagOf(step, "readonly"))
		mode := permissions.Mode()
		if err := rsfs.SetPermissions(p, permissions); err != nil {
			return map[string]any{"err": errJSON(err)}, true
		}
		return map[string]any{"ok": mode}, true
	case "dir_builder":
		builder := rsfs.DirBuilderNew()
		builder.Recursive(flagOf(step, "recursive"))
		if mode, ok := step["mode"]; ok {
			builder.Mode(uint32(uintOf(mode)))
		}
		return unitJSON(builder.Create(p)), true
	case "chown_self":
		return unitJSON(rsfs.Chown(p, rs.Some(uint32(os.Geteuid())), rs.Some(uint32(os.Getegid())))), true
	case "chown_none":
		return unitJSON(rsfs.Chown(p, rs.None[uint32](), rs.None[uint32]())), true
	case "lchown_self":
		return unitJSON(rsfs.Lchown(p, rs.Some(uint32(os.Geteuid())), rs.Some(uint32(os.Getegid())))), true
	case "chown_root":
		return unitJSON(rsfs.Chown(p, rs.Some(uint32(0)), rs.Some(uint32(0)))), true
	case "debug_permissions":
		permissions := rsfs.PermissionsFromMode(uint32(uintOf(step["mode"])))
		return map[string]any{"debug": rs.DebugString(permissions), "readonly": permissions.Readonly()}, true
	case "debug_dir_builder":
		builder := rsfs.DirBuilderNew()
		builder.Recursive(flagOf(step, "recursive"))
		if mode, ok := step["mode"]; ok {
			builder.Mode(uint32(uintOf(mode)))
		}
		return rs.DebugString(builder), true
	case "try_lock_error_debug":
		var wouldBlock rsfs.TryLockError = &rsfs.TryLockError_WouldBlock{}
		var failed rsfs.TryLockError = &rsfs.TryLockError_Error{F0: rsio.ErrorFromRawOsError(int32(uintOf(step["code"])))}
		return map[string]any{
			"would_block":    []any{wouldBlock.Error(), rs.DebugString(wouldBlock)},
			"error":          []any{failed.Error(), rs.DebugString(failed)},
			"would_block_io": errJSON(rsfs.ErrorFromTryLockError(wouldBlock)),
		}, true
	case "mkfifo":
		return unix.Mkfifo(p, uint32(uintOf(step["mode"]))) == nil, true
	case "socket":
		listener, err := net.Listen("unix", p)
		if err != nil {
			return false, true
		}
		s.listeners = append(s.listeners, listener)
		return true, true
	}
	if strings.HasPrefix(textOf(step["do"]), "file_") {
		file, ok := s.files[name]
		if !ok {
			return map[string]any{"oracle_error": "no open file " + name}, true
		}
		return s.fileStep(file, textOf(step["do"]), step)
	}
	return nil, false
}

func (s *script) fileStep(file *rsfs.File, action string, step map[string]any) (any, bool) {
	switch action {
	case "file_write":
		n, err := file.Write(decodeBytes(step["data"]))
		return resultJSON(n, err), true
	case "file_write_all":
		return unitJSON(rsio.WriteAll(file, decodeBytes(step["data"]))), true
	case "file_flush":
		return unitJSON(file.Flush()), true
	case "file_read":
		buf := make([]byte, uintOf(step["n"]))
		n, err := file.Read(buf)
		if err != nil {
			return map[string]any{"err": errJSON(err)}, true
		}
		return map[string]any{"ok": hexOf(buf[:n])}, true
	case "file_read_exact":
		buf := make([]byte, uintOf(step["n"]))
		if err := rsio.ReadExact(file, buf); err != nil {
			return map[string]any{"err": errJSON(err)}, true
		}
		return map[string]any{"ok": hexOf(buf)}, true
	case "file_read_to_end":
		buf := decodeBytes(step["prefix"])
		n, err := rsio.ReadToEnd(file, &buf)
		if err != nil {
			return map[string]any{"err": errJSON(err), "data": hexOf(buf)}, true
		}
		return map[string]any{"ok": n, "data": hexOf(buf)}, true
	case "file_read_to_string":
		text := textOf(step["prefix"])
		n, err := rsio.ReadToString(file, &text)
		if err != nil {
			return map[string]any{"err": errJSON(err), "text": text}, true
		}
		return map[string]any{"ok": n, "text": text}, true
	case "file_seek":
		var pos rsio.SeekFrom
		switch textOf(step["whence"]) {
		case "start":
			pos = &rsio.SeekFrom_Start{F0: uintOf(step["offset"])}
		case "end":
			pos = &rsio.SeekFrom_End{F0: intOf(step["offset"])}
		case "current":
			pos = &rsio.SeekFrom_Current{F0: intOf(step["offset"])}
		default:
			return nil, false
		}
		n, err := rsio.Seek(file, pos)
		return resultJSON(n, err), true
	case "file_stream_position":
		n, err := rsio.StreamPosition(file)
		return resultJSON(n, err), true
	case "file_rewind":
		return unitJSON(rsio.Rewind(file)), true
	case "file_set_len":
		return unitJSON(file.SetLen(uintOf(step["len"]))), true
	case "file_metadata":
		metadata, err := file.Metadata()
		if err != nil {
			return resultJSON(nil, err), true
		}
		return resultJSON(metadataJSON(metadata), nil), true
	case "file_sync_all":
		return unitJSON(file.SyncAll()), true
	case "file_sync_data":
		return unitJSON(file.SyncData()), true
	case "file_try_clone":
		clone, err := file.TryClone()
		if err != nil {
			return map[string]any{"err": errJSON(err)}, true
		}
		as := textOf(step["as"])
		if old, ok := s.files[as]; ok {
			old.Drop()
		}
		s.files[as] = clone
		return map[string]any{"ok": nil}, true
	case "file_lock":
		return unitJSON(file.Lock()), true
	case "file_lock_shared":
		return unitJSON(file.LockShared()), true
	case "file_try_lock":
		return tryLockJSON(file.TryLock()), true
	case "file_try_lock_shared":
		return tryLockJSON(file.TryLockShared()), true
	case "file_unlock":
		return unitJSON(file.Unlock()), true
	case "file_set_modified":
		return unitJSON(file.SetModified(systemTime(step, "secs", "nanos"))), true
	case "file_set_times":
		times := rsfs.FileTimesNew()
		if _, ok := step["secs"]; ok {
			times = times.SetModified(systemTime(step, "secs", "nanos"))
		}
		if _, ok := step["asecs"]; ok {
			times = times.SetAccessed(systemTime(step, "asecs", "ananos"))
		}
		return unitJSON(file.SetTimes(times)), true
	case "file_modified", "file_accessed":
		metadata, err := file.Metadata()
		if err != nil {
			return timeJSON(time.Time{}, err), true
		}
		if action == "file_modified" {
			return timeJSON(metadata.Modified()), true
		}
		return timeJSON(metadata.Accessed()), true
	case "file_is_terminal":
		return file.IsTerminal(), true
	case "file_created":
		metadata, err := file.Metadata()
		if err == nil {
			_, err = metadata.Created()
		}
		if err != nil {
			return map[string]any{"err": errJSON(err)}, true
		}
		return map[string]any{"ok": true}, true
	case "file_debug":
		return normalizeFd(s.rootRelative(rs.DebugString(file))), true
	case "file_stream_len_matches":
		before, beforeErr := rsio.StreamPosition(file)
		end, endErr := rsio.Seek(file, &rsio.SeekFrom_End{F0: 0})
		out := map[string]any{"before": nil, "end": nil, "restored": nil}
		if beforeErr == nil {
			out["before"] = before
			restored, err := rsio.Seek(file, &rsio.SeekFrom_Start{F0: before})
			if err == nil {
				out["restored"] = restored
			}
		}
		if endErr == nil {
			out["end"] = end
		}
		return out, true
	case "file_set_permissions":
		return unitJSON(file.SetPermissions(rsfs.PermissionsFromMode(uint32(uintOf(step["mode"]))))), true
	case "file_read_at":
		buf := make([]byte, uintOf(step["n"]))
		n, err := file.ReadAt(buf, uintOf(step["offset"]))
		if err != nil {
			return map[string]any{"err": errJSON(err)}, true
		}
		return map[string]any{"ok": hexOf(buf[:n])}, true
	case "file_read_exact_at":
		buf := make([]byte, uintOf(step["n"]))
		if err := file.ReadExactAt(buf, uintOf(step["offset"])); err != nil {
			return map[string]any{"err": errJSON(err)}, true
		}
		return map[string]any{"ok": hexOf(buf)}, true
	case "file_write_at":
		n, err := file.WriteAt(decodeBytes(step["data"]), uintOf(step["offset"]))
		return resultJSON(n, err), true
	case "file_write_all_at":
		return unitJSON(file.WriteAllAt(decodeBytes(step["data"]), uintOf(step["offset"]))), true
	case "file_fchown_self":
		return unitJSON(rsfs.Fchown(file, rs.Some(uint32(os.Geteuid())), rs.Some(uint32(os.Getegid())))), true
	case "file_cloexec":
		flags, err := unix.FcntlInt(uintptr(file.AsRawFd()), unix.F_GETFD, 0)
		return err == nil && flags&unix.FD_CLOEXEC == unix.FD_CLOEXEC, true
	}
	return nil, false
}
