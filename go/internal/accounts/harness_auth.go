// Port of mj-accounts/src/harness_auth.rs.

// Whether a harness profile can reach its service without a login first,
// and Claude Code's login in the macOS Keychain. Discovery, quota probes and
// login staging all ask here.

package accounts

import (
	"encoding/json"
	"fmt"
	"log/slog"
	"os"
	"path/filepath"
	"strings"
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
	kind := profile.Kind
	home := profile.Home
	switch profile.AuthScheme().(type) {
	case core.AuthSchemeAwsCredentialChain:
		// AWS credentials can come from a role or an external credential
		// process; there is no harness login file. Doctor probes the chain
		// separately.
		return true
	case core.AuthSchemeNativeLoginStore:
		// OpenCode keeps its logins in its own database, or takes a provider
		// key from its environment; doctor reports a database it cannot read.
		logins, err := ProfileLogins(profile)
		return (err == nil && logins != nil) || len(profile.OpencodeEnvironmentLogins()) > 0
	}
	// A harness without a login reads its key from its environment, so no
	// file can prove it; the key itself does.
	if envKey, ok := kind.APIKeyVariable(); ok {
		return profile.SuppliesAPIKey(envKey)
	}
	// On a Mac the Keychain item is the only Claude login. A
	// `.credentials.json` there (a profile home's, left by earlier releases,
	// or Claude Code's plaintext fallback) is not one, so the marker proves
	// nothing, and every profile shares the item: asking the CLI about a
	// scoped home would only report the default profile's state again.
	if _, keychain := kind.LoginStore(core.HarnessHostCurrent(), home).(core.HarnessLoginStoreMacKeychain); keychain {
		return ClaudeKeychainReportsAuthenticated(executor)
	}
	if harnessAuthIsFile(profile.AuthenticationMarker()) ||
		(kind == core.HarnessKindKimi && harnessAuthIsFile(filepath.Join(home, "credentials"))) {
		return true
	}
	if kind != core.HarnessKindClaude {
		return false
	}
	// The default home on a host without a Keychain has no marker, so there
	// is no login: setting `CLAUDE_CONFIG_DIR` for the default home would
	// change Claude's profile selection.
	if isDefaultHome {
		return false
	}
	return ClaudeCLIReportsAuthenticated(home, executor)
}

// harnessAuthIsFile is Rust's Path::is_file: the path exists, following symlinks, and
// is a regular file.
func harnessAuthIsFile(path string) bool {
	info, err := os.Stat(path)
	return err == nil && info.Mode().IsRegular()
}

// ClaudeCLIReportsAuthenticated asks Claude Code about a scoped profile.
// Setting `CLAUDE_CONFIG_DIR` for the default home changes Claude's profile
// selection, so the default home is never asked.
func ClaudeCLIReportsAuthenticated(home string, executor core.CommandExecutor) bool {
	command := core.NewCommandSpec("claude", []string{"auth", "status", "--json"}).
		WithPurpose("check Claude Code authentication")
	command.Env[core.HarnessKindClaude.HomeEnv()] = home
	output, err := executor.Execute(&command)
	if err != nil || output.Status != 0 {
		return false
	}
	var status any
	if json.Unmarshal(output.Stdout, &status) != nil {
		return false
	}
	loggedIn, ok := harnessAuthJSONPointer(status, "loggedIn").(bool)
	return ok && loggedIn
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
	command := core.NewCommandSpec("security", []string{
		"find-generic-password",
		"-a",
		ClaudeKeychainAccount(),
		"-s",
		core.ClaudeKeychainService,
		"-w",
	}).WithPurpose("read Claude Code authentication from the macOS Keychain")
	command.SensitiveOutput = true
	output, err := executor.Execute(&command)
	if err != nil {
		return nil, fmt.Errorf("read Claude Code's login from the macOS Keychain: %w", err)
	}
	if output.Status == securityItemNotFound {
		return KeychainLoginMissing{}, nil
	}
	if output.Status != 0 {
		return KeychainLoginUnreadable{Status: output.Status}, nil
	}
	if !ClaudeCredentialsContainLogin(output.Stdout) {
		return nil, fmt.Errorf("the macOS Keychain item \"%s\" does not contain a usable Claude login", core.ClaudeKeychainService)
	}
	return KeychainLoginPresent{Login: output.Stdout}, nil
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
	var document any
	if json.Unmarshal(credentials, &document) != nil {
		return false
	}
	for _, pointer := range [][]string{
		{"claudeAiOauth", "accessToken"},
		{"claudeAiOauth", "refreshToken"},
		{"oauth", "accessToken"},
		{"apiKey"},
	} {
		if value, ok := harnessAuthJSONPointer(document, pointer...).(string); ok && strings.TrimSpace(value) != "" {
			return true
		}
	}
	return false
}

// harnessAuthJSONPointer follows object keys through a decoded JSON document, as
// serde_json's Value::pointer does for a path of object keys. Nil when a
// step is missing or not an object.
func harnessAuthJSONPointer(document any, keys ...string) any {
	for _, key := range keys {
		object, ok := document.(map[string]any)
		if !ok {
			return nil
		}
		document = object[key]
	}
	return document
}
