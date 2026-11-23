// Port of mj-github-relevance/src/github_student/runtime.rs.

// One native-runtime mechanism on every install: official pinned downloads,
// verified and atomically cached by the same preparation owner as the model.
//
// The private manifest types carry a `runtime` prefix: Manifest collides with
// assets.rs's, and the others would shadow the local variables named after
// them.

package githubrelevance

import (
	"archive/tar"
	"archive/zip"
	"compress/gzip"
	"context"
	"crypto/sha256"
	_ "embed"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"net/http"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"sync"
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/internal/core"
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

// rustOS and rustArch are std::env::consts::OS and ARCH for this host, as
// the unsupported-platform message names them.
func rustOS() string {
	if runtime.GOOS == "darwin" {
		return "macos"
	}
	return runtime.GOOS
}

func rustArch() string {
	switch runtime.GOARCH {
	case "amd64":
		return "x86_64"
	case "arm64":
		return "aarch64"
	case "386":
		return "x86"
	case "ppc64", "ppc64le":
		return "powerpc64"
	case "loong64":
		return "loongarch64"
	case "mips64", "mips64le":
		return "mips64"
	case "mipsle":
		return "mips"
	}
	return runtime.GOARCH
}

// target is the runtime manifest's platform key for this host, or a
// preparation failure announcing PlatformUnsupported.
func target() (string, error) {
	switch os, arch := rustOS(), rustArch(); {
	case os == "linux" && arch == "x86_64":
		return "x86_64-unknown-linux-gnu", nil
	case os == "linux" && arch == "aarch64":
		return "aarch64-unknown-linux-gnu", nil
	case os == "macos" && arch == "x86_64":
		return "x86_64-apple-darwin", nil
	case os == "macos" && arch == "aarch64":
		return "aarch64-apple-darwin", nil
	case os == "windows" && arch == "x86_64":
		return "x86_64-pc-windows-msvc", nil
	default:
		return "", &preparationFailure{
			kind:   StudentFailureKindLoad,
			cause:  fmt.Sprintf("local GitHub scoring is unavailable on %s/%s", os, arch),
			notice: StudentNoticePlatformUnsupported{},
		}
	}
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
	selected := runtimeSelection.Load()
	return selected != nil &&
		((runtime.GOOS == "windows" && !selected.directml) ||
			(runtime.GOOS == "linux" && runtime.GOARCH == "amd64" && !selected.cuda))
}

func cudaSelected() bool {
	selected := runtimeSelection.Load()
	return selected != nil && selected.cuda
}

// selectRuntime is runtime.rs's select (a Go keyword): the native library
// for this host, probing CUDA's libraries on x86-64 Linux and DirectX 12 on
// Windows when useGPU asks for a GPU. Only those probes change the
// selection.
func selectRuntime(useGPU bool) (selection, error) {
	target, err := target()
	if err != nil {
		return selection{}, err
	}
	selected := selection{target: target}
	if useGPU {
		if missing, driver := probeCUDA(); driver {
			selected.missing = missing
			if len(missing) == 0 {
				selected.target += "-cuda"
				selected.cuda = true
			}
		}
		if runtime.GOOS == "windows" && directx12Available() {
			selected.target += "-directml"
			selected.directml = true
		}
	}
	return selected, nil
}

// prepareSelected chooses the native library once per process and returns
// the path of its verified copy under root. The owner chooses before
// loading ORT; a later preference change cannot replace a process-global
// native library underneath live sessions.
func prepareSelected(ctx context.Context, root string, useGPU bool, notice Notice, client *http.Client) (string, error) {
	if runtimeSelection.Load() == nil {
		choice, err := selectRuntime(useGPU)
		if err != nil {
			return "", err
		}
		if runtimeSelection.CompareAndSwap(nil, &choice) && len(choice.missing) > 0 {
			notice(StudentNoticeCUDALibrariesMissing{Missing: slices.Clone(choice.missing)})
		}
	}
	selected := runtimeSelection.Load()
	if selected == nil {
		return "", errors.New("runtime selection missing")
	}
	platform, ok := pinnedRuntimeManifest().Platforms[selected.target]
	if !ok {
		return "", errors.New("unsupported runtime target")
	}
	return preparePlatform(ctx, root, selected.target, platform, client)
}

// cacheDirectory includes the entire pin inventory in the key: DirectML's
// independent revision can change without overwriting a DLL used by another
// daemon.
func cacheDirectory(root, target string) string {
	digest := sha256.Sum256(runtimeManifestJSON)
	return filepath.Join(root, fmt.Sprintf("%s-%s-%s", pinnedRuntimeManifest().Version, target, core.LowerHex(digest[:])))
}

// cachedCPULibrary is the verified CPU library already cached under root (a
// #[cfg(test)] helper in Rust).
func cachedCPULibrary(root string) (string, error) {
	target, err := target()
	if err != nil {
		return "", err
	}
	platform, ok := pinnedRuntimeManifest().Platforms[target]
	if !ok {
		return "", errors.New("unsupported runtime target")
	}
	choice, err := selectRuntime(false)
	if err != nil {
		return "", err
	}
	runtimeSelection.CompareAndSwap(nil, &choice)
	directory := cacheDirectory(root, target)
	if err := verified(directory, files(&platform)); err != nil {
		return "", err
	}
	return canonicalize(filepath.Join(directory, platform.Library))
}

// canonicalize is fs::canonicalize: the absolute path with every symbolic
// link resolved.
func canonicalize(path string) (string, error) {
	absolute, err := filepath.Abs(path)
	if err != nil {
		return "", err
	}
	return filepath.EvalSymlinks(absolute)
}

// resolveVerifiedRuntime is the verified library's canonical path.
func resolveVerifiedRuntime(directory, library string) (string, error) {
	path, err := canonicalize(filepath.Join(directory, library))
	if err != nil {
		return "", fmt.Errorf("resolve verified runtime path: %w", err)
	}
	return path, nil
}

func preparePlatform(ctx context.Context, root, target string, platform runtimePlatform, client *http.Client) (string, error) {
	destination := cacheDirectory(root, target)
	if verified(destination, files(&platform)) == nil {
		return resolveVerifiedRuntime(destination, platform.Library)
	}
	staging, err := newStaging(root)
	if err != nil {
		return "", err
	}
	defer staging.drop()
	downloads := make([]func(ctx context.Context) error, len(platform.Archives))
	for i, archive := range platform.Archives {
		path := filepath.Join(staging.path(), fmt.Sprintf("download-%d", i))
		downloads[i] = func(ctx context.Context) error { return download(ctx, archive.URL, path, client) }
	}
	if err := tryJoinAll(ctx, downloads); err != nil {
		return "", err
	}
	for i := range platform.Archives {
		archive := &platform.Archives[i]
		path := filepath.Join(staging.path(), fmt.Sprintf("download-%d", i))
		if err := verifyFile(path, archive.Bytes, archive.SHA256); err != nil {
			return "", preparationFailureError(StudentFailureKindVerification, err, nil)
		}
		if err := extract(path, archive, staging.path()); err != nil {
			return "", preparationFailureError(StudentFailureKindVerification, err, nil)
		}
		if err := os.Remove(path); err != nil {
			return "", err
		}
	}
	directory, err := staging.publish(destination, files(&platform))
	if err != nil {
		return "", err
	}
	return resolveVerifiedRuntime(directory, platform.Library)
}

// extract writes archive's pinned files from the downloaded archive at path
// into directory.
func extract(path string, archive *runtimeArchive, directory string) error {
	write := func(file *runtimeFile, reader io.Reader) error {
		output, err := os.OpenFile(filepath.Join(directory, file.Name), os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o666)
		if err != nil {
			return err
		}
		defer output.Close()
		if _, err := io.Copy(output, reader); err != nil {
			return err
		}
		if err := output.Sync(); err != nil {
			return err
		}
		return output.Close()
	}
	if strings.HasSuffix(archive.URL, ".nupkg") || strings.HasSuffix(archive.URL, ".zip") {
		reader, err := zip.OpenReader(path)
		if err != nil {
			return err
		}
		defer reader.Close()
		for i := range archive.Files {
			file := &archive.Files[i]
			index := slices.IndexFunc(reader.File, func(entry *zip.File) bool { return entry.Name == file.Source })
			if index < 0 {
				return errors.New("specified file not found in archive")
			}
			entry := reader.File[index]
			mode := entry.Mode()
			if strings.HasSuffix(entry.Name, "/") || mode.IsDir() || mode&fs.ModeSymlink != 0 {
				return fmt.Errorf("runtime entry is not a regular file: %s", file.Source)
			}
			contents, err := entry.Open()
			if err != nil {
				return err
			}
			err = write(file, contents)
			contents.Close()
			if err != nil {
				return err
			}
		}
		return nil
	}
	input, err := os.Open(path)
	if err != nil {
		return err
	}
	defer input.Close()
	decompressed, err := gzip.NewReader(input)
	if err != nil {
		return err
	}
	entries := tar.NewReader(decompressed)
	extracted := 0
	for {
		header, err := entries.Next()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return err
		}
		index := slices.IndexFunc(archive.Files, func(file runtimeFile) bool {
			return slices.Equal(pathComponents(header.Name), pathComponents(file.Source))
		})
		if index < 0 {
			continue
		}
		file := &archive.Files[index]
		if header.Typeflag != tar.TypeReg && header.Typeflag != '\x00' {
			return fmt.Errorf("runtime entry is not a regular file: %s", file.Source)
		}
		if err := write(file, entries); err != nil {
			return err
		}
		extracted++
	}
	if extracted != len(archive.Files) {
		return errors.New("runtime archive is missing pinned files")
	}
	return nil
}

// pathComponents is a slash-separated path as Rust's Path compares it:
// repeated separators and interior "." components do not count.
func pathComponents(path string) []string {
	var components []string
	if strings.HasPrefix(path, "/") {
		components = append(components, "/")
	}
	for i, component := range strings.Split(path, "/") {
		if component == "" || (component == "." && (i > 0 || len(components) > 0)) {
			continue
		}
		components = append(components, component)
	}
	return components
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
	if err := checkWindowsRuntime(); err != nil {
		return nil, err
	}
	runtimeEnvironment.mu.Lock()
	defer runtimeEnvironment.mu.Unlock()
	if runtimeEnvironment.value != nil {
		return runtimeEnvironment.value, nil
	}
	if err := preloadWindowsLibraries(path); err != nil {
		return nil, err
	}
	loaded, err := loadOrtEnvironment(path)
	if err != nil {
		return nil, err
	}
	runtimeEnvironment.value = loaded
	return loaded, nil
}
