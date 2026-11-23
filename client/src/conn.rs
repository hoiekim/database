//! One connection to the server, and the pool that shares a few of them.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use database_protocol::error::{DbError, DbResult, ErrorCode};
use database_protocol::message::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::Config;

fn lost(what: impl std::fmt::Display) -> DbError {
    DbError::new(ErrorCode::Connection, format!("connection to the database lost: {what}"))
}

pub(crate) struct Conn {
    stream: TcpStream,
    /// Set when the stream can no longer be trusted to be between two
    /// requests: after an I/O error, a timeout, or a caller that went away
    /// mid-request.
    broken: bool,
    last_used: Instant,
    statement_timeout: Duration,
    lock_wait: Duration,
}

impl Conn {
    async fn open(config: &Config) -> DbResult<Conn> {
        let address = (config.host.as_str(), config.port);
        let stream = match tokio::time::timeout(config.connect_timeout, TcpStream::connect(address)).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(e)) => return Err(DbError::new(ErrorCode::Connection, format!("cannot reach the database at {}:{}: {e}", config.host, config.port))),
            Err(_) => return Err(DbError::new(ErrorCode::Connection, format!("timed out connecting to the database at {}:{}", config.host, config.port))),
        };
        // Requests are small and each waits for its answer: never delay one
        // to batch it with the next.
        stream.set_nodelay(true).map_err(lost)?;
        let mut conn =
            Conn { stream, broken: false, last_used: Instant::now(), statement_timeout: config.statement_timeout, lock_wait: config.lock_wait };
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
        let payload = encode_request(request, self.statement_timeout.as_millis() as u64);
        if payload.len() > MAX_FRAME {
            return Err(DbError::invalid(format!("a request of {} bytes is larger than the {} allowed", payload.len(), MAX_FRAME)));
        }
        let mut frame = Vec::with_capacity(payload.len() + 4);
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(&payload);
        self.broken = true;
        self.stream.write_all(&frame).await.map_err(lost)?;
        Ok(())
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
        let mut frame = vec![0u8; len];
        self.stream.read_exact(&mut frame).await.map_err(lost)?;
        decode_response(&frame).map_err(|e| lost(e.message))
    }

    pub(crate) fn settle(&mut self) {
        self.broken = false;
        self.last_used = Instant::now();
    }

    /// Sends a request and reads its whole answer. A result that came in
    /// several frames is put back together. An error from the server is
    /// returned as `Err` and leaves the connection usable.
    pub(crate) async fn call(&mut self, request: Request) -> DbResult<Response> {
        // The server enforces the statement's deadline and answers with an
        // error; this outer limit only catches a server that went silent.
        let patience = if self.statement_timeout.is_zero() { Duration::from_secs(86_400) } else { self.statement_timeout + self.lock_wait + Duration::from_secs(10) };
        match tokio::time::timeout(patience, self.exchange(&request)).await {
            Ok(result) => result,
            Err(_) => Err(lost("no answer in time")),
        }
    }

    async fn exchange(&mut self, request: &Request) -> DbResult<Response> {
        self.send(request).await?;
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
                        self.settle();
                        return Ok(Response::Rows { set, more: false });
                    }
                }
                Response::Error(e) => {
                    self.settle();
                    return Err(e);
                }
                other => {
                    self.settle();
                    return Ok(other);
                }
            }
        }
    }
}

pub(crate) struct Pool {
    pub(crate) config: Config,
    idle: Mutex<Vec<Conn>>,
    permits: Arc<Semaphore>,
}

impl Pool {
    pub(crate) fn new(config: Config) -> Pool {
        let permits = Arc::new(Semaphore::new(config.pool_size.max(1)));
        Pool { config, idle: Mutex::new(Vec::new()), permits }
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
