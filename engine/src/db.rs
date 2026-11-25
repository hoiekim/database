//! A database: one data file, its log, and the transactions on them.
//!
//! **Versions.** Each commit makes a new version: a catalog whose trees
//! share every unchanged page with the version before. Readers hold a
//! version and read it undisturbed for as long as they like.
//!
//! **One writer.** A writing transaction takes the database's write lock
//! and works on a private copy of the newest catalog. Nothing it does is
//! visible, or even reachable, until it commits; a rollback just drops
//! the copy and the pages it wrote.
//!
//! **Commit.** The transaction's record is appended to the log, its pages
//! join the store, and the write lock is released at once, so the next
//! writer can start. Then the committer waits for the log to be synced
//! (one sync serves every commit appended meanwhile), and only then is
//! the version published to readers and the client told. So a reader
//! never sees, and a client is never told of, a commit that a crash could
//! take back.
//!
//! **Forks.** A transaction that talks to its owner between statements
//! would hold the write lock while it waits for the network. It may run
//! as a fork instead: on a private copy of the newest durable catalog,
//! without the lock, beside the writer and other forks. Nobody sees what
//! it writes. To commit it takes the lock, and if no commit was made
//! since it began, its copy is simply the next version (`attach`). If
//! one was, the fork is dropped, and its owner runs the same statements
//! again in an ordinary transaction, checking that each answers as it
//! did: then nothing the owner was told has changed, and committing is
//! the same as having run alone at that moment. The values statements
//! draw rather than derive (random ids, counters) are kept for that
//! second run: see `eval::Tape`.
//!
//! **Recovery.** Open the data file as of its last checkpoint, apply the
//! log's records in order, checkpoint. A torn record at the log's end was
//! never acknowledged and is ignored.
//!
//! **The archive.** With one configured (`archive.rs`), a checkpoint does
//! not remove the log files it has passed: it copies each to the archive
//! first, and removes it once the copy is whole there. A copy that fails
//! leaves the file where it is, to be tried again by the next
//! checkpoint, so that every acknowledged commit is in the archive or in
//! a log file of the data directory. Nothing of this is on a commit's
//! way but the time its record is stamped with.
//!
//! **Following.** A database may be a standby's copy of another
//! server's: it takes no writes of its own and is given its primary's
//! log instead, the bytes of each log file as they lie there
//! (`receive`). It writes them to a file of the same number at the same
//! places, syncs, and applies the records that are whole, as recovery
//! applies them, each batch as one version. So its files are at every
//! moment a prefix of the primary's, a crash leaves it a shorter prefix
//! that its own recovery reads, and its readers see the primary as of
//! some commit. It checkpoints where the primary's log passes from one
//! file to the next, which is the one place where a checkpoint can say
//! which log continues from it.
//!
//! **Merges beside the writers.** A full-text index is kept in segments
//! that are merged as they gather (`fts.rs`), and merging reads nothing
//! that changes. So it is done beside the writers, as a fork's work is:
//! on a version held as a reader holds one, into pages lent by the
//! store. Only putting the merged tree in place is a commit, made as
//! the writer, and it is made only if the segments read are still as
//! they were. It changes no row, puts nothing in the log, and after a
//! crash is simply not there: the log's rows are indexed again as they
//! are replayed, and their segments merged again.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use database_protocol::error::{DbError, ErrorCode};
use database_protocol::message::{Ddl, RowSet};
use database_protocol::query::{Delete, Insert, Query, Update};
use database_protocol::schema::TableDef;
use database_protocol::time::now_micros;
use database_protocol::value::Value;

use crate::archive::{self, Archive, Chain, Progress};
use crate::btree::{self, Pages};
use crate::cache::PageCache;
use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::eval::{Cost, Ctx, Drawn, Read, Scope};
use crate::exec;
use crate::page::{PageId, FIRST_DATA_PAGE};
use crate::row::Row;
use crate::store::{Limits, Mark, Store, Writer};
use crate::vfs::Vfs;
use crate::wal::{self, Wal};
use crate::write::Tx;

#[derive(Clone, Debug)]
pub struct Options {
    /// Whether a commit waits for the disk before it is acknowledged.
    pub sync: bool,
    pub limits: Limits,
    /// A checkpoint is due once the log is this long.
    pub checkpoint_bytes: u64,
    /// Where log files are copied before a checkpoint removes them, and
    /// bases written: see `archive.rs`. `None` keeps no log.
    pub archive: Option<Arc<Archive>>,
    /// With an archive: a checkpoint is due once the oldest commit of
    /// the current log file has waited this long, so that a commit is
    /// not much longer than this in the data directory alone. Zero
    /// leaves checkpoints to come when they otherwise would.
    pub archive_every: Duration,
    /// How many bytes of log files a checkpoint has passed are kept in
    /// the data directory all the same, the newest first, for standbys
    /// to be sent: one that was away (its primary restarted, the
    /// network cut) asks for the log from where it had got to. Zero
    /// keeps none.
    pub keep_log_bytes: u64,
}

impl Default for Options {
    fn default() -> Self {
        Options { sync: true, limits: Limits::default(), checkpoint_bytes: 8 * 1024 * 1024, archive: None, archive_every: Duration::from_secs(60), keep_log_bytes: 0 }
    }
}

/// A database's place in the archive, and how far its log has got there.
struct Archiving {
    archive: Arc<Archive>,
    history: String,
    progress: Mutex<Progress>,
}

/// One version of the database.
pub struct Snapshot {
    pub version: u64,
    pub catalog: Arc<Catalog>,
}

pub struct Database {
    name: String,
    dir: PathBuf,
    vfs: Arc<dyn Vfs>,
    store: Arc<Store>,
    wal: Wal,
    /// Whether a writing transaction holds the database, and how many
    /// threads wait for it to let go.
    writing: Mutex<(bool, usize)>,
    write_free: Condvar,
    /// The newest committed version, durable or not: what the next
    /// writer builds on.
    latest: Mutex<Arc<Snapshot>>,
    /// The newest durable version: what readers see.
    published: Mutex<Arc<Snapshot>>,
    /// Versions committed and not yet durable, oldest first.
    pending: Mutex<VecDeque<Arc<Snapshot>>>,
    /// Commits whose owners asked to be told when they are durable.
    waiters: Mutex<Waiters>,
    checkpointing: Mutex<()>,
    /// Set when the disk failed under a commit or a checkpoint. Nothing
    /// more is accepted: what is on disk is still consistent, and a
    /// restart recovers it.
    failed: AtomicBool,
    options: Options,
    last_commit: Mutex<Instant>,
    commits_since_checkpoint: AtomicU64,
    pub commits: AtomicU64,
    /// Set by whoever merges full-text segments beside this database's
    /// writers, and called when a commit leaves segments due. Until it
    /// is set, writers merge what they make due themselves.
    merger: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>>,
    /// Held while segments are merged beside the writers: one merge at
    /// a time, and none once the database is retired.
    merging: Mutex<()>,
    /// Merges made beside the writers and put in place; and merges
    /// dropped, because the segments they read had changed meanwhile.
    pub merges_beside: AtomicU64,
    pub merges_dropped: AtomicU64,
    archiving: Option<Archiving>,
    /// A time no commit in the data file's checkpoint was made after,
    /// and none in the log since before: what a base taken of that
    /// checkpoint says of itself.
    moment: AtomicI64,
    /// When the first commit since the last checkpoint was made.
    first_commit: Mutex<Instant>,
    /// When the archive last refused a file, for trying again.
    archive_failed: Mutex<Option<Instant>>,
    /// Set for a database that follows another server's: see `receive`.
    following: Option<Mutex<Mirror>>,
    /// The oldest log file each reader of the log still needs: a
    /// standby being sent it, or one that was just sent a base. Log
    /// files from the least of these on are not removed.
    log_pins: Mutex<(u64, std::collections::BTreeMap<u64, u64>)>,
}

/// How far a following database has got in its primary's log.
struct Mirror {
    /// Bytes of the current log file that are written and synced here
    /// and are not yet a whole record.
    pending: Vec<u8>,
    /// Where the records applied so far end in the current log file.
    applied: u64,
    /// Where the records of the log file before ended: 0 if it held
    /// none, or it is not known.
    previous: u64,
    /// The primary's own place in its log when it was last heard from,
    /// its clock then, and when that was by this server's.
    source: Option<((u64, u64), i64, Instant)>,
}

/// How a following database stands, for `stats`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Followed {
    /// Where the copy of the primary's log ends here: the file and the
    /// byte. Everything before it is on this server's disk.
    pub received: (u64, u64),
    /// Where the records applied, which readers see, end in that file.
    pub applied: u64,
    /// The primary's place when it was last heard from, the time its
    /// clock gave then, and how long ago that was.
    pub source: Option<((u64, u64), i64, Duration)>,
}

/// A hold on the log files from one on: see `Database::pin_log`.
pub struct LogPin {
    db: Arc<Database>,
    id: u64,
}

impl LogPin {
    /// Moves the hold forward: files before `generation` may go.
    pub fn advance(&self, generation: u64) {
        self.db.log_pins.lock().unwrap().1.insert(self.id, generation);
    }
}

impl Drop for LogPin {
    fn drop(&mut self) {
        self.db.log_pins.lock().unwrap().1.remove(&self.id);
    }
}

/// What a reader of the log is given next: see `Database::read_log`.
#[derive(Debug, PartialEq, Eq)]
pub enum LogRead {
    /// Bytes of the log file asked for, from the place asked for.
    Bytes(Vec<u8>),
    /// That file holds no more and never will: the next one follows.
    Next,
    /// Nothing more is on disk yet.
    Wait,
}

pub(crate) fn data_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.db"))
}

/// How long after the archive refused a file it is offered again.
const ARCHIVE_RETRY: Duration = Duration::from_secs(5);

/// Ends the name a data file is made under, before it is whole.
pub const CREATING: &str = "--creating";

pub(crate) fn random_uuid() -> [u8; 16] {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    let filled = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).is_ok();
    if !filled {
        let mut rng = crate::vfs::Rng::new(now_micros() as u64 ^ (&bytes as *const _ as u64));
        bytes[..8].copy_from_slice(&rng.next().to_le_bytes());
        bytes[8..].copy_from_slice(&rng.next().to_le_bytes());
    }
    bytes
}

/// Called with a commit's fate, once, by whichever thread learns it.
pub type Done = Box<dyn FnOnce(std::result::Result<(), DbError>) + Send>;

struct Waiters {
    list: Vec<(u64, Done)>,
    /// Whether someone has taken on syncing the log for them.
    syncing: bool,
}

/// A commit that is in the log, though perhaps not yet on disk. Readers
/// cannot see it, and its owner must not be told of it, until `wait`
/// returns or `Database::when_durable` calls back.
#[must_use]
pub struct Committed {
    db: Arc<Database>,
    version: u64,
}

impl Committed {
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Blocks until the commit is on disk and visible to every reader.
    pub fn wait(self) -> Result<()> {
        let synced = self.db.fatal(self.db.wal.sync_to(self.version))?;
        self.db.publish_up_to(synced);
        // Synced into a file that was deleted meanwhile is not durable.
        self.db.check_failed()
    }
}

struct WriteGuard(Arc<Database>);

impl Drop for WriteGuard {
    fn drop(&mut self) {
        let mut writing = self.0.writing.lock().unwrap();
        writing.0 = false;
        // Waking is a call into the kernel: only when someone waits,
        // which between writers that queue for their turn is seldom.
        if writing.1 > 0 {
            self.0.write_free.notify_one();
        }
    }
}

impl Database {
    /// Makes a new, empty database in `dir`.
    pub fn create(vfs: Arc<dyn Vfs>, dir: &Path, name: &str, cache: Arc<PageCache>, store_id: u32, options: Options) -> Result<Arc<Database>> {
        let path = data_path(dir, name);
        if vfs.exists(&path) {
            return Err(Error::db(ErrorCode::AlreadyExists, format!("database {name:?} already exists")));
        }
        let uuid = random_uuid();
        // Log files left by an earlier database of this name are not ours.
        wal::remove_logs(&*vfs, dir, name, |_| true)?;
        // The data file is made under another name and given its own
        // only when it is whole and on disk. A process killed before
        // that leaves a file the next start sweeps away; under its own
        // name, a file without a header would be a database that cannot
        // be opened, and a server that cannot start.
        let making = dir.join(format!("{name}{CREATING}.db"));
        if vfs.exists(&making) {
            vfs.remove(&making)?;
        }
        let file = vfs.open(&making, true)?;
        let store = Arc::new(Store::create(file, cache, store_id, uuid, options.limits)?);
        let mut wal = Wal::create(vfs.clone(), dir, name, uuid, 1, 0, options.sync)?;
        if options.archive.is_some() {
            wal = wal.stamping(0);
        }
        vfs.rename(&making, &path)?;
        vfs.sync_dir(dir)?;
        let catalog = Catalog { next_table_id: 1, ..Catalog::default() };
        let db = Self::assemble(vfs, dir, name, store, wal, 0, catalog, options, None);
        // The archive begins with the database: a base of it empty.
        db.moment.store(db.wal.moment(), Ordering::Relaxed);
        db.fatal(db.settle_logs(1, false))?;
        Ok(db)
    }

    /// Opens a database, replaying whatever its log holds beyond the
    /// last checkpoint.
    pub fn open(vfs: Arc<dyn Vfs>, dir: &Path, name: &str, cache: Arc<PageCache>, store_id: u32, options: Options) -> Result<Arc<Database>> {
        Self::open_as(vfs, dir, name, cache, store_id, options, false)
    }

    /// Opens a database that follows another server's (see `receive`):
    /// replays the copy of the primary's log that is here, and goes on
    /// from where its records end. Unlike `open` it begins no log file
    /// and writes no checkpoint, since both would say that the log
    /// continues somewhere the primary's does not.
    pub fn open_following(vfs: Arc<dyn Vfs>, dir: &Path, name: &str, cache: Arc<PageCache>, store_id: u32, mut options: Options) -> Result<Arc<Database>> {
        options.archive = None;
        options.keep_log_bytes = 0;
        Self::open_as(vfs, dir, name, cache, store_id, options, true)
    }

    fn open_as(vfs: Arc<dyn Vfs>, dir: &Path, name: &str, cache: Arc<PageCache>, store_id: u32, options: Options, following: bool) -> Result<Arc<Database>> {
        let file = vfs.open(&data_path(dir, name), false)?;
        let (store, meta, catalog_bytes) = Store::open(file, cache, store_id, options.limits)?;
        let store = Arc::new(store);
        let catalog = Catalog::decode(&catalog_bytes)?;
        // Log files from before the checkpoint were only not yet deleted,
        // or, with an archive, not yet copied there: those are kept, and
        // are no part of recovery.
        if options.archive.is_none() && options.keep_log_bytes == 0 {
            wal::remove_logs(&*vfs, dir, name, |g| g < meta.wal_gen)?;
        }
        let generations: Vec<u64> = wal::list_logs(&*vfs, dir, name)?.into_iter().filter(|g| *g >= meta.wal_gen).collect();

        let mut tx = Tx::new(Writer::new(store.clone()), catalog, now_micros());
        tx.logging = false;
        let mut changed = false;
        // Tables that need not survive a restart start empty.
        let volatile: Vec<String> = tx.catalog.tables.values().filter(|t| t.volatile && (t.rows > 0 || t.root != 0)).map(|t| t.name.clone()).collect();
        for table in volatile {
            tx.truncate(&table)?;
            changed = true;
        }
        // A log file is synced whole before the next is begun, so only
        // the newest can end in a torn record, or short of where its
        // records ended when the next was begun, which the next one
        // says. Records in a file after one that did mean commits were
        // lost in between, and applying the later ones without them
        // would give a database that never existed.
        let gap = |damaged: u64| Error::corrupt(format!("log file {damaged} of database {name:?} is damaged before its end, and commits were made after it"));
        let mut torn: Option<u64> = None;
        // The file before: its generation, and where its records ended
        // if it held any.
        let mut before: Option<(u64, u64)> = None;
        // The newest file, and where what was read of it ends.
        let mut last: Option<(u64, u64)> = None;
        // The newest time a record says its commit was made.
        let mut stamped = 0i64;
        for generation in &generations {
            let mut records = 0u64;
            let read = wal::replay(&*vfs, dir, name, &meta.uuid, *generation, &mut |record| {
                if let Some(damaged) = torn {
                    return Err(gap(damaged));
                }
                records += 1;
                changed = true;
                stamped = stamped.max(record.time.unwrap_or(0));
                // A record applied once when it was committed. If it does
                // not apply now, the data file is not the one it was
                // logged against: a disk lost pages it said were synced.
                tx.replay(record.changes).map_err(|e| match e {
                    Error::Io(_) | Error::Corrupt(_) => e,
                    other => Error::corrupt(format!("log file {generation} of database {name:?} does not apply to its data file: {other}")),
                })
            })?;
            if read.after > 0 {
                let whole = match before {
                    Some((it, ended)) if it + 1 == *generation => ended == read.after,
                    // Not there: removed, if the checkpoint covers it.
                    _ => *generation <= meta.wal_gen,
                };
                if !whole {
                    if records > 0 {
                        return Err(gap(generation - 1));
                    }
                    torn.get_or_insert(generation - 1);
                }
            }
            if read.end == wal::End::Torn {
                torn.get_or_insert(*generation);
            }
            before = Some((*generation, if records > 0 { read.at } else { 0 }));
            last = Some((*generation, read.at));
        }
        let mut version = meta.version;
        let Tx { writer, catalog, .. } = tx;
        if changed {
            version += 1;
            writer.commit(version)?;
            store.publish(version);
        } else {
            writer.rollback();
        }
        if following {
            // The copy goes on in the file it had got to, after the
            // last record that is whole: a tail the crash tore is cut
            // off, and the primary sends those bytes again.
            let (generation, at) = last.unwrap_or((meta.wal_gen, 0));
            let wal = Wal::resume(vfs.clone(), dir, name, meta.uuid, generation, version, options.sync, at)?;
            let mirror = Mirror { pending: Vec::new(), applied: wal.position().1, previous: 0, source: None };
            return Ok(Self::assemble(vfs, dir, name, store, wal, version, catalog, options, Some(mirror)));
        }
        let next_generation = generations.last().map_or(meta.wal_gen, |g| g + 1).max(meta.wal_gen);
        let previous = before.filter(|(it, _)| it + 1 == next_generation).map_or(0, |(_, ended)| ended);
        let mut wal = Wal::create_after(vfs.clone(), dir, name, meta.uuid, next_generation, version, options.sync, previous)?;
        if options.archive.is_some() {
            wal = wal.stamping(stamped);
        }
        let db = Self::assemble(vfs.clone(), dir, name, store, wal, version, catalog, options, None);
        db.moment.store(db.wal.moment(), Ordering::Relaxed);
        if changed {
            // Put what was replayed into the data file now, so the log
            // it came from can go.
            let latest = db.latest.lock().unwrap().clone();
            let capture = db.store.capture(version, &latest.catalog.encode(), next_generation)?;
            db.store.write_checkpoint(capture)?;
        }
        db.settle_logs(next_generation, false)?;
        Ok(db)
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble(vfs: Arc<dyn Vfs>, dir: &Path, name: &str, store: Arc<Store>, wal: Wal, version: u64, catalog: Catalog, options: Options, following: Option<Mirror>) -> Arc<Database> {
        let snapshot = Arc::new(Snapshot { version, catalog: Arc::new(catalog) });
        let archiving = options.archive.clone().map(|archive| Archiving { history: archive::history_name(name, &store.meta().uuid), archive, progress: Mutex::new(Progress::default()) });
        Arc::new(Database {
            name: name.to_string(),
            dir: dir.to_path_buf(),
            vfs,
            store,
            wal,
            writing: Mutex::new((false, 0)),
            write_free: Condvar::new(),
            latest: Mutex::new(snapshot.clone()),
            published: Mutex::new(snapshot),
            pending: Mutex::new(VecDeque::new()),
            waiters: Mutex::new(Waiters { list: Vec::new(), syncing: false }),
            checkpointing: Mutex::new(()),
            failed: AtomicBool::new(false),
            options,
            last_commit: Mutex::new(Instant::now()),
            commits_since_checkpoint: AtomicU64::new(0),
            commits: AtomicU64::new(0),
            merger: std::sync::OnceLock::new(),
            merging: Mutex::new(()),
            merges_beside: AtomicU64::new(0),
            merges_dropped: AtomicU64::new(0),
            archiving,
            moment: AtomicI64::new(0),
            first_commit: Mutex::new(Instant::now()),
            archive_failed: Mutex::new(None),
            following: following.map(Mutex::new),
            log_pins: Mutex::new((0, std::collections::BTreeMap::new())),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    fn check_failed(&self) -> Result<()> {
        if self.failed.load(Ordering::Relaxed) {
            return Err(Error::db(ErrorCode::Unavailable, format!("database {:?} is no longer available: it was dropped or replaced, or it stopped after a disk error", self.name)));
        }
        Ok(())
    }

    /// A database that follows another takes no writes of its own.
    fn check_writable(&self) -> Result<()> {
        if self.following.is_some() {
            return Err(Error::db(ErrorCode::ReadOnly, format!("database {:?} follows another server's and takes no writes: write to the primary, or promote this one", self.name)));
        }
        Ok(())
    }

    /// Marks the database failed if `result` is a disk error.
    fn fatal<T>(&self, result: Result<T>) -> Result<T> {
        if matches!(result, Err(Error::Io(_) | Error::Corrupt(_))) {
            self.failed.store(true, Ordering::Relaxed);
        }
        result
    }

    /// Refuses everything from now on: the database is being dropped or
    /// replaced.
    pub fn retire(&self) {
        self.failed.store(true, Ordering::Relaxed);
        // A checkpoint already under way ends by deleting log files by
        // name: it must be done before the name can be given to another
        // database. None starts after this, the database having failed.
        drop(self.checkpointing.lock().unwrap());
        // Likewise a merge beside the writers, which writes pages to the
        // data file: it gives up when it sees the database retired, and
        // must have, before the file can be another database's.
        drop(self.merging.lock().unwrap());
    }

    pub fn has_failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    fn lock_writes(self: &Arc<Self>, wait: Duration) -> Result<WriteGuard> {
        let deadline = Instant::now() + wait;
        let mut writing = self.writing.lock().unwrap();
        while writing.0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::db(ErrorCode::LockTimeout, format!("waited {wait:?} for the write lock of {:?}", self.name)));
            }
            writing.1 += 1;
            writing = self.write_free.wait_timeout(writing, left).unwrap().0;
            writing.1 -= 1;
        }
        writing.0 = true;
        Ok(WriteGuard(self.clone()))
    }

    /// Makes every pending version up to `version` visible to readers.
    fn publish_up_to(&self, version: u64) {
        let mut published = self.published.lock().unwrap();
        let mut pending = self.pending.lock().unwrap();
        while pending.front().is_some_and(|s| s.version <= version) {
            let snapshot = pending.pop_front().unwrap();
            if snapshot.version > published.version {
                self.store.publish(snapshot.version);
                *published = snapshot;
            }
        }
    }

    /// The newest committed version, durable or not.
    pub fn latest_version(&self) -> u64 {
        self.latest.lock().unwrap().version
    }

    /// Whether `version` is on disk and visible to readers.
    pub fn is_published(&self, version: u64) -> bool {
        self.published.lock().unwrap().version >= version
    }

    /// Arranges for `done` to be called once everything up to `version`
    /// is on disk and visible to readers: at once if it already is.
    ///
    /// This is how a commit is awaited without holding a thread. Nothing
    /// syncs the log on its own, so when this returns true the caller
    /// has become the one to do it, and must call `sync_waiters`.
    pub fn when_durable(&self, version: u64, done: Done) -> bool {
        let mut waiters = self.waiters.lock().unwrap();
        if self.published.lock().unwrap().version >= version {
            drop(waiters);
            done(Ok(()));
            return false;
        }
        waiters.list.push((version, done));
        !std::mem::replace(&mut waiters.syncing, true)
    }

    /// Syncs the log and tells the waiters it covered, up to `rounds`
    /// times while some remain. Returns whether some still do: then the
    /// caller must call this again (from a thread that may block for
    /// long), for nobody else will.
    pub fn sync_waiters(&self, rounds: usize) -> bool {
        for _ in 0..rounds {
            let target = {
                let mut waiters = self.waiters.lock().unwrap();
                match waiters.list.iter().map(|(version, _)| *version).max() {
                    Some(target) => target,
                    None => {
                        waiters.syncing = false;
                        return false;
                    }
                }
            };
            let outcome = self.fatal(self.wal.sync_to(target)).and_then(|synced| self.check_failed().map(|()| synced)).map_err(Error::into_db);
            // Published first, then told: a waiter that arrives between
            // the two finds its version published and is told at once.
            if let Ok(synced) = &outcome {
                self.publish_up_to(*synced);
            }
            let ready: Vec<(u64, Done)> = {
                let mut waiters = self.waiters.lock().unwrap();
                match &outcome {
                    Ok(synced) => {
                        let (ready, rest) = std::mem::take(&mut waiters.list).into_iter().partition(|(version, _)| version <= synced);
                        waiters.list = rest;
                        ready
                    }
                    Err(_) => std::mem::take(&mut waiters.list),
                }
            };
            for (_, done) in ready {
                done(outcome.clone().map(|_| ()));
            }
        }
        let mut waiters = self.waiters.lock().unwrap();
        if waiters.list.is_empty() {
            waiters.syncing = false;
        }
        waiters.syncing
    }

    /// Starts a transaction that reads the newest durable version.
    pub fn begin_read(self: &Arc<Self>) -> ReadTxn {
        let published = self.published.lock().unwrap();
        let pinned = self.store.pin();
        debug_assert_eq!(pinned, published.version);
        ReadTxn { db: self.clone(), snapshot: published.clone(), pinned, now: now_micros(), cost: Cost::default() }
    }

    /// Starts a transaction that may write, waiting up to `wait` for the
    /// write lock. `interactive` says the caller will see results before
    /// it commits: then everything it could read is made durable first.
    pub fn begin_write(self: &Arc<Self>, wait: Duration, interactive: bool) -> Result<WriteTxn> {
        self.check_writable()?;
        self.check_failed()?;
        let guard = self.lock_writes(wait)?;
        self.check_failed()?;
        let base = self.latest.lock().unwrap().clone();
        if interactive {
            let synced = self.fatal(self.wal.sync_to(base.version))?;
            self.publish_up_to(synced);
        }
        let tx = Tx::new(self.writer(false), (*base.catalog).clone(), now_micros());
        Ok(WriteTxn { db: self.clone(), tx: Some(tx), base: base.version, failed: false, savepoints: Vec::new(), guard: Some(guard), pinned: None })
    }

    /// A transaction's view of the pages, told who merges the full-text
    /// segments it leaves due.
    fn writer(&self, fork: bool) -> Writer {
        let mut writer = if fork { Writer::fork(self.store.clone()) } else { Writer::new(self.store.clone()) };
        writer.leave_beside(self.merger.get().is_some());
        writer
    }

    /// Leaves the merging of full-text segments to someone beside this
    /// database's writers: `wake` is called each time a commit leaves
    /// segments due, and whoever it wakes calls `merge_text_segments`
    /// until nothing is. Without this, and always while the log is
    /// replayed, a writer merges what it makes due before it goes on.
    pub fn merge_beside(&self, wake: Box<dyn Fn() + Send + Sync>) {
        let _ = self.merger.set(wake);
    }

    /// Merges the cheapest run of full-text segments that is due in some
    /// index, beside the writers, and puts the result in place. Returns
    /// whether there was anything to merge: the caller then asks again.
    ///
    /// The merge reads the newest committed version and writes a tree
    /// nothing refers to, as a fork writes. Then it takes the writer's
    /// place for a moment: if the segments it read are in the index as
    /// they were (see `fts::install`), the tree replaces them and that
    /// is committed, with nothing in the log; if not, the tree is
    /// dropped. `give_up` is asked now and then whether to stop.
    pub fn merge_text_segments(self: &Arc<Self>, give_up: &dyn Fn() -> bool) -> Result<bool> {
        let _one = self.merging.lock().unwrap();
        self.check_failed()?;
        // A version is held as a reader holds one, and held before the
        // newest is looked up: no page of that version, nor of any made
        // since, is given out again while the merge reads.
        struct Held<'a>(&'a Store, u64);
        impl Drop for Held<'_> {
            fn drop(&mut self) {
                self.0.unpin(self.1);
            }
        }
        let _held = Held(&self.store, self.store.pin());
        let read = self.latest.lock().unwrap().clone();
        let stop = || give_up() || self.has_failed();
        for table in read.catalog.tables.values() {
            for (i, index) in table.indexes.iter().enumerate() {
                if !matches!(index.def.kind, database_protocol::schema::IndexKind::FullText { .. }) {
                    continue;
                }
                let mut writer = Writer::fork(self.store.clone());
                let Some(merged) = self.fatal(crate::fts::merge_beside(&mut writer, table.index_roots[i], &stop))? else { continue };
                // Its pages go to the file now, where they will lie: the
                // commit below then has nothing to carry.
                self.fatal(writer.spill_all())?;
                // The write lock, waited for a little at a time: whoever
                // holds it may hold it long (an exclusive transaction),
                // and meanwhile the engine may be stopping or the
                // database being dropped, which wait for this merge.
                let guard = loop {
                    if stop() {
                        return Ok(false);
                    }
                    match self.lock_writes(Duration::from_millis(20)) {
                        Ok(guard) => break guard,
                        Err(Error::Db(e)) if e.code == ErrorCode::LockTimeout => continue,
                        Err(e) => return Err(e),
                    }
                };
                self.check_failed()?;
                let base = self.latest.lock().unwrap().clone();
                writer.attach();
                let mut tx = Tx::new(writer, (*base.catalog).clone(), now_micros());
                // The index as it is now: of the same table, by its
                // number, and defined as it was.
                let now = base.catalog.table_by_id(table.id).and_then(|now| Some((now, now.indexes.iter().position(|other| other.def == index.def)?)));
                let mut installed = false;
                if let Some((now, at)) = now {
                    if let Some(root) = self.fatal(crate::fts::install(&mut tx.writer, now.index_roots[at], &merged))? {
                        tx.set_index_root(&now.name, at, root)?;
                        installed = true;
                    }
                }
                if !installed {
                    // What it read has changed: its tree goes, with `tx`.
                    self.merges_dropped.fetch_add(1, Ordering::Relaxed);
                    return Ok(true);
                }
                let txn = WriteTxn { db: self.clone(), tx: Some(tx), base: base.version, failed: false, savepoints: Vec::new(), guard: Some(guard), pinned: None };
                let _ = txn.commit_start()?;
                self.merges_beside.fetch_add(1, Ordering::Relaxed);
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Starts a transaction that may write but does not take the write
    /// lock: a fork of the newest durable version, which it holds as a
    /// reader does. It cannot commit as it is: see `WriteTxn::attach`.
    pub fn begin_fork(self: &Arc<Self>) -> Result<WriteTxn> {
        self.check_writable()?;
        self.check_failed()?;
        let (base, pinned) = {
            let published = self.published.lock().unwrap();
            (published.clone(), self.store.pin())
        };
        let tx = Tx::new(self.writer(true), (*base.catalog).clone(), now_micros());
        // Its owner will ask what each statement drew, to draw it again
        // if the fork has to be run a second time.
        tx.tape.record(true);
        Ok(WriteTxn {
            db: self.clone(),
            tx: Some(tx),
            base: base.version,
            failed: false,
            savepoints: Vec::new(),
            guard: None,
            pinned: Some(pinned),
        })
    }

    /// Whether a checkpoint would do some good now.
    pub fn checkpoint_due(&self) -> bool {
        // A following database checkpoints where its primary's log
        // passes from one file to the next, and nowhere else.
        if self.has_failed() || self.following.is_some() {
            return false;
        }
        // Log files the archive refused are offered again now and then,
        // whether or not anything was committed since.
        if self.archive_failed.lock().unwrap().is_some_and(|at| at.elapsed() > ARCHIVE_RETRY) {
            return true;
        }
        if self.commits_since_checkpoint.load(Ordering::Relaxed) == 0 {
            return false;
        }
        // A long log, or a quiet moment after some writes; or, with an
        // archive, commits that have waited their time to be copied.
        let waited = self.archiving.is_some() && !self.options.archive_every.is_zero() && self.first_commit.lock().unwrap().elapsed() > self.options.archive_every;
        waited || self.wal.size() > self.options.checkpoint_bytes || self.last_commit.lock().unwrap().elapsed() > Duration::from_secs(5)
    }

    /// Writes everything committed into the data file and drops the log
    /// that held it, or with an archive hands it there. Writers are held
    /// only while the new log file is started.
    ///
    /// With an archive, a checkpoint that was asked for closes the log
    /// file even if it holds nothing, so that the archive is then known
    /// to be whole up to this moment.
    pub fn checkpoint(self: &Arc<Self>) -> Result<()> {
        let _one = self.checkpointing.lock().unwrap();
        self.checkpoint_held(false, true).map(|_| ())
    }

    /// `checkpoint`, as the engine makes one when it is due: with
    /// nothing committed since the last, the log file is left open.
    pub fn checkpoint_when_due(self: &Arc<Self>) -> Result<()> {
        let _one = self.checkpointing.lock().unwrap();
        self.checkpoint_held(false, false).map(|_| ())
    }

    /// `checkpoint`, by whoever holds `checkpointing`. With `base`, a
    /// base is written to the archive whether or not it lacks one; with
    /// `close`, the log file is closed and handed to the archive though
    /// nothing was committed to it. Returns what the archive refused,
    /// if it refused something.
    fn checkpoint_held(self: &Arc<Self>, base: bool, close: bool) -> Result<Option<String>> {
        self.checkpoint_within(base, close, Duration::from_secs(3600))
    }

    /// `checkpoint_held`, waiting no longer than `wait` for the writer
    /// to let go.
    fn checkpoint_within(self: &Arc<Self>, base: bool, close: bool, wait: Duration) -> Result<Option<String>> {
        if self.following.is_some() {
            return Ok(None);
        }
        self.check_failed()?;
        let guard = self.lock_writes(wait)?;
        let latest = self.latest.lock().unwrap().clone();
        if latest.version == self.store.meta().version {
            // Nothing to write. With an archive there may still be log
            // files to hand it: the ones it refused before, and this
            // one, empty, for the time its seal will say.
            if self.archiving.is_none() {
                return Ok(None);
            }
            if close {
                self.fatal(self.wal.rotate())?;
                self.moment.store(self.wal.moment(), Ordering::Relaxed);
            }
            drop(guard);
            return self.fatal(self.settle_logs(self.wal.generation(), base));
        }
        let generation = self.fatal(self.wal.rotate())?;
        let moment = self.wal.moment();
        self.publish_up_to(latest.version);
        let capture = self.fatal(self.store.capture(latest.version, &latest.catalog.encode(), generation))?;
        self.commits_since_checkpoint.store(0, Ordering::Relaxed);
        drop(guard);
        self.fatal(self.store.write_checkpoint(capture))?;
        self.moment.store(moment, Ordering::Relaxed);
        self.fatal(self.settle_logs(generation, base))
    }

    /// Lets go of the log files before `generation`, which a checkpoint
    /// has passed: removes them, or with an archive copies each there
    /// and removes it when the copy is whole. The caller holds
    /// `checkpointing`, or is the only one to know of the database yet.
    ///
    /// An error is the data directory's. What the archive refuses is
    /// not one: the files it refused stay, the reason is kept for
    /// `stats` and returned, and the next checkpoint tries again.
    fn settle_logs(&self, generation: u64, base: bool) -> Result<Option<String>> {
        let Some(archiving) = &self.archiving else {
            let passed: Vec<u64> = if self.options.keep_log_bytes == 0 && self.log_pins.lock().unwrap().1.is_empty() {
                // Nothing asks for a file to stay: each goes by its name.
                return wal::remove_logs(&*self.vfs, &self.dir, &self.name, |g| g < generation).map(|()| None);
            } else {
                wal::list_logs(&*self.vfs, &self.dir, &self.name)?.into_iter().filter(|g| *g < generation).collect()
            };
            self.remove_passed(&passed)?;
            return Ok(None);
        };
        let mut progress = archiving.progress.lock().unwrap();
        let refused = self.archive_logs(archiving, &mut progress, generation, base)?;
        let now_failing = refused.is_some();
        if now_failing != progress.error.is_some() || (now_failing && progress.error != refused) {
            match &refused {
                Some(why) => eprintln!("database: archive: {} of database {:?} is not in the archive, and its log files are kept until it is: {why}", archiving.history, self.name),
                None => eprintln!("database: archive: {} of database {:?} is in the archive again", archiving.history, self.name),
            }
        }
        progress.error = refused.clone();
        *self.archive_failed.lock().unwrap() = refused.as_ref().map(|_| Instant::now());
        Ok(refused)
    }

    fn archive_logs(&self, archiving: &Archiving, progress: &mut Progress, generation: u64, base: bool) -> Result<Option<String>> {
        let (archive, history) = (&archiving.archive, archiving.history.as_str());
        let uuid = self.store.meta().uuid;
        let said = |e: Error| e.to_string();
        let mut locals: Vec<u64> = wal::list_logs(&*self.vfs, &self.dir, &self.name)?.into_iter().filter(|g| *g < generation).collect();
        progress.kept = locals.len();
        if progress.base.is_some_and(|base| !archive.has_base(history, base)) {
            // The base the log files follow is not where it was: the
            // archive was emptied, or is not there at all just now.
            // Reading it again tells which.
            progress.chain = Chain::Unknown;
            progress.known = None;
        }
        if progress.chain == Chain::Unknown {
            // What the archive holds decides what it still needs. Until
            // it can be read, nothing is removed.
            match archive.list(history) {
                // No further than this server has itself seen the
                // archive take: a directory may show a file whose sync
                // failed, and a power cut would take that one back.
                Ok(listing) => {
                    progress.base = listing.bases.last().copied();
                    progress.chain = listing.next().map_or(Chain::Broken, |next| Chain::Next(progress.known.take().map_or(next, |known| known.min(next))));
                }
                Err(e) => return Ok(Some(said(e))),
            }
        }
        // The archive continues if the next file it needs is one the
        // data directory still has, or the one being written.
        let oldest = locals.first().copied().unwrap_or(generation);
        let continues = matches!(progress.chain, Chain::Next(next) if next >= oldest && next <= generation);
        let mut refused = None;
        // A base of the data file as its checkpoint stands. Nothing a
        // checkpoint names is written again before the next, and the
        // caller holds the next one off. The log file being written is
        // the one that continues from it: the checkpoint was made as
        // that file began, or it is older and no file since holds a
        // record, the last start having found none to replay.
        let meta = self.store.meta();
        let put_base = |progress: &mut Progress| {
            let len = u64::from(meta.next_page) * crate::page::PAGE_SIZE as u64;
            let put = archive.put_base(history, generation, self.moment.load(Ordering::Relaxed), len, &|buf, at| self.store.read_file(buf, at));
            if put.is_ok() {
                progress.bases_written += 1;
                progress.base = Some(generation);
            }
            put.map_err(said)
        };
        if !continues {
            // Nothing in the archive leads here: it begins again, with
            // a base, and the log files the data directory still has
            // are from before it.
            match put_base(progress) {
                Ok(()) => progress.chain = Chain::Next(generation),
                // Every log file stays until the archive takes a base,
                // and the archive is read again before the next try: it
                // may then hold what it did before it could not be
                // reached, and the files kept meanwhile continue that.
                Err(why) => {
                    progress.chain = Chain::Unknown;
                    return Ok(Some(why));
                }
            }
        }
        // Every file before `generation` was full before the moment the
        // data file's checkpoint was made, or the start found: no commit
        // in one of them is later than that.
        let sealed = self.moment.load(Ordering::Relaxed);
        let mut passed = Vec::new();
        while let Some(&local) = locals.first() {
            match progress.chain {
                // Not continued by any base: no copy of it could be
                // applied to anything.
                Chain::Broken | Chain::Unknown => {}
                // Copied already, or from before the base.
                Chain::Next(next) if local < next => {}
                Chain::Next(next) if local == next => {
                    let copied = self.vfs.open(&wal::wal_path(&self.dir, &self.name, local), false).map_err(Error::Io).and_then(|file| archive.put_log(history, &uuid, local, &*file, sealed));
                    if let Err(e) = copied {
                        refused = Some(said(e));
                        break;
                    }
                    progress.logs_written += 1;
                    progress.chain = Chain::Next(next + 1);
                }
                // A file the data directory lacks lies between: what
                // follows it continues from nothing.
                Chain::Next(_) => progress.chain = Chain::Broken,
            }
            locals.remove(0);
            passed.push(local);
        }
        // Copied or not to be copied, a file may yet stay for a standby.
        self.remove_passed(&passed)?;
        progress.kept = locals.len();
        if continues && base && refused.is_none() {
            // A base that was asked for. It is written after the log
            // files before it: the archive's newest base is then always
            // one that every log file it holds leads to or follows, and
            // a server that starts on it knows from the base alone
            // which file the archive lacks next.
            refused = put_base(progress).err();
        }
        Ok(refused)
    }

    /// Writes a base of the database to the archive, after a checkpoint
    /// that leaves the log before it there as well. Returns the base's
    /// place: the log file that continues from it.
    pub fn base_copy(self: &Arc<Self>) -> Result<u64> {
        let Some(archiving) = &self.archiving else {
            return Err(Error::db(ErrorCode::InvalidRequest, "the server has no archive to write a base to: it was started without DATABASE_ARCHIVE"));
        };
        let _one = self.checkpointing.lock().unwrap();
        // What the archive holds is read again: a base is asked for by
        // someone who may have changed it by hand.
        {
            let mut progress = archiving.progress.lock().unwrap();
            if let Chain::Next(next) = progress.chain {
                progress.known = Some(next);
            }
            progress.chain = Chain::Unknown;
        }
        if let Some(why) = self.checkpoint_held(true, true)? {
            return Err(Error::db(ErrorCode::Io, format!("the archive did not take the base of {}: {why}", archiving.history)));
        }
        Ok(self.wal.generation())
    }

    /// A checkpoint that fails if the archive did not take every log
    /// file before it: for whoever is about to remove the database, and
    /// with it the only other copy of its last commits.
    /// It waits up to `wait` for a transaction that holds the database
    /// to end.
    pub fn checkpoint_archived(self: &Arc<Self>, wait: Duration) -> Result<()> {
        let _one = self.checkpointing.lock().unwrap();
        match (self.checkpoint_within(false, true, wait)?, &self.archiving) {
            (Some(why), Some(archiving)) => Err(Error::db(ErrorCode::Io, format!("the archive has not taken the log of {} ({why}), and removing database {:?} would remove the only copy of its last commits: make the archive writable, or start the server without it", archiving.history, self.name))),
            _ => Ok(()),
        }
    }

    // ------------------------------------------------------- a standby

    /// Removes the log files `passed` names, ascending, which nothing
    /// in this server needs any more, but for what standbys may ask
    /// for: the files a pin holds, and the newest of the rest up to
    /// `keep_log_bytes`.
    fn remove_passed(&self, passed: &[u64]) -> Result<()> {
        let pinned = self.pinned_from();
        let mut room = self.options.keep_log_bytes;
        let mut keeping = room > 0;
        let mut removed = false;
        for generation in passed.iter().rev() {
            let path = wal::wal_path(&self.dir, &self.name, *generation);
            if keeping {
                // The newest stay while they fit, and nothing older
                // than one that did not: a standby needs them in a row.
                let len = self.vfs.open(&path, false)?.len()?;
                keeping = len <= room;
                if keeping {
                    room -= len;
                    continue;
                }
            }
            if *generation >= pinned {
                continue;
            }
            self.vfs.remove(&path)?;
            removed = true;
        }
        if removed {
            self.vfs.sync_dir(&self.dir)?;
        }
        Ok(())
    }

    /// The oldest log file a reader of the log still needs: none is
    /// removed from that one on.
    fn pinned_from(&self) -> u64 {
        self.log_pins.lock().unwrap().1.values().copied().min().unwrap_or(u64::MAX)
    }

    /// Holds the log files from `generation` on in the data directory,
    /// whatever checkpoints pass them, until the pin is dropped or moved
    /// on: for whoever sends them to a standby.
    pub fn pin_log(self: &Arc<Self>, generation: u64) -> LogPin {
        let mut pins = self.log_pins.lock().unwrap();
        pins.0 += 1;
        let id = pins.0;
        pins.1.insert(id, generation);
        LogPin { db: self.clone(), id }
    }

    /// The data file's uuid: what the log files, the archive and a
    /// standby know this history by.
    pub fn uuid(&self) -> [u8; 16] {
        self.store.meta().uuid
    }

    /// Where the log is on disk up to. Every commit whose record ends
    /// at or before it was synced, and none after it was acknowledged.
    pub fn synced_position(&self) -> (u64, u64) {
        self.wal.synced_position()
    }

    /// Waits until the log is on disk beyond `position` or `patience`
    /// has passed, and returns where it is on disk up to.
    pub fn wait_for_log(&self, position: (u64, u64), patience: Duration) -> (u64, u64) {
        self.wal.wait_beyond(position, patience)
    }

    /// Reads the log for a standby that has it up to byte `offset` of
    /// file `generation`: at most `most` bytes of what is on disk beyond
    /// that. Only what is synced is given out, so that a standby never
    /// holds a commit its primary could lose in a crash and then make
    /// differently.
    ///
    /// `Behind` if the file is no longer kept; an error, too, for a
    /// place the log has not reached, which is a place in some other
    /// history.
    pub fn read_log(&self, generation: u64, offset: u64, most: usize) -> Result<LogRead> {
        self.check_failed()?;
        let synced = self.wal.synced_position();
        if (generation, offset) > synced || offset < wal::FIRST_RECORD {
            return Err(Error::db(ErrorCode::InvalidRequest, format!("the log of database {:?} is at {} and does not hold {}: the standby has another history than this server's", self.name, archive::position_text(synced.0, synced.1), archive::position_text(generation, offset))));
        }
        let file = match self.vfs.open(&wal::wal_path(&self.dir, &self.name, generation), false) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Error::db(ErrorCode::Behind, format!("log file {generation} of database {:?} is no longer kept", self.name))),
            Err(e) => return Err(e.into()),
        };
        // Up to where it is synced, in the file being written; in an
        // older one, to where its records end.
        let end = if generation == synced.0 { synced.1 } else { wal::replay_file(&*file, &self.uuid(), generation, &mut |_| Ok(()))?.at.max(wal::FIRST_RECORD) };
        if offset > end {
            return Err(Error::db(ErrorCode::InvalidRequest, format!("log file {generation} of database {:?} ends at byte {end}, before byte {offset}: the standby has another history than this server's", self.name)));
        }
        if offset == end {
            return Ok(if generation == synced.0 { LogRead::Wait } else { LogRead::Next });
        }
        let mut bytes = vec![0u8; ((end - offset) as usize).min(most)];
        crate::vfs::read_exact_at(&*file, &mut bytes, offset)?;
        Ok(LogRead::Bytes(bytes))
    }

    /// Gives a base of the database to `sink`, as `archive.rs` writes
    /// one: the data file as of a checkpoint, then the trailer that
    /// says which log file continues from it. Returns that file's
    /// number, and a pin that keeps the log from it on until whoever
    /// took the base has come back for the log.
    pub fn base_to(self: &Arc<Self>, sink: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<(u64, LogPin)> {
        self.check_writable()?;
        let _one = self.checkpointing.lock().unwrap();
        self.checkpoint_held(false, false)?;
        let generation = self.wal.generation();
        let pin = self.pin_log(generation);
        let meta = self.store.meta();
        let len = u64::from(meta.next_page) * crate::page::PAGE_SIZE as u64;
        archive::write_base(generation, self.moment.load(Ordering::Relaxed), len, &|buf, at| self.store.read_file(buf, at), sink)?;
        Ok((generation, pin))
    }

    /// How a following database stands; `None` for one that does not
    /// follow.
    pub fn followed(&self) -> Option<Followed> {
        let mirror = self.following.as_ref()?.lock().unwrap();
        Some(Followed { received: self.wal.position(), applied: mirror.applied, source: mirror.source.map(|(position, time, heard)| (position, time, heard.elapsed())) })
    }

    /// Notes where the primary's own log was, and what its clock said,
    /// when it last told this standby.
    pub fn heard_from_source(&self, position: (u64, u64), time: i64) {
        if let Some(mirror) = &self.following {
            mirror.lock().unwrap().source = Some((position, time, Instant::now()));
        }
    }

    /// Takes bytes of the primary's log: `bytes` lie at byte `offset` of
    /// its log file `generation`. They are written to this database's
    /// file of that number at that place and synced, and then the
    /// records that are now whole are applied, all of them as one
    /// version, which readers see when it is whole.
    ///
    /// Bytes this database already has are passed over, so that a
    /// primary may send again from wherever the standby says it is.
    /// Bytes beyond the end of what it has are refused: there would be
    /// a hole before them. The first bytes of the next file end the
    /// current one: what it held is checkpointed, and the files before
    /// the new one removed.
    ///
    /// Killed at any step, the database starts as a shorter prefix of
    /// the primary's log, and is sent the rest again.
    pub fn receive(self: &Arc<Self>, generation: u64, offset: u64, bytes: &[u8]) -> Result<()> {
        let Some(mirror) = &self.following else {
            return Err(Error::db(ErrorCode::InvalidRequest, format!("database {:?} follows no other", self.name)));
        };
        self.check_failed()?;
        let _one = self.checkpointing.lock().unwrap();
        let mut mirror = mirror.lock().unwrap();
        let (current, end) = self.wal.position();
        let out_of_place = || Error::db(ErrorCode::InvalidRequest, format!("database {:?} has its primary's log up to {} and was sent {}", self.name, archive::position_text(current, end), archive::position_text(generation, offset)));
        let mut bytes = bytes;
        if generation == current + 1 && offset == wal::FIRST_RECORD {
            if !mirror.pending.is_empty() {
                return Err(Error::corrupt(format!("log file {current} of database {:?} ends inside a record, and the primary has gone on to the next", self.name)));
            }
            self.fatal(self.turn_log(&mut mirror))?;
        } else if generation == current && offset <= end {
            bytes = &bytes[((end - offset) as usize).min(bytes.len())..];
        } else {
            return Err(out_of_place());
        }
        if bytes.is_empty() {
            return Ok(());
        }
        let generation = self.wal.generation();
        self.fatal(self.wal.append_copy(bytes))?;
        mirror.pending.extend_from_slice(bytes);
        // The records that are whole now.
        let mut at = mirror.applied;
        let mut taken = 0usize;
        let mut changes: Vec<std::ops::Range<usize>> = Vec::new();
        while let Some((record, len)) = self.fatal(wal::parse(generation, at, &mirror.pending[taken..]))? {
            match record {
                wal::Record::Link(after) => {
                    if mirror.previous > 0 && mirror.previous != after {
                        return self.fatal(Err(Error::corrupt(format!("log file {} of database {:?} ended at byte {} here, and the primary's next file says it ended at byte {after}", generation - 1, self.name, mirror.previous))));
                    }
                }
                wal::Record::Commit { changes: body, .. } => {
                    let start = taken + len - body.len();
                    changes.push(start..start + body.len());
                }
            }
            taken += len;
            at += len as u64;
        }
        if !changes.is_empty() {
            let records: Vec<&[u8]> = changes.iter().map(|range| &mirror.pending[range.clone()]).collect();
            self.fatal(self.apply(&records))?;
        }
        mirror.applied = at;
        mirror.pending.drain(..taken);
        Ok(())
    }

    /// Applies records of the primary's log as one commit: the changes
    /// of each, in order, by the code that replays a log, and then one
    /// version that readers see whole or not at all.
    fn apply(self: &Arc<Self>, records: &[&[u8]]) -> Result<()> {
        let guard = self.lock_writes(Duration::from_secs(3600))?;
        let base = self.latest.lock().unwrap().clone();
        let mut tx = Tx::new(self.writer(false), (*base.catalog).clone(), now_micros());
        tx.logging = false;
        for record in records {
            // A record applied once on the primary. If it does not
            // apply here, this is not the data file it was logged
            // against.
            tx.replay(record).map_err(|e| match e {
                Error::Io(_) | Error::Corrupt(_) => e,
                other => Error::corrupt(format!("the primary's log does not apply to database {:?}: {other}", self.name)),
            })?;
        }
        let version = base.version + 1;
        let Tx { writer, catalog, .. } = tx;
        let left_due = writer.left_due();
        // Its record is on this server's disk already: the version is
        // durable as it is made.
        self.wal.skip(version);
        writer.commit(version)?;
        let snapshot = Arc::new(Snapshot { version, catalog: Arc::new(catalog) });
        self.pending.lock().unwrap().push_back(snapshot.clone());
        *self.latest.lock().unwrap() = snapshot;
        self.commits.fetch_add(records.len() as u64, Ordering::Relaxed);
        self.publish_up_to(version);
        drop(guard);
        if left_due {
            if let Some(wake) = self.merger.get() {
                wake();
            }
        }
        Ok(())
    }

    /// Passes from the copy of one log file to the next, as the primary
    /// has: everything the file held is applied, so the data file is
    /// checkpointed as of here, with the next file as the one that
    /// continues from it, and the files before that one are removed.
    fn turn_log(self: &Arc<Self>, mirror: &mut Mirror) -> Result<()> {
        let guard = self.lock_writes(Duration::from_secs(3600))?;
        let latest = self.latest.lock().unwrap().clone();
        let generation = self.wal.begin_copy()?;
        if latest.version != self.store.meta().version {
            let capture = self.store.capture(latest.version, &latest.catalog.encode(), generation)?;
            drop(guard);
            self.store.write_checkpoint(capture)?;
        }
        wal::remove_logs(&*self.vfs, &self.dir, &self.name, |g| g < generation)?;
        mirror.previous = if mirror.applied > wal::FIRST_RECORD { mirror.applied } else { 0 };
        mirror.applied = wal::FIRST_RECORD;
        Ok(())
    }

    /// The database's history in the archive, and how its archiving
    /// stands.
    pub fn archive_progress(&self) -> Option<(String, Progress)> {
        self.archiving.as_ref().map(|archiving| (archiving.history.clone(), archiving.progress.lock().unwrap().clone()))
    }

    /// Where the newest commit's record ends in the log: the file and
    /// the byte. A restore is asked for a moment by one of these.
    pub fn log_position(&self) -> (u64, u64) {
        self.wal.position()
    }

    /// Deletes the database's files. The caller makes sure nobody uses it.
    pub fn destroy(vfs: &dyn Vfs, dir: &Path, name: &str) -> Result<()> {
        // The data file goes first, and with it the database. Stopped
        // between the two, a server that had removed the log first would
        // start with the database back, as of its last checkpoint and
        // without the commits made since.
        let path = data_path(dir, name);
        if vfs.exists(&path) {
            vfs.remove(&path)?;
            vfs.sync_dir(dir)?;
        }
        wal::remove_logs(vfs, dir, name, |_| true)
    }

    /// Reads every page and checks that the trees, the indexes and the
    /// free list agree. Holds writers off while it runs.
    pub fn check(self: &Arc<Self>) -> Result<String> {
        let _one = self.checkpointing.lock().unwrap();
        let _guard = self.lock_writes(Duration::from_secs(3600))?;
        // After a disk error the files are as the last sync left them,
        // but what is held in memory may be a commit or a checkpoint
        // half made: there is nothing here to check until a restart has
        // read the files again.
        self.check_failed()?;
        let latest = self.latest.lock().unwrap().clone();
        let pages: &dyn Pages = &*self.store;
        let mut used: HashSet<PageId> = HashSet::new();
        let mut claim = |id: PageId, what: &str| -> Result<()> {
            if !used.insert(id) {
                return Err(Error::corrupt(format!("page {id} is used twice (second by {what})")));
            }
            Ok(())
        };
        let mut report = String::new();
        let mut rows_total = 0u64;
        for table in latest.catalog.tables.values() {
            let cost = Cost::default();
            let ctx = Ctx { pages, catalog: &latest.catalog, now: 0, deadline: None, tape: None, params: &[], cost: &cost };
            // The rows, and the large values they name.
            let mut rows = 0u64;
            let mut blob_bytes: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
            let mut keys: Vec<Vec<u8>> = Vec::new();
            btree::walk(pages, table.root, &mut |id| claim(id, &table.name), &mut |key, value| {
                rows += 1;
                keys.push(key.to_vec());
                let row = Row::parse(value.to_vec())?;
                for i in 0..row.stored() {
                    let Some(b) = row.blob(i) else { continue };
                    if blob_bytes.insert(b.id, b.len).is_some() {
                        return Err(Error::corrupt(format!("large value {} of table {:?} belongs to two rows", b.id, table.name)));
                    }
                    // The start a row keeps of a value is the value's.
                    if let Some((start, _)) = row.kept_start(i) {
                        if start.len() as u64 > b.len || crate::blob::read_range(pages, table.blobs, b.id, b.len, 0, start.len() as u64)? != start {
                            return Err(Error::corrupt(format!("a row of table {:?} keeps a start of large value {} that is not how the value starts", table.name, b.id)));
                        }
                    }
                }
                Ok(())
            })?;
            if rows != table.rows {
                return Err(Error::corrupt(format!("table {:?} holds {rows} rows but counts {}", table.name, table.rows)));
            }
            let mut stored: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
            btree::walk(pages, table.blobs, &mut |id| claim(id, &table.name), &mut |key, value| {
                let id = u64::from_be_bytes(key.get(..8).and_then(|id| id.try_into().ok()).ok_or_else(|| Error::corrupt("a large value's key is malformed"))?);
                *stored.entry(id).or_insert(0) += value.len() as u64;
                Ok(())
            })?;
            if stored != blob_bytes {
                return Err(Error::corrupt(format!("the large values of table {:?} do not match the rows that name them", table.name)));
            }
            // Each index: exactly the entries the rows call for.
            for (i, index) in table.indexes.iter().enumerate() {
                if matches!(index.def.kind, database_protocol::schema::IndexKind::FullText { .. }) {
                    crate::fts::verify(&ctx, table, i, &mut |id| claim(id, &index.def.name))?;
                    continue;
                }
                let mut entries = 0u64;
                btree::walk(pages, table.index_roots[i], &mut |id| claim(id, &index.def.name), &mut |key, value| {
                    entries += 1;
                    let pk = exec::entry_pk(key, value)?;
                    if btree::get(pages, table.root, pk)?.is_none() {
                        return Err(Error::corrupt(format!("index {:?} points at a row that is not there", index.def.name)));
                    }
                    Ok(())
                })?;
                let mut expected = 0u64;
                for pk in &keys {
                    let row = exec::fetch(&ctx, table, pk)?.ok_or_else(|| Error::corrupt("a row vanished during the check"))?;
                    for key in crate::write::entry_keys(&ctx, table, i, &row, pk)? {
                        expected += 1;
                        if btree::get(pages, table.index_roots[i], &key)?.is_none() {
                            return Err(Error::corrupt(format!("index {:?} lacks an entry for a row", index.def.name)));
                        }
                    }
                }
                if entries != expected {
                    return Err(Error::corrupt(format!("index {:?} holds {entries} entries where the rows call for {expected}", index.def.name)));
                }
            }
            rows_total += rows;
            report.push_str(&format!("table {}: {rows} rows, {} indexes\n", table.name, table.indexes.len()));
        }
        // Every page is in a tree, free, or part of the checkpoint.
        let (free, next_page, chain) = self.store.free_pages();
        let reachable = used.len();
        for id in free.iter().chain(&chain) {
            if !used.insert(*id) {
                return Err(Error::corrupt(format!("page {id} is both in use and free")));
            }
        }
        let expected = (next_page - FIRST_DATA_PAGE) as usize;
        if used.len() != expected || used.iter().any(|id| *id < FIRST_DATA_PAGE || *id >= next_page) {
            return Err(Error::corrupt(format!("{} pages are accounted for, of {expected} in the file", used.len())));
        }
        report.push_str(&format!(
            "database {}: version {}, {} tables, {rows_total} rows, {reachable} pages in use, {} free, {} bytes\n",
            self.name,
            latest.version,
            latest.catalog.tables.len(),
            free.len(),
            u64::from(next_page) * crate::page::PAGE_SIZE as u64
        ));
        Ok(report)
    }

    pub fn stats(&self) -> String {
        let latest = self.latest.lock().unwrap().clone();
        let archive = match self.archive_progress() {
            None => String::new(),
            Some((history, progress)) => {
                let (generation, offset) = self.wal.position();
                let stands = match (&progress.error, progress.chain) {
                    (Some(why), _) => format!("refused, {} log files kept for it: {why}", progress.kept),
                    (None, Chain::Next(next)) => format!("whole up to log file {next}"),
                    (None, _) => "without a base yet".to_string(),
                };
                format!("archive of {}: log position {}, history {history}, {} log files and {} bases written, {stands}\n", self.name, archive::position_text(generation, offset), progress.logs_written, progress.bases_written)
            }
        };
        let standby = match self.followed() {
            None => {
                let pins = self.log_pins.lock().unwrap();
                match pins.1.values().min() {
                    Some(from) => format!("log of {}: {} standbys are being sent it, and it is kept from file {from} on for them\n", self.name, pins.1.len()),
                    None => String::new(),
                }
            }
            Some(followed) => {
                let at = archive::position_text(followed.received.0, followed.received.1);
                let source = match followed.source {
                    None => "its primary has not been heard from since this server started".to_string(),
                    Some((end, _, heard)) => {
                        let behind = match (end.0 == followed.received.0, end.1.saturating_sub(followed.received.1)) {
                            (true, 0) => "it has all of it".to_string(),
                            (true, bytes) => format!("it is {bytes} bytes behind"),
                            (false, _) => format!("it is {} log files behind", end.0.saturating_sub(followed.received.0)),
                        };
                        format!("its primary's log was at {} when last heard from, {} ms ago: {behind}", archive::position_text(end.0, end.1), heard.as_millis())
                    }
                };
                format!("standby copy {}: a copy of {}, which takes no writes; it has its primary's log up to {at}; {source}\n", self.name, archive::history_name(&self.name, &self.uuid()))
            }
        };
        format!(
            "database {}: version {}, {} tables, {} pages, {} free, {} dirty, log {} bytes, {} commits, {} disk reads, {} disk writes, {} merges of full-text segments beside the writers and {} dropped\n{archive}{standby}",
            self.name,
            latest.version,
            latest.catalog.tables.len(),
            self.store.file_pages(),
            self.store.free_count(),
            self.store.dirty_pages(),
            self.wal.size(),
            self.commits.load(Ordering::Relaxed),
            self.store.disk_reads.load(Ordering::Relaxed),
            self.store.disk_writes.load(Ordering::Relaxed),
            self.merges_beside.load(Ordering::Relaxed),
            self.merges_dropped.load(Ordering::Relaxed),
        )
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn vfs(&self) -> &Arc<dyn Vfs> {
        &self.vfs
    }
}

/// A transaction that only reads: one version of the database, held for
/// as long as this lives.
pub struct ReadTxn {
    db: Arc<Database>,
    snapshot: Arc<Snapshot>,
    pinned: u64,
    now: i64,
    /// What its queries have read since it was last asked.
    cost: Cost,
}

/// The pages one query has read, kept at hand for as long as it runs. A
/// query that looks up many rows reads the same few upper pages of a tree
/// for each: here they are found without going back to the shared cache
/// and its locks.
struct Recent<'a> {
    store: &'a Store,
    pages: std::cell::RefCell<crate::page::PageMap<crate::page::Page>>,
}

/// As many pages as a query keeps for itself: half a megabyte.
const RECENT_PAGES: usize = 64;

impl Pages for Recent<'_> {
    fn page(&self, id: PageId) -> Result<crate::page::Page> {
        if let Some(page) = self.pages.borrow().get(&id) {
            return Ok(page.clone());
        }
        let page = self.store.read(id)?;
        let mut pages = self.pages.borrow_mut();
        if pages.len() >= RECENT_PAGES {
            // The leaves go: each is read once or a few times. The pages
            // above them are read again for every row looked up, and
            // stay unless they alone fill the room.
            pages.retain(|_, page| crate::page::kind(page) == crate::page::KIND_BRANCH);
            if pages.len() >= RECENT_PAGES / 2 {
                pages.clear();
            }
        }
        pages.insert(id, page.clone());
        Ok(page)
    }

    fn view(&self, id: PageId, read: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        if let Some(page) = self.pages.borrow().get(&id) {
            return read(page);
        }
        read(&self.page(id)?)
    }
}

impl ReadTxn {
    pub fn ctx(&self, deadline: Option<Instant>) -> Ctx<'_> {
        Ctx { pages: &*self.db.store, catalog: &self.snapshot.catalog, now: self.now, deadline, tape: None, params: &[], cost: &self.cost }
    }

    /// What its queries have read since this was last asked: see `Read`.
    pub fn take_read(&self) -> Read {
        self.cost.take()
    }

    pub fn query(&self, q: &Query, deadline: Option<Instant>) -> Result<RowSet> {
        self.query_with(q, &[], deadline)
    }

    /// Runs a query with `params` for its `Expr::Param`s.
    pub fn query_with(&self, q: &Query, params: &[Value], deadline: Option<Instant>) -> Result<RowSet> {
        // A version's pages never change, so what the query has read
        // once it may read again from its own copy.
        let recent = Recent { store: &self.db.store, pages: Default::default() };
        let ctx = Ctx { pages: &recent, catalog: &self.snapshot.catalog, now: self.now, deadline, tape: None, params, cost: &self.cost };
        exec::query(&ctx, q, &Scope::default())
    }

    /// Tries a query for a moment: `None` if it has not finished by
    /// `until`.
    pub fn query_within(&self, q: &Query, params: &[Value], until: Instant) -> Result<Option<RowSet>> {
        match self.query_with(q, params, Some(until)) {
            Ok(rows) => Ok(Some(rows)),
            Err(Error::Db(e)) if e.code == ErrorCode::Timeout => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn describe(&self) -> Vec<TableDef> {
        self.snapshot.catalog.defs_in_order()
    }

    pub fn catalog(&self) -> &Arc<Catalog> {
        &self.snapshot.catalog
    }

    pub fn now(&self) -> i64 {
        self.now
    }

    pub fn version(&self) -> u64 {
        self.snapshot.version
    }
}

impl Drop for ReadTxn {
    fn drop(&mut self) {
        self.db.store.unpin(self.pinned);
    }
}

struct Savepoint {
    name: String,
    mark: Mark,
    catalog: Catalog,
    log_len: usize,
}

/// A transaction that may write. It holds the database's write lock,
/// unless it is a fork that has not been attached.
pub struct WriteTxn {
    db: Arc<Database>,
    tx: Option<Tx>,
    base: u64,
    /// A statement failed: only a rollback, whole or to a savepoint, is
    /// possible now.
    failed: bool,
    savepoints: Vec<Savepoint>,
    guard: Option<WriteGuard>,
    /// For a fork: the version it holds as a reader would.
    pinned: Option<u64>,
}

fn aborted() -> Error {
    Error::db(ErrorCode::TransactionAborted, "an earlier statement failed: the transaction can only be rolled back")
}

impl WriteTxn {
    fn tx(&mut self) -> Result<&mut Tx> {
        if self.failed {
            return Err(aborted());
        }
        Ok(self.tx.as_mut().expect("present until the transaction ends"))
    }

    /// Runs a statement, marking the transaction failed if it fails.
    fn run<T>(&mut self, deadline: Option<Instant>, f: impl FnOnce(&mut Tx) -> Result<T>) -> Result<T> {
        let tx = self.tx()?;
        tx.deadline = deadline;
        let result = f(tx);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    pub fn query(&mut self, q: &Query, deadline: Option<Instant>) -> Result<RowSet> {
        // A failed read changes nothing, but the caller may have acted on
        // an assumption it broke: fail the transaction as a write would.
        self.run(deadline, |tx| exec::query(&tx.ctx(), q, &Scope::default()))
    }

    /// Tries a query for a moment: `None` if it has not finished by
    /// `until`. Nothing has changed then and the transaction has not
    /// failed, so the caller may run the query again at leisure.
    pub fn query_within(&mut self, q: &Query, until: Instant) -> Result<Option<RowSet>> {
        let tx = self.tx()?;
        tx.deadline = Some(until);
        match exec::query(&tx.ctx(), q, &Scope::default()) {
            Ok(rows) => Ok(Some(rows)),
            Err(Error::Db(e)) if e.code == ErrorCode::Timeout => Ok(None),
            Err(e) => {
                self.failed = true;
                Err(e)
            }
        }
    }

    pub fn insert(&mut self, ins: &Insert, deadline: Option<Instant>) -> Result<RowSet> {
        self.run(deadline, |tx| tx.insert(ins))
    }

    /// `insert`, for an insert whose texts were cut into words before
    /// the transaction began: see `fts::cut_ahead`.
    pub fn insert_ahead(&mut self, ins: &Insert, ahead: Option<&crate::fts::Ahead>, deadline: Option<Instant>) -> Result<RowSet> {
        self.run(deadline, |tx| tx.insert_ahead(ins, ahead))
    }

    pub fn update(&mut self, upd: &Update, deadline: Option<Instant>) -> Result<RowSet> {
        self.run(deadline, |tx| tx.update(upd))
    }

    pub fn delete(&mut self, del: &Delete, deadline: Option<Instant>) -> Result<RowSet> {
        self.run(deadline, |tx| tx.delete(del))
    }

    pub fn ddl(&mut self, ddl: &Ddl, deadline: Option<Instant>) -> Result<()> {
        self.run(deadline, |tx| match ddl {
            Ddl::CreateTable { def, if_not_exists } => tx.create_table(def, *if_not_exists),
            Ddl::DropTable { name, if_exists } => tx.drop_table(name, *if_exists),
            Ddl::AddColumn { table, column, if_not_exists } => tx.add_column(table, column, *if_not_exists),
            Ddl::DropColumn { table, column, if_exists } => tx.drop_column(table, column, *if_exists),
            Ddl::CreateIndex { table, index, if_not_exists } => tx.create_index(table, index, *if_not_exists),
            Ddl::DropIndex { table, index, if_exists } => tx.drop_index(table, index, *if_exists),
        })
    }

    pub fn describe(&mut self) -> Result<Vec<TableDef>> {
        Ok(self.tx()?.catalog.defs_in_order())
    }

    pub fn now(&self) -> i64 {
        self.tx.as_ref().map_or(0, |tx| tx.now)
    }

    /// Sets the clock its statements read: to run again, at the same
    /// moment, what a fork ran.
    pub fn set_now(&mut self, now: i64) {
        if let Some(tx) = self.tx.as_mut() {
            tx.now = now;
        }
    }

    /// The random ids and counters' numbers its statements have drawn
    /// since this was last asked.
    pub fn take_drawn(&mut self) -> Vec<Drawn> {
        self.tx
            .as_ref()
            .map_or_else(Vec::new, |tx| tx.tape.take_drawn())
    }

    /// Has the next statement draw these, in order, where it draws.
    pub fn give(&mut self, values: &[Drawn]) {
        if let Some(tx) = self.tx.as_ref() {
            tx.tape.give(values);
        }
    }

    /// Gives the values the statements run from now on read for their
    /// `Expr::Param`s, and hands back those given before: a prepared
    /// statement's values are lent for its run and taken back after.
    pub fn bind(&mut self, params: Vec<Value>) -> Vec<Value> {
        match self.tx.as_mut() {
            Some(tx) => std::mem::replace(&mut tx.params, params),
            None => Vec::new(),
        }
    }

    /// What its statements have read since this was last asked: see
    /// `Read`.
    pub fn take_read(&self) -> Read {
        self.tx.as_ref().map_or_else(Read::default, |tx| tx.cost.take())
    }

    /// Whether this is a fork that still runs beside the writer.
    pub fn is_fork(&self) -> bool {
        self.guard.is_none()
    }

    /// Whether a statement has failed, so that only a rollback is left.
    pub fn has_failed(&self) -> bool {
        self.failed
    }

    /// Whether it has changed anything so far.
    pub fn has_written(&self) -> bool {
        self.tx
            .as_ref()
            .is_some_and(|tx| !(tx.writer.is_clean() && tx.log.buf.is_empty()))
    }

    /// Pages it holds in memory.
    pub fn private_pages(&self) -> usize {
        self.tx.as_ref().map_or(0, |tx| tx.writer.private_pages())
    }

    /// Makes a fork the database's writer, waiting up to `wait` for the
    /// write lock, if nothing has been committed since the fork began:
    /// from then on it is a transaction like any other, and may commit.
    /// Returns false, leaving the fork as it was, if something has: what
    /// it read may no longer hold, and it can only be rolled back.
    pub fn attach(&mut self, wait: Duration) -> Result<bool> {
        if self.guard.is_some() {
            return Ok(true);
        }
        self.db.check_failed()?;
        let guard = self.db.lock_writes(wait)?;
        if self.db.latest.lock().unwrap().version != self.base {
            return Ok(false);
        }
        self.guard = Some(guard);
        if let Some(tx) = self.tx.as_mut() {
            tx.writer.attach();
            // It will not be run again: what it draws is asked for no more.
            tx.tape.record(false);
        }
        // It builds on the newest version with no other writer about:
        // nothing it can reach will be freed under it.
        if let Some(pinned) = self.pinned.take() {
            self.db.store.unpin(pinned);
        }
        Ok(true)
    }

    /// Direct access for loading a dump: writes that are already known to
    /// be valid, without a log record.
    pub fn raw(&mut self) -> Result<&mut Tx> {
        self.tx()
    }

    pub fn savepoint(&mut self, name: &str) -> Result<()> {
        let tx = self.tx()?;
        let savepoint = Savepoint { name: name.to_string(), mark: tx.writer.savepoint(), catalog: tx.catalog.clone(), log_len: tx.log.buf.len() };
        self.savepoints.push(savepoint);
        Ok(())
    }

    fn find_savepoint(&self, name: &str) -> Result<usize> {
        self.savepoints.iter().rposition(|s| s.name == name).ok_or_else(|| Error::Db(DbError::not_found(format!("no savepoint {name:?}"))))
    }

    /// Undoes everything since the savepoint, which stays. The
    /// transaction can go on, even after a failed statement.
    pub fn rollback_to(&mut self, name: &str) -> Result<()> {
        let at = self.find_savepoint(name)?;
        self.savepoints.truncate(at + 1);
        let savepoint = &self.savepoints[at];
        let tx = self.tx.as_mut().expect("present until the transaction ends");
        tx.writer.rollback_to(savepoint.mark);
        tx.catalog = savepoint.catalog.clone();
        tx.log.buf.truncate(savepoint.log_len);
        self.failed = false;
        Ok(())
    }

    pub fn release(&mut self, name: &str) -> Result<()> {
        if self.failed {
            return Err(aborted());
        }
        let at = self.find_savepoint(name)?;
        self.savepoints.truncate(at);
        Ok(())
    }

    /// Commits. When this returns `Ok`, the changes are on disk and
    /// visible to every reader.
    pub fn commit(self) -> Result<()> {
        let (db, base) = (self.db.clone(), self.base);
        match self.commit_start()? {
            Some(committed) => committed.wait(),
            // It wrote nothing, but what it read and may have answered
            // from must be on disk before its owner is told anything.
            None => {
                let synced = db.fatal(db.wal.sync_to(base))?;
                db.publish_up_to(synced);
                db.check_failed()
            }
        }
    }

    /// The version this transaction builds on: the newest committed when
    /// it began, durable or not.
    pub fn base(&self) -> u64 {
        self.base
    }

    /// Commits as far as the log, and lets the next writer in. The
    /// changes are final, but not yet on disk nor visible: see
    /// `Committed`. `None` if the transaction wrote nothing.
    pub fn commit_start(mut self) -> Result<Option<Committed>> {
        if self.failed {
            return Err(aborted());
        }
        if self.guard.is_none() {
            return Err(Error::db(
                ErrorCode::Internal,
                "a fork was committed without the write lock",
            ));
        }
        // A database dropped or replaced meanwhile takes no more commits.
        self.db.check_failed()?;
        let tx = self.tx.take().expect("present until the transaction ends");
        let db = self.db.clone();
        if tx.writer.is_clean() && tx.log.buf.is_empty() {
            // Nothing was written: nothing to make durable.
            return Ok(None);
        }
        let version = self.base + 1;
        let Tx { writer, catalog, log, .. } = tx;
        let left_due = writer.left_due();
        if log.buf.is_empty() {
            db.wal.skip(version);
        } else {
            db.fatal(db.wal.append(version, &log.buf))?;
        }
        db.fatal(writer.commit(version))?;
        let snapshot = Arc::new(Snapshot { version, catalog: Arc::new(catalog) });
        db.pending.lock().unwrap().push_back(snapshot.clone());
        *db.latest.lock().unwrap() = snapshot;
        *db.last_commit.lock().unwrap() = Instant::now();
        db.commits.fetch_add(1, Ordering::Relaxed);
        if db.commits_since_checkpoint.fetch_add(1, Ordering::Relaxed) == 0 {
            *db.first_commit.lock().unwrap() = Instant::now();
        }
        // A commit that put nothing in the log, with nothing before it
        // still on its way to the disk, has nothing to wait for: readers
        // may see it now.
        if db.wal.synced_version() >= version {
            db.publish_up_to(version);
        }
        // The next writer may start while this commit reaches the disk.
        self.guard.take();
        // And whoever merges beside the writers, if it left segments due.
        if left_due {
            if let Some(wake) = db.merger.get() {
                wake();
            }
        }
        Ok(Some(Committed { db, version }))
    }

    pub fn rollback(self) {}
}

impl Drop for WriteTxn {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            tx.writer.rollback();
        }
        if let Some(pinned) = self.pinned.take() {
            self.db.store.unpin(pinned);
        }
    }
}
