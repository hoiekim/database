//! The archive: the log kept beyond its checkpoints, in a directory the
//! operator names, so that a database can be had again as it stood at a
//! moment.
//!
//! A checkpoint makes the log files before it unnecessary for recovery
//! and removes them. With an archive, each is first copied there. The
//! log is logical (`wal.rs`), so its records apply to one thing only: the
//! data file they were logged against, as it stood where they begin. A
//! copy of the data file as of a checkpoint is a **base**; a base and the
//! log files that follow it without a break are a database at every
//! commit from the base on.
//!
//! ```text
//! <archive>/shop.5f0c…e1/              one history: a name and a data file's uuid
//!     base-0000000012                  the data file as log file 12 begins
//!     wal-0000000012                   log file 12, as far as its records go
//!     wal-0000000013
//! ```
//!
//! **A history** is the commits made to one data file, which its uuid
//! names. A database dropped and made again under its name is another
//! history; so is a database restored from an archive, which is given a
//! new uuid: what is committed to it after the restore is not what was
//! committed after that moment the first time, and the two must not be
//! taken for one log.
//!
//! **A file is in the archive whole or not at all.** Each is written
//! under another name, synced, and given its own; then the directory is
//! synced. Only then is the log file it copies removed from the data
//! directory. A file whose name ends in `--writing` was being written
//! when the server stopped, and is written again. A file whose sync
//! failed is not taken for written, whatever the directory then shows:
//! it is written again from the log file, which is still there.
//!
//! **A base** is the data file's bytes up to the last page its checkpoint
//! knows of, and after them a trailer: which log file continues from it,
//! a time no commit in it was made after, its length and a CRC of its
//! bytes. It is copied while the server runs, with checkpoints held off:
//! nothing a checkpoint names is written again before the next one, so
//! the bytes copied are that checkpoint, whatever transactions write
//! meanwhile to pages it does not name.
//!
//! **A log file's copy** is its header and its records: the room the file
//! was grown by is left out, and so is a tail a crash tore, which was
//! never acknowledged. After them comes a **seal**: a time, taken when
//! the next file was begun, that no commit in this file was made after
//! and none in a later file before. The archive ends with a seal, and so
//! knows up to what time it is the whole of what was committed, which
//! its last commit's time alone would not say. The copy reads as the
//! file does (`wal::replay_file`).
//!
//! Restoring is recovery (`Database::open`) on a base and copies of the
//! log files after it, the last cut after the commit the moment names:
//! see `Engine::recover_database`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use database_protocol::time::format_timestamp;

use crate::error::{Error, Result};
use crate::vfs::{read_exact_at, File, Vfs};
use crate::wal::{self, Commit, End, Replayed};

/// Ends the name a file of the archive is written under, before it is
/// whole.
const WRITING: &str = "--writing";
const BASE_MAGIC: &[u8; 8] = b"dbbase01";
const TRAILER_LEN: u64 = 40;
/// How much of a file is read and written at a time.
const CHUNK: usize = 1 << 20;

pub use database_protocol::message::Moment;

/// A place in a history's log, written as it is read back: `12:4096`.
pub fn position_text(generation: u64, offset: u64) -> String {
    format!("{generation}:{offset}")
}

/// Reads a position written by `position_text`.
pub fn parse_position(text: &str) -> Option<(u64, u64)> {
    let (generation, offset) = text.split_once(':')?;
    Some((generation.parse().ok()?, offset.parse().ok()?))
}

pub struct Archive {
    vfs: Arc<dyn Vfs>,
    dir: PathBuf,
}

impl std::fmt::Debug for Archive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Archive({})", self.dir.display())
    }
}

/// The directory of a history: the database's name and its data file's
/// uuid.
pub fn history_name(name: &str, uuid: &[u8; 16]) -> String {
    let hex: String = uuid.iter().map(|b| format!("{b:02x}")).collect();
    format!("{name}.{hex}")
}

/// The name and uuid a history's directory is named by.
pub fn parse_history(history: &str) -> Option<(&str, [u8; 16])> {
    let (name, hex) = history.rsplit_once('.')?;
    if hex.len() != 32 || !hex.is_ascii() || crate::engine::check_database_name(name).is_err() {
        return None;
    }
    let mut uuid = [0u8; 16];
    for (i, byte) in uuid.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some((name, uuid))
}

fn base_file(generation: u64) -> String {
    format!("base-{generation:010}")
}

fn log_file(generation: u64) -> String {
    format!("wal-{generation:010}")
}

/// What a history's directory holds, ascending.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    pub bases: Vec<u64>,
    pub logs: Vec<u64>,
}

impl Listing {
    /// The log file the archive lacks next: the first after its newest
    /// base that is not there. `None` without a base, since log files
    /// apply to nothing then.
    pub fn next(&self) -> Option<u64> {
        let mut next = *self.bases.last()?;
        while self.logs.binary_search(&next).is_ok() {
            next += 1;
        }
        Some(next)
    }
}

/// What a base's trailer says of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Base {
    /// The log file that continues from it.
    pub generation: u64,
    /// A time no commit it holds was made after, and none in the log
    /// files from `generation` on before. 0 if the log was not stamping.
    pub time: i64,
    /// The bytes of the data file it holds.
    pub len: u64,
    crc: u32,
}

impl Base {
    fn trailer(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(TRAILER_LEN as usize);
        out.extend_from_slice(BASE_MAGIC);
        out.extend_from_slice(&self.generation.to_le_bytes());
        out.extend_from_slice(&self.time.to_le_bytes());
        out.extend_from_slice(&self.len.to_le_bytes());
        out.extend_from_slice(&self.crc.to_le_bytes());
        let crc = crc32fast::hash(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }
}

/// What reading one archived log file found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LogSummary {
    pub commits: u64,
    /// The times of its first and last stamped commits.
    pub first: Option<i64>,
    pub last: Option<i64>,
}

fn damaged(what: String) -> Error {
    Error::corrupt(what)
}

impl Archive {
    pub fn new(vfs: Arc<dyn Vfs>, dir: &Path) -> Archive {
        Archive { vfs, dir: dir.to_path_buf() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn history_dir(&self, history: &str) -> PathBuf {
        self.dir.join(history)
    }

    /// The histories the archive holds, by name.
    pub fn histories(&self) -> Result<Vec<String>> {
        let mut names: Vec<String> = self.vfs.list_dirs(&self.dir)?.into_iter().filter(|name| parse_history(name).is_some()).collect();
        names.sort();
        Ok(names)
    }

    /// The bases and log files of a history. A file still being written
    /// is not one yet.
    pub fn list(&self, history: &str) -> Result<Listing> {
        let mut listing = Listing::default();
        let files = match self.vfs.list(&self.history_dir(history)) {
            Ok(files) => files,
            // No directory of that name in an archive that can be read
            // is a history with nothing in it yet. An archive that
            // cannot be read says nothing of what it holds.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.vfs.list_dirs(&self.dir)?;
                Vec::new()
            }
            Err(e) => return Err(e.into()),
        };
        for file in files {
            if let Some(generation) = file.strip_prefix("base-").and_then(|g| g.parse().ok()) {
                listing.bases.push(generation);
            } else if let Some(generation) = file.strip_prefix("wal-").and_then(|g| g.parse().ok()) {
                listing.logs.push(generation);
            }
        }
        listing.bases.sort_unstable();
        listing.logs.sort_unstable();
        Ok(listing)
    }

    /// Writes one file of a history, so that it is there whole or not at
    /// all: under another name, synced, then renamed, and the directory
    /// synced.
    fn put(&self, history: &str, name: &str, write: &mut dyn FnMut(&dyn File) -> Result<()>) -> Result<()> {
        let dir = self.history_dir(history);
        self.vfs.create_dir_all(&dir)?;
        let writing = dir.join(format!("{name}{WRITING}"));
        let file = self.vfs.open(&writing, true)?;
        file.set_len(0)?;
        write(&*file)?;
        file.sync()?;
        drop(file);
        self.vfs.rename(&writing, &dir.join(name))?;
        // The directory itself is an entry of the archive's.
        let synced = self.vfs.sync_dir(&dir).and_then(|()| self.vfs.sync_dir(&self.dir));
        if let Err(e) = synced {
            // The file has its name and may not keep it. It is taken
            // away again, as well as that can be done, so that whoever
            // lists the directory is not shown a file a power cut may
            // take back: what is listed is what a sync has answered for.
            let _ = self.vfs.remove(&dir.join(name));
            return Err(e.into());
        }
        Ok(())
    }

    /// Copies a log file of the data directory into the history: its
    /// header and the records that check. What follows them is room the
    /// file was grown by, or the tail of a crash.
    ///
    /// The copy ends with a seal: `sealed` is a time no commit in the
    /// file was made after and none in a later file before. It is how
    /// the archive knows, of a time after its last commit, that nothing
    /// was committed between the two.
    pub fn put_log(&self, history: &str, uuid: &[u8; 16], generation: u64, local: &dyn File, sealed: i64) -> Result<()> {
        let read = wal::replay_file(local, uuid, generation, &mut |_| Ok(()))?;
        self.put(history, &log_file(generation), &mut |out| {
            let mut at = read.at;
            if at == 0 {
                // It holds nothing of this data file's. The copy says as
                // much, and keeps the place in the sequence.
                at = wal::FIRST_RECORD;
                out.write_at(&wal::file_header(generation, uuid), 0)?;
            } else {
                copy(local, out, at, &mut |_| {})?;
            }
            out.write_at(&wal::seal(generation, at, sealed), at)?;
            Ok(())
        })
    }

    /// Writes a base: `len` bytes that `read` gives, and the trailer.
    pub fn put_base(&self, history: &str, generation: u64, time: i64, len: u64, read: &dyn Fn(&mut [u8], u64) -> std::io::Result<()>) -> Result<()> {
        self.put(history, &base_file(generation), &mut |out| {
            let mut hasher = crc32fast::Hasher::new();
            let mut buf = vec![0u8; CHUNK];
            let mut at = 0u64;
            while at < len {
                let n = (len - at).min(CHUNK as u64) as usize;
                read(&mut buf[..n], at)?;
                hasher.update(&buf[..n]);
                out.write_at(&buf[..n], at)?;
                at += n as u64;
            }
            let base = Base { generation, time, len, crc: hasher.finalize() };
            out.write_at(&base.trailer(), len)?;
            Ok(())
        })
    }

    /// Whether the history has a base under this number, as far as the
    /// directory shows.
    pub fn has_base(&self, history: &str, generation: u64) -> bool {
        self.vfs.exists(&self.history_dir(history).join(base_file(generation)))
    }

    fn open(&self, history: &str, name: &str) -> Result<Arc<dyn File>> {
        Ok(self.vfs.open(&self.history_dir(history).join(name), false)?)
    }

    /// Reads a base's trailer. The bytes before it are not read: see
    /// `copy_base`.
    pub fn base(&self, history: &str, generation: u64) -> Result<Base> {
        let name = base_file(generation);
        let file = self.open(history, &name)?;
        let len = file.len()?;
        let bad = || damaged(format!("{name} of {history} in the archive is damaged: it does not end as a base ends"));
        if len < TRAILER_LEN {
            return Err(bad());
        }
        let mut trailer = vec![0u8; TRAILER_LEN as usize];
        read_exact_at(&*file, &mut trailer, len - TRAILER_LEN)?;
        let field = |at: usize| u64::from_le_bytes(trailer[at..at + 8].try_into().unwrap());
        let base = Base { generation: field(8), time: field(16) as i64, len: field(24), crc: u32::from_le_bytes(trailer[32..36].try_into().unwrap()) };
        if trailer != base.trailer() || base.generation != generation || base.len != len - TRAILER_LEN {
            return Err(bad());
        }
        Ok(base)
    }

    /// Reads a base whole, giving its bytes to `out` in order if there
    /// is one, and fails if they are not the bytes that were written.
    pub fn copy_base(&self, history: &str, base: &Base, out: Option<&dyn File>) -> Result<()> {
        let name = base_file(base.generation);
        let file = self.open(history, &name)?;
        let mut hasher = crc32fast::Hasher::new();
        let mut buf = vec![0u8; CHUNK];
        let mut at = 0u64;
        while at < base.len {
            let n = (base.len - at).min(CHUNK as u64) as usize;
            read_exact_at(&*file, &mut buf[..n], at)?;
            hasher.update(&buf[..n]);
            if let Some(out) = out {
                out.write_at(&buf[..n], at)?;
            }
            at += n as u64;
        }
        if hasher.finalize() != base.crc {
            return Err(damaged(format!("{name} of {history} in the archive is damaged: its bytes are not the ones that were written")));
        }
        Ok(())
    }

    /// Reads an archived log file, calling `on_commit` with each commit.
    pub fn read_log(&self, history: &str, uuid: &[u8; 16], generation: u64, on_commit: &mut dyn FnMut(Commit) -> Result<()>) -> Result<Replayed> {
        let file = self.open(history, &log_file(generation))?;
        wal::replay_file(&*file, uuid, generation, on_commit)
    }

    /// Copies the first `len` bytes of an archived log file to `out`.
    pub fn copy_log(&self, history: &str, generation: u64, len: u64, out: &dyn File) -> Result<()> {
        let file = self.open(history, &log_file(generation))?;
        copy(&*file, out, len, &mut |_| {})
    }

    /// Reads every file of a history and says what it holds, or how it
    /// is not whole. With `commits`, a line for each commit as well: its
    /// position, its time and the bytes of its changes.
    ///
    /// A history is whole when every base checks, and from its oldest
    /// base on every log file is there, checks to its end, and ends
    /// where the next says it ended. Any moment from the oldest base to
    /// the last commit can then be restored.
    pub fn report(&self, history: &str, commits: bool) -> Result<String> {
        let Some((_, uuid)) = parse_history(history) else {
            return Err(Error::Db(database_protocol::error::DbError::invalid(format!("{history:?} does not name a history of the archive"))));
        };
        let listing = self.list(history)?;
        let not_whole = |what: String| damaged(format!("the archive of {history} is not whole: {what}"));
        let Some(&oldest) = listing.bases.first() else {
            return Err(not_whole("it holds no base, and log files apply to nothing without one".to_string()));
        };
        let mut out = String::new();
        for generation in &listing.bases {
            let base = self.base(history, *generation)?;
            self.copy_base(history, &base, None)?;
            out.push_str(&format!("{history}: base {generation}, {} bytes, taken {}\n", base.len, time_text(base.time)));
        }
        let mut lines = String::new();
        let mut total = LogSummary::default();
        let mut last_position = None;
        // Where the file before ended, if it held records.
        let mut before: Option<u64> = None;
        let mut generation = oldest;
        while listing.logs.binary_search(&generation).is_ok() {
            let read = self.read_log(history, &uuid, generation, &mut |commit| {
                total.commits += 1;
                if let Some(time) = commit.time {
                    total.first.get_or_insert(time);
                    total.last = Some(time);
                }
                last_position = Some((generation, commit.end));
                if commits {
                    lines.push_str(&format!("{}  {}  {} bytes\n", position_text(generation, commit.end), commit.time.map_or_else(|| "no time".to_string(), format_timestamp), commit.changes.len()));
                }
                Ok(())
            })?;
            if read.end == End::Torn || read.at == 0 || read.sealed == 0 {
                return Err(not_whole(format!("log file {generation} is damaged after byte {}", read.at)));
            }
            if let (Some(ended), true) = (before, read.after > 0) {
                if ended != read.after {
                    return Err(not_whole(format!("log file {} ends at byte {ended}, and log file {generation} says it ended at byte {}", generation - 1, read.after)));
                }
            }
            before = (read.at > wal::FIRST_RECORD).then_some(read.at);
            generation += 1;
        }
        if let Some(later) = listing.logs.iter().find(|g| **g > generation) {
            return Err(not_whole(format!("log file {generation} is missing, and log file {later} is there: no moment after log file {} can be restored from a base before it", generation.saturating_sub(1))));
        }
        if let Some(later) = listing.bases.iter().find(|g| **g > generation) {
            return Err(not_whole(format!("log file {generation} is missing before base {later}: no moment between them can be restored")));
        }
        out.push_str(&lines);
        let logs = if generation > oldest { format!("log files {oldest} to {}", generation - 1) } else { "no log files".to_string() };
        let span = match (total.first, total.last, last_position) {
            (Some(first), Some(last), Some((g, end))) => format!(", made from {} to {}, the last at {}", format_timestamp(first), format_timestamp(last), position_text(g, end)),
            _ => String::new(),
        };
        out.push_str(&format!("{history}: whole: {} bases, {logs}, {} commits{span}\n", listing.bases.len(), total.commits));
        Ok(out)
    }
}

/// A base's or a commit's time as a report shows it.
pub fn time_text(time: i64) -> String {
    if time == 0 {
        "at a time not recorded".to_string()
    } else {
        format_timestamp(time)
    }
}

/// Copies the first `len` bytes of `from` to `to`, showing each piece to
/// `seen` on its way.
fn copy(from: &dyn File, to: &dyn File, len: u64, seen: &mut dyn FnMut(&[u8])) -> Result<()> {
    let mut buf = vec![0u8; CHUNK.min(len as usize)];
    let mut at = 0u64;
    while at < len {
        let n = (len - at).min(CHUNK as u64) as usize;
        read_exact_at(from, &mut buf[..n], at)?;
        seen(&buf[..n]);
        to.write_at(&buf[..n], at)?;
        at += n as u64;
    }
    Ok(())
}

/// Whether the archive of a history continues: what a database knows of
/// its own, between one checkpoint and the next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chain {
    /// The archive has not been read yet, or could not be.
    Unknown,
    /// It holds no base that the log files still in the data directory
    /// continue from: the next checkpoint begins with a base.
    Broken,
    /// It holds a base and every log file after it up to this one, which
    /// is the next to be copied.
    Next(u64),
}

/// How a database's archiving stands, for `stats`.
#[derive(Clone, Debug)]
pub struct Progress {
    pub chain: Chain,
    pub logs_written: u64,
    pub bases_written: u64,
    /// Log files a checkpoint has passed that wait in the data directory
    /// to be copied.
    pub kept: usize,
    /// Why the last attempt to write to the archive failed, if it did.
    pub error: Option<String>,
    /// The file this server knew the archive to lack next, kept while
    /// the archive is read again.
    pub known: Option<u64>,
    /// The newest base the archive was seen to hold.
    pub base: Option<u64>,
}

impl Default for Progress {
    fn default() -> Self {
        Progress { chain: Chain::Unknown, logs_written: 0, bases_written: 0, kept: 0, error: None, known: None, base: None }
    }
}
