//go:build windows

// ONNX Runtime library loading on Windows: LoadLibraryExW with no flags,
// as ort's load-dynamic opens it with libloading; the verified DLLs are
// preloaded by absolute path first (github_student_runtime_windows.go).

package githubrelevance

import (
	"unsafe"

	"golang.org/x/sys/windows"
)

func ortOpenLibrary(path string) (uintptr, error) {
	handle, err := windows.LoadLibraryEx(path, 0, 0)
	return uintptr(handle), err
}

func ortSymbol(library uintptr, name string) (uintptr, error) {
	return windows.GetProcAddress(windows.Handle(library), name)
}

// ortPath is an ORTCHAR_T path: wchar_t (UTF-16) on Windows.
type ortPath []uint16

func ortPathChars(path string) ortPath {
	chars, err := windows.UTF16FromString(path)
	if err != nil {
		// A path with a NUL cannot name a file; ORT reports it missing.
		chars = []uint16{0}
	}
	return chars
}

func (p ortPath) pointer() unsafe.Pointer { return unsafe.Pointer(&p[0]) }
