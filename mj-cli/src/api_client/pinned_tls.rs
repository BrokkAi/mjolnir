//! Trust exactly the certificate the local daemon says its viewer serves.
//!
//! The web viewer may serve a self-signed certificate (the phone setup does).
//! WebPKI refuses such a certificate as a server identity, and a self-signed
//! one marked `CA:TRUE` fails even when added as a root (`CaUsedAsEndEntity`).
//! The CLI does not need a CA chain: it already trusts the daemon, which
//! publishes the served certificate's SHA-256 over its token-authenticated
//! control channel. The verifier here accepts that one certificate and nothing
//! else, and still checks through the handshake signature that the server
//! holds the certificate's private key.

use std::sync::Arc;

use anyhow::{Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, Error, SignatureScheme};

#[derive(Debug)]
struct PinnedCertificate {
    sha256: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedCertificate {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, Error> {
        // The pin names one exact certificate, so the host name, chain, and
        // validity window add nothing: the daemon vouches for these bytes.
        if mj_controller::server::api::certificate_der_sha256(end_entity) == self.sha256 {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// A rustls client configuration that accepts only the certificate whose DER
/// SHA-256 is `sha256` (hex).
pub(super) fn pinned_client_config(sha256: &str) -> Result<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .context("select TLS versions for the daemon API")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCertificate {
            sha256: sha256.to_ascii_lowercase(),
            provider,
        }))
        .with_no_client_auth();
    Ok(config)
}
