// Instance names, configuration directories and atomic file writes.
// Port of mj-core/src/config/loading.rs.

package core

import (
	"crypto/rand"
	"crypto/sha256"
	"encoding/binary"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"sync/atomic"
)

// ConfigVersion is the config.toml schema version this build reads and writes.
const ConfigVersion = 23

// ProductDir is the directory name under the platform configuration and data roots.
const ProductDir = "mjolnir"

// EnvOverride reads a configuration override under its MJ_ name. Mjolnir
// shares no state or environment with hel installs; there is no legacy fallback.
func EnvOverride(name string) (string, bool) {
	return os.LookupEnv("MJ_" + name)
}

// InstanceEnv selects an isolated Mjolnir instance. An instance keeps its own
// configuration, database, daemon, and logs, so MJ_INSTANCE=dev never shares
// state with the default setup.
const InstanceEnv = "MJ_INSTANCE"

// instanceDir holds one isolated instance's files under the default
// configuration and data roots (<root>/mjolnir/instances/<name>).
const instanceDir = "instances"

// InstanceName is the trimmed instance name from InstanceEnv. Empty means no instance.
func InstanceName() (string, bool) {
	name, ok := EnvOverride("INSTANCE")
	if !ok {
		return "", false
	}
	trimmed := strings.TrimSpace(name)
	return trimmed, trimmed != ""
}

// IsValidInstanceName reports whether name is safe to use as a single path
// segment under the instances directory.
func IsValidInstanceName(name string) bool {
	return ValidateID("instance", name) == nil
}

// ValidateInstanceEnv fails when InstanceEnv names something that cannot be an
// instance. Every shipped binary calls this during startup so a typo fails
// closed instead of silently using the default directories.
func ValidateInstanceEnv() error {
	if name, ok := InstanceName(); ok {
		return ValidateID("instance", name)
	}
	return nil
}

// SessionIndexEnv names the directory holding SessionWiki's index.
// SessionWiki reads it itself; Mjolnir only decides what it should say.
const SessionIndexEnv = "SESSIONWIKI_DATA"

var sessionIndexResolved atomic.Bool

// ApplyInstanceFlag records a --instance flag value for this process and the
// daemon and workers it spawns. The explicit flag wins over InstanceEnv.
// Callers run it during startup, before other goroutines read the environment.
func ApplyInstanceFlag(value *string) error {
	if value != nil {
		name := strings.TrimSpace(*value)
		if err := ValidateID("instance", name); err != nil {
			return err
		}
		os.Setenv(InstanceEnv, name)
	}
	if err := ValidateInstanceEnv(); err != nil {
		return err
	}
	// The data directory is settled by now, which is what the session index
	// location follows.
	existing, hasExisting := os.LookupEnv(SessionIndexEnv)
	override, hasOverride := EnvOverride("DATA_DIR")
	instance, _ := InstanceName()
	if directory, ok := sessionIndexDirFor(optional(existing, hasExisting), optional(override, hasOverride),
		withInstanceDir(defaultDataDir(), instance)); ok {
		os.Setenv(SessionIndexEnv, directory)
	}
	sessionIndexResolved.Store(true)
	return nil
}

func optional(value string, ok bool) *string {
	if !ok {
		return nil
	}
	return &value
}

// sessionIndexDirFor says where this process's session index belongs, or
// false to leave the choice alone.
//
// Session indexing is always on, so a process running against an overridden
// data directory, such as every end-to-end test daemon, must not walk the
// user's real session stores or write the user's real index. Such a process
// gets its own index beside its own data. A named instance without a
// data-directory override keeps sharing the user's one index, and so does an
// override that names the directory this process would use anyway. An
// explicit SESSIONWIKI_DATA is always obeyed.
func sessionIndexDirFor(existing, dataDirOverride *string, defaultDataDir string) (string, bool) {
	if existing != nil || dataDirOverride == nil {
		return "", false
	}
	if samePath(*dataDirOverride, defaultDataDir) {
		return "", false
	}
	return filepath.Join(*dataDirOverride, "sessionwiki"), true
}

// samePath compares paths by component, as Rust's PathBuf equality does: a
// trailing separator or repeated separators do not make two paths differ.
func samePath(a, b string) bool {
	return filepath.Clean(a) == filepath.Clean(b) && filepath.IsAbs(a) == filepath.IsAbs(b)
}

// SessionIndexIsResolved reports whether process startup has chosen where the
// session index lives. Only ApplyInstanceFlag sets this, and only a real
// Mjolnir binary runs it.
func SessionIndexIsResolved() bool { return sessionIndexResolved.Load() }

// withInstanceDir nests base under the instances directory when an instance is
// selected. A name that fails validation falls back to base; startup
// validation rejects it first, so this only guards future callers that skip
// that check.
func withInstanceDir(base, instance string) string {
	if instance != "" && IsValidInstanceName(instance) {
		return filepath.Join(base, instanceDir, instance)
	}
	return base
}

// ConfigDir is the directory holding config.toml.
func ConfigDir() string {
	if path, ok := EnvOverride("CONFIG_DIR"); ok {
		return path
	}
	root, err := os.UserConfigDir()
	if err != nil {
		root = ".config"
	}
	instance, _ := InstanceName()
	return withInstanceDir(filepath.Join(root, ProductDir), instance)
}

// ConfigPath is the path of config.toml.
func ConfigPath() string { return filepath.Join(ConfigDir(), "config.toml") }

// DataDir is the directory holding the database, sessions and logs.
func DataDir() string {
	if path, ok := EnvOverride("DATA_DIR"); ok {
		return path
	}
	instance, _ := InstanceName()
	return withInstanceDir(defaultDataDir(), instance)
}

// defaultDataDir is the default instance's data directory: no instance, no override.
func defaultDataDir() string {
	root, ok := userDataLocalDir()
	if !ok {
		root = filepath.Join(".local", "share")
	}
	return filepath.Join(root, ProductDir)
}

// userDataLocalDir is the platform's per-user local data directory: the XDG
// data home on Unix, Application Support on macOS and LocalAppData on Windows.
func userDataLocalDir() (string, bool) {
	switch runtime.GOOS {
	case "windows":
		if dir := os.Getenv("LocalAppData"); dir != "" {
			return dir, true
		}
		return "", false
	case "darwin", "ios":
		home, err := os.UserHomeDir()
		if err != nil {
			return "", false
		}
		return filepath.Join(home, "Library", "Application Support"), true
	default:
		if dir := os.Getenv("XDG_DATA_HOME"); filepath.IsAbs(dir) {
			return dir, true
		}
		home, err := os.UserHomeDir()
		if err != nil {
			return "", false
		}
		return filepath.Join(home, ".local", "share"), true
	}
}

// EnsureMayControlStore refuses to let a development build own the default
// instance's store.
//
// This is the one decision behind every step that would take the default
// instance over: starting its daemon, replacing or stopping a running one,
// and opening its database as the writer, which migrates it. A development
// build may still read the default instance through its running daemon.
// Development builds are the ones run from a Go build or test directory; an
// installed mj lives elsewhere. action names the refused step.
func EnsureMayControlStore(store, action string) error {
	executable, err := os.Executable()
	if err != nil {
		return nil
	}
	if buildDir, ok := developmentBuildControllingDefaultStore(executable, store, defaultDataDir()); ok {
		return fmt.Errorf("refusing to %s: this is a development build from %s and %s is the "+
			"default Mjolnir instance, which holds live sessions. Development builds may "+
			"read the default instance but never start, replace, stop, or migrate it. "+
			"Test this build in an isolated instance with `--instance <name>`.",
			action, buildDir, store)
	}
	return nil
}

// developmentBuildControllingDefaultStore is the pure form of
// EnsureMayControlStore: the development build directory holding executable,
// when store is the default instance's data directory.
func developmentBuildControllingDefaultStore(executable, store, defaultStore string) (string, bool) {
	same := false
	canonicalStore, errStore := filepath.EvalSymlinks(store)
	canonicalDefault, errDefault := filepath.EvalSymlinks(defaultStore)
	if errStore == nil && errDefault == nil {
		same = canonicalStore == canonicalDefault
	} else {
		same = samePath(store, defaultStore)
	}
	if !same {
		return "", false
	}
	return developmentBuildDir(executable)
}

// developmentBuildMarker marks a directory that development builds of the Go
// port are written to (go/bin, created by the build script), playing the role
// Cargo's .fingerprint directory plays for the Rust build.
const developmentBuildMarker = ".mj-development-build"

// developmentBuildDir is the development build directory holding executable,
// directly or one level down. `go test` and `go run` executables live in a
// temporary go-build directory, which also counts. A path that no longer
// names a file, such as a Linux /proc/self/exe target ending in " (deleted)"
// after a rebuild, is judged by its directory.
func developmentBuildDir(executable string) (string, bool) {
	directory := filepath.Dir(executable)
	for range 2 {
		if info, err := os.Stat(filepath.Join(directory, developmentBuildMarker)); err == nil && info.IsDir() {
			return directory, true
		}
		if info, err := os.Stat(filepath.Join(directory, ".fingerprint")); err == nil && info.IsDir() {
			return directory, true
		}
		if strings.HasPrefix(filepath.Base(directory), "go-build") {
			return directory, true
		}
		directory = filepath.Dir(directory)
	}
	return "", false
}

// InstanceIdentity is stamped on every worker this Mjolnir instance creates,
// so a recovery scan from another instance can tell the worker is not its
// own. A named --instance uses its name; otherwise the data directory path is
// fingerprinted so an explicit MJ_DATA_DIR override also gets its own identity.
func InstanceIdentity() string {
	instance, _ := InstanceName()
	return InstanceIdentityFor(instance, DataDir())
}

// InstanceIdentityFor is the pure form of InstanceIdentity. An empty instance
// means none.
func InstanceIdentityFor(instance, dataDir string) string {
	if instance != "" && IsValidInstanceName(instance) {
		return instance
	}
	digest := sha256.Sum256([]byte(dataDir))
	return LowerHex(digest[:])[:16]
}

// SessionsDir is where per-session files live.
func SessionsDir() string { return filepath.Join(DataDir(), "sessions") }

// UniqueConfigID is the first available configuration identifier, retaining
// a readable base.
func UniqueConfigID[T any](entries map[string]T, base string) string {
	if _, taken := entries[base]; !taken {
		return base
	}
	for number := 2; ; number++ {
		suffix := fmt.Sprintf("-%d", number)
		// Configuration identifiers are ASCII and limited to 64 bytes.
		prefix := []rune(base)
		if len(prefix) > 64-len(suffix) {
			prefix = prefix[:64-len(suffix)]
		}
		candidate := string(prefix) + suffix
		if _, taken := entries[candidate]; !taken {
			return candidate
		}
	}
}

// ValidateID checks a configuration identifier of the given kind.
func ValidateID(kind, id string) error {
	valid := id != "" && len(id) <= 64 && id != "." && id != ".."
	for i := 0; valid && i < len(id); i++ {
		b := id[i]
		valid = b >= 'a' && b <= 'z' || b >= 'A' && b <= 'Z' || b >= '0' && b <= '9' ||
			b == '-' || b == '_' || b == '.'
	}
	if !valid {
		return fmt.Errorf("invalid %s id %q; use 1-64 ASCII letters, digits, '.', '-' or '_'", kind, id)
	}
	return nil
}

// ValidateRelativeDestination checks a destination inside a session workspace.
func ValidateRelativeDestination(path string) error {
	if path == "" || filepath.IsAbs(path) {
		return errors.New("destination must be a non-empty relative path")
	}
	if filepath.VolumeName(path) != "" || isSeparator(rune(path[0])) {
		return errors.New("destination must not be absolute")
	}
	for i, component := range strings.FieldsFunc(path, isSeparator) {
		switch component {
		case ".":
			// Like Rust's Path::components, only a leading "." is a component;
			// interior ones are dropped.
			if i == 0 {
				return errors.New("destination must not contain '.'")
			}
		case "..":
			return errors.New("destination must not contain '..'")
		}
	}
	return nil
}

func isSeparator(r rune) bool { return os.IsPathSeparator(uint8(r)) && r < 0x80 }

// AtomicWrite replaces path without exposing a partially written
// configuration or state file.
func AtomicWrite(path string, body []byte) error {
	return atomicWriteWithParent(path, body, true)
}

// AtomicWriteExisting replaces path only while its directory still exists.
//
// Worker state lives inside a directory that session teardown deletes out
// from under the running daemon. Recreating it here would resurrect a closed
// session's relay state, so a vanished parent must be an error instead.
func AtomicWriteExisting(path string, body []byte) error {
	return atomicWriteWithParent(path, body, false)
}

// SyncDirectory makes a rename or new entry in path durable. Directory fsync
// is only available on Unix; Windows cannot open a directory handle this way.
func SyncDirectory(path string) error {
	if runtime.GOOS == "windows" {
		return nil
	}
	directory, err := os.Open(path)
	if err == nil {
		err = directory.Sync()
		directory.Close()
	}
	if err != nil {
		return fmt.Errorf("sync directory %s: %w", path, err)
	}
	return nil
}

// CreateOwnerOnlyDirectory creates directory and any missing parents reachable
// only by their owner. Session transcripts, credentials, and logs live in
// these directories, so on Unix they must never be group- or world-traversable.
func CreateOwnerOnlyDirectory(directory string) error {
	if err := os.MkdirAll(directory, 0o700); err != nil {
		return fmt.Errorf("create %s: %w", directory, err)
	}
	return nil
}

func atomicWriteWithParent(path string, body []byte, createParent bool) error {
	parent := filepath.Dir(path)
	if createParent {
		if err := os.MkdirAll(parent, 0o777); err != nil {
			return fmt.Errorf("create %s: %w", parent, err)
		}
	} else if info, err := os.Stat(parent); err != nil || !info.IsDir() {
		return fmt.Errorf("directory %s is missing", parent)
	}

	var random [8]byte
	if _, err := rand.Read(random[:]); err != nil {
		return fmt.Errorf("generate temporary filename: %w", err)
	}
	name := filepath.Base(path)
	if name == "." || name == string(filepath.Separator) {
		name = "hel"
	}
	temporary := filepath.Join(parent, fmt.Sprintf(".%s.%d.%016x.tmp", name, os.Getpid(), binary.LittleEndian.Uint64(random[:])))

	err := func() error {
		file, err := os.OpenFile(temporary, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
		if err != nil {
			return fmt.Errorf("create %s: %w", temporary, err)
		}
		if runtime.GOOS != "windows" {
			if err := file.Chmod(0o600); err != nil {
				file.Close()
				return err
			}
		}
		if _, err := file.Write(body); err != nil {
			file.Close()
			return fmt.Errorf("write %s: %w", temporary, err)
		}
		if err := file.Sync(); err != nil {
			file.Close()
			return fmt.Errorf("sync %s: %w", temporary, err)
		}
		if err := file.Close(); err != nil {
			return fmt.Errorf("write %s: %w", temporary, err)
		}
		if err := os.Rename(temporary, path); err != nil {
			return fmt.Errorf("replace %s with %s: %w", path, temporary, err)
		}
		return SyncDirectory(parent)
	}()
	if err != nil {
		os.Remove(temporary)
	}
	return err
}
