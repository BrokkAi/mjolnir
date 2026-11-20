package rspath

import "github.com/BrokkAi/mjolnir/go/rs"

// The functions below are std::path and std::ffi::OsStr as the Rust program
// has them on the platform this program is built for: each is the method of
// the same name on Host, where the documentation is.

// Components is Path::components.
func Components(path string) *PathComponents { return Host.Components(path) }

// Iter is Path::iter.
func Iter(path string) *PathIter { return Host.Iter(path) }

// Ancestors is Path::ancestors.
func Ancestors(path string) *PathAncestors { return Host.Ancestors(path) }

// ComponentAsOsStr is Component::as_os_str.
func ComponentAsOsStr(c Component) string { return Host.ComponentAsOsStr(c) }

// Join is Path::join. Nothing is cleaned: Join("a", "../b") is "a/../b".
func Join(base, path string) string { return Host.Join(base, path) }

// Push is PathBuf::push.
func Push(buf *string, path string) { Host.Push(buf, path) }

// Pop is PathBuf::pop.
func Pop(buf *string) bool { return Host.Pop(buf) }

// Parent is Path::parent.
func Parent(path string) rs.Option[string] { return Host.Parent(path) }

// FileName is Path::file_name.
func FileName(path string) rs.Option[string] { return Host.FileName(path) }

// FileStem is Path::file_stem.
func FileStem(path string) rs.Option[string] { return Host.FileStem(path) }

// FilePrefix is Path::file_prefix.
func FilePrefix(path string) rs.Option[string] { return Host.FilePrefix(path) }

// Extension is Path::extension.
func Extension(path string) rs.Option[string] { return Host.Extension(path) }

// WithFileName is Path::with_file_name.
func WithFileName(path, fileName string) string { return Host.WithFileName(path, fileName) }

// WithExtension is Path::with_extension.
func WithExtension(path, extension string) string { return Host.WithExtension(path, extension) }

// WithAddedExtension is Path::with_added_extension.
func WithAddedExtension(path, extension string) string {
	return Host.WithAddedExtension(path, extension)
}

// SetFileName is PathBuf::set_file_name.
func SetFileName(buf *string, fileName string) { Host.SetFileName(buf, fileName) }

// SetExtension is PathBuf::set_extension.
func SetExtension(buf *string, extension string) bool { return Host.SetExtension(buf, extension) }

// AddExtension is PathBuf::add_extension.
func AddExtension(buf *string, extension string) bool { return Host.AddExtension(buf, extension) }

// StartsWith is Path::starts_with. It compares whole components, not text.
func StartsWith(path, base string) bool { return Host.StartsWith(path, base) }

// EndsWith is Path::ends_with. It compares whole components, not text.
func EndsWith(path, child string) bool { return Host.EndsWith(path, child) }

// StripPrefix is Path::strip_prefix. The error is a *StripPrefixError.
func StripPrefix(path, base string) (string, error) { return Host.StripPrefix(path, base) }

// TrimPrefix is Path::trim_prefix (unstable in Rust 1.99.1).
func TrimPrefix(path, base string) string { return Host.TrimPrefix(path, base) }

// IsAbsolute is Path::is_absolute.
func IsAbsolute(path string) bool { return Host.IsAbsolute(path) }

// IsRelative is Path::is_relative.
func IsRelative(path string) bool { return Host.IsRelative(path) }

// HasRoot is Path::has_root.
func HasRoot(path string) bool { return Host.HasRoot(path) }

// IsEmpty is Path::is_empty.
func IsEmpty(path string) bool { return Host.IsEmpty(path) }

// ToStr is Path::to_str and OsStr::to_str: None when the path is not valid
// Unicode.
func ToStr(path string) rs.Option[string] { return Host.ToStr(path) }

// ToStringLossy is Path::to_string_lossy and OsStr::to_string_lossy.
func ToStringLossy(path string) string { return Host.ToStringLossy(path) }

// Display is Path::display and OsStr::display made into text.
func Display(path string) string { return Host.Display(path) }

// Debug wraps a path or an OsStr for `{:?}`.
func Debug(path string) PathDebug { return Host.Debug(path) }

// DebugString is `format!("{:?}", path)` for a Path, PathBuf, OsStr or
// OsString.
func DebugString(path string) string { return Host.DebugString(path) }

// Eq is `a == b` for two paths, which Rust compares by components.
func Eq(a, b string) bool { return Host.Eq(a, b) }

// Cmp is `a.cmp(&b)` for two paths, which Rust orders by components.
func Cmp(a, b string) int { return Host.Cmp(a, b) }

// HashKey is a key that is the same for two paths exactly when Eq says they
// are equal.
func HashKey(path string) string { return Host.HashKey(path) }

// PathBufFromIter is `iter.collect::<PathBuf>()`.
func PathBufFromIter(parts ...string) string { return Host.PathBufFromIter(parts...) }

// Extend is PathBuf::extend.
func Extend(buf *string, parts ...string) { Host.Extend(buf, parts...) }

// PathBufFromComponents is `components.collect::<PathBuf>()`.
func PathBufFromComponents(components []Component) string {
	return Host.PathBufFromComponents(components)
}

// ExtendComponents is PathBuf::extend over components.
func ExtendComponents(buf *string, components []Component) {
	Host.ExtendComponents(buf, components)
}

// PathBufIntoString is PathBuf::into_string. The error is an
// *IntoStringError.
func PathBufIntoString(path string) (string, error) { return Host.PathBufIntoString(path) }

// HasTrailingSep is Path::has_trailing_sep (unstable in Rust 1.99.1).
func HasTrailingSep(path string) bool { return Host.HasTrailingSep(path) }

// WithTrailingSep is Path::with_trailing_sep (unstable in Rust 1.99.1).
func WithTrailingSep(path string) string { return Host.WithTrailingSep(path) }

// TrimTrailingSep is Path::trim_trailing_sep (unstable in Rust 1.99.1).
func TrimTrailingSep(path string) string { return Host.TrimTrailingSep(path) }

// SetTrailingSep is PathBuf::set_trailing_sep (unstable in Rust 1.99.1).
func SetTrailingSep(buf *string, trailingSep bool) { Host.SetTrailingSep(buf, trailingSep) }

// PushTrailingSep is PathBuf::push_trailing_sep (unstable in Rust 1.99.1).
func PushTrailingSep(buf *string) { Host.PushTrailingSep(buf) }

// PopTrailingSep is PathBuf::pop_trailing_sep (unstable in Rust 1.99.1).
func PopTrailingSep(buf *string) { Host.PopTrailingSep(buf) }

// NormalizeLexically is Path::normalize_lexically (unstable in Rust 1.99.1).
// The error is a *NormalizeError.
func NormalizeLexically(path string) (string, error) { return Host.NormalizeLexically(path) }

// OsStrToStr is OsStr::to_str.
func OsStrToStr(s string) rs.Option[string] { return Host.OsStrToStr(s) }

// OsStrToStringLossy is OsStr::to_string_lossy.
func OsStrToStringLossy(s string) string { return Host.OsStrToStringLossy(s) }

// OsStrDisplay is OsStr::display made into text.
func OsStrDisplay(s string) string { return Host.OsStrDisplay(s) }

// OsStringIntoString is OsString::into_string. The error is an
// *IntoStringError.
func OsStringIntoString(s string) (string, error) { return Host.OsStringIntoString(s) }

// OsStringPush is OsString::push.
func OsStringPush(buf *string, s string) { Host.OsStringPush(buf, s) }

// OsStrJoin is `[S]::join(&OsStr)` for a slice of OsStr.
func OsStrJoin(parts []string, sep string) string { return Host.OsStrJoin(parts, sep) }
