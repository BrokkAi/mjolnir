// Port of mj-server/src/server/api/token.rs (only the certificate pins so far).

package server

import (
	"crypto/sha256"
	"encoding/pem"
	"errors"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// CertificateDerSha256 is the pin a local API client checks the viewer's
// certificate against: lowercase hex SHA-256 of the certificate's DER bytes.
func CertificateDerSha256(der []byte) string {
	digest := sha256.Sum256(der)
	return core.LowerHex(digest[:])
}

// ServedCertificateSha256 pins the first (leaf) certificate of a PEM chain
// the viewer serves.
//
// The daemon publishes this with the viewer URL so the CLI can trust exactly
// the certificate the operator configured, including a self-signed one that
// no CA chain, and no `CA:FALSE` rule, would accept.
func ServedCertificateSha256(pemBytes []byte) (string, error) {
	// CertificateDer::pem_slice_iter yields the CERTIFICATE sections and
	// skips every other kind.
	rest := pemBytes
	for {
		block, remaining := pem.Decode(rest)
		if block == nil {
			return "", errors.New("the certificate file holds no PEM certificate")
		}
		rest = remaining
		if block.Type == "CERTIFICATE" {
			return CertificateDerSha256(block.Bytes), nil
		}
	}
}
