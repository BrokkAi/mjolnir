// Port of mj-github-relevance/src/github_student/assets.rs.

// Verified immutable publication shared by model and native-runtime
// preparation.

package githubrelevance

import (
	"context"
	"crypto/sha256"
	_ "embed"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
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

func verifyFile(path string, bytes uint64, sha256Hex string) error {
	input, err := os.Open(path)
	if err != nil {
		return err
	}
	defer input.Close()
	info, err := input.Stat()
	if err != nil {
		return err
	}
	if uint64(info.Size()) != bytes {
		return fmt.Errorf("%s: wrong size", path)
	}
	hasher := sha256.New()
	buffer := make([]byte, 65536)
	for {
		n, err := input.Read(buffer)
		hasher.Write(buffer[:n])
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return err
		}
	}
	if core.LowerHex(hasher.Sum(nil)) != sha256Hex {
		return fmt.Errorf("%s: wrong SHA-256", path)
	}
	return nil
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
	if err := verified(s.path(), files); err != nil {
		return "", preparationFailureError(StudentFailureKindVerification, err, nil)
	}
	root := filepath.Dir(destination)
	if root == destination {
		return "", errors.New("cache destination has no parent")
	}
	file, err := os.OpenFile(filepath.Join(root, ".publication.lock"), os.O_WRONLY|os.O_CREATE, 0o666)
	if err != nil {
		return "", err
	}
	defer file.Close()
	if err := lockPublication(file); err != nil {
		return "", err
	}
	lock := core.NewFileLock(file)
	defer lock.Drop()
	if verified(destination, files) == nil {
		return destination, nil
	}
	if _, err := os.Lstat(destination); err == nil {
		if err := os.RemoveAll(destination); err != nil {
			return "", err
		}
	}
	if err := os.Rename(s.path(), destination); err != nil {
		return "", err
	}
	s.directory = ""
	return destination, nil
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
	cancelled := errors.New("student preparation cancelled")
	if ctx.Err() != nil {
		return cancelled
	}
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return err
	}
	response, err := client.Do(request)
	if err != nil {
		if ctx.Err() != nil {
			return cancelled
		}
		return err
	}
	defer response.Body.Close()
	var retryAfter *time.Duration
	if delay, ok := core.RetryAfterHeader(response.Header); ok {
		retryAfter = &delay
	}
	if err := errorForStatus(response); err != nil {
		return preparationFailureError(StudentFailureKindDownload, err, retryAfter)
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o777); err != nil {
		return err
	}
	output, err := os.Create(path)
	if err != nil {
		return err
	}
	defer output.Close()
	if _, err := io.Copy(output, response.Body); err != nil {
		if ctx.Err() != nil {
			return cancelled
		}
		return err
	}
	if err := output.Sync(); err != nil {
		return err
	}
	return output.Close()
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
	destination := filepath.Join(root, manifest.Revision)
	if verified(destination, manifest.Files) == nil {
		return destination, nil
	}
	staging, err := newStaging(root)
	if err != nil {
		return "", err
	}
	defer staging.drop()
	fetches := make([]func(ctx context.Context) error, len(manifest.Files))
	for i, artifact := range manifest.Files {
		url := fmt.Sprintf("%s/%s/resolve/%s/%s", base, manifest.Repository, trimAllSuffixes(manifest.Revision, "-coreml"), artifact.Name)
		path := filepath.Join(staging.path(), artifact.Name)
		existing := rootForReuse(destination, artifact.Name)
		fetches[i] = func(ctx context.Context) error {
			copied := func() error {
				if err := verifyFile(existing, artifact.Bytes, artifact.SHA256); err != nil {
					return err
				}
				if err := os.MkdirAll(filepath.Dir(path), 0o777); err != nil {
					return err
				}
				return copyFile(existing, path)
			}()
			if copied == nil {
				return nil
			}
			return download(ctx, url, path, client)
		}
	}
	if err := tryJoinAll(ctx, fetches); err != nil {
		return "", err
	}
	return staging.publish(destination, manifest.Files)
}

// trimAllSuffixes is str::trim_end_matches: every trailing repetition of
// suffix removed.
func trimAllSuffixes(text, suffix string) string {
	for suffix != "" && strings.HasSuffix(text, suffix) {
		text = strings.TrimSuffix(text, suffix)
	}
	return text
}

// copyFile is fs::copy: the contents of from written over to, with from's
// permissions.
func copyFile(from, to string) error {
	input, err := os.Open(from)
	if err != nil {
		return err
	}
	defer input.Close()
	info, err := input.Stat()
	if err != nil {
		return err
	}
	output, err := os.OpenFile(to, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, info.Mode().Perm())
	if err != nil {
		return err
	}
	if _, err := io.Copy(output, input); err != nil {
		output.Close()
		return err
	}
	return output.Close()
}

// tryJoinAll is futures' try_join_all over work started together: the
// first error is returned once every other piece has stopped, cancelled by
// that error as dropping their futures cancels them in Rust.
func tryJoinAll(ctx context.Context, work []func(ctx context.Context) error) error {
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()
	var (
		wait  sync.WaitGroup
		once  sync.Once
		first error
	)
	for _, piece := range work {
		wait.Go(func() {
			if err := piece(ctx); err != nil {
				once.Do(func() {
					first = err
					cancel()
				})
			}
		})
	}
	wait.Wait()
	return first
}
