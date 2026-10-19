//! Optional TLS for the hub connection. The hub loads a certificate and key
//! (PEM); machines trust either the system's roots or one CA file, which
//! suits a private CA or a self-signed hub certificate.

use std::path::Path;
use std::sync::Arc;

use rustls::RootCertStore;
pub use rustls::{ClientConfig, ServerConfig};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName};

use crate::{Error, Result};

/// Hub addresses starting with this use TLS.
pub const SCHEME: &str = "tls://";

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

fn tls_err(what: impl std::fmt::Display) -> impl FnOnce(String) -> Error {
    move |e| Error::Tls(format!("{what}: {e}"))
}

fn certs(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let certs = CertificateDer::pem_file_iter(path)
        .map_err(|e| tls_err(path.display())(e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| tls_err(path.display())(e.to_string()))?;
    if certs.is_empty() {
        return Err(tls_err(path.display())("no certificates in file".into()));
    }
    Ok(certs)
}

/// TLS settings for a hub serving `cert` (a PEM chain) with `key`.
pub fn server_config(cert: &Path, key: &Path) -> Result<Arc<ServerConfig>> {
    let chain = certs(cert)?;
    let key = PrivateKeyDer::from_pem_file(key).map_err(|e| tls_err(key.display())(e.to_string()))?;
    let config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| tls_err("hub TLS")(e.to_string()))?
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|e| tls_err("hub certificate")(e.to_string()))?;
    Ok(Arc::new(config))
}

/// TLS settings for machines: trust the certificates in `ca`, or the
/// system's roots without one. A certificate in `ca` is also accepted when
/// the hub presents exactly that certificate, so a plain self-signed one
/// (which `openssl req -x509` marks as a CA) works too, like a pinned SSH
/// host key.
pub fn client_config(ca: Option<&Path>) -> Result<Arc<ClientConfig>> {
    let builder = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| tls_err("hub TLS")(e.to_string()))?;
    let Some(path) = ca else {
        let mut roots = RootCertStore::empty();
        for c in rustls_native_certs::load_native_certs().certs {
            let _ = roots.add(c);
        }
        if roots.is_empty() {
            return Err(Error::Tls(
                "no system root certificates found; pass the hub's CA file".into(),
            ));
        }
        return Ok(Arc::new(
            builder.with_root_certificates(roots).with_no_client_auth(),
        ));
    };
    let pinned = certs(path)?;
    let mut roots = RootCertStore::empty();
    let (added, _) = roots.add_parsable_certificates(pinned.iter().cloned());
    let webpki = if added > 0 {
        Some(
            rustls::client::WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider())
                .build()
                .map_err(|e| tls_err(path.display())(e.to_string()))?,
        )
    } else {
        None
    };
    let verifier = Arc::new(PinnedOrCa {
        pinned,
        webpki,
        provider: provider(),
    });
    Ok(Arc::new(
        builder
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth(),
    ))
}

/// Accepts the exact certificates given, or a chain to them as CAs.
#[derive(Debug)]
struct PinnedOrCa {
    pinned: Vec<CertificateDer<'static>>,
    webpki: Option<Arc<rustls::client::WebPkiServerVerifier>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedOrCa {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: rustls_pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if self.pinned.iter().any(|c| c.as_ref() == end_entity.as_ref()) {
            return Ok(rustls::client::danger::ServerCertVerified::assertion());
        }
        match &self.webpki {
            Some(v) => v.verify_server_cert(end_entity, intermediates, server_name, ocsp, now),
            None => Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            )),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
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
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// The name the hub's certificate must carry: the host part of `addr`.
pub(crate) fn server_name(addr: &str) -> Result<ServerName<'static>> {
    let host = match addr.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => host,
        _ => addr,
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    ServerName::try_from(host.to_owned()).map_err(|e| Error::Tls(format!("hub address {addr}: {e}")))
}
