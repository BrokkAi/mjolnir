//go:build (darwin || freebsd || linux || netbsd) && !android && !faketime

// ONNX Runtime library loading on Unix: dlopen through purego, as ort's
// load-dynamic opens it with libloading (RTLD_LAZY | RTLD_LOCAL).

package githubrelevance

import (
	"unsafe"

	"github.com/ebitengine/purego"
)

func ortOpenLibrary(path string) (uintptr, error) {
	return purego.Dlopen(path, purego.RTLD_LAZY|purego.RTLD_LOCAL)
}

func ortSymbol(library uintptr, name string) (uintptr, error) {
	return purego.Dlsym(library, name)
}

// ortPath is an ORTCHAR_T path: UTF-8 on Unix.
type ortPath []byte

func ortPathChars(path string) ortPath { return cString(path) }

func (p ortPath) pointer() unsafe.Pointer { return unsafe.Pointer(&p[0]) }
