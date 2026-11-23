//! The engine: every database in one directory, sharing one page cache
//! and one background thread that checkpoints them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use database_protocol::error::{DbError, ErrorCode};

use crate::cache::PageCache;
use crate::db::{Database, Options};
use crate::error::{Error, Result};
use crate::vfs::{DiskVfs, Vfs};

#[derive(Clone, Debug)]
pub struct EngineOptions {
    /// Memory for cached pages, shared by all databases.
    pub cache_bytes: usize,
    pub db: Options,
}

impl Default for EngineOptions {
    fn default() -> Self {
        EngineOptions { cache_bytes: 32 * 1024 * 1024, db: Options::default() }
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
    /// Held for as long as the engine lives, so that two processes never
    /// open one directory.
    _lock: Option<std::fs::File>,
}

pub struct Engine {
    inner: Arc<Inner>,
    checkpointer: Option<std::thread::JoinHandle<()>>,
}

/// Ends the name a database is built under while a dump is loaded.
pub const RESTORING: &str = "--restoring";

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
    pub fn open_with(vfs: Arc<dyn Vfs>, dir: &Path, options: EngineOptions, lock: Option<std::fs::File>) -> Result<Engine> {
        vfs.create_dir_all(dir)?;
        let inner = Arc::new(Inner {
            vfs: vfs.clone(),
            dir: dir.to_path_buf(),
            cache: Arc::new(PageCache::new(options.cache_bytes)),
            options,
            dbs: Mutex::new(HashMap::new()),
            next_store: AtomicU32::new(1),
            stopping: Mutex::new(false),
            wake: Condvar::new(),
            _lock: lock,
        });
        // What a restore was building when the server stopped is no use.
        let mut swept = false;
        for file in vfs.list(dir)? {
            if file.contains(RESTORING) {
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
                    let db = Database::open(vfs.clone(), dir, name, inner.cache.clone(), id, inner.options.db.clone())?;
                    inner.dbs.lock().unwrap().insert(name.to_string(), db);
                }
            }
        }
        let background = inner.clone();
        let checkpointer = std::thread::Builder::new().name("checkpointer".into()).spawn(move || loop {
            {
                let stopping = background.stopping.lock().unwrap();
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
                    let _ = db.checkpoint();
                }
            }
        })?;
        Ok(Engine { inner, checkpointer: Some(checkpointer) })
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
        let db = Database::create(self.inner.vfs.clone(), &self.inner.dir, name, self.inner.cache.clone(), id, self.inner.options.db.clone())?;
        dbs.insert(name.to_string(), db.clone());
        Ok(db)
    }

    /// Removes a database and its files. Transactions still open on it
    /// fail from here on.
    pub fn drop_database(&self, name: &str) -> Result<()> {
        let db = self.inner.dbs.lock().unwrap().remove(name).ok_or_else(|| Error::Db(DbError::not_found(format!("no database {name:?}"))))?;
        db.retire();
        Database::destroy(&*self.inner.vfs, &self.inner.dir, name)
    }

    /// Puts the database built as `temp` in the place of `name`, which
    /// goes if it exists. A crash at any point leaves `name` as either
    /// the old database or the new one.
    pub(crate) fn replace_database(&self, temp: &str, name: &str) -> Result<()> {
        check_database_name(name)?;
        let (vfs, dir) = (&*self.inner.vfs, &self.inner.dir);
        let built = self.inner.dbs.lock().unwrap().remove(temp).ok_or_else(|| Error::Db(DbError::not_found(format!("no database {temp:?}"))))?;
        built.checkpoint()?;
        built.retire();
        drop(built);
        if let Some(old) = self.inner.dbs.lock().unwrap().remove(name) {
            // Its log emptied into its file, the old database is whole
            // on disk until the rename replaces it.
            old.checkpoint()?;
            old.retire();
        }
        crate::wal::remove_logs(vfs, dir, name, |_| true)?;
        vfs.rename(&dir.join(format!("{temp}.db")), &dir.join(format!("{name}.db")))?;
        crate::wal::remove_logs(vfs, dir, temp, |_| true)?;
        vfs.sync_dir(dir)?;
        let id = self.inner.next_store.fetch_add(1, Ordering::Relaxed);
        let db = Database::open(self.inner.vfs.clone(), dir, name, self.inner.cache.clone(), id, self.inner.options.db.clone())?;
        self.inner.dbs.lock().unwrap().insert(name.to_string(), db);
        Ok(())
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

    fn stop_checkpointer(&mut self) {
        *self.inner.stopping.lock().unwrap() = true;
        self.inner.wake.notify_all();
        if let Some(thread) = self.checkpointer.take() {
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
