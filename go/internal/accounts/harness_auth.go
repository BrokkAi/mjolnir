// Port of mj-accounts/src/harness_auth.rs.

// Whether a harness profile can reach its service without a login first,
// and Claude Code's login in the macOS Keychain. Discovery, quota probes and
// login staging all ask here.

package accounts

import (
	"log/slog"
	"os"
	"path/filepath"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/harnessinstall"
)

func HarnessIsAuthenticatedWithExecutor(profile *core.HarnessProfile, executor core.CommandExecutor) bool {
	isDefaultHome := false
	if userHome, ok := core.UserHome(); ok {
		isDefaultHome = filepath.Clean(profile.Home) == filepath.Join(userHome, profile.Kind.DefaultHomeLeaf())
	}
	return HarnessIsAuthenticatedWith(profile, isDefaultHome, executor)
}

// HarnessIsAuthenticatedWith is whether this profile can talk to its
// service without a login first.
//
// An API-key profile is proven by its harness configuration file, because
// its key lives in the profile's `environment` rather than in a credential
// file; HarnessProfile.AuthenticationMarker already names the right file
// for either case. A harness without a login or a configuration file, such
// as DSH, is proven by its key alone.
func HarnessIsAuthenticatedWith(profile *core.HarnessProfile, isDefaultHome bool, executor core.CommandExecutor) bool {
	panic("unported: mj-accounts/src/harness_auth.rs harness_is_authenticated_with")
}

// ClaudeCLIReportsAuthenticated asks Claude Code about a scoped profile.
// Setting `CLAUDE_CONFIG_DIR` for the default home changes Claude's profile
// selection, so the default home is never asked.
func ClaudeCLIReportsAuthenticated(home string, executor core.CommandExecutor) bool {
	panic("unported: mj-accounts/src/harness_auth.rs claude_cli_reports_authenticated")
}

// ClaudeKeychainReportsAuthenticated is whether the Keychain holds a Claude
// login. `security` is already authorized for the item, so this does not
// raise a Keychain prompt; the shared executor still bounds a wedged
// lookup.
func ClaudeKeychainReportsAuthenticated(executor core.CommandExecutor) bool {
	credentials, err := ClaudeKeychainCredentials(executor)
	return err == nil && credentials != nil && ClaudeCredentialsContainLogin(credentials)
}

// KeychainLogin is what the macOS Keychain said about Claude Code's login:
// KeychainLoginMissing, KeychainLoginUnreadable or KeychainLoginPresent.
type KeychainLogin interface{ isKeychainLogin() }

// KeychainLoginMissing: no such item (`security` exit 44).
type KeychainLoginMissing struct{}

// KeychainLoginUnreadable: the item could not be read: the Keychain is
// locked (exit 36) or `security` failed another way. Claude Code reads that
// as no login, but nobody should act on it as if the login were gone.
type KeychainLoginUnreadable struct{ Status int }

// KeychainLoginPresent is the login, which the caller owns and must not
// log.
type KeychainLoginPresent struct{ Login []byte }

func (KeychainLoginMissing) isKeychainLogin()    {}
func (KeychainLoginUnreadable) isKeychainLogin() {}
func (KeychainLoginPresent) isKeychainLogin()    {}

// ClaudeKeychainAccount is the account of Claude Code's Keychain item, by
// Claude Code's own rule. The reader and the writer
// (WriteClaudeKeychainLogin) share it, so both address the item Claude Code
// uses.
func ClaudeKeychainAccount() string {
	// Claude Code takes `$USER || os.userInfo().username`.
	if user := os.Getenv("USER"); user != "" {
		return core.ClaudeKeychainAccount(&user)
	}
	if user, err := harnessinstall.AccountName(); err == nil {
		return core.ClaudeKeychainAccount(&user)
	}
	return core.ClaudeKeychainAccount(nil)
}

// KeychainReadTimeout is how long a `security` lookup of the Keychain login
// may take before it is treated as wedged (it can wait on a Keychain
// prompt). The same bound as the other host credential lookups (the AWS
// export).
const KeychainReadTimeout = 30 * time.Second

// securityItemNotFound is the `security` exit status for "item not found".
const securityItemNotFound = 44

// ReadClaudeKeychainLogin reads Claude Code's login from the macOS Keychain
// item every profile shares (core.ClaudeKeychainService). This is the one
// reader; the callers differ only in what an unreadable Keychain means to
// them. It fails only when `security` cannot run or the item holds no
// usable login.
func ReadClaudeKeychainLogin(executor core.CommandExecutor) (KeychainLogin, error) {
	panic("unported: mj-accounts/src/harness_auth.rs read_claude_keychain_login")
}

// ClaudeKeychainCredentials is Claude Code's Keychain login for a reader
// that, like Claude Code, treats an unreadable Keychain as no login: staging
// a copy and the authentication check. Nil when there is none. The caller
// owns the returned copy and must not log it.
func ClaudeKeychainCredentials(executor core.CommandExecutor) ([]byte, error) {
	login, err := ReadClaudeKeychainLogin(executor)
	if err != nil {
		return nil, err
	}
	switch login := login.(type) {
	case KeychainLoginPresent:
		return login.Login, nil
	case KeychainLoginUnreadable:
		slog.Warn("the macOS Keychain could not be read; Claude Code's login is treated as absent", "status", login.Status)
	}
	return nil, nil
}

func ClaudeCredentialsContainLogin(credentials []byte) bool {
	panic("unported: mj-accounts/src/harness_auth.rs claude_credentials_contain_login")
}
