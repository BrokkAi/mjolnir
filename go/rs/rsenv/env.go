// Port of std 1.99.1 src/env.rs

package rsenv

import (
	"iter"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rspath"
)

// Inspection and manipulation of the process's environment.
//
// This module contains functions to inspect various aspects such as
// environment variables, process arguments, the current directory, and various
// other important directories.
//
// There are several functions and structs in this module that have a
// counterpart ending in `os`. Those ending in `os` will return an OsString and
// those without will return a String.

// CurrentDir is std::env::current_dir: the current working directory as a
// PathBuf.
//
// # Platform-specific behavior
//
// This function currently corresponds to the `getcwd` function on Unix and
// the `GetCurrentDirectoryW` function on Windows.
//
// # Errors
//
// Returns an error (an *rsio.Error) if the current working directory value is
// invalid. Possible cases:
//
//   - Current directory does not exist.
//   - There are insufficient permissions to access the current directory.
func CurrentDir() (string, error) {
	return sysGetcwd()
}

// SetCurrentDir is std::env::set_current_dir: changes the current working
// directory to the specified path.
//
// # Platform-specific behavior
//
// This function currently corresponds to the `chdir` function on Unix and the
// `SetCurrentDirectoryW` function on Windows.
//
// Returns an error (an *rsio.Error) if the operation fails.
func SetCurrentDir(path string) error {
	return sysChdir(path)
}

// EnvVars is std::env::Vars: an iterator over a snapshot of the environment
// variables of this process, taken when Vars was called. (The Go name
// differs because Vars is the function, as std::env::vars is; the same holds
// for the other iterators here.)
//
// Next panics, as Rust's does, on the first variable whose name or value is
// not valid Unicode. Use VarsOs where that can happen.
type EnvVars struct {
	inner EnvVarsOs
}

// EnvVarsOs is std::env::VarsOs: an iterator over a snapshot of the environment
// variables of this process, taken when VarsOs was called.
type EnvVarsOs struct {
	inner *sysEnv
}

// Vars is std::env::vars: an iterator of (variable, value) pairs of strings,
// for all the environment variables of the current process.
//
// The returned iterator contains a snapshot of the process's environment
// variables at the time of this invocation. Modifications to environment
// variables afterwards will not be reflected in the returned iterator.
//
// # Panics
//
// While iterating, the returned iterator will panic if any key or value in
// the environment is not valid unicode. If this is not desired, consider
// using VarsOs.
//
// Unlike Var, this function does not panic on keys that contain an equals
// sign or a NUL.
func Vars() *EnvVars {
	return &EnvVars{inner: *VarsOs()}
}

// VarsOs is std::env::vars_os: an iterator of (variable, value) pairs of OS
// strings, for all the environment variables of the current process.
//
// The returned iterator contains a snapshot of the process's environment
// variables at the time of this invocation. Modifications to environment
// variables afterwards will not be reflected in the returned iterator.
//
// Note that the returned iterator will not check if the environment variables
// are valid Unicode.
func VarsOs() *EnvVarsOs {
	return &EnvVarsOs{inner: sysEnvSnapshot()}
}

// Next is Iterator::next for Vars.
func (v *EnvVars) Next() rs.Option[rs.Tuple2[string, string]] {
	pair, ok := v.inner.Next().Get()
	if !ok {
		return rs.None[rs.Tuple2[string, string]]()
	}
	name := rs.Unwrap(rspath.OsStringIntoString(pair.F0))
	value := rs.Unwrap(rspath.OsStringIntoString(pair.F1))
	return rs.Some(rs.T2(name, value))
}

// All ranges over the pairs, consuming the iterator as a Rust `for` loop
// does:
//
//	for name, value := range rsenv.Vars().All() { ... }
func (v *EnvVars) All() iter.Seq2[string, string] {
	return func(yield func(string, string) bool) {
		for {
			pair, ok := v.Next().Get()
			if !ok || !yield(pair.F0, pair.F1) {
				return
			}
		}
	}
}

// Collect is `vars.collect::<Vec<_>>()`.
func (v *EnvVars) Collect() []rs.Tuple2[string, string] {
	var pairs []rs.Tuple2[string, string]
	for {
		pair, ok := v.Next().Get()
		if !ok {
			return pairs
		}
		pairs = append(pairs, pair)
	}
}

// FmtDebug is `impl Debug for Vars`: `Vars { inner: [("K", "V"), ..] }`,
// listing the pairs still to come.
func (v *EnvVars) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("Vars").Field("inner", v.inner.inner).Finish()
}

// Next is Iterator::next for VarsOs.
func (v *EnvVarsOs) Next() rs.Option[rs.Tuple2[string, string]] {
	return v.inner.next()
}

// All ranges over the pairs, consuming the iterator as a Rust `for` loop
// does:
//
//	for name, value := range rsenv.VarsOs().All() { ... }
func (v *EnvVarsOs) All() iter.Seq2[string, string] {
	return func(yield func(string, string) bool) {
		for {
			pair, ok := v.Next().Get()
			if !ok || !yield(pair.F0, pair.F1) {
				return
			}
		}
	}
}

// Collect is `vars_os.collect::<Vec<_>>()`.
func (v *EnvVarsOs) Collect() []rs.Tuple2[string, string] {
	var pairs []rs.Tuple2[string, string]
	for {
		pair, ok := v.Next().Get()
		if !ok {
			return pairs
		}
		pairs = append(pairs, pair)
	}
}

// FmtDebug is `impl Debug for VarsOs`: `VarsOs { inner: [("K", "V"), ..] }`,
// listing the pairs still to come.
func (v *EnvVarsOs) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("VarsOs").Field("inner", v.inner).Finish()
}

// Var is std::env::var: fetches the environment variable key from the
// current process.
//
// # Errors
//
// This function will return an error if the environment variable isn't set:
// a *VarError_NotPresent.
//
// This function may return an error if the environment variable's name
// contains the equal sign character (`=`) or the NUL character.
//
// This function will return an error if the environment variable's value is
// not valid Unicode: a *VarError_NotUnicode holding the value. If this is not
// desired, consider using VarOs.
func Var(key string) (string, error) {
	value, ok := sysGetenv(key).Get()
	if !ok {
		return "", &VarError_NotPresent{}
	}
	text, err := rspath.OsStringIntoString(value)
	if err != nil {
		return "", &VarError_NotUnicode{F0: value}
	}
	return text, nil
}

// VarOs is std::env::var_os: fetches the environment variable key from the
// current process, returning None if the variable isn't set or if there is
// another error.
//
// It may return None if the environment variable's name contains the equal
// sign character (`=`) or the NUL character.
//
// Note that this function will not check if the environment variable is
// valid Unicode. If you want to have an error on invalid UTF-8, use Var.
func VarOs(key string) rs.Option[string] {
	return sysGetenv(key)
}

// VarError is std::env::VarError: the error type for operations interacting
// with environment variables. Possibly returned from Var.
type VarError interface {
	isVarError()
	error
}

// VarError_NotPresent is VarError::NotPresent: the specified environment
// variable was not present in the current process's environment.
type VarError_NotPresent struct{}

// VarError_NotUnicode is VarError::NotUnicode: the specified environment
// variable was found, but it did not contain valid unicode data. The found
// data is returned as a payload of this variant.
type VarError_NotUnicode struct{ F0 string }

func (*VarError_NotPresent) isVarError() {}
func (*VarError_NotUnicode) isVarError() {}

var _ = rs.RegisterEnum[VarError]("VarError",
	rs.Variant[VarError_NotPresent]("NotPresent", rs.ShapeUnit),
	rs.Variant[VarError_NotUnicode]("NotUnicode", rs.ShapeTuple),
)

// String holds the Display text: `environment variable not found`.
func (e *VarError_NotPresent) String() string {
	return "environment variable not found"
}

// String holds the Display text: `environment variable was not valid unicode:
// "..."`, with the value written as `{:?}` writes an OsString.
func (e *VarError_NotUnicode) String() string {
	return "environment variable was not valid unicode: " + rspath.DebugString(e.F0)
}

// Error makes the variant a Go error with its Display text.
func (e *VarError_NotPresent) Error() string { return e.String() }

// Error makes the variant a Go error with its Display text.
func (e *VarError_NotUnicode) Error() string { return e.String() }

// FmtDisplay is `impl Display for VarError`. Rust writes the text with
// write!, which does not apply width or precision.
func (e *VarError_NotPresent) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(e.String()) }

// FmtDisplay is `impl Display for VarError`. Rust writes the text with
// write!, which does not apply width or precision.
func (e *VarError_NotUnicode) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(e.String()) }

// FmtDebug is the derived Debug of VarError::NotUnicode, with the OsString
// written by its own Debug.
func (e *VarError_NotUnicode) FmtDebug(f *rs.Formatter) error {
	return f.DebugTuple("NotUnicode").Field(rspath.Debug(e.F0)).Finish()
}

// SetVar is std::env::set_var: sets the environment variable key to the value
// value for the currently running process.
//
// # Safety
//
// This function is safe to call in a single-threaded program. In Rust it is
// `unsafe` because other threads may read the environment through the C
// library at the same time; a Go program without C has only the Go runtime's
// copy, which is locked.
//
// # Panics
//
// This function may panic if key is empty, contains an ASCII equals sign `=`
// or the NUL character `'\0'`, or when value contains the NUL character, with
// Rust's message: "failed to set environment variable `{key:?}` to
// `{value:?}`: {error}".
func SetVar(key, value string) {
	if err := sysSetenv(key, value); err != nil {
		rs.PanicAt(1, "failed to set environment variable `"+rspath.DebugString(key)+"` to `"+
			rspath.DebugString(value)+"`: "+err.Error())
	}
}

// RemoveVar is std::env::remove_var: removes an environment variable from the
// environment of the currently running process.
//
// # Panics
//
// This function may panic if key is empty, contains an ASCII equals sign `=`
// or the NUL character `'\0'`, with Rust's message: "failed to remove
// environment variable `{key:?}`: {error}".
func RemoveVar(key string) {
	if err := sysUnsetenv(key); err != nil {
		rs.PanicAt(1, "failed to remove environment variable `"+rspath.DebugString(key)+"`: "+err.Error())
	}
}

// EnvSplitPaths is std::env::SplitPaths: an iterator that splits an environment
// variable into paths according to platform-specific conventions.
type EnvSplitPaths struct {
	inner sysSplitPaths
}

// SplitPaths is std::env::split_paths: parses input according to
// platform conventions for the `PATH` environment variable.
//
// Returns an iterator over the paths contained in unparsed. The iterator
// element type is PathBuf.
//
// On most Unix platforms, the separator is `:` and there is no syntax for
// escaping a separator. On Windows, the separator is `;`, and quoted regions
// (`"..."`) may contain it.
//
// Empty parts are kept: "" yields one empty path, and on Unix "a::b" yields
// "a", "" and "b".
func SplitPaths(unparsed string) *EnvSplitPaths {
	return &EnvSplitPaths{inner: sysSplitPathsNew(rspath.Host, unparsed)}
}

// Next is Iterator::next for SplitPaths.
func (s *EnvSplitPaths) Next() rs.Option[string] {
	return s.inner.next()
}

// All ranges over the paths, consuming the iterator as a Rust `for` loop
// does.
func (s *EnvSplitPaths) All() iter.Seq[string] {
	return func(yield func(string) bool) {
		for {
			path, ok := s.Next().Get()
			if !ok || !yield(path) {
				return
			}
		}
	}
}

// Collect is `split_paths(..).collect::<Vec<_>>()`.
func (s *EnvSplitPaths) Collect() []string {
	var paths []string
	for path := range s.All() {
		paths = append(paths, path)
	}
	return paths
}

// FmtDebug is `impl Debug for SplitPaths`: `SplitPaths { .. }`.
func (s *EnvSplitPaths) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("SplitPaths").FinishNonExhaustive()
}

// JoinPathsError is std::env::JoinPathsError: the error type for operations
// on the `PATH` variable. Possibly returned from JoinPaths.
type JoinPathsError struct {
	inner sysJoinPathsError
}

// JoinPaths is std::env::join_paths: joins a collection of paths
// appropriately for the `PATH` environment variable.
//
// # Errors
//
// Returns an error (a *JoinPathsError) if one of the input paths contains an
// invalid character for constructing the `PATH` variable (a double quote on
// Windows or a colon on Unix), or if the system does not have a `PATH`-like
// variable.
func JoinPaths(paths []string) (string, error) {
	joined, err := sysJoinPaths(rspath.Host, paths)
	if err != nil {
		return "", &JoinPathsError{inner: *err}
	}
	return joined, nil
}

// String holds the Display text, which is the platform's: on Unix "path
// segment contains separator `:`", on Windows "path segment contains `\"`".
func (e *JoinPathsError) String() string {
	return e.inner.String()
}

// Error makes JoinPathsError a Go error with its Display text.
func (e *JoinPathsError) Error() string { return e.String() }

// FmtDisplay is `impl Display for JoinPathsError`.
func (e *JoinPathsError) FmtDisplay(f *rs.Formatter) error {
	return e.inner.FmtDisplay(f)
}

// FmtDebug is the derived Debug of JoinPathsError: `JoinPathsError { inner:
// JoinPathsError }`.
func (e *JoinPathsError) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("JoinPathsError").Field("inner", &e.inner).Finish()
}

// HomeDir is std::env::home_dir: the path of the current user's home
// directory if known.
//
// This may return None if getting the directory fails or if the platform
// does not have user home directories.
//
// # Unix
//
//   - Returns the value of the `HOME` environment variable if it is set and
//     not an empty string.
//   - Otherwise, it tries to determine the home directory by looking up the
//     current user's entry in the user database, as getpwuid_r does.
//   - If this fails, returns None.
//
// # Windows
//
//   - Returns the value of the `USERPROFILE` environment variable if it is
//     set and not an empty string.
//   - Otherwise, GetUserProfileDirectory is used to return the path. This may
//     change in the future.
//
// In UWP (Universal Windows Platform) targets this function is unimplemented
// and always returns None.
//
// Before Rust 1.85.0, this function used to return the value of the `HOME`
// environment variable on Windows, which in Cygwin or Mingw environments
// could return non-standard paths like `/home/you` instead of
// `C:\Users\you`.
func HomeDir() rs.Option[string] {
	return sysHomeDir()
}

// TempDir is std::env::temp_dir: the path of a temporary directory.
//
// The temporary directory may be shared among users, or between processes
// with different privileges; thus, the creation of any files or directories
// in the temporary directory must use a secure method to create a uniquely
// named file. Creating a file or directory with a fixed or predictable name
// may result in "insecure temporary file" security vulnerabilities.
//
// # Platform-specific behavior
//
// On Unix, returns the value of the `TMPDIR` environment variable if it is
// set, otherwise the value is OS-specific:
//
//   - On Darwin-based OSes (macOS, iOS, etc) it returns the directory
//     provided by `confstr(_CS_DARWIN_USER_TEMP_DIR, ...)`, as recommended
//     by Apple's security guidelines.
//   - On all other unix-based OSes, it returns `/tmp`.
//
// On Windows, the behavior is equivalent to that of GetTempPath2 /
// GetTempPath, which this function uses internally.
//
// Note that, this may change in the future.
func TempDir() string {
	return sysTempDir()
}

// CurrentExe is std::env::current_exe: the full filesystem path of the
// current running executable.
//
// # Platform-specific behavior
//
// If the executable was invoked through a symbolic link, some platforms will
// return the path of the symbolic link and other platforms will return the
// path of the symbolic link's target.
//
// If the executable is renamed while it is running, platforms may return the
// path at the time it was loaded instead of the new path.
//
// On Linux this is the target of /proc/self/exe, which keeps the " (deleted)"
// suffix the kernel gives a file that has been removed.
//
// # Errors
//
// Acquiring the path of the current executable is a platform-specific
// operation that can fail for a good number of reasons. Some errors can
// include, but not be limited to, filesystem operations failing or general
// syscall failures. The error is an *rsio.Error.
//
// # Security
//
// The output of this function must be treated with care to avoid security
// vulnerabilities, particularly in processes that run with privileges higher
// than the user, such as setuid or setgid programs.
func CurrentExe() (string, error) {
	return sysCurrentExe()
}

// EnvArgs is std::env::Args: an iterator over the arguments of a process,
// yielding a String value for each argument.
//
// The first element is traditionally the path of the executable, but it can
// be set to arbitrary text, and might not even exist. This means this
// property should not be relied upon for security purposes.
type EnvArgs struct {
	inner EnvArgsOs
}

// EnvArgsOs is std::env::ArgsOs: an iterator over the arguments of a process,
// yielding an OsString value for each argument.
type EnvArgsOs struct {
	inner sysArgs
}

// Args is std::env::args: the arguments that this program was started with
// (normally passed via the command line).
//
// # Panics
//
// The returned iterator will panic during iteration if any argument to the
// process is not valid Unicode. If this is not desired, use ArgsOs.
func Args() *EnvArgs {
	return &EnvArgs{inner: *ArgsOs()}
}

// ArgsOs is std::env::args_os: the arguments that this program was started
// with (normally passed via the command line).
//
// The first element is traditionally the path of the executable, but it can
// be set to arbitrary text, and might not even exist. This means this
// property should not be relied upon for security purposes.
//
// On Unix systems the shell usually expands unquoted arguments with glob
// patterns (such as `*` and `?`). On Windows this is not done, and such
// arguments are passed as-is.
//
// On Windows the arguments are parsed out of the process's command line as
// Rust parses them, which differs from Go's os.Args where a quote is
// doubled or the program name holds a backslash.
//
// Note that the returned iterator will not check if the arguments to the
// process are valid Unicode.
func ArgsOs() *EnvArgsOs {
	return &EnvArgsOs{inner: sysArgsNew()}
}

// Next is Iterator::next for Args. It panics, as `into_string().unwrap()`
// does, on an argument that is not valid Unicode.
func (a *EnvArgs) Next() rs.Option[string] {
	arg, ok := a.inner.Next().Get()
	if !ok {
		return rs.None[string]()
	}
	return rs.Some(rs.Unwrap(rspath.OsStringIntoString(arg)))
}

// NextBack is DoubleEndedIterator::next_back for Args.
func (a *EnvArgs) NextBack() rs.Option[string] {
	arg, ok := a.inner.NextBack().Get()
	if !ok {
		return rs.None[string]()
	}
	return rs.Some(rs.Unwrap(rspath.OsStringIntoString(arg)))
}

// Len is ExactSizeIterator::len for Args: how many arguments remain.
func (a *EnvArgs) Len() int { return a.inner.Len() }

// IsEmpty is ExactSizeIterator::is_empty for Args.
func (a *EnvArgs) IsEmpty() bool { return a.inner.IsEmpty() }

// All ranges over the arguments, consuming the iterator as a Rust `for` loop
// does.
func (a *EnvArgs) All() iter.Seq[string] {
	return func(yield func(string) bool) {
		for {
			arg, ok := a.Next().Get()
			if !ok || !yield(arg) {
				return
			}
		}
	}
}

// Collect is `args.collect::<Vec<_>>()`.
func (a *EnvArgs) Collect() []string {
	var args []string
	for arg := range a.All() {
		args = append(args, arg)
	}
	return args
}

// FmtDebug is `impl Debug for Args`: `Args { inner: ["a", "b"] }`, listing
// the arguments still to come.
func (a *EnvArgs) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("Args").Field("inner", &a.inner.inner).Finish()
}

// Next is Iterator::next for ArgsOs.
func (a *EnvArgsOs) Next() rs.Option[string] { return a.inner.next() }

// NextBack is DoubleEndedIterator::next_back for ArgsOs.
func (a *EnvArgsOs) NextBack() rs.Option[string] { return a.inner.nextBack() }

// Len is ExactSizeIterator::len for ArgsOs: how many arguments remain.
func (a *EnvArgsOs) Len() int { return a.inner.len() }

// IsEmpty is ExactSizeIterator::is_empty for ArgsOs.
func (a *EnvArgsOs) IsEmpty() bool { return a.inner.len() == 0 }

// All ranges over the arguments, consuming the iterator as a Rust `for` loop
// does.
func (a *EnvArgsOs) All() iter.Seq[string] {
	return func(yield func(string) bool) {
		for {
			arg, ok := a.Next().Get()
			if !ok || !yield(arg) {
				return
			}
		}
	}
}

// Collect is `args_os.collect::<Vec<_>>()`.
func (a *EnvArgsOs) Collect() []string {
	var args []string
	for arg := range a.All() {
		args = append(args, arg)
	}
	return args
}

// FmtDebug is `impl Debug for ArgsOs`: `ArgsOs { inner: ["a", "b"] }`,
// listing the arguments still to come.
func (a *EnvArgsOs) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("ArgsOs").Field("inner", &a.inner).Finish()
}

// Constants associated with the current target (std::env::consts).

// ConstsArch is std::env::consts::ARCH: a string describing the architecture
// of the CPU that is currently in use, `x86_64` or `aarch64`.
const ConstsArch = sysArch

// ConstsFamily is std::env::consts::FAMILY: the family of the operating
// system, `unix` or `windows`.
const ConstsFamily = sysFamily

// ConstsOs is std::env::consts::OS: a string describing the specific
// operating system in use, `linux`, `macos` or `windows`.
const ConstsOs = sysOs

// ConstsDllPrefix is std::env::consts::DLL_PREFIX: the prefix used for
// shared libraries on this platform, `lib` or empty on Windows.
const ConstsDllPrefix = sysDllPrefix

// ConstsDllSuffix is std::env::consts::DLL_SUFFIX: the suffix used for shared
// libraries on this platform, `.so`, `.dylib` or `.dll`.
const ConstsDllSuffix = sysDllSuffix

// ConstsDllExtension is std::env::consts::DLL_EXTENSION: DLL_SUFFIX without
// the dot.
const ConstsDllExtension = sysDllExtension

// ConstsExeSuffix is std::env::consts::EXE_SUFFIX: the suffix used for
// executable binaries on this platform, `.exe` on Windows and empty
// elsewhere.
const ConstsExeSuffix = sysExeSuffix

// ConstsExeExtension is std::env::consts::EXE_EXTENSION: EXE_SUFFIX without
// the dot.
const ConstsExeExtension = sysExeExtension

// containsNul reports whether s holds a NUL byte, which no C string and no
// string given to the Windows API can hold.
func containsNul(s string) bool {
	return strings.IndexByte(s, 0) >= 0
}
