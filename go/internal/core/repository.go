// Port of mj-core/src/repository.rs.

// Repository identity is independent of a checkout's location and push settings.

package core

import (
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"os/exec"
	"slices"
	"strings"
	"syscall"
)

// ProjectBundleSnapshot is a session's accepted project shape. Catalog
// merges change its project ID, not repository IDs, checkout layout, or
// source settings already accepted.
type ProjectBundleSnapshot struct {
	Bundle         ProjectBundle                 `json:"bundle"`
	Identities     map[string]RepositoryIdentity `json:"identities"`
	NetworkSources map[string]NetworkGitSource   `json:"network_sources"`
	// Repository IDs whose source is a non-snapshot directory rather than a
	// committed Git checkout. A bare local target accepts one; it is fixed
	// here at acceptance so a later `git init` or first commit in that folder
	// cannot silently change what the session's checkpoints and resumes
	// mean. A sorted set (Rust BTreeSet).
	PlainDirectories []string `json:"plain_directories,omitempty"`
}

// MarshalJSON writes empty maps as `{}`, as serde writes an empty BTreeMap.
func (s ProjectBundleSnapshot) MarshalJSON() ([]byte, error) {
	identities := s.Identities
	if identities == nil {
		identities = map[string]RepositoryIdentity{}
	}
	sources := s.NetworkSources
	if sources == nil {
		sources = map[string]NetworkGitSource{}
	}
	var plain []string
	if len(s.PlainDirectories) > 0 {
		plain = slices.Clone(s.PlainDirectories)
		slices.Sort(plain)
		plain = slices.Compact(plain)
	}
	return marshalSerdeJSON(struct {
		Bundle           ProjectBundle                 `json:"bundle"`
		Identities       map[string]RepositoryIdentity `json:"identities"`
		NetworkSources   map[string]NetworkGitSource   `json:"network_sources"`
		PlainDirectories []string                      `json:"plain_directories,omitempty"`
	}{s.Bundle, identities, sources, plain})
}

func (s *ProjectBundleSnapshot) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "bundle", "identities"); err != nil {
		return err
	}
	var wire struct {
		Bundle           ProjectBundle               `json:"bundle"`
		Identities       map[string]json.RawMessage  `json:"identities"`
		NetworkSources   map[string]NetworkGitSource `json:"network_sources"`
		PlainDirectories []string                    `json:"plain_directories"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	identities := make(map[string]RepositoryIdentity, len(wire.Identities))
	for id, raw := range wire.Identities {
		identity, err := unmarshalRepositoryIdentity(raw)
		if err != nil {
			return err
		}
		identities[id] = identity
	}
	plain := wire.PlainDirectories
	if len(plain) > 0 {
		slices.Sort(plain)
		plain = slices.Compact(plain)
	}
	*s = ProjectBundleSnapshot{
		Bundle:           wire.Bundle,
		Identities:       identities,
		NetworkSources:   wire.NetworkSources,
		PlainDirectories: plain,
	}
	return nil
}

// PrimaryIsPlainDirectory is whether the primary project source was accepted
// as a directory that checkpoints leave in place rather than a committed Git
// checkout.
func (s *ProjectBundleSnapshot) PrimaryIsPlainDirectory() bool {
	for _, id := range s.PlainDirectories {
		if id == s.Bundle.PrimaryRepo {
			return true
		}
	}
	return false
}

func (s *ProjectBundleSnapshot) Key() (string, error) {
	if len(s.Identities) != len(s.Bundle.Repositories) {
		return "", errors.New("project identities do not cover its repositories")
	}
	for _, id := range s.PlainDirectories {
		member := false
		for _, repository := range s.Bundle.Repositories {
			if repository.ID == id {
				member = true
				break
			}
		}
		if !member {
			return "", fmt.Errorf("plain project directory %s is not a project repository", debugString(id))
		}
		if _, local := s.Identities[id].(RepositoryIdentityLocal); !local {
			return "", fmt.Errorf("plain project directory %s has a non-local identity", debugString(id))
		}
	}
	primary, ok := s.Identities[s.Bundle.PrimaryRepo]
	if !ok || primary == nil {
		return "", errors.New("project primary identity is missing")
	}
	members := [][2]string{}
	for _, repository := range s.Bundle.Repositories {
		identity, ok := s.Identities[repository.ID]
		if !ok || identity == nil {
			return "", errors.New("project member identity is missing")
		}
		members = append(members, [2]string{identity.Key(), repository.Destination})
	}
	slices.SortFunc(members, func(a, b [2]string) int {
		if order := strings.Compare(a[0], b[0]); order != 0 {
			return order
		}
		return strings.Compare(a[1], b[1])
	})
	encoded, err := marshalSerdeJSON([]any{primary.Key(), members})
	if err != nil {
		return "", err
	}
	return "bundle:" + string(encoded), nil
}

func (s *ProjectBundleSnapshot) SourceKey() (string, error) {
	if len(s.Bundle.Repositories) == 1 {
		primary, ok := s.Identities[s.Bundle.PrimaryRepo]
		if !ok || primary == nil {
			return "", errors.New("project primary identity is missing")
		}
		return primary.Key(), nil
	}
	return s.Key()
}

func (s *ProjectBundleSnapshot) Name() string {
	var names []string
	for _, repository := range s.Bundle.Repositories {
		if identity, ok := s.Identities[repository.ID]; ok && identity != nil {
			names = append(names, identity.Name())
		}
	}
	names = slices.Compact(names)
	return strings.Join(names, " + ")
}

// RepositoryIdentity is a sealed interface over RepositoryIdentityGithub,
// RepositoryIdentityNetwork, RepositoryIdentityLocal and
// RepositoryIdentityRemoteDirectory. A nil value means Rust's None. It
// serializes as serde's externally tagged, snake_case enum.
type RepositoryIdentity interface {
	isRepositoryIdentity()
	Key() string
	Name() string
}

type RepositoryIdentityGithub struct {
	Owner      string
	Repository string
}

type RepositoryIdentityNetwork struct {
	URL string
}

type RepositoryIdentityLocal struct {
	Root string
}

type RepositoryIdentityRemoteDirectory struct {
	Host string `json:"host"`
	Root string `json:"root"`
}

func (RepositoryIdentityGithub) isRepositoryIdentity()          {}
func (RepositoryIdentityNetwork) isRepositoryIdentity()         {}
func (RepositoryIdentityLocal) isRepositoryIdentity()           {}
func (RepositoryIdentityRemoteDirectory) isRepositoryIdentity() {}

func (i RepositoryIdentityGithub) MarshalJSON() ([]byte, error) {
	return marshalSerdeJSON(map[string][2]string{"github": {i.Owner, i.Repository}})
}

func (i RepositoryIdentityNetwork) MarshalJSON() ([]byte, error) {
	return marshalSerdeJSON(map[string]string{"network": i.URL})
}

func (i RepositoryIdentityLocal) MarshalJSON() ([]byte, error) {
	return marshalSerdeJSON(map[string]string{"local": i.Root})
}

func (i RepositoryIdentityRemoteDirectory) MarshalJSON() ([]byte, error) {
	type wire RepositoryIdentityRemoteDirectory
	return marshalSerdeJSON(map[string]wire{"remote_directory": wire(i)})
}

// UnmarshalRepositoryIdentity decodes a RepositoryIdentity for other
// packages (the store reads a project location's identity column).
func UnmarshalRepositoryIdentity(data []byte) (RepositoryIdentity, error) {
	return unmarshalRepositoryIdentity(data)
}

// unmarshalRepositoryIdentity decodes serde's externally tagged form.
func unmarshalRepositoryIdentity(data []byte) (RepositoryIdentity, error) {
	var tagged map[string]json.RawMessage
	if err := json.Unmarshal(data, &tagged); err != nil {
		return nil, err
	}
	if len(tagged) != 1 {
		return nil, errors.New("invalid type: expected a RepositoryIdentity map with a single key")
	}
	for variant, content := range tagged {
		switch variant {
		case "github":
			var pair [2]string
			if err := json.Unmarshal(content, &pair); err != nil {
				return nil, err
			}
			return RepositoryIdentityGithub{Owner: pair[0], Repository: pair[1]}, nil
		case "network":
			var url string
			if err := json.Unmarshal(content, &url); err != nil {
				return nil, err
			}
			return RepositoryIdentityNetwork{URL: url}, nil
		case "local":
			var root string
			if err := json.Unmarshal(content, &root); err != nil {
				return nil, err
			}
			return RepositoryIdentityLocal{Root: root}, nil
		case "remote_directory":
			if err := requireFields(content, "host", "root"); err != nil {
				return nil, err
			}
			type wire RepositoryIdentityRemoteDirectory
			var directory wire
			if err := json.Unmarshal(content, &directory); err != nil {
				return nil, err
			}
			return RepositoryIdentityRemoteDirectory(directory), nil
		default:
			return nil, fmt.Errorf("unknown variant `%s`, expected one of `github`, `network`, `local`, `remote_directory`", variant)
		}
	}
	panic("unreachable")
}

// repositoryIdentityVariant is the declaration order Rust's derived Ord
// compares first.
func repositoryIdentityVariant(identity RepositoryIdentity) int {
	switch identity.(type) {
	case RepositoryIdentityGithub:
		return 0
	case RepositoryIdentityNetwork:
		return 1
	case RepositoryIdentityLocal:
		return 2
	case RepositoryIdentityRemoteDirectory:
		return 3
	}
	return 4
}

// CompareRepositoryIdentity is the Rust derived Ord: variant order, then
// fields (paths compare by component).
func CompareRepositoryIdentity(a, b RepositoryIdentity) int {
	if order := repositoryIdentityVariant(a) - repositoryIdentityVariant(b); order != 0 {
		return order
	}
	switch left := a.(type) {
	case RepositoryIdentityGithub:
		right := b.(RepositoryIdentityGithub)
		if order := strings.Compare(left.Owner, right.Owner); order != 0 {
			return order
		}
		return strings.Compare(left.Repository, right.Repository)
	case RepositoryIdentityNetwork:
		return strings.Compare(left.URL, b.(RepositoryIdentityNetwork).URL)
	case RepositoryIdentityLocal:
		return comparePaths(left.Root, b.(RepositoryIdentityLocal).Root)
	case RepositoryIdentityRemoteDirectory:
		right := b.(RepositoryIdentityRemoteDirectory)
		if order := strings.Compare(left.Host, right.Host); order != 0 {
			return order
		}
		return comparePaths(left.Root, right.Root)
	}
	return 0
}

// RepositoryIdentityFromRemote normalizes only equivalences guaranteed by
// the remote provider. Other servers can have case-sensitive paths or
// different transport namespaces.
func RepositoryIdentityFromRemote(source string) RepositoryIdentity {
	return RepositoryIdentityFromRemoteWith(source, ShorthandAllowed)
}

func RepositoryIdentityFromRemoteWith(source string, shorthand Shorthand) RepositoryIdentity {
	identity, _, ok := repositoryIdentityAndLabel(source, shorthand)
	if !ok {
		return nil
	}
	return identity
}

// repositoryIdentityAndLabel is the identity and display label of a Git
// remote. `shorthand` says whether a bare `owner/repository` means GitHub or
// is a relative path, which a network remote cannot be. Only GitHub's main
// endpoint is a Github identity; the same repository over `ssh.github.com`
// or an explicit SSH port stays Network, because stored identities
// distinguish them.
func repositoryIdentityAndLabel(source string, shorthand Shorthand) (RepositoryIdentity, string, bool) {
	source = DisplayURL(strings.TrimSpace(source))
	if remote := ParseGithubRemote(source, shorthand); remote != nil && remote.IsDefaultEndpoint() {
		label := remote.Owner + "/" + remote.Repository
		return RepositoryIdentityGithub{
			Owner:      asciiLowercase(remote.Owner),
			Repository: asciiLowercase(remote.Repository),
		}, label, true
	}
	if ValidateNetworkURL(source) != nil {
		return nil, "", false
	}
	url := strings.TrimRight(DisplayURL(source), "/")
	for strings.HasSuffix(url, ".git") {
		url = strings.TrimSuffix(url, ".git")
	}
	return RepositoryIdentityNetwork{URL: url}, url, true
}

func RepositoryIdentityRemoteLabel(source string) (string, bool) {
	return RepositoryIdentityRemoteLabelWith(source, ShorthandAllowed)
}

func RepositoryIdentityRemoteLabelWith(source string, shorthand Shorthand) (string, bool) {
	clean := DisplayURL(strings.TrimSpace(source))
	_, label, ok := repositoryIdentityAndLabel(clean, shorthand)
	return label, ok
}

func (i RepositoryIdentityGithub) Key() string  { return "github:" + i.Owner + "/" + i.Repository }
func (i RepositoryIdentityNetwork) Key() string { return "git:" + i.URL }
func (i RepositoryIdentityLocal) Key() string   { return "path:" + i.Root }
func (i RepositoryIdentityRemoteDirectory) Key() string {
	return "path:" + i.Host + ":" + i.Root
}

func (i RepositoryIdentityGithub) Name() string { return i.Repository }
func (i RepositoryIdentityNetwork) Name() string {
	return i.URL[strings.LastIndexAny(i.URL, "/:")+1:]
}
func (i RepositoryIdentityLocal) Name() string { return rootName(i.Root) }
func (i RepositoryIdentityRemoteDirectory) Name() string {
	return rootName(i.Root)
}

// rootName is a directory's last name, or the whole path when it has none.
func rootName(root string) string {
	if name, ok := pathFileName(root); ok {
		return name
	}
	return root
}

// LocalIdentity resolves identity without using the current feature branch
// or altering Git configuration. The caller's executor bounds any
// advertised-HEAD request.
func LocalIdentity(path string, executor CommandExecutor) (RepositoryIdentity, error) {
	resolved, err := ResolveDirectory(path, nil, executor)
	if err != nil {
		return nil, err
	}
	return resolved.Identity, nil
}

type ResolvedDirectory struct {
	CheckoutRoot   string
	RepositoryRoot string
	Identity       RepositoryIdentity
	// The directory has no committed Git checkout. Only a bare local target
	// accepts one; see ResolveLocalDirectory.
	PlainDirectory bool
}

// RepositoryUnavailable is expected absence when discovering historical
// directories. Other resolver failures still indicate an error that
// discovery must report and retry. A sealed interface over
// RepositoryUnavailableDirectory and RepositoryUnavailableNotRepository.
type RepositoryUnavailable interface {
	error
	isRepositoryUnavailable()
}

type RepositoryUnavailableDirectory struct{ Path string }

type RepositoryUnavailableNotRepository struct{ Path string }

func (RepositoryUnavailableDirectory) isRepositoryUnavailable()     {}
func (RepositoryUnavailableNotRepository) isRepositoryUnavailable() {}

func (e RepositoryUnavailableDirectory) Error() string {
	return fmt.Sprintf("project directory %s is unavailable", e.Path)
}

func (e RepositoryUnavailableNotRepository) Error() string {
	return fmt.Sprintf("%s is not a Git repository", e.Path)
}

// ResolveDirectory: the executor can run on the controller or translate Git
// commands to SSH. Preserve the selected checkout while collapsing its
// identity to the owner of the common Git directory.
func ResolveDirectory(path string, host *string, executor CommandExecutor) (ResolvedDirectory, error) {
	roots, err := resolveDirectoryRoots(path, host, executor)
	if err != nil {
		return ResolvedDirectory{}, err
	}
	identity, err := directoryIdentity(path, host, roots.repositoryRoot, executor)
	if err != nil {
		return ResolvedDirectory{}, err
	}
	return ResolvedDirectory{
		CheckoutRoot:   roots.checkoutRoot,
		RepositoryRoot: roots.repositoryRoot,
		Identity:       identity,
	}, nil
}

type directoryRoots struct {
	checkoutRoot   string
	repositoryRoot string
}

func resolveDirectoryRoots(path string, host *string, executor CommandExecutor) (directoryRoots, error) {
	var directory string
	if host == nil {
		canonical, err := canonicalizePath(path)
		if err != nil {
			if errors.Is(err, fs.ErrNotExist) || errors.Is(err, syscall.ENOTDIR) {
				err = RepositoryUnavailableDirectory{Path: path}
			}
			return directoryRoots{}, fmt.Errorf("resolve project directory %s: %w", path, err)
		}
		directory = canonical
		info, err := os.Stat(directory)
		if err != nil {
			return directoryRoots{}, fmt.Errorf("inspect project directory %s: %w", directory, err)
		}
		if !info.IsDir() {
			return directoryRoots{}, RepositoryUnavailableDirectory{Path: path}
		}
	} else {
		if !IsAbsolute(path) {
			return directoryRoots{}, errors.New("remote project directory must be absolute")
		}
		directory = path
	}
	command := NewCommandSpec("git", []string{
		"-C", directory, "rev-parse", "--path-format=absolute", "--show-toplevel", "--git-common-dir",
	}).WithPurpose("resolve project repository and checkout")
	command.Env["LC_ALL"] = "C"
	output, err := executor.Execute(&command)
	if err != nil {
		return directoryRoots{}, err
	}
	if output.Status == 128 {
		diagnostic := strings.TrimSpace(fromUTF8Lossy(output.Stderr))
		if strings.HasPrefix(diagnostic, "fatal: not a git repository") {
			return directoryRoots{}, RepositoryUnavailableNotRepository{Path: path}
		}
		// Remote paths cannot be inspected on the controller. Git's explicit
		// absence diagnosis is distinct from SSH or filesystem access errors.
		if strings.HasPrefix(diagnostic, "fatal: cannot change to '") &&
			(strings.HasSuffix(diagnostic, "': No such file or directory") ||
				strings.HasSuffix(diagnostic, "': Not a directory")) {
			return directoryRoots{}, RepositoryUnavailableDirectory{Path: path}
		}
	}
	if output.Status != 0 {
		return directoryRoots{}, fmt.Errorf("%s is not a Git repository with a readable checkout: %s", path, strings.TrimSpace(fromUTF8Lossy(output.Stderr)))
	}
	text, err := decodeUTF8(output.Stdout, "decode project Git roots")
	if err != nil {
		return directoryRoots{}, err
	}
	roots := rustLines(text)
	if len(roots) == 0 {
		return directoryRoots{}, errors.New("Git omitted the checkout root")
	}
	checkoutRoot := roots[0]
	if !IsAbsoluteOnHostOrTarget(checkoutRoot) {
		return directoryRoots{}, fmt.Errorf("Git returned a non-absolute checkout root: %s", checkoutRoot)
	}
	if host == nil {
		checkoutRoot, err = CanonicalGitOutputPath(checkoutRoot)
		if err != nil {
			return directoryRoots{}, err
		}
	}
	if len(roots) < 2 {
		return directoryRoots{}, errors.New("Git omitted the shared repository directory")
	}
	common, err := ResolveGitPath(directory, roots[1])
	if err != nil {
		return directoryRoots{}, err
	}
	if len(roots) > 2 {
		return directoryRoots{}, fmt.Errorf("Git returned unexpected project roots: %s", debugString(text))
	}
	root := checkoutRoot
	if name, ok := pathFileName(common); ok && name == ".git" {
		parent, ok := pathParent(common)
		if !ok {
			return directoryRoots{}, errors.New("Git common directory has no parent")
		}
		root = parent
	}
	repositoryRoot := root
	if host == nil {
		repositoryRoot, err = canonicalizePath(root)
		if err != nil {
			return directoryRoots{}, err
		}
	}
	return directoryRoots{checkoutRoot: checkoutRoot, repositoryRoot: repositoryRoot}, nil
}

func directoryIdentity(path string, host *string, repositoryRoot string, executor CommandExecutor) (RepositoryIdentity, error) {
	url, err := IdentityFetchURL(path, executor)
	if err != nil {
		return nil, err
	}
	if url != nil {
		identity := RepositoryIdentityFromRemote(*url)
		if identity == nil {
			return nil, fmt.Errorf("invalid repository identity remote for %s", path)
		}
		return identity, nil
	}
	if host != nil {
		return RepositoryIdentityRemoteDirectory{Host: *host, Root: repositoryRoot}, nil
	}
	return RepositoryIdentityLocal{Root: repositoryRoot}, nil
}

// ResolveLocalDirectory resolves a directory a bare local session may run in.
//
// A local directory that is not a committed Git checkout is still a project
// the user can open: its identity is its canonical path, it has no checkout
// above it, and nothing about it can be cloned. This includes an ordinary
// folder, a host with no Git executable, and a repository that has not made
// its first commit yet. Git failures that could hide unreadable repository
// state remain errors. Only a local target qualifies; a remote directory
// still has to be a repository because it is addressed by the repository it
// shares with its sessions.
func ResolveLocalDirectory(path string, executor CommandExecutor) (ResolvedDirectory, error) {
	roots, err := resolveDirectoryRoots(path, nil, executor)
	if err != nil {
		var notRepository RepositoryUnavailableNotRepository
		// Git is only used to recognize a richer project shape. A local
		// directory does not need Git to be used directly, so a missing
		// executable selects that shape instead of refusing the session.
		if errors.As(err, &notRepository) || commandNotFound(err) {
			return plainLocalDirectory(path)
		}
		return ResolvedDirectory{}, err
	}
	command := NewCommandSpec("git", []string{"-C", path, "rev-parse", "--verify", "--quiet", "HEAD"}).
		WithPurpose("inspect local project commit")
	command.Env["LC_ALL"] = "C"
	head, err := executor.Execute(&command)
	if err != nil {
		if commandNotFound(err) {
			return plainLocalDirectory(path)
		}
		return ResolvedDirectory{}, err
	}
	switch {
	case head.Status == 0 && strings.TrimSpace(fromUTF8Lossy(head.Stdout)) != "":
	// A repository with no first commit can still be a working directory.
	// It becomes a plain project at acceptance, before a later commit can
	// change checkpoint semantics.
	case head.Status == 0 || head.Status == 1:
		return plainLocalDirectory(path)
	default:
		return ResolvedDirectory{}, fmt.Errorf("inspect local project commit failed with status %d: %s", head.Status, strings.TrimSpace(fromUTF8Lossy(head.Stderr)))
	}
	identity, err := directoryIdentity(path, nil, roots.repositoryRoot, executor)
	if err != nil {
		return ResolvedDirectory{}, err
	}
	return ResolvedDirectory{
		CheckoutRoot:   roots.checkoutRoot,
		RepositoryRoot: roots.repositoryRoot,
		Identity:       identity,
	}, nil
}

func plainLocalDirectory(path string) (ResolvedDirectory, error) {
	root, err := canonicalizePath(path)
	if err != nil {
		return ResolvedDirectory{}, fmt.Errorf("resolve project directory %s: %w", path, err)
	}
	return ResolvedDirectory{
		CheckoutRoot:   root,
		RepositoryRoot: root,
		Identity:       RepositoryIdentityLocal{Root: root},
		PlainDirectory: true,
	}, nil
}

// commandNotFound is whether any cause of err is a missing file or
// executable (Rust's io::ErrorKind::NotFound).
func commandNotFound(err error) bool {
	return errors.Is(err, fs.ErrNotExist) || errors.Is(err, exec.ErrNotFound)
}
