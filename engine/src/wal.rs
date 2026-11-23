//! The write-ahead log: what makes a commit durable.
//!
//! A transaction's changes are appended here as one record and synced
//! before the commit is acknowledged; the data file catches up later, at a
//! checkpoint. After a crash the data file is as of its last checkpoint
//! and the records after it are applied again.
//!
//! The log is a sequence of files, one per checkpoint interval, named by a
//! generation number. A checkpoint starts a new file, and once it is on
//! disk the older files are deleted.
//!
//! ```text
//! file    header (36 bytes): magic, generation, the data file's uuid, CRC
//! record  length (u32), CRC-32 (u32), bytes
//! ```
//!
//! A record's CRC covers the generation and the record's offset as well as
//! its bytes, so nothing left over from another file or another place can
//! pass for a record. Reading stops at the first record that does not
//! check: that is the torn tail of a crash, and nothing after it was ever
//! acknowledged.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use crate::error::{Error, Result};
use crate::vfs::{read_exact_at, File, Vfs};

const MAGIC: &[u8; 8] = b"dbwal001";
const HEADER_LEN: u64 = 36;
/// No record is longer than this. A length beyond it is damage.
const MAX_RECORD: u32 = 1 << 30;

pub fn wal_path(dir: &Path, name: &str, generation: u64) -> PathBuf {
    dir.join(format!("{name}.wal-{generation:010}"))
}

/// The generation in a log file's name, if `file` is one of `name`'s.
pub fn wal_generation(name: &str, file: &str) -> Option<u64> {
    file.strip_prefix(name)?.strip_prefix(".wal-")?.parse().ok()
}

fn header(generation: u64, uuid: &[u8; 16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN as usize);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&generation.to_le_bytes());
    out.extend_from_slice(uuid);
    let crc = crc32fast::hash(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

fn record_crc(generation: u64, offset: u64, bytes: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&generation.to_le_bytes());
    hasher.update(&offset.to_le_bytes());
    hasher.update(bytes);
    hasher.finalize()
}

struct Tail {
    file: Arc<dyn File>,
    generation: u64,
    offset: u64,
    /// The newest version whose record is fully appended.
    appended: u64,
    /// The newest version that has a record at all.
    last_record: u64,
}

struct Synced {
    /// Every version up to this one is on disk.
    version: u64,
    syncing: bool,
    failed: bool,
}

pub struct Wal {
    vfs: Arc<dyn Vfs>,
    dir: PathBuf,
    name: String,
    uuid: [u8; 16],
    tail: Mutex<Tail>,
    synced: Mutex<Synced>,
    wake: Condvar,
    /// When false, `sync_to` returns at once: commits are then only as
    /// durable as the operating system makes them.
    pub sync: bool,
}

fn failed() -> Error {
    Error::Io(std::io::Error::other("an earlier write to the log failed"))
}

impl Wal {
    /// Starts a new log file of the given generation, replacing any file
    /// of that name. `version` is the newest committed version so far.
    pub fn create(vfs: Arc<dyn Vfs>, dir: &Path, name: &str, uuid: [u8; 16], generation: u64, version: u64, sync: bool) -> Result<Wal> {
        let file = Self::start_file(&*vfs, dir, name, &uuid, generation)?;
        Ok(Wal {
            vfs,
            dir: dir.to_path_buf(),
            name: name.to_string(),
            uuid,
            tail: Mutex::new(Tail { file, generation, offset: HEADER_LEN, appended: version, last_record: version }),
            synced: Mutex::new(Synced { version, syncing: false, failed: false }),
            wake: Condvar::new(),
            sync,
        })
    }

    fn start_file(vfs: &dyn Vfs, dir: &Path, name: &str, uuid: &[u8; 16], generation: u64) -> Result<Arc<dyn File>> {
        let path = wal_path(dir, name, generation);
        let file = vfs.open(&path, true)?;
        file.set_len(0)?;
        file.write_at(&header(generation, uuid), 0)?;
        // The file must exist, under its name, before any commit relies
        // on it.
        file.sync()?;
        vfs.sync_dir(dir)?;
        Ok(file)
    }

    pub fn generation(&self) -> u64 {
        self.tail.lock().unwrap().generation
    }

    /// Bytes in the current file.
    pub fn size(&self) -> u64 {
        self.tail.lock().unwrap().offset
    }

    /// Appends the record that commits `version`. The caller holds the
    /// database's write lock, so appends never interleave.
    pub fn append(&self, version: u64, bytes: &[u8]) -> Result<()> {
        if self.synced.lock().unwrap().failed {
            return Err(failed());
        }
        let mut tail = self.tail.lock().unwrap();
        let mut frame = Vec::with_capacity(bytes.len() + 8);
        frame.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        frame.extend_from_slice(&record_crc(tail.generation, tail.offset, bytes).to_le_bytes());
        frame.extend_from_slice(bytes);
        if let Err(e) = tail.file.write_at(&frame, tail.offset) {
            self.synced.lock().unwrap().failed = true;
            return Err(e.into());
        }
        tail.offset += frame.len() as u64;
        tail.appended = version;
        tail.last_record = version;
        Ok(())
    }

    /// Notes that `version` committed without a record of its own: it
    /// changed only what need not survive a crash. If every record before
    /// it is on disk already, so is it, without a sync.
    pub fn skip(&self, version: u64) {
        let mut tail = self.tail.lock().unwrap();
        tail.appended = version;
        let mut synced = self.synced.lock().unwrap();
        if synced.version >= tail.last_record {
            synced.version = version;
        }
    }

    /// Returns once `version` is on disk. Callers arriving together share
    /// one sync: the first does it for everything appended so far, the
    /// rest wait for it. Returns the newest version now durable.
    pub fn sync_to(&self, version: u64) -> Result<u64> {
        let mut synced = self.synced.lock().unwrap();
        loop {
            if synced.failed {
                return Err(failed());
            }
            if synced.version >= version {
                return Ok(synced.version);
            }
            if synced.syncing {
                synced = self.wake.wait(synced).unwrap();
                continue;
            }
            synced.syncing = true;
            drop(synced);
            let (file, appended) = {
                let tail = self.tail.lock().unwrap();
                (tail.file.clone(), tail.appended)
            };
            let result = if self.sync { file.sync() } else { Ok(()) };
            synced = self.synced.lock().unwrap();
            synced.syncing = false;
            match result {
                Ok(()) => synced.version = synced.version.max(appended),
                Err(_) => synced.failed = true,
            }
            self.wake.notify_all();
        }
    }

    /// The newest version known to be on disk.
    pub fn synced_version(&self) -> u64 {
        self.synced.lock().unwrap().version
    }

    /// Starts the next file. Everything appended so far is synced first,
    /// so a checkpoint that later deletes the old file loses nothing.
    /// Called with the write lock held. Returns the new generation.
    pub fn rotate(&self) -> Result<u64> {
        let mut tail = self.tail.lock().unwrap();
        let result = (|| {
            tail.file.sync()?;
            let generation = tail.generation + 1;
            let file = Self::start_file(&*self.vfs, &self.dir, &self.name, &self.uuid, generation)?;
            Ok::<_, Error>((generation, file))
        })();
        let mut synced = self.synced.lock().unwrap();
        match result {
            Ok((generation, file)) => {
                synced.version = synced.version.max(tail.appended);
                self.wake.notify_all();
                tail.file = file;
                tail.generation = generation;
                tail.offset = HEADER_LEN;
                Ok(generation)
            }
            Err(e) => {
                synced.failed = true;
                self.wake.notify_all();
                Err(e)
            }
        }
    }

    /// Deletes the log files older than `generation`, once a checkpoint
    /// has made them unnecessary.
    pub fn remove_before(&self, generation: u64) -> Result<()> {
        remove_logs(&*self.vfs, &self.dir, &self.name, |g| g < generation)
    }
}

/// Deletes the log files of `name` whose generation `which` selects.
pub fn remove_logs(vfs: &dyn Vfs, dir: &Path, name: &str, which: impl Fn(u64) -> bool) -> Result<()> {
    let mut removed = false;
    for file in vfs.list(dir)? {
        if wal_generation(name, &file).is_some_and(&which) {
            vfs.remove(&dir.join(&file))?;
            removed = true;
        }
    }
    if removed {
        vfs.sync_dir(dir)?;
    }
    Ok(())
}

/// The generations of `name`'s log files in `dir`, ascending.
pub fn list_logs(vfs: &dyn Vfs, dir: &Path, name: &str) -> Result<Vec<u64>> {
    let mut generations: Vec<u64> = vfs.list(dir)?.iter().filter_map(|file| wal_generation(name, file)).collect();
    generations.sort_unstable();
    Ok(generations)
}

/// Reads one log file, calling `on_record` with each record that checks,
/// in order. Stops quietly at the first that does not. A file whose
/// header is missing, torn or of another data file holds nothing.
pub fn replay(vfs: &dyn Vfs, dir: &Path, name: &str, uuid: &[u8; 16], generation: u64, on_record: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
    let file = vfs.open(&wal_path(dir, name, generation), false)?;
    let len = file.len()?;
    let mut head = vec![0u8; HEADER_LEN as usize];
    if len < HEADER_LEN || read_exact_at(&*file, &mut head, 0).is_err() || head != header(generation, uuid) {
        return Ok(());
    }
    let mut offset = HEADER_LEN;
    loop {
        let mut prefix = [0u8; 8];
        if offset + 8 > len || read_exact_at(&*file, &mut prefix, offset).is_err() {
            return Ok(());
        }
        let size = u32::from_le_bytes(prefix[..4].try_into().unwrap());
        let crc = u32::from_le_bytes(prefix[4..].try_into().unwrap());
        if size > MAX_RECORD || offset + 8 + u64::from(size) > len {
            return Ok(());
        }
        let mut bytes = vec![0u8; size as usize];
        if read_exact_at(&*file, &mut bytes, offset + 8).is_err() || record_crc(generation, offset, &bytes) != crc {
            return Ok(());
        }
        on_record(&bytes)?;
        offset += 8 + u64::from(size);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::MemVfs;

    const UUID: [u8; 16] = [7; 16];

    fn records(vfs: &MemVfs, generation: u64) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        replay(vfs, Path::new("/d"), "t", &UUID, generation, &mut |r| {
            out.push(r.to_vec());
            Ok(())
        })
        .unwrap();
        out
    }

    #[test]
    fn appended_records_are_read_back_in_order() {
        let vfs = MemVfs::new();
        let wal = Wal::create(Arc::new(vfs.clone()), Path::new("/d"), "t", UUID, 1, 0, true).unwrap();
        for v in 1..=50u64 {
            wal.append(v, &vec![v as u8; (v * 37 % 900) as usize]).unwrap();
        }
        assert_eq!(wal.sync_to(50).unwrap(), 50);
        let got = records(&vfs, 1);
        assert_eq!(got.len(), 50);
        assert!(got.iter().enumerate().all(|(i, r)| r.len() == ((i as u64 + 1) * 37 % 900) as usize));
    }

    #[test]
    fn a_crash_keeps_every_synced_record_and_a_prefix_of_the_rest() {
        let vfs = MemVfs::new();
        let wal = Wal::create(Arc::new(vfs.clone()), Path::new("/d"), "t", UUID, 1, 0, true).unwrap();
        for v in 1..=10u64 {
            wal.append(v, &[v as u8; 600]).unwrap();
        }
        wal.sync_to(10).unwrap();
        for v in 11..=20u64 {
            wal.append(v, &[v as u8; 600]).unwrap();
        }
        for seed in 0..200 {
            let got = records(&vfs.crash_image(seed), 1);
            assert!(got.len() >= 10 && got.len() <= 20, "seed {seed}: {} records", got.len());
            for (i, r) in got.iter().enumerate() {
                assert_eq!(r, &vec![i as u8 + 1; 600], "seed {seed}: records come back whole and in order");
            }
        }
    }

    #[test]
    fn a_flipped_bit_ends_the_log_there() {
        let vfs = MemVfs::new();
        let wal = Wal::create(Arc::new(vfs.clone()), Path::new("/d"), "t", UUID, 1, 0, true).unwrap();
        for v in 1..=5u64 {
            wal.append(v, &[v as u8; 100]).unwrap();
        }
        wal.sync_to(5).unwrap();
        vfs.corrupt(&wal_path(Path::new("/d"), "t", 1), HEADER_LEN + 2 * 108 + 50);
        assert_eq!(records(&vfs, 1).len(), 2);
    }

    #[test]
    fn rotation_starts_a_new_file_and_old_ones_can_go() {
        let vfs = MemVfs::new();
        let wal = Wal::create(Arc::new(vfs.clone()), Path::new("/d"), "t", UUID, 1, 0, true).unwrap();
        wal.append(1, b"one").unwrap();
        assert_eq!(wal.rotate().unwrap(), 2);
        assert_eq!(wal.synced_version(), 1, "rotation syncs what was appended");
        wal.append(2, b"two").unwrap();
        wal.sync_to(2).unwrap();
        assert_eq!(list_logs(&vfs, Path::new("/d"), "t").unwrap(), vec![1, 2]);
        assert_eq!(records(&vfs, 1), vec![b"one".to_vec()]);
        assert_eq!(records(&vfs, 2), vec![b"two".to_vec()]);
        wal.remove_before(2).unwrap();
        assert_eq!(list_logs(&vfs, Path::new("/d"), "t").unwrap(), vec![2]);
        // A file of another database, or of another generation's name,
        // holds nothing for this one.
        let mut none = 0;
        replay(&vfs, Path::new("/d"), "t", &[8; 16], 2, &mut |_| {
            none += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(none, 0);
    }

    #[test]
    fn concurrent_committers_share_syncs() {
        let vfs = MemVfs::new();
        let wal = Arc::new(Wal::create(Arc::new(vfs.clone()), Path::new("/d"), "t", UUID, 1, 0, true).unwrap());
        let next = Arc::new(Mutex::new(0u64));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (wal, next) = (wal.clone(), next.clone());
                std::thread::spawn(move || {
                    for _ in 0..200 {
                        let version = {
                            let mut n = next.lock().unwrap();
                            *n += 1;
                            wal.append(*n, b"record").unwrap();
                            *n
                        };
                        assert!(wal.sync_to(version).unwrap() >= version);
                    }
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        assert_eq!(records(&vfs, 1).len(), 1600);
        assert_eq!(wal.synced_version(), 1600);
    }
}
