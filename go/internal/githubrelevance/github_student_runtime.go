// Port of mj-github-relevance/src/github_student/runtime.rs.

// One native-runtime mechanism on every install: official pinned downloads,
// verified and atomically cached by the same preparation owner as the model.
//
// The private manifest types carry a `runtime` prefix: Manifest collides with
// assets.rs's, and the others would shadow the local variables named after
// them.

package githubrelevance

import (
	"context"
	_ "embed"
	"encoding/json"
	"net/http"
	"sync"
	"sync/atomic"
)

type runtimeManifest struct {
	Version   string                     `json:"version"`
	Platforms map[string]runtimePlatform `json:"platforms"`
}

func (m *runtimeManifest) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "version", "platforms"); err != nil {
		return err
	}
	type wire runtimeManifest
	return json.Unmarshal(data, (*wire)(m))
}

type runtimePlatform struct {
	Library  string           `json:"library"`
	Archives []runtimeArchive `json:"archives"`
}

func (p *runtimePlatform) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "library", "archives"); err != nil {
		return err
	}
	type wire runtimePlatform
	return json.Unmarshal(data, (*wire)(p))
}

type runtimeArchive struct {
	URL    string        `json:"url"`
	Bytes  uint64        `json:"bytes"`
	SHA256 string        `json:"sha256"`
	Files  []runtimeFile `json:"files"`
}

func (a *runtimeArchive) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "url", "bytes", "sha256", "files"); err != nil {
		return err
	}
	type wire runtimeArchive
	return json.Unmarshal(data, (*wire)(a))
}

type runtimeFile struct {
	Source string `json:"source"`
	Name   string `json:"name"`
	Bytes  uint64 `json:"bytes"`
	SHA256 string `json:"sha256"`
}

func (f *runtimeFile) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "source", "name", "bytes", "sha256"); err != nil {
		return err
	}
	type wire runtimeFile
	return json.Unmarshal(data, (*wire)(f))
}

//go:embed data/github_student/runtime.json
var runtimeManifestJSON []byte

var pinnedRuntimeManifestValue = sync.OnceValue(func() *runtimeManifest {
	var pinned runtimeManifest
	if err := json.Unmarshal(runtimeManifestJSON, &pinned); err != nil {
		panic("checked-in runtime manifest: " + err.Error())
	}
	return &pinned
})

// pinnedRuntimeManifest is runtime.rs's manifest(): the checked-in runtime
// manifest.
func pinnedRuntimeManifest() *runtimeManifest {
	return pinnedRuntimeManifestValue()
}

// target is the runtime manifest's platform key for this host, or a
// preparation failure announcing PlatformUnsupported.
func target() (string, error) {
	panic("unported: mj-github-relevance/src/github_student/runtime.rs target")
}

func validatePlatform() error {
	_, err := target()
	return err
}

// files is the extracted files of every archive of platform, as the
// artifacts its published cache must hold.
func files(platform *runtimePlatform) []artifact {
	var artifacts []artifact
	for _, archive := range platform.Archives {
		for _, file := range archive.Files {
			artifacts = append(artifacts, artifact{Name: file.Name, Bytes: file.Bytes, SHA256: file.SHA256})
		}
	}
	return artifacts
}

type selection struct {
	target   string
	missing  []string
	cuda     bool
	directml bool
}

// runtimeSelection is the SELECTION OnceLock: the native-library choice,
// made once per process before ORT loads (set with CompareAndSwap from nil).
var runtimeSelection atomic.Pointer[selection]

func directmlSelected() bool {
	selected := runtimeSelection.Load()
	return selected != nil && selected.directml
}

func gpuRequiresRestart() bool {
	panic("unported: mj-github-relevance/src/github_student/runtime.rs gpu_requires_restart")
}

func cudaSelected() bool {
	selected := runtimeSelection.Load()
	return selected != nil && selected.cuda
}

// selectRuntime is runtime.rs's select (a Go keyword): the native library
// for this host, probing CUDA's libraries on x86-64 Linux and DirectX 12 on
// Windows when useGPU asks for a GPU.
func selectRuntime(useGPU bool) (selection, error) {
	panic("unported: mj-github-relevance/src/github_student/runtime.rs select")
}

// directx12Available reports whether D3D12CreateDevice accepts the default
// adapter at feature level 11.0, DirectML's documented minimum. Rust
// defines it only on Windows; elsewhere it is false.
func directx12Available() bool {
	panic("unported: mj-github-relevance/src/github_student/runtime.rs directx12_available")
}

// prepareSelected chooses the native library once per process and returns
// the path of its verified copy under root.
func prepareSelected(ctx context.Context, root string, useGPU bool, notice Notice, client *http.Client) (string, error) {
	panic("unported: mj-github-relevance/src/github_student/runtime.rs prepare_selected")
}

// cacheDirectory includes the entire pin inventory in the key: DirectML's
// independent revision can change without overwriting a DLL used by another
// daemon.
func cacheDirectory(root, target string) string {
	panic("unported: mj-github-relevance/src/github_student/runtime.rs cache_directory")
}

// cachedCPULibrary is the verified CPU library already cached under root (a
// #[cfg(test)] helper in Rust).
func cachedCPULibrary(root string) (string, error) {
	panic("unported: mj-github-relevance/src/github_student/runtime.rs cached_cpu_library")
}

func preparePlatform(ctx context.Context, root, target string, platform runtimePlatform, client *http.Client) (string, error) {
	panic("unported: mj-github-relevance/src/github_student/runtime.rs prepare_platform")
}

// extract writes archive's pinned files from the downloaded archive at path
// into directory.
func extract(path string, archive *runtimeArchive, directory string) error {
	panic("unported: mj-github-relevance/src/github_student/runtime.rs extract")
}

// runtimeEnvironment is environment()'s ENVIRONMENT static. ORT permits
// exactly one environment per process, even after its last session drops,
// so this small API owner stays alive across model retries.
var runtimeEnvironment struct {
	mu    sync.Mutex
	value *ortEnvironment
}

// runtimeWindowsLibraries is environment()'s LIBRARY static on Windows: the
// verified DLLs preloaded by absolute path, kept loaded for the process.
var runtimeWindowsLibraries struct {
	mu     sync.Mutex
	loaded []uintptr
}

// environment loads the ORT library at path. Only called after every native
// payload and its notices have been verified.
func environment(path string) (*ortEnvironment, error) {
	panic("unported: mj-github-relevance/src/github_student/runtime.rs environment")
}
