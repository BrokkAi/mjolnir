// Port of std 1.99.1 src/path.rs (absolute)

package rspath

// Absolute is std::path::absolute: it makes the path absolute without
// accessing the filesystem.
//
// If the path is relative, the current directory is used as the base
// directory. All intermediate components will be resolved according to
// platform-specific rules, but unlike canonicalize, this does not resolve
// symlinks and may succeed even if the path does not exist.
//
// If the path is empty or getting the current directory fails, then an error
// will be returned.
//
// # Platform-specific behavior
//
// On POSIX platforms, the path is resolved using POSIX semantics, except that
// it stops short of resolving symlinks. This means it will keep `..`
// components and trailing separators.
//
// On Windows, for verbatim paths, this will simply return the path as given.
// For other paths, this is currently equivalent to calling
// GetFullPathNameW.
//
// It follows the syntax of the platform the program is built for and reads
// that platform's current directory, so it is not a method of Flavor.
func Absolute(path string) (string, error) {
	if len(path) == 0 {
		return "", ioErrorInvalidInput("cannot make an empty path absolute")
	}
	return sysAbsolute(path)
}
