//! The engine against a model, through crashes.
//!
//! A random workload runs on a file system that can show what a power cut
//! at any moment would leave on disk. After each simulated cut the
//! database is opened again and must hold exactly what was committed: all
//! of every acknowledged transaction, none of any other, with every index
//! and the free list consistent.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use database_engine::db::{Database, Options};
use database_engine::engine::{Engine, EngineOptions};
use database_engine::store::Limits;
use database_engine::vfs::{MemVfs, Rng};
use database_protocol::expr::*;
use database_protocol::message::Ddl;
use database_protocol::query::*;
use database_protocol::schema::*;
use database_protocol::value::{Type, Value};

const DIR: &str = "/data";
const WAIT: Duration = Duration::from_secs(5);

fn options() -> EngineOptions {
    // Small limits, so that pages are written early, evicted and read
    // back, and checkpoints happen, within a short test; and full-text
    // segments of a handful of rows, so that they are made, merged and
    // cleaned of dead rows all the time.
    database_engine::fts::set_pending_bytes(400);
    EngineOptions { cache_bytes: 64 * 8192, db: Options { sync: true, limits: Limits { max_dirty: 24, max_private: 24 }, checkpoint_bytes: 64 * 1024 } }
}

fn open(vfs: &MemVfs) -> Engine {
    let engine = Engine::open_with(Arc::new(vfs.clone()), Path::new(DIR), options(), None).expect("the engine opens");
    // Writers leave the merging of full-text segments to be done beside
    // them. Here it is done by hand (`Merger`), at moments that a seed
    // decides, so that a run can be had again; one test lets the
    // engine's own thread do it.
    engine.pause_merging(true);
    engine
}

/// Merges full-text segments beside the workload, now and then: between
/// two transactions, with a fork standing open or not, before a
/// checkpoint and after, and into a power cut.
struct Merger {
    rng: Rng,
    /// Merges made and put in place.
    made: u64,
}

impl Merger {
    fn new(seed: u64) -> Merger {
        Merger { rng: Rng::new(seed ^ 0x0003_E26E), made: 0 }
    }

    fn step(&mut self, db: &Arc<Database>) {
        match self.rng.below(4) {
            // One merge, so that what else is due stays due through
            // whatever comes next.
            0 => self.made += u64::from(db.merge_text_segments(&|| false).unwrap_or(false)),
            // Everything that is due. A failure is the disk gone: the
            // next transaction finds that out.
            1 => {
                while db.merge_text_segments(&|| false).unwrap_or(false) {
                    self.made += 1;
                }
            }
            _ => {}
        }
    }
}

fn items() -> TableDef {
    TableDef::new("items")
        .column(ColumnDef::new("id", Type::Int))
        .column(ColumnDef::new("owner", Type::Int))
        .column(ColumnDef::new("name", Type::Text))
        .column(ColumnDef::new("body", Type::Text).default(""))
        .column(ColumnDef::new("tags", Type::list(Type::Text)).default(Vec::<String>::new()))
        .primary_key(["id"])
        .unique("items_name", ["name"])
        .index(IndexDef::new("items_owner").on(["owner", "id"]))
        .index(IndexDef::new("items_tags").on(["owner"]).on_each(col("tags")))
        .index(IndexDef::new("items_search").on(["owner"]).full_text(TextConfig::Simple, [(col("body"), 1.0)]))
}

/// What a row of `items` holds, as the model keeps it.
#[derive(Clone, Debug, PartialEq)]
struct Item {
    owner: i64,
    name: String,
    body: String,
    tags: Vec<String>,
}

type Model = BTreeMap<i64, Item>;

fn insert_of(id: i64, item: &Item) -> Insert {
    Insert::into("items").set("id", id).set("owner", item.owner).set("name", item.name.as_str()).set("body", item.body.as_str()).set("tags", item.tags.clone())
}

fn random_item(rng: &mut Rng, id: i64) -> Item {
    let words = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"];
    let body = match rng.below(12) {
        // Now and then a body far larger than a page.
        0 => words[rng.below(6) as usize].repeat(2_000 + rng.below(6_000) as usize),
        _ => (0..rng.below(12)).map(|_| words[rng.below(6) as usize]).collect::<Vec<_>>().join(" "),
    };
    let tags = (0..rng.below(4)).map(|_| format!("t{}", rng.below(5))).collect();
    Item { owner: rng.below(4) as i64, name: format!("item-{id}-{}", rng.below(1000)), body, tags }
}

/// The table as it is, read by primary key.
fn read_model(db: &Arc<Database>) -> Model {
    let all = db.begin_read().query(&Query::table("items"), None).unwrap();
    all.rows
        .iter()
        .map(|r| {
            let (Value::Int(id), Value::Int(owner), Value::Text(name), Value::Text(body), Value::List(tags)) = (&r[0], &r[1], &r[2], &r[3], &r[4]) else { panic!("unexpected row {r:?}") };
            let tags = tags.iter().map(|t| t.to_string()).collect();
            (*id, Item { owner: *owner, name: name.clone(), body: body.clone(), tags })
        })
        .collect()
}

/// Reads the whole table and compares it with the model, by primary key
/// and through each index.
fn verify(db: &Arc<Database>, model: &Model) {
    let got = read_model(db);
    let txn = db.begin_read();
    assert_eq!(got.len(), model.len(), "row count");
    for (id, item) in model {
        assert_eq!(got.get(id), Some(item), "row {id}");
    }
    for owner in 0..4i64 {
        let by_owner = txn.query(&Query::table("items").index("items_owner", KeyRange::eq([owner])).columns(["id"]), None).unwrap();
        let ids: Vec<i64> = by_owner.rows.iter().map(|r| if let Value::Int(n) = r[0] { n } else { panic!() }).collect();
        let want: Vec<i64> = model.iter().filter(|(_, i)| i.owner == owner).map(|(id, _)| *id).collect();
        assert_eq!(ids, want, "index by owner {owner}");

        let tagged = txn.query(&Query::table("items").index("items_tags", KeyRange::eq([val(owner), val("t1")])).columns(["id"]), None).unwrap();
        let mut ids: Vec<i64> = tagged.rows.iter().map(|r| if let Value::Int(n) = r[0] { n } else { panic!() }).collect();
        ids.sort();
        let want: Vec<i64> = model.iter().filter(|(_, i)| i.owner == owner && i.tags.iter().any(|t| t == "t1")).map(|(id, _)| *id).collect();
        assert_eq!(ids, want, "index by tag for owner {owner}");

        let found = txn.query(&Query::table("items").search("items_search", [owner], "gamma").columns(["id"]), None).unwrap();
        let mut ids: Vec<i64> = found.rows.iter().map(|r| if let Value::Int(n) = r[0] { n } else { panic!() }).collect();
        ids.sort();
        let want: Vec<i64> = model.iter().filter(|(_, i)| i.owner == owner && i.body.split(' ').any(|w| w == "gamma")).map(|(id, _)| *id).collect();
        assert_eq!(ids, want, "search for owner {owner}");
    }
    let by_name = txn.query(&Query::table("items").index("items_name", KeyRange::all()).columns(["name"]), None).unwrap();
    let mut names: Vec<&String> = model.values().map(|i| &i.name).collect();
    names.sort();
    assert_eq!(by_name.rows.len(), names.len());
    drop(txn);
    db.check().expect("the files are consistent");
}

/// One random transaction of a few statements. Returns the model as it
/// is if the transaction commits.
fn random_transaction(rng: &mut Rng, db: &Arc<Database>, model: &Model, next_id: &mut i64) -> Result<Option<Model>, database_engine::error::Error> {
    let mut after = model.clone();
    // One in three runs as a fork: without the write lock until it is
    // done, then attached, which nothing here ever prevents.
    let forked = rng.chance(3);
    let mut txn = if forked {
        db.begin_fork()?
    } else {
        db.begin_write(WAIT, false)?
    };
    for _ in 0..1 + rng.below(4) {
        match rng.below(10) {
            0..=3 => {
                let id = *next_id;
                *next_id += 1;
                let item = random_item(rng, id);
                txn.insert(&insert_of(id, &item), None)?;
                after.insert(id, item);
            }
            4..=6 if !after.is_empty() => {
                let id = *after.keys().nth(rng.below(after.len() as u64) as usize).unwrap();
                let fresh = random_item(rng, id);
                let mut update = Update::table("items").key([id]).set("owner", fresh.owner).set("tags", fresh.tags.clone());
                let item = after.get_mut(&id).unwrap();
                item.owner = fresh.owner;
                item.tags = fresh.tags;
                // Half the time the body too, so large values are freed
                // and written, and half the time not, so they are kept.
                if rng.chance(2) {
                    update = update.set("body", fresh.body.as_str());
                    item.body = fresh.body;
                }
                txn.update(&update, None)?;
            }
            7..=8 if !after.is_empty() => {
                let id = *after.keys().nth(rng.below(after.len() as u64) as usize).unwrap();
                txn.delete(&Delete::from("items").key([id]), None)?;
                after.remove(&id);
            }
            _ => {
                // A statement that touches many rows at once.
                let owner = rng.below(4) as i64;
                txn.update(&Update::table("items").index("items_owner", KeyRange::eq([owner])).set("owner", (owner + 1) % 4), None)?;
                for item in after.values_mut().filter(|i| i.owner == owner) {
                    item.owner = (owner + 1) % 4;
                }
            }
        }
    }
    if rng.chance(6) {
        txn.rollback();
        return Ok(None);
    }
    if forked {
        assert!(txn.attach(WAIT)?, "nothing was committed beside the fork");
    }
    txn.commit()?;
    Ok(Some(after))
}

/// A fork that stands open beside the workload, holding pages of its
/// own, and is in the end dropped: through commits, checkpoints and
/// power cuts it must leave no trace, and none of its pages lost.
struct Bystander {
    rng: Rng,
    fork: Option<(database_engine::db::WriteTxn, u64)>,
}

impl Bystander {
    fn new(seed: u64) -> Bystander {
        Bystander {
            rng: Rng::new(seed ^ 0x5EED_F0C5),
            fork: None,
        }
    }

    /// Now and then begins a fork, writes in it, or ends it.
    fn step(&mut self, db: &Arc<Database>) {
        match (&mut self.fork, self.rng.below(4)) {
            (None, 0) => self.fork = db.begin_fork().ok().map(|fork| (fork, db.latest_version())),
            (Some((fork, _)), 0 | 1) => {
                // Rows no committed transaction ever holds. A failure
                // (the disk gone, a name taken) is all the same to it.
                let id = 2_000_000 + self.rng.below(1_000_000) as i64;
                let item = random_item(&mut self.rng, id);
                let _ = fork.insert(&insert_of(id, &item), None);
            }
            (Some((fork, began)), 2) => {
                // It can become the writer only if nobody has committed
                // since it began, and it is rolled back either way.
                if let Ok(attached) = fork.attach(WAIT) {
                    assert_eq!(
                        attached,
                        *began == db.latest_version(),
                        "a fork attaches exactly when no commit was made beside it"
                    );
                }
                self.fork = None;
            }
            _ => {}
        }
    }
}

fn create(engine: &Engine) -> Arc<Database> {
    let db = engine.create_database("test").unwrap();
    let mut txn = db.begin_write(WAIT, false).unwrap();
    txn.ddl(&Ddl::CreateTable { def: items(), if_not_exists: false }, None).unwrap();
    txn.commit().unwrap();
    db
}

#[test]
fn a_workload_matches_its_model_and_survives_clean_restarts() {
    let vfs = MemVfs::new();
    let mut rng = Rng::new(11);
    let mut model = Model::new();
    let mut next_id = 1;
    let mut engine = open(&vfs);
    let mut db = create(&engine);
    let mut bystander = Bystander::new(11);
    let mut merger = Merger::new(11);
    for step in 0..600 {
        bystander.step(&db);
        merger.step(&db);
        if let Some(after) = random_transaction(&mut rng, &db, &model, &mut next_id).unwrap() {
            model = after;
        }
        if step % 50 == 49 {
            verify(&db, &model);
        }
        if step % 97 == 96 {
            db.checkpoint().unwrap();
        }
        if step % 150 == 149 {
            bystander.fork = None;
            drop(db);
            if rng.chance(2) {
                engine.close().unwrap();
            } else {
                // Stopped without a final checkpoint: the log is replayed.
                drop(engine);
            }
            engine = open(&vfs);
            db = engine.database("test").unwrap();
            verify(&db, &model);
        }
    }
    verify(&db, &model);
    assert!(model.len() > 20, "the workload left data behind");
    assert!(merger.made > 20, "segments were merged beside the workload ({} times)", merger.made);
    assert!(db.merges_dropped.load(std::sync::atomic::Ordering::Relaxed) == 0, "between two transactions nothing changes under a merge");
}

#[test]
fn a_power_cut_at_any_moment_loses_no_commit_and_keeps_no_other() {
    // CRASH_SEEDS=2000 for a longer hunt.
    let seeds: u64 = std::env::var("CRASH_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(60);
    let mut cuts = 0;
    for seed in 0..seeds {
        let vfs = MemVfs::new();
        let mut rng = Rng::new(seed);
        let mut model = Model::new();
        let mut next_id = 1;
        let engine = open(&vfs);
        let db = create(&engine);
        let mut bystander = Bystander::new(seed);
        let mut merger = Merger::new(seed);
        // Some history first, then the cut somewhere in what follows.
        for _ in 0..rng.below(40) {
            bystander.step(&db);
            merger.step(&db);
            if let Some(after) = random_transaction(&mut rng, &db, &model, &mut next_id).unwrap() {
                model = after;
            }
        }
        if rng.chance(3) {
            db.checkpoint().unwrap();
        }
        vfs.crash_after(rng.below(120), seed);
        // The transaction under way when the disk goes: it may or may
        // not have been made durable before its client could be told.
        let mut in_flight: Option<Model> = None;
        loop {
            bystander.step(&db);
            // A merge the cut falls into changes no row: whatever of it
            // reached the disk, recovery gives the last commit.
            merger.step(&db);
            let mut attempt_rng = rng.clone();
            let mut attempt_id = next_id;
            match random_transaction(&mut attempt_rng, &db, &model, &mut attempt_id) {
                Ok(result) => {
                    rng = attempt_rng;
                    next_id = attempt_id;
                    if let Some(after) = result {
                        model = after;
                    }
                    if rng.chance(15) {
                        let _ = db.checkpoint();
                    }
                }
                Err(_) => {
                    // Work out what that transaction would have left.
                    let mut replay_rng = rng.clone();
                    let mut replay_id = next_id;
                    in_flight = model_of_transaction(&mut replay_rng, &model, &mut replay_id);
                    break;
                }
            }
            if vfs.has_crashed() {
                break;
            }
        }
        let Some(image) = vfs.take_crash_image() else { continue };
        cuts += 1;
        drop(bystander);
        drop(db);
        drop(engine);

        let recovered = open(&image);
        let db = recovered.database("test").unwrap_or_else(|e| panic!("seed {seed}: the database is gone: {e}"));
        let got = read_model(&db);
        if got == model {
            verify(&db, &model);
        } else {
            let after = in_flight.unwrap_or_else(|| panic!("seed {seed}: recovery gave a state that was never committed"));
            assert!(got == after, "seed {seed}: recovery gave neither the last commit nor the one under way");
            verify(&db, &after);
        }
        // And it works on: a write after recovery, and another restart.
        let mut txn = db.begin_write(WAIT, false).unwrap();
        txn.insert(&insert_of(1_000_000, &Item { owner: 0, name: "after".into(), body: "gamma".into(), tags: vec![] }), None).unwrap();
        txn.commit().unwrap();
        drop(db);
        recovered.close().unwrap();
        let again = open(&image);
        let db = again.database("test").unwrap();
        assert!(db.begin_read().query(&Query::table("items").key([1_000_000]), None).unwrap().rows.len() == 1, "seed {seed}");
        db.check().unwrap();
    }
    assert!(cuts > seeds * 2 / 3, "most runs were cut ({cuts} of {seeds})");
}

/// The model a transaction would produce, by running the same random
/// choices against the model alone.
fn model_of_transaction(rng: &mut Rng, model: &Model, next_id: &mut i64) -> Option<Model> {
    let mut after = model.clone();
    let _forked = rng.chance(3);
    for _ in 0..1 + rng.below(4) {
        match rng.below(10) {
            0..=3 => {
                let id = *next_id;
                *next_id += 1;
                after.insert(id, random_item(rng, id));
            }
            4..=6 if !after.is_empty() => {
                let id = *after.keys().nth(rng.below(after.len() as u64) as usize).unwrap();
                let fresh = random_item(rng, id);
                let item = after.get_mut(&id).unwrap();
                item.owner = fresh.owner;
                item.tags = fresh.tags;
                if rng.chance(2) {
                    item.body = fresh.body;
                }
            }
            7..=8 if !after.is_empty() => {
                let id = *after.keys().nth(rng.below(after.len() as u64) as usize).unwrap();
                after.remove(&id);
            }
            _ => {
                let owner = rng.below(4) as i64;
                for item in after.values_mut().filter(|i| i.owner == owner) {
                    item.owner = (owner + 1) % 4;
                }
            }
        }
    }
    if rng.chance(6) {
        return None;
    }
    Some(after)
}

#[test]
fn segments_are_merged_beside_writers_that_do_not_wait_for_it() {
    // The engine's own thread merges, at moments nobody chose, while
    // three threads write and one searches. No writer ever merges a run
    // of segments itself (there are never enough of them for that), no
    // search ever misses a committed row or meets one that is gone, and
    // in the end the index is whole and its segments few.
    let vfs = MemVfs::new();
    let engine = open(&vfs);
    let db = create(&engine);
    engine.pause_merging(false);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writers: Vec<_> = (0..3i64)
        .map(|w| {
            let db = db.clone();
            std::thread::spawn(move || {
                let mut rng = Rng::new(100 + w as u64);
                let mut mine: Model = Model::new();
                for n in 0..400i64 {
                    let id = w * 1_000_000 + n;
                    let mut txn = db.begin_write(WAIT, false).unwrap();
                    let mut item = random_item(&mut rng, id);
                    item.owner = w;
                    txn.insert(&insert_of(id, &item), None).unwrap();
                    mine.insert(id, item);
                    // Now and then an earlier row of this writer's goes,
                    // so that segments have dead rows when they merge.
                    if n % 5 == 4 {
                        let gone = w * 1_000_000 + rng.below(n as u64) as i64;
                        if mine.remove(&gone).is_some() {
                            txn.delete(&Delete::from("items").key([gone]), None).unwrap();
                        }
                    }
                    txn.commit().unwrap();
                }
                mine
            })
        })
        .collect();
    let searching = {
        let (db, stop) = (db.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut searches = 0u32;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let txn = db.begin_read();
                for owner in 0..3i64 {
                    // What a search finds is what the rows say, at the
                    // one moment the reader holds.
                    let found = txn.query(&Query::table("items").search("items_search", [owner], "gamma").columns(["id"]), None).unwrap();
                    let mut ids: Vec<i64> = found.rows.iter().map(|r| if let Value::Int(n) = r[0] { n } else { panic!() }).collect();
                    ids.sort();
                    let rows = txn.query(&Query::table("items").index("items_owner", KeyRange::eq([owner])).columns(["id", "body"]), None).unwrap();
                    let want: Vec<i64> = rows
                        .rows
                        .iter()
                        .filter_map(|r| match (&r[0], &r[1]) {
                            (Value::Int(id), Value::Text(body)) if body.split(' ').any(|word| word == "gamma") => Some(*id),
                            _ => None,
                        })
                        .collect();
                    assert_eq!(ids, want, "a search beside writers and merges, owner {owner}");
                }
                searches += 1;
            }
            searches
        })
    };
    let mut model = Model::new();
    for writer in writers {
        model.extend(writer.join().unwrap());
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(searching.join().unwrap() > 0);
    // The merging thread is given a moment to finish what is due.
    let made = || db.merges_beside.load(std::sync::atomic::Ordering::Relaxed);
    let mut settled = made();
    for _ in 0..200 {
        std::thread::sleep(Duration::from_millis(20));
        if made() == settled {
            break;
        }
        settled = made();
    }
    assert!(made() > 10, "{} merges were made beside the writers", made());
    engine.pause_merging(true);
    while db.merge_text_segments(&|| false).unwrap() {}
    verify(&db, &model);
    // And all of it is as good after a restart that replays the log.
    drop(db);
    drop(engine);
    let engine = open(&vfs);
    verify(&engine.database("test").unwrap(), &model);
}

/// The rows a search of `items` for a word finds, for owner 0.
fn found(db: &Arc<Database>, word: &str) -> Vec<i64> {
    let found = db.begin_read().query(&Query::table("items").search("items_search", [0], word).columns(["id"]), None).unwrap();
    let mut ids: Vec<i64> = found.rows.iter().map(|r| if let Value::Int(n) = r[0] { n } else { panic!() }).collect();
    ids.sort();
    ids
}

#[test]
fn a_merge_beside_is_dropped_if_its_segments_change_meanwhile() {
    // Rows of forty words of their own each, so that a merge of a few
    // segments reads enough words to ask, as it goes, whether to give
    // up: that is where these commits are made, while it merges.
    let vfs = MemVfs::new();
    let engine = open(&vfs);
    let db = create(&engine);
    let merges = |db: &Arc<Database>| (db.merges_beside.load(std::sync::atomic::Ordering::Relaxed), db.merges_dropped.load(std::sync::atomic::Ordering::Relaxed));
    let mut model = Model::new();
    let add = |model: &mut Model, id: i64| {
        let body = (0..40).map(|k| format!("u{id}x{k}")).collect::<Vec<_>>().join(" ");
        let item = Item { owner: 0, name: format!("n{id}"), body, tags: Vec::new() };
        let mut txn = db.begin_write(WAIT, false).unwrap();
        txn.insert(&insert_of(id, &item), None).unwrap();
        txn.commit().unwrap();
        model.insert(id, item);
    };
    for id in 0..12 {
        add(&mut model, id);
    }

    // A row of a merged segment dies while the merge is made: the merge
    // is thrown away, and the row is not found.
    let asked = std::cell::Cell::new(0);
    let made = db
        .merge_text_segments(&|| {
            if asked.replace(asked.get() + 1) == 0 {
                let mut txn = db.begin_write(WAIT, false).unwrap();
                txn.delete(&Delete::from("items").key([1]), None).unwrap();
                txn.commit().unwrap();
            }
            false
        })
        .unwrap();
    assert!(made && asked.get() > 0, "a merge was made, and asked whether to give up");
    assert_eq!(merges(&db), (0, 1), "and it was dropped");
    model.remove(&1);
    assert!(found(&db, "u1x7").is_empty());
    assert_eq!(found(&db, "u2x7"), [2]);
    verify(&db, &model);

    // Made again with nothing changing, it is put in place.
    assert!(db.merge_text_segments(&|| false).unwrap());
    assert_eq!(merges(&db), (1, 1));
    assert!(found(&db, "u1x7").is_empty());
    assert_eq!(found(&db, "u0x39"), [0]);
    verify(&db, &model);

    // Rows added while a merge is made go into segments of their own,
    // after the merged ones: the merge still fits, and is put in place.
    for id in 12..24 {
        add(&mut model, id);
    }
    let asked = std::cell::Cell::new(0);
    let more = std::cell::RefCell::new(Model::new());
    assert!(db
        .merge_text_segments(&|| {
            if asked.replace(asked.get() + 1) == 0 {
                for id in 100..104 {
                    add(&mut more.borrow_mut(), id);
                }
            }
            false
        })
        .unwrap());
    assert!(asked.get() > 0);
    assert_eq!(merges(&db), (2, 1));
    model.extend(more.into_inner());
    assert_eq!(found(&db, "u101x3"), [101]);
    assert_eq!(found(&db, "u13x3"), [13]);
    verify(&db, &model);

    // A merge given up on leaves the index as it was.
    let before = merges(&db);
    assert!(!db.merge_text_segments(&|| true).unwrap());
    assert_eq!(merges(&db), before);
    while db.merge_text_segments(&|| false).unwrap() {}
    verify(&db, &model);
    drop(db);
    drop(engine);
    let engine = open(&vfs);
    verify(&engine.database("test").unwrap(), &model);
}

#[test]
fn a_rolled_back_savepoint_leaves_what_came_before_it() {
    let vfs = MemVfs::new();
    let engine = open(&vfs);
    let db = create(&engine);
    let mut model = Model::new();
    let item = |n: i64| Item { owner: n % 4, name: format!("n{n}"), body: "delta ".repeat(n as usize % 7), tags: vec![format!("t{}", n % 5)] };

    let mut txn = db.begin_write(WAIT, true).unwrap();
    for n in 1..=40 {
        txn.insert(&insert_of(n, &item(n)), None).unwrap();
        model.insert(n, item(n));
    }
    txn.savepoint("a").unwrap();
    for n in 41..=80 {
        txn.insert(&insert_of(n, &item(n)), None).unwrap();
    }
    txn.update(&Update::table("items").set("owner", 3), None).unwrap();
    txn.delete(&Delete::from("items").key([5]), None).unwrap();
    // A statement that fails leaves the transaction unusable...
    let duplicate = txn.insert(&insert_of(999, &Item { name: "n7".into(), ..item(999) }), None);
    assert!(duplicate.is_err());
    assert!(txn.insert(&insert_of(1000, &item(1000)), None).is_err());
    // ...until it goes back to the savepoint.
    txn.rollback_to("a").unwrap();
    txn.insert(&insert_of(81, &item(81)), None).unwrap();
    model.insert(81, item(81));
    txn.savepoint("b").unwrap();
    txn.delete(&Delete::from("items"), None).unwrap();
    txn.rollback_to("b").unwrap();
    txn.release("b").unwrap();
    txn.commit().unwrap();
    verify(&db, &model);

    drop(db);
    drop(engine);
    let engine = open(&vfs);
    verify(&engine.database("test").unwrap(), &model);
}

#[test]
fn readers_keep_their_version_while_writers_go_on() {
    let vfs = MemVfs::new();
    let engine = open(&vfs);
    let db = create(&engine);
    let mut rng = Rng::new(5);
    let mut model = Model::new();
    let mut next_id = 1;
    let mut held: Vec<(database_engine::db::ReadTxn, Model)> = Vec::new();
    for step in 0..300 {
        if let Some(after) = random_transaction(&mut rng, &db, &model, &mut next_id).unwrap() {
            model = after;
        }
        if step % 30 == 0 {
            held.push((db.begin_read(), model.clone()));
        }
        if step % 70 == 69 {
            db.checkpoint().unwrap();
        }
        if held.len() > 4 {
            held.remove(0);
        }
        // Every reader still sees exactly the moment it began.
        for (txn, then) in &held {
            let rows = txn.query(&Query::table("items").columns(["id", "name"]), None).unwrap().rows;
            assert_eq!(rows.len(), then.len());
            for row in rows {
                let (Value::Int(id), Value::Text(name)) = (&row[0], &row[1]) else { panic!() };
                assert_eq!(&then[id].name, name);
            }
        }
    }
    drop(held);
    verify(&db, &model);
}

#[test]
fn a_dump_restores_to_the_same_rows() {
    let vfs = MemVfs::new();
    let engine = open(&vfs);
    let db = create(&engine);
    let mut rng = Rng::new(9);
    let mut model = Model::new();
    let mut next_id = 1;
    for _ in 0..200 {
        if let Some(after) = random_transaction(&mut rng, &db, &model, &mut next_id).unwrap() {
            model = after;
        }
    }
    let mut dump = Vec::new();
    let rows = database_engine::dump::dump(&engine, &[], &|name| format!("note for {name}").into_bytes(), &mut dump).unwrap();
    assert_eq!(rows, model.len() as u64);

    // Into an empty server.
    let other_vfs = MemVfs::new();
    let other = open(&other_vfs);
    let mut restore = database_engine::dump::Restore::new(&other, false);
    for piece in dump.chunks(7_777) {
        restore.feed(piece).unwrap();
    }
    assert_eq!(restore.finish().unwrap(), [("test".to_string(), b"note for test".to_vec())]);
    verify(&other.database("test").unwrap(), &model);
    drop(other);
    let other = open(&other_vfs);
    verify(&other.database("test").unwrap(), &model);

    // Over an existing database only when told to replace it.
    let mut refused = database_engine::dump::Restore::new(&other, false);
    assert!(refused.feed(&dump).is_err());
    drop(refused);
    verify(&other.database("test").unwrap(), &model);

    // A dump cut short, or with one bit flipped, changes nothing.
    for damaged in [dump[..dump.len() - 3].to_vec(), {
        let mut d = dump.clone();
        let at = d.len() / 2;
        d[at] ^= 0x10;
        d
    }] {
        let mut restore = database_engine::dump::Restore::new(&engine, true);
        let fed = restore.feed(&damaged);
        assert!(fed.is_err() || restore.finish().is_err());
        verify(&engine.database("test").unwrap(), &model);
    }
    assert_eq!(engine.names(), ["test"]);
}
