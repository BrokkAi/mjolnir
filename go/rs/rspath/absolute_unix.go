// Port of std 1.99.1 src/sys/path/unix.rs (absolute) and
// src/sys/paths/unix.rs (getcwd)

//go:build !windows

package rspath

import (
	"strings"
	"syscall"
)

// sysAbsolute is sys::path::absolute: make a POSIX path absolute without
// changing its semantics.
func sysAbsolute(path string) (string, error) {
	// This is mostly a wrapper around collecting `Path::components`, with
	// exceptions made where this conflicts with the POSIX specification.
	// See 4.16 Pathname Resolution, IEEE Std 1003.1-2024
	// https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/V1_chap04.html#tag_04_16

	// Get the components, skipping the redundant leading "." component if it exists.
	stripped, err := Unix.StripPrefix(path, ".")
	if err != nil {
		stripped = path
	}
	components := Unix.Components(stripped)
	pathOs := path

	var normalized string
	if Unix.IsAbsolute(path) {
		// "If a pathname begins with two successive <slash> characters, the
		// first component following the leading <slash> characters may be
		// interpreted in an implementation-defined manner, although more than
		// two leading <slash> characters shall be treated as a single <slash>
		// character."
		if strings.HasPrefix(pathOs, "//") && !strings.HasPrefix(pathOs, "///") {
			components.Next()
			normalized = "//"
		} else {
			normalized = ""
		}
	} else {
		normalized, err = getcwd()
		if err != nil {
			return "", err
		}
	}
	Unix.ExtendComponents(&normalized, components.Collect())

	// "Interfaces using pathname resolution may specify additional constraints
	// when a pathname that does not name an existing directory contains at
	// least one non- <slash> character and contains one or more trailing
	// <slash> characters".
	// A trailing <slash> is also meaningful if "a symbolic link is
	// encountered during pathname resolution".
	if strings.HasSuffix(pathOs, "/") {
		Unix.Push(&normalized, "")
	}

	return normalized, nil
}

// getcwd is sys::paths::getcwd, which env::current_dir is: the kernel's name
// for the current directory. os.Getwd is not that: it answers with $PWD when
// that names the same directory, so it keeps a symbolic link the shell came
// through, and getcwd(3) does not.
func getcwd() (string, error) {
	directory, err := syscall.Getwd()
	if err != nil {
		return "", ioErrorFromOs(err)
	}
	return directory, nil
}
