// Port of mj-cli/src/api_client/pinned_tls.rs.
//
// Trust exactly the certificate the local daemon says its viewer serves.
//
// The web viewer may serve a self-signed certificate (the phone setup does).
// WebPKI refuses such a certificate as a server identity, and a self-signed
// one marked `CA:TRUE` fails even when added as a root. The CLI does not
// need a CA chain: it already trusts the daemon, which publishes the served
// certificate's SHA-256 over its token-authenticated control channel. The
// verifier here accepts that one certificate and nothing else; crypto/tls
// still checks through the handshake signature that the server holds the
// certificate's private key, as rustls does with a custom verifier.

package cli

import (
	"crypto/tls"
	"crypto/x509"
	"errors"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/server"
)

// pinnedCertificate accepts only the certificate whose DER SHA-256 is
// sha256 (lowercase hex).
type pinnedCertificate struct {
	sha256 string
}

// errPinMismatch is rustls's
// `InvalidCertificate(ApplicationVerificationFailure)`.
var errPinMismatch = errors.New("invalid peer certificate: ApplicationVerificationFailure")

// verifyServerCert is the tls.Config VerifyPeerCertificate hook. The pin
// names one exact certificate, so the host name, chain, and validity window
// add nothing: the daemon vouches for these bytes.
func (p pinnedCertificate) verifyServerCert(rawCerts [][]byte, _ [][]*x509.Certificate) error {
	if len(rawCerts) == 0 {
		return errPinMismatch
	}
	if server.CertificateDerSha256(rawCerts[0]) == p.sha256 {
		return nil
	}
	return errPinMismatch
}

// pinnedClientConfig is a TLS client configuration that accepts only the
// certificate whose DER SHA-256 is sha256 (hex, either case).
//
// InsecureSkipVerify turns off only the chain and host name checks, which
// verifyServerCert replaces; the handshake still proves the server holds the
// certificate's key. TLS 1.2 and 1.3 are rustls's safe defaults.
func pinnedClientConfig(sha256 string) (*tls.Config, error) {
	// to_ascii_lowercase: only ASCII letters change.
	pin := pinnedCertificate{sha256: strings.Map(func(r rune) rune {
		if r >= 'A' && r <= 'Z' {
			return r + ('a' - 'A')
		}
		return r
	}, sha256)}
	return &tls.Config{
		MinVersion:            tls.VersionTLS12,
		InsecureSkipVerify:    true,
		VerifyPeerCertificate: pin.verifyServerCert,
	}, nil
}
