// Package rspath stands in for Rust's std::path and std::ffi::OsStr and
// OsString, as of Rust 1.99.1 (std/src/path.rs, std/src/sys/path/unix.rs,
// windows.rs and windows_prefix.rs, std/src/ffi/os_str.rs,
// std/src/sys/os_str/bytes.rs and wtf8.rs, core/src/wtf8.rs,
// alloc/src/wtf8, std/src/os/unix/ffi and std/src/os/windows/ffi).
//
// # Representation
//
// A Rust Path, PathBuf, OsStr and OsString are all a Go string here, holding
// the encoded bytes of the OsStr: any bytes on Unix, WTF-8 on Windows (which
// is also how Go holds a Windows file name with an unpaired surrogate). The
// conversions that only change the Rust type (Path::new, PathBuf::from,
// as_os_str, to_path_buf, into_os_string, Box<Path>, Rc<Path>, Arc<Path>,
// Cow<Path>) are the identity. Methods are functions taking the path first;
// the ones that change a PathBuf or OsString take a *string.
//
// Nothing is cleaned: Join("a", "../b") is "a/../b". Only this package
// interprets a path; product code does not use path or path/filepath.
//
// # Flavours
//
// Rust compiles one path syntax into std. This package holds both, as the
// Flavor values Unix and Windows, so that either can be used and tested on
// any host (Mjolnir handles remote Linux paths while running on Windows):
//
//	rspath.Join(base, name)            // as std::path does on this platform
//	rspath.Unix.Join(base, name)       // always as on Unix
//	rspath.Windows.IsAbsolute(path)    // always as on Windows
//
// The package-level functions are the methods of Host, the platform the
// program is built for. The Windows flavour has the drive, UNC, device and
// verbatim prefixes and both separators; the Unix flavour has neither.
//
// # Covered
//
// std::path: Components (PathComponents, with Next, NextBack, AsPath,
// Equal, Compare, Debug), Component and Prefix with their variants,
// PrefixComponent, Iter (PathIter), Ancestors (PathAncestors), Join, Push,
// Pop, Extend, PathBufFromIter, Parent, FileName, FileStem, FilePrefix,
// Extension, WithFileName, WithExtension, WithAddedExtension, SetFileName,
// SetExtension, AddExtension, StartsWith, EndsWith, StripPrefix with
// StripPrefixError, TrimPrefix, IsAbsolute, IsRelative, HasRoot, IsEmpty,
// the trailing-separator methods, NormalizeLexically with NormalizeError,
// ToStr, ToStringLossy, Display, Debug, Eq and Cmp (by components, as Rust
// compares paths), HashKey (a map key equal exactly when Eq is), IsSeparator,
// MainSeparator, Separators, and std::path::absolute (Absolute, host only).
//
// std::ffi: OsStr::to_str, to_string_lossy, display, len, is_empty,
// is_ascii, eq_ignore_ascii_case, to_ascii_lowercase and uppercase,
// make_ascii_lowercase and uppercase, as_encoded_bytes,
// from_encoded_bytes_unchecked; OsString::push (which joins a lead and a
// trail surrogate on Windows), into_string with IntoStringError, and joining
// a slice of OsStr; std::os::unix::ffi (as_bytes, from_bytes, from_vec,
// into_vec) and std::os::windows::ffi (encode_wide, from_wide).
//
// Display and ToStringLossy replace what is not Unicode as Rust does: on Unix
// each maximal broken UTF-8 sequence becomes one U+FFFD, on Windows each
// unpaired surrogate does. Debug escapes as Rust's OsStr Debug does: `\xFF`
// for a broken byte on Unix and `\u{d800}` for a surrogate on Windows. Padding
// Display's text as a str gives what Rust prints for `{:>10}` and the like,
// except for a Windows path that holds an unpaired surrogate, which Rust
// writes without applying the flags.
//
// # Not covered
//
//   - The Path methods that touch the file system (metadata,
//     symlink_metadata, canonicalize, read_link, read_dir, exists,
//     try_exists, is_file, is_dir, is_symlink) belong to go/rs/rsfs.
//     Mjolnir calls exists, is_file, is_dir, is_symlink, canonicalize,
//     metadata, try_exists, symlink_metadata and read_dir.
//   - Capacity management (with_capacity, capacity, reserve, try_reserve,
//     shrink_to_fit, shrink_to, leak) has no meaning for a Go string;
//     clearing a PathBuf or OsString is assigning "".
//   - The Cygwin flavour of std::path (and test_decompositions_cygwin).
//
// # Tests
//
// it_path_test.go ports std/tests/path.rs, both flavours on any host;
// sys_path_windows_mod_test.go, ffi_os_str_test.go, sys_os_str_bytes_test.go
// and wtf8_test.go port the unit tests of the Rust files above. The
// differential tests (oracle_test.go) compare every function with the real
// std::path (go/port/oracle, module std_path) over edge cases and seeded
// random paths. The corpus std_path_unix was recorded on Linux. A Windows
// corpus (std_path_windows) can only be recorded where a Windows build of the
// oracle runs; TestDiffAgainstRustOtherFlavor replays it when it is there.
// Until then the Windows flavour is held to Rust's own Windows tests and,
// for paths without a prefix, to the Unix flavour
// (flavor_agreement_test.go).
package rspath
