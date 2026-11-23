// Port of mj-targets/src/tailscale.rs.
//
// Optional trusted HTTPS for the daemon-owned web viewer.
//
// Tailscale owns ACME issuance for its `ts.net` names. Hel only discovers
// the local node, asks the CLI for that certificate, and stores the
// resulting pair in its private data directory.

package targets

import (
	"bytes"
	"crypto/rand"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"log/slog"
	"os"
	"path/filepath"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/harnessinstall"
)

const certFile = "tailscale-cert.pem"
const keyFile = "tailscale-key.pem"

type tailscale struct {
	binary     string
	certDomain string
}

// TailscaleTLS is a Tailscale identity and its persisted certificate pair.
type TailscaleTLS struct {
	tailscale tailscale
	certPath  string
	keyPath   string
}

func (t *TailscaleTLS) CertDomain() string {
	return t.tailscale.certDomain
}

func (t *TailscaleTLS) CertPath() string {
	return t.certPath
}

func (t *TailscaleTLS) KeyPath() string {
	return t.keyPath
}

// Renew refreshes the persisted pair. Callers keep serving their currently
// loaded certificate until they have successfully reloaded these files.
func (t *TailscaleTLS) Renew(executor core.CommandExecutor) error {
	return mintCertificate(&t.tailscale, t.certPath, t.keyPath, executor)
}

// PrepareTailscaleTLS discovers a certificate-capable local Tailscale node
// and mints its initial certificate. Discovery failures are intentionally
// returned to the caller, which can safely fall back to a loopback-only
// listener.
func PrepareTailscaleTLS(root string, executor core.CommandExecutor) (*TailscaleTLS, error) {
	binary, ok := findBinary()
	if !ok {
		return nil, errors.New("tailscale CLI not found in PATH or the macOS app bundle")
	}
	return prepareTailscaleTLSWithBinary(root, binary, executor)
}

func prepareTailscaleTLSWithBinary(root string, binary string, executor core.CommandExecutor) (*TailscaleTLS, error) {
	identity, err := discoverWithBinary(binary, executor)
	if err != nil {
		return nil, err
	}
	if err := os.MkdirAll(root, 0o777); err != nil {
		return nil, fmt.Errorf("create web viewer TLS directory %s: %w", root, err)
	}
	tls := &TailscaleTLS{
		tailscale: identity,
		certPath:  filepath.Join(root, certFile),
		keyPath:   filepath.Join(root, keyFile),
	}
	if err := tls.Renew(executor); err != nil {
		return nil, err
	}
	return tls, nil
}

func discoverWithBinary(binary string, executor core.CommandExecutor) (tailscale, error) {
	command := core.NewCommandSpec(binary, []string{"status", "--json"}).
		WithPurpose("inspect local Tailscale status")
	output, err := executor.Execute(&command)
	if err != nil {
		return tailscale{}, err
	}
	if output.Status != 0 {
		return tailscale{}, fmt.Errorf("`tailscale status` failed: %s", strings.TrimSpace(lossyText(output.Stderr)))
	}
	var status tailscaleStatus
	if err := json.Unmarshal(output.Stdout, &status); err != nil {
		return tailscale{}, fmt.Errorf("parse `tailscale status --json` output: %w", err)
	}
	domain, err := certDomain(&status)
	if err != nil {
		return tailscale{}, err
	}
	return tailscale{binary: binary, certDomain: domain}, nil
}

func mintCertificate(identity *tailscale, certPath string, keyPath string, executor core.CommandExecutor) error {
	parent := filepath.Dir(certPath)
	if err := os.MkdirAll(parent, 0o777); err != nil {
		return fmt.Errorf("create web viewer TLS directory %s: %w", parent, err)
	}
	suffix, err := temporarySuffix()
	if err != nil {
		return err
	}
	stagedCert := filepath.Join(parent, fmt.Sprintf(".%s.%s.tmp", certFile, suffix))
	stagedKey := filepath.Join(parent, fmt.Sprintf(".%s.%s.tmp", keyFile, suffix))
	defer removeStagedFile(stagedKey)
	defer removeStagedFile(stagedCert)
	command := core.NewCommandSpec(identity.binary, []string{
		"cert", "--cert-file", stagedCert, "--key-file", stagedKey, identity.certDomain,
	}).WithPurpose("obtain the web viewer Tailscale certificate")
	output, err := executor.Execute(&command)
	if err != nil {
		return err
	}
	if output.Status != 0 {
		return fmt.Errorf("`tailscale cert %s` failed: %s", identity.certDomain, strings.TrimSpace(lossyText(output.Stderr)))
	}
	cert, err := os.ReadFile(stagedCert)
	if err != nil {
		return fmt.Errorf("read issued certificate %s: %w", stagedCert, err)
	}
	key, err := os.ReadFile(stagedKey)
	if err != nil {
		return fmt.Errorf("read issued private key %s: %w", stagedKey, err)
	}
	if err := validatePEM(cert, key); err != nil {
		return err
	}
	if err := core.AtomicWrite(certPath, cert); err != nil {
		return err
	}
	return core.AtomicWrite(keyPath, key)
}

func temporarySuffix() (string, error) {
	var random [8]byte
	if _, err := rand.Read(random[:]); err != nil {
		return "", fmt.Errorf("generate temporary certificate filename: %w", err)
	}
	return fmt.Sprintf("%d.%016x", os.Getpid(), binary.LittleEndian.Uint64(random[:])), nil
}

func validatePEM(cert []byte, key []byte) error {
	if !bytes.Contains(cert, []byte("-----BEGIN CERTIFICATE-----")) {
		return errors.New("tailscale returned a certificate file without a PEM certificate")
	}
	if !bytes.Contains(key, []byte("PRIVATE KEY-----")) {
		return errors.New("tailscale returned a key file without a PEM private key")
	}
	return nil
}

func removeStagedFile(path string) {
	if err := os.Remove(path); err != nil && !errors.Is(err, fs.ErrNotExist) {
		slog.Warn("could not remove staged Tailscale certificate file", "path", path, "error", err)
	}
}

func findBinary() (string, bool) {
	if path, ok := os.LookupEnv("PATH"); ok {
		if binary, ok := findBinaryInPath(path); ok {
			return binary, true
		}
	}
	return findBundledBinary()
}

func findBinaryInPath(path string) (string, bool) {
	return harnessinstall.FindProgramOnPath("tailscale", path, true)
}

// tailscaleStatus is the part of `tailscale status --json` Mjolnir reads.
type tailscaleStatus struct {
	BackendState string   `json:"BackendState"`
	CertDomains  []string `json:"CertDomains"`
}

// UnmarshalJSON requires BackendState, as serde does.
func (s *tailscaleStatus) UnmarshalJSON(data []byte) error {
	var decoded struct {
		BackendState *string  `json:"BackendState"`
		CertDomains  []string `json:"CertDomains"`
	}
	if err := json.Unmarshal(data, &decoded); err != nil {
		return err
	}
	if decoded.BackendState == nil {
		return errors.New("missing field `BackendState`")
	}
	*s = tailscaleStatus{BackendState: *decoded.BackendState, CertDomains: decoded.CertDomains}
	return nil
}

func certDomain(status *tailscaleStatus) (string, error) {
	if status.BackendState != "Running" {
		return "", fmt.Errorf("tailscale is not running (state: %s); run `tailscale up` first", status.BackendState)
	}
	if len(status.CertDomains) > 0 {
		if domain := strings.TrimRight(status.CertDomains[0], "."); domain != "" {
			return domain, nil
		}
	}
	return "", errors.New("this tailnet has no HTTPS certificate domains; enable MagicDNS and HTTPS Certificates under the DNS tab at https://login.tailscale.com/admin/dns, then run `mj daemon restart`")
}
