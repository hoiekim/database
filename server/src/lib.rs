//! The database server: listens on a TCP port and serves the requests of
//! `database-protocol` from the databases in its data directory.
//!
//! One task per connection reads a request, runs it on a blocking thread
//! (the engine does its own I/O and never awaits), and writes the answer.
//! A connection's transaction lives in its session between requests and
//! is rolled back if the connection ends first.

mod access;
pub mod memory;
mod session;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use database_engine::db::Options;
use database_engine::engine::{Engine, EngineOptions};
use database_engine::store::Limits;
use tokio::net::TcpListener;
use tokio::sync::{watch, Notify};

pub use access::sha256;

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
    /// How long a lone write waits for the database's write lock.
    pub lock_wait: Duration,
    /// How long a connection may sit on an open writing transaction
    /// without sending anything before the server rolls it back and
    /// closes the connection. It holds every other writer up meanwhile.
    pub idle_transaction: Duration,
}

impl ServerConfig {
    pub fn new(data_dir: impl Into<PathBuf>, admin_password: &str) -> ServerConfig {
        ServerConfig {
            data_dir: data_dir.into(),
            listen: SocketAddr::from(([127, 0, 0, 1], 0)),
            admin_password: admin_password.to_string(),
            cache_bytes: 32 * 1024 * 1024,
            sync: true,
            lock_wait: Duration::from_secs(30),
            idle_transaction: Duration::from_secs(60),
        }
    }
}

pub(crate) struct State {
    pub engine: Engine,
    pub access: Mutex<access::Access>,
    pub config: ServerConfig,
    /// Named locks: the database and name, and the connection holding it.
    pub locks: Mutex<HashMap<(String, String), u64>>,
    pub lock_freed: Notify,
    pub open_connections: AtomicU64,
    pub connection_closed: Notify,
    /// Requests received, ever.
    pub requests: AtomicU64,
}

/// A running server.
pub struct Server {
    addr: SocketAddr,
    stop: watch::Sender<bool>,
    state: Arc<State>,
    accepting: Option<tokio::task::JoinHandle<()>>,
}

impl Server {
    /// Opens the data directory, recovers every database in it, and
    /// starts accepting connections.
    pub async fn start(config: ServerConfig) -> std::io::Result<Server> {
        // What a database holds outside the cache is kept in proportion
        // to it: an eighth in committed pages waiting to be written, a
        // quarter in the pages of an open transaction.
        let cache_pages = config.cache_bytes / database_engine::page::PAGE_SIZE;
        let limits = Limits { max_dirty: (cache_pages / 8).max(64), max_private: (cache_pages / 4).max(128) };
        let options = EngineOptions { cache_bytes: config.cache_bytes, db: Options { sync: config.sync, limits, ..Options::default() } };
        let dir = config.data_dir.clone();
        // Recovery reads and writes files: keep it off the async threads.
        let engine = tokio::task::spawn_blocking(move || Engine::open(&dir, options)).await.map_err(std::io::Error::other)?.map_err(|e| std::io::Error::other(e.to_string()))?;
        let access = access::Access::load(&config.data_dir)?;
        let listener = TcpListener::bind(config.listen).await?;
        let addr = listener.local_addr()?;
        let state = Arc::new(State {
            engine,
            access: Mutex::new(access),
            config,
            locks: Mutex::new(HashMap::new()),
            lock_freed: Notify::new(),
            open_connections: AtomicU64::new(0),
            connection_closed: Notify::new(),
            requests: AtomicU64::new(0),
        });
        let (stop, stopping) = watch::channel(false);
        // While requests arrive, and once more after they stop, memory
        // that was freed is handed back to the system. An idle server
        // does nothing here.
        let (tidy_state, mut tidy_stop) = (state.clone(), stopping.clone());
        tokio::spawn(async move {
            let mut seen = 0;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                    _ = tidy_stop.changed() => return,
                }
                let now = tidy_state.requests.load(Ordering::Relaxed);
                if now != seen {
                    seen = now;
                    let _ = tokio::task::spawn_blocking(memory::trim).await;
                }
            }
        });
        let accept_state = state.clone();
        let accepting = tokio::spawn(async move {
            let mut next_id = 1u64;
            let mut stopped = stopping.clone();
            loop {
                let accepted = tokio::select! {
                    accepted = listener.accept() => accepted,
                    _ = stopped.changed() => return,
                };
                let Ok((stream, _)) = accepted else {
                    // Out of file descriptors, most likely: wait for some
                    // to be given back rather than spin.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                };
                let id = next_id;
                next_id += 1;
                let state = accept_state.clone();
                let stopping = stopping.clone();
                state.open_connections.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(async move {
                    session::serve(stream, state.clone(), id, stopping).await;
                    state.open_connections.fetch_sub(1, Ordering::Relaxed);
                    state.connection_closed.notify_waiters();
                });
            }
        });
        Ok(Server { addr, stop, state, accepting: Some(accepting) })
    }

    /// The address the server listens on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stops accepting, lets requests in flight finish, writes a final
    /// checkpoint and closes the files.
    pub async fn stop(mut self) {
        let _ = self.stop.send(true);
        if let Some(accepting) = self.accepting.take() {
            let _ = accepting.await;
        }
        // Connections end after the request they are on. Give them a
        // moment, then checkpoint whatever is committed.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while self.state.open_connections.load(Ordering::Relaxed) > 0 && tokio::time::Instant::now() < deadline {
            let closed = self.state.connection_closed.notified();
            let _ = tokio::time::timeout(Duration::from_millis(50), closed).await;
        }
        let state = self.state.clone();
        let _ = tokio::task::spawn_blocking(move || state.engine.checkpoint_all()).await;
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}
