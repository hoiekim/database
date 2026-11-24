//! The database server: listens on a TCP port, and on a Unix domain
//! socket if it is given one, and serves the requests of
//! `database-protocol` from the databases in its data directory.
//!
//! One task per connection reads a request, runs it on a blocking thread
//! (the engine does its own I/O and never awaits), and writes the answer.
//! A connection's transaction lives in its session between requests and
//! is rolled back if the connection ends first.

mod access;
mod gate;
pub mod memory;
mod session;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use database_engine::db::Options;
use database_engine::engine::{Engine, EngineOptions};
use database_engine::store::Limits;
use tokio::io::{AsyncRead, AsyncWrite};
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
    /// A Unix domain socket to listen on as well, for clients on the same
    /// host: a request and its answer then skip the network stack. Who
    /// may connect is decided by the file's permissions, and then by the
    /// password, as on TCP. Not on systems without such sockets.
    pub socket: Option<PathBuf>,
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
    /// A statement that takes this long or longer is written to the
    /// server's log (standard error) with what it read: see
    /// `session::slow`. `None` logs none.
    pub slow_statement: Option<Duration>,
}

impl ServerConfig {
    pub fn new(data_dir: impl Into<PathBuf>, admin_password: &str) -> ServerConfig {
        ServerConfig {
            data_dir: data_dir.into(),
            listen: SocketAddr::from(([127, 0, 0, 1], 0)),
            socket: None,
            admin_password: admin_password.to_string(),
            cache_bytes: 32 * 1024 * 1024,
            sync: true,
            lock_wait: Duration::from_secs(30),
            idle_transaction: Duration::from_secs(60),
            slow_statement: None,
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
    /// The connections being served. Kept apart from the rest, so that a
    /// connection's task can say it is done after it has let go of
    /// everything else: see `connect`.
    pub open: Arc<Open>,
    /// Connections accepted, ever: each one's number.
    pub connections: AtomicU64,
    /// Requests received, ever.
    pub requests: AtomicU64,
    /// Each database's queue of writers.
    pub gates: Mutex<HashMap<String, Arc<gate::Gate>>>,
    /// Optimistic transactions that wrote, by how their commit went: the
    /// copy they ran on became the next version as it stood; they were
    /// run again on a newer version and answered the same; or they were
    /// run again and did not, and so failed with a conflict.
    pub forks: [AtomicU64; 3],
}

/// How many connections are being served, and a way to be told when one
/// ends.
#[derive(Default)]
pub(crate) struct Open {
    count: AtomicU64,
    closed: Notify,
}

/// One connection's share of the count, given back when it is dropped:
/// so also if serving the connection panics.
struct Serving(Arc<Open>);

impl Drop for Serving {
    fn drop(&mut self) {
        self.0.count.fetch_sub(1, Ordering::Relaxed);
        self.0.closed.notify_waiters();
    }
}

/// A running server.
pub struct Server {
    addr: SocketAddr,
    socket: Option<PathBuf>,
    stop: watch::Sender<bool>,
    state: Arc<State>,
    /// The tasks that accept connections, and the one that tidies memory:
    /// all end when the server is told to stop.
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// Serves one accepted connection on a task of its own.
fn connect<R, W>(state: &Arc<State>, stopping: &watch::Receiver<bool>, reader: R, writer: W)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let id = state.connections.fetch_add(1, Ordering::Relaxed) + 1;
    let (state, stopping) = (state.clone(), stopping.clone());
    state.open.count.fetch_add(1, Ordering::Relaxed);
    let serving = Serving(state.open.clone());
    tokio::spawn(async move {
        // Dropped last, after the connection has let go of the server's
        // state: whoever waits for the count to reach nothing (`stop`)
        // may then take it that nothing of the connections is left, the
        // hold on the data directory included.
        let _serving = serving;
        session::serve(reader, writer, state, id, stopping).await;
    });
}

/// Binds the Unix socket at `path`. A socket file left by a server that
/// is gone is removed first; one that still answers is somebody's.
#[cfg(unix)]
fn bind_socket(path: &Path) -> std::io::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::FileTypeExt;
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if !meta.file_type().is_socket() {
            return Err(std::io::Error::other(format!("{} exists and is not a socket", path.display())));
        }
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(std::io::Error::new(std::io::ErrorKind::AddrInUse, format!("another server is listening on {}", path.display())));
        }
        std::fs::remove_file(path)?;
    }
    tokio::net::UnixListener::bind(path)
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
        let options = EngineOptions { cache_bytes: config.cache_bytes, db: Options { sync: config.sync, limits, ..Options::default() }, ..EngineOptions::default() };
        let dir = config.data_dir.clone();
        // Recovery reads and writes files: keep it off the async threads.
        let engine = tokio::task::spawn_blocking(move || Engine::open(&dir, options)).await.map_err(std::io::Error::other)?.map_err(|e| std::io::Error::other(e.to_string()))?;
        let access = access::Access::load(&config.data_dir)?;
        let listener = TcpListener::bind(config.listen).await?;
        let addr = listener.local_addr()?;
        #[cfg(unix)]
        let unix = match &config.socket {
            Some(path) => Some((bind_socket(path)?, path.clone())),
            None => None,
        };
        #[cfg(not(unix))]
        if config.socket.is_some() {
            return Err(std::io::Error::other("this system has no Unix domain sockets"));
        }
        let socket = config.socket.clone();
        let state = Arc::new(State {
            engine,
            access: Mutex::new(access),
            config,
            locks: Mutex::new(HashMap::new()),
            lock_freed: Notify::new(),
            open: Arc::default(),
            connections: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            gates: Mutex::new(HashMap::new()),
            forks: Default::default(),
        });
        let (stop, stopping) = watch::channel(false);
        // While requests arrive, and once more after they stop, memory
        // that was freed is handed back to the system. An idle server
        // does nothing here.
        let mut tasks = Vec::new();
        let (tidy_state, mut tidy_stop) = (state.clone(), stopping.clone());
        tasks.push(tokio::spawn(async move {
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
        }));
        let (accept_state, accept_stopping) = (state.clone(), stopping.clone());
        tasks.push(tokio::spawn(async move {
            let mut stopped = accept_stopping.clone();
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
                // Requests are small and each waits for its answer:
                // never hold one back to send it with the next.
                let _ = stream.set_nodelay(true);
                let (reader, writer) = stream.into_split();
                connect(&accept_state, &accept_stopping, reader, writer);
            }
        }));
        #[cfg(unix)]
        if let Some((listener, path)) = unix {
            let (accept_state, accept_stopping) = (state.clone(), stopping.clone());
            tasks.push(tokio::spawn(async move {
                let mut stopped = accept_stopping.clone();
                loop {
                    let accepted = tokio::select! {
                        accepted = listener.accept() => accepted,
                        _ = stopped.changed() => break,
                    };
                    let Ok((stream, _)) = accepted else {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    };
                    let (reader, writer) = stream.into_split();
                    connect(&accept_state, &accept_stopping, reader, writer);
                }
                // Nobody listens any more: the name is free again.
                drop(listener);
                let _ = std::fs::remove_file(&path);
            }));
        }
        Ok(Server { addr, socket, stop, state, tasks })
    }

    /// The address the server listens on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The Unix domain socket the server listens on as well, if any.
    pub fn socket(&self) -> Option<&Path> {
        self.socket.as_deref()
    }

    /// Stops accepting, lets requests in flight finish, writes a final
    /// checkpoint and closes the files.
    pub async fn stop(mut self) {
        let _ = self.stop.send(true);
        // Its own tasks are waited for: when this returns nothing of the
        // server is left that could hold the data directory, and another
        // may be started on it at once.
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
        // Connections end after the request they are on. Give them a
        // moment, then checkpoint whatever is committed.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while self.state.open.count.load(Ordering::Relaxed) > 0 && tokio::time::Instant::now() < deadline {
            let closed = self.state.open.closed.notified();
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
