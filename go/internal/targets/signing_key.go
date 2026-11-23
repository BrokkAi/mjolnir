// Port of mj-targets/src/signing_key.rs.
//
// Process-wide key material: the TLS provider every executable installs,
// and the signing key that phone cookies and AWS credential scopes derive
// from.

package targets

import (
	"crypto/rand"
	"fmt"
	"path/filepath"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// InstallRustlsCryptoProvider selects the process-wide TLS provider before
// any TLS configuration is built.
//
// Rust needs this because dependency feature unification can enable both
// rustls providers. Go's crypto/tls has a single provider, so there is
// nothing to select; the function remains so startup reads as in Rust.
func InstallRustlsCryptoProvider() {}

const CookieKeyBytes = 32
const cookieKeyFile = "phone-cookie-key"

func CookieKeyPath() string {
	return filepath.Join(core.DataDir(), cookieKeyFile)
}

// LoadOrCreateCookieKey loads the phone cookie signing key, creating it on
// first use.
//
// This signing key keeps a signed-in phone signed in across daemon restarts.
// Logged-out identities are tracked separately. Deleting the key is
// therefore the explicit sign-everyone-out gesture: the next start writes a
// new key and every outstanding cookie stops validating. A missing file is
// ordinary first use; an unreadable or too-short one is replaced loudly,
// because refusing to start would be a worse answer than asking phones to
// enter the viewer code again.
//
// The key has one creator: a file lock beside it serializes every caller in
// every process, and the key is re-read under the lock, so concurrent first
// uses (the phone server starting while sessions launch, parallel renewals)
// all end with the key the first of them wrote. The key file itself is
// replaced atomically, so the unlocked read below never sees a partial key.
func LoadOrCreateCookieKey(path string) ([]byte, error) {
	panic("unported: mj-targets/src/signing_key.rs load_or_create_cookie_key")
}

// lockCookieKey is the lock that makes one caller the creator of the key at
// path.
func lockCookieKey(path string) (*core.FileLock, error) {
	panic("unported: mj-targets/src/signing_key.rs lock_cookie_key")
}

func GenerateCookieKey() ([CookieKeyBytes]byte, error) {
	var key [CookieKeyBytes]byte
	if _, err := rand.Read(key[:]); err != nil {
		return key, fmt.Errorf("generate Mjolnir cookie key: %w", err)
	}
	return key, nil
}
