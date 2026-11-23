//go:build windows

// Port of the #[cfg(windows)] parts of
// mj-github-relevance/src/github_student/runtime.rs: the DirectX 12 probe,
// the Visual C++ runtime check and the verified DLL preload.

package githubrelevance

import (
	"errors"
	"fmt"
	"path/filepath"
	"sort"
	"strings"
	"syscall"
	"unsafe"

	"golang.org/x/sys/windows"
)

// directx12Available reports whether D3D12CreateDevice accepts the default
// adapter at feature level 11.0, DirectML's documented minimum. A null
// output validates support without creating a device (S_FALSE on success).
func directx12Available() bool {
	library, err := windows.LoadLibraryEx("d3d12.dll", 0, windows.LOAD_LIBRARY_SEARCH_SYSTEM32)
	if err != nil {
		return false
	}
	defer windows.FreeLibrary(library)
	create, err := windows.GetProcAddress(library, "D3D12CreateDevice")
	if err != nil {
		return false
	}
	// IID_ID3D12Device {189819f1-1db6-4b57-be54-1821339b85f7}.
	device := &windows.GUID{Data1: 0x189819f1, Data2: 0x1db6, Data3: 0x4b57, Data4: [8]byte{0xbe, 0x54, 0x18, 0x21, 0x33, 0x9b, 0x85, 0xf7}}
	result, _, _ := syscall.SyscallN(create, 0, 0xb000, uintptr(unsafe.Pointer(device)), 0)
	return int32(result) >= 0
}

// checkWindowsRuntime inspects only the Microsoft runtime DLLs in System32;
// no symbols are invoked.
func checkWindowsRuntime() error {
	var missing []string
	for _, name := range []string{"VCRUNTIME140.dll", "VCRUNTIME140_1.dll", "MSVCP140.dll", "MSVCP140_1.dll"} {
		library, err := windows.LoadLibraryEx(name, 0, windows.LOAD_LIBRARY_SEARCH_SYSTEM32)
		if err != nil {
			missing = append(missing, name)
			continue
		}
		windows.FreeLibrary(library)
	}
	if len(missing) == 0 {
		return nil
	}
	return &preparationFailure{
		kind:   StudentFailureKindWindowsRuntimeMissing,
		cause:  fmt.Sprintf("Microsoft Visual C++ Redistributable x64 DLLs missing: %s", strings.Join(missing, ", ")),
		notice: StudentNoticeWindowsRuntimeMissing{Missing: missing},
	}
}

// preloadWindowsLibraries loads the verified main DLL and every bundled
// dependency by absolute path, once per process, searching only their
// directory and System32 so no PATH or current-directory copy is used. The
// handles stay loaded for the process-global ORT API's lifetime.
func preloadWindowsLibraries(path string) error {
	runtimeWindowsLibraries.mu.Lock()
	defer runtimeWindowsLibraries.mu.Unlock()
	if len(runtimeWindowsLibraries.loaded) > 0 {
		return nil
	}
	selected := runtimeSelection.Load()
	if selected == nil {
		return errors.New("runtime selection missing")
	}
	platform, ok := pinnedRuntimeManifest().Platforms[selected.target]
	if !ok {
		return errors.New("unsupported runtime target")
	}
	var names []string
	for _, archive := range platform.Archives {
		for _, file := range archive.Files {
			if strings.HasSuffix(file.Name, ".dll") {
				names = append(names, file.Name)
			}
		}
	}
	// onnxruntime.dll last, the dependencies first in manifest order.
	sort.SliceStable(names, func(i, j int) bool { return names[i] != "onnxruntime.dll" && names[j] == "onnxruntime.dll" })
	root := filepath.Dir(path)
	var loaded []uintptr
	for _, name := range names {
		library, err := windows.LoadLibraryEx(filepath.Join(root, name), 0,
			windows.LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR|windows.LOAD_LIBRARY_SEARCH_SYSTEM32)
		if err != nil {
			return err
		}
		loaded = append(loaded, uintptr(library))
	}
	runtimeWindowsLibraries.loaded = loaded
	return nil
}
