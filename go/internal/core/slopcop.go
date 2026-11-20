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
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
	"unicode"
	"unicode/utf8"
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
	validated, err := validateOpaqueToken("slopcop token", maxTokenBytes, []byte(token))
	if err != nil {
		return SlopcopLogin{}, err
	}
	login := SlopcopLogin{token: validated}
	if serviceURL != nil {
		if url := strings.TrimSpace(*serviceURL); url != "" {
			checked, err := validateServiceURL(url)
			if err != nil {
				return SlopcopLogin{}, err
			}
			login.serviceURL = &checked
		}
	}
	return login, nil
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
	if len(data) > MaxLoginBytes {
		return SlopcopLogin{}, fmt.Errorf("slopcop login is above the %d byte limit", MaxLoginBytes)
	}
	if !utf8.Valid(data) {
		return SlopcopLogin{}, errors.New("slopcop login is not valid UTF-8")
	}
	lines := agentRustLines(string(data))
	token := ""
	if len(lines) > 0 {
		token = lines[0]
	}
	var serviceURL *string
	if len(lines) > 1 {
		serviceURL = &lines[1]
	}
	if len(lines) > 2 {
		return SlopcopLogin{}, errors.New("slopcop login has more than a token and a service URL")
	}
	return NewSlopcopLogin(token, serviceURL)
}

// agentRustLines splits text as Rust's str::lines does: on `\n`, dropping a
// `\r` before it and the empty piece after a final newline.
func agentRustLines(text string) []string {
	var lines []string
	for text != "" {
		line, rest, found := strings.Cut(text, "\n")
		if found {
			line = strings.TrimSuffix(line, "\r")
		}
		lines = append(lines, line)
		text = rest
	}
	return lines
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
	set := func(name string) *string {
		value, ok := environment(name)
		if !ok {
			return nil
		}
		value = strings.TrimSpace(value)
		if value == "" {
			return nil
		}
		return &value
	}
	token := set(TokenEnv)
	serviceURL := set(ServiceEnv)
	if token != nil && serviceURL != nil {
		login, err := NewSlopcopLogin(*token, serviceURL)
		if err != nil {
			return nil, err
		}
		return &login, nil
	}
	var stored slopcopStoredConfig
	if configDir != nil {
		var err error
		stored, err = readStoredConfig(filepath.Join(*configDir, "slopcop", "config.json"))
		if err != nil {
			return nil, err
		}
	}
	if token == nil {
		token = stored.Token
	}
	if token == nil {
		return nil, nil
	}
	if serviceURL == nil {
		serviceURL = stored.ServiceURL
	}
	login, err := NewSlopcopLogin(*token, serviceURL)
	if err != nil {
		return nil, err
	}
	return &login, nil
}

// slopcopStoredConfig is the fields of slopcop's `config.json` it resolves a
// login from.
type slopcopStoredConfig struct {
	ServiceURL *string `json:"serviceUrl"`
	Token      *string `json:"token"`
}

func readStoredConfig(path string) (slopcopStoredConfig, error) {
	file, err := os.Open(path)
	if errors.Is(err, fs.ErrNotExist) {
		return slopcopStoredConfig{}, nil
	}
	if err != nil {
		return slopcopStoredConfig{}, fmt.Errorf("read slopcop login %s: %w", path, err)
	}
	defer file.Close()
	data, err := io.ReadAll(io.LimitReader(file, maxConfigBytes+1))
	if err != nil {
		return slopcopStoredConfig{}, fmt.Errorf("read slopcop login %s: %w", path, err)
	}
	if int64(len(data)) > maxConfigBytes {
		return slopcopStoredConfig{}, fmt.Errorf("slopcop login %s is above the %d byte limit", path, maxConfigBytes)
	}
	var stored slopcopStoredConfig
	if err := json.Unmarshal(data, &stored); err != nil {
		return slopcopStoredConfig{}, fmt.Errorf("parse slopcop login %s: %w", path, err)
	}
	for _, value := range []**string{&stored.Token, &stored.ServiceURL} {
		if *value != nil {
			text := strings.TrimSpace(**value)
			if text == "" {
				*value = nil
			} else {
				*value = &text
			}
		}
	}
	return stored, nil
}

// ReadLogin returns the session copy's state and contents.
func ReadLogin(path string) (CredentialSnapshot, *SlopcopLogin, error) {
	if err := refuseSymlinkedToken("slopcop login", path); err != nil {
		return CredentialSnapshot{}, nil, err
	}
	data, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return CredentialSnapshotAbsent(), nil, nil
	}
	if err != nil {
		return CredentialSnapshot{}, nil, fmt.Errorf("read slopcop login %s: %w", path, err)
	}
	login, err := SlopcopLoginDecode(data)
	if err != nil {
		return CredentialSnapshot{}, nil, err
	}
	return login.Snapshot(), &login, nil
}

// WriteLogin installs data, an encoded login, owner-only in an existing
// directory.
func WriteLogin(path string, data []byte) (CredentialSnapshot, error) {
	login, err := SlopcopLoginDecode(data)
	if err != nil {
		return CredentialSnapshot{}, err
	}
	if err := refuseSymlinkedToken("slopcop login", path); err != nil {
		return CredentialSnapshot{}, err
	}
	if err := AtomicWriteExisting(path, login.Encode()); err != nil {
		return CredentialSnapshot{}, err
	}
	return login.Snapshot(), nil
}

func RemoveLogin(path string) error {
	if err := refuseSymlinkedToken("slopcop login", path); err != nil {
		return err
	}
	if err := os.Remove(path); err != nil && !errors.Is(err, fs.ErrNotExist) {
		return fmt.Errorf("remove slopcop login %s: %w", path, err)
	}
	return nil
}
