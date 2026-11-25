//! The page store: one data file, the pages of it held in memory, and the
//! rules for when a page may be written and when its number may be reused.
//!
//! Pages are never changed in place once committed. A transaction writes
//! new pages (see `Writer`) and frees the ones they replace. A page is
//! then in one of these places:
//!
//! - **private** to a transaction that has not committed;
//! - **dirty**: committed, in memory, not yet in the data file;
//! - **flushing**: being written by a checkpoint;
//! - **clean**: in the data file, and perhaps in the shared cache.
//!
//! The data file is only ever valid as of its last **checkpoint**: the
//! header page names the catalog and the free list of one version. What
//! was committed since is in the log (`wal.rs`) and replayed on open. A
//! checkpoint writes every dirty page, then the catalog and the free list
//! of the version it captured, syncs, and only then switches the header.
//! Writing a page early is always safe, because a new page takes a number
//! that the last checkpoint does not use.
//!
//! A freed page's number can be handed out again once nothing can still
//! read it: no reader pinned at an older version, and no checkpoint, on
//! disk or under way, that still names it.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::btree::{Pages, PagesMut};
use crate::cache::PageCache;
use crate::error::{Error, Result};
use crate::page::*;
use crate::vfs::{read_exact_at, File};

const KIND_META: u8 = 5;
const MAGIC: &[u8; 8] = b"dbstore1";
/// Payload bytes in one page of a chain.
pub const CHAIN_PAYLOAD: usize = PAGE_SIZE - HEADER;

/// The data file's header: what the last checkpoint left.
#[derive(Clone, Debug, PartialEq)]
pub struct Meta {
    /// Counts checkpoints. Of the two header copies the higher one wins.
    pub counter: u64,
    /// The committed version the file holds.
    pub version: u64,
    pub catalog: PageId,
    pub catalog_len: u64,
    pub freelist: PageId,
    pub freelist_len: u64,
    /// One past the highest page the checkpoint knows of.
    pub next_page: PageId,
    /// The first log file that holds commits after `version`.
    pub wal_gen: u64,
    /// Ties the log files to this data file.
    pub uuid: [u8; 16],
}

impl Meta {
    fn encode(&self) -> Vec<u8> {
        let mut page = new_buf(KIND_META);
        page[8..16].copy_from_slice(MAGIC);
        put_u64(&mut page, 16, self.counter);
        put_u64(&mut page, 24, self.version);
        put_u32(&mut page, 32, self.catalog);
        put_u64(&mut page, 36, self.catalog_len);
        put_u32(&mut page, 44, self.freelist);
        put_u64(&mut page, 48, self.freelist_len);
        put_u32(&mut page, 56, self.next_page);
        put_u64(&mut page, 60, self.wal_gen);
        page[68..84].copy_from_slice(&self.uuid);
        put_u32(&mut page, 84, PAGE_SIZE as u32);
        sealed(&page)
    }

    fn decode(page: &[u8]) -> Option<Meta> {
        if !verify(page) || kind(page) != KIND_META || &page[8..16] != MAGIC || get_u32(page, 84) != PAGE_SIZE as u32 {
            return None;
        }
        Some(Meta {
            counter: get_u64(page, 16),
            version: get_u64(page, 24),
            catalog: get_u32(page, 32),
            catalog_len: get_u64(page, 36),
            freelist: get_u32(page, 44),
            freelist_len: get_u64(page, 48),
            next_page: get_u32(page, 56),
            wal_gen: get_u64(page, 60),
            uuid: page[68..84].try_into().unwrap(),
        })
    }
}

#[derive(Default)]
struct Mem {
    dirty: PageMap<Page>,
    flushing: PageMap<Page>,
}

/// Which page numbers may be in `Mem`, for a reader to ask without its
/// lock: a bit for each number, many numbers to a bit. A reader takes
/// the lock only for a number whose bit is set, which few of the pages
/// it reads have: nearly all are in the cache or the file.
struct MaybeInMem {
    bits: Vec<AtomicU64>,
    /// Numbers marked since the bits were last set afresh.
    marked: AtomicUsize,
}

/// 65,536 bits: a few hundred pages wait in memory at a time.
const MAYBE_WORDS: usize = 1024;

impl MaybeInMem {
    fn new() -> MaybeInMem {
        MaybeInMem { bits: (0..MAYBE_WORDS).map(|_| AtomicU64::new(0)).collect(), marked: AtomicUsize::new(0) }
    }

    fn place(id: PageId) -> (usize, u64) {
        let h = u64::from(id).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 48;
        ((h >> 6) as usize % MAYBE_WORDS, 1 << (h & 63))
    }

    fn may_hold(&self, id: PageId) -> bool {
        let (word, bit) = Self::place(id);
        self.bits[word].load(Ordering::Acquire) & bit != 0
    }

    /// Marks a page about to be put in `Mem`. Called with its lock held
    /// for writing.
    fn mark(&self, id: PageId) {
        let (word, bit) = Self::place(id);
        self.bits[word].fetch_or(bit, Ordering::Release);
        self.marked.fetch_add(1, Ordering::Relaxed);
    }

    /// Sets the bits afresh from what `mem` holds now, dropping the
    /// marks of pages that have left it. Called with its lock held for
    /// writing. Each word goes from one value that covers every page in
    /// `mem` to another that does, so a reader that looks meanwhile is
    /// never told that a page there is not.
    fn renew(&self, mem: &Mem) {
        let mut fresh = vec![0u64; MAYBE_WORDS];
        for id in mem.dirty.keys().chain(mem.flushing.keys()) {
            let (word, bit) = Self::place(*id);
            fresh[word] |= bit;
        }
        for (word, bits) in fresh.into_iter().enumerate() {
            self.bits[word].store(bits, Ordering::Release);
        }
        self.marked.store(0, Ordering::Relaxed);
    }
}

struct Alloc {
    free: Vec<PageId>,
    next_page: PageId,
    /// Pages handed out since the last checkpoint was captured. No
    /// checkpoint names them, so once freed they wait only for readers.
    young: PageSet,
    /// Freed pages that no checkpoint names, by the version that freed
    /// them: reusable once no reader is older than that version.
    wait_readers: BTreeMap<u64, Vec<PageId>>,
    /// Freed pages that a checkpoint may name: reusable once, in
    /// addition, a checkpoint of that version or later is on disk.
    wait_checkpoint: BTreeMap<u64, Vec<PageId>>,
    /// The version of the checkpoint on disk.
    durable: u64,
    /// The pages holding that checkpoint's catalog and free list.
    chain_pages: Vec<PageId>,
    /// Pages held by forks: transactions that write beside the one
    /// writer and have not committed (see `Writer::fork`). No version
    /// uses them, so a checkpoint counts them free: after a crash they
    /// are, the fork having died with the process.
    lent: PageSet,
    meta: Meta,
}

struct Pins {
    /// The newest version readers may pin.
    published: u64,
    counts: BTreeMap<u64, usize>,
}

/// How much a store keeps in memory outside the shared cache.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Committed pages not yet written, before the committer writes them.
    pub max_dirty: usize,
    /// Pages of one open transaction, before it writes some out.
    pub max_private: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_dirty: 1024, max_private: 2048 }
    }
}

pub struct Store {
    id: u32,
    file: Arc<dyn File>,
    cache: Arc<PageCache>,
    mem: RwLock<Mem>,
    alloc: Mutex<Alloc>,
    pins: Mutex<Pins>,
    limits: Limits,
    /// Set when a write to the file failed: nothing more is written.
    failed: AtomicBool,
    /// Per table and column, one past the highest number an
    /// `auto_increment` counter has given any transaction. A writer and
    /// the forks beside it each count on their own copy of the counter:
    /// this keeps them from giving out the same number twice.
    serials: Mutex<HashMap<(u32, usize), i64>>,
    /// How many pages `mem` holds. A reader looks there, behind its lock,
    /// only when this says there is something to find: after a
    /// checkpoint, until the next commit, there is not.
    in_mem: AtomicUsize,
    /// And, when there is, which pages it may be.
    maybe: MaybeInMem,
    /// The version the oldest reader must have reached before any freed
    /// page that waits can be given back: `u64::MAX` when none can.
    release_at: AtomicU64,
    pub disk_reads: AtomicU64,
    pub disk_writes: AtomicU64,
}

/// What a checkpoint needs, taken at one commit boundary.
pub struct Capture {
    version: u64,
    chain: Vec<(PageId, Vec<u8>)>,
    meta: Meta,
    old_chain: Vec<PageId>,
}

fn chain_pages_needed(len: usize) -> usize {
    len.div_ceil(CHAIN_PAYLOAD)
}

/// Lays `data` out over the given pages, each naming the next.
fn build_chain(data: &[u8], ids: &[PageId]) -> Vec<(PageId, Vec<u8>)> {
    let mut out = Vec::with_capacity(ids.len());
    for (i, chunk) in data.chunks(CHAIN_PAYLOAD).enumerate() {
        let mut page = new_buf(KIND_CHAIN);
        put_u32(&mut page, 8, ids.get(i + 1).copied().unwrap_or(NO_PAGE));
        put_u32(&mut page, 12, chunk.len() as u32);
        page[HEADER..HEADER + chunk.len()].copy_from_slice(chunk);
        out.push((ids[i], page));
    }
    out
}

impl Store {
    /// Starts an empty data file.
    pub fn create(file: Arc<dyn File>, cache: Arc<PageCache>, id: u32, uuid: [u8; 16], limits: Limits) -> Result<Store> {
        let meta = Meta { counter: 0, version: 0, catalog: NO_PAGE, catalog_len: 0, freelist: NO_PAGE, freelist_len: 0, next_page: FIRST_DATA_PAGE, wal_gen: 1, uuid };
        file.set_len(0)?;
        file.write_at(&meta.encode(), 0)?;
        file.write_at(&vec![0u8; PAGE_SIZE], PAGE_SIZE as u64)?;
        file.sync()?;
        Ok(Store::assemble(file, cache, id, meta, Vec::new(), Vec::new(), limits))
    }

    /// Opens a data file as of its last checkpoint. Returns the store, its
    /// header and the catalog the checkpoint stored.
    ///
    /// Of the two header copies the newer valid one is taken, and a copy
    /// is passed over only for what was read of it: bytes that do not
    /// check, or a file too short to hold it. A copy that cannot be read
    /// may be the newer one. The older would then be a checkpoint whose
    /// log is gone, and the file would be cut back to its length below:
    /// so a read that fails is returned as the error it is.
    pub fn open(file: Arc<dyn File>, cache: Arc<PageCache>, id: u32, limits: Limits) -> Result<(Store, Meta, Vec<u8>)> {
        let mut best: Option<Meta> = None;
        for slot in 0..2u64 {
            let mut page = vec![0u8; PAGE_SIZE];
            match read_exact_at(&*file, &mut page, slot * PAGE_SIZE as u64) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => continue,
                Err(e) => return Err(Error::Io(e)),
            }
            if let Some(meta) = Meta::decode(&page) {
                if best.as_ref().is_none_or(|b| meta.counter > b.counter) {
                    best = Some(meta);
                }
            }
        }
        let meta = best.ok_or_else(|| Error::corrupt("neither copy of the data file's header is valid"))?;
        let read_chain = |head: PageId, len: u64| -> Result<(Vec<u8>, Vec<PageId>)> {
            let mut data = Vec::with_capacity(len as usize);
            let mut pages = Vec::new();
            let mut id = head;
            while id != NO_PAGE {
                let mut page = vec![0u8; PAGE_SIZE];
                // A header that names a page the file does not have is
                // damage, like a page that fails its checksum, and not
                // a failing disk.
                read_exact_at(&*file, &mut page, u64::from(id) * PAGE_SIZE as u64).map_err(|e| match e.kind() {
                    std::io::ErrorKind::UnexpectedEof => Error::corrupt(format!("page {id} of the checkpoint lies beyond the end of the data file")),
                    _ => Error::Io(e),
                })?;
                if !verify(&page) || kind(&page) != KIND_CHAIN {
                    return Err(Error::corrupt(format!("page {id} of the checkpoint is damaged")));
                }
                let used = get_u32(&page, 12) as usize;
                if used > CHAIN_PAYLOAD || pages.len() > meta.next_page as usize {
                    return Err(Error::corrupt(format!("page {id} of the checkpoint is malformed")));
                }
                data.extend_from_slice(&page[HEADER..HEADER + used]);
                pages.push(id);
                id = get_u32(&page, 8);
            }
            if data.len() as u64 != len {
                return Err(Error::corrupt("the checkpoint's catalog or free list has the wrong length"));
            }
            Ok((data, pages))
        };
        let (catalog, mut chain_pages) = read_chain(meta.catalog, meta.catalog_len)?;
        let (free_bytes, free_chain) = read_chain(meta.freelist, meta.freelist_len)?;
        chain_pages.extend(free_chain);
        let free: Vec<PageId> = free_bytes.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        // The header may have come from the system's cache without ever
        // having reached the disk: a checkpoint whose last sync failed
        // leaves it so, for whoever opens the file next. Whoever does
        // will remove the log this checkpoint replaced, and that log is
        // then the only durable copy of what the header stands for. So
        // the header is written again and synced before it is believed.
        // If it was on disk, this writes what is there, and no tear of
        // that changes anything; if it was not, this is the checkpoint's
        // own last step, made at last or torn like any other.
        file.write_at(&meta.encode(), (meta.counter % 2) * PAGE_SIZE as u64)?;
        file.sync()?;
        // Whatever lies beyond the checkpoint's last page was written by
        // transactions the log will replay; drop it.
        let end = u64::from(meta.next_page) * PAGE_SIZE as u64;
        if file.len()? > end {
            file.set_len(end)?;
        }
        Ok((Store::assemble(file, cache, id, meta.clone(), free, chain_pages, limits), meta, catalog))
    }

    /// The uuid of a data file that nothing has open.
    pub fn uuid_of(file: &dyn File) -> Result<[u8; 16]> {
        let mut best: Option<Meta> = None;
        for slot in 0..2u64 {
            let mut page = vec![0u8; PAGE_SIZE];
            match read_exact_at(file, &mut page, slot * PAGE_SIZE as u64) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => continue,
                Err(e) => return Err(Error::Io(e)),
            }
            if let Some(meta) = Meta::decode(&page) {
                if best.as_ref().is_none_or(|b| meta.counter > b.counter) {
                    best = Some(meta);
                }
            }
        }
        best.map(|meta| meta.uuid).ok_or_else(|| Error::corrupt("neither copy of the data file's header is valid"))
    }

    /// Gives a data file that nothing has open another uuid: a new
    /// history, which no log file of the old one is taken for (see
    /// `archive.rs`). The checkpoint is otherwise as it was, written as
    /// the next one is, to the other copy of the header: a crash leaves
    /// the file with one uuid or the other.
    pub fn rebrand(file: &dyn File, uuid: [u8; 16]) -> Result<()> {
        let mut best: Option<Meta> = None;
        for slot in 0..2u64 {
            let mut page = vec![0u8; PAGE_SIZE];
            match read_exact_at(file, &mut page, slot * PAGE_SIZE as u64) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => continue,
                Err(e) => return Err(Error::Io(e)),
            }
            if let Some(meta) = Meta::decode(&page) {
                if best.as_ref().is_none_or(|b| meta.counter > b.counter) {
                    best = Some(meta);
                }
            }
        }
        let meta = best.ok_or_else(|| Error::corrupt("neither copy of the data file's header is valid"))?;
        let meta = Meta { counter: meta.counter + 1, uuid, ..meta };
        file.write_at(&meta.encode(), (meta.counter % 2) * PAGE_SIZE as u64)?;
        file.sync()?;
        Ok(())
    }

    fn assemble(file: Arc<dyn File>, cache: Arc<PageCache>, id: u32, meta: Meta, free: Vec<PageId>, chain_pages: Vec<PageId>, limits: Limits) -> Store {
        Store {
            id,
            file,
            cache,
            mem: RwLock::new(Mem::default()),
            alloc: Mutex::new(Alloc {
                free,
                next_page: meta.next_page,
                young: PageSet::default(),
                wait_readers: BTreeMap::new(),
                wait_checkpoint: BTreeMap::new(),
                durable: meta.version,
                chain_pages,
                lent: PageSet::default(),
                meta: meta.clone(),
            }),
            pins: Mutex::new(Pins { published: meta.version, counts: BTreeMap::new() }),
            limits,
            failed: AtomicBool::new(false),
            serials: Mutex::new(HashMap::new()),
            in_mem: AtomicUsize::new(0),
            maybe: MaybeInMem::new(),
            release_at: AtomicU64::new(u64::MAX),
            disk_reads: AtomicU64::new(0),
            disk_writes: AtomicU64::new(0),
        }
    }

    pub fn meta(&self) -> Meta {
        self.alloc.lock().unwrap().meta.clone()
    }

    /// Reads the data file's bytes as they lie, for a copy of it.
    pub fn read_file(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        read_exact_at(&*self.file, buf, offset)
    }

    fn check_failed(&self) -> Result<()> {
        if self.failed.load(Ordering::Relaxed) {
            return Err(Error::Io(std::io::Error::other("an earlier write to the data file failed")));
        }
        Ok(())
    }

    /// Notes how many pages `mem` holds, for readers: called with its
    /// lock held for writing, after every change to it. `left` says
    /// that pages have gone from it, which is the time to forget their
    /// marks; so is having marked many since that was last done, as
    /// pages also leave one by one when they are freed.
    fn count_mem(&self, mem: &Mem, left: bool) {
        if left || self.maybe.marked.load(Ordering::Relaxed) > MAYBE_WORDS * 8 {
            self.maybe.renew(mem);
        }
        self.in_mem.store(mem.dirty.len() + mem.flushing.len(), Ordering::Release);
    }

    fn write_page(&self, id: PageId, page: &[u8]) -> Result<()> {
        self.check_failed()?;
        self.disk_writes.fetch_add(1, Ordering::Relaxed);
        self.file.write_at(&sealed(page), u64::from(id) * PAGE_SIZE as u64).inspect_err(|_| self.failed.store(true, Ordering::Relaxed))?;
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        self.check_failed()?;
        self.file.sync().inspect_err(|_| self.failed.store(true, Ordering::Relaxed))?;
        Ok(())
    }

    /// Reads a page from the file, checking it is what was written.
    fn read_disk(&self, id: PageId) -> Result<Page> {
        let mut buf = vec![0u8; PAGE_SIZE];
        self.disk_reads.fetch_add(1, Ordering::Relaxed);
        read_exact_at(&*self.file, &mut buf, u64::from(id) * PAGE_SIZE as u64).map_err(|e| match e.kind() {
            std::io::ErrorKind::UnexpectedEof => Error::corrupt(format!("page {id} lies beyond the end of the data file")),
            _ => Error::Io(e),
        })?;
        if !verify(&buf) {
            return Err(Error::corrupt(format!("page {id} fails its checksum")));
        }
        Ok(Arc::from(buf))
    }

    /// A committed page, from memory or from the file.
    pub fn read(&self, id: PageId) -> Result<Page> {
        // A page a reader can reach was put in `mem`, and counted, before
        // the version that holds it was shown to anyone; and it is in
        // the cache or the file before it leaves and is counted out.
        if self.in_mem.load(Ordering::Acquire) > 0 && self.maybe.may_hold(id) {
            let mem = self.mem.read().unwrap();
            if let Some(page) = mem.dirty.get(&id).or_else(|| mem.flushing.get(&id)) {
                return Ok(page.clone());
            }
        }
        if let Some(page) = self.cache.get(self.id, id) {
            return Ok(page);
        }
        let page = self.read_disk(id)?;
        self.cache.insert(self.id, id, page.clone());
        Ok(page)
    }

    fn take_page(&self, lent: bool) -> PageId {
        let mut alloc = self.alloc.lock().unwrap();
        let id = match alloc.free.pop() {
            Some(id) => id,
            None => {
                let id = alloc.next_page;
                alloc.next_page += 1;
                id
            }
        };
        alloc.young.insert(id);
        if lent {
            alloc.lent.insert(id);
        }
        id
    }

    /// The next number of a table's counter, which stands at `floor` in
    /// the transaction that asks: `floor`, or more if a transaction that
    /// has not committed, or has since been undone, was given that
    /// already. `claim` keeps the number from everyone who asks later,
    /// whether or not the asker commits: a writer and the forks beside it
    /// each count on their own copy of the counter, and this is all that
    /// keeps them from giving out the same number.
    fn next_serial(&self, table: u32, column: usize, floor: i64, claim: bool) -> i64 {
        let mut serials = self.serials.lock().unwrap();
        let number = serials.get(&(table, column)).map_or(floor, |given| floor.max(*given));
        if claim {
            serials.insert((table, column), number.saturating_add(1));
        }
        number
    }

    // ------------------------------------------------------------ readers

    /// Holds the published version for a reader: pages it can reach are
    /// kept until `unpin`.
    pub fn pin(&self) -> u64 {
        let mut pins = self.pins.lock().unwrap();
        let version = pins.published;
        *pins.counts.entry(version).or_insert(0) += 1;
        version
    }

    pub fn unpin(&self, version: u64) {
        // The oldest version still needed, if this reader was the last
        // to hold back anything older.
        let moved_on = {
            let mut pins = self.pins.lock().unwrap();
            let oldest = pins.counts.keys().next().copied();
            if let Some(count) = pins.counts.get_mut(&version) {
                *count -= 1;
                if *count == 0 {
                    pins.counts.remove(&version);
                }
            }
            (oldest == Some(version) && !pins.counts.contains_key(&version)).then(|| pins.counts.keys().next().copied().unwrap_or(pins.published).min(pins.published))
        };
        // Every reader comes through here, and after a burst of writes
        // pages wait that only a checkpoint will free: the allocator's
        // lock is taken only when there is something to give back.
        if moved_on.is_some_and(|oldest| oldest >= self.release_at.load(Ordering::Acquire)) {
            self.release();
        }
    }

    /// Makes `version` the one new readers see.
    pub fn publish(&self, version: u64) {
        let oldest = {
            let mut pins = self.pins.lock().unwrap();
            pins.published = pins.published.max(version);
            pins.counts.keys().next().copied().unwrap_or(pins.published).min(pins.published)
        };
        if oldest >= self.release_at.load(Ordering::Acquire) {
            self.release();
        }
    }

    pub fn published(&self) -> u64 {
        self.pins.lock().unwrap().published
    }

    /// The oldest version any reader, present or about to start, can see.
    fn oldest_needed(&self) -> u64 {
        let pins = self.pins.lock().unwrap();
        pins.counts.keys().next().copied().unwrap_or(pins.published).min(pins.published)
    }

    /// Returns to the free list every freed page nothing can read any
    /// more.
    fn release(&self) {
        let mut alloc = self.alloc.lock().unwrap();
        if alloc.wait_readers.is_empty() && alloc.wait_checkpoint.is_empty() {
            self.note_waiting(&alloc);
            return;
        }
        let oldest = self.oldest_needed();
        let durable = alloc.durable;
        let mut released = Vec::new();
        while let Some(entry) = alloc.wait_readers.first_entry() {
            if *entry.key() > oldest {
                break;
            }
            released.extend(entry.remove());
        }
        while let Some(entry) = alloc.wait_checkpoint.first_entry() {
            if *entry.key() > oldest.min(durable) {
                break;
            }
            released.extend(entry.remove());
        }
        self.note_waiting(&alloc);
        if released.is_empty() {
            return;
        }
        {
            let mut mem = self.mem.write().unwrap();
            for id in &released {
                mem.dirty.remove(id);
            }
            self.count_mem(&mem, false);
        }
        for id in &released {
            self.cache.remove(self.id, *id);
        }
        alloc.free.extend(released);
    }

    /// Notes the oldest reader's version from which some waiting page
    /// can be given back: called with the allocator's lock held, after
    /// every change to what waits or to the checkpoint on disk.
    fn note_waiting(&self, alloc: &Alloc) {
        let readers = alloc.wait_readers.keys().next().copied().unwrap_or(u64::MAX);
        let checkpoint = alloc.wait_checkpoint.keys().next().copied().filter(|freed_at| *freed_at <= alloc.durable).unwrap_or(u64::MAX);
        self.release_at.store(readers.min(checkpoint), Ordering::Release);
    }

    // ------------------------------------------------------------ writers

    /// Writes dirty pages to the file when there are too many, so they
    /// can leave memory. They become durable with the next checkpoint.
    fn flush_dirty_if_many(&self) -> Result<()> {
        let pages: Vec<(PageId, Page)> = {
            let mem = self.mem.read().unwrap();
            if mem.dirty.len() <= self.limits.max_dirty {
                return Ok(());
            }
            mem.dirty.iter().map(|(id, page)| (*id, page.clone())).collect()
        };
        for (id, page) in &pages {
            self.write_page(*id, page)?;
        }
        let mut mem = self.mem.write().unwrap();
        for (id, page) in pages {
            // Skip a page freed and given out again while it was written.
            if mem.dirty.get(&id).is_some_and(|current| Arc::ptr_eq(current, &page)) {
                self.cache.insert(self.id, id, page);
                mem.dirty.remove(&id);
            }
        }
        self.count_mem(&mem, true);
        Ok(())
    }

    // --------------------------------------------------------- checkpoint

    /// Fixes what a checkpoint of `version` will write. Must be called
    /// with no transaction committing, and with every commit up to
    /// `version` already applied to the store.
    pub fn capture(&self, version: u64, catalog: &[u8], wal_gen: u64) -> Result<Capture> {
        self.check_failed()?;
        let mut alloc = self.alloc.lock().unwrap();
        {
            let mut mem = self.mem.write().unwrap();
            debug_assert!(mem.flushing.is_empty(), "one checkpoint at a time");
            mem.flushing = std::mem::take(&mut mem.dirty);
        }
        // From here on, a page handed out before this moment may be named
        // by this checkpoint.
        alloc.young.clear();
        // Freed pages that wait only for readers and are in the set being
        // written must also wait until the checkpoint is done, so their
        // numbers cannot be reused under the writer. The rest are on disk
        // already and nothing will write them again.
        let waiting = std::mem::take(&mut alloc.wait_readers);
        {
            let mem = self.mem.read().unwrap();
            for (freed_at, pages) in waiting {
                let (writing, written): (Vec<PageId>, Vec<PageId>) = pages.into_iter().partition(|id| mem.flushing.contains_key(id));
                if !writing.is_empty() {
                    alloc.wait_checkpoint.entry(freed_at).or_default().extend(writing);
                }
                if !written.is_empty() {
                    alloc.wait_readers.entry(freed_at).or_default().extend(written);
                }
            }
        }
        self.note_waiting(&alloc);

        let take = |alloc: &mut Alloc| match alloc.free.pop() {
            Some(id) => id,
            None => {
                let id = alloc.next_page;
                alloc.next_page += 1;
                id
            }
        };
        let catalog_ids: Vec<PageId> = (0..chain_pages_needed(catalog.len())).map(|_| take(&mut alloc)).collect();
        // The free list as of this version: what is free now, what waits
        // (all of it was freed at or before this version), and the pages
        // of the checkpoint this one replaces.
        let old_chain = alloc.chain_pages.clone();
        let mut free: Vec<PageId> = alloc.free.clone();
        free.extend(alloc.wait_readers.values().flatten());
        free.extend(alloc.wait_checkpoint.values().flatten());
        free.extend(&alloc.lent);
        free.extend(&old_chain);
        // Its own pages come from the end of the file, so that taking
        // them does not change the list being written.
        let free_ids: Vec<PageId> = (0..chain_pages_needed(free.len() * 4))
            .map(|_| {
                let id = alloc.next_page;
                alloc.next_page += 1;
                id
            })
            .collect();
        let free_bytes: Vec<u8> = free.iter().flat_map(|id| id.to_le_bytes()).collect();

        let mut chain = build_chain(catalog, &catalog_ids);
        chain.extend(build_chain(&free_bytes, &free_ids));
        let meta = Meta {
            counter: alloc.meta.counter + 1,
            version,
            catalog: catalog_ids.first().copied().unwrap_or(NO_PAGE),
            catalog_len: catalog.len() as u64,
            freelist: free_ids.first().copied().unwrap_or(NO_PAGE),
            freelist_len: free_bytes.len() as u64,
            next_page: alloc.next_page,
            wal_gen,
            uuid: alloc.meta.uuid,
        };
        Ok(Capture { version, chain, meta, old_chain })
    }

    /// Writes a captured checkpoint and makes it the file's state. May
    /// run while later transactions commit.
    pub fn write_checkpoint(&self, capture: Capture) -> Result<()> {
        let mut pages: Vec<(PageId, Page)> = {
            let mem = self.mem.read().unwrap();
            mem.flushing.iter().map(|(id, page)| (*id, page.clone())).collect()
        };
        pages.sort_by_key(|(id, _)| *id);
        for (id, page) in &pages {
            self.write_page(*id, page)?;
        }
        for (id, page) in &capture.chain {
            self.write_page(*id, page)?;
        }
        self.sync()?;
        // The header copies alternate, so a torn write leaves the other.
        let slot = capture.meta.counter % 2;
        self.check_failed()?;
        self.file.write_at(&capture.meta.encode(), slot * PAGE_SIZE as u64).inspect_err(|_| self.failed.store(true, Ordering::Relaxed))?;
        self.sync()?;

        // The written pages move to the cache before the checkpoint counts
        // as durable: until then none of their numbers can be released
        // and reused, so none can be put back in the cache stale.
        for (id, page) in pages {
            self.cache.insert(self.id, id, page);
        }
        {
            let mut mem = self.mem.write().unwrap();
            mem.flushing.clear();
            self.count_mem(&mem, true);
        }
        {
            let mut alloc = self.alloc.lock().unwrap();
            alloc.durable = capture.version;
            alloc.free.extend(capture.old_chain);
            alloc.chain_pages = capture.chain.iter().map(|(id, _)| *id).collect();
            alloc.meta = capture.meta;
        }
        self.release();
        Ok(())
    }

    // -------------------------------------------------------- inspection

    /// Pages committed and not yet in the data file.
    pub fn dirty_pages(&self) -> usize {
        let mem = self.mem.read().unwrap();
        mem.dirty.len() + mem.flushing.len()
    }

    /// Every page number that is not in use by the newest version, and
    /// the count of pages in the file, for the consistency check. Call
    /// with no writer active.
    pub fn free_pages(&self) -> (Vec<PageId>, PageId, Vec<PageId>) {
        let alloc = self.alloc.lock().unwrap();
        let mut free = alloc.free.clone();
        free.extend(alloc.wait_readers.values().flatten());
        free.extend(alloc.wait_checkpoint.values().flatten());
        free.extend(&alloc.lent);
        (free, alloc.next_page, alloc.chain_pages.clone())
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn file_pages(&self) -> PageId {
        self.alloc.lock().unwrap().next_page
    }

    pub fn free_count(&self) -> usize {
        self.alloc.lock().unwrap().free.len()
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        self.cache.remove_store(self.id);
    }
}

impl Pages for Store {
    fn page(&self, id: PageId) -> Result<Page> {
        self.read(id)
    }
}

/// A transaction's view of the pages: everything committed, plus the pages
/// it has written itself, which nobody else can see until `commit`.
pub struct Writer {
    store: Arc<Store>,
    private: PageMap<Page>,
    /// Private pages written to the file to save memory.
    spilled: PageSet,
    /// Private pages a savepoint may have to bring back: they are no
    /// longer changed in place.
    frozen: PageSet,
    /// Numbers this transaction took and gave back, to take again.
    recycled: Vec<PageId>,
    /// Committed pages this transaction's trees no longer use.
    freed: Vec<PageId>,
    /// Frozen pages they no longer use.
    frozen_freed: Vec<PageId>,
    /// Every number handed out, in order, for `rollback_to`.
    alloc_log: Vec<PageId>,
    /// Whether this writes beside the database's one writer: see `fork`.
    fork: bool,
    /// Whether work its trees leave due, and that can be done beside the
    /// writers, is left to whoever does it there: see `fts::Merging`.
    leaves_beside: bool,
    /// Whether it has left such work.
    left_due: bool,
}

/// A point in a transaction's page writes to return to.
#[derive(Clone, Copy, Debug)]
pub struct Mark {
    alloc_log: usize,
    freed: usize,
    frozen_freed: usize,
}

impl Writer {
    pub fn new(store: Arc<Store>) -> Writer {
        Writer {
            store,
            private: PageMap::default(),
            spilled: PageSet::default(),
            frozen: PageSet::default(),
            recycled: Vec::new(),
            freed: Vec::new(),
            frozen_freed: Vec::new(),
            alloc_log: Vec::new(),
            fork: false,
            leaves_beside: false,
            left_due: false,
        }
    }

    /// Says whether work that can be done beside the writers is left to
    /// whoever does it there. A writer leaves none unless told to.
    pub fn leave_beside(&mut self, on: bool) {
        self.leaves_beside = on;
    }

    pub fn leaves_beside(&self) -> bool {
        self.leaves_beside
    }

    /// Notes that the transaction has left such work due.
    pub fn note_due(&mut self) {
        self.left_due = true;
    }

    pub fn left_due(&self) -> bool {
        self.left_due
    }

    /// Writes every page the transaction holds in memory to the file,
    /// where it will lie once the transaction commits: so that its
    /// commit has nothing left to carry, and is over in a moment. As
    /// with `spill_if_many`, nothing durable is touched.
    pub fn spill_all(&mut self) -> Result<()> {
        for (id, page) in std::mem::take(&mut self.private) {
            self.store.write_page(id, &page)?;
            self.spilled.insert(id);
        }
        Ok(())
    }

    /// A writer that may work while another does: on a version it does
    /// not have to itself, which the caller keeps pinned as a reader
    /// would. Its pages are its own until `commit`, and that may be
    /// called only if no other writer has committed since the version it
    /// began on, and with every other writer held off. Otherwise there
    /// is only `rollback`.
    pub fn fork(store: Arc<Store>) -> Writer {
        let mut writer = Writer::new(store);
        writer.fork = true;
        writer
    }

    /// Makes a fork the database's writer. The caller holds every other
    /// writer off, and none has committed since the fork began.
    pub fn attach(&mut self) {
        if !self.fork {
            return;
        }
        self.fork = false;
        // Its pages are a writer's now: the next checkpoint finds no
        // transaction open, and so none of them lent.
        let mut alloc = self.store.alloc.lock().unwrap();
        for id in self
            .private
            .keys()
            .chain(&self.spilled)
            .chain(&self.recycled)
        {
            alloc.lent.remove(id);
        }
    }

    /// Pages held in memory for the transaction.
    pub fn private_pages(&self) -> usize {
        self.private.len()
    }

    /// The next number of a table's `auto_increment` counter, which
    /// stands at `floor` in this transaction. `own` says the transaction
    /// made the table itself and has not committed: nobody else can see
    /// the table, so the number need not be kept from anyone, and is
    /// not, since an undone table's number is given to the next table.
    pub fn next_serial(&self, table: u32, column: usize, floor: i64, own: bool) -> i64 {
        self.store.next_serial(table, column, floor, !own)
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    fn owns(&self, id: PageId) -> bool {
        self.private.contains_key(&id) || self.spilled.contains(&id)
    }

    /// Whether the transaction has written anything.
    pub fn is_clean(&self) -> bool {
        self.alloc_log.is_empty() && self.freed.is_empty()
    }

    /// Remembers the present state. Pages written so far are kept as they
    /// are from here on, so `rollback_to` can return to them.
    pub fn savepoint(&mut self) -> Mark {
        self.frozen.extend(self.private.keys());
        self.frozen.extend(&self.spilled);
        Mark { alloc_log: self.alloc_log.len(), freed: self.freed.len(), frozen_freed: self.frozen_freed.len() }
    }

    /// Undoes every page write since the mark.
    pub fn rollback_to(&mut self, mark: Mark) {
        for id in self.alloc_log.split_off(mark.alloc_log) {
            let had = self.private.remove(&id).is_some() | self.spilled.remove(&id);
            self.frozen.remove(&id);
            if had {
                self.store.cache.remove(self.store.id, id);
                self.recycled.push(id);
            }
        }
        self.freed.truncate(mark.freed);
        self.frozen_freed.truncate(mark.frozen_freed);
    }

    /// Writes some private pages to the file when there are too many.
    /// Their numbers are free in the last checkpoint, so nothing durable
    /// is touched.
    fn spill_if_many(&mut self) -> Result<()> {
        if self.private.len() <= self.store.limits.max_private {
            return Ok(());
        }
        // Leaves and chain pages first: branches are read again at once.
        let mut victims: Vec<PageId> = self.private.iter().filter(|(_, page)| kind(page) != KIND_BRANCH).map(|(id, _)| *id).collect();
        victims.truncate(self.private.len() / 2);
        for id in victims {
            // Written first and let go of after: a page whose write
            // failed is still the transaction's, to give back when it
            // is undone.
            self.store.write_page(id, &self.private[&id])?;
            self.private.remove(&id);
            self.spilled.insert(id);
        }
        Ok(())
    }

    /// Makes the transaction's pages part of the committed state as
    /// `version`. The caller publishes the version when it is durable.
    pub fn commit(mut self, version: u64) -> Result<()> {
        for id in std::mem::take(&mut self.frozen_freed) {
            if self.private.remove(&id).is_some() | self.spilled.remove(&id) {
                self.store.cache.remove(self.store.id, id);
                self.recycled.push(id);
            }
        }
        {
            let mut alloc = self.store.alloc.lock().unwrap();
            let mut mem = self.store.mem.write().unwrap();
            if self.fork {
                // Every page it took is now in one of these three.
                for id in self
                    .private
                    .keys()
                    .chain(&self.spilled)
                    .chain(&self.recycled)
                {
                    alloc.lent.remove(id);
                }
            }
            for id in self.freed.drain(..) {
                // A page handed out since the last checkpoint is named by
                // none; any other may be.
                if alloc.young.contains(&id) {
                    alloc.wait_readers.entry(version).or_default().push(id);
                } else {
                    alloc.wait_checkpoint.entry(version).or_default().push(id);
                }
            }
            for (id, page) in self.private.drain() {
                self.store.maybe.mark(id);
                mem.dirty.insert(id, page);
            }
            self.store.count_mem(&mem, false);
            alloc.free.append(&mut self.recycled);
            self.store.note_waiting(&alloc);
        }
        self.spilled.clear();
        self.store.flush_dirty_if_many()
    }

    /// Forgets everything the transaction wrote.
    pub fn rollback(mut self) {
        self.abandon();
    }

    fn abandon(&mut self) {
        let mut ids: Vec<PageId> = self.private.drain().map(|(id, _)| id).collect();
        for id in self.spilled.drain() {
            self.store.cache.remove(self.store.id, id);
            ids.push(id);
        }
        ids.append(&mut self.recycled);
        self.freed.clear();
        self.frozen_freed.clear();
        self.alloc_log.clear();
        if !ids.is_empty() {
            let mut alloc = self.store.alloc.lock().unwrap();
            if self.fork {
                for id in &ids {
                    alloc.lent.remove(id);
                }
            }
            alloc.free.extend(ids);
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.abandon();
    }
}

impl Pages for Writer {
    fn page(&self, id: PageId) -> Result<Page> {
        if let Some(page) = self.private.get(&id) {
            return Ok(page.clone());
        }
        if self.spilled.contains(&id) {
            return self.store.read_disk(id);
        }
        self.store.read(id)
    }
}

impl PagesMut for Writer {
    fn alloc(&mut self, buf: Vec<u8>) -> Result<PageId> {
        self.alloc_page(Arc::from(buf))
    }

    fn alloc_page(&mut self, page: Page) -> Result<PageId> {
        let id = match self.recycled.pop() {
            Some(id) => id,
            None => self.store.take_page(self.fork),
        };
        self.alloc_log.push(id);
        self.private.insert(id, page);
        self.spill_if_many()?;
        Ok(id)
    }

    fn is_private(&self, id: PageId) -> bool {
        self.owns(id) && !self.frozen.contains(&id)
    }

    fn replace(&mut self, id: PageId, buf: Vec<u8>) {
        debug_assert!(self.is_private(id));
        self.spilled.remove(&id);
        self.private.insert(id, Arc::from(buf));
    }

    fn edit(&mut self, id: PageId, change: &mut dyn FnMut(&mut [u8])) -> bool {
        if self.frozen.contains(&id) {
            return false;
        }
        match self.private.get_mut(&id).and_then(Arc::get_mut) {
            Some(buf) => {
                change(buf);
                true
            }
            None => false,
        }
    }

    fn free(&mut self, id: PageId) {
        if self.frozen.contains(&id) {
            self.frozen_freed.push(id);
        } else if self.private.remove(&id).is_some() | self.spilled.remove(&id) {
            self.recycled.push(id);
        } else {
            self.freed.push(id);
        }
    }
}
