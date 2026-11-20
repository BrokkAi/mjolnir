// Port of std 1.99.1 src/sys/args/unix.rs

//go:build unix

package rsenv

import (
	"os"
	"slices"
)

// sysArgsNew is sys::args::args: the arguments the program was started with.
//
// Rust reads them from argv, which the C runtime handed to the program; the
// Go runtime copied the same argv into os.Args. Each call gets its own copy,
// as Rust builds a new vector.
func sysArgsNew() sysArgs {
	return sysArgs{iter: slices.Clone(os.Args)}
}
