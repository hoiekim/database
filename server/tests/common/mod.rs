//! What every test file starts from: a server of its own on a free port,
//! with an empty data directory that is removed afterwards.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

pub use database_client::*;
use database_server::{Server, ServerConfig};

pub const ADMIN_PASSWORD: &str = "admin-password";
pub const PASSWORD: &str = "test-password";

/// A directory under the system's temporary one, removed when dropped.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> TempDir {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
        let path = std::env::temp_dir().join(format!("database-test-{}-{n}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub struct TestServer {
    pub server: Option<Server>,
    pub dir: TempDir,
    pub port: u16,
}

impl TestServer {
    /// A server on an empty data directory. Commits do not wait for the
    /// disk: the tests that are about durability start their own.
    pub async fn start() -> TestServer {
        TestServer::start_with(TempDir::new(), |_| {}).await
    }

    /// A server on the given directory, which may hold databases from an
    /// earlier run, with its settings adjusted by `configure`.
    pub async fn start_with(dir: TempDir, configure: impl FnOnce(&mut ServerConfig)) -> TestServer {
        let mut config = ServerConfig::new(dir.path(), ADMIN_PASSWORD);
        config.sync = false;
        configure(&mut config);
        let server = Server::start(config).await.expect("server starts");
        let port = server.addr().port();
        TestServer { server: Some(server), dir, port }
    }

    /// Stops the server cleanly and hands back its directory, to start
    /// another on it.
    pub async fn restart_dir(mut self) -> TempDir {
        if let Some(server) = self.server.take() {
            server.stop().await;
        }
        self.dir
    }

    pub fn url(&self, database: &str) -> String {
        format!("database://{database}:{PASSWORD}@127.0.0.1:{}", self.port)
    }

    /// A client with the administrator's password, on no database.
    pub async fn admin(&self) -> Client {
        Client::connect_url(&format!("database://{ADMIN_PASSWORD}@127.0.0.1:{}", self.port)).await.expect("admin connects")
    }

    /// A client on the named database, which is created if it is not
    /// there yet.
    pub async fn client(&self, database: &str) -> Client {
        self.admin().await.admin().create_database_if_not_exists(database, PASSWORD).await.expect("database is created");
        Client::connect_url(&self.url(database)).await.expect("client connects")
    }

    pub async fn stop(mut self) {
        if let Some(server) = self.server.take() {
            server.stop().await;
        }
    }

    /// A client on an existing database with settings of the test's own.
    pub async fn client_with(&self, database: &str, configure: impl FnOnce(&mut Config)) -> Client {
        let mut config = Config::from_url(&self.url(database)).expect("valid url");
        configure(&mut config);
        Client::connect(config).await.expect("client connects")
    }
}

/// A fresh server and a client on an empty database named `test`.
pub async fn fresh() -> (TestServer, Client) {
    let server = TestServer::start().await;
    let client = server.client("test").await;
    (server, client)
}

/// The values of one column, for comparing against a list.
pub fn column<T: FromValue>(rows: &Rows, name: &str) -> Vec<T> {
    rows.iter().map(|row| row.get::<T>(name).expect("column has the expected type")).collect()
}

pub fn uuid(n: u128) -> Uuid {
    Uuid(n.to_be_bytes())
}

/// A timestamp from RFC 3339 text.
pub fn ts(text: &str) -> Timestamp {
    Timestamp::parse(text).expect("valid timestamp")
}
