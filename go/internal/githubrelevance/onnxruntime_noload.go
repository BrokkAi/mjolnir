//go:build !windows && !((darwin || freebsd || linux || netbsd) && !android && !faketime)

// Hosts without a dynamic loader binding: ONNX Runtime cannot be loaded
// (runtime.rs's target() already refuses them before any download).

package githubrelevance

import (
	"errors"
	"unsafe"
)

func ortOpenLibrary(string) (uintptr, error) {
	return 0, errors.New("dynamic loading is unavailable on this platform")
}

func ortSymbol(uintptr, string) (uintptr, error) {
	return 0, errors.New("dynamic loading is unavailable on this platform")
}

type ortPath []byte

func ortPathChars(path string) ortPath { return cString(path) }

func (p ortPath) pointer() unsafe.Pointer { return unsafe.Pointer(&p[0]) }
