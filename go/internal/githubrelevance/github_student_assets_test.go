// Port of the tests in mj-github-relevance/src/github_student/assets.rs.

package githubrelevance

import (
	"context"
	"crypto/sha256"
	"errors"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strconv"
	"sync"
	"sync/atomic"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

type assetSource struct {
	bytes     []byte
	interrupt atomic.Bool
	corrupt   atomic.Bool
	requests  atomic.Int64
}

// startAssetServer serves source at every path.
func startAssetServer(t *testing.T, source *assetSource) *httptest.Server {
	t.Helper()
	server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, _ *http.Request) {
		source.requests.Add(1)
		writer.Header().Set("Content-Length", strconv.Itoa(len(source.bytes)))
		if source.interrupt.Swap(false) {
			// Cross the 64KiB boundary before the peer aborts the body.
			writer.Write(source.bytes[:100_000])
			writer.(http.Flusher).Flush()
			panic(http.ErrAbortHandler)
		}
		bytes := append([]byte(nil), source.bytes...)
		if source.corrupt.Load() {
			bytes[0] ^= 1
		}
		writer.Write(bytes)
	}))
	t.Cleanup(server.Close)
	return server
}

func newAssetSource() *assetSource {
	bytes := make([]byte, 300_000)
	for i := range bytes {
		bytes[i] = byte(i % 251)
	}
	return &assetSource{bytes: bytes}
}

func assetManifest(source *assetSource) manifest {
	digest := sha256.Sum256(source.bytes)
	return manifest{
		Repository: "fixture",
		Revision:   "immutable",
		Files:      []artifact{{Name: "model", Bytes: uint64(len(source.bytes)), SHA256: core.LowerHex(digest[:])}},
	}
}

func TestInterruptedDownloadNeverPublishesAndRestartRecovers(t *testing.T) {
	targets.InstallRustlsCryptoProvider()
	source := newAssetSource()
	source.interrupt.Store(true)
	server := startAssetServer(t, source)
	root := t.TempDir()
	client := &http.Client{}
	ctx := context.Background()
	nested := assetManifest(source)
	nested.Files[0].Name = "onnx/fp32/model.onnx"
	if _, err := prepare(ctx, root, nested.clone(), server.URL, client); err == nil {
		t.Fatal("an interrupted download must fail")
	}
	if _, err := os.Stat(filepath.Join(root, "immutable")); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("an interrupted download was published: %v", err)
	}
	path, err := prepare(ctx, root, nested.clone(), server.URL, client)
	if err != nil {
		t.Fatal(err)
	}
	if err := verified(path, nested.Files); err != nil {
		t.Fatal(err)
	}
	if got := source.requests.Load(); got != 2 {
		t.Fatalf("%d requests", got)
	}
	// A CoreML sibling revision reuses nested model files from the base
	// revision, creating its staging parents without another HTTP fetch.
	sibling := nested.selected(true)
	reused, err := prepare(ctx, root, sibling.clone(), server.URL, client)
	if err != nil {
		t.Fatal(err)
	}
	if err := verified(reused, sibling.Files); err != nil {
		t.Fatal(err)
	}
	if got := source.requests.Load(); got != 2 {
		t.Fatalf("%d requests", got)
	}
}

func TestBadDigestIsAVerificationFailureAndNotACacheEntry(t *testing.T) {
	targets.InstallRustlsCryptoProvider()
	source := newAssetSource()
	source.corrupt.Store(true)
	server := startAssetServer(t, source)
	root := t.TempDir()
	client := &http.Client{}
	_, err := prepare(context.Background(), root, assetManifest(source), server.URL, client)
	var failure *preparationFailure
	if !errors.As(err, &failure) || failure.kind != StudentFailureKindVerification {
		t.Fatalf("error %v", err)
	}
	if _, err := os.Stat(filepath.Join(root, "immutable")); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("a corrupt download was published: %v", err)
	}
	source.corrupt.Store(false)
	path, err := prepare(context.Background(), root, assetManifest(source), server.URL, client)
	if err != nil {
		t.Fatal(err)
	}
	if err := verified(path, assetManifest(source).Files); err != nil {
		t.Fatal(err)
	}
}

func TestPublicationSerializesOldAndNewPreparersAcrossHandoff(t *testing.T) {
	source := newAssetSource()
	root := t.TempDir()
	files := assetManifest(source).Files
	destination := filepath.Join(root, "immutable")
	// This bypasses the owner deliberately to simulate two daemon epochs.
	first, err := newStaging(root)
	if err != nil {
		t.Fatal(err)
	}
	second, err := newStaging(root)
	if err != nil {
		t.Fatal(err)
	}
	for _, staged := range []*staging{first, second} {
		if err := os.WriteFile(filepath.Join(staged.path(), "model"), source.bytes, 0o666); err != nil {
			t.Fatal(err)
		}
	}
	var wait sync.WaitGroup
	errs := make([]error, 2)
	for i, staged := range []*staging{first, second} {
		wait.Go(func() {
			defer staged.drop()
			_, errs[i] = staged.publish(destination, files)
		})
	}
	wait.Wait()
	for _, err := range errs {
		if err != nil {
			t.Fatal(err)
		}
	}
	if err := verified(destination, files); err != nil {
		t.Fatal(err)
	}
}
