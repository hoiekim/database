//! `faults.rs` once more, with the engine keeping its log in an archive
//! (`archive.rs`) on the disk that fails. An archive adds operations to
//! every checkpoint, every start and every drop, and stamps every log
//! record; none of that may cost the database a commit, whatever the
//! disk does under it. What the archive itself then holds is the
//! business of `archive.rs` in this directory.

#[path = "faults.rs"]
mod archived;
