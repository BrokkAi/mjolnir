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
	"errors"
	"fmt"
	"net/url"
	"strings"
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
	type wire ExactCheckout
	return unmarshalDenyUnknown(data, (*wire)(c))
}

func (c *ExactCheckout) Validate() error {
	panic("unported: mj-core/src/remote_git.rs ExactCheckout::validate")
}

// NetworkGitSource is the network endpoints a target checkout should use for
// Git fetches and pushes. The first push URL is the primary destination; all
// configured push URLs are retained because Git intentionally supports
// pushing to several destinations.
type NetworkGitSource struct {
	FetchURL string   `json:"fetch_url"`
	PushURLs []string `json:"push_urls"`
}

// ResolveRepository resolves a configured repository without contacting its
// server.
//
// GitHub's `owner/repository` shorthand is expanded to the HTTPS clone URL.
// The default branch is intentionally not inferred here; callers that need
// it must make the separate DefaultBranch request.
func ResolveRepository(repository *ProjectRepository, executor CommandExecutor) (NetworkGitSource, error) {
	panic("unported: mj-core/src/remote_git.rs resolve_repository")
}

// DefaultBranch asks the configured server for the branch its symbolic
// `HEAD` names.
//
// This is the only network operation in this module. The supplied executor
// owns its timeout, cancellation, and subprocess supervision; this function
// only makes the command noninteractive and parses its bounded result.
func DefaultBranch(source *NetworkGitSource, executor CommandExecutor) (string, error) {
	panic("unported: mj-core/src/remote_git.rs default_branch")
}

// ValidateNetworkURL validates that a Git endpoint uses a network transport.
//
// Git accepts local paths and several helper transports anywhere a remote
// URL is expected. They would make a target checkout depend on the
// controller's filesystem or execute an unbounded helper, so they are
// rejected here.
func ValidateNetworkURL(url string) error {
	panic("unported: mj-core/src/remote_git.rs validate_network_url")
}

// DisplayURL returns a URL suitable for diagnostics or persistence in
// user-visible state. Userinfo is removed from both URL and scp-style SSH
// spellings.
func DisplayURL(url string) string {
	panic("unported: mj-core/src/remote_git.rs display_url")
}

func expandGithubSource(source string) (string, error) {
	panic("unported: mj-core/src/remote_git.rs expand_github_source")
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

// GithubHost is which GitHub host a remote names.
type GithubHost int

const (
	// `github.com`, and shorthand, which means `github.com`.
	GithubHostGithubCom GithubHost = iota
	// `ssh.github.com`, GitHub's SSH endpoint on port 443.
	GithubHostSSHGithubCom
)

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
	panic("unported: mj-core/src/remote_git.rs parse_github_remote")
}

// isGithubWebHost is whether `url` names GitHub's web host on its default
// port. Rust's `url::Url` drops a scheme's default port, so `:443` on https
// counts as no port.
func isGithubWebHost(u *url.URL) bool {
	panic("unported: mj-core/src/remote_git.rs is_github_web_host")
}

// IsGithubHTTPSURL is whether `source` is an https URL on GitHub's web host,
// whatever its path. Authentication follows the server, so a caller deciding
// whether to send a GitHub token must not depend on the path being a
// repository name ParseGithubRemote accepts, such as one written with
// percent escapes.
func IsGithubHTTPSURL(source string) bool {
	panic("unported: mj-core/src/remote_git.rs is_github_https_url")
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
	panic("unported: mj-core/src/remote_git.rs network_git_output")
}

func parseDefaultBranch(output *CommandOutput) (string, error) {
	panic("unported: mj-core/src/remote_git.rs parse_default_branch")
}

func validateBranchName(branch string) error {
	panic("unported: mj-core/src/remote_git.rs validate_branch_name")
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
