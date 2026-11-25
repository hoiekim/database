//! TLS on the TCP listener: the certificate the server shows, and which
//! of the connections that arrive are held to it.
//!
//! A connection says in its first three bytes whether it begins a TLS
//! handshake or a greeting in the clear, so one port serves both where
//! both are allowed, and a client in the clear on a server that requires
//! TLS can be told why it is refused, in a frame it can read.
//!
//! The certificate and its key are files. They are read at the start,
//! and read again when either file has changed: looked at when a
//! connection arrives, at most once a second. A renewal is then a matter
//! of replacing the files. Files that cannot be read, or do not belong
//! together (one of the two replaced, the other not yet), change nothing:
//! the server goes on with the certificate it has and says so in its log.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use rustls::crypto::ring;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::net::TcpStream;

/// The certificate a server shows, and whether it insists on TLS.
#[derive(Clone, Debug)]
pub struct TlsConfig {
    /// A PEM file: the server's certificate, followed by those of the
    /// authorities between it and the one clients trust, if any.
    pub certificate: PathBuf,
    /// A PEM file: the certificate's private key.
    pub key: PathBuf,
    /// Whether a connection over TCP in the clear is still served. Off,
    /// it is refused in its greeting: every password and row on the
    /// network is then encrypted. On is for the days in which clients
    /// are moved to TLS one by one.
    pub plain: bool,
}

impl TlsConfig {
    /// TLS required of every connection over TCP.
    pub fn new(certificate: impl Into<PathBuf>, key: impl Into<PathBuf>) -> TlsConfig {
        TlsConfig { certificate: certificate.into(), key: key.into(), plain: false }
    }
}

/// How often the files are looked at for a change, at most.
const LOOK_EVERY: Duration = Duration::from_secs(1);

/// When a file was last written, and how long it is: what a change of
/// the file changes.
type Stamp = Option<(SystemTime, u64)>;

fn stamp(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

fn load(config: &TlsConfig) -> std::io::Result<Arc<rustls::ServerConfig>> {
    let bad = |path: &Path, e: &dyn std::fmt::Display| std::io::Error::other(format!("{}: {e}", path.display()));
    let certificates = CertificateDer::pem_file_iter(&config.certificate)
        .and_then(|certificates| certificates.collect::<Result<Vec<_>, _>>())
        .map_err(|e| bad(&config.certificate, &e))?;
    if certificates.is_empty() {
        return Err(bad(&config.certificate, &"no certificate in the file"));
    }
    let key = PrivateKeyDer::from_pem_file(&config.key).map_err(|e| bad(&config.key, &e))?;
    // TLS 1.3 alone: every client this server has speaks it, and the
    // older versions are code the server would carry to no end.
    let server = rustls::ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(std::io::Error::other)?
        .with_no_client_auth()
        // Refuses a key that is not the certificate's.
        .with_single_cert(certificates, key)
        .map_err(|e| std::io::Error::other(format!("{} and {}: {e}", config.certificate.display(), config.key.display())))?;
    Ok(Arc::new(server))
}

struct Current {
    server: Arc<rustls::ServerConfig>,
    /// The files as they were when last read, whether or not that
    /// reading gave a certificate.
    stamps: (Stamp, Stamp),
    looked: Instant,
}

/// The server's side of TLS, shared by the connections that arrive.
pub(crate) struct Tls {
    config: TlsConfig,
    current: Mutex<Current>,
}

impl Tls {
    /// Reads the certificate and its key. A server is not started on
    /// files it cannot use.
    pub(crate) fn open(config: TlsConfig) -> std::io::Result<Tls> {
        let stamps = (stamp(&config.certificate), stamp(&config.key));
        let server = load(&config).map_err(|e| std::io::Error::other(format!("the TLS certificate cannot be used: {e}")))?;
        Ok(Tls { config, current: Mutex::new(Current { server, stamps, looked: Instant::now() }) })
    }

    pub(crate) fn plain_allowed(&self) -> bool {
        self.config.plain
    }

    /// What the next handshake is made with: the files' certificate, read
    /// again if they have changed since they were last read.
    pub(crate) fn acceptor(&self) -> tokio_rustls::TlsAcceptor {
        let mut current = self.current.lock().unwrap();
        if current.looked.elapsed() >= LOOK_EVERY {
            current.looked = Instant::now();
            let stamps = (stamp(&self.config.certificate), stamp(&self.config.key));
            if stamps != current.stamps {
                // Noted whether or not they can be used: files that
                // cannot are reported once, not at every connection.
                current.stamps = stamps;
                match load(&self.config) {
                    Ok(server) => {
                        current.server = server;
                        eprintln!("database: the TLS certificate was read again from {}", self.config.certificate.display());
                    }
                    Err(e) => eprintln!("database: the TLS certificate has changed and cannot be used, so the one before it is kept: {e}"),
                }
            }
        }
        tokio_rustls::TlsAcceptor::from(current.server.clone())
    }
}

/// Whether a connection's first three bytes begin a TLS handshake: a
/// handshake record (22) of version 3.x. In the clear they are the low
/// bytes of a greeting's length, which is under 16,384, so the third is
/// zero; in a handshake record it is the minor version, which is not.
pub(crate) fn is_handshake(first: [u8; 3]) -> bool {
    first[0] == 0x16 && first[1] == 0x03 && first[2] != 0
}

/// A connection's first three bytes, left where they are for whoever
/// reads the connection next. `None` if it closed before sending them.
pub(crate) async fn first_bytes(stream: &TcpStream) -> Option<[u8; 3]> {
    let mut first = [0u8; 3];
    loop {
        match stream.peek(&mut first).await {
            Ok(3) => return Some(first),
            Ok(0) | Err(_) => return None,
            // Fewer have arrived than are needed, which a client that
            // sends its first frame whole does not do: look again.
            Ok(_) => tokio::time::sleep(Duration::from_millis(1)).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handshake_is_told_from_a_greeting_by_three_bytes() {
        // What TLS clients send first: a handshake record, whose version
        // field has read 3.1 since TLS 1.0 and reads 3.3 from some.
        assert!(is_handshake([0x16, 0x03, 0x01]));
        assert!(is_handshake([0x16, 0x03, 0x03]));
        // Every length a greeting may have, as its first three bytes:
        // none is taken for a handshake, 0x0316 = 790 bytes included.
        for len in 0u32..=16_384 {
            let bytes = len.to_le_bytes();
            assert!(!is_handshake([bytes[0], bytes[1], bytes[2]]), "a greeting of {len} bytes");
        }
    }
}
