// Port of mj-targets/src/tailscale.rs.
//
// Optional trusted HTTPS for the daemon-owned web viewer.
//
// Tailscale owns ACME issuance for its `ts.net` names. Hel only discovers
// the local node, asks the CLI for that certificate, and stores the
// resulting pair in its private data directory.

package targets

import (
	"encoding/json"
	"errors"
	"os"

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
	panic("unported: mj-targets/src/tailscale.rs prepare_tailscale_tls_with_binary")
}

func discoverWithBinary(binary string, executor core.CommandExecutor) (tailscale, error) {
	panic("unported: mj-targets/src/tailscale.rs discover_with_binary")
}

func mintCertificate(identity *tailscale, certPath string, keyPath string, executor core.CommandExecutor) error {
	panic("unported: mj-targets/src/tailscale.rs mint_certificate")
}

func temporarySuffix() (string, error) {
	panic("unported: mj-targets/src/tailscale.rs temporary_suffix")
}

func validatePEM(cert []byte, key []byte) error {
	panic("unported: mj-targets/src/tailscale.rs validate_pem")
}

func removeStagedFile(path string) {
	panic("unported: mj-targets/src/tailscale.rs remove_staged_file")
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
	panic("unported: mj-targets/src/tailscale.rs cert_domain")
}
