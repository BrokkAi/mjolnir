//go:build !android

// Port of the #[cfg(all(target_os = "linux", target_arch = "x86_64"))] part
// of mj-github-relevance/src/github_student/runtime.rs's select: the CUDA
// library probe.

package githubrelevance

import "github.com/ebitengine/purego"

// cudaSonames are the exact non-system dependencies from ldd of the pinned
// ORT 1.23.0 CUDA provider (not guessed CUDA package names).
var cudaSonames = []string{
	"libcublasLt.so.12",
	"libcublas.so.12",
	"libcurand.so.10",
	"libcufft.so.11",
	"libcudart.so.12",
	"libcudnn.so.9",
}

// probeCUDA opens the driver and then each dependency through the system
// loader, holding every handle until all are checked; no symbol is called.
// driver is false without libcuda.so.1; missing names the dependencies the
// loader could not open.
func probeCUDA() (missing []string, driver bool) {
	handle, err := purego.Dlopen("libcuda.so.1", purego.RTLD_LAZY|purego.RTLD_LOCAL)
	if err != nil {
		return nil, false
	}
	libraries := []uintptr{handle}
	defer func() {
		for _, library := range libraries {
			purego.Dlclose(library)
		}
	}()
	for _, name := range cudaSonames {
		library, err := purego.Dlopen(name, purego.RTLD_LAZY|purego.RTLD_LOCAL)
		if err != nil {
			missing = append(missing, name)
			continue
		}
		libraries = append(libraries, library)
	}
	return missing, true
}
