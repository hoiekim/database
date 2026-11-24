//! One connection to the server, and the pool that shares a few of them.

use std::collections::HashSet;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use database_protocol::error::{DbError, DbResult, ErrorCode};
use database_protocol::message::*;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::Config;

fn lost(what: impl std::fmt::Display) -> DbError {
    DbError::new(ErrorCode::Connection, format!("connection to the database lost: {what}"))
}

/// What carries a connection: TCP, or a Unix domain socket to a server on
/// the same host.
enum Stream {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
}

impl AsyncRead for Stream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stream::Tcp(stream) => Pin::new(stream).poll_read(cx, buf),
            #[cfg(unix)]
            Stream::Unix(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Stream::Tcp(stream) => Pin::new(stream).poll_write(cx, buf),
            #[cfg(unix)]
            Stream::Unix(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stream::Tcp(stream) => Pin::new(stream).poll_flush(cx),
            #[cfg(unix)]
            Stream::Unix(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stream::Tcp(stream) => Pin::new(stream).poll_shutdown(cx),
            #[cfg(unix)]
            Stream::Unix(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

async fn connect(config: &Config) -> DbResult<Stream> {
    let unreachable = |e: std::io::Error| DbError::new(ErrorCode::Connection, format!("cannot reach the database at {}: {e}", config.address()));
    let timed_out = || DbError::new(ErrorCode::Connection, format!("timed out connecting to the database at {}", config.address()));
    match &config.socket {
        #[cfg(unix)]
        Some(path) => match tokio::time::timeout(config.connect_timeout, tokio::net::UnixStream::connect(path)).await {
            Ok(stream) => Ok(Stream::Unix(stream.map_err(unreachable)?)),
            Err(_) => Err(timed_out()),
        },
        #[cfg(not(unix))]
        Some(_) => Err(DbError::new(ErrorCode::Connection, "this system has no Unix domain sockets: connect by host and port")),
        None => match tokio::time::timeout(config.connect_timeout, TcpStream::connect((config.host.as_str(), config.port))).await {
            Ok(stream) => {
                let stream = stream.map_err(unreachable)?;
                // Requests are small and each waits for its answer: never
                // delay one to batch it with the next.
                stream.set_nodelay(true).map_err(lost)?;
                Ok(Stream::Tcp(stream))
            }
            Err(_) => Err(timed_out()),
        },
    }
}

/// What a connection reads from its socket at a time.
const READ_BUFFER: usize = 8 * 1024;

/// The most a connection's buffers keep between requests.
const KEPT_BUFFER: usize = 256 * 1024;

/// How many statements a connection remembers having sent the server to
/// keep before it forgets them all and sends each again when next used.
/// The server keeps fewer, and says so when asked for one it let go.
const REMEMBERED_STATEMENTS: usize = 4 * KEPT_STATEMENTS;

/// Calls `f` with each prepared statement a request holds: itself, a
/// transaction's first statement, the statements of a batch.
fn prepared_in(request: &mut Request, f: &mut dyn FnMut(u64, &mut Option<Arc<Request>>)) {
    match request {
        Request::Prepared { id, statement, .. } => f(*id, statement),
        Request::BeginWith { statement, .. } | Request::Explain(statement) => prepared_in(statement, f),
        Request::Batch(items) => items.iter_mut().for_each(|item| prepared_in(&mut item.statement, f)),
        _ => {}
    }
}

pub(crate) struct Conn {
    /// Read through a buffer: a small answer's length and its body are
    /// then one read from the socket, not two.
    stream: BufReader<Stream>,
    /// Set when the stream can no longer be trusted to be between two
    /// requests: after an I/O error, a timeout, or a caller that went away
    /// mid-request.
    broken: bool,
    last_used: Instant,
    statement_timeout: Duration,
    lock_wait: Duration,
    /// The prepared statements the server keeps for this connection, by
    /// number: named again, one is sent without the statement itself.
    kept: HashSet<u64>,
    /// What requests are encoded into and answers read into, used again
    /// for each.
    sending: Vec<u8>,
    receiving: Vec<u8>,
    /// Whether the last call ended without its request having left: it
    /// was refused here, before a byte of it was written.
    unsent: bool,
}

impl Conn {
    async fn open(config: &Config) -> DbResult<Conn> {
        let stream = connect(config).await?;
        let mut conn = Conn {
            stream: BufReader::with_capacity(READ_BUFFER, stream),
            broken: false,
            last_used: Instant::now(),
            statement_timeout: config.statement_timeout,
            lock_wait: config.lock_wait,
            kept: HashSet::new(),
            sending: Vec::new(),
            receiving: Vec::new(),
            unsent: false,
        };
        let hello = Request::Hello {
            version: PROTOCOL_VERSION,
            database: config.database.clone(),
            password: config.password.clone(),
            client: config.name.clone(),
        };
        match tokio::time::timeout(config.connect_timeout, conn.call(hello)).await {
            Ok(Ok(Response::Hello { .. })) => Ok(conn),
            Ok(Ok(_)) => Err(lost("unexpected answer to the greeting")),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(lost("no answer to the greeting")),
        }
    }

    pub(crate) async fn send(&mut self, request: &Request) -> DbResult<()> {
        let mut frame = std::mem::take(&mut self.sending);
        frame.clear();
        frame.extend_from_slice(&[0; 4]);
        let mut frame = encode_request_onto(frame, request, self.statement_timeout.as_millis() as u64);
        let len = frame.len() - 4;
        if len > MAX_FRAME {
            return Err(DbError::invalid(format!("a request of {len} bytes is larger than the {MAX_FRAME} allowed")));
        }
        frame[..4].copy_from_slice(&(len as u32).to_le_bytes());
        self.broken = true;
        self.unsent = false;
        let sent = self.stream.get_mut().write_all(&frame).await.map_err(lost);
        if frame.capacity() <= KEPT_BUFFER {
            self.sending = frame;
        }
        sent
    }

    /// Reads one frame. The connection stays marked broken until the
    /// caller has read the last frame of the answer and calls `settle`.
    pub(crate) async fn receive(&mut self) -> DbResult<Response> {
        let mut len = [0u8; 4];
        self.stream.read_exact(&mut len).await.map_err(lost)?;
        let len = u32::from_le_bytes(len) as usize;
        if len > MAX_FRAME {
            return Err(lost("the server sent a frame larger than allowed"));
        }
        let mut frame = std::mem::take(&mut self.receiving);
        frame.clear();
        frame.resize(len, 0);
        self.stream.read_exact(&mut frame).await.map_err(lost)?;
        let response = decode_response(&frame).map_err(|e| lost(e.message));
        if frame.capacity() <= KEPT_BUFFER {
            self.receiving = frame;
        }
        response
    }

    pub(crate) fn settle(&mut self) {
        self.broken = false;
        self.last_used = Instant::now();
    }

    /// Whether the last call's request never left this side: the server
    /// knows nothing of it, and the connection is as it was before.
    pub(crate) fn never_left(&self) -> bool {
        self.unsent
    }

    /// Reads one whole answer: a result that came in several frames is
    /// put back together. The outer error is the connection's, the inner
    /// one the server's answer.
    async fn answer(&mut self) -> DbResult<DbResult<Response>> {
        let mut merged: Option<RowSet> = None;
        loop {
            match self.receive().await? {
                Response::Rows { set, more } => {
                    let set = match merged.take() {
                        Some(mut all) => {
                            all.rows.extend(set.rows);
                            all
                        }
                        None => set,
                    };
                    if more {
                        merged = Some(set);
                    } else {
                        return Ok(Ok(Response::Rows { set, more: false }));
                    }
                }
                Response::Error(e) => return Ok(Err(e)),
                other => return Ok(Ok(other)),
            }
        }
    }

    /// How long to wait for an answer. The server enforces the
    /// statement's deadline and answers with an error; this outer limit
    /// only catches a server that went silent.
    fn patience(&self) -> Duration {
        if self.statement_timeout.is_zero() {
            Duration::from_secs(86_400)
        } else {
            self.statement_timeout + self.lock_wait + Duration::from_secs(10)
        }
    }

    /// Sends a request and reads its whole answer. An error from the
    /// server is returned as `Err` and leaves the connection usable.
    pub(crate) async fn call(&mut self, mut request: Request) -> DbResult<Response> {
        let held = self.hold_back(&mut request);
        let mut result = self.exchange(&request).await;
        if !held.is_empty() && matches!(&result, Err(e) if e.code == ErrorCode::NotPrepared) {
            self.give_back(&mut request, held);
            result = self.exchange(&request).await;
        }
        self.note_kept(&mut request, result.as_ref().err());
        result
    }

    /// Sends a batch and reads the answer of each of its `statements`.
    pub(crate) async fn call_batch(&mut self, mut request: Request, statements: usize) -> DbResult<Vec<Response>> {
        let held = self.hold_back(&mut request);
        let mut result = self.exchange_batch(&request, statements).await;
        if !held.is_empty() && matches!(&result, Err(e) if e.code == ErrorCode::NotPrepared) {
            self.give_back(&mut request, held);
            result = self.exchange_batch(&request, statements).await;
        }
        self.note_kept(&mut request, result.as_ref().err());
        result
    }

    /// Takes out of a request the prepared statements the server keeps
    /// for this connection: their numbers are enough.
    fn hold_back(&self, request: &mut Request) -> Vec<Arc<Request>> {
        let mut held = Vec::new();
        if !self.kept.is_empty() {
            prepared_in(request, &mut |id, statement| {
                if self.kept.contains(&id) {
                    held.extend(statement.take());
                }
            });
        }
        held
    }

    /// Puts back what `hold_back` took: the server keeps fewer statements
    /// than this connection thought, and is sent them again.
    fn give_back(&mut self, request: &mut Request, held: Vec<Arc<Request>>) {
        let mut held = held.into_iter();
        prepared_in(request, &mut |id, statement| {
            if statement.is_none() {
                *statement = held.next();
                self.kept.remove(&id);
            }
        });
    }

    /// Remembers which statements the server now keeps: those sent with
    /// a request that it took in. A request refused as it stood, before
    /// its statements were looked at, proves nothing.
    fn note_kept(&mut self, request: &mut Request, error: Option<&DbError>) {
        if matches!(error, Some(e) if matches!(e.code, ErrorCode::Connection | ErrorCode::InvalidRequest | ErrorCode::NotPrepared | ErrorCode::Unavailable)) {
            return;
        }
        prepared_in(request, &mut |id, statement| {
            if statement.is_some() {
                if self.kept.len() >= REMEMBERED_STATEMENTS {
                    self.kept.clear();
                }
                self.kept.insert(id);
            }
        });
    }

    async fn exchange(&mut self, request: &Request) -> DbResult<Response> {
        let patience = self.patience();
        let exchange = async {
            self.begin_exchange(request).await?;
            let answer = self.answer().await?;
            self.settle();
            answer
        };
        match tokio::time::timeout(patience, exchange).await {
            Ok(result) => result,
            Err(_) => Err(lost("no answer in time")),
        }
    }

    async fn exchange_batch(&mut self, request: &Request, statements: usize) -> DbResult<Vec<Response>> {
        let patience = self.patience();
        let exchange = async {
            self.begin_exchange(request).await?;
            let mut answers = Vec::with_capacity(statements);
            for _ in 0..statements {
                match self.answer().await? {
                    Ok(answer) => answers.push(answer),
                    // A batch that failed is answered with its error and
                    // nothing else.
                    Err(e) => {
                        self.settle();
                        return Err(e);
                    }
                }
            }
            self.settle();
            Ok(answers)
        };
        match tokio::time::timeout(patience, exchange).await {
            Ok(result) => result,
            Err(_) => Err(lost("no answer in time")),
        }
    }

    async fn begin_exchange(&mut self, request: &Request) -> DbResult<()> {
        // An earlier call that was abandoned midway (its future dropped
        // by a timeout, say) left its answer unread on this connection:
        // the next answer read would be that one. The connection is of
        // no further use, and whatever transaction it carried is over.
        self.unsent = true;
        if self.broken {
            return Err(lost("an earlier request on this connection was abandoned before its answer was read"));
        }
        self.send(request).await
    }
}

pub(crate) struct Pool {
    pub(crate) config: Config,
    idle: Mutex<Vec<Conn>>,
    permits: Arc<Semaphore>,
    /// How often transactions have lately conflicted on their first run:
    /// see `Pool::contended`.
    conflicts: AtomicU32,
}

/// Where `Pool::conflicts` stops rising, and from where it counts as
/// contention.
const CONFLICTS_MOST: u32 = 16;
const CONFLICTS_MANY: u32 = 8;

impl Pool {
    pub(crate) fn new(config: Config) -> Pool {
        let permits = Arc::new(Semaphore::new(config.pool_size.max(1)));
        Pool { config, idle: Mutex::new(Vec::new()), permits, conflicts: AtomicU32::new(0) }
    }

    /// Notes how a transaction's first run beside other writers ended.
    /// A conflict counts for two commits, so that the count rises while
    /// more than a third of the transactions conflict.
    pub(crate) fn note_first_run(&self, conflicted: bool) {
        let _ = self.conflicts.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| Some(if conflicted { (n + 2).min(CONFLICTS_MOST) } else { n.saturating_sub(1) }));
    }

    /// Whether conflicts have lately been the rule. A transaction that
    /// conflicts then is not run beside the others a second time, where
    /// it would conflict again: it goes straight to the write lock.
    pub(crate) fn contended(&self) -> bool {
        self.conflicts.load(Ordering::Relaxed) >= CONFLICTS_MANY
    }

    pub(crate) async fn acquire(self: &Arc<Pool>) -> DbResult<PooledConn> {
        let permit = self.permits.clone().acquire_owned().await.map_err(|_| lost("the client was closed"))?;
        loop {
            let candidate = self.idle.lock().unwrap().pop();
            let Some(mut conn) = candidate else { break };
            // A connection that sat idle may have been closed by a server
            // restart: find out before a real request is lost on it.
            if conn.last_used.elapsed() > Duration::from_secs(20) && conn.call(Request::Ping).await.is_err() {
                continue;
            }
            return Ok(PooledConn { conn: Some(conn), pool: self.clone(), _permit: permit, held: false });
        }
        let conn = Conn::open(&self.config).await?;
        Ok(PooledConn { conn: Some(conn), pool: self.clone(), _permit: permit, held: false })
    }

    /// Closes the idle connections. Ones in use close when released.
    pub(crate) fn clear(&self) {
        self.idle.lock().unwrap().clear();
    }
}

/// A connection out of the pool. Dropped, it goes back, unless it is
/// broken or still holds a transaction or a lock, in which case it is
/// closed and the server undoes what it held.
pub(crate) struct PooledConn {
    conn: Option<Conn>,
    pool: Arc<Pool>,
    _permit: OwnedSemaphorePermit,
    /// The server holds state for this connection: an open transaction or
    /// a named lock.
    pub(crate) held: bool,
}

impl PooledConn {
    pub(crate) fn conn(&mut self) -> &mut Conn {
        self.conn.as_mut().expect("connection is present until dropped")
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            if !conn.broken && !self.held {
                self.pool.idle.lock().unwrap().push(conn);
            }
        }
    }
}
