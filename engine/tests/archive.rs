//! The archive of the log, and restoring from it to a moment, through
//! the faults a disk can show either.
//!
//! The data directory and the archive are on one simulated disk here, so
//! that a power cut takes both at the same moment. A workload commits,
//! checkpoints and takes bases while every operation it makes on the
//! disk is kept; each commit is noted with what the table then held and
//! where its record ended in the log. Then the disk is rebuilt as it
//! stood before each operation, the power is cut there, and the engine
//! started again. What must hold, each time:
//!
//! - the database holds every commit that was acknowledged (R-X10, as
//!   without an archive);
//! - every acknowledged commit is in the archive or in a log file the
//!   data directory keeps: after one more checkpoint the archive is
//!   whole, and restores the table as it stood at any of them (R-O8);
//! - a restore to a moment holds exactly the commits made up to it
//!   (R-O9).
//!
//! The same for an archive that fails: every operation on it once, a
//! disk with no room, a directory that cannot be reached at all. And a
//! restore is itself cut before each of its operations.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::*;
use database_engine::archive::{parse_history, position_text, Archive, Moment};
use database_engine::db::Database;
use database_engine::engine::{Engine, EngineOptions};
use database_engine::error::Error;
use database_engine::vfs::{Fate, Fault, MemVfs, Op, OpKind, Rng, Vfs};
use database_engine::wal;
use database_protocol::error::ErrorCode;
use database_protocol::query::*;

const ARCHIVE: &str = "/archive";

/// The engine as these tests run it: with an archive, and with nothing
/// checkpointing by itself, so that a workload makes the same operations
/// on the disk every time it runs.
fn archiving() -> EngineOptions {
    let mut options = options();
    options.db.checkpoint_bytes = u64::MAX;
    options.archive = Some(PathBuf::from(ARCHIVE));
    options
}

fn start(vfs: &MemVfs) -> Engine {
    try_open_with(vfs, archiving()).expect("the engine starts")
}

fn in_archive(op: &Op) -> bool {
    op.path.starts_with(ARCHIVE)
}

fn archive_of(vfs: &MemVfs) -> Archive {
    Archive::new(Arc::new(vfs.clone()), Path::new(ARCHIVE))
}

/// The one history the archive holds of `test`.
fn history(vfs: &MemVfs) -> String {
    let histories: Vec<String> = archive_of(vfs).histories().unwrap().into_iter().filter(|h| parse_history(h).unwrap().0 == "test").collect();
    assert_eq!(histories.len(), 1, "one history of the database: {histories:?}");
    histories[0].clone()
}

/// A commit the engine acknowledged: what the table held with it, and
/// where its record ends in the log.
#[derive(Clone)]
struct Made {
    model: Model,
    position: (u64, u64),
}

fn moment(made: &Made) -> Moment {
    Moment::Position { generation: made.position.0, offset: made.position.1 }
}

/// One change to the table, in a transaction of its own. Returns false,
/// with the model as it was, if the engine refused it.
fn change(db: &Arc<Database>, rng: &mut Rng, model: &mut Model, next_id: &mut i64) -> bool {
    let mut after = model.clone();
    let result = (|| {
        let mut txn = db.begin_write(WAIT, false)?;
        for _ in 0..1 + rng.below(3) {
            let existing: Vec<i64> = after.keys().copied().collect();
            match rng.below(4) {
                0 | 1 => {
                    let item = random_item(rng, *next_id);
                    txn.insert(&insert_of(*next_id, &item), None)?;
                    after.insert(*next_id, item);
                    *next_id += 1;
                }
                2 if !existing.is_empty() => {
                    let id = existing[rng.below(existing.len() as u64) as usize];
                    let owner = rng.below(4) as i64;
                    txn.update(&Update::table("items").key([id]).set("owner", owner), None)?;
                    after.get_mut(&id).unwrap().owner = owner;
                }
                3 if !existing.is_empty() => {
                    let id = existing[rng.below(existing.len() as u64) as usize];
                    txn.delete(&Delete::from("items").key([id]), None)?;
                    after.remove(&id);
                }
                _ => {}
            }
        }
        txn.commit()
    })();
    match result {
        Ok(()) => {
            *model = after;
            true
        }
        Err(_) => false,
    }
}

/// Commits `commits` changes, with a checkpoint now and then and a base
/// once, noting each commit that was acknowledged.
fn workload(db: &Arc<Database>, seed: u64, commits: usize, model: &mut Model, made: &mut Vec<Made>, noted: &mut dyn FnMut()) {
    let mut rng = Rng::new(seed);
    let mut next_id = model.keys().next_back().map_or(1, |id| id + 1);
    for n in 0..commits {
        if change(db, &mut rng, model, &mut next_id) {
            made.push(Made { model: model.clone(), position: db.log_position() });
            noted();
        }
        if n % 5 == 4 {
            let _ = db.checkpoint();
        }
        if n == commits / 2 {
            let _ = db.base_copy();
        }
    }
}

/// Restores `test` at `to` as the database `name` and returns it.
fn restore(engine: &Engine, to: Moment, name: &str) -> Arc<Database> {
    engine.recover_database("test", to, name, true).unwrap_or_else(|e| panic!("the restore to {to:?} fails: {e}"));
    engine.database(name).unwrap()
}

/// The archive is whole, and restores the table as it stood at each of
/// `sample` and at its end, which is `last`.
fn verify_archive(engine: &Engine, sample: &[&Made], last: &Model) {
    engine.archive_report("test", false).unwrap_or_else(|e| panic!("the archive is not whole: {e}"));
    for made in sample {
        let at = restore(engine, moment(made), "then");
        verify(&at, &made.model);
    }
    verify(&restore(engine, Moment::End, "then"), last);
}

/// A disk with a database, an archive of it, and the commits made.
fn written(seed: u64, commits: usize) -> (MemVfs, Model, Vec<Made>) {
    let vfs = MemVfs::new();
    let engine = start(&vfs);
    let db = create(&engine);
    let (mut model, mut made) = (Model::new(), vec![Made { model: Model::new(), position: db.log_position() }]);
    workload(&db, seed, commits, &mut model, &mut made, &mut || {});
    db.checkpoint().unwrap();
    drop(db);
    drop(engine);
    (vfs, model, made)
}

// R-O8
#[test]
fn a_checkpoint_hands_its_log_to_the_archive_before_removing_it() {
    let vfs = MemVfs::new();
    let engine = start(&vfs);
    let db = create(&engine);
    let (mut model, mut made) = (Model::new(), Vec::new());
    workload(&db, 1, 12, &mut model, &mut made, &mut || {});
    db.checkpoint().unwrap();
    // The data directory keeps the log file being written and no other.
    let (current, _) = db.log_position();
    assert_eq!(wal::list_logs(&vfs, Path::new(DIR), "test").unwrap(), [current]);
    // The archive has a base from the database's first moment, and every
    // log file from it to the one being written.
    let listing = archive_of(&vfs).list(&history(&vfs)).unwrap();
    assert_eq!(listing.bases.first(), Some(&1), "{listing:?}");
    assert_eq!(listing.logs, (1..current).collect::<Vec<_>>());
    assert!(listing.bases.len() >= 2, "and the base that was asked for: {listing:?}");
    let report = engine.archive_report("test", false).unwrap();
    assert!(report.contains("whole"), "{report}");
    // And says so in the database's statistics.
    let stats = db.stats();
    assert!(stats.contains(&format!("whole up to log file {current}")), "{stats}");
}

// R-O9
#[test]
fn a_database_is_restored_as_it_stood_at_each_commit_it_made() {
    let (vfs, model, made) = written(2, 30);
    let engine = start(&vfs);
    for made in &made {
        let then = restore(&engine, moment(made), "then");
        verify(&then, &made.model);
    }
    verify(&restore(&engine, Moment::End, "then"), &model);
    // The database itself is as it was, and takes commits.
    let db = engine.database("test").unwrap();
    verify(&db, &model);
    // A position between two commits is the commit before it.
    let (generation, offset) = made[made.len() - 2].position;
    let between = if made[made.len() - 1].position.0 == generation { Moment::Position { generation, offset: offset + 1 } } else { Moment::Position { generation, offset: u64::MAX } };
    verify(&restore(&engine, between, "then"), &made[made.len() - 2].model);
}

/// Every commit the archive holds of `test`: where its record ends and
/// when it was made.
fn commits(vfs: &MemVfs) -> Vec<((u64, u64), i64)> {
    let (archive, history) = (archive_of(vfs), history(vfs));
    let uuid = parse_history(&history).unwrap().1;
    let mut out = Vec::new();
    for generation in archive.list(&history).unwrap().logs {
        archive
            .read_log(&history, &uuid, generation, &mut |commit| {
                out.push(((generation, commit.end), commit.time.expect("a log that is archived stamps its records")));
                Ok(())
            })
            .unwrap();
    }
    out
}

// R-O9
#[test]
fn a_restore_to_a_time_holds_the_commits_made_up_to_it_and_no_other() {
    let (vfs, _, made) = written(3, 30);
    let engine = start(&vfs);
    let times = commits(&vfs);
    // The table's own commit comes first in the log and is not in `made`.
    let model_at = |position: (u64, u64)| &made.iter().rev().find(|made| made.position <= position).expect("a commit at or before it").model;
    assert!(times.windows(2).all(|pair| pair[0].1 <= pair[1].1), "times never go back");
    for (i, (position, time)) in times.iter().enumerate() {
        // The last commit made at this time, and the last made before it.
        if times.get(i + 1).is_none_or(|(_, next)| next > time) && i + 1 < times.len() {
            verify(&restore(&engine, Moment::Time(*time), "then"), model_at(*position));
        }
        if i > 0 && times[i - 1].1 < *time {
            verify(&restore(&engine, Moment::Time(time - 1), "then"), model_at(times[i - 1].0));
        }
    }
    // The very last time is the end of the archive, and so is any later:
    // those are refused, since nothing says the archive is the whole of
    // what was committed by then.
    let last = times.last().unwrap().1;
    verify(&restore(&engine, Moment::Time(last - 1), "then"), model_at(times.iter().rev().find(|(_, time)| *time < last).unwrap().0));
    let refused = engine.recover_database("test", Moment::Time(last + 1_000_000), "later", false).unwrap_err();
    assert!(refused.to_string().contains("cannot say what was committed"), "{refused}");
    assert!(engine.database("later").is_err());
}

fn refusal(engine: &Engine, to: Moment, target: &str, replace: bool) -> String {
    match engine.recover_database("test", to, target, replace) {
        Ok(report) => panic!("the restore to {to:?} was not refused: {report}"),
        Err(e) => e.to_string(),
    }
}

/// Whatever a refused restore was building is gone, and the database is
/// as it was.
fn untouched(vfs: &MemVfs, engine: &Engine, model: &Model) {
    assert_eq!(engine.names(), ["test"]);
    let left: Vec<String> = vfs.list(Path::new(DIR)).unwrap().into_iter().filter(|file| file.contains("--")).collect();
    assert!(left.is_empty(), "left in the data directory: {left:?}");
    verify(&engine.database("test").unwrap(), model);
}

// R-O9
#[test]
fn a_restore_the_archive_cannot_make_whole_is_refused_and_changes_nothing() {
    let (vfs, model, made) = written(4, 30);
    let history = history(&vfs);
    let dir = Path::new(ARCHIVE).join(&history);
    let listing = archive_of(&vfs).list(&history).unwrap();
    let last = *listing.logs.last().unwrap();
    let late = made.iter().rev().find(|made| made.position.0 > listing.bases[1]).expect("a commit after the second base");
    {
        let engine = start(&vfs);
        // A database that exists is not written over unasked.
        let refused = refusal(&engine, Moment::End, "test", false);
        assert!(refused.contains("already exists"), "{refused}");
        // A position past the end of the archive.
        let refused = refusal(&engine, Moment::Position { generation: last + 5, offset: 0 }, "then", false);
        assert!(refused.contains("before the position asked for"), "{refused}");
        // A name that is no database's.
        assert!(engine.recover_database("other", Moment::End, "then", false).unwrap_err().to_string().contains("no history"));
        untouched(&vfs, &engine, &model);
    }
    // A log file gone from the middle: no moment after it, from a base
    // before it. A moment before it is restored as ever, and so is one
    // after the base that follows the gap.
    let gone = listing.bases[1] - 1;
    let broken = vfs.snapshot();
    broken.remove(&dir.join(format!("wal-{gone:010}"))).unwrap();
    {
        let engine = start(&broken);
        let early = made.iter().rev().find(|made| made.position.0 < gone).unwrap();
        verify(&restore(&engine, moment(early), "then"), &early.model);
        verify(&restore(&engine, moment(late), "then"), &late.model);
        engine.drop_database("then").unwrap();
        let lost = made.iter().find(|made| made.position.0 == gone).unwrap();
        let refused = refusal(&engine, moment(lost), "then", false);
        assert!(refused.contains(&format!("log file {gone} is missing")), "{refused}");
        untouched(&broken, &engine, &model);
        let unwhole = engine.archive_report("test", false).unwrap_err().to_string();
        assert!(unwhole.contains("not whole") && unwhole.contains(&format!("log file {gone} is missing")), "{unwhole}");
    }
    // A log file with a bit flipped: nothing after the flip, from a base
    // before it.
    let damaged = vfs.snapshot();
    let hit = made.iter().find(|made| made.position.0 == last && made.position.1 > 300).expect("a commit well into the last log file");
    damaged.corrupt(&dir.join(format!("wal-{last:010}")), 100);
    {
        let engine = start(&damaged);
        let refused = refusal(&engine, moment(hit), "then", false);
        assert!(refused.contains(&format!("log file {last} in the archive is damaged")), "{refused}");
        untouched(&damaged, &engine, &model);
        assert!(engine.archive_report("test", false).unwrap_err().to_string().contains("damaged"));
    }
    // A base with a bit flipped is not restored from.
    let damaged = vfs.snapshot();
    damaged.corrupt(&dir.join(format!("base-{:010}", listing.bases[1])), 8192 * 2 + 77);
    {
        let engine = start(&damaged);
        let refused = refusal(&engine, moment(late), "then", false);
        assert!(refused.contains("its bytes are not the ones that were written"), "{refused}");
        untouched(&damaged, &engine, &model);
        assert!(engine.archive_report("test", false).is_err());
    }
    // A moment before the oldest base the archive still has.
    let pruned = vfs.snapshot();
    pruned.remove(&dir.join(format!("base-{:010}", listing.bases[0]))).unwrap();
    {
        let engine = start(&pruned);
        let refused = refusal(&engine, moment(&made[1]), "then", false);
        assert!(refused.contains("was taken after it"), "{refused}");
        untouched(&pruned, &engine, &model);
    }
}

// R-O9
#[test]
fn a_restored_database_is_a_history_of_its_own() {
    let (vfs, model, made) = written(5, 20);
    let engine = start(&vfs);
    let was = history(&vfs);
    let then = &made[made.len() / 2];
    // Over the database itself: the commits after the moment are gone
    // from it, and still in the archive under the history they were made
    // in.
    engine.recover_database("test", moment(then), "test", true).unwrap();
    let db = engine.database("test").unwrap();
    verify(&db, &then.model);
    let now = db.archive_progress().unwrap().0;
    assert_ne!(now, was, "a restore begins a new history");
    // What is committed now goes to the new history, from a base of its
    // own, and the old one is as it was: whole, to its last commit.
    let (mut model_now, mut made_now) = (then.model.clone(), Vec::new());
    workload(&db, 6, 8, &mut model_now, &mut made_now, &mut || {});
    db.checkpoint().unwrap();
    let archive = archive_of(&vfs);
    assert!(archive.report(&was, false).unwrap().contains("whole"));
    assert!(archive.report(&now, false).unwrap().contains("whole"));
    // Each is restored by its own name; the database's name alone means
    // the history it has now.
    engine.recover_database(&was, Moment::End, "first", false).unwrap();
    verify(&engine.database("first").unwrap(), &model);
    engine.recover_database("test", Moment::End, "second", false).unwrap();
    verify(&engine.database("second").unwrap(), &model_now);
    // With the database gone, the name alone no longer says which.
    engine.drop_database("test").unwrap();
    let refused = engine.recover_database("test", Moment::End, "third", false).unwrap_err().to_string();
    assert!(refused.contains("histories of") && refused.contains(&was) && refused.contains(&now), "{refused}");
    // Dropped, it is in the archive up to its last commit.
    engine.recover_database(&now, Moment::End, "test", false).unwrap();
    verify(&engine.database("test").unwrap(), &model_now);
}

/// The disk as it stood before operation `at` of a journal kept from
/// `base` on, cut there the way `fate` says.
fn cut(base: &MemVfs, journal: &[database_engine::vfs::Entry], at: usize, fate: &mut dyn FnMut(&database_engine::vfs::Unsynced) -> Fate) -> MemVfs {
    let disk = base.snapshot();
    for entry in &journal[..at] {
        disk.apply(entry);
    }
    disk.crash_image_with(fate)
}

// R-O8, R-O9, R-X10
#[test]
fn a_kill_before_any_operation_loses_no_commit_from_the_database_or_from_its_archive() {
    let vfs = MemVfs::new();
    let base = vfs.snapshot();
    vfs.start_journal();
    let engine = start(&vfs);
    let db = create(&engine);
    let (mut model, mut made) = (Model::new(), vec![Made { model: Model::new(), position: db.log_position() }]);
    // How many operations had been made when each commit was acknowledged.
    let mut told: Vec<usize> = vec![vfs.journal_len()];
    workload(&db, 7, 24, &mut model, &mut made, &mut || told.push(vfs.journal_len()));
    db.checkpoint().unwrap();
    drop(db);
    engine.close().unwrap();
    let journal = vfs.take_journal();
    assert!(journal.iter().any(|entry| in_archive(&entry.op)), "the workload wrote to the archive");

    let mut rng = Rng::new(11);
    for at in told[0]..=journal.len() {
        // Nothing unsynced lost; all of it lost; and each change kept or
        // lost on its own, the last write torn.
        let last = journal[..at].iter().rposition(|entry| matches!(entry.op.kind, OpKind::Write { .. })).map(|i| journal[i].op.index);
        let seed = rng.next();
        let fates: [Box<dyn FnMut(&database_engine::vfs::Unsynced) -> Fate>; 3] = [
            Box::new(|_| Fate::Kept),
            Box::new(|_| Fate::Lost),
            Box::new(move |change| {
                let mut rng = Rng::new(seed ^ change.op);
                match (Some(change.op) == last, rng.below(3)) {
                    (true, _) => Fate::Torn(1),
                    (false, 0) => Fate::Lost,
                    _ => Fate::Kept,
                }
            }),
        ];
        for (way, mut fate) in fates.into_iter().enumerate() {
            let disk = cut(&base, &journal, at, &mut fate);
            let engine = start(&disk);
            let db = engine.database("test").unwrap_or_else(|e| panic!("killed before operation {at}, cut {way}: {e}"));
            // Every commit that was acknowledged by then, and at most
            // the one under way.
            let acknowledged = told.iter().filter(|ops| **ops <= at).count();
            let got = read_model(&db);
            let held = (acknowledged - 1..made.len().min(acknowledged + 1)).find(|i| made[*i].model == got).unwrap_or_else(|| panic!("killed before operation {at}, cut {way}: the database holds neither what was acknowledged nor that and the commit under way"));
            verify_indexes(&db, &got);
            // One more checkpoint, and the archive has it all: it is
            // whole, and restores the table as it stood at commits made
            // before the kill, the last of them among these.
            db.checkpoint().unwrap();
            let sample = [&made[rng.below(held as u64 + 1) as usize], &made[held]];
            verify_archive(&engine, &sample, &got);
        }
    }
}

/// The workload over an archive that fails as `plan` says, and what must
/// hold of it afterwards: nothing acknowledged is lost from the database
/// while the archive fails, and once it works again it is made whole from
/// the log files the data directory kept.
fn through_a_failing_archive(seed: u64, plan: impl FnMut(&Op) -> Fault + Send + 'static) {
    let vfs = MemVfs::new();
    let engine = start(&vfs);
    let db = create(&engine);
    let (mut model, mut made) = (Model::new(), vec![Made { model: Model::new(), position: db.log_position() }]);
    workload(&db, seed, 6, &mut model, &mut made, &mut || {});
    db.checkpoint().unwrap();
    vfs.set_plan(Some(Box::new(plan)));
    let before = made.len();
    workload(&db, seed + 1, 20, &mut model, &mut made, &mut || {});
    assert_eq!(made.len(), before + 20, "a failing archive refuses no commit");
    let _ = db.checkpoint();
    assert!(!db.has_failed(), "a failing archive does not fail the database");
    verify(&db, &model);
    // The power goes while the archive still fails: the database comes
    // back whole, and so does what it kept for the archive.
    let crashed = vfs.crash_image(seed);
    vfs.set_plan(None);
    for disk in [vfs.clone(), crashed] {
        let engine = start(&disk);
        let db = engine.database("test").unwrap();
        verify(&db, &model);
        db.checkpoint().unwrap();
        assert_eq!(db.archive_progress().unwrap().1.kept, 0, "nothing is kept once the archive has it");
        let sample: Vec<&Made> = made.iter().step_by(5).collect();
        verify_archive(&engine, &sample, &model);
    }
}

// R-O10
#[test]
fn an_archive_that_cannot_be_reached_keeps_the_log_in_the_data_directory() {
    through_a_failing_archive(20, |op| if in_archive(op) { Fault::Fail(std::io::ErrorKind::NotConnected) } else { Fault::None });

    // While it cannot be reached the log files stay, each checkpoint
    // adding its own, and `stats` says why.
    let vfs = MemVfs::new();
    let engine = start(&vfs);
    let db = create(&engine);
    let (mut model, mut made) = (Model::new(), Vec::new());
    vfs.set_plan(Some(Box::new(|op| if in_archive(op) { Fault::Fail(std::io::ErrorKind::NotConnected) } else { Fault::None })));
    workload(&db, 21, 10, &mut model, &mut made, &mut || {});
    db.checkpoint().unwrap();
    let kept = wal::list_logs(&vfs, Path::new(DIR), "test").unwrap();
    assert!(kept.len() >= 3, "the log files a checkpoint passed are kept: {kept:?}");
    let stats = db.stats();
    assert!(stats.contains("refused") && stats.contains("simulated fault"), "{stats}");
    // A database whose last commits are nowhere else is not dropped.
    let refused = engine.drop_database("test").unwrap_err().to_string();
    assert!(refused.contains("the only copy of its last commits"), "{refused}");
    verify(&engine.database("test").unwrap(), &model);
    // Started without an archive, the server lets the kept files go:
    // that is how an operator gives the archive up.
    drop(db);
    drop(engine);
    vfs.set_plan(None);
    let mut without = archiving();
    without.archive = None;
    let engine = try_open_with(&vfs, without).unwrap();
    verify(&engine.database("test").unwrap(), &model);
    assert_eq!(wal::list_logs(&vfs, Path::new(DIR), "test").unwrap().len(), 1);
}

// R-O10
#[test]
fn an_archive_with_no_room_keeps_the_log_in_the_data_directory() {
    through_a_failing_archive(30, |op| match &op.kind {
        OpKind::Write { len, .. } if in_archive(op) => Fault::Short(len / 2),
        OpKind::Create if in_archive(op) => Fault::Fail(std::io::ErrorKind::StorageFull),
        _ => Fault::None,
    });
}

// R-O10
#[test]
fn an_archive_whose_every_operation_fails_once_is_whole_in_the_end() {
    // Count the operations a clean run makes on the archive, then fail
    // each of them in a run of its own.
    let count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let counting = count.clone();
    through_a_failing_archive(40, move |op| {
        if in_archive(op) {
            counting.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Fault::None
    });
    let operations = count.load(std::sync::atomic::Ordering::Relaxed);
    assert!(operations > 20, "the workload wrote to the archive {operations} times");
    for failing in 0..operations {
        let mut seen = 0;
        through_a_failing_archive(40, move |op| {
            if !in_archive(op) {
                return Fault::None;
            }
            seen += 1;
            match (&op.kind, seen - 1 == failing) {
                (_, false) => Fault::None,
                // A sync that fails as a real one does: what it was to
                // make durable is on the disk or is not.
                (OpKind::Sync | OpKind::SyncDir, true) => Fault::SyncLoses(failing),
                (_, true) => Fault::Fail(std::io::ErrorKind::Other),
            }
        });
    }
}

// R-O9
#[test]
fn a_restore_cut_before_any_of_its_operations_leaves_the_database_as_it_was() {
    let (vfs, model, made) = written(50, 16);
    let then = &made[made.len() / 2];
    let base = vfs.snapshot();
    vfs.start_journal();
    {
        let engine = start(&vfs);
        engine.recover_database("test", moment(then), "test", true).unwrap();
        engine.recover_database("test", Moment::End, "copy", false).unwrap();
        drop(engine);
    }
    let journal = vfs.take_journal();
    let mut rng = Rng::new(51);
    for at in 0..=journal.len() {
        let seed = rng.next();
        let fates: [Box<dyn FnMut(&database_engine::vfs::Unsynced) -> Fate>; 3] = [
            Box::new(|_| Fate::Kept),
            Box::new(|_| Fate::Lost),
            Box::new(move |change| if Rng::new(seed ^ change.op).chance(2) { Fate::Lost } else { Fate::Kept }),
        ];
        for (way, mut fate) in fates.into_iter().enumerate() {
            let disk = cut(&base, &journal, at, &mut fate);
            let engine = try_open_with(&disk, archiving()).unwrap_or_else(|e| panic!("cut before operation {at} of a restore, way {way}: {e}"));
            // The database is what it was or what it was restored to,
            // whole either way, and nothing half built is left.
            let db = engine.database("test").unwrap();
            let got = read_model(&db);
            assert!(got == model || got == then.model, "cut before operation {at} of a restore, way {way}: the database is neither as it was nor as it was restored");
            verify_indexes(&db, &got);
            if let Ok(copy) = engine.database("copy") {
                verify(&copy, &then.model);
            }
            let left: Vec<String> = disk.list(Path::new(DIR)).unwrap().into_iter().filter(|file| file.contains("--")).collect();
            assert!(left.is_empty(), "cut before operation {at} of a restore, way {way}: left {left:?}");
            // The archive was only read: the first history restores still.
            let first = engine.archive_report("", false).unwrap();
            assert!(first.contains("whole"), "{first}");
        }
    }
}

// R-O8
#[test]
fn a_log_that_is_archived_says_when_each_commit_was_made_and_an_older_build_refuses_it() {
    let vfs = MemVfs::new();
    let engine = start(&vfs);
    let db = create(&engine);
    let before = database_protocol::time::now_micros();
    let (mut model, mut made) = (Model::new(), Vec::new());
    workload(&db, 60, 3, &mut model, &mut made, &mut || {});
    let after = database_protocol::time::now_micros();
    let (generation, _) = db.log_position();
    let uuid = db.store().meta().uuid;
    let mut records = Vec::new();
    wal::replay(&vfs, Path::new(DIR), "test", &uuid, generation, &mut |commit| {
        let time = commit.time.expect("a time");
        assert!((before..=after).contains(&time));
        records.push(commit.changes.to_vec());
        Ok(())
    })
    .unwrap();
    assert!(!records.is_empty());
    // A build from before the stamp gives a record's bytes, as they lie
    // in the file, to the code that applies changes: which takes the
    // stamp for a change it does not know, and stops. It never reads a
    // time as a change.
    let file = vfs.open(&wal::wal_path(Path::new(DIR), "test", generation), false).unwrap();
    let mut raw = vec![0u8; file.len().unwrap() as usize];
    database_engine::vfs::read_exact_at(&*file, &mut raw, 0).unwrap();
    let first = records[0].as_slice();
    let at = raw.windows(first.len()).position(|window| window == first).expect("the record's changes are in the file");
    let stamped = &raw[at - wal::STAMP_LEN..at + first.len()];
    let mut tx = database_engine::write::Tx::new(database_engine::store::Writer::new(db.store().clone()), (**db.begin_read().catalog()).clone(), 0);
    let refused = tx.replay(stamped).unwrap_err();
    assert!(matches!(&refused, Error::Corrupt(what) if what.contains("unknown operation 241")), "{refused}");
    tx.writer.rollback();

    // Without an archive nothing is stamped: the log is byte for byte
    // what it was before there were stamps.
    let plain = MemVfs::new();
    let mut options = archiving();
    options.archive = None;
    let engine = try_open_with(&plain, options).unwrap();
    let db = create(&engine);
    let (mut model, mut made) = (Model::new(), Vec::new());
    workload(&db, 60, 3, &mut model, &mut made, &mut || {});
    let (generation, _) = db.log_position();
    wal::replay(&plain, Path::new(DIR), "test", &db.store().meta().uuid, generation, &mut |commit| {
        assert_eq!(commit.time, None);
        Ok(())
    })
    .unwrap();
    assert!(db.stats().lines().all(|line| !line.starts_with("archive")));
    assert!(matches!(db.base_copy(), Err(Error::Db(e)) if e.code == ErrorCode::InvalidRequest));
}

// R-O8
#[test]
fn an_archive_begun_on_a_database_with_rows_begins_with_a_base_of_them() {
    // A database written without an archive, then started with one.
    let vfs = MemVfs::new();
    let mut without = archiving();
    without.archive = None;
    let (mut model, mut made) = (Model::new(), Vec::new());
    {
        let engine = try_open_with(&vfs, without.clone()).unwrap();
        let db = create(&engine);
        workload(&db, 70, 10, &mut model, &mut made, &mut || {});
    }
    {
        let engine = start(&vfs);
        let db = engine.database("test").unwrap();
        workload(&db, 71, 5, &mut model, &mut made, &mut || {});
        db.checkpoint().unwrap();
        verify_archive(&engine, &[made.last().unwrap()], &model);
        // What was committed before the archive began cannot be had.
        let refused = refusal(&engine, Moment::Position { generation: 1, offset: 0 }, "before", false);
        assert!(refused.contains("was taken after it"), "{refused}");
    }
    // Stopped, run without the archive for a while, and started with it
    // again: the log between is in no archive, and a base begins anew.
    // The archive then says which moments it cannot give, until what is
    // older than the new base is removed from it.
    {
        let engine = try_open_with(&vfs, without).unwrap();
        let db = engine.database("test").unwrap();
        workload(&db, 72, 5, &mut model, &mut made, &mut || {});
        db.checkpoint().unwrap();
    }
    let engine = start(&vfs);
    let db = engine.database("test").unwrap();
    workload(&db, 73, 5, &mut model, &mut made, &mut || {});
    db.checkpoint().unwrap();
    verify(&restore(&engine, Moment::End, "then"), &model);
    verify(&restore(&engine, moment(made.last().unwrap()), "then"), &model);
    let unwhole = engine.archive_report("test", false).unwrap_err().to_string();
    assert!(unwhole.contains("not whole") && unwhole.contains("is missing"), "{unwhole}");
    let (history, archive) = (history(&vfs), archive_of(&vfs));
    let listing = archive.list(&history).unwrap();
    let newest = *listing.bases.last().unwrap();
    for (kind, generation) in listing.bases.iter().map(|g| ("base", g)).chain(listing.logs.iter().map(|g| ("wal", g))) {
        if *generation < newest {
            vfs.remove(&Path::new(ARCHIVE).join(&history).join(format!("{kind}-{generation:010}"))).unwrap();
        }
    }
    let report = engine.archive_report("test", true).unwrap();
    assert!(report.contains("whole"), "{report}");
    // Each commit is listed by where it ends, which is what a restore
    // is asked for.
    let (generation, offset) = made.last().unwrap().position;
    assert!(report.contains(&position_text(generation, offset)), "{report}");
}
