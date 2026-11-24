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

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use database_protocol::error::{DbError, ErrorCode};
use database_protocol::message::{Ddl, RowSet};
use database_protocol::query::{Delete, Insert, Query, Update};
use database_protocol::schema::TableDef;
use database_protocol::time::now_micros;

use crate::btree::{self, Pages};
use crate::cache::PageCache;
use crate::catalog::Catalog;
use crate::error::{Error, Result};
use crate::eval::{Ctx, Drawn, Scope};
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
}

impl Default for Options {
    fn default() -> Self {
        Options { sync: true, limits: Limits::default(), checkpoint_bytes: 8 * 1024 * 1024 }
    }
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
}

fn data_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.db"))
}

fn random_uuid() -> [u8; 16] {
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
        let file = vfs.open(&path, true)?;
        let store = Arc::new(Store::create(file, cache, store_id, uuid, options.limits)?);
        vfs.sync_dir(dir)?;
        let wal = Wal::create(vfs.clone(), dir, name, uuid, 1, 0, options.sync)?;
        let catalog = Catalog { next_table_id: 1, ..Catalog::default() };
        Ok(Self::assemble(vfs, dir, name, store, wal, 0, catalog, options))
    }

    /// Opens a database, replaying whatever its log holds beyond the
    /// last checkpoint.
    pub fn open(vfs: Arc<dyn Vfs>, dir: &Path, name: &str, cache: Arc<PageCache>, store_id: u32, options: Options) -> Result<Arc<Database>> {
        let file = vfs.open(&data_path(dir, name), false)?;
        let (store, meta, catalog_bytes) = Store::open(file, cache, store_id, options.limits)?;
        let store = Arc::new(store);
        let catalog = Catalog::decode(&catalog_bytes)?;
        // Log files from before the checkpoint were only not yet deleted.
        wal::remove_logs(&*vfs, dir, name, |g| g < meta.wal_gen)?;
        let generations = wal::list_logs(&*vfs, dir, name)?;

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
        // the newest can end in a torn record. Records in a file after
        // one that ended torn mean commits were lost in between, and
        // applying the later ones without them would give a database
        // that never existed.
        let mut torn: Option<u64> = None;
        for generation in &generations {
            let end = wal::replay(&*vfs, dir, name, &meta.uuid, *generation, &mut |record| {
                if let Some(damaged) = torn {
                    return Err(Error::corrupt(format!("log file {damaged} of database {name:?} is damaged before its end, and commits were made after it")));
                }
                changed = true;
                tx.replay(record)
            })?;
            if end == wal::End::Torn {
                torn.get_or_insert(*generation);
            }
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
        let next_generation = generations.last().map_or(meta.wal_gen, |g| g + 1).max(meta.wal_gen);
        let wal = Wal::create(vfs.clone(), dir, name, meta.uuid, next_generation, version, options.sync)?;
        let db = Self::assemble(vfs.clone(), dir, name, store, wal, version, catalog, options);
        if changed {
            // Put what was replayed into the data file now, so the log
            // it came from can go.
            let latest = db.latest.lock().unwrap().clone();
            let capture = db.store.capture(version, &latest.catalog.encode(), next_generation)?;
            db.store.write_checkpoint(capture)?;
        }
        wal::remove_logs(&*vfs, dir, name, |g| g < next_generation)?;
        Ok(db)
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble(vfs: Arc<dyn Vfs>, dir: &Path, name: &str, store: Arc<Store>, wal: Wal, version: u64, catalog: Catalog, options: Options) -> Arc<Database> {
        let snapshot = Arc::new(Snapshot { version, catalog: Arc::new(catalog) });
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
        ReadTxn { db: self.clone(), snapshot: published.clone(), pinned, now: now_micros() }
    }

    /// Starts a transaction that may write, waiting up to `wait` for the
    /// write lock. `interactive` says the caller will see results before
    /// it commits: then everything it could read is made durable first.
    pub fn begin_write(self: &Arc<Self>, wait: Duration, interactive: bool) -> Result<WriteTxn> {
        self.check_failed()?;
        let guard = self.lock_writes(wait)?;
        self.check_failed()?;
        let base = self.latest.lock().unwrap().clone();
        if interactive {
            let synced = self.fatal(self.wal.sync_to(base.version))?;
            self.publish_up_to(synced);
        }
        let tx = Tx::new(Writer::new(self.store.clone()), (*base.catalog).clone(), now_micros());
        Ok(WriteTxn { db: self.clone(), tx: Some(tx), base: base.version, failed: false, savepoints: Vec::new(), guard: Some(guard), pinned: None })
    }

    /// Starts a transaction that may write but does not take the write
    /// lock: a fork of the newest durable version, which it holds as a
    /// reader does. It cannot commit as it is: see `WriteTxn::attach`.
    pub fn begin_fork(self: &Arc<Self>) -> Result<WriteTxn> {
        self.check_failed()?;
        let (base, pinned) = {
            let published = self.published.lock().unwrap();
            (published.clone(), self.store.pin())
        };
        let tx = Tx::new(Writer::fork(self.store.clone()), (*base.catalog).clone(), now_micros());
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
        if self.commits_since_checkpoint.load(Ordering::Relaxed) == 0 || self.has_failed() {
            return false;
        }
        // A long log, or a quiet moment after some writes.
        self.wal.size() > self.options.checkpoint_bytes || self.last_commit.lock().unwrap().elapsed() > Duration::from_secs(5)
    }

    /// Writes everything committed into the data file and drops the log
    /// that held it. Writers are held only while the new log file is
    /// started.
    pub fn checkpoint(self: &Arc<Self>) -> Result<()> {
        let _one = self.checkpointing.lock().unwrap();
        self.check_failed()?;
        let guard = self.lock_writes(Duration::from_secs(3600))?;
        let latest = self.latest.lock().unwrap().clone();
        if latest.version == self.store.meta().version {
            return Ok(());
        }
        let generation = self.fatal(self.wal.rotate())?;
        self.publish_up_to(latest.version);
        let capture = self.fatal(self.store.capture(latest.version, &latest.catalog.encode(), generation))?;
        self.commits_since_checkpoint.store(0, Ordering::Relaxed);
        drop(guard);
        self.fatal(self.store.write_checkpoint(capture))?;
        self.fatal(self.wal.remove_before(generation))
    }

    /// Deletes the database's files. The caller makes sure nobody uses it.
    pub fn destroy(vfs: &dyn Vfs, dir: &Path, name: &str) -> Result<()> {
        wal::remove_logs(vfs, dir, name, |_| true)?;
        let path = data_path(dir, name);
        if vfs.exists(&path) {
            vfs.remove(&path)?;
        }
        vfs.sync_dir(dir)?;
        Ok(())
    }

    /// Reads every page and checks that the trees, the indexes and the
    /// free list agree. Holds writers off while it runs.
    pub fn check(self: &Arc<Self>) -> Result<String> {
        let _one = self.checkpointing.lock().unwrap();
        let _guard = self.lock_writes(Duration::from_secs(3600))?;
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
            let ctx = Ctx { pages, catalog: &latest.catalog, now: 0, deadline: None, tape: None };
            // The rows, and the large values they name.
            let mut rows = 0u64;
            let mut blob_bytes: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
            let mut keys: Vec<Vec<u8>> = Vec::new();
            btree::walk(pages, table.root, &mut |id| claim(id, &table.name), &mut |key, value| {
                rows += 1;
                keys.push(key.to_vec());
                for b in Row::parse(value.to_vec())?.blobs() {
                    if blob_bytes.insert(b.id, b.len).is_some() {
                        return Err(Error::corrupt(format!("large value {} of table {:?} belongs to two rows", b.id, table.name)));
                    }
                }
                Ok(())
            })?;
            if rows != table.rows {
                return Err(Error::corrupt(format!("table {:?} holds {rows} rows but counts {}", table.name, table.rows)));
            }
            let mut stored: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
            btree::walk(pages, table.blobs, &mut |id| claim(id, &table.name), &mut |key, value| {
                let id = u64::from_be_bytes(key[..8].try_into().map_err(|_| Error::corrupt("a large value's key is malformed"))?);
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
        format!(
            "database {}: version {}, {} tables, {} pages, {} free, {} dirty, log {} bytes, {} commits, {} disk reads, {} disk writes\n",
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
        Ctx { pages: &*self.db.store, catalog: &self.snapshot.catalog, now: self.now, deadline, tape: None }
    }

    pub fn query(&self, q: &Query, deadline: Option<Instant>) -> Result<RowSet> {
        // A version's pages never change, so what the query has read
        // once it may read again from its own copy.
        let recent = Recent { store: &self.db.store, pages: Default::default() };
        let ctx = Ctx { pages: &recent, catalog: &self.snapshot.catalog, now: self.now, deadline, tape: None };
        exec::query(&ctx, q, &Scope::default())
    }

    /// Tries a query for a moment: `None` if it has not finished by
    /// `until`.
    pub fn query_within(&self, q: &Query, until: Instant) -> Result<Option<RowSet>> {
        match self.query(q, Some(until)) {
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
        db.commits_since_checkpoint.fetch_add(1, Ordering::Relaxed);
        // A commit that put nothing in the log, with nothing before it
        // still on its way to the disk, has nothing to wait for: readers
        // may see it now.
        if db.wal.synced_version() >= version {
            db.publish_up_to(version);
        }
        // The next writer may start while this commit reaches the disk.
        self.guard.take();
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
