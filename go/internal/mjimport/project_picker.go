// Port of mj-import/src/project_picker.rs.

// Repository discovery shared by the terminal and authenticated web picker.
//
// Callers run this blocking service on a supervised task and supply a
// cancellable executor with a deadline. No credentials or subprocess output
// are attached to errors, so the same actionable messages suit either UI.
//
// Rust re-exports mj_core::project_picker's ProjectDiscovery,
// ProjectDiscoveryRequest, ProjectEntry and ProjectEntryKind here; Go
// callers use them from package core.

package mjimport

import (
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"syscall"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const (
	maxEntries    = 100
	maxInputBytes = 4096
)

// discoveryError is one of the fixed, intentionally public sentences.
type discoveryError struct {
	message string
}

func (e *discoveryError) Error() string { return e.message }

// ErrorMessage: only intentionally public sentences cross the HTTP
// boundary, even if a future implementation adds an error from another
// source.
func ErrorMessage(err error) string {
	var discovery *discoveryError
	if errors.As(err, &discovery) {
		return discovery.message
	}
	return "Project discovery failed. Retry the request."
}

// ValidateRequest rejects an input too long or containing NUL before any
// work.
func ValidateRequest(request core.ProjectDiscoveryRequest) error {
	panic("unported: mj-import/src/project_picker.rs validate_request")
}

// Discover browses immediate subdirectories or the first page of
// authenticated GitHub results. Directory sources are absolute paths;
// GitHub sources are owner/repo.
func Discover(request core.ProjectDiscoveryRequest, executor core.CommandExecutor) (core.ProjectDiscovery, error) {
	panic("unported: mj-import/src/project_picker.rs discover")
}

func checkCancelled(executor core.CommandExecutor) error {
	if executor.CancellationRequested() {
		return &discoveryError{"Project discovery was cancelled or timed out; retry the request."}
	}
	return nil
}

// directoryError picks the sentence for a filesystem error by its kind.
func directoryError(err error) *discoveryError {
	switch {
	case errors.Is(err, fs.ErrNotExist):
		return &discoveryError{"That directory no longer exists. Choose another directory."}
	case errors.Is(err, syscall.ENOTDIR):
		return &discoveryError{"That path is not a directory. Choose a folder to browse."}
	case errors.Is(err, fs.ErrPermission):
		return &discoveryError{"The directory cannot be read. Check its permissions or choose another directory."}
	}
	return &discoveryError{"The directory could not be read. Check the path and filesystem, then retry."}
}

// pathText is path as text, refused when it is not valid Unicode.
func pathText(path string) (string, error) {
	if !utf8.ValidString(path) {
		return "", &discoveryError{"This directory contains a path that is not valid Unicode and cannot be shown in the picker."}
	}
	return path, nil
}

// isRepository is whether path has a `.git` directory or file.
func isRepository(path string) (bool, error) {
	info, err := os.Stat(filepath.Join(path, ".git"))
	if errors.Is(err, fs.ErrNotExist) {
		return false, nil
	}
	if err != nil {
		return false, directoryError(err)
	}
	return info.IsDir() || info.Mode().IsRegular(), nil
}

func discoverDirectory(path, filter string, executor core.CommandExecutor) (core.ProjectDiscovery, error) {
	panic("unported: mj-import/src/project_picker.rs discover_directory")
}

// githubRepository requires full_name, as serde does.
type githubRepository struct {
	FullName    string  `json:"full_name"`
	Description *string `json:"description"`
}

func (r *githubRepository) UnmarshalJSON(data []byte) error {
	var wire struct {
		FullName    *string `json:"full_name"`
		Description *string `json:"description"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	if wire.FullName == nil {
		return fmt.Errorf("missing field `full_name`")
	}
	*r = githubRepository{FullName: *wire.FullName, Description: wire.Description}
	return nil
}

// githubSearch requires every field, as serde does.
type githubSearch struct {
	Items             []githubRepository `json:"items"`
	TotalCount        int                `json:"total_count"`
	IncompleteResults bool               `json:"incomplete_results"`
}

func (s *githubSearch) UnmarshalJSON(data []byte) error {
	var wire struct {
		Items *[]githubRepository `json:"items"`
		// TotalCount is unsigned, as Rust's usize refuses a negative count.
		TotalCount        *uint64 `json:"total_count"`
		IncompleteResults *bool   `json:"incomplete_results"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	if wire.Items == nil || wire.TotalCount == nil || wire.IncompleteResults == nil {
		return fmt.Errorf("missing field in GitHub search result")
	}
	*s = githubSearch{Items: *wire.Items, TotalCount: int(*wire.TotalCount), IncompleteResults: *wire.IncompleteResults}
	return nil
}

type githubSearchResult struct {
	repositories []githubRepository
	truncated    bool
}

func discoverGithub(query string, executor core.CommandExecutor) (core.ProjectDiscovery, error) {
	panic("unported: mj-import/src/project_picker.rs discover_github")
}

// ownerScopedQuery reads `owner/`, `owner/name` or a GitHub remote URL as
// an owner and an optional repository name.
func ownerScopedQuery(query string) (owner string, name *string, ok bool) {
	panic("unported: mj-import/src/project_picker.rs owner_scoped_query")
}

func projectDiscovery(repositories []githubRepository, truncated bool) core.ProjectDiscovery {
	panic("unported: mj-import/src/project_picker.rs project_discovery")
}

// executeGithubQuery runs `gh api` for the user's repositories (query nil)
// or a repository search. swallow422 answers an empty result when GitHub
// rejects a synthesized owner search.
func executeGithubQuery(query *string, swallow422 bool, executor core.CommandExecutor) (githubSearchResult, error) {
	panic("unported: mj-import/src/project_picker.rs execute_github_query")
}

// githubError inspects stderr only to select a fixed sentence. Never
// retain, log, or return raw stderr: gh can include credentials, URLs, or
// account details there.
func githubError(status int, stderr []byte) *discoveryError {
	panic("unported: mj-import/src/project_picker.rs github_error")
}
