// Port of mj-harness-install/src/session_image.rs.

// Deterministic project-image recipes shared by the daemon and CLI output.

package harnessinstall

import (
	"bytes"
	"crypto/sha256"
	_ "embed"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"slices"
	"sort"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
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
	var manifest map[string]any
	if err := json.Unmarshal(miseRuntimeJSON, &manifest); err != nil {
		return MiseArtifact{}, err
	}
	version, _ := manifest["version"].(string)
	if version != core.MiseVersion {
		return MiseArtifact{}, errors.New("mise download metadata does not match the managed runtime pin")
	}
	platforms, _ := manifest["platforms"].(map[string]any)
	platform, _ := platforms["linux-"+arch].(map[string]any)
	field := func(name string) (string, error) {
		if value, ok := platform[name].(string); ok {
			return value, nil
		}
		return "", fmt.Errorf("mise has no pinned %s for linux-%s", name, arch)
	}
	url, err := field("url")
	if err != nil {
		return MiseArtifact{}, err
	}
	sha, err := field("sha256")
	if err != nil {
		return MiseArtifact{}, err
	}
	return MiseArtifact{Version: version, URL: url, Sha256: sha}, nil
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
	return core.LowerHex(digest[:]), nil
}

// Containerfile is the complete build input: no source checkout or build
// context is required.
func Containerfile(base *ImageBase, platform string, mise *MiseArtifact, files map[string]string) (string, error) {
	for _, value := range []string{base.Reference, base.User, base.Workdir, platform, mise.URL, mise.Version} {
		if strings.ContainsAny(value, "\r\n") {
			return "", errors.New("image metadata contains a newline")
		}
	}
	if len(mise.Sha256) != 64 || !isASCIIHexText(mise.Sha256) {
		return "", errors.New("invalid mise SHA256")
	}
	if !strings.Contains(base.Reference, "@sha256:") {
		return "", errors.New("base image must be pinned by digest")
	}
	recipe, err := RecipeHash(files)
	if err != nil {
		return "", err
	}
	var text strings.Builder
	fmt.Fprintf(&text, "FROM %s\nUSER root\nADD --checksum=sha256:%s %s /usr/local/bin/mise\nRUN chmod 0755 /usr/local/bin/mise\nENV MISE_DATA_DIR=/opt/mise \\\n    RUSTUP_HOME=/opt/rustup \\\n    CARGO_HOME=/opt/cargo \\\n    MISE_TRUSTED_CONFIG_PATHS=%s \\\n    MISE_IDIOMATIC_VERSION_FILE_ENABLE_TOOLS=rust,node,python,ruby,go \\\n    PATH=/opt/mise/shims:$PATH\nWORKDIR %s\n",
		base.Reference, mise.Sha256, mise.URL, ImageRecipeDir, ImageRecipeDir)
	paths := make([]string, 0, len(files))
	for path := range files {
		paths = append(paths, path)
	}
	sort.Strings(paths)
	for _, path := range paths {
		contents := files[path]
		if !slices.Contains(RecipeFiles, path) {
			return "", fmt.Errorf("unsupported recipe file %s", debugString(path))
		}
		parent := "."
		if index := strings.LastIndex(path, "/"); index > 0 {
			parent = path[:index]
		}
		// Linux limits each exec argument to 32 pages (MAX_ARG_STRLEN in
		// include/uapi/linux/binfmts.h). 48 KiB encodes to 64 KiB, leaving
		// room for the shell program even with 4 KiB pages. This chunks the
		// transfer rather than limiting the recipe's total size.
		const chunkSize = 48 * 1024
		var chunks [][]byte
		data := []byte(contents)
		for len(data) > 0 {
			n := min(chunkSize, len(data))
			chunks = append(chunks, data[:n])
			data = data[n:]
		}
		if len(chunks) == 0 {
			chunks = [][]byte{{}}
		}
		for index, chunk := range chunks {
			encoded := base64.StdEncoding.EncodeToString(chunk)
			redirect := ">>"
			if index == 0 {
				redirect = ">"
			}
			fmt.Fprintf(&text, "RUN mkdir -p '%s' && printf '%%s' '%s' | base64 -d %s '%s' && chmod 0644 '%s'\n",
				parent, encoded, redirect, path, path)
		}
	}
	// Sessions on a non-root base run as that user and install recipe changes
	// into these directories. Chown in the install layer: a later layer would
	// copy every installed file up again.
	user, _, _ := strings.Cut(base.User, ":")
	chown := ""
	if user != "" && user != "root" && user != "0" {
		for _, b := range []byte(base.User) {
			if !isASCIIAlphanumeric(b) && strings.IndexByte("._:-", b) < 0 {
				return "", fmt.Errorf("unsupported base image user %s", debugString(base.User))
			}
		}
		chown = fmt.Sprintf(" && mkdir -p /opt/mise /opt/rustup /opt/cargo && chown -R %s /opt/mise /opt/rustup /opt/cargo", base.User)
	}
	fmt.Fprintf(&text, "RUN mise bootstrap packages apply --yes && mise install%s\n", chown)
	fmt.Fprintf(&text, "LABEL dev.mj.image.recipe=\"%s\" \\\n    dev.mj.image.mise=%s \\\n    dev.mj.image.platform=%s\n",
		recipe, harnessJSONString(mise.Version), harnessJSONString(platform))
	userLine := base.User
	if userLine == "" {
		userLine = "root"
	}
	workdir := base.Workdir
	if workdir == "" {
		workdir = "/"
	}
	fmt.Fprintf(&text, "USER %s\nWORKDIR %s\n", userLine, harnessJSONString(workdir))
	return text.String(), nil
}

func ImageTag(containerfile string) string {
	digest := sha256.Sum256([]byte(containerfile))
	return core.LowerHex(digest[:])[:16]
}

// harnessMarshalJSON encodes v as serde_json does: without HTML escapes and
// without a trailing newline.
func harnessMarshalJSON(v any) ([]byte, error) {
	var body bytes.Buffer
	encoder := json.NewEncoder(&body)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(v); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(body.Bytes(), []byte("\n")), nil
}

// harnessJSONString is serde_json::to_string of a string.
func harnessJSONString(value string) string {
	encoded, _ := harnessMarshalJSON(value)
	return string(encoded)
}

func isASCIIHexText(value string) bool {
	for _, b := range []byte(value) {
		if !(b >= '0' && b <= '9' || b >= 'a' && b <= 'f' || b >= 'A' && b <= 'F') {
			return false
		}
	}
	return true
}
