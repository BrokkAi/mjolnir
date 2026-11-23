// Port of the tests in mj-github-relevance/src/github_student/runtime.rs.

package githubrelevance

import (
	"context"
	"net/http"
	"os"
	"path/filepath"
	"slices"
	"sort"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

func TestNativeArchivesPublishOnlyVerifiedLibrariesAndNotices(t *testing.T) {
	skipUnlessIgnoredTestsRequested(t, "downloads the official pinned native archives for all supported platforms")
	targets.InstallRustlsCryptoProvider()
	root, err := os.MkdirTemp("", "github-student-native-integration-")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(root) })
	client := &http.Client{}
	targetNames := make([]string, 0, len(pinnedRuntimeManifest().Platforms))
	for target := range pinnedRuntimeManifest().Platforms {
		targetNames = append(targetNames, target)
	}
	sort.Strings(targetNames)
	for _, target := range targetNames {
		platform := pinnedRuntimeManifest().Platforms[target]
		library, err := preparePlatform(context.Background(), root, target, platform, client)
		if err != nil {
			t.Fatalf("%s: %v", target, err)
		}
		directory := filepath.Dir(library)
		if err := verified(directory, files(&platform)); err != nil {
			t.Fatal(err)
		}
		entries, err := os.ReadDir(directory)
		if err != nil {
			t.Fatal(err)
		}
		var actual, expected []string
		for _, entry := range entries {
			actual = append(actual, entry.Name())
		}
		for _, file := range files(&platform) {
			expected = append(expected, file.Name)
		}
		slices.Sort(actual)
		slices.Sort(expected)
		expected = slices.Compact(expected)
		if !slices.Equal(actual, expected) {
			t.Fatalf("%s: downloaded archives and unselected payloads must not remain in the published cache: %v, want %v", target, actual, expected)
		}
	}
}
