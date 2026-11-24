//! One connection: its greeting, its transaction, its locks.
//!
//! **Writes take turns.** A database has one writer at a time, and a
//! connection's writes wait for their turn in the database's `Gate`. A
//! statement that is its own transaction, if it is small, is a job the
//! gate runs back to back with other connections' jobs. A long statement
//! and an exclusive transaction hold the turn themselves.
//!
//! **An optimistic transaction is a fork.** It runs on a private copy of
//! the database without the turn, so its statements, and the waits for
//! its owner between them, hold nobody up. The connection keeps what was
//! asked of it and a digest of each answer. At commit it takes the turn.
//! If nothing was committed since the fork began, the copy becomes the
//! next version as it stands. Otherwise the fork is dropped and the
//! requests are run again, alone, on the newest version: if each answers
//! as it did, the owner was told nothing that is no longer so, and the
//! second run is committed; if one does not, the transaction fails with
//! `Conflict` and nothing of it remains. A fork that grows large, or is
//! asked for something not worth doing twice, is settled the same way
//! early and carries on holding the turn.

use std::hash::Hasher;
use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use database_engine::db::{Database, ReadTxn, WriteTxn};
use database_engine::dump::{self, Restore};
use database_engine::eval::Drawn;
use database_protocol::error::{DbError, DbResult, ErrorCode};
use database_protocol::message::*;
use database_protocol::query::{Access, Bound, InsertSource, Query};
use database_protocol::time::now_micros;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};

use crate::access::{same, sha256};
use crate::gate::{Gate, Turn};
use crate::State;

/// What a statement answered, in a few bytes: enough to tell whether it
/// answers the same when asked again.
type Digest = (u64, u64, usize);

fn digest(responses: &[Response]) -> Digest {
    let mut bytes = Vec::new();
    for response in responses {
        bytes = encode_response_onto(bytes, response);
    }
    // Two hashes of 64 bits, keyed apart: 128 bits between an answer
    // that changed and a commit that should not have been.
    let mut first = std::collections::hash_map::DefaultHasher::new();
    first.write(&bytes);
    let mut second = std::collections::hash_map::DefaultHasher::new();
    second.write_u64(0x9E37_79B9_7F4A_7C15);
    second.write(&bytes);
    (first.finish(), second.finish(), bytes.len())
}

/// One request a fork answered, kept so that it can be asked again.
struct Step {
    request: Request,
    /// The random ids and counters' numbers the statement drew.
    drawn: Vec<Drawn>,
    outcome: Result<Digest, ErrorCode>,
}

/// A transaction that runs beside the writers: see the top of this file.
struct Fork {
    txn: WriteTxn,
    steps: Vec<Step>,
    /// The size of the requests kept in `steps`.
    bytes: usize,
    /// How long its statements took to run.
    spent: Duration,
    /// How long it waits for its turn, when it needs one.
    wait: Duration,
}

/// The most a fork's requests may take in memory before it is settled.
const FORK_BYTES: usize = 1024 * 1024;
/// The most requests it may hold.
const FORK_STEPS: usize = 4096;
/// The most pages it may hold in memory.
const FORK_PAGES: usize = 128;
/// A fork whose statements took longer than this to run is not run again
/// on a thread that serves other connections too.
const BRIEF_FORK: Duration = Duration::from_micros(500);

impl Fork {
    /// Whether the fork may run a statement as it is: one that is cheap
    /// to run twice, in a fork that is still small.
    fn takes(&self, request: &Request, bytes: usize) -> bool {
        bytes <= BRIEF_BYTES
            && self.bytes + bytes <= FORK_BYTES
            && self.steps.len() < FORK_STEPS
            && self.txn.private_pages() <= FORK_PAGES
            && match request {
                Request::Query(_) => true,
                Request::Delete(delete) => by_key(&delete.rows),
                Request::Ddl(_) => false,
                other => brief(other, bytes),
            }
    }

    /// Keeps a request the fork has answered.
    fn record(
        &mut self,
        request: Request,
        bytes: usize,
        outcome: &DbResult<Vec<Response>>,
        took: Duration,
    ) {
        let outcome = match outcome {
            Ok(responses) => Ok(digest(responses)),
            Err(e) => Err(e.code),
        };
        self.steps.push(Step {
            request,
            drawn: self.txn.take_drawn(),
            outcome,
        });
        self.bytes += bytes;
        self.spent += took;
    }
}

fn conflict() -> DbError {
    DbError::new(ErrorCode::Conflict, "another transaction's commit changed what this one had read: nothing of it took effect, and it can be run again")
}

enum Txn {
    Read(ReadTxn),
    /// Dropped in this order: the transaction, then its turn.
    Write(Box<WriteTxn>, Turn),
    Fork(Box<Fork>),
    /// The server ended the transaction, for the reason kept here, and
    /// the connection has yet to end it too.
    Dead(DbError),
}

type RestoreTask = tokio::task::JoinHandle<DbResult<Vec<(String, Vec<u8>)>>>;

struct Session {
    id: u64,
    state: Arc<State>,
    /// The database the connection is on. Empty for one that only
    /// administers.
    name: String,
    database: Option<Arc<Database>>,
    /// Its queue of writers, once the connection has written.
    gate: Option<Arc<Gate>>,
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

async fn read_frame(reader: &mut BufReader<OwnedReadHalf>, largest: usize) -> std::io::Result<Option<Vec<u8>>> {
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

/// What a connection reads from its socket at a time.
const READ_BUFFER: usize = 8 * 1024;

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
    let (reader, mut writer) = stream.into_split();
    // A small request arrives whole: its length and its body are then
    // one read from the socket, not two.
    let mut reader = BufReader::with_capacity(READ_BUFFER, reader);
    let mut session = Session { id, state: state.clone(), name: String::new(), database: None, gate: None, admin: false, txn: None, request_bytes: 0, held: Vec::new(), restore: None };

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
            // A fork holds a version as a reader does, and nobody up.
            Some(Txn::Read(_) | Txn::Fork(_) | Txn::Dead(_)) => state.config.idle_transaction.saturating_mul(IDLE_READ_FACTOR),
            None => Duration::from_secs(u64::from(u32::MAX)),
        };
        let len = tokio::select! {
            len = tokio::time::timeout(patience, budget::read_len(&mut reader)) => len,
            _ = stopping.changed() => break,
        };
        let Ok(Ok(Some(len))) = len else { break };
        // A large request's share of memory, taken before its bytes are
        // read (see `budget`). Whoever holds the turn between requests
        // does not wait for it.
        let holds_turn = matches!(session.txn, Some(Txn::Write(..)));
        let share = tokio::select! {
            share = budget::budget().take(len, !holds_turn) => share,
            _ = stopping.changed() => break,
        };
        let frame = tokio::select! {
            frame = tokio::time::timeout(patience, budget::read_body(&mut reader, len, MAX_FRAME)) => frame,
            _ = stopping.changed() => break,
        };
        let Ok(Ok(frame)) = frame else { break };
        state.requests.fetch_add(1, Ordering::Relaxed);
        let outcome = match decode_request(&frame) {
            Ok((request, timeout_ms)) => {
                session.request_bytes = frame.len();
                drop(frame);
                let deadline = (timeout_ms > 0).then(|| Instant::now() + Duration::from_millis(timeout_ms));
                // The statement of a bulk load goes a way of its own.
                match session.load(request, deadline).await {
                    Ok(answer) => Ok(answer),
                    Err(request) => session.handle(request, deadline, &mut writer).await,
                }
            }
            Err(e) => Ok(vec![Response::Error(e)]),
        };
        share.give_back();
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

mod budget;
mod load;

fn rows(set: RowSet) -> Vec<Response> {
    rows_responses(set)
}

fn read_only() -> DbError {
    DbError::new(
        ErrorCode::InvalidRequest,
        "a reading transaction cannot write",
    )
}

/// Runs one statement in a transaction that only reads.
fn run_read(
    txn: &ReadTxn,
    request: &Request,
    deadline: Option<Instant>,
) -> DbResult<Vec<Response>> {
    Ok(match request {
        Request::Query(q) => rows(txn.query(q, deadline).map_err(|e| e.into_db())?),
        Request::Describe => vec![Response::Tables(txn.describe())],
        Request::Now => vec![Response::Time(txn.now())],
        Request::Insert(_)
        | Request::Update(_)
        | Request::Delete(_)
        | Request::Ddl(_)
        | Request::Savepoint(_)
        | Request::RollbackTo(_)
        | Request::Release(_) => return Err(read_only()),
        _ => return Err(DbError::invalid("that request is not a statement")),
    })
}

/// Runs one statement in a transaction that may write.
fn run_write(
    txn: &mut WriteTxn,
    request: &Request,
    deadline: Option<Instant>,
) -> DbResult<Vec<Response>> {
    Ok(match request {
        Request::Query(q) => rows(txn.query(q, deadline).map_err(|e| e.into_db())?),
        Request::Insert(i) => rows(txn.insert(i, deadline).map_err(|e| e.into_db())?),
        Request::Update(u) => rows(txn.update(u, deadline).map_err(|e| e.into_db())?),
        Request::Delete(d) => rows(txn.delete(d, deadline).map_err(|e| e.into_db())?),
        Request::Ddl(ddl) => {
            txn.ddl(ddl, deadline).map_err(|e| e.into_db())?;
            vec![Response::Ok]
        }
        Request::Describe => vec![Response::Tables(txn.describe().map_err(|e| e.into_db())?)],
        Request::Now => vec![Response::Time(txn.now())],
        Request::Savepoint(name) => {
            txn.savepoint(name).map_err(|e| e.into_db())?;
            vec![Response::Ok]
        }
        Request::RollbackTo(name) => {
            txn.rollback_to(name).map_err(|e| e.into_db())?;
            vec![Response::Ok]
        }
        Request::Release(name) => {
            txn.release(name).map_err(|e| e.into_db())?;
            vec![Response::Ok]
        }
        _ => return Err(DbError::invalid("that request is not a statement")),
    })
}

fn is_write(request: &Request) -> bool {
    matches!(request, Request::Insert(_) | Request::Update(_) | Request::Delete(_) | Request::Ddl(_))
}

/// Whether a statement names its rows by their primary keys, a few of
/// them.
fn by_key(q: &Query) -> bool {
    q.joins.is_empty()
        && matches!(&q.access, Access::Primary { ranges, .. }
            if ranges.len() <= 8 && ranges.iter().all(|r| matches!((&r.lower, &r.upper), (Bound::Included(a), Bound::Included(b)) if a == b)))
}

/// The largest request that counts as small.
const BRIEF_BYTES: usize = 64 * 1024;

/// Whether a statement is sure to be over in a moment, and so may run on
/// a thread that serves other connections too: a few rows inserted, or
/// rows updated by their keys, in a request of ordinary size. A delete is
/// never counted so, for what it may take with it.
fn brief(request: &Request, bytes: usize) -> bool {
    bytes <= BRIEF_BYTES
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

fn is_lock_timeout(e: &database_engine::error::Error) -> bool {
    matches!(e, database_engine::error::Error::Db(e) if e.code == ErrorCode::LockTimeout)
}

/// Takes the engine's write lock, which is free to whoever has the turn
/// but for the moments a checkpoint or a check holds it: tried at once,
/// and waited for, with this thread's other tasks moved elsewhere, only
/// then.
fn begin_write(db: &Arc<Database>, wait: Duration, interactive: bool) -> DbResult<WriteTxn> {
    match db.begin_write(Duration::ZERO, interactive) {
        Err(e) if is_lock_timeout(&e) => {
            tokio::task::block_in_place(|| db.begin_write(wait, interactive)).map_err(|e| e.into_db())
        }
        other => other.map_err(|e| e.into_db()),
    }
}

/// Makes a fork the database's writer if it still can be: see
/// `WriteTxn::attach`. The caller has the turn.
fn attach(txn: &mut WriteTxn, wait: Duration) -> DbResult<bool> {
    match txn.attach(Duration::ZERO) {
        Err(e) if is_lock_timeout(&e) => {
            tokio::task::block_in_place(|| txn.attach(wait)).map_err(|e| e.into_db())
        }
        other => other.map_err(|e| e.into_db()),
    }
}

/// Runs a fork's requests again in `txn`, which is to stand for it.
/// Fails with `Conflict` if one answers otherwise than the fork's owner
/// was told.
fn replay(txn: &mut WriteTxn, steps: &[Step], deadline: Option<Instant>) -> DbResult<()> {
    for step in steps {
        txn.give(&step.drawn);
        let outcome = run_write(txn, &step.request, deadline);
        txn.take_drawn();
        let same = match (&outcome, &step.outcome) {
            (Ok(responses), Ok(seen)) => digest(responses) == *seen,
            (Err(e), Err(code)) => e.code == *code,
            _ => false,
        };
        if !same {
            return Err(match outcome {
                // Out of time says nothing of what has changed.
                Err(e) if e.code == ErrorCode::Timeout => e,
                _ => conflict(),
            });
        }
    }
    Ok(())
}

/// Runs a statement as a transaction of its own, with the turn held.
/// Returns its answer, and the version that must be on disk before the
/// answer is given: its own commit, or what it read if it made none.
/// Whatever it answers (rows, no rows, an error such as a key already
/// taken) may rest on a commit still on its way to the disk.
fn lone_write(
    db: &Arc<Database>,
    statement: &Request,
    deadline: Option<Instant>,
    wait: Duration,
) -> (DbResult<Vec<Response>>, u64) {
    let mut txn = match begin_write(db, wait, false) {
        Ok(txn) => txn,
        Err(e) => return (Err(e), 0),
    };
    let base = txn.base();
    match run_write(&mut txn, statement, deadline) {
        Ok(responses) => match txn.commit_start() {
            Ok(committed) => (Ok(responses), committed.map_or(base, |c| c.version())),
            Err(e) => (Err(e.into_db()), base),
        },
        Err(e) => {
            drop(txn);
            (Err(e), base)
        }
    }
}

/// Commits a fork, with the turn held: as it stands if nothing has been
/// committed since it began, and otherwise by running it again. Returns
/// the outcome and the version that must be on disk before it is told.
fn commit_fork(
    state: &State,
    db: &Arc<Database>,
    fork: Fork,
    deadline: Option<Instant>,
) -> (DbResult<()>, u64) {
    let (outcome, how) = commit_fork_as(db, fork, deadline);
    if let Some(how) = how {
        state.forks[how].fetch_add(1, Ordering::Relaxed);
    }
    outcome
}

/// As it stood, run again, or in conflict: see `State::forks`.
const AS_IT_STOOD: usize = 0;
const RUN_AGAIN: usize = 1;
const IN_CONFLICT: usize = 2;

fn commit_fork_as(
    db: &Arc<Database>,
    fork: Fork,
    deadline: Option<Instant>,
) -> ((DbResult<()>, u64), Option<usize>) {
    let (outcome, how) = commit_fork_how(db, fork, deadline);
    let how = match (&outcome.0, how) {
        (Ok(()), how) => Some(how),
        (Err(e), _) if e.code == ErrorCode::Conflict => Some(IN_CONFLICT),
        _ => None,
    };
    (outcome, how)
}

fn commit_fork_how(
    db: &Arc<Database>,
    fork: Fork,
    deadline: Option<Instant>,
) -> ((DbResult<()>, u64), usize) {
    let Fork {
        mut txn,
        steps,
        wait,
        ..
    } = fork;
    match attach(&mut txn, wait) {
        Err(e) => return ((Err(e), 0), AS_IT_STOOD),
        Ok(true) => {
            let base = txn.base();
            let outcome = match txn.commit_start() {
                Ok(committed) => (Ok(()), committed.map_or(base, |c| c.version())),
                Err(e) => (Err(e.into_db()), base),
            };
            return (outcome, AS_IT_STOOD);
        }
        Ok(false) => {}
    }
    let now = txn.now();
    drop(txn);
    let mut again = match begin_write(db, wait, false) {
        Ok(txn) => txn,
        Err(e) => return ((Err(e), 0), RUN_AGAIN),
    };
    again.set_now(now);
    let base = again.base();
    let outcome = match replay(&mut again, &steps, deadline) {
        Ok(()) => match again.commit_start() {
            Ok(committed) => (Ok(()), committed.map_or(base, |c| c.version())),
            Err(e) => (Err(e.into_db()), base),
        },
        Err(e) => {
            drop(again);
            (Err(e), base)
        }
    };
    (outcome, RUN_AGAIN)
}

/// Waits, without holding a thread, until everything up to `version` is
/// on disk and visible to readers.
async fn durable(db: &Arc<Database>, version: u64) -> DbResult<()> {
    if db.is_published(version) {
        return Ok(());
    }
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
            Request::Begin { kind, lock_wait_ms } => {
                if self.txn.is_some() {
                    return Err(DbError::invalid("a transaction is already open on this connection"));
                }
                let db = self.database()?;
                let wait = Duration::from_millis(lock_wait_ms);
                self.txn = Some(match kind {
                    TxnKind::Read => Txn::Read(db.begin_read()),
                    TxnKind::Exclusive => {
                        let turn = self.turn(wait).await?;
                        // It will show its owner what it reads before it
                        // commits: all of that must be on disk first.
                        durable(&db, db.latest_version()).await?;
                        Txn::Write(Box::new(begin_write(&db, wait, true)?), turn)
                    }
                    // A fork reads what is on disk, and waits for nobody.
                    TxnKind::Optimistic => Txn::Fork(Box::new(Fork {
                        txn: db.begin_fork().map_err(|e| e.into_db())?,
                        steps: Vec::new(),
                        bytes: 0,
                        spent: Duration::ZERO,
                        wait,
                    })),
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
                Some(Txn::Fork(fork)) => self.commit_fork(*fork, deadline).await,
                Some(Txn::Read(_)) => Ok(vec![Response::Ok]),
                Some(Txn::Dead(e)) => Err(e),
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
            statement => self.statement(statement, deadline).await,
        }
    }

    async fn statement(
        &mut self,
        statement: Request,
        deadline: Option<Instant>,
    ) -> DbResult<Vec<Response>> {
        let db = self.database()?;
        let bytes = self.request_bytes;
        match &self.txn {
            Some(Txn::Dead(e)) => {
                return Err(DbError::new(
                    ErrorCode::TransactionAborted,
                    format!(
                        "the transaction was rolled back and can only be ended: {}",
                        e.message
                    ),
                ))
            }
            // A fork asked for more than is worth doing twice becomes the
            // writer first.
            Some(Txn::Fork(fork)) if !fork.takes(&statement, bytes) => {
                self.settle(deadline).await?
            }
            _ => {}
        }
        // A read is tried first on this thread, for a moment. Most take
        // microseconds, and handing one to another thread would cost
        // more than running it. One that proves long is begun again
        // where it may take its time: a read changes nothing, so nothing
        // is lost but the moment.
        if let Request::Query(q) = &statement {
            let started = Instant::now();
            let quick = started + QUICK;
            if deadline.is_none_or(|d| d > quick) {
                let tried = match &mut self.txn {
                    Some(Txn::Read(txn)) => txn.query_within(q, quick),
                    Some(Txn::Write(txn, _)) => txn.query_within(q, quick),
                    Some(Txn::Fork(fork)) => fork.txn.query_within(q, quick),
                    Some(Txn::Dead(_)) => unreachable!("answered above"),
                    None => db.begin_read().query_within(q, quick),
                };
                let outcome: Option<DbResult<Vec<Response>>> = match tried {
                    Ok(Some(set)) => Some(Ok(rows(set))),
                    Ok(None) => None,
                    Err(e) => Some(Err(e.into_db())),
                };
                if let Some(outcome) = outcome {
                    if let Some(Txn::Fork(fork)) = &mut self.txn {
                        fork.record(statement, bytes, &outcome, started.elapsed());
                    }
                    return outcome;
                }
            }
        }
        let brief = brief(&statement, bytes);
        match self.txn.take() {
            Some(Txn::Fork(mut fork)) => {
                let started = Instant::now();
                let (mut fork, statement, result) = if brief {
                    let result = run_write(&mut fork.txn, &statement, deadline);
                    (fork, statement, result)
                } else {
                    blocking(move || {
                        let result = run_write(&mut fork.txn, &statement, deadline);
                        (fork, statement, result)
                    })
                    .await?
                };
                fork.record(statement, bytes, &result, started.elapsed());
                self.txn = Some(Txn::Fork(fork));
                result
            }
            Some(mut txn) => {
                let run = move |txn: &mut Txn| match txn {
                    Txn::Read(txn) => run_read(txn, &statement, deadline),
                    Txn::Write(txn, _) => run_write(txn, &statement, deadline),
                    Txn::Fork(_) | Txn::Dead(_) => unreachable!("answered above"),
                };
                let (txn, result) = if brief {
                    let result = run(&mut txn);
                    (txn, result)
                } else {
                    blocking(move || {
                        let result = run(&mut txn);
                        (txn, result)
                    })
                    .await?
                };
                self.txn = Some(txn);
                result
            }
            // A statement alone is its own transaction: a write takes
            // its turn, runs, and answers once it is on disk.
            None if is_write(&statement) => {
                let wait = self.state.config.lock_wait;
                let (result, version) = if brief {
                    let db = db.clone();
                    // The request comes back with the outcome, to be
                    // taken apart here and not while the turn is held.
                    let (outcome, _statement) = self
                        .in_turn(wait, move || {
                            let outcome = lone_write(&db, &statement, deadline, wait);
                            (outcome, statement)
                        })
                        .await?;
                    outcome
                } else {
                    let turn = self.turn(wait).await?;
                    let db = db.clone();
                    let outcome =
                        blocking(move || lone_write(&db, &statement, deadline, wait)).await?;
                    drop(turn);
                    outcome
                };
                durable(&db, version).await?;
                result
            }
            None => blocking(move || run_read(&db.begin_read(), &statement, deadline)).await?,
        }
    }

    /// Commits the connection's fork.
    async fn commit_fork(
        &mut self,
        fork: Fork,
        deadline: Option<Instant>,
    ) -> DbResult<Vec<Response>> {
        let db = self.database()?;
        if fork.txn.has_failed() {
            blocking(move || drop(fork)).await?;
            return Err(DbError::new(
                ErrorCode::TransactionAborted,
                "an earlier statement failed: the transaction can only be rolled back",
            ));
        }
        if !fork.txn.has_written() {
            // It only read, and all of it from one version that was on
            // disk: there is nothing to check and nothing to wait for.
            blocking(move || drop(fork)).await?;
            return Ok(vec![Response::Ok]);
        }
        let wait = fork.wait;
        let (result, version) = if fork.spent <= BRIEF_FORK {
            let (db, state) = (db.clone(), self.state.clone());
            self.in_turn(wait, move || commit_fork(&state, &db, fork, deadline))
                .await?
        } else {
            let turn = self.turn(wait).await?;
            let (db, state) = (db.clone(), self.state.clone());
            let outcome = blocking(move || commit_fork(&state, &db, fork, deadline)).await?;
            drop(turn);
            outcome
        };
        durable(&db, version).await?;
        result.map(|()| vec![Response::Ok])
    }

    /// Makes the connection's fork the database's writer, holding the
    /// turn from here on: as it stands, or run again on the newest
    /// version. If that finds a conflict the transaction is dead.
    async fn settle(&mut self, deadline: Option<Instant>) -> DbResult<()> {
        let Some(Txn::Fork(fork)) = self.txn.take() else {
            return Ok(());
        };
        match self.settle_fork(*fork, deadline).await {
            Ok((txn, turn)) => {
                self.txn = Some(Txn::Write(Box::new(txn), turn));
                Ok(())
            }
            Err(e) => {
                self.txn = Some(Txn::Dead(e.clone()));
                Err(e)
            }
        }
    }

    async fn settle_fork(
        &mut self,
        fork: Fork,
        deadline: Option<Instant>,
    ) -> DbResult<(WriteTxn, Turn)> {
        let db = self.database()?;
        let wait = fork.wait;
        let turn = self.turn(wait).await?;
        // From here on it shows its owner the newest version, which must
        // be on disk first.
        durable(&db, db.latest_version()).await?;
        let txn = blocking(move || {
            let Fork { mut txn, steps, .. } = fork;
            if attach(&mut txn, wait)? {
                return Ok(txn);
            }
            let now = txn.now();
            drop(txn);
            let mut again = begin_write(&db, wait, true)?;
            again.set_now(now);
            replay(&mut again, &steps, deadline)?;
            Ok::<_, DbError>(again)
        })
        .await??;
        Ok((txn, turn))
    }

    fn gate(&mut self) -> Arc<Gate> {
        match &self.gate {
            Some(gate) => gate.clone(),
            None => {
                let gate = self
                    .state
                    .gates
                    .lock()
                    .unwrap()
                    .entry(self.name.clone())
                    .or_insert_with(Gate::new)
                    .clone();
                self.gate = Some(gate.clone());
                gate
            }
        }
    }

    /// Waits for this connection's turn to write to its database.
    async fn turn(&mut self, wait: Duration) -> DbResult<Turn> {
        self.gate()
            .turn(wait)
            .await
            .ok_or_else(|| lock_timeout(wait))
    }

    /// Runs `work` when the connection's turn to write comes, wherever
    /// the turn finds it (see `Gate::run`), and returns what it returned.
    /// `work` must be brief. Fails if the turn has not come within `wait`.
    async fn in_turn<T: Send + 'static>(
        &mut self,
        wait: Duration,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> DbResult<T> {
        const WAITING: u8 = 0;
        const BEGUN: u8 = 1;
        const GIVEN_UP: u8 = 2;
        let lost = || DbError::new(ErrorCode::Internal, "the outcome of a write was lost");
        let state = Arc::new(AtomicU8::new(WAITING));
        let (tell, mut told) = oneshot::channel();
        let job_state = state.clone();
        self.gate().run(Box::new(move || {
            // Not if its connection has stopped waiting for it.
            if job_state
                .compare_exchange(WAITING, BEGUN, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let _ = tell.send(work());
            }
        }));
        // With nobody ahead of it, it has run already.
        match told.try_recv() {
            Ok(outcome) => return Ok(outcome),
            Err(oneshot::error::TryRecvError::Closed) => return Err(lost()),
            Err(oneshot::error::TryRecvError::Empty) => {}
        }
        match tokio::time::timeout(wait, &mut told).await {
            Ok(outcome) => outcome.map_err(|_| lost()),
            Err(_)
                if state
                    .compare_exchange(WAITING, GIVEN_UP, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok() =>
            {
                Err(lock_timeout(wait))
            }
            // It began just as the wait ran out: it will finish.
            Err(_) => told.await.map_err(|_| lost()),
        }
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
                let forks = |how: usize| state.forks[how].load(Ordering::Relaxed);
                report.push_str(&format!("optimistic transactions: {} committed as they stood, {} run again, {} in conflict\n", forks(AS_IT_STOOD), forks(RUN_AGAIN), forks(IN_CONFLICT)));
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
