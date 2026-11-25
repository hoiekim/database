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
//! acknowledged. That is said only of bytes that were read. A read that
//! fails says nothing about what the file holds, and is an error.
//!
//! A file is synced whole before the next is begun, so only the newest
//! can end short. To hold a disk to that, the first record written to a
//! file that follows one with records is preceded by a record of the
//! log's own, a **link**, that says where those records ended. A file
//! that ends anywhere else has lost records since, to a sync that lied or
//! to damage, and what the next file holds must not be applied over the
//! gap. A file that never gets a record needs no link and has none: a
//! server stopped in good order leaves a log that says nothing new.
//!
//! A file that grows with every record makes every sync slow: the file
//! system must then commit the file's new length as well as its bytes,
//! which here took four times as long as the bytes alone. So a busy log
//! is grown ahead of its records, by zeros, in steps that double to a
//! megabyte, and a record is then written over room the file already
//! has. A record is never empty, so the zeros end the log for a reader.
//!
//! A log that is kept beyond its checkpoint (see `archive.rs`) is asked
//! what the database held at a moment, which the changes alone do not
//! say. Such a log **stamps** its records: each begins with a byte no
//! change begins with, and the time of the commit after it. A log that
//! is not kept writes its records as it always did.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use crate::error::{Error, Result};
use crate::vfs::{read_exact_at, File, Vfs};

const MAGIC: &[u8; 8] = b"dbwal001";
const HEADER_LEN: u64 = 36;
/// No record is longer than this. A length beyond it is damage.
const MAX_RECORD: u32 = 1 << 30;
/// How far ahead of its records a log file is grown: nothing at first,
/// so that a file which gets one record costs no more than the record,
/// then from the least of these to the most, doubling.
const GROW_LEAST: u64 = 16 * 1024;
const GROW_MOST: u64 = 1024 * 1024;
/// The first byte of a link. No transaction's record begins with it
/// (`write.rs` numbers what a record holds from 1), and a build that
/// knows nothing of links refuses the record rather than pass it over.
const LINK: u8 = 0xF0;
/// A link: that byte, and where the records of the file before ended.
const LINK_LEN: usize = 9;
/// The first byte of a stamped record: a commit's time, in microseconds
/// since the epoch, and then its changes. Like a link's, it is a byte no
/// change begins with, so a build that knows nothing of stamps refuses
/// the record rather than read a time as a change.
const STAMP: u8 = 0xF1;
/// The first byte of a seal: the record that ends a log file's copy in
/// the archive, with a time no commit in the file was made after and
/// none in a later file before. A file in the data directory has none.
const SEAL: u8 = 0xF2;
/// What a stamp adds to a record: that byte and the time.
pub const STAMP_LEN: usize = 9;
/// Where a file's first record begins.
pub const FIRST_RECORD: u64 = HEADER_LEN;

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
    stamped_crc(generation, offset, &[], bytes)
}

/// The CRC of a record that is `stamp` and then `bytes`.
fn stamped_crc(generation: u64, offset: u64, stamp: &[u8], bytes: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&generation.to_le_bytes());
    hasher.update(&offset.to_le_bytes());
    hasher.update(stamp);
    hasher.update(bytes);
    hasher.finalize()
}

struct Tail {
    file: Arc<dyn File>,
    generation: u64,
    /// Where the records of the file before this one ended, to be said
    /// with this file's first record: 0 once said, or with nothing to
    /// say.
    link: u64,
    offset: u64,
    /// How long the file is: records are written before this where they
    /// can be.
    allocated: u64,
    /// How far past a record the file is grown when next it must grow.
    grow: u64,
    /// The newest version whose record is fully appended.
    appended: u64,
    /// The newest version that has a record at all.
    last_record: u64,
    /// The time the newest stamp gave, which the next will not be
    /// before: a clock set back does not put a later commit earlier.
    stamped: i64,
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
    /// Whether each record says when its commit was made.
    stamp: bool,
}

fn failed() -> Error {
    Error::Io(std::io::Error::other("an earlier write to the log failed"))
}

impl Wal {
    /// Starts a new log file of the given generation, replacing any file
    /// of that name. `version` is the newest committed version so far.
    pub fn create(vfs: Arc<dyn Vfs>, dir: &Path, name: &str, uuid: [u8; 16], generation: u64, version: u64, sync: bool) -> Result<Wal> {
        Self::create_after(vfs, dir, name, uuid, generation, version, sync, 0)
    }

    /// The same, for a file that follows another: `previous` is where
    /// the records of the file before it ended, or 0 if it held none or
    /// there is none. The file will say so with its first record.
    #[allow(clippy::too_many_arguments)]
    pub fn create_after(vfs: Arc<dyn Vfs>, dir: &Path, name: &str, uuid: [u8; 16], generation: u64, version: u64, sync: bool, previous: u64) -> Result<Wal> {
        let file = Self::start_file(&*vfs, dir, name, &uuid, generation)?;
        Ok(Wal {
            vfs,
            dir: dir.to_path_buf(),
            name: name.to_string(),
            uuid,
            tail: Mutex::new(Tail { file, generation, link: previous, offset: HEADER_LEN, allocated: HEADER_LEN, grow: 0, appended: version, last_record: version, stamped: 0 }),
            synced: Mutex::new(Synced { version, syncing: false, failed: false }),
            wake: Condvar::new(),
            sync,
            stamp: false,
        })
    }

    /// Has every record from now on say when its commit was made, none
    /// earlier than `floor`: the newest time the log already holds.
    pub fn stamping(mut self, floor: i64) -> Wal {
        self.stamp = true;
        self.tail.get_mut().unwrap().stamped = floor;
        self
    }

    /// A time no commit made so far is stamped after, and none made from
    /// now on before. The caller holds the write lock, so that no commit
    /// is being made. 0 for a log that does not stamp.
    pub fn moment(&self) -> i64 {
        if !self.stamp {
            return 0;
        }
        let mut tail = self.tail.lock().unwrap();
        tail.stamped = tail.stamped.max(database_protocol::time::now_micros());
        tail.stamped
    }

    /// Where the newest record ends: the file and the byte.
    pub fn position(&self) -> (u64, u64) {
        let tail = self.tail.lock().unwrap();
        (tail.generation, tail.offset)
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
        debug_assert!(!bytes.is_empty(), "an empty record would read as the end of the log");
        // What a reader would take for damage must not be written: the
        // commit fails here, whole, instead of being lost at recovery.
        let stamp_len = if self.stamp { STAMP_LEN } else { 0 };
        if bytes.len() + stamp_len > MAX_RECORD as usize {
            return Err(Error::db(
                database_protocol::error::ErrorCode::OutOfRange,
                format!("the transaction's changes take {} bytes of log, and one transaction may take {MAX_RECORD}: commit it in parts", bytes.len() + stamp_len),
            ));
        }
        if self.synced.lock().unwrap().failed {
            return Err(failed());
        }
        let mut tail = self.tail.lock().unwrap();
        let mut frame = Vec::with_capacity(bytes.len() + 16 + LINK_LEN + stamp_len);
        if tail.link > 0 {
            // The file's first record: the link goes before it, in the
            // same write.
            let mut link = [LINK; LINK_LEN];
            link[1..].copy_from_slice(&tail.link.to_le_bytes());
            frame.extend_from_slice(&(LINK_LEN as u32).to_le_bytes());
            frame.extend_from_slice(&record_crc(tail.generation, tail.offset, &link).to_le_bytes());
            frame.extend_from_slice(&link);
        }
        let mut stamp = [STAMP; STAMP_LEN];
        let stamped = if self.stamp { tail.stamped.max(database_protocol::time::now_micros()) } else { tail.stamped };
        stamp[1..].copy_from_slice(&stamped.to_le_bytes());
        let stamp = &stamp[..stamp_len];
        let at = tail.offset + frame.len() as u64;
        frame.extend_from_slice(&((bytes.len() + stamp_len) as u32).to_le_bytes());
        frame.extend_from_slice(&stamped_crc(tail.generation, at, stamp, bytes).to_le_bytes());
        frame.extend_from_slice(stamp);
        frame.extend_from_slice(bytes);
        let end = tail.offset + frame.len() as u64;
        let mut written = tail.file.write_at(&frame, tail.offset);
        if written.is_ok() && end > tail.allocated {
            // The file grew: grow it further, for the records to come.
            if tail.grow > 0 {
                written = tail.file.write_at(&vec![0u8; tail.grow as usize], end);
            }
            tail.allocated = end + tail.grow;
            tail.grow = (tail.grow * 2).clamp(GROW_LEAST, GROW_MOST);
        }
        if let Err(e) = written {
            self.synced.lock().unwrap().failed = true;
            return Err(e.into());
        }
        tail.offset = end;
        tail.link = 0;
        tail.appended = version;
        tail.last_record = version;
        tail.stamped = stamped;
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
                // A file that holds a record holds a transaction's.
                tail.link = if tail.offset > HEADER_LEN { tail.offset } else { 0 };
                tail.file = file;
                tail.generation = generation;
                tail.offset = HEADER_LEN;
                tail.allocated = HEADER_LEN;
                tail.grow = 0;
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

/// How the reading of a log file ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// At the end of what was written: the end of the file, or room the
    /// file was grown by and never written over.
    Clean,
    /// At something that is not a whole record. In the newest file that
    /// is the tail a crash tore, and nothing after it was acknowledged.
    /// In any older file it is damage: the file was synced whole before
    /// the next was begun.
    Torn,
}

/// What reading one log file found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Replayed {
    pub end: End,
    /// Where its records ended: 0 for a file that holds none of this
    /// data file's, for want of a header.
    pub at: u64,
    /// Where the file says the records of the file before it ended: 0
    /// if it says nothing, for having no records of its own or because
    /// that file had none.
    pub after: u64,
    /// The time the file's seal gives, for a copy that has one: 0 if
    /// it has none.
    pub sealed: i64,
}

/// One transaction's record, as a reader of the log is given it.
#[derive(Clone, Copy, Debug)]
pub struct Commit<'a> {
    /// Where the record ends in its file: with the file's generation,
    /// the commit's position in the log.
    pub end: u64,
    /// When the commit was made, if the log was stamping.
    pub time: Option<i64>,
    /// What the transaction changed: see `write.rs`.
    pub changes: &'a [u8],
}

/// Reads one log file, calling `on_record` with each transaction's record
/// that checks, in order, and stops at the first that does not, saying
/// how and where. A file whose header is missing or torn holds nothing
/// and ends torn; a whole file of another data file holds nothing of this
/// one's.
///
/// A file ends where its length says it does, or at bytes that were read
/// and do not check. A read that fails is neither: the records may all
/// be there, so it is returned as the error it is, and the caller must
/// not go on to delete the file.
pub fn replay(vfs: &dyn Vfs, dir: &Path, name: &str, uuid: &[u8; 16], generation: u64, on_record: &mut dyn FnMut(Commit) -> Result<()>) -> Result<Replayed> {
    let file = vfs.open(&wal_path(dir, name, generation), false)?;
    replay_file(&*file, uuid, generation, on_record)
}

/// `replay`, of a log file under whatever name it was opened by: a copy
/// of one reads as the file it was copied from.
pub fn replay_file(file: &dyn File, uuid: &[u8; 16], generation: u64, on_record: &mut dyn FnMut(Commit) -> Result<()>) -> Result<Replayed> {
    let len = file.len()?;
    let nothing = |end| Replayed { end, at: 0, after: 0, sealed: 0 };
    let mut head = vec![0u8; HEADER_LEN as usize];
    if len < HEADER_LEN {
        return Ok(nothing(End::Torn));
    }
    read_exact_at(file, &mut head, 0)?;
    if head != header(generation, uuid) {
        // Whole, but another data file's: left from a database that had
        // this name before. Anything else in a header's place is damage.
        let whole = head[..8] == MAGIC[..] && crc32fast::hash(&head[..32]).to_le_bytes() == head[32..];
        return Ok(nothing(if whole { End::Clean } else { End::Torn }));
    }
    let mut offset = HEADER_LEN;
    let mut after = 0;
    // Where the records end, and the seal after them if there is one.
    let (mut at, mut sealed) = (HEADER_LEN, 0);
    loop {
        let ended = |end| Replayed { end, at, after, sealed };
        if offset == len {
            return Ok(ended(End::Clean));
        }
        let mut prefix = [0u8; 8];
        if offset + 8 > len {
            return Ok(ended(End::Torn));
        }
        read_exact_at(file, &mut prefix, offset)?;
        let size = u32::from_le_bytes(prefix[..4].try_into().unwrap());
        let crc = u32::from_le_bytes(prefix[4..].try_into().unwrap());
        // No record is empty: a length of zero is the room a file was
        // grown by and not yet written over.
        if size == 0 {
            return Ok(ended(if crc == 0 { End::Clean } else { End::Torn }));
        }
        if size > MAX_RECORD || offset + 8 + u64::from(size) > len {
            return Ok(ended(End::Torn));
        }
        let mut bytes = vec![0u8; size as usize];
        read_exact_at(file, &mut bytes, offset + 8)?;
        if record_crc(generation, offset, &bytes) != crc {
            return Ok(ended(End::Torn));
        }
        let end = offset + 8 + u64::from(size);
        if bytes[0] == SEAL && bytes.len() == STAMP_LEN {
            sealed = i64::from_le_bytes(bytes[1..].try_into().unwrap());
            offset = end;
            continue;
        }
        if offset == HEADER_LEN && bytes.len() == LINK_LEN && bytes[0] == LINK {
            after = u64::from_le_bytes(bytes[1..].try_into().unwrap());
        } else if bytes[0] == STAMP && bytes.len() >= STAMP_LEN {
            let time = i64::from_le_bytes(bytes[1..STAMP_LEN].try_into().unwrap());
            on_record(Commit { end, time: Some(time), changes: &bytes[STAMP_LEN..] })?;
        } else {
            on_record(Commit { end, time: None, changes: &bytes })?;
        }
        offset = end;
        at = end;
    }
}

/// A seal to follow the records of a copy of log file `generation` that
/// end at `at`: see `SEAL`.
pub fn seal(generation: u64, at: u64, time: i64) -> Vec<u8> {
    let mut record = [SEAL; STAMP_LEN];
    record[1..].copy_from_slice(&time.to_le_bytes());
    let mut out = Vec::with_capacity(8 + STAMP_LEN);
    out.extend_from_slice(&(STAMP_LEN as u32).to_le_bytes());
    out.extend_from_slice(&record_crc(generation, at, &record).to_le_bytes());
    out.extend_from_slice(&record);
    out
}

/// A log file's first bytes, for whoever writes a copy of one.
pub fn file_header(generation: u64, uuid: &[u8; 16]) -> Vec<u8> {
    header(generation, uuid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::MemVfs;

    const UUID: [u8; 16] = [7; 16];

    fn records(vfs: &MemVfs, generation: u64) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        replay(vfs, Path::new("/d"), "t", &UUID, generation, &mut |r| {
            out.push(r.changes.to_vec());
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
    fn a_busy_log_is_grown_ahead_of_its_records() {
        let vfs = MemVfs::new();
        let wal = Wal::create(Arc::new(vfs.clone()), Path::new("/d"), "t", UUID, 1, 0, true).unwrap();
        let file = vfs.open(&wal_path(Path::new("/d"), "t", 1), false).unwrap();
        // One record costs its own bytes and no more.
        wal.append(1, &[1; 100]).unwrap();
        assert_eq!(file.len().unwrap(), HEADER_LEN + 108);
        // From the second on, the file is longer than what it holds, and
        // its length changes for few of them.
        let mut lengths = vec![file.len().unwrap()];
        for v in 2..=3000u64 {
            wal.append(v, &[v as u8; 700]).unwrap();
            let len = file.len().unwrap();
            assert!(len >= wal.size());
            if len != *lengths.last().unwrap() {
                lengths.push(len);
            }
        }
        assert!(lengths.len() < 12, "the file changed length {} times", lengths.len());
        wal.sync_to(3000).unwrap();
        // The zeros beyond the last record end the log for a reader,
        // before a crash and after one.
        assert_eq!(records(&vfs, 1).len(), 3000);
        assert_eq!(records(&vfs.crash_image(5), 1).len(), 3000);
        // A new file starts small again: its header, the link to the
        // file before it, and the one record.
        wal.rotate().unwrap();
        wal.append(3001, &[9; 10]).unwrap();
        let next = vfs.open(&wal_path(Path::new("/d"), "t", 2), false).unwrap();
        assert_eq!(next.len().unwrap(), HEADER_LEN + 8 + LINK_LEN as u64 + 18);
    }

    #[test]
    fn a_file_says_where_the_one_before_it_ended() {
        let read = |vfs: &MemVfs, generation: u64| {
            let mut records = 0;
            let read = replay(vfs, Path::new("/d"), "t", &UUID, generation, &mut |_| {
                records += 1;
                Ok(())
            })
            .unwrap();
            (read, records)
        };
        let vfs = MemVfs::new();
        let wal = Wal::create(Arc::new(vfs.clone()), Path::new("/d"), "t", UUID, 1, 0, true).unwrap();
        // A file with no records leaves the next nothing to say.
        wal.rotate().unwrap();
        for v in 1..=3u64 {
            wal.append(v, &[v as u8; 100]).unwrap();
        }
        let ended = HEADER_LEN + 3 * 108;
        assert_eq!(wal.size(), ended);
        assert_eq!(read(&vfs, 2), (Replayed { end: End::Clean, at: ended, after: 0, sealed: 0 }, 3));
        // A file that has no record yet says nothing either, and is as
        // files were before there were links.
        wal.rotate().unwrap();
        assert_eq!(vfs.file_len(&wal_path(Path::new("/d"), "t", 3)), HEADER_LEN);
        assert_eq!(read(&vfs, 3), (Replayed { end: End::Clean, at: HEADER_LEN, after: 0, sealed: 0 }, 0));
        // Its first record brings the link with it. The link is not a
        // transaction's record, and later records bring none.
        wal.append(4, &[4; 50]).unwrap();
        wal.append(5, &[5; 50]).unwrap();
        wal.sync_to(5).unwrap();
        let linked = HEADER_LEN + 8 + LINK_LEN as u64;
        assert_eq!(read(&vfs, 3), (Replayed { end: End::Clean, at: linked + 2 * 58, after: ended, sealed: 0 }, 2));
        // The same for a file begun when a database is opened.
        let wal = Wal::create_after(Arc::new(vfs.clone()), Path::new("/d"), "t", UUID, 7, 5, true, 500).unwrap();
        assert_eq!(wal.size(), HEADER_LEN);
        wal.rotate().unwrap();
        assert_eq!(read(&vfs, 7), (Replayed { end: End::Clean, at: HEADER_LEN, after: 0, sealed: 0 }, 0));
        wal.append(6, &[6; 10]).unwrap();
        assert_eq!(read(&vfs, 8), (Replayed { end: End::Clean, at: HEADER_LEN + 18, after: 0, sealed: 0 }, 1));
        let wal = Wal::create_after(Arc::new(vfs.clone()), Path::new("/d"), "t", UUID, 9, 6, true, 500).unwrap();
        wal.append(7, &[7; 10]).unwrap();
        assert_eq!(read(&vfs, 9), (Replayed { end: End::Clean, at: linked + 18, after: 500, sealed: 0 }, 1));
    }

    #[test]
    fn a_reader_is_told_how_a_file_ended() {
        let end = |vfs: &MemVfs| replay(vfs, Path::new("/d"), "t", &UUID, 1, &mut |_| Ok(())).unwrap().end;
        let path = wal_path(Path::new("/d"), "t", 1);
        let written = || {
            let vfs = MemVfs::new();
            let wal = Wal::create(Arc::new(vfs.clone()), Path::new("/d"), "t", UUID, 1, 0, true).unwrap();
            for v in 1..=5u64 {
                wal.append(v, &[v as u8; 300]).unwrap();
            }
            wal.sync_to(5).unwrap();
            vfs
        };
        // Whole, with the room it was grown by still zero after it.
        let vfs = written();
        assert!(vfs.open(&path, false).unwrap().len().unwrap() > HEADER_LEN + 5 * 308);
        assert_eq!(end(&vfs), End::Clean);
        // Cut exactly after a record: nothing says it was ever longer.
        vfs.open(&path, false).unwrap().set_len(HEADER_LEN + 3 * 308).unwrap();
        assert_eq!(end(&vfs), End::Clean);
        // Cut inside a record, or inside the eight bytes before one.
        vfs.open(&path, false).unwrap().set_len(HEADER_LEN + 2 * 308 + 100).unwrap();
        assert_eq!(end(&vfs), End::Torn);
        vfs.open(&path, false).unwrap().set_len(HEADER_LEN + 2 * 308 + 5).unwrap();
        assert_eq!(end(&vfs), End::Torn);
        // A record that no longer checks.
        let vfs = written();
        vfs.open(&path, false).unwrap().write_at(&[0xFF], HEADER_LEN + 308 + 50).unwrap();
        assert_eq!(end(&vfs), End::Torn);
        // A header that is not one.
        let vfs = written();
        vfs.open(&path, false).unwrap().write_at(&[0xFF; 4], 10).unwrap();
        assert_eq!(end(&vfs), End::Torn);
        // The whole log of another data file holds nothing of this one's,
        // and is not damage: it is left from a database that had the
        // name before.
        let vfs = written();
        assert_eq!(replay(&vfs, Path::new("/d"), "t", &[9; 16], 1, &mut |_| panic!("not its records")).unwrap().end, End::Clean);
    }

    #[test]
    fn a_record_too_long_to_read_back_is_not_written() {
        let vfs = MemVfs::new();
        let wal = Wal::create(Arc::new(vfs.clone()), Path::new("/d"), "t", UUID, 1, 0, true).unwrap();
        // Never touched, so the gigabyte costs no memory.
        let huge = vec![0u8; MAX_RECORD as usize + 1];
        assert!(matches!(wal.append(1, &huge), Err(Error::Db(e)) if e.code == database_protocol::error::ErrorCode::OutOfRange));
        // The log is as it was, and takes the next commit.
        assert_eq!(wal.size(), HEADER_LEN);
        wal.append(1, &[7; 20]).unwrap();
        assert_eq!(wal.sync_to(1).unwrap(), 1);
        assert_eq!(records(&vfs, 1), [vec![7u8; 20]]);
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
    fn a_read_that_fails_is_an_error_and_not_the_end_of_the_log() {
        let vfs = MemVfs::new();
        let wal = Wal::create(Arc::new(vfs.clone()), Path::new("/d"), "t", UUID, 1, 0, true).unwrap();
        for v in 1..=5u64 {
            wal.append(v, &[v as u8; 300]).unwrap();
        }
        wal.sync_to(5).unwrap();
        let path = wal_path(Path::new("/d"), "t", 1);
        // The header, the eight bytes before the third record, and the
        // third record itself: wherever the disk gives no answer, the
        // reader has none either, having passed on what it read before.
        let third = HEADER_LEN + 2 * 308;
        for (unreadable, read_before) in [(0, 0), (third + 4, 2), (third + 8 + 150, 2)] {
            vfs.fail_reads_within(&path, unreadable..unreadable + 1);
            let mut read = 0;
            let end = replay(&vfs, Path::new("/d"), "t", &UUID, 1, &mut |_| {
                read += 1;
                Ok(())
            });
            assert!(matches!(end, Err(Error::Io(_))), "byte {unreadable} cannot be read: {end:?}");
            assert_eq!(read, read_before, "byte {unreadable} cannot be read");
        }
        // The same file, once it can be read, holds every record.
        vfs.fail_reads(None);
        assert_eq!(records(&vfs, 1).len(), 5);
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
