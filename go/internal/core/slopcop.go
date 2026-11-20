// Port of mj-core/src/slopcop.rs.

// The slopcop login a session borrows from the person running Mjolnir.
//
// slopcop is the CLI for the hosted SlopCop check service. A session that
// cannot read the host's own login (see
// CredentialSyncTarget.SyncsHostLogins) gets a copy in its staged harness
// home as LoginFile: written at launch, kept current by credential sync, and
// exported by the session's `slopcop` wrapper on every invocation. Mjolnir
// reads slopcop's own configuration and never writes it.

package core

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"strings"
	"unicode"
)

// LoginFile is a session's copy of the login, relative to its harness home.
const LoginFile = "slopcop-login"

// TokenEnv and ServiceEnv are the variables slopcop reads ahead of its
// stored configuration.
const TokenEnv = "SLOPCOP_TOKEN"
const ServiceEnv = "SLOPCOP_SERVICE"

const maxTokenBytes = 4 * 1024
const maxServiceURLBytes = 2 * 1024

// maxConfigBytes: slopcop's stored configuration holds a URL and a token;
// anything larger is not that file.
const maxConfigBytes int64 = 64 * 1024

// MaxLoginBytes is the encoded login: the token line, the service URL line,
// and newlines.
const MaxLoginBytes = maxTokenBytes + maxServiceURLBytes + 2

// SlopcopLogin is the token and, when it is not slopcop's default, the
// service URL.
type SlopcopLogin struct {
	token      string
	serviceURL *string
}

// GoString keeps the token out of debug output (Rust's Debug impl).
func (l SlopcopLogin) GoString() string {
	return fmt.Sprintf("SlopcopLogin { token: \"<redacted>\", service_url: %s }", agentDebugOption(l.serviceURL))
}

// String keeps the token out of formatted output.
func (l SlopcopLogin) String() string {
	return l.GoString()
}

// agentDebugOption formats an optional string the way Rust's Debug does.
func agentDebugOption(value *string) string {
	if value == nil {
		return "None"
	}
	return fmt.Sprintf("Some(%q)", *value)
}

func NewSlopcopLogin(token string, serviceURL *string) (SlopcopLogin, error) {
	panic("unported: mj-core/src/slopcop.rs SlopcopLogin::new")
}

// Encode is the two-line form the session wrapper reads with `read -r`: the
// token, then the service URL, which is empty for slopcop's default.
func (l SlopcopLogin) Encode() []byte {
	service := ""
	if l.serviceURL != nil {
		service = *l.serviceURL
	}
	return []byte(l.token + "\n" + service + "\n")
}

func SlopcopLoginDecode(data []byte) (SlopcopLogin, error) {
	panic("unported: mj-core/src/slopcop.rs SlopcopLogin::decode")
}

func (l SlopcopLogin) Snapshot() CredentialSnapshot {
	return CredentialSnapshot{
		Present:     true,
		Fingerprint: CredentialFingerprint(l.Encode()),
	}
}

// validateServiceURL: the URL must fit on one line of the login file and in
// one environment variable. Whether it names a usable service is slopcop's
// decision, as it is for the host's own `--service`, so Mjolnir carries it
// unchanged.
func validateServiceURL(url string) (string, error) {
	if len(url) > maxServiceURLBytes {
		return "", fmt.Errorf("slopcop service URL is above the %d byte limit", maxServiceURLBytes)
	}
	if strings.ContainsFunc(url, func(r rune) bool { return unicode.IsSpace(r) || unicode.IsControl(r) }) {
		return "", errors.New("slopcop service URL contains whitespace or control characters")
	}
	return url, nil
}

// HostLogin is the host's login, resolved from this process's environment
// and slopcop's stored configuration under the user configuration directory
// (os.UserConfigDir, which `dirs::config_dir` matches on every platform).
func HostLogin() (*SlopcopLogin, error) {
	configDir, err := os.UserConfigDir()
	var dir *string
	if err == nil {
		dir = &configDir
	}
	return ResolveLogin(func(name string) (string, bool) { return os.LookupEnv(name) }, dir)
}

// ResolveLogin resolves a login the way slopcop does: SLOPCOP_TOKEN, else
// the stored token; SLOPCOP_SERVICE, else the stored service URL.
//
// (nil, nil) means there is no login. A stored configuration that exists but
// cannot be read is an error: the login is unknown, and a caller must not
// treat it as signed out.
func ResolveLogin(environment func(name string) (string, bool), configDir *string) (*SlopcopLogin, error) {
	panic("unported: mj-core/src/slopcop.rs resolve_login")
}

// slopcopStoredConfig is the fields of slopcop's `config.json` it resolves a
// login from.
type slopcopStoredConfig struct {
	ServiceURL *string `json:"serviceUrl"`
	Token      *string `json:"token"`
}

func readStoredConfig(path string) (slopcopStoredConfig, error) {
	panic("unported: mj-core/src/slopcop.rs read_stored_config")
}

// ReadLogin returns the session copy's state and contents.
func ReadLogin(path string) (CredentialSnapshot, *SlopcopLogin, error) {
	panic("unported: mj-core/src/slopcop.rs read_login")
}

// WriteLogin installs data, an encoded login, owner-only in an existing
// directory.
func WriteLogin(path string, data []byte) (CredentialSnapshot, error) {
	login, err := SlopcopLoginDecode(data)
	if err != nil {
		return CredentialSnapshot{}, err
	}
	if err := RefuseSymlinkedToken("slopcop login", path); err != nil {
		return CredentialSnapshot{}, err
	}
	if err := AtomicWriteExisting(path, login.Encode()); err != nil {
		return CredentialSnapshot{}, err
	}
	return login.Snapshot(), nil
}

func RemoveLogin(path string) error {
	if err := RefuseSymlinkedToken("slopcop login", path); err != nil {
		return err
	}
	if err := os.Remove(path); err != nil && !errors.Is(err, fs.ErrNotExist) {
		return fmt.Errorf("remove slopcop login %s: %w", path, err)
	}
	return nil
}
