//! One connection: its greeting, its transaction, its locks.

use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use database_engine::db::{Database, ReadTxn, WriteTxn};
use database_engine::dump::{self, Restore};
use database_protocol::error::{DbError, DbResult, ErrorCode};
use database_protocol::message::*;
use database_protocol::query::{Access, Bound, InsertSource, Query};
use database_protocol::time::now_micros;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};

use crate::access::{same, sha256};
use crate::State;

/// A writer's turn at a database: writers queue for it without holding
/// a thread, and the one that has it finds the engine's own lock free.
type Turn = tokio::sync::OwnedMutexGuard<()>;

enum Txn {
    Read(ReadTxn),
    /// Dropped in this order: the transaction, then its turn.
    Write(WriteTxn, Turn),
}

type RestoreTask = tokio::task::JoinHandle<DbResult<Vec<(String, Vec<u8>)>>>;

struct Session {
    id: u64,
    state: Arc<State>,
    /// The database the connection is on. Empty for one that only
    /// administers.
    name: String,
    database: Option<Arc<Database>>,
    admin: bool,
    txn: Option<Txn>,
    /// The size of the request being answered.
    request_bytes: usize,
    /// The named locks this connection holds.
    held: Vec<String>,
    restore: Option<(mpsc::Sender<Vec<u8>>, RestoreTask)>,
}

fn denied() -> DbError {
    DbError::new(ErrorCode::AccessDenied, "no such database, or the wrong password")
}

fn admin_only() -> DbError {
    DbError::new(ErrorCode::AccessDenied, "this needs the administrator's password")
}

/// Runs engine code, which blocks on locks and on the disk, where it
/// holds up no other connection: on this thread, after the runtime has
/// moved the thread's other tasks elsewhere. The request never changes
/// threads, which is most of what a small one costs.
async fn blocking<T>(f: impl FnOnce() -> T) -> DbResult<T> {
    Ok(tokio::task::block_in_place(f))
}

/// The largest frame taken from a connection that has not yet said who
/// it is: room for a greeting and little else.
const GREETING_FRAME: usize = 16 * 1024;

async fn read_frame(reader: &mut OwnedReadHalf, largest: usize) -> std::io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match reader.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > largest {
        return Err(std::io::Error::other("frame larger than allowed"));
    }
    // Room for an ordinary request at once; beyond that, grown as the
    // bytes arrive, so that a length alone reserves little.
    let mut frame = Vec::with_capacity(len.min(64 * 1024));
    let read = reader.take(len as u64).read_to_end(&mut frame).await?;
    if read != len {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    }
    Ok(Some(frame))
}

/// How long a read may run on a thread that serves other connections
/// too, before it is moved to one of its own.
const QUICK: Duration = Duration::from_micros(250);

/// How many times longer than a writing transaction a reading one may
/// sit idle before its connection is closed.
const IDLE_READ_FACTOR: u32 = 10;

/// How long one frame may take to send. A client that has stopped
/// reading would otherwise hold a dump's snapshot, or a result, for ever.
const SEND_PATIENCE: Duration = Duration::from_secs(300);

/// The most a connection's sending buffer keeps between responses.
const KEPT_BUFFER: usize = 256 * 1024;

/// Sends one response as a frame. `buf` is the connection's own buffer,
/// used again for each: an answer of ordinary size allocates nothing.
async fn write_response(writer: &mut OwnedWriteHalf, buf: &mut Vec<u8>, response: &Response) -> std::io::Result<()> {
    let mut frame = std::mem::take(buf);
    frame.clear();
    frame.extend_from_slice(&[0; 4]);
    let mut frame = encode_response_onto(frame, response);
    let len = (frame.len() - 4) as u32;
    frame[..4].copy_from_slice(&len.to_le_bytes());
    let sent = match tokio::time::timeout(SEND_PATIENCE, writer.write_all(&frame)).await {
        Ok(sent) => sent,
        Err(_) => Err(std::io::ErrorKind::TimedOut.into()),
    };
    if frame.capacity() <= KEPT_BUFFER {
        *buf = frame;
    }
    sent
}

pub(crate) async fn serve(stream: TcpStream, state: Arc<State>, id: u64, mut stopping: watch::Receiver<bool>) {
    let _ = stream.set_nodelay(true);
    let (mut reader, mut writer) = stream.into_split();
    let mut session = Session { id, state: state.clone(), name: String::new(), database: None, admin: false, txn: None, request_bytes: 0, held: Vec::new(), restore: None };

    // The greeting, within ten seconds.
    let hello = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut reader, GREETING_FRAME)).await;
    let greeted = match hello {
        Ok(Ok(Some(frame))) => match decode_request(&frame) {
            Ok((Request::Hello { version, database, password, .. }, _)) => session.greet(version, database, &password),
            Ok(_) => Err(DbError::new(ErrorCode::InvalidRequest, "the first request must be a greeting")),
            Err(e) => Err(e),
        },
        _ => return,
    };
    let reply = match &greeted {
        Ok(()) => Response::Hello { version: PROTOCOL_VERSION, server: format!("database {}", env!("CARGO_PKG_VERSION")) },
        Err(e) => Response::Error(e.clone()),
    };
    let mut sending = Vec::new();
    if write_response(&mut writer, &mut sending, &reply).await.is_err() || greeted.is_err() {
        return;
    }

    loop {
        // A connection sitting on the write lock is given only so long.
        // One sitting on a snapshot is given longer, but not for ever:
        // while it is held, no page freed since can be used again.
        let patience = match session.txn {
            Some(Txn::Write(..)) => state.config.idle_transaction,
            Some(Txn::Read(_)) => state.config.idle_transaction.saturating_mul(IDLE_READ_FACTOR),
            None => Duration::from_secs(u64::from(u32::MAX)),
        };
        let frame = tokio::select! {
            frame = tokio::time::timeout(patience, read_frame(&mut reader, MAX_FRAME)) => frame,
            _ = stopping.changed() => break,
        };
        let frame = match frame {
            Ok(Ok(Some(frame))) => frame,
            _ => break,
        };
        state.requests.fetch_add(1, Ordering::Relaxed);
        let outcome = match decode_request(&frame) {
            Ok((request, timeout_ms)) => {
                session.request_bytes = frame.len();
                drop(frame);
                let deadline = (timeout_ms > 0).then(|| Instant::now() + Duration::from_millis(timeout_ms));
                session.handle(request, deadline, &mut writer).await
            }
            Err(e) => Ok(vec![Response::Error(e)]),
        };
        let Ok(responses) = outcome else { break };
        let mut failed = false;
        for response in &responses {
            failed |= write_response(&mut writer, &mut sending, response).await.is_err();
        }
        if failed {
            break;
        }
    }
}

fn rows(set: RowSet) -> Vec<Response> {
    rows_responses(set)
}

/// Runs one statement inside the connection's transaction.
fn run_in_txn(txn: &mut Txn, request: &Request, deadline: Option<Instant>) -> DbResult<Vec<Response>> {
    let read_only = || DbError::new(ErrorCode::InvalidRequest, "a reading transaction cannot write");
    Ok(match (txn, request) {
        (Txn::Read(t), Request::Query(q)) => rows(t.query(q, deadline).map_err(|e| e.into_db())?),
        (Txn::Read(t), Request::Describe) => vec![Response::Tables(t.describe())],
        (Txn::Read(t), Request::Now) => vec![Response::Time(t.now())],
        (Txn::Read(_), _) => return Err(read_only()),
        (Txn::Write(t, _), Request::Query(q)) => rows(t.query(q, deadline).map_err(|e| e.into_db())?),
        (Txn::Write(t, _), Request::Insert(i)) => rows(t.insert(i, deadline).map_err(|e| e.into_db())?),
        (Txn::Write(t, _), Request::Update(u)) => rows(t.update(u, deadline).map_err(|e| e.into_db())?),
        (Txn::Write(t, _), Request::Delete(d)) => rows(t.delete(d, deadline).map_err(|e| e.into_db())?),
        (Txn::Write(t, _), Request::Ddl(ddl)) => {
            t.ddl(ddl, deadline).map_err(|e| e.into_db())?;
            vec![Response::Ok]
        }
        (Txn::Write(t, _), Request::Describe) => vec![Response::Tables(t.describe().map_err(|e| e.into_db())?)],
        (Txn::Write(t, _), Request::Now) => vec![Response::Time(t.now())],
        (Txn::Write(t, _), Request::Savepoint(name)) => {
            t.savepoint(name).map_err(|e| e.into_db())?;
            vec![Response::Ok]
        }
        (Txn::Write(t, _), Request::RollbackTo(name)) => {
            t.rollback_to(name).map_err(|e| e.into_db())?;
            vec![Response::Ok]
        }
        (Txn::Write(t, _), Request::Release(name)) => {
            t.release(name).map_err(|e| e.into_db())?;
            vec![Response::Ok]
        }
        _ => return Err(DbError::invalid("that request is not a statement")),
    })
}

fn is_write(request: &Request) -> bool {
    matches!(request, Request::Insert(_) | Request::Update(_) | Request::Delete(_) | Request::Ddl(_))
}

/// Whether a statement is sure to be over in a moment, and so may run on
/// a thread that serves other connections too: a few rows inserted, or
/// rows updated by their keys, in a request of ordinary size. A delete is
/// never counted so, for what it may take with it.
fn brief(request: &Request, bytes: usize) -> bool {
    let by_key = |q: &Query| {
        q.joins.is_empty()
            && matches!(&q.access, Access::Primary { ranges, .. }
                if ranges.len() <= 8 && ranges.iter().all(|r| matches!((&r.lower, &r.upper), (Bound::Included(a), Bound::Included(b)) if a == b)))
    };
    bytes <= 64 * 1024
        && match request {
            Request::Insert(insert) => matches!(&insert.source, InsertSource::Values(rows) if rows.len() <= 8),
            Request::Update(update) => by_key(&update.rows),
            Request::Describe | Request::Now | Request::Savepoint(_) | Request::RollbackTo(_) | Request::Release(_) => true,
            _ => false,
        }
}

fn lock_timeout(wait: Duration) -> DbError {
    DbError::new(ErrorCode::LockTimeout, format!("waited {wait:?} for the write lock"))
}

/// Takes the engine's write lock, which is free but for the moments a
/// checkpoint or a check holds it: tried at once, and waited for on a
/// thread of its own only then.
async fn begin_write(db: &Arc<Database>, wait: Duration, interactive: bool) -> DbResult<WriteTxn> {
    match db.begin_write(Duration::ZERO, interactive) {
        Err(e) if matches!(&e, database_engine::error::Error::Db(e) if e.code == ErrorCode::LockTimeout) => {
            let db = db.clone();
            blocking(move || db.begin_write(wait, interactive)).await?.map_err(|e| e.into_db())
        }
        other => other.map_err(|e| e.into_db()),
    }
}

/// Waits, without holding a thread, until everything up to `version` is
/// on disk and visible to readers.
async fn durable(db: &Arc<Database>, version: u64) -> DbResult<()> {
    let (tell, told) = tokio::sync::oneshot::channel();
    let lead = db.when_durable(
        version,
        Box::new(move |outcome| {
            let _ = tell.send(outcome);
        }),
    );
    if lead {
        // Nobody is syncing the log, so this thread does, once: in a
        // quiet moment that is the whole of it, and the commit has not
        // changed threads. If more commits came meanwhile, a thread of
        // its own carries on for them.
        if db.sync_waiters(1) {
            let db = db.clone();
            tokio::task::spawn_blocking(move || {
                db.sync_waiters(usize::MAX);
            });
        }
    }
    told.await.map_err(|_| DbError::new(ErrorCode::Internal, "the commit's outcome was lost"))?
}

/// Hands a dump to the connection as it is produced.
struct ChunkWriter {
    out: mpsc::Sender<Vec<u8>>,
    buffer: Vec<u8>,
}

impl Write for ChunkWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        if self.buffer.len() >= 256 * 1024 {
            self.flush()?;
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if !self.buffer.is_empty() {
            // Waits when the connection reads slowly, so that the dump
            // is never held in memory.
            self.out.blocking_send(std::mem::take(&mut self.buffer)).map_err(|_| std::io::Error::other("the client went away"))?;
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.end();
    }
}

impl Session {
    fn greet(&mut self, version: u32, database: String, password: &str) -> DbResult<()> {
        if version != PROTOCOL_VERSION {
            return Err(DbError::new(ErrorCode::ProtocolMismatch, format!("the client speaks protocol {version}, the server {PROTOCOL_VERSION}")));
        }
        self.admin = same(&sha256(password.as_bytes()), &sha256(self.state.config.admin_password.as_bytes()));
        if database.is_empty() {
            return if self.admin { Ok(()) } else { Err(denied()) };
        }
        let allowed = self.admin || self.state.access.lock().unwrap().check(&database, password);
        let db = self.state.engine.database(&database).map_err(|_| denied())?;
        if !allowed {
            return Err(denied());
        }
        self.database = Some(db);
        self.name = database;
        Ok(())
    }

    fn database(&self) -> DbResult<Arc<Database>> {
        let db = self.database.clone().ok_or_else(|| DbError::invalid("this connection is on no database"))?;
        // The name may have been given to another database since.
        match self.state.engine.database(&self.name) {
            Ok(current) if Arc::ptr_eq(&current, &db) => Ok(db),
            _ => Err(DbError::new(ErrorCode::Unavailable, format!("database {:?} was dropped or replaced; reconnect", self.name))),
        }
    }

    /// Answers a request. `Err` means the connection must close.
    async fn handle(&mut self, request: Request, deadline: Option<Instant>, writer: &mut OwnedWriteHalf) -> Result<Vec<Response>, ()> {
        let answer = match request {
            Request::Admin(Admin::Dump { databases }) => return self.dump(databases, writer).await,
            other => self.answer(other, deadline).await,
        };
        Ok(answer.unwrap_or_else(|e| vec![Response::Error(e)]))
    }

    async fn answer(&mut self, request: Request, deadline: Option<Instant>) -> DbResult<Vec<Response>> {
        match request {
            Request::Hello { .. } => Err(DbError::invalid("already greeted")),
            Request::Ping => Ok(vec![Response::Ok]),
            Request::Begin { write, lock_wait_ms } => {
                if self.txn.is_some() {
                    return Err(DbError::invalid("a transaction is already open on this connection"));
                }
                let db = self.database()?;
                self.txn = Some(if write {
                    let wait = Duration::from_millis(lock_wait_ms);
                    let turn = self.turn(wait).await?;
                    // It will show its owner what it reads before it
                    // commits: all of that must be on disk first.
                    durable(&db, db.latest_version()).await?;
                    Txn::Write(begin_write(&db, wait, true).await?, turn)
                } else {
                    Txn::Read(db.begin_read())
                });
                Ok(vec![Response::Ok])
            }
            Request::Commit => match self.txn.take() {
                Some(Txn::Write(txn, turn)) => {
                    let db = self.database()?;
                    let committed = txn.commit_start().map_err(|e| e.into_db())?;
                    // The next writer may start while this reaches the disk.
                    drop(turn);
                    if let Some(committed) = committed {
                        durable(&db, committed.version()).await?;
                    }
                    Ok(vec![Response::Ok])
                }
                Some(Txn::Read(_)) => Ok(vec![Response::Ok]),
                None => Err(DbError::new(ErrorCode::TransactionAborted, "no transaction is open")),
            },
            Request::Rollback => {
                if let Some(txn) = self.txn.take() {
                    // Dropping it gives pages back: off the async thread.
                    blocking(move || drop(txn)).await?;
                }
                Ok(vec![Response::Ok])
            }
            Request::Now if self.txn.is_none() => Ok(vec![Response::Time(now_micros())]),
            Request::Lock { name, wait_ms } => self.lock(name, Duration::from_millis(wait_ms)).await,
            Request::Unlock { name } => {
                self.unlock(&name);
                Ok(vec![Response::Ok])
            }
            Request::Admin(admin) => self.admin(admin).await,
            statement => {
                let db = self.database()?;
                // A read is tried first on this thread, for a moment.
                // Most take microseconds, and handing one to another
                // thread would cost more than running it. One that proves
                // long is begun again where it may take its time: a read
                // changes nothing, so nothing is lost but the moment.
                if let Request::Query(q) = &statement {
                    let quick = Instant::now() + QUICK;
                    if deadline.is_none_or(|d| d > quick) {
                        let tried = match &mut self.txn {
                            Some(Txn::Read(txn)) => txn.query_within(q, quick),
                            Some(Txn::Write(txn, _)) => txn.query_within(q, quick),
                            None => db.begin_read().query_within(q, quick),
                        };
                        if let Some(set) = tried.map_err(|e| e.into_db())? {
                            return Ok(rows(set));
                        }
                    }
                }
                let brief = brief(&statement, self.request_bytes);
                match self.txn.take() {
                    Some(mut txn) => {
                        let (txn, result) = if brief {
                            let result = run_in_txn(&mut txn, &statement, deadline);
                            (txn, result)
                        } else {
                            blocking(move || {
                                let result = run_in_txn(&mut txn, &statement, deadline);
                                (txn, result)
                            })
                            .await?
                        };
                        self.txn = Some(txn);
                        result
                    }
                    // A statement alone is its own transaction: a write
                    // takes its turn, runs, and answers once it is on disk.
                    None if is_write(&statement) => {
                        let wait = self.state.config.lock_wait;
                        let turn = self.turn(wait).await?;
                        let mut txn = Txn::Write(begin_write(&db, wait, false).await?, turn);
                        let (txn, result) = if brief {
                            let result = run_in_txn(&mut txn, &statement, deadline);
                            (txn, result)
                        } else {
                            blocking(move || {
                                let result = run_in_txn(&mut txn, &statement, deadline);
                                (txn, result)
                            })
                            .await?
                        };
                        let Txn::Write(txn, turn) = txn else { unreachable!() };
                        // What the statement read may be a commit still on
                        // its way to the disk. Whatever it answers (rows, no
                        // rows, an error such as a key already taken) rests
                        // on that commit, so it waits for it like its own.
                        let base = txn.base();
                        let responses = match result {
                            Ok(responses) => responses,
                            Err(e) => {
                                // Undone off this thread: it may have much to give back.
                                blocking(move || drop(txn)).await?;
                                drop(turn);
                                durable(&db, base).await?;
                                return Err(e);
                            }
                        };
                        let committed = txn.commit_start().map_err(|e| e.into_db())?;
                        drop(turn);
                        durable(&db, committed.map_or(base, |c| c.version())).await?;
                        Ok(responses)
                    }
                    None => blocking(move || run_in_txn(&mut Txn::Read(db.begin_read()), &statement, deadline)).await?,
                }
            }
        }
    }

    /// Waits for this connection's turn to write to its database.
    async fn turn(&self, wait: Duration) -> DbResult<Turn> {
        let gate = self.state.gates.lock().unwrap().entry(self.name.clone()).or_default().clone();
        tokio::time::timeout(wait, gate.lock_owned()).await.map_err(|_| lock_timeout(wait))
    }

    async fn lock(&mut self, name: String, wait: Duration) -> DbResult<Vec<Response>> {
        let key = (self.name.clone(), name.clone());
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            // Ask to be told of a release before looking, so that one
            // between the look and the wait is not missed.
            let freed = self.state.lock_freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            {
                let mut locks = self.state.locks.lock().unwrap();
                match locks.get(&key) {
                    None => {
                        locks.insert(key, self.id);
                        self.held.push(name);
                        return Ok(vec![Response::Flag(true)]);
                    }
                    Some(owner) if *owner == self.id => return Ok(vec![Response::Flag(true)]),
                    Some(_) => {}
                }
            }
            if tokio::time::timeout_at(deadline, freed).await.is_err() {
                return Ok(vec![Response::Flag(false)]);
            }
        }
    }

    fn unlock(&mut self, name: &str) {
        if let Some(at) = self.held.iter().position(|h| h == name) {
            self.held.remove(at);
            self.state.locks.lock().unwrap().remove(&(self.name.clone(), name.to_string()));
            self.state.lock_freed.notify_waiters();
        }
    }

    /// Releases what the connection held when it ends. Called as the
    /// session is dropped, so that it also runs if the task serving the
    /// connection panics.
    fn end(&mut self) {
        // An open transaction rolls back as it is dropped.
        if let Some(txn) = self.txn.take() {
            tokio::task::spawn_blocking(move || drop(txn));
        }
        for name in std::mem::take(&mut self.held) {
            self.state.locks.lock().unwrap().remove(&(self.name.clone(), name));
        }
        self.state.lock_freed.notify_waiters();
    }

    /// The databases an admin request is about: the connection's own, or
    /// all of them on a connection that is on none.
    fn targets(&self) -> DbResult<Vec<Arc<Database>>> {
        match &self.database {
            Some(_) => Ok(vec![self.database()?]),
            None => self.state.engine.names().iter().map(|n| self.state.engine.database(n).map_err(|e| e.into_db())).collect(),
        }
    }

    async fn admin(&mut self, request: Admin) -> DbResult<Vec<Response>> {
        let state = self.state.clone();
        // A database's own password is enough to look after it.
        if !self.admin && !matches!(request, Admin::Checkpoint | Admin::Check | Admin::Stats) {
            return Err(admin_only());
        }
        match request {
            Admin::CreateDatabase { name, password, if_not_exists } => {
                if state.engine.database(&name).is_ok() {
                    return if if_not_exists { Ok(vec![Response::Ok]) } else { Err(DbError::new(ErrorCode::AlreadyExists, format!("database {name:?} already exists"))) };
                }
                blocking(move || {
                    state.engine.create_database(&name).map_err(|e| e.into_db())?;
                    state.access.lock().unwrap().set(&name, &password).map_err(|e| DbError::new(ErrorCode::Io, e.to_string()))
                })
                .await??;
                Ok(vec![Response::Ok])
            }
            Admin::DropDatabase { name, if_exists } => {
                if state.engine.database(&name).is_err() {
                    return if if_exists { Ok(vec![Response::Ok]) } else { Err(DbError::not_found(format!("no database {name:?}"))) };
                }
                blocking(move || {
                    state.engine.drop_database(&name).map_err(|e| e.into_db())?;
                    state.access.lock().unwrap().remove(&name).map_err(|e| DbError::new(ErrorCode::Io, e.to_string()))
                })
                .await??;
                Ok(vec![Response::Ok])
            }
            Admin::SetPassword { name, password } => {
                state.engine.database(&name).map_err(|e| e.into_db())?;
                blocking(move || state.access.lock().unwrap().set(&name, &password).map_err(|e| DbError::new(ErrorCode::Io, e.to_string()))).await??;
                Ok(vec![Response::Ok])
            }
            Admin::ListDatabases => Ok(vec![Response::Names(state.engine.names())]),
            Admin::Checkpoint => {
                let targets = self.targets()?;
                blocking(move || targets.iter().try_for_each(|db| db.checkpoint())).await?.map_err(|e| e.into_db())?;
                Ok(vec![Response::Ok])
            }
            Admin::Check => {
                let targets = self.targets()?;
                let report = blocking(move || targets.iter().map(|db| db.check()).collect::<Result<String, _>>()).await?.map_err(|e| e.into_db())?;
                Ok(vec![Response::Text(report)])
            }
            Admin::Stats => {
                let mut report: String = self.targets()?.iter().map(|db| db.stats()).collect();
                let (held, capacity) = state.engine.cache_usage();
                report.push_str(&format!("cache: {held} of {capacity} pages\n"));
                Ok(vec![Response::Text(report)])
            }
            Admin::RestoreBegin { replace } => {
                if self.restore.is_some() {
                    return Err(DbError::invalid("a restore is already under way on this connection"));
                }
                let (chunks, mut incoming) = mpsc::channel::<Vec<u8>>(4);
                let task = tokio::task::spawn_blocking(move || {
                    let mut restore = Restore::new(&state.engine, replace);
                    while let Some(chunk) = incoming.blocking_recv() {
                        restore.feed(&chunk).map_err(|e| e.into_db())?;
                    }
                    let restored = restore.finish().map_err(|e| e.into_db())?;
                    // The passwords come back with the databases.
                    let mut access = state.access.lock().unwrap();
                    for (name, note) in &restored {
                        access.set_note(name, note).map_err(|e| DbError::new(ErrorCode::Io, e.to_string()))?;
                    }
                    Ok(restored)
                });
                self.restore = Some((chunks, task));
                Ok(vec![Response::Ok])
            }
            Admin::RestoreChunk(bytes) => {
                let Some((chunks, _)) = &self.restore else { return Err(DbError::invalid("no restore is under way")) };
                if chunks.send(bytes).await.is_err() {
                    // The loader stopped: it has the reason.
                    let (_, task) = self.restore.take().unwrap();
                    return Err(task.await.map_err(|e| DbError::new(ErrorCode::Internal, e.to_string()))?.err().unwrap_or_else(|| DbError::new(ErrorCode::Internal, "the restore stopped early")));
                }
                Ok(vec![Response::Ok])
            }
            Admin::RestoreEnd => {
                let Some((chunks, task)) = self.restore.take() else { return Err(DbError::invalid("no restore is under way")) };
                drop(chunks);
                task.await.map_err(|e| DbError::new(ErrorCode::Internal, e.to_string()))??;
                Ok(vec![Response::Ok])
            }
            Admin::Dump { .. } => unreachable!("handled by the caller"),
        }
    }

    /// Streams a dump: chunks, then `Ok` or the error that stopped it.
    async fn dump(&mut self, databases: Vec<String>, writer: &mut OwnedWriteHalf) -> Result<Vec<Response>, ()> {
        // A database's own connection may dump that database.
        let own = self.database.is_some() && (databases.is_empty() || databases == [self.name.clone()]);
        if !self.admin && !own {
            return Ok(vec![Response::Error(admin_only())]);
        }
        let names = if own && !self.admin || (databases.is_empty() && self.database.is_some()) { vec![self.name.clone()] } else { databases };
        // The connection's own database must still be the one it signed
        // in to: a database restored over it since has another password,
        // which this connection never gave.
        if self.database.is_some() {
            if let Err(e) = self.database() {
                return Ok(vec![Response::Error(e)]);
            }
        }
        let state = self.state.clone();
        let (out, mut chunks) = mpsc::channel::<Vec<u8>>(4);
        let task = tokio::task::spawn_blocking(move || {
            let mut sink = ChunkWriter { out, buffer: Vec::new() };
            let note = |name: &str| state.access.lock().unwrap().note(name);
            dump::dump(&state.engine, &names, &note, &mut sink).map_err(|e| e.into_db())?;
            sink.flush().map_err(|e| DbError::new(ErrorCode::Connection, e.to_string()))
        });
        while let Some(chunk) = chunks.recv().await {
            if write_response(writer, &mut Vec::new(), &Response::Chunk(chunk)).await.is_err() {
                return Err(());
            }
        }
        Ok(vec![match task.await {
            Ok(Ok(())) => Response::Ok,
            Ok(Err(e)) => Response::Error(e),
            Err(e) => Response::Error(DbError::new(ErrorCode::Internal, e.to_string())),
        }])
    }
}
