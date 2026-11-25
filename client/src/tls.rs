//! TLS for a connection over TCP: how the server is known to be the
//! server.
//!
//! A client that asks for TLS always verifies whom it reached. There is
//! no setting that encrypts without verifying, and none that falls back
//! to an open connection: either would let whoever sits between the two
//! hosts read the password, which is what TLS is asked for to prevent.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use database_protocol::error::{DbError, DbResult, ErrorCode};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{ring, verify_tls12_signature, verify_tls13_signature, WebPkiSupportedAlgorithms};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

use crate::config::Config;

/// How a client that connects over TLS knows the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tls {
    /// Whose word is taken for the server's certificate.
    pub trust: Trust,
    /// The name the certificate must be for. `None` for the host the
    /// client connects to; set when that is not a name the certificate
    /// carries (an address, or a name that only resolves inside a
    /// container network).
    pub name: Option<String>,
}

/// Whose word is taken for the server's certificate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trust {
    /// The certificate authorities this system trusts: the file named by
    /// `SSL_CERT_FILE`, or the first of the usual places that exists
    /// (see `SYSTEM_BUNDLES`).
    System,
    /// The authorities in this file of PEM certificates, and no other.
    Authority(PathBuf),
    /// The server must present exactly the first certificate of this
    /// PEM file. Nobody else's word is taken, and the certificate's
    /// name and dates are not looked at: holding the file is knowing the
    /// server. This is how the server's own commands know it, from the
    /// file it was started with.
    Certificate(PathBuf),
}

impl Tls {
    /// Verified against the system's certificate authorities.
    pub fn system() -> Tls {
        Tls { trust: Trust::System, name: None }
    }

    /// Verified against the authorities in a PEM file.
    pub fn authority(path: impl Into<PathBuf>) -> Tls {
        Tls { trust: Trust::Authority(path.into()), name: None }
    }

    /// Verified by being exactly the certificate in a PEM file.
    pub fn certificate(path: impl Into<PathBuf>) -> Tls {
        Tls { trust: Trust::Certificate(path.into()), name: None }
    }

    /// The name the server's certificate must be for, where it is not
    /// the host connected to.
    pub fn name(mut self, name: &str) -> Tls {
        self.name = Some(name.to_string());
        self
    }
}

/// Where systems keep the certificate authorities they trust, as one
/// file of PEM certificates: Debian and Ubuntu, Fedora and RHEL, openSUSE,
/// Alpine and macOS, FreeBSD.
const SYSTEM_BUNDLES: [&str; 5] = [
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/ca-bundle.pem",
    "/etc/ssl/cert.pem",
    "/usr/local/share/certs/ca-root-nss.crt",
];

fn invalid(what: impl std::fmt::Display) -> DbError {
    DbError::new(ErrorCode::Connection, format!("TLS cannot be set up: {what}"))
}

fn certificates(path: &Path) -> DbResult<Vec<CertificateDer<'static>>> {
    let read = CertificateDer::pem_file_iter(path).and_then(|certificates| certificates.collect::<Result<Vec<_>, _>>());
    let certificates = read.map_err(|e| invalid(format!("{}: {e}", path.display())))?;
    if certificates.is_empty() {
        return Err(invalid(format!("{} holds no certificate", path.display())));
    }
    Ok(certificates)
}

fn system_bundle() -> DbResult<PathBuf> {
    if let Some(path) = std::env::var_os("SSL_CERT_FILE").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    SYSTEM_BUNDLES
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .ok_or_else(|| invalid("this system keeps its certificate authorities in none of the usual files: name the file in SSL_CERT_FILE, or give the server's authority with tls_ca"))
}

/// Takes a certificate for the server's if it is, byte for byte, the one
/// it was given, and the server proves it holds that certificate's key.
#[derive(Debug)]
struct Exactly {
    certificate: CertificateDer<'static>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Exactly {
    fn verify_server_cert(&self, end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>], _name: &ServerName<'_>, _ocsp: &[u8], _now: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.certificate.as_ref() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure))
        }
    }

    fn verify_tls12_signature(&self, message: &[u8], certificate: &CertificateDer<'_>, signed: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, certificate, signed, &self.algorithms)
    }

    fn verify_tls13_signature(&self, message: &[u8], certificate: &CertificateDer<'_>, signed: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, certificate, signed, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// What a client's connections are opened through when it asks for TLS:
/// made once for the client, so the authorities are read once.
pub(crate) struct Connector {
    connector: tokio_rustls::TlsConnector,
    name: ServerName<'static>,
}

impl Connector {
    pub(crate) fn new(config: &Config, tls: &Tls) -> DbResult<Connector> {
        let provider = Arc::new(ring::default_provider());
        let algorithms = provider.signature_verification_algorithms;
        let builder = ClientConfig::builder_with_provider(provider).with_protocol_versions(&[&rustls::version::TLS13]).map_err(invalid)?;
        let client = match &tls.trust {
            Trust::Certificate(path) => {
                let certificate = certificates(path)?.swap_remove(0);
                builder.dangerous().with_custom_certificate_verifier(Arc::new(Exactly { certificate, algorithms })).with_no_client_auth()
            }
            trust => {
                let mut roots = RootCertStore::empty();
                match trust {
                    Trust::Authority(path) => {
                        for certificate in certificates(path)? {
                            roots.add(certificate).map_err(|e| invalid(format!("{}: {e}", path.display())))?;
                        }
                    }
                    // A system's file may hold authorities this library
                    // cannot read: the others are still trusted.
                    _ => {
                        let path = system_bundle()?;
                        roots.add_parsable_certificates(certificates(&path)?);
                        if roots.is_empty() {
                            return Err(invalid(format!("{} holds no usable certificate authority", path.display())));
                        }
                    }
                }
                builder.with_root_certificates(roots).with_no_client_auth()
            }
        };
        let name = tls.name.as_deref().unwrap_or(&config.host);
        let name = ServerName::try_from(name.to_string()).map_err(|_| invalid(format!("{name:?} is not a name a certificate can be for")))?;
        Ok(Connector { connector: tokio_rustls::TlsConnector::from(Arc::new(client)), name })
    }

    /// The handshake, over a connection just opened.
    pub(crate) async fn connect(&self, stream: TcpStream, address: &str) -> DbResult<TlsStream<TcpStream>> {
        self.connector.connect(self.name.clone(), stream).await.map_err(|e| {
            // A server that does not serve TLS takes the handshake for a
            // frame too large to be a greeting, and closes.
            let hint = if e.kind() == std::io::ErrorKind::UnexpectedEof { ": the server closed the connection, as one that does not serve TLS does" } else { "" };
            DbError::new(ErrorCode::Connection, format!("the TLS handshake with the database at {address} failed: {e}{hint}"))
        })
    }
}
