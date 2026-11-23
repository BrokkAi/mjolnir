// Port of mj-core/src/remote_git.rs.

// Resolve repository sources that can be used by a network-backed checkout.
//
// A source contains the URLs to put in the target repository's `origin`.
// Resolution is deliberately separate from discovering the default branch:
// reading a host checkout's configuration is local work, while asking a
// server what `HEAD` means is an explicit, bounded operation.
//
// Rust re-exports local_git::resolve_local_repository here; in package core
// it is ResolveLocalRepository.

package core

import (
	"encoding/json"
	"errors"
	"fmt"
	"net/netip"
	"strconv"
	"strings"
	"unicode"
	"unicode/utf8"
)

// FinalizeManagedCheckout finalizes a managed checkout using its recorded
// immutable launch base. The caller supplies the command boundary (local Git
// or a target executor). Only legacy restores whose base object is already
// absent skip retention; their recorded markers retain the same meaning as
// before base refs existed.
func FinalizeManagedCheckout(base string, retainBase bool, run func(args []string) error) error {
	if base == "" {
		return errors.New("managed checkout has no immutable base commit")
	}
	// Git config is not a GC root when the launch commit becomes orphaned.
	if retainBase {
		if err := run([]string{"update-ref", HistoryCutoffBaseRef, base}); err != nil {
			return err
		}
	}
	for _, setting := range [][2]string{
		{"push.default", "current"},
		{"push.autoSetupRemote", "true"},
		{"remote.pushDefault", "origin"},
		{"remote.origin.mirror", "false"},
		{"mj.baseCommit", base},
		{"mj.remoteWorkspace", "true"},
	} {
		if err := run([]string{"config", "--local", "--replace-all", setting[0], setting[1]}); err != nil {
			return err
		}
	}
	return nil
}

// ExactCheckout is an immutable starting selection for one repository in a
// bundle. The API names it `at` and `branch`; the daemon picks the bundle's
// primary repository.
type ExactCheckout struct {
	RepositoryID string  `json:"repository_id"`
	Commit       string  `json:"commit"`
	Branch       *string `json:"branch,omitempty"`
	// The workspace holds `commit`, the diff base, and their ancestors, and
	// nothing later: it has no `origin` remote, remote-tracking refs, or tags,
	// and does not borrow objects from the clone cache. The API and `mj new`
	// name it `no_later_history`.
	NoLaterHistory bool `json:"no_later_history,omitempty"`
}

func (c *ExactCheckout) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "repository_id", "commit"); err != nil {
		return err
	}
	type wire ExactCheckout
	return unmarshalDenyUnknown(data, (*wire)(c))
}

func (c *ExactCheckout) Validate() error {
	if c.RepositoryID == "" {
		return errors.New("checkout repository_id is required")
	}
	if !IsObjectID(c.Commit) {
		return errors.New("`at` must be a full nonzero commit object ID")
	}
	if c.Branch != nil {
		if err := validateBranchName(*c.Branch); err != nil {
			return fmt.Errorf("invalid `branch`: %w", err)
		}
		if *c.Branch == "HEAD" {
			return errors.New("`branch` cannot be HEAD")
		}
	}
	return nil
}

// NetworkGitSource is the network endpoints a target checkout should use for
// Git fetches and pushes. The first push URL is the primary destination; all
// configured push URLs are retained because Git intentionally supports
// pushing to several destinations.
type NetworkGitSource struct {
	FetchURL string   `json:"fetch_url"`
	PushURLs []string `json:"push_urls"`
}

// MarshalJSON writes no push URLs as `[]`, as serde writes an empty Vec.
func (s NetworkGitSource) MarshalJSON() ([]byte, error) {
	type wire NetworkGitSource
	value := wire(s)
	value.PushURLs = nonNilStrings(value.PushURLs)
	return marshalSerdeJSON(value)
}

func (s *NetworkGitSource) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "fetch_url", "push_urls"); err != nil {
		return err
	}
	type wire NetworkGitSource
	return json.Unmarshal(data, (*wire)(s))
}

// ResolveRepository resolves a configured repository without contacting its
// server.
//
// GitHub's `owner/repository` shorthand is expanded to the HTTPS clone URL.
// The default branch is intentionally not inferred here; callers that need
// it must make the separate DefaultBranch request.
func ResolveRepository(repository *ProjectRepository, executor CommandExecutor) (NetworkGitSource, error) {
	if repository.GitRef != nil {
		return NetworkGitSource{}, fmt.Errorf("repository %s sets obsolete `git_ref`; remove it so Hel can resolve the remote default branch", debugString(repository.ID))
	}
	switch {
	case repository.Github != nil && repository.Local == nil:
		fetchURL, err := ExpandGithubSource(*repository.Github)
		if err != nil {
			return NetworkGitSource{}, err
		}
		return NetworkGitSource{FetchURL: fetchURL, PushURLs: []string{fetchURL}}, nil
	case repository.Github == nil && repository.Local != nil:
		// A missing remote is the person's to fix, so the reason is marked
		// as a refusal and travels to whoever asked for the session instead
		// of stopping at the daemon log.
		source, err := ResolveLocalRepository(*repository.Local, executor)
		if err != nil {
			refusal := RefusalPrecondition(fmt.Sprintf(
				"repository %s needs valid default network Git remotes for an isolated session; configure a remote or use a raw local session",
				debugString(repository.ID),
			))
			return NetworkGitSource{}, fmt.Errorf("%w: %w", refusal, err)
		}
		return source, nil
	case repository.Github == nil && repository.Local == nil:
		return NetworkGitSource{}, fmt.Errorf("repository %s has no configured Git source", debugString(repository.ID))
	}
	return NetworkGitSource{}, fmt.Errorf("repository %s must configure exactly one of `github` or `local`", debugString(repository.ID))
}

// githubCredentialHelper answers Git's credential request for github.com
// from GH_TOKEN or GITHUB_TOKEN, then the GitHub CLI.
const githubCredentialHelper = `credential.https://github.com.helper=!f() { if [ "$1" = get ] && [ -n "${GH_TOKEN:-${GITHUB_TOKEN:-}}" ]; then printf '%s\n' username=x-access-token "password=${GH_TOKEN:-$GITHUB_TOKEN}"; else gh auth git-credential "$@"; fi; }; f`

// DefaultBranch asks the configured server for the branch its symbolic
// `HEAD` names.
//
// This is the only network operation in this module. The supplied executor
// owns its timeout, cancellation, and subprocess supervision; this function
// only makes the command noninteractive and parses its bounded result.
func DefaultBranch(source *NetworkGitSource, executor CommandExecutor) (string, error) {
	if err := ValidateNetworkURL(source.FetchURL); err != nil {
		return "", fmt.Errorf("invalid Git fetch URL %s: %w", DisplayURL(source.FetchURL), err)
	}
	if executor.CancellationRequested() {
		return "", errors.New("operation cancelled while resolving the remote default branch")
	}

	// Keep the source as an argument, including any configured credentials.
	// DisplayURL is used only for diagnostics; stripping credentials here
	// would change how Git authenticates.
	command := NewCommandSpec("git", []string{"ls-remote", "--symref", "--", source.FetchURL, "HEAD"}).
		WithPurpose("resolve remote default branch")
	// Git does not read GH_TOKEN itself. Scope the normal GitHub token/CLI
	// credential flow to this command, without changing the host Git config.
	if parsed, ok := parseRustURL(source.FetchURL); ok && parsed.scheme == "https" && parsed.hasHost && parsed.host == "github.com" {
		command.Args = append([]string{"-c", githubCredentialHelper}, command.Args...)
	}
	UnattendedGitEnvironment(&command)

	output, err := executor.Execute(&command)
	if err != nil {
		detail := sanitizeDiagnostic(err.Error(), source.FetchURL)
		return "", fmt.Errorf("could not resolve the default branch from %s: %s", DisplayURL(source.FetchURL), detail)
	}
	if executor.CancellationRequested() {
		return "", errors.New("operation cancelled while resolving the remote default branch")
	}
	if output.Status != 0 {
		detail := sanitizeDiagnostic(strings.TrimSpace(strings.ToValidUTF8(string(output.Stderr), "�")), source.FetchURL)
		if detail == "" {
			return "", fmt.Errorf("could not resolve the default branch from %s (Git exited with status %d)", DisplayURL(source.FetchURL), output.Status)
		}
		return "", fmt.Errorf("could not resolve the default branch from %s: %s", DisplayURL(source.FetchURL), detail)
	}
	branch, err := parseDefaultBranch(&output)
	if err != nil {
		return "", fmt.Errorf("parse default branch advertised by %s: %w", DisplayURL(source.FetchURL), err)
	}
	return branch, nil
}

// ValidateNetworkURL validates that a Git endpoint uses a network transport.
//
// Git accepts local paths and several helper transports anywhere a remote
// URL is expected. They would make a target checkout depend on the
// controller's filesystem or execute an unbounded helper, so they are
// rejected here.
func ValidateNetworkURL(url string) error {
	_, _, err := networkURL(url, false)
	return err
}

// ProjectSource parses a project source and removes HTTP userinfo before it
// can be saved. It returns the accepted spelling verbatim except for its
// HTTP credentials; stripped reports that credentials were removed (Rust's
// Cow::Owned).
func ProjectSource(source string, shorthand Shorthand) (accepted string, stripped bool, err error) {
	source = strings.TrimSpace(source)
	if remote := ParseGithubRemote(source, shorthand); remote != nil && remote.Spelling == GithubSpellingShorthand {
		return source, false, nil
	}
	return networkURL(source, true)
}

// validateConfiguredProjectSource keeps loading all sources accepted by the
// previous config validator.
func validateConfiguredProjectSource(source string) error {
	_, _, err := ProjectSource(source, ShorthandAllowed)
	if err != nil && wasConfiguredGithubSource(source) {
		return nil
	}
	return err
}

// wasConfiguredGithubSource: validation at checkout still rejects unusable
// sources.
func wasConfiguredGithubSource(source string) bool {
	source = strings.TrimSpace(source)
	if source == "" || strings.HasPrefix(source, "-") || strings.IndexFunc(source, unicode.IsSpace) >= 0 {
		return false
	}
	if ParseGithubRemote(source, ShorthandAllowed) != nil {
		return true
	}
	repositoryPath := source
	for _, prefix := range []string{"https://github.com/", "git@github.com:", "ssh://git@github.com/"} {
		if rest, ok := strings.CutPrefix(source, prefix); ok {
			repositoryPath = rest
			break
		}
	}
	for strings.HasSuffix(repositoryPath, ".git") {
		repositoryPath = strings.TrimSuffix(repositoryPath, ".git")
	}
	parts := strings.Split(repositoryPath, "/")
	return len(parts) == 2 && parts[0] != "" && parts[1] != ""
}

// SSHLogin is the SSH login in an accepted Git URL. An empty login uses SSH
// configuration. false when the source names no SSH login at all.
func SSHLogin(source string) (string, bool) {
	if strings.Contains(source, "://") {
		parsed, ok := parseRustURL(source)
		if !ok || parsed.scheme != "ssh" {
			return "", false
		}
		return parsed.username, true
	}
	authority, _, found := strings.Cut(source, ":")
	if !found {
		return "", false
	}
	if at := strings.LastIndex(authority, "@"); at >= 0 {
		return authority[:at], true
	}
	return "", true
}

// ProjectSourceName is a label derived from the repository path,
// independent of HTTP query data.
func ProjectSourceName(source string) (string, bool) {
	if remote := ParseGithubRemote(source, ShorthandAllowed); remote != nil {
		return remote.Repository, true
	}
	path := source
	if strings.Contains(source, "://") {
		if parsed, ok := parseRustURL(source); ok {
			path = parsed.path
		}
	}
	path = strings.TrimRight(path, "/")
	for strings.HasSuffix(path, ".git") {
		path = strings.TrimSuffix(path, ".git")
	}
	name := path[strings.LastIndexAny(path, "/:")+1:]
	if name == "" {
		return "", false
	}
	return name, true
}

const HTTPUserinfoRemoved = "Removed credentials from the HTTP Git URL. Configure Git credentials on the target."

// networkURL validates a network Git URL. With stripHTTPUserinfo it removes
// the userinfo of an HTTP(S) URL, keeping the rest exactly as entered;
// stripped reports that it did (Rust's Cow::Owned).
func networkURL(url string, stripHTTPUserinfo bool) (accepted string, stripped bool, err error) {
	value := strings.TrimSpace(url)
	if value == "" {
		return "", false, errors.New("Git URL is empty")
	}
	if value != url || strings.HasPrefix(value, "-") || strings.IndexFunc(value, unicode.IsSpace) >= 0 {
		return "", false, fmt.Errorf("Git URL %s contains invalid whitespace or option syntax", DisplayURL(value))
	}
	if LooksLikePath(value) {
		return "", false, fmt.Errorf("Git URL %s is a local repository path", DisplayURL(value))
	}
	for _, prefix := range []string{"ext::", "helper::", "local::", "fd::"} {
		if strings.HasPrefix(value, prefix) {
			return "", false, fmt.Errorf("Git URL %s uses a local or helper transport", DisplayURL(value))
		}
	}

	if !strings.Contains(value, "://") {
		if scheme, _, found := strings.Cut(value, ":"); found {
			switch asciiLowercase(scheme) {
			case "file", "ext", "helper", "local", "fd", "http", "https", "ssh", "git":
				return "", false, fmt.Errorf("Git URL %s uses an unsupported non-network or malformed URL transport", DisplayURL(value))
			}
		}
	}

	if strings.Contains(value, "://") {
		parsed, err := parseRustURLWithError(value)
		if err != nil {
			return "", false, fmt.Errorf("parse Git URL %s: %w", DisplayURL(value), err)
		}
		scheme := parsed.scheme
		switch scheme {
		case "http", "https", "ssh", "git":
		default:
			return "", false, fmt.Errorf("Git URL %s does not use a supported network transport", DisplayURL(value))
		}
		if !parsed.hasHost {
			return "", false, fmt.Errorf("Git URL %s does not use a supported network transport", DisplayURL(value))
		}
		if stripHTTPUserinfo && (scheme == "http" || scheme == "https") {
			// Keep the path, query, fragment and escape spelling exactly as
			// entered. The parsed URL establishes its transport and host.
			_, endpoint, _ := strings.Cut(value, "://")
			authority := strings.TrimLeft(endpoint, `/\`)
			start := len(value) - len(authority)
			if end := strings.IndexAny(authority, `/?#\`); end >= 0 {
				authority = authority[:end]
			}
			if at := strings.LastIndex(authority, "@"); at >= 0 {
				return value[:start] + value[start+at+1:], true, nil
			}
		}
		return value, false, nil
	}

	// The remaining network spelling Git accepts is scp-like SSH, for
	// example `git@github.com:owner/repository.git`. A path without a colon
	// is a local path and is rejected.
	host, remotePath, found := strings.Cut(value, ":")
	if !found {
		return "", false, fmt.Errorf("Git URL %s is a local repository path", DisplayURL(value))
	}
	if host == "" || remotePath == "" || strings.HasPrefix(host, ".") || strings.Contains(host, "/") ||
		strings.Contains(host, `\`) || (strings.Contains(host, "@") && strings.HasSuffix(host, "@")) {
		return "", false, fmt.Errorf("Git URL %s is not a valid scp-style SSH endpoint", DisplayURL(value))
	}
	if at := strings.LastIndex(host, "@"); at >= 0 && (host[:at] == "" || host[at+1:] == "") {
		return "", false, fmt.Errorf("Git URL %s is not a valid scp-style SSH endpoint", DisplayURL(value))
	}
	if strings.HasPrefix(remotePath, ":") {
		return "", false, fmt.Errorf("Git URL %s uses an unsupported helper transport", DisplayURL(value))
	}
	return value, false, nil
}

// IsHostOnlyGitSource reports whether a Git source names the host's
// filesystem or loopback network. Bundle validation rejects local paths and
// file URLs before checkout.
func IsHostOnlyGitSource(source string) bool {
	source = strings.TrimSpace(source)
	if LooksLikePath(source) {
		return true
	}
	if strings.Contains(source, "://") || (len(source) >= 5 && asciiEqualFold(source[:5], "file:")) {
		parsed, ok := parseRustURL(source)
		return ok && (parsed.scheme == "file" || (parsed.hasHost && isLoopbackGitHost(parsed.host)))
	}
	authority, _, found := strings.Cut(source, ":")
	if !found {
		return true // Git interprets a bare name as a relative filesystem path.
	}
	if strings.ContainsAny(authority, `/\`) {
		return true
	}
	host := authority[strings.LastIndex(authority, "@")+1:]
	if strings.HasPrefix(host, "[") {
		// Git recognizes the colon after a bracketed IPv6 host as the path separator.
		endpoint := source[len(authority)-len(host):]
		bracketed, _, found := strings.Cut(endpoint, "]:")
		return found && isLoopbackGitHost(bracketed)
	}
	return isLoopbackGitHost(host)
}

func isLoopbackGitHost(host string) bool {
	host = strings.TrimRight(strings.TrimLeft(host, "["), "]")
	if asciiEqualFold(strings.TrimRight(host, "."), "localhost") {
		return true
	}
	// Rust's IpAddr parser: no zones; an IPv6 address is loopback only as ::1.
	address, err := netip.ParseAddr(host)
	if err != nil || address.Zone() != "" {
		return false
	}
	if address.Is6() {
		return address == netip.IPv6Loopback()
	}
	return address.IsLoopback()
}

// asciiEqualFold is Rust's `str::eq_ignore_ascii_case`.
func asciiEqualFold(left, right string) bool {
	return len(left) == len(right) && asciiLowercase(left) == asciiLowercase(right)
}

// DisplayURL returns a URL suitable for diagnostics or persistence in
// user-visible state. Userinfo is removed from both URL and scp-style SSH
// spellings.
func DisplayURL(url string) string {
	if parsed, ok := parseRustURL(url); ok {
		parsed.username = ""
		parsed.password = nil
		return parsed.String()
	}

	if schemeEnd := strings.Index(url, "://"); schemeEnd >= 0 {
		authorityStart := schemeEnd + 3
		authorityEnd := len(url)
		if offset := strings.IndexAny(url[authorityStart:], "/?#"); offset >= 0 {
			authorityEnd = authorityStart + offset
		}
		if at := strings.LastIndex(url[authorityStart:authorityEnd], "@"); at >= 0 {
			at += authorityStart
			return url[:authorityStart] + url[at+1:]
		}
	} else if colon := strings.Index(url, ":"); colon >= 0 {
		authority := url[:colon]
		if at := strings.LastIndex(authority, "@"); at >= 0 {
			return url[at+1:colon] + url[colon:]
		}
	}
	return url
}

func ExpandGithubSource(source string) (string, error) {
	source = strings.TrimSpace(source)
	shorthand := !strings.Contains(source, "://") && !strings.Contains(source, "@") &&
		!strings.Contains(source, ":") && strings.Contains(source, "/")
	expanded := source
	if shorthand {
		owner, repository, ok := GithubOwnerRepo(source)
		if !ok {
			return "", fmt.Errorf("invalid GitHub owner/repository %s", debugString(source))
		}
		expanded = fmt.Sprintf("https://github.com/%s/%s.git", owner, repository)
	}
	if err := ValidateNetworkURL(expanded); err != nil {
		return "", fmt.Errorf("invalid configured GitHub source %s: %w", DisplayURL(expanded), err)
	}
	return expanded, nil
}

// CanonicalGithubHTTPSURL is `https://github.com/{owner}/{repository}.git`
// for a remote on GitHub's default endpoint.
func CanonicalGithubHTTPSURL(source string, shorthand Shorthand) (string, bool) {
	remote := ParseGithubRemote(source, shorthand)
	if remote == nil || !remote.IsDefaultEndpoint() {
		return "", false
	}
	return fmt.Sprintf("https://github.com/%s/%s.git", remote.Owner, remote.Repository), true
}

// DisplayNetworkURL returns a sanitized, normalized URL for user-facing
// clone/push previews.
//
// Default-endpoint GitHub remotes are shown as
// `https://github.com/{owner}/{repository}.git` (stripping any userinfo and
// unifying SSH/HTTPS/suffix spellings) without altering operational URLs
// passed to Git. Other remotes use DisplayURL.
func DisplayNetworkURL(url string) string {
	sanitized := DisplayURL(strings.TrimSpace(url))
	if canonical, ok := CanonicalGithubHTTPSURL(sanitized, ShorthandRejected); ok {
		return canonical
	}
	return sanitized
}

// DisplayNetworkURLs sanitizes, normalizes, and deduplicates a list of
// user-facing remote URLs while preserving first-seen order.
func DisplayNetworkURLs(urls []string) []string {
	seen := map[string]bool{}
	displayed := []string{}
	for _, url := range urls {
		url = DisplayNetworkURL(url)
		if !seen[url] {
			seen[url] = true
			displayed = append(displayed, url)
		}
	}
	return displayed
}

// Shorthand is whether ParseGithubRemote accepts a bare `owner/repository`.
//
// Text typed into a `github =` setting, the CLI or the API may be shorthand.
// Text read from `git config remote.origin.url` may not: there `mirror/repo`
// is a relative filesystem path, not GitHub.
type Shorthand int

const (
	ShorthandAllowed Shorthand = iota
	ShorthandRejected
)

// GithubSpelling is how a GitHub remote was written.
type GithubSpelling int

const (
	// A bare `owner/repository`.
	GithubSpellingShorthand GithubSpelling = iota
	GithubSpellingHTTPS
	GithubSpellingHTTP
	// `[git@]github.com:owner/repository`.
	GithubSpellingScp
	// `ssh://[git@]host[:port]/owner/repository`.
	GithubSpellingSSH
	// `git://github.com/owner/repository`. GitHub stopped serving the
	// protocol in 2022, but an older remote may still be written this way.
	GithubSpellingGit
)

// String is the Rust variant name, as `{:?}` prints it.
func (s GithubSpelling) String() string {
	switch s {
	case GithubSpellingShorthand:
		return "Shorthand"
	case GithubSpellingHTTPS:
		return "Https"
	case GithubSpellingHTTP:
		return "Http"
	case GithubSpellingScp:
		return "Scp"
	case GithubSpellingSSH:
		return "Ssh"
	case GithubSpellingGit:
		return "Git"
	}
	return fmt.Sprintf("GithubSpelling(%d)", int(s))
}

// GithubHost is which GitHub host a remote names.
type GithubHost int

const (
	// `github.com`, and shorthand, which means `github.com`.
	GithubHostGithubCom GithubHost = iota
	// `ssh.github.com`, GitHub's SSH endpoint on port 443.
	GithubHostSSHGithubCom
)

// String is the Rust variant name, as `{:?}` prints it.
func (h GithubHost) String() string {
	switch h {
	case GithubHostGithubCom:
		return "GithubCom"
	case GithubHostSSHGithubCom:
		return "SshGithubCom"
	}
	return fmt.Sprintf("GithubHost(%d)", int(h))
}

// GithubRemote is a remote that names a repository on GitHub. Owner and
// repository keep the case they were written in; callers that compare
// case-insensitively lower them.
type GithubRemote struct {
	Owner      string
	Repository string
	Spelling   GithubSpelling
	Host       GithubHost
	// A port written in the remote that is not the scheme's default. The
	// default ports of `http` and `https` are never reported.
	Port *uint16
}

// IsDefaultEndpoint is whether the remote names GitHub's main endpoint
// without an explicit port. `ssh://git@github.com:22/...` and
// `ssh.github.com` are the same repository over another route, and
// identities keep them apart, as they do for `git://`, which was never a
// GitHub identity.
func (r *GithubRemote) IsDefaultEndpoint() bool {
	return r.Host == GithubHostGithubCom && r.Port == nil && r.Spelling != GithubSpellingGit
}

// GithubSSHRewritePrefixes is the two SSH spellings Git can rewrite to HTTPS
// with `url.https://github.com/.insteadOf`.
var GithubSSHRewritePrefixes = [2]string{"git@github.com:", "ssh://git@github.com/"}

// UsesGithubSSHRewritePrefix is whether `remote` begins with one of
// GithubSSHRewritePrefixes, which is exactly when such a rewrite applies to
// it.
func UsesGithubSSHRewritePrefix(remote string) bool {
	for _, prefix := range GithubSSHRewritePrefixes {
		if strings.HasPrefix(remote, prefix) {
			return true
		}
	}
	return false
}

// ParseGithubRemote parses a GitHub remote: `owner/repository` (with
// ShorthandAllowed), `https://` and `http://` URLs for `github.com` (userinfo
// ignored), `[git@]github.com:owner/repository`, `ssh://[git@]github.com[:22]/...`,
// and `ssh://[git@]ssh.github.com:443/...`. The host is case-insensitive. A
// trailing `/` and one `.git` are removed. Other hosts, ports, schemes, SSH
// users, extra path segments, queries and fragments return nil, as do owner
// or repository names GitHub cannot have.
func ParseGithubRemote(source string, shorthand Shorthand) *GithubRemote {
	source = strings.TrimSpace(source)
	var (
		spelling GithubSpelling
		host     GithubHost
		port     *uint16
		path     string
	)
	if strings.Contains(source, "://") {
		url, ok := parseRustURL(source)
		if !ok || url.query != nil || url.fragment != nil || !url.hasHost {
			return nil
		}
		hostName := url.host
		switch url.scheme {
		case "https", "http":
			spelling = GithubSpellingHTTPS
			if url.scheme == "http" {
				spelling = GithubSpellingHTTP
			}
			if !isGithubWebHost(url) {
				return nil
			}
			host = GithubHostGithubCom
		case "ssh":
			if (url.username != "" && url.username != "git") || url.password != nil {
				return nil
			}
			var portOK bool
			switch {
			case strings.EqualFold(hostName, "github.com"):
				host = GithubHostGithubCom
				portOK = url.port == nil || *url.port == 22
			case strings.EqualFold(hostName, "ssh.github.com"):
				host = GithubHostSSHGithubCom
				portOK = url.port != nil && *url.port == 443
			default:
				return nil
			}
			if !portOK {
				return nil
			}
			spelling = GithubSpellingSSH
			port = url.port
		case "git":
			if !strings.EqualFold(hostName, "github.com") || url.port != nil || url.username != "" || url.password != nil {
				return nil
			}
			spelling, host = GithubSpellingGit, GithubHostGithubCom
		default:
			return nil
		}
		stripped, found := strings.CutPrefix(url.path, "/")
		if !found {
			return nil
		}
		path = stripped
	} else if authority, rest, found := strings.Cut(source, ":"); found {
		hostName := authority
		if at := strings.LastIndex(authority, "@"); at >= 0 {
			if authority[:at] != "git" {
				return nil
			}
			hostName = authority[at+1:]
		}
		// `ssh.github.com` is GitHub's SSH-over-443 endpoint; the person's
		// SSH configuration selects the port for this spelling.
		switch {
		case strings.EqualFold(hostName, "github.com"):
			host = GithubHostGithubCom
		case strings.EqualFold(hostName, "ssh.github.com"):
			host = GithubHostSSHGithubCom
		default:
			return nil
		}
		spelling, path = GithubSpellingScp, rest
	} else if shorthand == ShorthandAllowed && !strings.Contains(source, "@") {
		spelling, host, path = GithubSpellingShorthand, GithubHostGithubCom, source
	} else {
		return nil
	}

	path = strings.TrimRight(path, "/")
	path = strings.TrimSuffix(path, ".git")
	parts := strings.Split(path, "/")
	if len(parts) != 2 || !ValidGithubOwnerLogin(parts[0]) || !validGithubRepositoryName(parts[1]) {
		return nil
	}
	return &GithubRemote{Owner: parts[0], Repository: parts[1], Spelling: spelling, Host: host, Port: port}
}

// isGithubWebHost is whether `url` names GitHub's web host on its default
// port. Rust's `url::Url` drops a scheme's default port, so `:443` on https
// counts as no port.
func isGithubWebHost(url *rustURL) bool {
	return url.port == nil && url.hasHost && strings.EqualFold(url.host, "github.com")
}

// IsGithubHTTPSURL is whether `source` is an https URL on GitHub's web host,
// whatever its path. Authentication follows the server, so a caller deciding
// whether to send a GitHub token must not depend on the path being a
// repository name ParseGithubRemote accepts, such as one written with
// percent escapes.
func IsGithubHTTPSURL(source string) bool {
	url, ok := parseRustURL(strings.TrimSpace(source))
	return ok && url.scheme == "https" && isGithubWebHost(url)
}

// GithubTokenCredentialConfig is `git -c` arguments that answer credential
// requests for GitHub's web hosts with the token in the child's `GH_TOKEN`.
// Git matches a scoped helper against the server it actually contacts, so a
// host whose Git configuration rewrites github.com (`url.*.insteadOf`) to a
// mirror never sees the token, and the host's own helpers still answer for
// the mirror. The empty value first clears any host helper scoped to the
// same URL.
func GithubTokenCredentialConfig() []string {
	var args []string
	for _, host := range []string{"github.com", "gist.github.com"} {
		args = append(args,
			"-c", fmt.Sprintf("credential.https://%s.helper=", host),
			"-c", fmt.Sprintf(`credential.https://%s.helper=!f() { if [ "$1" = get ]; then printf '%%s\n' username=x-access-token "password=$GH_TOKEN"; fi; }; f`, host),
		)
	}
	return args
}

// GithubOwnerRepo is the owner and repository of a GitHub source, in the
// case written. Accepts the spellings ResolveRepository does, including
// `owner/repository`.
func GithubOwnerRepo(source string) (string, string, bool) {
	remote := ParseGithubRemote(source, ShorthandAllowed)
	if remote == nil {
		return "", "", false
	}
	return remote.Owner, remote.Repository, true
}

func validGithubRepositoryName(repository string) bool {
	if repository == "" || len(repository) > 100 || repository == "." || repository == ".." {
		return false
	}
	for i := 0; i < len(repository); i++ {
		b := repository[i]
		isAlnum := (b >= 'a' && b <= 'z') || (b >= 'A' && b <= 'Z') || (b >= '0' && b <= '9')
		if !isAlnum && b != '-' && b != '_' && b != '.' {
			return false
		}
	}
	return true
}

func networkGitOutput(output *CommandOutput) (*string, *string, error) {
	if !utf8.Valid(output.Stdout) {
		return nil, nil, errors.New("decode `git ls-remote` output: stream did not contain valid UTF-8")
	}
	var branch, commit *string
	for _, line := range rustLines(string(output.Stdout)) {
		line = strings.TrimSpace(line)
		if line == "" {
			continue
		}
		if rest, found := strings.CutPrefix(line, "ref:"); found {
			fields := strings.Fields(rest)
			if len(fields) == 2 && fields[1] == "HEAD" {
				candidate, found := strings.CutPrefix(fields[0], "refs/heads/")
				if !found {
					continue
				}
				previous := branch
				branch = &candidate
				if previous != nil {
					return nil, nil, errors.New("remote HEAD advertises multiple default branches")
				}
			}
			continue
		}
		fields := strings.Fields(line)
		if len(fields) == 2 && fields[1] == "HEAD" {
			previous := commit
			value := fields[0]
			commit = &value
			if previous != nil {
				return nil, nil, errors.New("remote HEAD advertises multiple commit ids")
			}
		}
	}
	return branch, commit, nil
}

// rustLines is Rust's `str::lines`: lines split on `\n`, each without a
// trailing `\r`, and no empty last line after a final newline.
func rustLines(text string) []string {
	if text == "" {
		return nil
	}
	lines := strings.Split(strings.TrimSuffix(text, "\n"), "\n")
	for i, line := range lines {
		lines[i] = strings.TrimSuffix(line, "\r")
	}
	return lines
}

func parseDefaultBranch(output *CommandOutput) (string, error) {
	branch, commit, err := networkGitOutput(output)
	if err != nil {
		return "", err
	}
	if branch == nil {
		return "", errors.New("remote did not advertise a symbolic HEAD branch")
	}
	if err := validateBranchName(*branch); err != nil {
		return "", err
	}
	if commit == nil {
		return "", errors.New("remote did not advertise a commit for HEAD")
	}
	if !IsObjectID(*commit) {
		return "", errors.New("remote advertised an invalid commit id for HEAD")
	}
	return *branch, nil
}

func validateBranchName(branch string) error {
	invalid := branch == "" ||
		strings.HasPrefix(branch, "-") ||
		strings.HasPrefix(branch, "/") ||
		strings.HasSuffix(branch, "/") ||
		strings.HasPrefix(branch, ".") ||
		strings.HasSuffix(branch, ".") ||
		strings.Contains(branch, "..") ||
		strings.Contains(branch, "//") ||
		strings.Contains(branch, "@{") ||
		branch == "@" ||
		strings.IndexFunc(branch, func(c rune) bool {
			return unicode.IsControl(c) || unicode.IsSpace(c) || strings.ContainsRune(`~^:?*[\`, c)
		}) >= 0
	if !invalid {
		for _, part := range strings.Split(branch, "/") {
			if part == "." || part == ".." || strings.HasSuffix(part, ".lock") {
				invalid = true
				break
			}
		}
	}
	if invalid {
		return errors.New("remote advertised an invalid default branch name")
	}
	return nil
}

// IsObjectID is whether value is a full SHA-1 or SHA-256 Git object id other
// than the null id.
func IsObjectID(value string) bool {
	if len(value) != 40 && len(value) != 64 {
		return false
	}
	nonzero := false
	for i := 0; i < len(value); i++ {
		b := value[i]
		if !((b >= '0' && b <= '9') || (b >= 'a' && b <= 'f') || (b >= 'A' && b <= 'F')) {
			return false
		}
		nonzero = nonzero || b != '0'
	}
	return nonzero
}

func isWindowsAbsolutePath(value string) bool {
	if len(value) < 3 {
		return false
	}
	first := value[0]
	return ((first >= 'a' && first <= 'z') || (first >= 'A' && first <= 'Z')) &&
		value[1] == ':' && (value[2] == '/' || value[2] == '\\')
}

func sanitizeDiagnostic(text string, source string) string {
	return strings.ReplaceAll(text, source, DisplayURL(source))
}

// rustURL is a URL as Rust's `url` crate (the WHATWG URL standard) parses
// it, for the parts this module reads: the scheme and host lowercased where
// the standard does, a scheme's default port dropped, dot segments resolved,
// and `Url::to_string`'s serialization. Go's net/url keeps the text as
// written and accepts relative references, so it cannot decide these
// questions the same way.
type rustURL struct {
	scheme   string
	username string
	password *string
	// hasAuthority is whether the serialization has `//`.
	hasAuthority bool
	hasHost      bool
	host         string
	port         *uint16
	path         string
	query        *string
	fragment     *string
}

func (u *rustURL) String() string {
	var text strings.Builder
	text.WriteString(u.scheme)
	text.WriteString(":")
	if u.hasAuthority {
		text.WriteString("//")
		if u.username != "" || u.password != nil {
			text.WriteString(u.username)
			if u.password != nil {
				text.WriteString(":")
				text.WriteString(*u.password)
			}
			text.WriteString("@")
		}
		text.WriteString(u.host)
		if u.port != nil {
			fmt.Fprintf(&text, ":%d", *u.port)
		}
	} else if strings.HasPrefix(u.path, "//") {
		text.WriteString("/.")
	}
	text.WriteString(u.path)
	if u.query != nil {
		text.WriteString("?")
		text.WriteString(*u.query)
	}
	if u.fragment != nil {
		text.WriteString("#")
		text.WriteString(*u.fragment)
	}
	return text.String()
}

func parseRustURL(input string) (*rustURL, bool) {
	url, err := parseRustURLWithError(input)
	return url, err == nil
}

// URL is a URL as Rust's `url::Url::parse` reads it and `Url::to_string`
// writes it. Crates above core that call the url crate directly (the
// checkpoint archive's origin redaction) use it, so every port of url::Url
// shares the one parser above.
type URL struct{ parsed *rustURL }

// ParseURL is `url::Url::parse`; ok is false for text the url crate rejects.
func ParseURL(text string) (URL, bool) {
	parsed, ok := parseRustURL(text)
	if !ok {
		return URL{}, false
	}
	return URL{parsed: parsed}, true
}

// Scheme is `Url::scheme`, lowercased as the standard does.
func (u URL) Scheme() string { return u.parsed.scheme }

// Username is `Url::username`, percent-encoded; empty when there is none.
func (u URL) Username() string { return u.parsed.username }

// Password is `Url::password`; an empty password is no password.
func (u URL) Password() (string, bool) {
	if u.parsed.password == nil {
		return "", false
	}
	return *u.parsed.password, true
}

// WithoutCredentials is the URL after `set_username("")` and
// `set_password(None)`. ok is false where the url crate refuses both: a URL
// without a host, with an empty host, or with the file scheme.
func (u URL) WithoutCredentials() (URL, bool) {
	if !u.parsed.hasHost || u.parsed.host == "" || u.parsed.scheme == "file" {
		return URL{}, false
	}
	redacted := *u.parsed
	redacted.username = ""
	redacted.password = nil
	return URL{parsed: &redacted}, true
}

// WithoutPassword is the URL after `set_password(None)`. ok is false where
// the url crate refuses it: a URL without a host, with an empty host, or
// with the file scheme.
func (u URL) WithoutPassword() (URL, bool) {
	if !u.parsed.hasHost || u.parsed.host == "" || u.parsed.scheme == "file" {
		return URL{}, false
	}
	redacted := *u.parsed
	redacted.password = nil
	return URL{parsed: &redacted}, true
}

// String is `Url::to_string`.
func (u URL) String() string { return u.parsed.String() }

// rustURLDefaultPorts is the WHATWG special schemes and their default ports;
// `file` is special and has none.
var rustURLDefaultPorts = map[string]uint16{"http": 80, "https": 443, "ws": 80, "wss": 443, "ftp": 21}

func rustURLIsSpecial(scheme string) bool {
	_, ok := rustURLDefaultPorts[scheme]
	return ok || scheme == "file"
}

// parseRustURLWithError parses an absolute URL as the `url` crate does,
// returning its error text (`relative URL without a base`, `empty host`,
// `invalid port number`, `invalid domain character`) on failure.
func parseRustURLWithError(input string) (*rustURL, error) {
	// Leading and trailing C0 controls and spaces are ignored; tabs and
	// newlines anywhere are removed.
	input = strings.TrimFunc(input, func(c rune) bool { return c <= ' ' })
	input = strings.Map(func(c rune) rune {
		if c == '\t' || c == '\n' || c == '\r' {
			return -1
		}
		return c
	}, input)

	colon := strings.Index(input, ":")
	if colon <= 0 || !isASCIIAlpha(input[0]) {
		return nil, errors.New("relative URL without a base")
	}
	for i := 1; i < colon; i++ {
		c := input[i]
		if !isASCIIAlpha(c) && !(c >= '0' && c <= '9') && c != '+' && c != '-' && c != '.' {
			return nil, errors.New("relative URL without a base")
		}
	}
	url := &rustURL{scheme: asciiLowercase(input[:colon])}
	rest := input[colon+1:]
	special := rustURLIsSpecial(url.scheme)

	if fragment := strings.Index(rest, "#"); fragment >= 0 {
		value := percentEncodeURL(rest[fragment+1:], urlFragmentSet)
		url.fragment = &value
		rest = rest[:fragment]
	}
	if query := strings.Index(rest, "?"); query >= 0 {
		set := urlQuerySet
		if special {
			set = urlSpecialQuerySet
		}
		value := percentEncodeURL(rest[query+1:], set)
		url.query = &value
		rest = rest[:query]
	}
	if special {
		rest = strings.ReplaceAll(rest, `\`, "/")
	}

	switch {
	case special && url.scheme != "file":
		// A special URL always has an authority, however many slashes
		// precede it.
		rest = strings.TrimLeft(rest, "/")
		authority, path := rest, ""
		if slash := strings.Index(rest, "/"); slash >= 0 {
			authority, path = rest[:slash], rest[slash:]
		}
		if err := url.parseAuthority(authority, true); err != nil {
			return nil, err
		}
		if !url.hasHost {
			return nil, errors.New("empty host")
		}
		url.path = resolveURLPath(path, true)
	case strings.HasPrefix(rest, "//"):
		rest = rest[2:]
		authority, path := rest, ""
		if slash := strings.Index(rest, "/"); slash >= 0 {
			authority, path = rest[:slash], rest[slash:]
		}
		if url.scheme == "file" {
			url.hasAuthority = true
			url.host = asciiLowercase(authority)
			url.hasHost = authority != ""
			url.path = resolveURLPath(path, true)
			break
		}
		if err := url.parseAuthority(authority, false); err != nil {
			return nil, err
		}
		if path != "" {
			url.path = resolveURLPath(path, false)
		}
	case strings.HasPrefix(rest, "/"):
		url.path = resolveURLPath(rest, special)
		if url.scheme == "file" {
			url.hasAuthority = true
		}
	default:
		if url.scheme == "file" {
			url.hasAuthority = true
			url.path = resolveURLPath("/"+rest, true)
			break
		}
		// A cannot-be-a-base URL keeps its opaque path.
		url.path = percentEncodeURL(rest, urlC0ControlSet)
	}
	return url, nil
}

// parseAuthority reads `[userinfo@]host[:port]`.
func (u *rustURL) parseAuthority(authority string, special bool) error {
	u.hasAuthority = true
	hostPort := authority
	if at := strings.LastIndex(authority, "@"); at >= 0 {
		userinfo := authority[:at]
		hostPort = authority[at+1:]
		username, password, hasPassword := strings.Cut(userinfo, ":")
		u.username = percentEncodeURL(username, urlUserinfoSet)
		if hasPassword {
			encoded := percentEncodeURL(password, urlUserinfoSet)
			if encoded != "" {
				u.password = &encoded
			}
		}
		if hostPort == "" {
			return errors.New("empty host")
		}
	}
	host, portText := hostPort, ""
	hasPort := false
	if strings.HasPrefix(hostPort, "[") {
		end := strings.Index(hostPort, "]")
		if end < 0 {
			return errors.New("invalid IPv6 address")
		}
		host = hostPort[:end+1]
		if remainder := hostPort[end+1:]; remainder != "" {
			if !strings.HasPrefix(remainder, ":") {
				return errors.New("invalid IPv6 address")
			}
			portText, hasPort = remainder[1:], true
		}
	} else if colon := strings.LastIndex(hostPort, ":"); colon >= 0 {
		host, portText, hasPort = hostPort[:colon], hostPort[colon+1:], true
	}
	if host == "" {
		if hasPort || special {
			return errors.New("empty host")
		}
		return nil
	}
	ipv6 := strings.HasPrefix(host, "[")
	for _, c := range host {
		forbidden := c < 0x20 || c == 0x7F || strings.ContainsRune(" #/:<>?@[\\]^|", c) || (special && c == '%')
		if forbidden && !(ipv6 && (c == '[' || c == ']' || c == ':')) {
			return errors.New("invalid domain character")
		}
	}
	if special {
		host = strings.ToLower(host)
	}
	u.host, u.hasHost = host, true
	if hasPort && portText != "" {
		for i := 0; i < len(portText); i++ {
			if portText[i] < '0' || portText[i] > '9' {
				return errors.New("invalid port number")
			}
		}
		port, err := strconv.ParseUint(portText, 10, 16)
		if err != nil {
			return errors.New("invalid port number")
		}
		value := uint16(port)
		if defaultPort, ok := rustURLDefaultPorts[u.scheme]; !ok || defaultPort != value {
			u.port = &value
		}
	}
	return nil
}

// resolveURLPath percent-encodes a path that starts with `/` (or is empty)
// and resolves its `.` and `..` segments. A special URL's empty path is `/`.
func resolveURLPath(path string, special bool) string {
	if path == "" {
		if special {
			return "/"
		}
		return ""
	}
	segments := strings.Split(path[1:], "/")
	var resolved []string
	for i, segment := range segments {
		last := i == len(segments)-1
		lowered := asciiLowercase(segment)
		switch lowered {
		case "..", ".%2e", "%2e.", "%2e%2e":
			if len(resolved) > 0 {
				resolved = resolved[:len(resolved)-1]
			}
			if last {
				resolved = append(resolved, "")
			}
		case ".", "%2e":
			if last {
				resolved = append(resolved, "")
			}
		default:
			resolved = append(resolved, percentEncodeURL(segment, urlPathSet))
		}
	}
	return "/" + strings.Join(resolved, "/")
}

func isASCIIAlpha(c byte) bool { return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') }

// The WHATWG percent-encode sets, beyond C0 controls and non-ASCII bytes.
const (
	urlC0ControlSet    = ""
	urlFragmentSet     = " \"<>`"
	urlQuerySet        = " \"#<>"
	urlSpecialQuerySet = " \"#<>'"
	urlPathSet         = " \"#<>?`{}"
	urlUserinfoSet     = " \"#<>?`{}/:;=@[\\]^|"
)

func percentEncodeURL(text, set string) string {
	var encoded strings.Builder
	for i := 0; i < len(text); i++ {
		c := text[i]
		if c < 0x20 || c >= 0x7F || strings.IndexByte(set, c) >= 0 {
			fmt.Fprintf(&encoded, "%%%02X", c)
			continue
		}
		encoded.WriteByte(c)
	}
	return encoded.String()
}
