//! The database server: listens on a TCP port and serves the requests of
//! `database-protocol` from the databases in its data directory.

use std::net::SocketAddr;
use std::path::PathBuf;

/// How a server is started.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Where the databases' files live. Created if absent.
    pub data_dir: PathBuf,
    /// The address to listen on. Port 0 picks a free one.
    pub listen: SocketAddr,
    /// The password for administering: creating and dropping databases,
    /// dumping, restoring.
    pub admin_password: String,
    /// Memory for cached pages, in bytes, shared by all databases.
    pub cache_bytes: usize,
    /// Whether a commit waits for the disk. Turning it off makes a crash
    /// able to lose the last moments of commits (never to corrupt), and
    /// is meant for tests.
    pub sync: bool,
}

impl ServerConfig {
    pub fn new(data_dir: impl Into<PathBuf>, admin_password: &str) -> ServerConfig {
        ServerConfig {
            data_dir: data_dir.into(),
            listen: SocketAddr::from(([127, 0, 0, 1], 0)),
            admin_password: admin_password.to_string(),
            cache_bytes: 32 * 1024 * 1024,
            sync: true,
        }
    }
}

/// A running server.
pub struct Server {
    addr: SocketAddr,
}

impl Server {
    /// Opens the data directory, recovers every database in it, and
    /// starts accepting connections.
    pub async fn start(_config: ServerConfig) -> std::io::Result<Server> {
        todo!("the server is written after the tests that define it")
    }

    /// The address the server listens on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stops accepting, lets requests in flight finish, writes a final
    /// checkpoint and closes the files.
    pub async fn stop(self) {}
}
