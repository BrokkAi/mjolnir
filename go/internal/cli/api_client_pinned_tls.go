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
)

// pinnedCertificate accepts only the certificate whose DER SHA-256 is
// sha256 (lowercase hex).
type pinnedCertificate struct {
	sha256 string
}

// verifyServerCert is the tls.Config VerifyPeerCertificate hook. The pin
// names one exact certificate, so the host name, chain, and validity window
// add nothing: the daemon vouches for these bytes.
func (p pinnedCertificate) verifyServerCert(rawCerts [][]byte, _ [][]*x509.Certificate) error {
	panic("unported: mj-cli/src/api_client/pinned_tls.rs PinnedCertificate::verify_server_cert")
}

// pinnedClientConfig is a TLS client configuration that accepts only the
// certificate whose DER SHA-256 is sha256 (hex, either case).
func pinnedClientConfig(sha256 string) (*tls.Config, error) {
	panic("unported: mj-cli/src/api_client/pinned_tls.rs pinned_client_config")
}
