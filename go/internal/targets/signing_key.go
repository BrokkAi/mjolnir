// Port of mj-targets/src/signing_key.rs.
//
// Process-wide key material: the TLS provider every executable installs,
// and the signing key that phone cookies and AWS credential scopes derive
// from.

package targets

import (
	"crypto/rand"
	"errors"
	"fmt"
	"io/fs"
	"log/slog"
	"os"
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
	if key, err := os.ReadFile(path); err == nil && len(key) >= CookieKeyBytes {
		return key, nil
	}
	creating, err := lockCookieKey(path)
	if err != nil {
		return nil, err
	}
	defer creating.Drop()
	key, err := os.ReadFile(path)
	switch {
	case err == nil && len(key) >= CookieKeyBytes:
		return key, nil
	case err == nil:
		slog.Warn(fmt.Sprintf("phone cookie key is shorter than %d bytes; generating a new key signs every phone out", CookieKeyBytes),
			"path", path, "bytes", len(key))
	case errors.Is(err, fs.ErrNotExist):
	default:
		slog.Warn(fmt.Sprintf("could not read the phone cookie key (%v); generating a new key signs every phone out", err),
			"path", path)
	}
	generated, err := GenerateCookieKey()
	if err != nil {
		return nil, err
	}
	if err := core.AtomicWrite(path, generated[:]); err != nil {
		return nil, fmt.Errorf("persist Mjolnir phone cookie key %s: %w", path, err)
	}
	return generated[:], nil
}

// lockCookieKey is the lock that makes one caller the creator of the key at
// path.
func lockCookieKey(path string) (*core.FileLock, error) {
	lockPath := path + ".lock"
	if parent := filepath.Dir(lockPath); parent != "" && parent != "." {
		if err := os.MkdirAll(parent, 0o777); err != nil {
			return nil, fmt.Errorf("create phone cookie key directory %s: %w", parent, err)
		}
	}
	file, err := os.OpenFile(lockPath, os.O_CREATE|os.O_RDWR, 0o600)
	if err != nil {
		return nil, fmt.Errorf("open phone cookie key lock %s: %w", lockPath, err)
	}
	if err := libLockFile(file); err != nil {
		_ = file.Close()
		return nil, fmt.Errorf("lock phone cookie key %s: %w", path, err)
	}
	return core.NewFileLock(file), nil
}

func GenerateCookieKey() ([CookieKeyBytes]byte, error) {
	var key [CookieKeyBytes]byte
	if _, err := rand.Read(key[:]); err != nil {
		return key, fmt.Errorf("generate Mjolnir cookie key: %w", err)
	}
	return key, nil
}
