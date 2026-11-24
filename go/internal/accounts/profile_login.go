// Port of mj-accounts/src/profile_login.rs.

// Read-only sign-in status for configured profiles.
//
// This is deliberately separate from capability probing. A model probe
// answers what the harness offers after it starts; this answers whether a
// credential is present before anyone starts it. Settings shows the answer
// beside each profile so a signed-out profile is visible before a session
// fails.

package accounts

import (
	"encoding/json"
	"fmt"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ProfileLoginStatuses is the status of every profile in config, keyed by
// profile id. Each profile is probed concurrently under its own 10 s
// deadline (Rust: `profile_login::statuses`).
func ProfileLoginStatuses(config *core.Config, cancelled *atomic.Bool) map[string]core.LoginStatus {
	type probed struct {
		id     string
		status core.LoginStatus
	}
	results := make(chan probed, len(config.Profiles))
	var group sync.WaitGroup
	for id, profile := range config.Profiles {
		group.Add(1)
		go func() {
			defer group.Done()
			executor := core.NewCancellableProcessExecutor(cancelled).
				WithDeadline(10 * time.Second)
			result := ProfileLoginStatus(id, &profile, executor)
			if executor.IsCancelled() {
				result = core.LoginStatusUnknown{Detail: fmt.Sprintf(
					"profile %s login probe cancelled or exceeded its 10 s deadline", id)}
			}
			results <- probed{id: id, status: result}
		}()
	}
	group.Wait()
	close(results)
	statuses := make(map[string]core.LoginStatus, len(config.Profiles))
	for result := range results {
		statuses[result.id] = result.status
	}
	return statuses
}

// ProfileLoginStatus is whether profile has a usable harness login or needs
// no interactive login (Rust: `profile_login::status`).
func ProfileLoginStatus(profileID string, profile *core.HarnessProfile, executor core.CommandExecutor) core.LoginStatus {
	// A Claude setup token is Mjolnir-owned and does not live in the harness
	// home, so check it before the ordinary login file. A token supplied by
	// the profile's own environment is a login too.
	if profile.Kind == core.HarnessKindClaude {
		token, ok := profile.Environment.Get("CLAUDE_CODE_OAUTH_TOKEN")
		if harnessAuthIsFile(core.ClaudeOauthTokenPath(profileID)) ||
			(ok && strings.TrimSpace(token) != "") {
			return core.LoginStatusSignedIn{}
		}
	}
	switch scheme := profile.AuthScheme().(type) {
	case core.AuthSchemeApiKey:
		if profile.SuppliesAPIKey(scheme.EnvKey) {
			return core.LoginStatusNotRequired{Detail: scheme.EnvKey + " API key"}
		}
		return core.LoginStatusNotRequired{Detail: scheme.EnvKey + " missing"}
	case core.AuthSchemeInlineApiKey:
		return core.LoginStatusNotRequired{Detail: "inline API key"}
	case core.AuthSchemeAwsCredentialChain:
		return core.LoginStatusNotRequired{Detail: "AWS credentials"}
	case core.AuthSchemeNoAuthentication:
		return core.LoginStatusNotRequired{Detail: "no login required"}
	case core.AuthSchemeNativeLoginStore:
		logins, err := ProfileLogins(profile)
		switch {
		case err != nil:
			return core.LoginStatusUnknown{Detail: fmt.Sprintf("could not read OpenCode logins: %v", err)}
		case logins != nil:
			return core.LoginStatusSignedIn{}
		case len(profile.OpencodeEnvironmentLogins()) > 0:
			return core.LoginStatusSignedIn{}
		default:
			return core.LoginStatusSignedOut{}
		}
	}
	// AuthScheme::NativeLogin.
	//
	// On a Mac the Keychain item is the only Claude login; a
	// `.credentials.json` there (a profile home's, or Claude Code's
	// plaintext fallback) is not one. A Mac without the item is signed out,
	// and `mj login` fixes it.
	if _, keychain := profile.Kind.LoginStore(core.HarnessHostCurrent(), profile.Home).(core.HarnessLoginStoreMacKeychain); keychain {
		login, err := ReadClaudeKeychainLogin(executor)
		if err != nil {
			return core.LoginStatusUnknown{Detail: fmt.Sprintf(
				"could not read the Claude login from the macOS Keychain: %v", err)}
		}
		switch login := login.(type) {
		case KeychainLoginMissing:
			return core.LoginStatusSignedOut{}
		case KeychainLoginUnreadable:
			return core.LoginStatusUnknown{Detail: fmt.Sprintf(
				"the macOS Keychain could not be read (security exited with status %d)", login.Status)}
		default:
			return core.LoginStatusSignedIn{}
		}
	}
	if !HarnessIsAuthenticatedWithExecutor(profile, executor) {
		return core.LoginStatusSignedOut{}
	}
	// Authenticated, but a recorded access-token expiry is worth calling
	// out. Harnesses with no expiry record simply stay "signed in".
	snapshot, bytes, err := core.ReadCredentialFile(profile.Kind, profile.AuthenticationMarker())
	if err != nil {
		return core.LoginStatusUnknown{Detail: fmt.Sprintf("could not read credentials: %v", err)}
	}
	if !snapshot.Present {
		return core.LoginStatusSignedIn{}
	}
	if !json.Valid(bytes) {
		return core.LoginStatusUnknown{Detail: "the credential file is not valid JSON"}
	}
	expiry, ok := core.CredentialExpiry(profile.Kind, bytes)
	if !ok && profile.Kind == core.HarnessKindGrok {
		expiry, ok = core.CredentialFreshness(profile.Kind, bytes)
	}
	if ok && expiry <= core.EpochMillis() {
		return core.LoginStatusExpired{}
	}
	return core.LoginStatusSignedIn{}
}
