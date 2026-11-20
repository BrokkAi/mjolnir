// Package rsenv stands in for Rust's std::env, as of Rust 1.99.1
// (std/src/env.rs, std/src/sys/env, std/src/sys/paths, std/src/sys/args and
// std/src/sys/env_consts.rs). It wraps the Go runtime's environment, os and
// golang.org/x/sys with Rust's names, results, error texts and panics.
//
// Paths, OsStr and OsString are Go strings, as in go/rs/rspath; errors that
// are io::Error in Rust are *rsio.Error.
//
// # Covered
//
//   - CurrentDir, SetCurrentDir (getcwd and chdir; GetCurrentDirectoryW and
//     SetCurrentDirectoryW).
//   - Var with VarError (VarError_NotPresent, VarError_NotUnicode: Display,
//     Debug and the derived Debug's OsString form), VarOs, Vars and VarsOs
//     (snapshots, iterated with Next, All or Collect, with Rust's Debug and
//     the panic on a name or value that is not Unicode), SetVar and
//     RemoveVar with Rust's panics ("failed to set environment variable
//     ...") for an empty name, a name holding `=`, or a NUL.
//   - SplitPaths and JoinPaths with JoinPathsError: `:` on Unix; `;` with
//     double-quoted regions on Windows. Both platforms' rules are built on
//     every host and tested there.
//   - HomeDir, TempDir, CurrentExe.
//   - Args and ArgsOs (Next, NextBack, Len, All, Collect, Debug). On Windows
//     the arguments are parsed from GetCommandLineW by Rust's parser, which
//     differs from os.Args where a quote is doubled or the program name
//     holds a backslash.
//   - The constants of std::env::consts (ConstsArch, ConstsOs, ...), for
//     linux, darwin and windows on amd64 and arm64. Another architecture
//     does not build: Rust's name for it is not recorded here.
//
// The iterator types carry the module's name (EnvVars, EnvVarsOs, EnvArgs,
// EnvArgsOs, EnvSplitPaths) because Vars, VarsOs, Args, ArgsOs and
// SplitPaths are the functions, as std::env::vars and the others are.
//
// # Where Go and Rust differ
//
// A Go program built without C has one environment, the Go runtime's, and no
// C library to ask. What the C library would do is done here:
//
//   - getenv: a name without `=` is looked up as every C library does. A
//     name holding `=` follows glibc on Linux (the first entry beginning
//     with the whole name and `=`) and the BSD rule on macOS (the name up to
//     its first `=`); the macOS rule is not checked against Rust here.
//   - Two entries with the same name: the Go runtime keeps the first and
//     drops the second, so VarsOs lists one where Rust's Unix code lists
//     both.
//   - home_dir's fallback, when HOME is unset or empty, reads the user
//     database: through the C library on macOS, and /etc/passwd on Linux,
//     which is what glibc reads unless nsswitch.conf names another source.
//   - temp_dir on macOS without TMPDIR asks `getconf DARWIN_USER_TEMP_DIR`,
//     which answers with confstr(_CS_DARWIN_USER_TEMP_DIR) as Rust asks it,
//     and falls back to /tmp as Rust does when that fails.
//   - current_exe on Linux reads /proc/self/exe, keeping the " (deleted)"
//     suffix of a removed executable (os.Executable removes it). On macOS it
//     is os.Executable, which gives the path _NSGetExecutablePath gives
//     except that a relative launch path loses a leading "./" that dyld
//     keeps ("/start/prog" against "/start/./prog").
//
// # Tests
//
// it_env_test.go and it_env_modify_test.go port std/tests/env.rs and
// env_modify.rs; sys_args_windows_mod_test.go ports the tests of the Windows
// command-line parser. The differential test (oracle_linux_test.go) compares
// split_paths, join_paths, VarError, getting, setting and removing
// variables with their panics, the current directory (including a deleted
// one), temp_dir, home_dir and current_exe with the real std::env
// (go/port/oracle, module std_path); its corpus is recorded on Linux. Under
// MJ_ORACLE=live it also compares the constants and home_dir's fallback,
// which depend on the machine. current_exe_linux_test.go holds CurrentExe to
// the " (deleted)" suffix.
package rsenv
