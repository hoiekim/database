//! The file system as the engine sees it: positioned reads and writes, a
//! sync that makes them durable, and directory operations. `DiskVfs` is the
//! real one. `MemVfs` keeps files in memory and remembers which writes were
//! never synced, so a test can ask for the state a power cut would leave
//! behind, at any point, and open the database on it.
//!
//! `MemVfs` is also where a test makes the disk fail. A **fault plan** is
//! asked about every operation that changes the disk, by its number and
//! kind, and may fail it, fill the disk under it, fail a sync so that what
//! it covered is lost, or have a sync lie. A **journal** keeps every such
//! operation, so that the disk as it stood before each one, which is where
//! a process killed at that step would have left it, can be rebuilt and
//! cut power to in every way that matters, one operation after another.
//!
//! A read changes nothing, so it has no number and no plan is asked
//! about it. A test names instead the file that cannot be read, or the
//! bytes of it (`fail_reads`): a disk that has them and cannot give them
//! just now.

use std::collections::BTreeMap;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub trait File: Send + Sync {
    /// Reads up to `buf.len()` bytes at `offset`; fewer only at end of file.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
    /// Makes every write so far durable (fdatasync).
    fn sync(&self) -> io::Result<()>;
    fn len(&self) -> io::Result<u64>;
    fn set_len(&self, len: u64) -> io::Result<()>;
}

pub trait Vfs: Send + Sync {
    fn open(&self, path: &Path, create: bool) -> io::Result<Arc<dyn File>>;
    fn remove(&self, path: &Path) -> io::Result<()>;
    fn exists(&self, path: &Path) -> bool;
    /// File names (not paths) directly inside `dir`.
    fn list(&self, dir: &Path) -> io::Result<Vec<String>>;
    /// The names of the directories in `dir`.
    fn list_dirs(&self, dir: &Path) -> io::Result<Vec<String>>;
    fn create_dir_all(&self, dir: &Path) -> io::Result<()>;
    /// Makes creations and removals inside `dir` durable.
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;
    /// Gives a file another name, replacing any file of that name.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
}

pub fn read_exact_at(file: &dyn File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let n = file.read_at(&mut buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "read past end of file"));
        }
        done += n;
    }
    Ok(())
}

// ---------------------------------------------------------------- disk

pub struct DiskVfs;

struct DiskFile(std::fs::File);

impl File for DiskFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        std::os::unix::fs::FileExt::read_at(&self.0, buf, offset)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        std::os::unix::fs::FileExt::write_all_at(&self.0, buf, offset)
    }
    fn sync(&self) -> io::Result<()> {
        self.0.sync_data()
    }
    fn len(&self) -> io::Result<u64> {
        Ok(self.0.metadata()?.len())
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.0.set_len(len)
    }
}

impl Vfs for DiskVfs {
    fn open(&self, path: &Path, create: bool) -> io::Result<Arc<dyn File>> {
        let file = std::fs::OpenOptions::new().read(true).write(true).create(create).truncate(false).open(path)?;
        Ok(Arc::new(DiskFile(file)))
    }
    fn remove(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            if let Ok(name) = entry?.file_name().into_string() {
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }
    fn list_dirs(&self, dir: &Path) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if let Ok(name) = entry.file_name().into_string() {
                    names.push(name);
                }
            }
        }
        names.sort();
        Ok(names)
    }
    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)
    }
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        std::fs::File::open(dir)?.sync_all()
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }
}

// -------------------------------------------------------------- memory

/// A deterministic generator for the crash simulation and the tests:
/// xorshift64*, seeded by the caller so a failing run can be repeated.
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// A number in `0..n`. `n` must not be zero.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    pub fn chance(&mut self, one_in: u64) -> bool {
        self.below(one_in) == 0
    }
}

/// What a disk writes whole or not at all.
pub const SECTOR: u64 = 512;

/// How many sectors a write of `len` bytes at `offset` touches.
pub fn sectors(offset: u64, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    ((offset + len as u64 - 1) / SECTOR - offset / SECTOR + 1) as usize
}

/// An operation about to change the disk, as `MemVfs` shows it to a fault
/// plan (see `MemVfs::set_plan`) before it happens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Op {
    /// Its place among the operations that changed this file system, or
    /// tried to: the first is 0.
    pub index: u64,
    pub kind: OpKind,
    /// The file; for `SyncDir`, the directory.
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpKind {
    Create,
    /// `grows` is how many bytes longer the file would be for it.
    Write { offset: u64, len: usize, grows: u64 },
    SetLen { len: u64 },
    Sync,
    SyncDir,
    Remove,
    Rename { to: PathBuf },
}

/// What a fault plan makes of an operation. A fault that speaks of
/// another kind of operation than the one it is given for leaves it be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Nothing: the operation happens.
    None,
    /// It fails with this kind of error and changes nothing.
    Fail(io::ErrorKind),
    /// A write the disk has room for only the first bytes of, this many:
    /// they are written, and the write fails as on a full disk.
    Short(usize),
    /// A sync that fails, after which the system no longer holds what it
    /// was to make durable: each of those writes has reached the disk or
    /// has not, as the seed decides, and no later sync will write it.
    /// Reads still see them, as they do from an operating system's cache.
    SyncLoses(u64),
    /// A sync that reports success and has made nothing durable.
    Lie,
}

/// Decides, operation by operation, what goes wrong.
pub type Plan = Box<dyn FnMut(&Op) -> Fault + Send>;

/// What a power cut leaves of one unsynced change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    Lost,
    Kept,
    /// Of a write: only the first sectors it touches, this many.
    Torn(usize),
    /// Of a write: only the sectors it touches whose bit is set, the
    /// first being bit 0 (and the 65th bit 0 again).
    Sectors(u64),
}

/// A change no sync has made durable, as shown to whoever decides what a
/// power cut leaves of it: see `MemVfs::crash_image_with`.
#[derive(Clone, Debug)]
pub struct Unsynced<'a> {
    /// The index of the operation that made it.
    pub op: u64,
    pub path: &'a Path,
    pub kind: UnsyncedKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsyncedKind {
    Write { offset: u64, len: usize },
    SetLen(u64),
    Create,
    Remove,
    Rename,
}

#[derive(Clone)]
enum Change {
    Write { offset: u64, data: Vec<u8> },
    SetLen(u64),
}

struct MemData {
    /// Its place in `MemState::all`.
    id: usize,
    /// What a crash is guaranteed to keep.
    durable: Vec<u8>,
    /// What reads see: every write so far.
    current: Vec<u8>,
    /// Writes since the last sync, oldest first, each with the index of
    /// the operation that made it.
    pending: Vec<(u64, Change)>,
}

type FileRef = Arc<Mutex<MemData>>;

fn apply(bytes: &mut Vec<u8>, change: &Change) {
    match change {
        Change::Write { offset, data } => write_into(bytes, *offset, data),
        Change::SetLen(len) => bytes.resize(*len as usize, 0),
    }
}

fn write_into(bytes: &mut Vec<u8>, offset: u64, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    let end = offset as usize + data.len();
    if bytes.len() < end {
        bytes.resize(end, 0);
    }
    bytes[offset as usize..end].copy_from_slice(data);
}

/// Applies what `fate` leaves of a change.
fn apply_fated(bytes: &mut Vec<u8>, change: &Change, fate: Fate) {
    let (offset, data) = match (change, fate) {
        (_, Fate::Lost) => return,
        (Change::Write { offset, data }, Fate::Torn(_) | Fate::Sectors(_)) => (*offset, data),
        _ => return apply(bytes, change),
    };
    let first = offset / SECTOR;
    for i in 0..sectors(offset, data.len()) {
        let kept = match fate {
            Fate::Torn(n) => i < n,
            Fate::Sectors(mask) => mask >> (i % 64) & 1 == 1,
            Fate::Lost | Fate::Kept => unreachable!("handled above"),
        };
        if kept {
            let from = offset.max((first + i as u64) * SECTOR);
            let to = (offset + data.len() as u64).min((first + i as u64 + 1) * SECTOR);
            write_into(bytes, from, &data[(from - offset) as usize..(to - offset) as usize]);
        }
    }
}

/// A change to the names in a directory.
#[derive(Clone)]
enum Renaming {
    Create(PathBuf, FileRef),
    Remove(PathBuf),
    Rename(PathBuf, PathBuf),
}

impl Renaming {
    fn path(&self) -> &Path {
        match self {
            Renaming::Create(path, _) | Renaming::Remove(path) | Renaming::Rename(path, _) => path,
        }
    }

    fn apply(&self, names: &mut BTreeMap<PathBuf, FileRef>) {
        match self {
            Renaming::Create(path, file) => {
                names.insert(path.clone(), file.clone());
            }
            Renaming::Remove(path) => {
                names.remove(path);
            }
            // One step: a crash leaves the old name or the new one, and
            // whatever had the new name is gone only with it.
            Renaming::Rename(from, to) => {
                if let Some(file) = names.remove(from) {
                    names.insert(to.clone(), file);
                }
            }
        }
    }

    fn kind(&self) -> UnsyncedKind {
        match self {
            Renaming::Create(..) => UnsyncedKind::Create,
            Renaming::Remove(_) => UnsyncedKind::Remove,
            Renaming::Rename(..) => UnsyncedKind::Rename,
        }
    }
}

/// An operation that changed the disk, kept so that it can be made again
/// on a copy: see `MemVfs::start_journal`.
#[derive(Clone, Debug)]
pub struct Entry {
    pub op: Op,
    effect: Effect,
}

impl Entry {
    /// Whether it is a sync that failed and lost what it covered.
    pub fn lost(&self) -> bool {
        matches!(self.effect, Effect::Sync { lost: Some(_), .. } | Effect::SyncDir { lost: Some(_) })
    }

    /// Where it wrote and how many bytes, if it is a write: fewer than
    /// were asked for when the disk was full.
    pub fn written(&self) -> Option<(u64, usize)> {
        match &self.effect {
            Effect::Write { offset, data, .. } => Some((*offset, data.len())),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
enum Effect {
    Create { file: usize },
    Write { file: usize, offset: u64, data: Vec<u8> },
    SetLen { file: usize, len: u64 },
    /// `lost`: the seed of a sync that failed and lost what it covered.
    Sync { file: usize, lost: Option<u64> },
    SyncDir { lost: Option<u64> },
    Remove,
    Rename { to: PathBuf },
}

#[derive(Default)]
struct MemState {
    /// The names as the running process sees them.
    files: BTreeMap<PathBuf, FileRef>,
    /// The names as of each directory's last sync: what a crash is
    /// guaranteed to keep.
    names: BTreeMap<PathBuf, FileRef>,
    /// Creations, removals and renamings since, oldest first, each with
    /// the index of the operation that made it.
    renamings: Vec<(u64, Renaming)>,
    /// Every file ever made, by `MemData::id`.
    all: Vec<FileRef>,
    /// Counts operations that change the disk, or try to.
    ops: u64,
    crash_at: Option<(u64, u64)>,
    crashed: bool,
    image: Option<MemVfs>,
    plan: Option<Plan>,
    journal: Option<Vec<Entry>>,
    /// The file whose reads fail, and which bytes of it: see `fail_reads`.
    fail_reads: Option<(PathBuf, Range<u64>)>,
}

impl MemState {
    fn create(&mut self, op: u64, path: &Path) -> FileRef {
        let file = Arc::new(Mutex::new(MemData { id: self.all.len(), durable: Vec::new(), current: Vec::new(), pending: Vec::new() }));
        self.all.push(file.clone());
        self.files.insert(path.to_path_buf(), file.clone());
        self.renamings.push((op, Renaming::Create(path.to_path_buf(), file.clone())));
        file
    }

    fn remove(&mut self, op: u64, path: &Path) -> io::Result<()> {
        self.files.remove(path).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))?;
        self.renamings.push((op, Renaming::Remove(path.to_path_buf())));
        Ok(())
    }

    fn rename(&mut self, op: u64, from: &Path, to: &Path) -> io::Result<()> {
        let file = self.files.remove(from).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))?;
        self.files.insert(to.to_path_buf(), file);
        self.renamings.push((op, Renaming::Rename(from.to_path_buf(), to.to_path_buf())));
        Ok(())
    }

    /// Makes a file's writes durable. With `lost`, it is the sync that
    /// failed: each write is durable or gone, as that seed decides.
    fn sync(file: &FileRef, lost: Option<u64>) {
        let mut data = file.lock().unwrap();
        let data = &mut *data;
        let mut rng = lost.map(Rng::new);
        for (_, change) in data.pending.drain(..) {
            let fate = match &mut rng {
                None => Fate::Kept,
                Some(rng) => random_fate(rng, &change),
            };
            apply_fated(&mut data.durable, &change, fate);
        }
    }

    /// Makes the names in `dir` durable, or, with `lost`, each change to
    /// them durable or undone for good as that seed decides.
    fn sync_dir(&mut self, dir: &Path, lost: Option<u64>) {
        let mut rng = lost.map(Rng::new);
        let (here, elsewhere): (Vec<_>, Vec<_>) = std::mem::take(&mut self.renamings).into_iter().partition(|(_, r)| r.path().parent() == Some(dir));
        self.renamings = elsewhere;
        for (_, renaming) in here {
            if rng.as_mut().is_none_or(|rng| rng.chance(2)) {
                renaming.apply(&mut self.names);
            }
        }
    }

    fn note(&mut self, op: &Op, effect: Effect) {
        if let Some(journal) = &mut self.journal {
            journal.push(Entry { op: op.clone(), effect });
        }
    }
}

/// What a power cut leaves of a write when nothing says otherwise: lost
/// one time in four, torn one in four, otherwise whole.
fn random_fate(rng: &mut Rng, change: &Change) -> Fate {
    match (rng.below(4), change) {
        (0, _) => Fate::Lost,
        (1, Change::SetLen(_)) => Fate::Lost,
        (1, Change::Write { offset, data }) => {
            // A run of its first sectors, or, less often, any of them.
            if rng.chance(3) {
                Fate::Sectors(rng.next())
            } else {
                Fate::Torn(rng.below(sectors(*offset, data.len()) as u64 + 1) as usize)
            }
        }
        _ => Fate::Kept,
    }
}

#[derive(Clone, Default)]
pub struct MemVfs {
    state: Arc<Mutex<MemState>>,
}

struct MemFile {
    data: FileRef,
    /// The name it was opened by, for a fault plan to read and for
    /// `fail_reads` to know it by.
    path: PathBuf,
    vfs: MemVfs,
}

fn crashed() -> io::Error {
    io::Error::other("simulated crash")
}

fn sync_failed(op: &Op) -> io::Error {
    io::Error::other(format!("simulated fault at operation {}: the sync failed", op.index))
}

fn read_failed() -> io::Error {
    io::Error::other("simulated fault: the read failed")
}

impl MemVfs {
    pub fn new() -> Self {
        Self::default()
    }

    /// The disk as a power cut right now could leave it. Synced data is
    /// kept. Of the unsynced writes, each survives or not on its own, and
    /// one that survives may be torn: only some of the sectors it touches
    /// are written, most often a run of the first. A file whose creation
    /// was never synced may be missing, and one whose removal was never
    /// synced may be back; a renaming leaves the old name or the new.
    pub fn crash_image(&self, seed: u64) -> MemVfs {
        let state = self.state.lock().unwrap();
        Self::random_image(&state, seed)
    }

    fn random_image(state: &MemState, seed: u64) -> MemVfs {
        let mut rng = Rng::new(seed);
        Self::image_of(state, &mut |unsynced| match unsynced.kind {
            UnsyncedKind::Write { offset, len } => random_fate(&mut rng, &Change::Write { offset, data: vec![0; len] }),
            UnsyncedKind::SetLen(len) => random_fate(&mut rng, &Change::SetLen(len)),
            UnsyncedKind::Create | UnsyncedKind::Remove | UnsyncedKind::Rename => {
                if rng.chance(2) {
                    Fate::Lost
                } else {
                    Fate::Kept
                }
            }
        })
    }

    /// The disk as a power cut right now would leave it if each unsynced
    /// change met the fate `fate` gives it. It is asked first about the
    /// creations, removals and renamings, oldest first (anything but
    /// `Lost` keeps one), then about the writes to each file that is
    /// left, oldest first.
    pub fn crash_image_with(&self, fate: &mut dyn FnMut(&Unsynced) -> Fate) -> MemVfs {
        let state = self.state.lock().unwrap();
        Self::image_of(&state, fate)
    }

    fn image_of(state: &MemState, fate: &mut dyn FnMut(&Unsynced) -> Fate) -> MemVfs {
        let mut names = state.names.clone();
        for (op, renaming) in &state.renamings {
            if fate(&Unsynced { op: *op, path: renaming.path(), kind: renaming.kind() }) != Fate::Lost {
                renaming.apply(&mut names);
            }
        }
        let mut image = MemState::default();
        for (path, file) in &names {
            let data = file.lock().unwrap();
            let mut bytes = data.durable.clone();
            for (op, change) in &data.pending {
                let kind = match change {
                    Change::Write { offset, data } => UnsyncedKind::Write { offset: *offset, len: data.len() },
                    Change::SetLen(len) => UnsyncedKind::SetLen(*len),
                };
                apply_fated(&mut bytes, change, fate(&Unsynced { op: *op, path, kind }));
            }
            let fresh = Arc::new(Mutex::new(MemData { id: image.all.len(), durable: bytes.clone(), current: bytes, pending: Vec::new() }));
            image.all.push(fresh.clone());
            image.files.insert(path.clone(), fresh.clone());
            image.names.insert(path.clone(), fresh);
        }
        MemVfs { state: Arc::new(Mutex::new(image)) }
    }

    /// How many changes no sync has made durable.
    pub fn unsynced(&self) -> usize {
        let state = self.state.lock().unwrap();
        state.renamings.len() + state.all.iter().map(|file| file.lock().unwrap().pending.len()).sum::<usize>()
    }

    /// A copy of the file system exactly as it is, unsynced changes and
    /// all, that shares nothing with this one.
    pub fn snapshot(&self) -> MemVfs {
        let state = self.state.lock().unwrap();
        let all: Vec<FileRef> = state
            .all
            .iter()
            .map(|file| {
                let data = file.lock().unwrap();
                Arc::new(Mutex::new(MemData { id: data.id, durable: data.durable.clone(), current: data.current.clone(), pending: data.pending.clone() }))
            })
            .collect();
        let copy_of = |file: &FileRef| all[file.lock().unwrap().id].clone();
        let copy = MemState {
            files: state.files.iter().map(|(path, file)| (path.clone(), copy_of(file))).collect(),
            names: state.names.iter().map(|(path, file)| (path.clone(), copy_of(file))).collect(),
            renamings: state
                .renamings
                .iter()
                .map(|(op, renaming)| {
                    let renaming = match renaming {
                        Renaming::Create(path, file) => Renaming::Create(path.clone(), copy_of(file)),
                        other => other.clone(),
                    };
                    (*op, renaming)
                })
                .collect(),
            ops: state.ops,
            all,
            ..Default::default()
        };
        MemVfs { state: Arc::new(Mutex::new(copy)) }
    }

    /// After `ops` more operations that change the disk, takes a crash
    /// image (see `take_crash_image`) and fails every operation from then
    /// on, as if the process had died there.
    pub fn crash_after(&self, ops: u64, seed: u64) {
        let mut state = self.state.lock().unwrap();
        state.crash_at = Some((state.ops + ops, seed));
    }

    pub fn take_crash_image(&self) -> Option<MemVfs> {
        self.state.lock().unwrap().image.take()
    }

    pub fn has_crashed(&self) -> bool {
        self.state.lock().unwrap().crashed
    }

    /// Operations that changed the disk so far, or tried to.
    pub fn ops(&self) -> u64 {
        self.state.lock().unwrap().ops
    }

    /// Has `plan` asked about every operation that changes the disk,
    /// before it does, from now on; `None` for no more faults. The plan
    /// is called with nothing locked, and may itself use the file
    /// system: what it does there happens before the operation it was
    /// asked about, and is not shown to it.
    pub fn set_plan(&self, plan: Option<Plan>) {
        self.state.lock().unwrap().plan = plan;
    }

    /// Starts keeping every operation that changes the disk, so that it
    /// can be made again, one at a time, on a `snapshot` taken now: see
    /// `apply`. That gives the disk as it stood before each operation,
    /// which is where a process killed there would have left it.
    pub fn start_journal(&self) {
        self.state.lock().unwrap().journal = Some(Vec::new());
    }

    /// How many operations the journal holds.
    pub fn journal_len(&self) -> usize {
        self.state.lock().unwrap().journal.as_ref().map_or(0, Vec::len)
    }

    /// Stops the journal and returns it.
    pub fn take_journal(&self) -> Vec<Entry> {
        self.state.lock().unwrap().journal.take().unwrap_or_default()
    }

    /// Makes a journaled operation again. This must be a copy of the
    /// file system the journal was kept on, as it stood before that
    /// operation: a `snapshot` from when the journal began, with every
    /// earlier entry applied in order.
    pub fn apply(&self, entry: &Entry) {
        let mut state = self.state.lock().unwrap();
        let (op, path) = (entry.op.index, &entry.op.path);
        state.ops = state.ops.max(op + 1);
        match &entry.effect {
            Effect::Create { file } => {
                assert_eq!(*file, state.all.len(), "a journal applied out of order");
                state.create(op, path);
            }
            Effect::Write { file, offset, data } => {
                let mut file = state.all[*file].lock().unwrap();
                let change = Change::Write { offset: *offset, data: data.clone() };
                apply(&mut file.current, &change);
                file.pending.push((op, change));
            }
            Effect::SetLen { file, len } => {
                let mut file = state.all[*file].lock().unwrap();
                apply(&mut file.current, &Change::SetLen(*len));
                file.pending.push((op, Change::SetLen(*len)));
            }
            Effect::Sync { file, lost } => MemState::sync(&state.all[*file], *lost),
            Effect::SyncDir { lost } => state.sync_dir(path, *lost),
            Effect::Remove => state.remove(op, path).expect("a journal applied out of order"),
            Effect::Rename { to } => state.rename(op, path, to).expect("a journal applied out of order"),
        }
    }

    /// Flips one bit of a file's durable content, for corruption tests.
    pub fn corrupt(&self, path: &Path, offset: u64) {
        let state = self.state.lock().unwrap();
        let mut data = state.files[path].lock().unwrap();
        data.current[offset as usize] ^= 0x40;
        data.durable = data.current.clone();
        data.pending.clear();
    }

    pub fn file_len(&self, path: &Path) -> u64 {
        let state = self.state.lock().unwrap();
        let len = state.files[path].lock().unwrap().current.len();
        len as u64
    }

    /// Makes every read of the file opened as `path` fail, until this is
    /// called again with another path or with `None`. One file fails at
    /// a time. The file is not touched: it still exists, has its length
    /// and takes writes, and reads what it held once the fault is
    /// cleared. A copy of the file system does not have the fault.
    pub fn fail_reads(&self, path: Option<&Path>) {
        self.state.lock().unwrap().fail_reads = path.map(|path| (path.to_path_buf(), 0..u64::MAX));
    }

    /// As `fail_reads`, for the reads that touch any of `bytes` only: a
    /// part of the disk that cannot be read, in a file that otherwise can.
    pub fn fail_reads_within(&self, path: &Path, bytes: Range<u64>) {
        self.state.lock().unwrap().fail_reads = Some((path.to_path_buf(), bytes));
    }

    /// Called before each read of `len` bytes at `offset` of the file
    /// opened as `path`.
    fn check_read(&self, path: &Path, offset: u64, len: usize) -> io::Result<()> {
        let state = self.state.lock().unwrap();
        if state.crashed {
            return Err(crashed());
        }
        if let Some((failing, bytes)) = &state.fail_reads {
            if failing == path && offset < bytes.end && offset.saturating_add(len as u64) > bytes.start {
                return Err(read_failed());
            }
        }
        Ok(())
    }

    /// Called before each operation that changes the disk: counts it,
    /// and asks the plan, if there is one, what goes wrong with it.
    fn begin(&self, kind: OpKind, path: &Path) -> io::Result<(Op, Fault)> {
        let (op, plan) = {
            let mut state = self.state.lock().unwrap();
            if state.crashed {
                return Err(crashed());
            }
            if let Some((at, seed)) = state.crash_at {
                if state.ops >= at {
                    state.crashed = true;
                    state.image = Some(Self::random_image(&state, seed));
                    return Err(crashed());
                }
            }
            let op = Op { index: state.ops, kind, path: path.to_path_buf() };
            state.ops += 1;
            (op, state.plan.take())
        };
        let Some(mut plan) = plan else { return Ok((op, Fault::None)) };
        let fault = plan(&op);
        // Asked again next time, unless another plan was set meanwhile.
        self.state.lock().unwrap().plan.get_or_insert(plan);
        match fault {
            Fault::Fail(kind) => Err(io::Error::new(kind, format!("simulated fault at operation {}", op.index))),
            fault => Ok((op, fault)),
        }
    }
}

impl File for MemFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.vfs.check_read(&self.path, offset, buf.len())?;
        let data = self.data.lock().unwrap();
        let start = (offset as usize).min(data.current.len());
        let n = buf.len().min(data.current.len() - start);
        buf[..n].copy_from_slice(&data.current[start..start + n]);
        Ok(n)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        let grows = (offset + buf.len() as u64).saturating_sub(self.data.lock().unwrap().current.len() as u64);
        let (op, fault) = self.vfs.begin(OpKind::Write { offset, len: buf.len(), grows }, &self.path)?;
        let (written, full) = match fault {
            Fault::Short(n) => (&buf[..n.min(buf.len())], true),
            _ => (buf, false),
        };
        if !(full && written.is_empty()) {
            let mut state = self.vfs.state.lock().unwrap();
            let mut data = self.data.lock().unwrap();
            let change = Change::Write { offset, data: written.to_vec() };
            apply(&mut data.current, &change);
            data.pending.push((op.index, change));
            state.note(&op, Effect::Write { file: data.id, offset, data: written.to_vec() });
        }
        if full {
            return Err(io::Error::new(io::ErrorKind::StorageFull, format!("simulated fault at operation {}: the disk is full", op.index)));
        }
        Ok(())
    }
    fn sync(&self) -> io::Result<()> {
        let (op, fault) = self.vfs.begin(OpKind::Sync, &self.path)?;
        let lost = match fault {
            Fault::Lie => return Ok(()),
            Fault::SyncLoses(seed) => Some(seed),
            _ => None,
        };
        let mut state = self.vfs.state.lock().unwrap();
        MemState::sync(&self.data, lost);
        let file = self.data.lock().unwrap().id;
        state.note(&op, Effect::Sync { file, lost });
        match lost {
            Some(_) => Err(sync_failed(&op)),
            None => Ok(()),
        }
    }
    fn len(&self) -> io::Result<u64> {
        Ok(self.data.lock().unwrap().current.len() as u64)
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        let (op, _) = self.vfs.begin(OpKind::SetLen { len }, &self.path)?;
        let mut state = self.vfs.state.lock().unwrap();
        let mut data = self.data.lock().unwrap();
        apply(&mut data.current, &Change::SetLen(len));
        data.pending.push((op.index, Change::SetLen(len)));
        state.note(&op, Effect::SetLen { file: data.id, len });
        Ok(())
    }
}

impl Vfs for MemVfs {
    fn open(&self, path: &Path, create: bool) -> io::Result<Arc<dyn File>> {
        let existing = self.state.lock().unwrap().files.get(path).cloned();
        let data = match existing {
            Some(data) => data,
            None if create => {
                let (op, _) = self.begin(OpKind::Create, path)?;
                let mut state = self.state.lock().unwrap();
                let data = state.create(op.index, path);
                let file = data.lock().unwrap().id;
                state.note(&op, Effect::Create { file });
                data
            }
            None => return Err(io::Error::new(io::ErrorKind::NotFound, "no such file")),
        };
        Ok(Arc::new(MemFile { data, path: path.to_path_buf(), vfs: self.clone() }))
    }
    fn remove(&self, path: &Path) -> io::Result<()> {
        let (op, _) = self.begin(OpKind::Remove, path)?;
        let mut state = self.state.lock().unwrap();
        state.remove(op.index, path)?;
        state.note(&op, Effect::Remove);
        Ok(())
    }
    fn exists(&self, path: &Path) -> bool {
        self.state.lock().unwrap().files.contains_key(path)
    }
    fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
        let state = self.state.lock().unwrap();
        let names = state.files.keys().filter(|p| p.parent() == Some(dir));
        Ok(names.filter_map(|p| p.file_name()?.to_str().map(String::from)).collect())
    }
    fn list_dirs(&self, dir: &Path) -> io::Result<Vec<String>> {
        // A directory is there for as long as it holds a file.
        let state = self.state.lock().unwrap();
        let mut names: Vec<String> = state.files.keys().filter(|p| p.parent().and_then(Path::parent) == Some(dir)).filter_map(|p| p.parent()?.file_name()?.to_str().map(String::from)).collect();
        names.dedup();
        Ok(names)
    }
    fn create_dir_all(&self, _dir: &Path) -> io::Result<()> {
        Ok(())
    }
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        let (op, fault) = self.begin(OpKind::SyncDir, dir)?;
        let lost = match fault {
            Fault::Lie => return Ok(()),
            Fault::SyncLoses(seed) => Some(seed),
            _ => None,
        };
        let mut state = self.state.lock().unwrap();
        state.sync_dir(dir, lost);
        state.note(&op, Effect::SyncDir { lost });
        match lost {
            Some(_) => Err(sync_failed(&op)),
            None => Ok(()),
        }
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let (op, _) = self.begin(OpKind::Rename { to: to.to_path_buf() }, from)?;
        let mut state = self.state.lock().unwrap();
        state.rename(op.index, from, to)?;
        state.note(&op, Effect::Rename { to: to.to_path_buf() });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synced_writes_survive_a_crash_and_unsynced_ones_may_not() {
        let vfs = MemVfs::new();
        let path = Path::new("/d/f");
        let file = vfs.open(path, true).unwrap();
        file.write_at(&[1u8; 2048], 0).unwrap();
        file.sync().unwrap();
        vfs.sync_dir(Path::new("/d")).unwrap();
        file.write_at(&[2u8; 2048], 2048).unwrap();

        let mut kept_all = false;
        let mut lost_some = false;
        for seed in 0..64 {
            let image = vfs.crash_image(seed);
            let f = image.open(path, false).unwrap();
            let mut buf = vec![0u8; 4096];
            let n = f.read_at(&mut buf, 0).unwrap();
            assert!(n >= 2048, "synced bytes are never lost");
            assert_eq!(&buf[..2048], &[1u8; 2048][..]);
            if n == 4096 && buf[2048..] == [2u8; 2048] {
                kept_all = true;
            } else {
                lost_some = true;
            }
        }
        assert!(kept_all && lost_some);
    }

    #[test]
    fn a_file_whose_creation_was_not_synced_may_vanish() {
        let vfs = MemVfs::new();
        vfs.open(Path::new("/d/kept"), true).unwrap();
        vfs.sync_dir(Path::new("/d")).unwrap();
        vfs.open(Path::new("/d/new"), true).unwrap();
        let mut seen = [false; 2];
        for seed in 0..32 {
            let image = vfs.crash_image(seed);
            assert!(image.exists(Path::new("/d/kept")));
            seen[image.exists(Path::new("/d/new")) as usize] = true;
        }
        assert_eq!(seen, [true, true]);
    }

    #[test]
    fn crash_after_fails_every_later_operation() {
        let vfs = MemVfs::new();
        let file = vfs.open(Path::new("/d/f"), true).unwrap();
        vfs.crash_after(2, 7);
        file.write_at(b"a", 0).unwrap();
        file.write_at(b"b", 1).unwrap();
        assert!(file.write_at(b"c", 2).is_err());
        assert!(file.sync().is_err());
        assert!(vfs.take_crash_image().is_some());
    }

    fn contents(vfs: &MemVfs, path: &Path) -> Vec<u8> {
        let file = vfs.open(path, false).unwrap();
        let mut bytes = vec![0u8; file.len().unwrap() as usize];
        read_exact_at(&*file, &mut bytes, 0).unwrap();
        bytes
    }

    #[test]
    fn a_torn_write_keeps_whole_sectors_of_the_disk() {
        let vfs = MemVfs::new();
        let path = Path::new("/d/f");
        let file = vfs.open(path, true).unwrap();
        file.write_at(&[1u8; 2048], 0).unwrap();
        file.sync().unwrap();
        vfs.sync_dir(Path::new("/d")).unwrap();
        // From the middle of the first sector to the middle of the fourth.
        file.write_at(&[2u8; 1536], 256).unwrap();
        assert_eq!(sectors(256, 1536), 4);
        let image = |fate: Fate| contents(&vfs.crash_image_with(&mut |_| fate), path);
        // Ones, with twos from each of these places to the next.
        let mixed = |twos: &[(usize, usize)]| {
            let mut want = vec![1u8; 2048];
            twos.iter().for_each(|(from, to)| want[*from..*to].fill(2));
            want
        };
        assert_eq!(image(Fate::Lost), mixed(&[]));
        assert_eq!(image(Fate::Kept), mixed(&[(256, 1792)]));
        assert_eq!(image(Fate::Torn(1)), mixed(&[(256, 512)]));
        assert_eq!(image(Fate::Torn(3)), mixed(&[(256, 1536)]));
        assert_eq!(image(Fate::Sectors(0b1010)), mixed(&[(512, 1024), (1536, 1792)]));
        // The file itself is as it was written.
        assert_eq!(contents(&vfs, path), mixed(&[(256, 1792)]));
    }

    #[test]
    fn a_renaming_leaves_one_name_or_the_other() {
        let vfs = MemVfs::new();
        let dir = Path::new("/d");
        for (name, byte) in [("old", 1u8), ("new", 2u8)] {
            let file = vfs.open(&dir.join(name), true).unwrap();
            file.write_at(&[byte; 8], 0).unwrap();
            file.sync().unwrap();
        }
        vfs.sync_dir(dir).unwrap();
        vfs.rename(&dir.join("new"), &dir.join("old")).unwrap();
        let mut seen = [false; 2];
        for seed in 0..32 {
            let image = vfs.crash_image(seed);
            let renamed = !image.exists(&dir.join("new"));
            seen[renamed as usize] = true;
            assert_eq!(contents(&image, &dir.join("old")), [if renamed { 2u8 } else { 1 }; 8], "the name is never without a file");
        }
        assert_eq!(seen, [true, true]);
        vfs.sync_dir(dir).unwrap();
        let image = vfs.crash_image(1);
        assert!(!image.exists(&dir.join("new")));
        assert_eq!(contents(&image, &dir.join("old")), [2u8; 8]);
    }

    #[test]
    fn a_plan_fails_fills_and_lies_by_operation() {
        let vfs = MemVfs::new();
        let path = Path::new("/d/f");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let ops = seen.clone();
        vfs.set_plan(Some(Box::new(move |op| {
            ops.lock().unwrap().push(op.clone());
            match op.index {
                2 => Fault::Fail(io::ErrorKind::PermissionDenied),
                3 => Fault::Short(100),
                4 => Fault::Lie,
                _ => Fault::None,
            }
        })));
        let file = vfs.open(path, true).unwrap();
        file.write_at(&[1u8; 600], 0).unwrap();
        assert_eq!(file.write_at(&[2u8; 600], 600).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(file.len().unwrap(), 600, "a failed write changes nothing");
        assert_eq!(file.write_at(&[3u8; 600], 600).unwrap_err().kind(), io::ErrorKind::StorageFull);
        assert_eq!(file.len().unwrap(), 700, "a full disk takes what it has room for");
        file.sync().unwrap();
        vfs.sync_dir(Path::new("/d")).unwrap();
        // The sync lied: a power cut may still take everything.
        assert_eq!(contents(&vfs.crash_image_with(&mut |_| Fate::Lost), path), [0u8; 0]);
        file.sync().unwrap();
        assert_eq!(contents(&vfs.crash_image_with(&mut |_| Fate::Lost), path).len(), 700);
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0], Op { index: 0, kind: OpKind::Create, path: path.to_path_buf() });
        assert_eq!(seen[3].kind, OpKind::Write { offset: 600, len: 600, grows: 600 });
        assert_eq!(seen[5], Op { index: 5, kind: OpKind::SyncDir, path: PathBuf::from("/d") });
        // No more faults, and no more questions.
        vfs.set_plan(None);
        file.write_at(&[4u8; 10], 0).unwrap();
        assert_eq!(seen.len(), 7);
    }

    #[test]
    fn what_a_failed_sync_lost_no_later_sync_brings_back() {
        let path = Path::new("/d/f");
        let mut outcomes = std::collections::BTreeSet::new();
        for seed in 0..32 {
            let vfs = MemVfs::new();
            let file = vfs.open(path, true).unwrap();
            vfs.sync_dir(Path::new("/d")).unwrap();
            file.write_at(&[1u8; 512], 0).unwrap();
            file.write_at(&[2u8; 512], 512).unwrap();
            vfs.set_plan(Some(Box::new(move |op| if op.kind == OpKind::Sync { Fault::SyncLoses(seed) } else { Fault::None })));
            assert!(file.sync().is_err());
            vfs.set_plan(None);
            // The process still reads what it wrote.
            assert_eq!(contents(&vfs, path).len(), 1024);
            // What the disk holds was settled by the failed sync: nothing
            // is unsynced now, and a sync that succeeds changes nothing.
            assert_eq!(vfs.unsynced(), 0);
            let before = contents(&vfs.crash_image(1), path);
            file.write_at(&[3u8; 512], 2048).unwrap();
            file.sync().unwrap();
            let after = contents(&vfs.crash_image(2), path);
            assert_eq!(after.len(), 2560);
            assert_eq!(after[2048..], [3u8; 512]);
            let lost = |bytes: &[u8], at: usize, byte: u8| bytes.get(at..at + 512) != Some(&[byte; 512][..]);
            assert_eq!((lost(&before, 0, 1), lost(&before, 512, 2)), (lost(&after, 0, 1), lost(&after, 512, 2)));
            outcomes.insert((lost(&after, 0, 1), lost(&after, 512, 2)));
        }
        assert!(outcomes.len() >= 3, "a failed sync loses some writes and not others: {outcomes:?}");
    }

    #[test]
    fn a_journal_rebuilds_the_disk_before_each_operation() {
        let vfs = MemVfs::new();
        let dir = Path::new("/d");
        // Something from before the journal: the copy starts from it.
        let old = vfs.open(&dir.join("old"), true).unwrap();
        old.write_at(&[9u8; 700], 0).unwrap();
        let base = vfs.snapshot();
        vfs.start_journal();
        let mut images = vec![vfs.crash_image(3)];
        let mut step = |vfs: &MemVfs| images.push(vfs.crash_image(3));
        let file = vfs.open(&dir.join("f"), true).unwrap();
        step(&vfs);
        file.write_at(&[1u8; 3000], 100).unwrap();
        step(&vfs);
        old.sync().unwrap();
        step(&vfs);
        vfs.sync_dir(dir).unwrap();
        step(&vfs);
        file.set_len(1000).unwrap();
        step(&vfs);
        vfs.rename(&dir.join("f"), &dir.join("g")).unwrap();
        step(&vfs);
        file.write_at(&[2u8; 2000], 500).unwrap();
        step(&vfs);
        vfs.remove(&dir.join("old")).unwrap();
        step(&vfs);
        file.sync().unwrap();
        step(&vfs);
        assert_eq!(vfs.journal_len(), 9);
        let journal = vfs.take_journal();
        assert_eq!(journal.len(), 9);
        assert!(journal.iter().enumerate().all(|(i, entry)| entry.op.index == i as u64 + 2));

        let all = |vfs: &MemVfs| -> Vec<(String, Vec<u8>)> { vfs.list(dir).unwrap().into_iter().map(|name| (name.clone(), contents(vfs, &dir.join(name)))).collect() };
        let copy = base.snapshot();
        for (n, image) in images.iter().enumerate() {
            // The same power cut on the copy leaves the same disk.
            assert_eq!(all(&copy.crash_image(3)), all(image), "before operation {n}");
            if let Some(entry) = journal.get(n) {
                copy.apply(entry);
            }
        }
        assert_eq!(all(&copy), all(&vfs));
        // The copy the journal started from is untouched.
        assert_eq!(all(&base), [("old".to_string(), vec![9u8; 700])]);
    }

    #[test]
    fn reads_of_one_file_can_be_made_to_fail_and_to_work_again() {
        let vfs = MemVfs::new();
        let (bad, good) = (Path::new("/d/bad"), Path::new("/d/good"));
        for path in [bad, good] {
            vfs.open(path, true).unwrap().write_at(b"bytes", 0).unwrap();
        }
        let before = vfs.open(bad, false).unwrap();
        vfs.fail_reads(Some(bad));
        // A handle from before the fault and one from after it both fail,
        // and not as a file that is too short does.
        let mut buf = [0u8; 5];
        for file in [&before, &vfs.open(bad, false).unwrap()] {
            let error = read_exact_at(&**file, &mut buf, 0).unwrap_err();
            assert_ne!(error.kind(), io::ErrorKind::UnexpectedEof);
        }
        // The file is otherwise as it was, other files are read, and so
        // is a copy of this one.
        assert!(vfs.exists(bad));
        assert_eq!(before.len().unwrap(), 5);
        for readable in [vfs.open(good, false).unwrap(), vfs.snapshot().open(bad, false).unwrap()] {
            read_exact_at(&*readable, &mut buf, 0).unwrap();
            assert_eq!(&buf, b"bytes");
        }
        // Only a part of it: the reads that touch the part fail.
        vfs.fail_reads_within(bad, 2..3);
        let mut two = [0u8; 2];
        read_exact_at(&*before, &mut two, 0).unwrap();
        assert_eq!(&two, b"by");
        assert!(read_exact_at(&*before, &mut two, 1).is_err());
        assert!(read_exact_at(&*before, &mut two, 2).is_err());
        read_exact_at(&*before, &mut two, 3).unwrap();
        assert_eq!(&two, b"es");
        assert!(read_exact_at(&*before, &mut buf, 0).is_err());
        // Cleared, the bytes are what they were.
        vfs.fail_reads(None);
        read_exact_at(&*before, &mut buf, 0).unwrap();
        assert_eq!(&buf, b"bytes");
    }
}
