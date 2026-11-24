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
	"io"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"syscall"
	"unicode"
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
	var inputs [2]string
	switch request := request.(type) {
	case core.ProjectDiscoveryRequestGithub:
		inputs = [2]string{request.Query, ""}
	case *core.ProjectDiscoveryRequestGithub:
		inputs = [2]string{request.Query, ""}
	case core.ProjectDiscoveryRequestDirectory:
		inputs = [2]string{request.Path, request.Filter}
	case *core.ProjectDiscoveryRequestDirectory:
		inputs = [2]string{request.Path, request.Filter}
	}
	for _, input := range inputs {
		if len(input) > maxInputBytes {
			return &discoveryError{"The discovery input is too long; use a shorter query or path."}
		}
	}
	for _, input := range inputs {
		if strings.ContainsRune(input, 0) {
			return &discoveryError{"The discovery input contains a NUL character; remove it and retry."}
		}
	}
	return nil
}

// Discover browses immediate subdirectories or the first page of
// authenticated GitHub results. Directory sources are absolute paths;
// GitHub sources are owner/repo.
func Discover(request core.ProjectDiscoveryRequest, executor core.CommandExecutor) (core.ProjectDiscovery, error) {
	if err := ValidateRequest(request); err != nil {
		return core.ProjectDiscovery{}, err
	}
	if err := checkCancelled(executor); err != nil {
		return core.ProjectDiscovery{}, err
	}
	switch request := request.(type) {
	case core.ProjectDiscoveryRequestGithub:
		return discoverGithub(strings.TrimSpace(request.Query), executor)
	case *core.ProjectDiscoveryRequestGithub:
		return discoverGithub(strings.TrimSpace(request.Query), executor)
	case core.ProjectDiscoveryRequestDirectory:
		return discoverDirectory(request.Path, request.Filter, executor)
	case *core.ProjectDiscoveryRequestDirectory:
		return discoverDirectory(request.Path, request.Filter, executor)
	}
	return core.ProjectDiscovery{}, fmt.Errorf("unknown project discovery request %T", request)
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

// pickerCanonicalize is Rust's fs::canonicalize: an absolute path with
// every link resolved, which must exist.
func pickerCanonicalize(path string) (string, error) {
	absolute, err := filepath.Abs(path)
	if err != nil {
		return "", err
	}
	return filepath.EvalSymlinks(absolute)
}

// pickerParent is Rust's Path::parent: nil for a root.
func pickerParent(path string) (string, bool) {
	parent := filepath.Dir(path)
	if parent == path {
		return "", false
	}
	return parent, true
}

// pickerSelected is one folder kept for the listing, ordered by its
// lowercase name and then its name, as Rust's BTreeMap key does.
type pickerSelected struct {
	lower string
	name  string
	path  string
}

func pickerSelectedLess(left, right *pickerSelected) bool {
	if left.lower != right.lower {
		return left.lower < right.lower
	}
	return left.name < right.name
}

// pickerReadBatch is how many directory entries one read returns. The scan
// keeps only the selected entries, so this bounds memory, not the result.
const pickerReadBatch = 256

func discoverDirectory(path, filter string, executor core.CommandExecutor) (core.ProjectDiscovery, error) {
	input := path
	if input == "" {
		input = "~"
	}
	expanded, err := core.ExpandLocal(input)
	if err != nil {
		return core.ProjectDiscovery{}, &discoveryError{"The home directory could not be resolved. Enter an explicit directory path instead of ~ or ~user."}
	}
	directory, err := pickerCanonicalize(expanded)
	if err != nil {
		return core.ProjectDiscovery{}, directoryError(err)
	}
	source, err := pathText(directory)
	if err != nil {
		return core.ProjectDiscovery{}, err
	}
	handle, err := os.Open(directory)
	if err != nil {
		return core.ProjectDiscovery{}, directoryError(err)
	}
	defer handle.Close()
	children, readErr := handle.ReadDir(pickerReadBatch)
	if readErr != nil && !errors.Is(readErr, io.EOF) {
		return core.ProjectDiscovery{}, directoryError(readErr)
	}
	var entries []core.ProjectEntry
	repository, err := isRepository(directory)
	if err != nil {
		return core.ProjectDiscovery{}, err
	}
	if repository {
		basename := source
		if _, ok := pickerParent(directory); ok {
			if name := filepath.Base(directory); utf8.ValidString(name) {
				basename = name
			}
		}
		entries = append(entries, core.ProjectEntry{
			Name:        "Use " + basename,
			Source:      source,
			Description: "Use this repository",
			Kind:        core.ProjectEntryKindRepository,
		})
	}
	remaining := maxEntries - len(entries)
	var selected []pickerSelected
	truncated := false
	filter = strings.ToLower(filter)
	// Keep memory bounded while sorting independently of read_dir order.
	// The scan is nonrecursive and checks the caller's deadline throughout.
	// Apply the name filter before limiting so omitted folders remain
	// reachable.
	for {
		for _, child := range children {
			if err := checkCancelled(executor); err != nil {
				return core.ProjectDiscovery{}, err
			}
			name := child.Name()
			if name == ".git" || utf8.ValidString(name) && !strings.Contains(strings.ToLower(name), filter) {
				continue
			}
			childPath := filepath.Join(directory, name)
			isDir := child.IsDir()
			if child.Type()&fs.ModeSymlink != 0 {
				info, err := os.Stat(childPath)
				switch {
				case err == nil:
					isDir = info.IsDir()
				case errors.Is(err, fs.ErrNotExist):
					isDir = false
				default:
					return core.ProjectDiscovery{}, directoryError(err)
				}
			}
			if !isDir {
				continue
			}
			name, err := pathText(name)
			if err != nil {
				return core.ProjectDiscovery{}, err
			}
			entry := pickerSelected{lower: strings.ToLower(name), name: name, path: childPath}
			at := sort.Search(len(selected), func(index int) bool { return !pickerSelectedLess(&selected[index], &entry) })
			selected = append(selected, pickerSelected{})
			copy(selected[at+1:], selected[at:])
			selected[at] = entry
			if len(selected) > remaining {
				selected = selected[:len(selected)-1]
				truncated = true
			}
		}
		if readErr != nil {
			break
		}
		children, readErr = handle.ReadDir(pickerReadBatch)
		if readErr != nil && !errors.Is(readErr, io.EOF) {
			return core.ProjectDiscovery{}, directoryError(readErr)
		}
	}
	for _, entry := range selected {
		if err := checkCancelled(executor); err != nil {
			return core.ProjectDiscovery{}, err
		}
		repository, err := isRepository(entry.path)
		if err != nil {
			return core.ProjectDiscovery{}, err
		}
		source, err := pathText(entry.path)
		if err != nil {
			return core.ProjectDiscovery{}, err
		}
		description, kind := "Directory", core.ProjectEntryKindDirectory
		if repository {
			description, kind = "Git repository", core.ProjectEntryKindRepository
		}
		entries = append(entries, core.ProjectEntry{
			Name:        entry.name,
			Source:      source,
			Description: description,
			Kind:        kind,
		})
	}
	if err := checkCancelled(executor); err != nil {
		return core.ProjectDiscovery{}, err
	}
	var parent *string
	if parentPath, ok := pickerParent(directory); ok {
		text, err := pathText(parentPath)
		if err != nil {
			return core.ProjectDiscovery{}, err
		}
		parent = &text
	}
	return core.ProjectDiscovery{
		Entries:   entries,
		Directory: &source,
		Parent:    parent,
		Truncated: truncated,
	}, nil
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
	if query == "" {
		result, err := executeGithubQuery(nil, false, executor)
		if err != nil {
			return core.ProjectDiscovery{}, err
		}
		return projectDiscovery(result.repositories, result.truncated), nil
	}
	if owner, name, ok := ownerScopedQuery(query); ok {
		q := fmt.Sprintf("user:%s fork:true", owner)
		if name != nil {
			q = fmt.Sprintf("user:%s %s in:name fork:true", owner, *name)
		}
		result, err := executeGithubQuery(&q, true, executor)
		if err != nil {
			return core.ProjectDiscovery{}, err
		}
		if name != nil {
			exact := utilityASCIILower(owner + "/" + *name)
			for index, repo := range result.repositories {
				if utilityASCIILower(repo.FullName) == exact {
					repositories := append([]githubRepository{repo}, result.repositories[:index]...)
					result.repositories = append(repositories, result.repositories[index+1:]...)
					break
				}
			}
		}
		return projectDiscovery(result.repositories, result.truncated), nil
	}
	if core.ValidGithubOwnerLogin(query) && !strings.ContainsAny(query, " \t\n/:") {
		q := fmt.Sprintf("user:%s fork:true", query)
		owner, err := executeGithubQuery(&q, true, executor)
		if err != nil {
			return core.ProjectDiscovery{}, err
		}
		if len(owner.repositories) >= maxEntries {
			return projectDiscovery(owner.repositories[:maxEntries], true), nil
		}
		if err := checkCancelled(executor); err != nil {
			return core.ProjectDiscovery{}, err
		}
		generalQuery := query + " fork:true"
		general, err := executeGithubQuery(&generalQuery, false, executor)
		if err != nil {
			return core.ProjectDiscovery{}, err
		}
		repositories := owner.repositories
		seen := map[string]bool{}
		for _, repo := range repositories {
			seen[utilityASCIILower(repo.FullName)] = true
		}
		for _, repo := range general.repositories {
			key := utilityASCIILower(repo.FullName)
			if !seen[key] {
				seen[key] = true
				repositories = append(repositories, repo)
			}
		}
		truncated := owner.truncated || general.truncated || len(repositories) > maxEntries
		return projectDiscovery(repositories[:min(len(repositories), maxEntries)], truncated), nil
	}
	hasFork := false
	for _, token := range strings.Fields(query) {
		token = strings.TrimPrefix(token, "-")
		if len(token) >= 5 && utilityASCIILower(token[:5]) == "fork:" {
			hasFork = true
			break
		}
	}
	q := query
	if !hasFork {
		q = query + " fork:true"
	}
	result, err := executeGithubQuery(&q, false, executor)
	if err != nil {
		return core.ProjectDiscovery{}, err
	}
	return projectDiscovery(result.repositories, result.truncated), nil
}

// ownerScopedQuery reads `owner/`, `owner/name` or a GitHub remote URL as
// an owner and an optional repository name.
func ownerScopedQuery(query string) (owner string, name *string, ok bool) {
	if remote := core.ParseGithubRemote(query, core.ShorthandRejected); remote != nil {
		repository := remote.Repository
		return remote.Owner, &repository, true
	}
	if strings.IndexFunc(query, unicode.IsSpace) >= 0 ||
		strings.Contains(query, ":") ||
		strings.Count(query, "/") != 1 {
		return "", nil, false
	}
	owner, rest, _ := strings.Cut(query, "/")
	if !core.ValidGithubOwnerLogin(owner) {
		return "", nil, false
	}
	if rest == "" {
		return owner, nil, true
	}
	part := strings.TrimSuffix(rest, ".git")
	if part == "" || len(part) > 100 || part == "." || part == ".." {
		return "", nil, false
	}
	for index := 0; index < len(part); index++ {
		b := part[index]
		if !(b >= 'a' && b <= 'z' || b >= 'A' && b <= 'Z' || b >= '0' && b <= '9' || b == '-' || b == '_' || b == '.') {
			return "", nil, false
		}
	}
	return owner, &part, true
}

func projectDiscovery(repositories []githubRepository, truncated bool) core.ProjectDiscovery {
	truncated = truncated || len(repositories) > maxEntries
	entries := []core.ProjectEntry{}
	for _, repo := range repositories[:min(len(repositories), maxEntries)] {
		description := ""
		if repo.Description != nil {
			description = *repo.Description
		}
		entries = append(entries, core.ProjectEntry{
			Name:        repo.FullName,
			Source:      repo.FullName,
			Description: description,
			Kind:        core.ProjectEntryKindRepository,
		})
	}
	return core.ProjectDiscovery{Entries: entries, Truncated: truncated}
}

// executeGithubQuery runs `gh api` for the user's repositories (query nil)
// or a repository search. swallow422 answers an empty result when GitHub
// rejects a synthesized owner search.
func executeGithubQuery(query *string, swallow422 bool, executor core.CommandExecutor) (githubSearchResult, error) {
	endpoint := "search/repositories"
	if query == nil {
		endpoint = "user/repos"
	}
	command := core.NewCommandSpec("gh", []string{
		"api",
		endpoint,
		"--hostname",
		"github.com",
		"--method",
		"GET",
		"--include",
		"--raw-field",
		"per_page=100",
		"--raw-field",
		"sort=updated",
	}).WithPurpose("discover GitHub repositories")
	if query == nil {
		command.Args = append(command.Args,
			"--raw-field", "direction=desc",
			"--raw-field", "visibility=all",
			"--raw-field", "affiliation=owner,collaborator,organization_member",
		)
	} else {
		// --raw-field treats @, braces, spaces, and qualifiers as literal
		// query data; --field would expand filenames and repository
		// placeholders.
		command.Args = append(command.Args,
			"--raw-field", "q="+*query,
			"--raw-field", "order=desc",
		)
	}
	command.Env["GH_PROMPT_DISABLED"] = "1"
	command.Env["GH_DEBUG"] = ""
	command.Env["NO_COLOR"] = "1"
	output, err := executor.Execute(&command)
	if err != nil {
		switch {
		case executor.CancellationRequested():
			return githubSearchResult{}, &discoveryError{"Project discovery was cancelled or timed out; retry the request."}
		case errors.Is(err, fs.ErrNotExist) || errors.Is(err, exec.ErrNotFound):
			return githubSearchResult{}, &discoveryError{"GitHub CLI (gh) is not installed or is not on PATH. Install it on the machine running discovery and retry."}
		}
		return githubSearchResult{}, &discoveryError{"GitHub CLI (gh) could not run. Check its installation and the network connection, then retry."}
	}
	if err := checkCancelled(executor); err != nil {
		return githubSearchResult{}, err
	}
	if output.Status != 0 {
		if swallow422 && strings.Contains(utilityASCIILower(strings.ToValidUTF8(string(output.Stderr), "�")), "http 422") {
			return githubSearchResult{repositories: []githubRepository{}}, nil
		}
		return githubSearchResult{}, githubError(output.Status, output.Stderr)
	}
	invalid := &discoveryError{"GitHub returned an unreadable repository list. Update GitHub CLI (gh) and retry."}
	if !utf8.Valid(output.Stdout) {
		return githubSearchResult{}, invalid
	}
	text := string(output.Stdout)
	headers, body, found := strings.Cut(text, "\r\n\r\n")
	if !found {
		headers, body, found = strings.Cut(text, "\n\n")
		if !found {
			return githubSearchResult{}, invalid
		}
	}
	truncated := false
	for _, line := range strings.Split(headers, "\n") {
		line = strings.TrimSuffix(line, "\r")
		name, value, ok := strings.Cut(line, ":")
		if !ok || utilityASCIILower(name) != "link" {
			continue
		}
		for _, link := range strings.Split(value, ",") {
			if strings.Contains(link, `rel="next"`) {
				truncated = true
			}
		}
	}
	var repositories []githubRepository
	if query == nil {
		var list *[]githubRepository
		if err := json.Unmarshal([]byte(body), &list); err != nil || list == nil {
			return githubSearchResult{}, invalid
		}
		repositories = *list
	} else {
		var result *githubSearch
		if err := json.Unmarshal([]byte(body), &result); err != nil || result == nil {
			return githubSearchResult{}, invalid
		}
		truncated = truncated || result.IncompleteResults || result.TotalCount > len(result.Items)
		repositories = result.Items
	}
	truncated = truncated || len(repositories) > maxEntries
	return githubSearchResult{repositories: repositories, truncated: truncated}, nil
}

// githubError inspects stderr only to select a fixed sentence. Never
// retain, log, or return raw stderr: gh can include credentials, URLs, or
// account details there.
func githubError(status int, stderr []byte) *discoveryError {
	text := utilityASCIILower(strings.ToValidUTF8(string(stderr), "�"))
	switch {
	case strings.Contains(text, "rate limit") || strings.Contains(text, "http 429"):
		return &discoveryError{"GitHub's API rate limit was reached. Wait a few minutes and retry."}
	case status == 4 ||
		strings.Contains(text, "gh auth login") ||
		strings.Contains(text, "http 401") ||
		strings.Contains(text, "bad credentials") ||
		strings.Contains(text, "not logged"):
		return &discoveryError{"GitHub login is unavailable or expired. Run gh auth login on the machine running discovery, then retry."}
	case strings.Contains(text, "http 403"):
		return &discoveryError{"GitHub denied access. Check the existing gh login's repository permissions and organization authorization, then retry."}
	case strings.Contains(text, "http 422"):
		return &discoveryError{"GitHub rejected that search. Check the query and its qualifiers, then retry."}
	}
	return &discoveryError{"GitHub could not be reached. Check the network connection and GitHub availability, then retry."}
}
