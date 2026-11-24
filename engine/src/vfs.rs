//! The file system as the engine sees it: positioned reads and writes, a
//! sync that makes them durable, and directory operations. `DiskVfs` is the
//! real one. `MemVfs` keeps files in memory and remembers which writes were
//! never synced, so a test can ask for the state a power cut would leave
//! behind, at any point, and open the database on it. It can also refuse
//! to read one file, or a part of one, as a disk does that has the bytes
//! and cannot give them just now.

use std::collections::{BTreeMap, BTreeSet};
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

enum Pending {
    Write { offset: u64, data: Vec<u8> },
    SetLen(u64),
}

#[derive(Default)]
struct MemData {
    /// What a crash is guaranteed to keep.
    durable: Vec<u8>,
    /// What reads see: durable plus every write since.
    current: Vec<u8>,
    /// Writes since the last sync, oldest first.
    pending: Vec<Pending>,
}

fn apply(bytes: &mut Vec<u8>, op: &Pending) {
    match op {
        Pending::Write { offset, data } => {
            let end = *offset as usize + data.len();
            if bytes.len() < end {
                bytes.resize(end, 0);
            }
            bytes[*offset as usize..end].copy_from_slice(data);
        }
        Pending::SetLen(len) => bytes.resize(*len as usize, 0),
    }
}

#[derive(Default)]
struct MemState {
    files: BTreeMap<PathBuf, Arc<Mutex<MemData>>>,
    /// Names whose directory entry has been synced.
    durable_names: BTreeSet<PathBuf>,
    /// Names removed since their directory was last synced, with what the
    /// file held: a crash may bring them back.
    removed: BTreeMap<PathBuf, Arc<Mutex<MemData>>>,
    /// Counts operations that change the disk, for `crash_after`.
    ops: u64,
    crash_at: Option<(u64, u64)>,
    crashed: bool,
    image: Option<MemVfs>,
    /// The file whose reads fail, and which bytes of it: see `fail_reads`.
    fail_reads: Option<(PathBuf, Range<u64>)>,
}

#[derive(Clone, Default)]
pub struct MemVfs {
    state: Arc<Mutex<MemState>>,
}

struct MemFile {
    data: Arc<Mutex<MemData>>,
    /// The name the file was opened by.
    path: PathBuf,
    vfs: MemVfs,
}

fn crashed() -> io::Error {
    io::Error::other("simulated crash")
}

fn read_failed() -> io::Error {
    io::Error::other("simulated read failure")
}

impl MemVfs {
    pub fn new() -> Self {
        Self::default()
    }

    /// The disk as a power cut right now could leave it. Synced data is
    /// kept. Of the unsynced writes, each survives or not on its own, and
    /// one that survives may be cut short at a 512-byte boundary. A file
    /// whose creation was never synced may be missing, and one whose
    /// removal was never synced may be back.
    pub fn crash_image(&self, seed: u64) -> MemVfs {
        let state = self.state.lock().unwrap();
        Self::image_of(&state, seed)
    }

    fn image_of(state: &MemState, seed: u64) -> MemVfs {
        let mut rng = Rng::new(seed);
        let mut files = BTreeMap::new();
        let mut names = BTreeSet::new();
        let survivors = state.files.iter().map(|(p, d)| (p, d, state.durable_names.contains(p)));
        let ghosts = state.removed.iter().map(|(p, d)| (p, d, false));
        for (path, data, name_durable) in survivors.chain(ghosts) {
            if files.contains_key(path) || (!name_durable && rng.chance(2)) {
                continue;
            }
            let data = data.lock().unwrap();
            let mut bytes = data.durable.clone();
            for op in &data.pending {
                match rng.below(4) {
                    0 => {}
                    1 => {
                        if let Pending::Write { offset, data } = op {
                            let sectors = data.len() / 512;
                            let keep = if sectors == 0 { 0 } else { rng.below(sectors as u64 + 1) as usize * 512 };
                            apply(&mut bytes, &Pending::Write { offset: *offset, data: data[..keep].to_vec() });
                        }
                    }
                    _ => apply(&mut bytes, op),
                }
            }
            let fresh = MemData { durable: bytes.clone(), current: bytes, pending: Vec::new() };
            files.insert(path.clone(), Arc::new(Mutex::new(fresh)));
            names.insert(path.clone());
        }
        let state = MemState { files, durable_names: names, ..Default::default() };
        MemVfs { state: Arc::new(Mutex::new(state)) }
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

    /// Operations that changed the disk so far.
    pub fn ops(&self) -> u64 {
        self.state.lock().unwrap().ops
    }

    /// Flips one bit of a file's durable content, for corruption tests.
    pub fn corrupt(&self, path: &Path, offset: u64) {
        let state = self.state.lock().unwrap();
        let mut data = state.files[path].lock().unwrap();
        data.current[offset as usize] ^= 0x40;
        data.durable = data.current.clone();
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
    /// cleared.
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

    /// Called before each operation that changes the disk.
    fn step(&self) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.crashed {
            return Err(crashed());
        }
        if let Some((at, seed)) = state.crash_at {
            if state.ops >= at {
                state.crashed = true;
                state.image = Some(Self::image_of(&state, seed));
                return Err(crashed());
            }
        }
        state.ops += 1;
        Ok(())
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
        self.vfs.step()?;
        let mut data = self.data.lock().unwrap();
        let op = Pending::Write { offset, data: buf.to_vec() };
        apply(&mut data.current, &op);
        data.pending.push(op);
        Ok(())
    }
    fn sync(&self) -> io::Result<()> {
        self.vfs.step()?;
        let mut data = self.data.lock().unwrap();
        data.durable = data.current.clone();
        data.pending.clear();
        Ok(())
    }
    fn len(&self) -> io::Result<u64> {
        Ok(self.data.lock().unwrap().current.len() as u64)
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.vfs.step()?;
        let mut data = self.data.lock().unwrap();
        let op = Pending::SetLen(len);
        apply(&mut data.current, &op);
        data.pending.push(op);
        Ok(())
    }
}

impl Vfs for MemVfs {
    fn open(&self, path: &Path, create: bool) -> io::Result<Arc<dyn File>> {
        let existing = self.state.lock().unwrap().files.get(path).cloned();
        let data = match existing {
            Some(data) => data,
            None if create => {
                self.step()?;
                let data = Arc::new(Mutex::new(MemData::default()));
                self.state.lock().unwrap().files.insert(path.to_path_buf(), data.clone());
                data
            }
            None => return Err(io::Error::new(io::ErrorKind::NotFound, "no such file")),
        };
        Ok(Arc::new(MemFile { data, path: path.to_path_buf(), vfs: self.clone() }))
    }
    fn remove(&self, path: &Path) -> io::Result<()> {
        self.step()?;
        let mut state = self.state.lock().unwrap();
        match state.files.remove(path) {
            Some(data) => {
                if state.durable_names.remove(path) {
                    state.removed.insert(path.to_path_buf(), data);
                }
                Ok(())
            }
            None => Err(io::Error::new(io::ErrorKind::NotFound, "no such file")),
        }
    }
    fn exists(&self, path: &Path) -> bool {
        self.state.lock().unwrap().files.contains_key(path)
    }
    fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
        let state = self.state.lock().unwrap();
        let names = state.files.keys().filter(|p| p.parent() == Some(dir));
        Ok(names.filter_map(|p| p.file_name()?.to_str().map(String::from)).collect())
    }
    fn create_dir_all(&self, _dir: &Path) -> io::Result<()> {
        Ok(())
    }
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.step()?;
        let mut state = self.state.lock().unwrap();
        let names: Vec<PathBuf> = state.files.keys().filter(|p| p.parent() == Some(dir)).cloned().collect();
        state.durable_names.extend(names);
        state.removed.retain(|p, _| p.parent() != Some(dir));
        Ok(())
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.step()?;
        let mut state = self.state.lock().unwrap();
        let data = state.files.remove(from).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such file"))?;
        // A crash before the directory is synced may leave either name.
        if state.durable_names.remove(from) {
            state.removed.insert(from.to_path_buf(), data.clone());
        }
        if let Some(replaced) = state.files.insert(to.to_path_buf(), data) {
            if state.durable_names.remove(to) {
                state.removed.insert(to.to_path_buf(), replaced);
            }
        }
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
        // The file is otherwise as it was, and other files are read.
        assert!(vfs.exists(bad));
        assert_eq!(before.len().unwrap(), 5);
        read_exact_at(&*vfs.open(good, false).unwrap(), &mut buf, 0).unwrap();
        assert_eq!(&buf, b"bytes");
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
