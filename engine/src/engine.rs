//! The engine: every database in one directory, sharing one page cache,
//! one background thread that checkpoints them, and one that merges
//! their full-text segments beside their writers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use database_protocol::error::{DbError, ErrorCode};

use crate::archive::{self, Archive, Moment};
use crate::cache::PageCache;
use crate::db::{data_path, random_uuid, Database, Options, CREATING};
use crate::store::Store;
use crate::wal;
use crate::error::{Error, Result};
use crate::vfs::{DiskVfs, Vfs};

#[derive(Clone, Debug)]
pub struct EngineOptions {
    /// Memory for cached pages, shared by all databases.
    pub cache_bytes: usize,
    pub db: Options,
    /// Whether the engine's thread starts with full-text segments left
    /// unmerged, as `Engine::pause_merging(true)` leaves them: for tests
    /// that merge by hand, at moments of their choosing, from the start.
    pub merging_paused: bool,
    /// A directory to keep the log in beyond its checkpoints, on the
    /// engine's own file system: see `archive.rs`.
    pub archive: Option<PathBuf>,
}

impl Default for EngineOptions {
    fn default() -> Self {
        EngineOptions { cache_bytes: 32 * 1024 * 1024, db: Options::default(), merging_paused: false, archive: None }
    }
}

/// How the databases tell the merging thread that one of them has
/// full-text segments due to be merged. It is theirs and the engine's
/// both, and holds neither.
#[derive(Default)]
struct Due {
    /// Whether some database has said so since the thread last looked.
    said: Mutex<bool>,
    wake: Condvar,
    /// Set to have the thread leave segments be: tests then merge by
    /// hand, at moments of their choosing.
    paused: AtomicBool,
}

impl Due {
    fn say(&self) {
        *self.said.lock().unwrap() = true;
        self.wake.notify_one();
    }
}

struct Inner {
    vfs: Arc<dyn Vfs>,
    dir: PathBuf,
    cache: Arc<PageCache>,
    options: EngineOptions,
    dbs: Mutex<HashMap<String, Arc<Database>>>,
    next_store: AtomicU32,
    stopping: Mutex<bool>,
    wake: Condvar,
    due: Arc<Due>,
    /// Held while a database is restored from the archive: one at a
    /// time.
    recovering: Mutex<()>,
    /// Held for as long as the engine lives, so that two processes never
    /// open one directory.
    _lock: Option<std::fs::File>,
}

impl Inner {
    /// Takes a database into the engine's care: the merging of its
    /// full-text segments is left to the engine's thread from here on.
    /// The thread looks at it once now: segments left due when the
    /// engine last stopped, with a merge given up on or not yet begun,
    /// are merged without waiting for a write to make more due.
    /// How a database of this name is opened. One that is being built
    /// under a temporary name keeps no log: it has no history until it
    /// is given its own name.
    fn options_for(&self, name: &str) -> Options {
        let mut options = self.options.db.clone();
        if name.contains(RESTORING) {
            options.archive = None;
        }
        options
    }

    fn adopt(&self, db: Arc<Database>) -> Arc<Database> {
        let due = self.due.clone();
        db.merge_beside(Box::new(move || due.say()));
        self.due.say();
        db
    }
}

pub struct Engine {
    inner: Arc<Inner>,
    checkpointer: Option<std::thread::JoinHandle<()>>,
    merger: Option<std::thread::JoinHandle<()>>,
}

/// Ends the name a database is built under while a dump is loaded.
pub const RESTORING: &str = "--restoring";
/// Ends the name a database is built under while it is restored from
/// the archive. It holds the other, so that what a server stopped in
/// the middle of either leaves is swept the same way.
const RECOVERING: &str = "--restoring-a-moment";

/// A name that is safe as part of a file name.
pub fn check_database_name(name: &str) -> Result<()> {
    // Two dashes in a row are kept for the engine's own temporary names.
    let ok = !name.is_empty() && name.len() <= 64 && !name.contains("--") && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if !ok {
        return Err(Error::Db(DbError::invalid(format!("{name:?} is not a valid database name: use a-z, 0-9, _ and -, at most 64 of them"))));
    }
    Ok(())
}

impl Engine {
    /// Opens the databases in a directory on disk, creating the directory
    /// if need be and recovering each database.
    pub fn open(dir: &Path, options: EngineOptions) -> Result<Engine> {
        std::fs::create_dir_all(dir)?;
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("LOCK"))?;
        if lock.try_lock().is_err() {
            return Err(Error::db(ErrorCode::Unavailable, format!("{} is in use by another server", dir.display())));
        }
        Self::open_with(Arc::new(DiskVfs), dir, options, Some(lock))
    }

    /// Opens on any file system: tests pass one that can simulate a crash.
    pub fn open_with(vfs: Arc<dyn Vfs>, dir: &Path, mut options: EngineOptions, lock: Option<std::fs::File>) -> Result<Engine> {
        vfs.create_dir_all(dir)?;
        if let Some(archive) = &options.archive {
            // An archive that is not there now is no reason not to
            // start: the log is kept in the data directory until it is.
            let _ = vfs.create_dir_all(archive);
            options.db.archive = Some(Arc::new(Archive::new(vfs.clone(), archive)));
        }
        let paused = options.merging_paused;
        let inner = Arc::new(Inner {
            vfs: vfs.clone(),
            dir: dir.to_path_buf(),
            cache: Arc::new(PageCache::new(options.cache_bytes)),
            options,
            dbs: Mutex::new(HashMap::new()),
            next_store: AtomicU32::new(1),
            stopping: Mutex::new(false),
            wake: Condvar::new(),
            due: Arc::new(Due { paused: AtomicBool::new(paused), ..Due::default() }),
            recovering: Mutex::new(()),
            _lock: lock,
        });
        // What a restore was building when the server stopped is no use,
        // nor is a data file that was still being made.
        let mut swept = false;
        for file in vfs.list(dir)? {
            if file.contains(RESTORING) || file.contains(CREATING) {
                vfs.remove(&dir.join(&file))?;
                swept = true;
            }
        }
        if swept {
            vfs.sync_dir(dir)?;
        }
        for file in vfs.list(dir)? {
            if let Some(name) = file.strip_suffix(".db") {
                if check_database_name(name).is_ok() {
                    let id = inner.next_store.fetch_add(1, Ordering::Relaxed);
                    let db = inner.adopt(Database::open(vfs.clone(), dir, name, inner.cache.clone(), id, inner.options_for(name))?);
                    inner.dbs.lock().unwrap().insert(name.to_string(), db);
                }
            }
        }
        let background = inner.clone();
        let checkpointer = std::thread::Builder::new().name("checkpointer".into()).spawn(move || loop {
            {
                // Told to stop before it first waited, it would otherwise
                // sleep through the wake-up it was sent.
                let stopping = background.stopping.lock().unwrap();
                if *stopping {
                    return;
                }
                let (stopping, _) = background.wake.wait_timeout(stopping, Duration::from_millis(250)).unwrap();
                if *stopping {
                    return;
                }
            }
            let dbs: Vec<Arc<Database>> = background.dbs.lock().unwrap().values().cloned().collect();
            for db in dbs {
                if db.checkpoint_due() {
                    // A failure marks the database failed; the next
                    // request reports it.
                    let _ = db.checkpoint_when_due();
                }
            }
        })?;
        // Merges wait for a database to say that segments are due: an
        // engine nobody writes to merges nothing and wakes for nothing.
        let merging = inner.clone();
        let merger = std::thread::Builder::new().name("merger".into()).spawn(move || loop {
            let stopping = || *merging.stopping.lock().unwrap();
            {
                let mut said = merging.due.said.lock().unwrap();
                loop {
                    if stopping() {
                        return;
                    }
                    if *said && !merging.due.paused.load(Ordering::Relaxed) {
                        break;
                    }
                    said = merging.due.wake.wait(said).unwrap();
                }
                *said = false;
            }
            let dbs: Vec<Arc<Database>> = merging.dbs.lock().unwrap().values().cloned().collect();
            for db in dbs {
                // A failure marks the database failed, or it was dropped
                // meanwhile: either way there is nothing more to do here.
                let mut more = true;
                while more && !stopping() && !merging.due.paused.load(Ordering::Relaxed) {
                    more = db.merge_text_segments(&stopping).unwrap_or(false);
                }
            }
        })?;
        Ok(Engine { inner, checkpointer: Some(checkpointer), merger: Some(merger) })
    }

    /// Has the engine's thread leave full-text segments unmerged, or
    /// take them up again. While it is paused, writers still leave their
    /// merges to be made beside them, and whoever paused it makes them,
    /// with `Database::merge_text_segments`: tests do, to merge at
    /// moments of their choosing.
    pub fn pause_merging(&self, paused: bool) {
        self.inner.due.paused.store(paused, Ordering::Relaxed);
        if !paused {
            self.inner.due.say();
        }
    }

    pub fn create_database(&self, name: &str) -> Result<Arc<Database>> {
        check_database_name(name)?;
        self.create_database_unchecked(name)
    }

    /// Creates a database under a name clients cannot use.
    pub(crate) fn create_database_unchecked(&self, name: &str) -> Result<Arc<Database>> {
        let mut dbs = self.inner.dbs.lock().unwrap();
        if dbs.contains_key(name) {
            return Err(Error::db(ErrorCode::AlreadyExists, format!("database {name:?} already exists")));
        }
        let id = self.inner.next_store.fetch_add(1, Ordering::Relaxed);
        let db = self.inner.adopt(Database::create(self.inner.vfs.clone(), &self.inner.dir, name, self.inner.cache.clone(), id, self.inner.options_for(name))?);
        dbs.insert(name.to_string(), db.clone());
        Ok(db)
    }

    /// Removes a database and its files. A transaction still open on it
    /// fails at its next statement or at its commit.
    pub fn drop_database(&self, name: &str) -> Result<()> {
        let db = self.database(name)?;
        // With an archive, the log goes there before the files go: a
        // database dropped by mistake is then in the archive up to its
        // last commit. One that has failed has no checkpoint to make.
        if !db.has_failed() {
            db.checkpoint_archived()?;
        }
        self.inner.dbs.lock().unwrap().remove(name);
        db.retire();
        Database::destroy(&*self.inner.vfs, &self.inner.dir, name)
    }

    /// Puts the database built as `temp` in the place of `name`, which
    /// goes if it exists. A crash at any point leaves `name` as either
    /// the old database or the new one.
    pub(crate) fn replace_database(&self, temp: &str, name: &str) -> Result<()> {
        check_database_name(name)?;
        let built = self.inner.dbs.lock().unwrap().remove(temp).ok_or_else(|| Error::Db(DbError::not_found(format!("no database {temp:?}"))))?;
        built.checkpoint()?;
        built.retire();
        drop(built);
        self.install(temp, name)
    }

    /// Gives the data file built as `temp`, whole and with nothing in
    /// its log, the name `name`, and opens it.
    fn install(&self, temp: &str, name: &str) -> Result<()> {
        let (vfs, dir) = (&*self.inner.vfs, &self.inner.dir);
        let old = self.inner.dbs.lock().unwrap().get(name).cloned();
        if let Some(old) = old {
            // Its log emptied into its file, the old database is whole
            // on disk until the rename replaces it; and with an archive,
            // its history is there to its last commit.
            old.checkpoint_archived()?;
            self.inner.dbs.lock().unwrap().remove(name);
            old.retire();
        }
        crate::wal::remove_logs(vfs, dir, name, |_| true)?;
        vfs.rename(&dir.join(format!("{temp}.db")), &dir.join(format!("{name}.db")))?;
        crate::wal::remove_logs(vfs, dir, temp, |_| true)?;
        vfs.sync_dir(dir)?;
        let id = self.inner.next_store.fetch_add(1, Ordering::Relaxed);
        let db = self.inner.adopt(Database::open(self.inner.vfs.clone(), dir, name, self.inner.cache.clone(), id, self.inner.options_for(name))?);
        self.inner.dbs.lock().unwrap().insert(name.to_string(), db);
        Ok(())
    }

    fn archive(&self) -> Result<&Arc<Archive>> {
        self.inner.options.db.archive.as_ref().ok_or_else(|| Error::db(ErrorCode::InvalidRequest, "the server has no archive: it was started without DATABASE_ARCHIVE"))
    }

    /// Writes a base of each named database to the archive, or of every
    /// database when none is named, and says where each begins.
    pub fn base_copy(&self, names: &[String]) -> Result<String> {
        self.archive()?;
        let names = if names.is_empty() { self.names() } else { names.to_vec() };
        let mut report = String::new();
        for name in names {
            let db = self.database(&name)?;
            let generation = db.base_copy()?;
            let history = db.archive_progress().map(|(history, _)| history).unwrap_or_default();
            report.push_str(&format!("{history}: base {generation} written\n"));
        }
        Ok(report)
    }

    /// The histories in the archive that `name` names: the one of that
    /// very name, or every one of a database of that name; all of them
    /// for an empty name.
    fn histories(&self, name: &str) -> Result<Vec<String>> {
        let all = self.archive()?.histories()?;
        if name.is_empty() {
            return Ok(all);
        }
        Ok(all.into_iter().filter(|history| history == name || archive::parse_history(history).is_some_and(|(of, _)| of == name)).collect())
    }

    /// Reads the archive of every history `name` names (see
    /// `histories`) and reports what each holds; fails at the first that
    /// is not whole. With `commits`, lists every commit.
    pub fn archive_report(&self, name: &str, commits: bool) -> Result<String> {
        let histories = self.histories(name)?;
        if histories.is_empty() && !name.is_empty() {
            return Err(Error::Db(DbError::not_found(format!("the archive holds no history of {name:?}"))));
        }
        let archive = self.archive()?;
        histories.iter().map(|history| archive.report(history, commits)).collect()
    }

    /// Makes database `target` what the history `from` held at the
    /// moment `to`: a base from the archive, and the commits after it up
    /// to that moment, applied as recovery applies them.
    ///
    /// `from` is a history's name in the archive, or a database's name
    /// if the archive holds one history of it, or holds the history of
    /// the database of that name now on the server. `target` must not
    /// exist unless `replace` is given; it is built under another name
    /// and takes `target` only when it is whole, so that a failure or a
    /// crash at any point leaves the server as it was. The restored
    /// database is a new history, under a new uuid.
    ///
    /// Refused, with nothing changed: a moment before the oldest base; a
    /// time after the one up to which the archive is known to be whole,
    /// which is when its last log file was closed; a position the archive does not reach; a base or
    /// a log file on the way that is missing, damaged, or does not end
    /// where the next says it ended.
    pub fn recover_database(&self, from: &str, to: Moment, target: &str, replace: bool) -> Result<String> {
        let archive = self.archive()?.clone();
        check_database_name(target)?;
        let _one = self.inner.recovering.lock().unwrap();
        let mut histories = self.histories(from)?;
        if histories.len() > 1 {
            // Of several histories of one name, the one that name has
            // on the server now.
            let current = self.database(from).ok().and_then(|db| db.archive_progress()).map(|(history, _)| history);
            match current.filter(|current| histories.contains(current)) {
                Some(current) => histories = vec![current],
                None => return Err(Error::Db(DbError::invalid(format!("the archive holds {} histories of {from:?}: name one of {}", histories.len(), histories.join(", "))))),
            }
        }
        let Some(history) = histories.pop() else {
            return Err(Error::Db(DbError::not_found(format!("the archive holds no history of {from:?}"))));
        };
        if !replace && self.inner.dbs.lock().unwrap().contains_key(target) {
            return Err(Error::db(ErrorCode::AlreadyExists, format!("database {target:?} already exists")));
        }
        let temp = format!("{target}{RECOVERING}");
        let (vfs, dir) = (&self.inner.vfs, &self.inner.dir);
        let sweep = || -> Result<()> {
            let path = data_path(dir, &temp);
            if vfs.exists(&path) {
                vfs.remove(&path)?;
            }
            wal::remove_logs(&**vfs, dir, &temp, |_| true)?;
            Ok(vfs.sync_dir(dir)?)
        };
        sweep()?;
        let built = self.build_from_archive(&archive, &history, to, &temp);
        let restored = match built {
            Ok(restored) => restored,
            Err(e) => {
                let _ = sweep();
                return Err(e);
            }
        };
        self.install(&temp, target)?;
        Ok(format!("{target}: restored from {history}: base {}, {restored}\n", restored.base))
    }

    /// Builds, under the name `temp`, the data file of `history` at the
    /// moment `to`, with nothing in its log and a uuid of its own.
    fn build_from_archive(&self, archive: &Archive, history: &str, to: Moment, temp: &str) -> Result<Restored> {
        let (vfs, dir) = (&self.inner.vfs, &self.inner.dir);
        let (_, uuid) = archive::parse_history(history).expect("a history's name");
        let refused = |what: String| Error::Db(DbError::invalid(format!("{history} cannot be restored to that moment: {what}")));
        let listing = archive.list(history)?;
        // The newest base that is not past the moment.
        let mut chosen = None;
        for generation in listing.bases.iter().rev() {
            let base = archive.base(history, *generation)?;
            let before = match to {
                Moment::End => true,
                Moment::Time(time) => base.time <= time,
                Moment::Position { generation, .. } => base.generation <= generation,
            };
            if before {
                chosen = Some(base);
                break;
            }
        }
        let Some(base) = chosen else {
            return Err(refused(match listing.bases.first() {
                Some(oldest) => format!("its oldest base, {oldest}, was taken after it"),
                None => "the archive holds no base of it".to_string(),
            }));
        };
        let file = vfs.open(&data_path(dir, temp), true)?;
        file.set_len(0)?;
        archive.copy_base(history, &base, Some(&*file))?;
        file.sync()?;
        drop(file);

        let mut restored = Restored { base: base.generation, commits: 0, last: None };
        // The time up to which the files read so far are known to be
        // all that was committed: the base's, then each file's seal.
        let mut whole_until = base.time;
        // Where the file before ended, if it held records.
        let mut before: Option<u64> = None;
        let mut generation = base.generation;
        loop {
            if listing.logs.binary_search(&generation).is_err() {
                // The archive ends here, as far as it is whole.
                if let Some(later) = listing.logs.iter().find(|g| **g > generation) {
                    return Err(refused(format!("log file {generation} is missing from the archive, and log file {later} is there")));
                }
                let ends = match restored.last {
                    Some((g, end, time)) => format!("the archive ends with the commit at {}, made {}", archive::position_text(g, end), archive::time_text(time.unwrap_or(0))),
                    None => format!("the archive holds no commit after base {}", base.generation),
                };
                match to {
                    Moment::End => break,
                    // Nothing was committed between the last commit
                    // and the last seal, and the time is no later.
                    Moment::Time(time) if time <= whole_until => break,
                    Moment::Time(_) => return Err(refused(format!("{ends}, and is the whole of what was committed up to {}, which is before the time asked for; a checkpoint of the running database brings its archive up to the moment it is made", archive::time_text(whole_until)))),
                    Moment::Position { .. } => return Err(refused(format!("{ends}, before the position asked for"))),
                }
            }
            let mut cut = wal::FIRST_RECORD;
            let mut passed = false;
            let read = archive.read_log(history, &uuid, generation, &mut |commit| {
                let within = !passed
                    && match to {
                        Moment::End => true,
                        // A commit without a time was made no later than
                        // the next that has one.
                        Moment::Time(time) => commit.time.is_none_or(|made| made <= time),
                        Moment::Position { generation: g, offset } => (generation, commit.end) <= (g, offset),
                    };
                if within {
                    cut = commit.end;
                    restored.commits += 1;
                    restored.last = Some((generation, commit.end, commit.time));
                } else {
                    passed = true;
                }
                Ok(())
            })?;
            whole_until = whole_until.max(read.sealed);
            if !passed && (read.end == wal::End::Torn || read.at == 0 || read.sealed == 0) {
                return Err(refused(format!("log file {generation} in the archive is damaged after byte {}", read.at)));
            }
            if before.is_some_and(|ended| read.after > 0 && ended != read.after) {
                return Err(refused(format!("log file {} in the archive ends at byte {}, and log file {generation} says it ended at byte {}", generation - 1, before.unwrap_or(0), read.after)));
            }
            let log = vfs.open(&wal::wal_path(dir, temp, generation), true)?;
            log.set_len(0)?;
            archive.copy_log(history, generation, cut, &*log)?;
            log.sync()?;
            if passed || matches!(to, Moment::Position { generation: g, .. } if g == generation) {
                break;
            }
            before = (read.at > wal::FIRST_RECORD).then_some(read.at);
            generation += 1;
        }
        vfs.sync_dir(dir)?;
        // Recovery, as after a crash: the base is the checkpoint, the
        // copies are its log.
        let id = self.inner.next_store.fetch_add(1, Ordering::Relaxed);
        let db = Database::open(vfs.clone(), dir, temp, self.inner.cache.clone(), id, self.inner.options_for(temp))?;
        db.checkpoint()?;
        db.retire();
        drop(db);
        // A new history from here: what is committed to it next is not
        // what the old one holds after this moment.
        Store::rebrand(&*vfs.open(&data_path(dir, temp), false)?, random_uuid())?;
        Ok(restored)
    }

    /// Whether the engine keeps its log in an archive.
    pub fn has_archive(&self) -> bool {
        self.inner.options.db.archive.is_some()
    }

    pub fn database(&self, name: &str) -> Result<Arc<Database>> {
        self.inner.dbs.lock().unwrap().get(name).cloned().ok_or_else(|| Error::Db(DbError::not_found(format!("no database {name:?}"))))
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.inner.dbs.lock().unwrap().keys().filter(|n| !n.contains(RESTORING)).cloned().collect();
        names.sort();
        names
    }

    pub fn checkpoint_all(&self) -> Result<()> {
        let dbs: Vec<Arc<Database>> = self.inner.dbs.lock().unwrap().values().cloned().collect();
        for db in dbs {
            db.checkpoint()?;
        }
        Ok(())
    }

    /// Pages the shared cache holds, and how many it may.
    pub fn cache_usage(&self) -> (usize, usize) {
        (self.inner.cache.len(), self.inner.cache.capacity())
    }

    /// Stops the engine's two threads. A merge under way gives up.
    fn stop_checkpointer(&mut self) {
        *self.inner.stopping.lock().unwrap() = true;
        self.inner.wake.notify_all();
        // Told with what it waits on held, so that it is either before
        // its look at `stopping` or already waiting.
        drop(self.inner.due.said.lock().unwrap());
        self.inner.due.wake.notify_all();
        for thread in [self.checkpointer.take(), self.merger.take()].into_iter().flatten() {
            let _ = thread.join();
        }
    }

    /// Checkpoints every database and stops. After this the log files
    /// hold nothing and the next start is immediate.
    pub fn close(mut self) -> Result<()> {
        self.stop_checkpointer();
        self.checkpoint_all()
    }
}

impl Drop for Engine {
    /// Dropped without `close`, the engine stops as a crash would leave
    /// it: nothing is flushed, and the next open replays the log.
    fn drop(&mut self) {
        self.stop_checkpointer();
    }
}

/// What a restore from the archive applied.
struct Restored {
    base: u64,
    commits: u64,
    /// The last commit applied: its log file, where its record ends,
    /// and its time if it has one.
    last: Option<(u64, u64, Option<i64>)>,
}

impl std::fmt::Display for Restored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.last {
            Some((generation, end, time)) => write!(f, "{} commits after it, the last at {}, made {}", self.commits, archive::position_text(generation, end), archive::time_text(time.unwrap_or(0))),
            None => write!(f, "no commit after it"),
        }
    }
}
