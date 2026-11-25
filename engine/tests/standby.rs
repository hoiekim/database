//! A database that follows another, through the faults a disk can show
//! it and the ways its primary can go away.
//!
//! Two engines, each on a simulated disk of its own: a primary that
//! commits, checkpoints and restarts, and a standby that is given the
//! primary's log as a server would give it, in pieces of any size. What
//! must hold:
//!
//! - what the standby's readers see is the primary as of one of its
//!   commits, never part of one, and never an earlier one than they saw
//!   before (R-O11);
//! - killed before any operation it makes on its disk, the standby
//!   starts as a prefix of the primary's history and is brought up to
//!   it by being sent the rest (R-O12);
//! - promoted, it is a history of its own that takes writes; killed
//!   before any operation of its promotion, it is a standby still or a
//!   promoted database, and never one that would take both its
//!   primary's log and writes of its own (R-O13).
//!
//! The same with real servers and connections cut on purpose is
//! `server/tests/standby.rs`.

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use common::*;
use database_engine::db::{Database, LogRead};
use database_engine::engine::{Engine, EngineOptions};
use database_engine::error::{Error, Result};
use database_engine::vfs::{Entry, Fate, MemVfs, Rng, Unsynced};
use database_engine::wal;
use database_protocol::error::ErrorCode;
use database_protocol::query::*;

/// A primary that keeps the log its checkpoints pass, for a standby
/// that is behind them; and nothing checkpoints by itself.
fn primary_options() -> EngineOptions {
    let mut options = options();
    options.db.checkpoint_bytes = u64::MAX;
    options.db.keep_log_bytes = 64 << 20;
    options
}

fn standby_options() -> EngineOptions {
    let mut options = options();
    options.db.checkpoint_bytes = u64::MAX;
    options
}

fn start(vfs: &MemVfs, options: EngineOptions) -> Engine {
    try_open_with(vfs, options).expect("the engine starts")
}

/// One change to the table, in a transaction of its own.
fn change(db: &Arc<Database>, rng: &mut Rng, model: &mut Model, next_id: &mut i64) {
    let mut txn = db.begin_write(WAIT, false).unwrap();
    for _ in 0..1 + rng.below(3) {
        let existing: Vec<i64> = model.keys().copied().collect();
        match rng.below(4) {
            0 | 1 => {
                let item = random_item(rng, *next_id);
                txn.insert(&insert_of(*next_id, &item), None).unwrap();
                model.insert(*next_id, item);
                *next_id += 1;
            }
            2 if !existing.is_empty() => {
                let id = existing[rng.below(existing.len() as u64) as usize];
                let owner = rng.below(4) as i64;
                txn.update(&Update::table("items").key([id]).set("owner", owner), None).unwrap();
                model.get_mut(&id).unwrap().owner = owner;
            }
            3 if !existing.is_empty() => {
                let id = existing[rng.below(existing.len() as u64) as usize];
                txn.delete(&Delete::from("items").key([id]), None).unwrap();
                model.remove(&id);
            }
            _ => {}
        }
    }
    txn.commit().unwrap();
}

/// Commits `commits` changes on the primary, with a checkpoint now and
/// then, adding what the table held after each to `made`.
fn workload(db: &Arc<Database>, seed: u64, commits: usize, model: &mut Model, made: &mut Vec<Model>) {
    let mut rng = Rng::new(seed);
    let mut next_id = model.keys().next_back().map_or(1, |id| id + 1);
    for n in 0..commits {
        change(db, &mut rng, model, &mut next_id);
        made.push(model.clone());
        if n % 6 == 5 {
            db.checkpoint().unwrap();
        }
    }
}

/// Begins a standby of the primary's `test` on `disk`, from a base the
/// primary gives, as a server passes one on.
fn seed(primary: &Arc<Database>, disk: &MemVfs) -> Result<Engine> {
    let engine = try_open_with(disk, standby_options())?;
    let mut seed = engine.seed("test")?;
    let mut fed = Ok(());
    let (_, _pin) = primary.base_to(&mut |bytes| {
        if fed.is_ok() {
            fed = seed.feed(bytes);
        }
        Ok(())
    })?;
    fed?;
    seed.finish(&primary.uuid())?;
    Ok(engine)
}

/// Sends the standby the primary's log from where it has got to, in
/// pieces of at most `most` bytes, until it has all that is on the
/// primary's disk. `after` is called after each piece.
fn send(primary: &Arc<Database>, standby: &Arc<Database>, most: &mut dyn FnMut() -> usize, after: &mut dyn FnMut()) -> Result<()> {
    loop {
        let (generation, offset) = standby.followed().expect("it follows").received;
        match primary.read_log(generation, offset, most())? {
            LogRead::Bytes(bytes) => standby.receive(generation, offset, &bytes)?,
            LogRead::Next => standby.receive(generation + 1, wal::FIRST_RECORD, &[])?,
            LogRead::Wait => return Ok(()),
        }
        after();
    }
}

fn send_all(primary: &Arc<Database>, standby: &Arc<Database>) {
    send(primary, standby, &mut || 1 << 20, &mut || {}).expect("the standby takes its primary's log");
}

/// Which of the primary's commits the standby is at: the table as it
/// reads is the table as it stood after that one. Later ones are tried
/// first, since a commit may leave the table as an earlier one did.
fn commit_of(standby: &Arc<Database>, made: &[Model], what: &str) -> usize {
    let got = read_model(standby);
    made.iter().rposition(|model| *model == got).unwrap_or_else(|| panic!("{what}: the standby holds what the primary held at no commit"))
}

/// A primary with a table and some commits, and a standby seeded from
/// it in the middle of them.
struct Pair {
    primary_disk: MemVfs,
    primary: Engine,
    standby_disk: MemVfs,
    standby: Engine,
    model: Model,
    /// The table after each commit, from its creation on.
    made: Vec<Model>,
}

fn pair(seed_of: u64) -> Pair {
    let primary_disk = MemVfs::new();
    let primary = start(&primary_disk, primary_options());
    let db = create(&primary);
    let (mut model, mut made) = (Model::new(), vec![Model::new()]);
    workload(&db, seed_of, 8, &mut model, &mut made);
    let standby_disk = MemVfs::new();
    let standby = seed(&db, &standby_disk).expect("the standby is seeded");
    Pair { primary_disk, primary, standby_disk, standby, model, made }
}

impl Pair {
    fn dbs(&self) -> (Arc<Database>, Arc<Database>) {
        (self.primary.database("test").unwrap(), self.standby.database("test").unwrap())
    }
}

// R-O11
#[test]
fn a_standby_reads_its_primary_as_of_one_commit_and_never_an_earlier_one_than_before() {
    let mut pair = pair(1);
    let (primary, standby) = pair.dbs();
    // Seeded from a base, it is the primary as of the base.
    assert_eq!(commit_of(&standby, &pair.made, "seeded"), pair.made.len() - 1);
    workload(&primary, 2, 30, &mut pair.model, &mut pair.made);
    // The log arrives a few bytes at a time: records come in parts, and
    // several at once.
    let mut rng = Rng::new(3);
    let mut at = commit_of(&standby, &pair.made, "before any log");
    let made = pair.made.clone();
    let reader = standby.clone();
    send(&primary, &standby, &mut || 1 + rng.below(300) as usize, &mut || {
        let now = commit_of(&reader, &made, "while the log arrives");
        assert!(now >= at, "the standby went back from commit {at} to commit {now}");
        at = now;
    })
    .unwrap();
    verify(&standby, &pair.model);
    // It says where it is: at the end of what its primary has on disk.
    assert_eq!(standby.followed().unwrap().received, primary.synced_position());
    // And it takes no writes.
    assert!(matches!(standby.begin_write(WAIT, false), Err(Error::Db(e)) if e.code == ErrorCode::ReadOnly));
    assert!(matches!(standby.begin_fork(), Err(Error::Db(e)) if e.code == ErrorCode::ReadOnly));
    // A reader that began before more log arrived keeps what it saw.
    let before = standby.begin_read();
    let seen = before.query(&Query::table("items"), None).unwrap().rows.len();
    workload(&primary, 4, 10, &mut pair.model, &mut pair.made);
    send_all(&primary, &standby);
    assert_eq!(before.query(&Query::table("items"), None).unwrap().rows.len(), seen);
    drop(before);
    verify(&standby, &pair.model);
}

/// The disk as it stood before operation `at` of a journal kept from
/// `base` on, cut there the way `fate` says.
fn cut(base: &MemVfs, journal: &[Entry], at: usize, fate: &mut Cut) -> MemVfs {
    let disk = base.snapshot();
    for entry in &journal[..at] {
        disk.apply(entry);
    }
    disk.crash_image_with(fate)
}

/// What a power cut leaves of each change no sync had made durable.
type Cut = Box<dyn FnMut(&Unsynced) -> Fate>;

/// Three ways a power cut differs: nothing unsynced lost, all of it
/// lost, and each change kept or lost on its own with the last write
/// torn.
fn cuts(journal: &[Entry], at: usize, seed: u64) -> Vec<Cut> {
    let last = journal[..at].iter().rposition(|entry| matches!(entry.op.kind, database_engine::vfs::OpKind::Write { .. })).map(|i| journal[i].op.index);
    vec![
        Box::new(|_| Fate::Kept),
        Box::new(|_| Fate::Lost),
        Box::new(move |change| match (Some(change.op) == last, Rng::new(seed ^ change.op).below(3)) {
            (true, _) => Fate::Torn(1),
            (false, 0) => Fate::Lost,
            _ => Fate::Kept,
        }),
    ]
}

// R-O12
#[test]
fn a_standby_killed_before_any_operation_of_applying_starts_as_a_prefix_and_is_brought_up() {
    let mut pair = pair(10);
    let (primary, standby) = pair.dbs();
    workload(&primary, 11, 24, &mut pair.model, &mut pair.made);
    let base = pair.standby_disk.snapshot();
    pair.standby_disk.start_journal();
    let mut rng = Rng::new(12);
    send(&primary, &standby, &mut || 1 + rng.below(4_000) as usize, &mut || {}).unwrap();
    let journal = pair.standby_disk.take_journal();
    assert!(journal.len() > 60, "applying made {} operations on the standby's disk", journal.len());
    let mut least = usize::MAX;
    for at in 0..=journal.len() {
        for (way, mut fate) in cuts(&journal, at, rng.next()).into_iter().enumerate() {
            let what = format!("killed before operation {at} of applying, cut {way}");
            let disk = cut(&base, &journal, at, &mut fate);
            let engine = try_open_with(&disk, standby_options()).unwrap_or_else(|e| panic!("{what}: {e}"));
            let db = engine.database("test").unwrap();
            // A standby still, at some commit of the primary's.
            assert!(db.followed().is_some(), "{what}: it no longer follows");
            let held = commit_of(&db, &pair.made, &what);
            least = least.min(held);
            verify_indexes(&db, &pair.made[held]);
            // Sent the rest from where it says it is, it is the primary.
            send_all(&primary, &db);
            verify(&db, &pair.model);
        }
    }
    assert!(least < pair.made.len() - 10, "the kills left the standby at many places");
}

// R-O12
#[test]
fn a_standby_follows_its_primary_through_the_primarys_crash_and_restart() {
    let mut pair = pair(20);
    for round in 0..4 {
        let (primary, standby) = pair.dbs();
        workload(&primary, 21 + round, 9, &mut pair.model, &mut pair.made);
        // The standby has part of what was committed when the primary
        // dies; more was committed after that.
        let mut pieces = 0;
        let _ = send(&primary, &standby, &mut || 700, &mut || pieces += 1);
        workload(&primary, 31 + round, 5, &mut pair.model, &mut pair.made);
        drop(primary);
        // A crash, or a stop in good order: either way the primary
        // begins another log file, and the one the standby was reading
        // has been passed by a checkpoint.
        if round % 2 == 0 {
            pair.primary_disk = pair.primary_disk.crash_image(round);
            pair.primary = start(&pair.primary_disk, primary_options());
        } else {
            let stopped = std::mem::replace(&mut pair.primary, start(&MemVfs::new(), primary_options()));
            stopped.close().unwrap();
            pair.primary = start(&pair.primary_disk, primary_options());
        }
        let (primary, standby) = pair.dbs();
        verify(&primary, &pair.model);
        send_all(&primary, &standby);
        verify(&standby, &pair.model);
    }
}

// R-O12
#[test]
fn a_standby_behind_what_its_primary_keeps_is_told_so_and_begins_again_from_a_base() {
    let primary_disk = MemVfs::new();
    // A primary that keeps no log beyond its checkpoints.
    let mut options = primary_options();
    options.db.keep_log_bytes = 0;
    let primary = start(&primary_disk, options);
    let db = create(&primary);
    let (mut model, mut made) = (Model::new(), vec![Model::new()]);
    workload(&db, 40, 4, &mut model, &mut made);
    let standby_disk = MemVfs::new();
    let standby = seed(&db, &standby_disk).unwrap();
    let following = standby.database("test").unwrap();
    // While the standby is being sent the log, the files it has not had
    // yet stay, whatever checkpoints pass them.
    let (generation, _) = following.followed().unwrap().received;
    let pin = db.pin_log(generation);
    workload(&db, 41, 14, &mut model, &mut made);
    send_all(&db, &following);
    verify(&following, &model);
    // Away, it holds nothing back: the primary's checkpoints remove
    // what they pass, and it is told that it is behind.
    drop(pin);
    workload(&db, 42, 14, &mut model, &mut made);
    let behind = send(&db, &following, &mut || 1 << 20, &mut || {}).unwrap_err();
    assert!(matches!(&behind, Error::Db(e) if e.code == ErrorCode::Behind), "{behind}");
    // It is what it was, a commit of the primary's, and reads.
    commit_of(&following, &made, "behind");
    // Dropped and seeded again, it is the primary once more.
    drop(following);
    standby.drop_database("test").unwrap();
    drop(standby);
    let standby = seed(&db, &standby_disk).unwrap();
    let following = standby.database("test").unwrap();
    send_all(&db, &following);
    verify(&following, &model);
}

// R-O12
#[test]
fn a_standby_is_begun_from_the_archive_and_reads_there_what_its_primary_no_longer_keeps() {
    // One disk for the primary's files and the archive, which the
    // standby reads as well: a mount both servers have.
    let shared = MemVfs::new();
    let mut options = primary_options();
    options.db.keep_log_bytes = 0;
    options.archive = Some(PathBuf::from("/archive"));
    let primary = start(&shared, options);
    let db = create(&primary);
    let (mut model, mut made) = (Model::new(), vec![Model::new()]);
    workload(&db, 50, 20, &mut model, &mut made);
    db.checkpoint().unwrap();
    // The standby's engine has its own data directory and that archive.
    let mut options = standby_options();
    options.archive = Some(PathBuf::from("/archive"));
    let standby = database_engine::engine::Engine::open_with(Arc::new(shared.clone()), std::path::Path::new("/standby"), options, None).unwrap();
    assert!(standby.seed_from_archive("test", &db.uuid()).unwrap());
    let following = standby.database("test").unwrap();
    assert!(following.followed().is_some());
    verify(&following, &model);
    // It keeps no archive of its own: the history is the primary's.
    assert!(following.archive_progress().is_none());
    // More is committed and checkpointed away while it is not looking.
    workload(&db, 51, 20, &mut model, &mut made);
    db.checkpoint().unwrap();
    workload(&db, 52, 3, &mut model, &mut made);
    let behind = send(&db, &following, &mut || 1 << 20, &mut || {}).unwrap_err();
    assert!(matches!(&behind, Error::Db(e) if e.code == ErrorCode::Behind), "{behind}");
    // The archive has what the primary let go, and the primary the rest.
    assert!(standby.catch_up_from_archive("test").unwrap());
    send_all(&db, &following);
    verify(&following, &model);
    // An archive without that history begins nothing.
    assert!(!standby.seed_from_archive("other", &db.uuid()).unwrap());
}

// R-O13
#[test]
fn a_standby_started_without_its_primary_is_a_standby_still_until_it_is_promoted() {
    let mut pair = pair(60);
    let (primary, standby) = pair.dbs();
    workload(&primary, 61, 12, &mut pair.model, &mut pair.made);
    send_all(&primary, &standby);
    let was = standby.uuid();
    drop(standby);
    // Started again by itself, as any server is started: it reads, and
    // refuses writes, restores over it and a base of it.
    let stopped = std::mem::replace(&mut pair.standby, start(&MemVfs::new(), standby_options()));
    stopped.close().unwrap();
    pair.standby = start(&pair.standby_disk, standby_options());
    let standby = pair.standby.database("test").unwrap();
    verify(&standby, &pair.model);
    assert!(matches!(standby.begin_write(WAIT, false), Err(Error::Db(e)) if e.code == ErrorCode::ReadOnly));
    assert!(matches!(standby.base_to(&mut |_| Ok(())), Err(Error::Db(e)) if e.code == ErrorCode::ReadOnly));
    // And is sent the log from where it was, as before.
    workload(&primary, 62, 6, &mut pair.model, &mut pair.made);
    send_all(&primary, &standby);
    verify(&standby, &pair.model);
    drop(standby);

    // Promoted, it is a database of its own: it takes writes, under
    // another uuid, and its primary's log is no longer for it.
    let report = pair.standby.promote("test").unwrap();
    assert!(report.contains("promoted at"), "{report}");
    let promoted = pair.standby.database("test").unwrap();
    assert!(promoted.followed().is_none());
    assert_ne!(promoted.uuid(), was);
    verify(&promoted, &pair.model);
    let mut own = pair.model.clone();
    workload(&promoted, 63, 6, &mut own, &mut Vec::new());
    verify(&promoted, &own);
    assert!(matches!(promoted.receive(1, wal::FIRST_RECORD, &[1]), Err(Error::Db(e)) if e.code == ErrorCode::InvalidRequest));
    assert!(pair.standby.promote("test").is_err(), "there is nothing left to promote");
    // The primary went its own way meanwhile, and a place in the
    // promoted database's log is no place in the primary's.
    let (generation, offset) = promoted.synced_position();
    assert!(primary.read_log(generation + 3, offset, 100).is_err());
    // Stopped and started, the promoted database is what it was.
    drop(promoted);
    let stopped = std::mem::replace(&mut pair.standby, start(&MemVfs::new(), standby_options()));
    stopped.close().unwrap();
    pair.standby = start(&pair.standby_disk, standby_options());
    let promoted = pair.standby.database("test").unwrap();
    assert!(promoted.followed().is_none());
    verify(&promoted, &own);
}

// R-O13
#[test]
fn a_promotion_killed_before_any_of_its_operations_leaves_a_standby_or_a_promoted_database() {
    let mut pair = pair(70);
    let (primary, standby) = pair.dbs();
    workload(&primary, 71, 14, &mut pair.model, &mut pair.made);
    // The standby has part of the log when it is promoted, the last
    // bytes of it part of a record: what is whole is what it keeps.
    for _ in 0..9 {
        let (generation, offset) = standby.followed().unwrap().received;
        match primary.read_log(generation, offset, 900).unwrap() {
            LogRead::Bytes(bytes) => standby.receive(generation, offset, &bytes).unwrap(),
            LogRead::Next => standby.receive(generation + 1, wal::FIRST_RECORD, &[]).unwrap(),
            LogRead::Wait => unreachable!("the primary has more"),
        }
    }
    let held = commit_of(&standby, &pair.made, "before the promotion");
    assert!(held < pair.made.len() - 1);
    let was = standby.uuid();
    drop(standby);
    let base = pair.standby_disk.snapshot();
    pair.standby_disk.start_journal();
    pair.standby.promote("test").unwrap();
    let journal = pair.standby_disk.take_journal();
    let mut rng = Rng::new(72);
    let (mut standbys, mut promoted) = (0, 0);
    for at in 0..=journal.len() {
        for (way, mut fate) in cuts(&journal, at, rng.next()).into_iter().enumerate() {
            let what = format!("killed before operation {at} of a promotion, cut {way}");
            let disk = cut(&base, &journal, at, &mut fate);
            let engine = try_open_with(&disk, standby_options()).unwrap_or_else(|e| panic!("{what}: {e}"));
            let db = engine.database("test").unwrap();
            // Either way it holds what it held, whole.
            verify(&db, &pair.made[held]);
            if db.followed().is_some() {
                // A standby still, of the history it was of, and its
                // copy of the log is still its primary's: sent the
                // rest, it is the primary.
                standbys += 1;
                assert_eq!(db.uuid(), was, "{what}");
                send_all(&primary, &db);
                verify(&db, &pair.model);
            } else {
                // Promoted: a history of its own, that takes writes.
                promoted += 1;
                assert_ne!(db.uuid(), was, "{what}");
                let mut own = pair.made[held].clone();
                workload(&db, 73, 3, &mut own, &mut Vec::new());
                verify(&db, &own);
            }
        }
    }
    assert!(standbys > 0 && promoted > 0, "{standbys} standbys and {promoted} promoted");
}

// R-O12
#[test]
fn a_seeding_killed_before_any_of_its_operations_leaves_no_database_or_a_standby() {
    let primary_disk = MemVfs::new();
    let primary = start(&primary_disk, primary_options());
    let db = create(&primary);
    let (mut model, mut made) = (Model::new(), vec![Model::new()]);
    workload(&db, 80, 10, &mut model, &mut made);
    let disk = MemVfs::new();
    let base = disk.snapshot();
    disk.start_journal();
    drop(seed(&db, &disk).unwrap());
    let journal = disk.take_journal();
    let mut rng = Rng::new(81);
    for at in 0..=journal.len() {
        for (way, mut fate) in cuts(&journal, at, rng.next()).into_iter().enumerate() {
            let what = format!("killed before operation {at} of seeding, cut {way}");
            let disk = cut(&base, &journal, at, &mut fate);
            let engine = try_open_with(&disk, standby_options()).unwrap_or_else(|e| panic!("{what}: {e}"));
            match engine.database("test") {
                // Not there: seeded again, it is.
                Err(_) => {
                    assert!(engine.names().is_empty(), "{what}");
                    drop(engine);
                    let engine = seed(&db, &disk).unwrap_or_else(|e| panic!("{what}: seeding again: {e}"));
                    verify(&engine.database("test").unwrap(), &model);
                }
                // There: whole, following, and the primary as of the base.
                Ok(standby) => {
                    assert!(standby.followed().is_some(), "{what}: a database that takes writes");
                    verify(&standby, &model);
                    send_all(&db, &standby);
                }
            }
        }
    }
}
