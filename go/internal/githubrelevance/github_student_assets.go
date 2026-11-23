// Port of mj-github-relevance/src/github_student/assets.rs.

// Verified immutable publication shared by model and native-runtime
// preparation.

package githubrelevance

import (
	"context"
	_ "embed"
	"encoding/json"
	"log/slog"
	"net/http"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
)

// manifest is a pinned model publication: a Hugging Face repository at an
// immutable revision and the files it must contain.
type manifest struct {
	Repository string     `json:"repository"`
	Revision   string     `json:"revision"`
	Files      []artifact `json:"files"`
}

func (m *manifest) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "repository", "revision", "files"); err != nil {
		return err
	}
	type wire manifest
	return json.Unmarshal(data, (*wire)(m))
}

type artifact struct {
	Name   string `json:"name"`
	SHA256 string `json:"sha256"`
	Bytes  uint64 `json:"bytes"`
}

func (a *artifact) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "name", "sha256", "bytes"); err != nil {
		return err
	}
	type wire artifact
	return json.Unmarshal(data, (*wire)(a))
}

// clone is Rust's derived Clone: the copy has its own file list.
func (m manifest) clone() manifest {
	m.Files = slices.Clone(m.Files)
	return m
}

// selected is the publication without the CoreML graphs and initializers,
// or with them under the "-coreml" sibling revision.
func (m manifest) selected(coreml bool) manifest {
	selected := m.clone()
	selected.Files = slices.DeleteFunc(selected.Files, func(f artifact) bool {
		return !coreml && (strings.HasPrefix(f.Name, "model-coreml-") || strings.HasPrefix(f.Name, "initializers-"))
	})
	if coreml {
		selected.Revision += "-coreml"
	}
	return selected
}

//go:embed data/github_student/manifest.json
var pinnedManifestJSON []byte

var pinnedManifestValue = sync.OnceValue(func() *manifest {
	var pinned manifest
	if err := json.Unmarshal(pinnedManifestJSON, &pinned); err != nil {
		panic("checked-in model manifest: " + err.Error())
	}
	return &pinned
})

// pinnedManifest is Manifest::pinned: the checked-in model manifest.
func pinnedManifest() *manifest {
	return pinnedManifestValue()
}

func verifyFile(path string, bytes uint64, sha256 string) error {
	panic("unported: mj-github-relevance/src/github_student/assets.rs verify_file")
}

func verified(path string, files []artifact) error {
	for _, file := range files {
		if err := verifyFile(filepath.Join(path, file.Name), file.Bytes, file.SHA256); err != nil {
			return err
		}
	}
	return nil
}

// staging is a private directory beside the published revisions where one
// publication is assembled. Its cleanup also runs off the caller's
// goroutine, including after cancellation.
type staging struct {
	// directory is the staging directory; "" once publish has moved it into
	// place or drop has removed it (Rust's Option<TempDir> taken).
	directory string
}

func newStaging(root string) (*staging, error) {
	if err := os.MkdirAll(root, 0o777); err != nil {
		return nil, err
	}
	directory, err := os.MkdirTemp(root, ".student-")
	if err != nil {
		return nil, err
	}
	return &staging{directory: directory}, nil
}

func (s *staging) path() string {
	if s.directory == "" {
		panic("staging directory consumed")
	}
	return s.directory
}

// publish verifies the staged files and moves them to destination. It
// blocks; run it off the caller's event loop. One OS lock serializes the
// publication decision across daemon replacement, including a cancelled old
// preparer.
func (s *staging) publish(destination string, files []artifact) (string, error) {
	panic("unported: mj-github-relevance/src/github_student/assets.rs Staging::publish")
}

// drop is Rust's Drop for Staging: remove an unpublished staging directory
// in the background.
func (s *staging) drop() {
	if s.directory == "" {
		return
	}
	directory := s.directory
	s.directory = ""
	go func() {
		if err := os.RemoveAll(directory); err != nil {
			slog.Warn("student staging cleanup failed", "error", err.Error())
		}
	}()
}

// download fetches url into path, failing promptly when ctx ends.
func download(ctx context.Context, url, path string, client *http.Client) error {
	panic("unported: mj-github-relevance/src/github_student/assets.rs download")
}

// rootForReuse is where a file of destination's revision may already be
// verified: a "-coreml" revision reuses its base revision's files.
func rootForReuse(destination, name string) string {
	revision := filepath.Base(destination)
	if base, ok := strings.CutSuffix(revision, "-coreml"); ok {
		return filepath.Join(filepath.Dir(destination), base, name)
	}
	return filepath.Join(destination, name)
}

// prepare returns the verified publication of manifest under root,
// downloading what is missing from base. Only the owner calls preparation;
// pair classification observes Loading.
func prepare(ctx context.Context, root string, manifest manifest, base string, client *http.Client) (string, error) {
	panic("unported: mj-github-relevance/src/github_student/assets.rs prepare")
}
