// Port of mj-core/src/session_image.rs.

// Deterministic project-image recipes shared by the daemon and CLI output.

package core

import (
	"bytes"
	"crypto/sha256"
	_ "embed"
	"encoding/json"
)

// RecipeFiles is the files mise reads at the repository root: the one list
// both the worker's post-clone step (which checkouts have a recipe) and the
// image generator (which files an image is built from) use. `go.mod` is
// deliberately absent: it changes with every dependency update, which would
// rebuild the image. Ordering is fixed by the map.
var RecipeFiles = []string{
	"mise.toml",
	".mise.toml",
	"mise/config.toml",
	".config/mise.toml",
	".tool-versions",
	"mise.lock",
	"rust-toolchain",
	"rust-toolchain.toml",
	".nvmrc",
	".node-version",
	".python-version",
	".ruby-version",
	".go-version",
}

// ImageRecipeDir is where a built image keeps the recipe files it was built
// from. The worker compares a checkout's recipe with this copy to know the
// image ran the recipe's postinstall hook.
const ImageRecipeDir = "/opt/mj-image/recipe"

//go:embed assets/mise/runtime.json
var miseRuntimeJSON []byte

type MiseArtifact struct {
	Version string `json:"version"`
	URL     string `json:"url"`
	Sha256  string `json:"sha256"`
}

// PinnedMise is the pinned static mise download for a Linux CPU
// architecture (Rust's `std::env::consts::ARCH` spelling: `x86_64`,
// `aarch64`), read from `assets/mise/runtime.json`. The worker installs it
// into containers and the image generator adds it to built images, so both
// use this one reader.
func PinnedMise(arch string) (MiseArtifact, error) {
	panic("unported: mj-core/src/session_image.rs pinned_mise")
}

type ImageBase struct {
	// Reference is a platform-specific, immutable reference
	// (repository@sha256:digest).
	Reference string
	User      string
	Workdir   string
}

func RecipeHash(files map[string]string) (string, error) {
	// serde_json writes a BTreeMap with sorted keys and without HTML escapes.
	var body bytes.Buffer
	encoder := json.NewEncoder(&body)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(files); err != nil {
		return "", err
	}
	digest := sha256.Sum256(bytes.TrimSuffix(body.Bytes(), []byte("\n")))
	return LowerHex(digest[:]), nil
}

// Containerfile is the complete build input: no source checkout or build
// context is required.
func Containerfile(base *ImageBase, platform string, mise *MiseArtifact, files map[string]string) (string, error) {
	panic("unported: mj-core/src/session_image.rs containerfile")
}

func ImageTag(containerfile string) string {
	digest := sha256.Sum256([]byte(containerfile))
	return LowerHex(digest[:])[:16]
}
