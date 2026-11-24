//! The engine against a model, through every fault a disk can show it.
//!
//! `recovery.rs` cuts the power at random moments of a random workload.
//! Here a scripted workload is run once, and then, for every operation
//! that changed the disk while it ran, the disk is rebuilt as it stood
//! just before that operation, which is where a process killed at that
//! step leaves it, and the power is cut there in every way that matters:
//! nothing lost, everything unsynced lost, the last write torn at its
//! sectors, the last write kept without those before it. After each cut
//! the engine is started and must hold exactly what its clients had been
//! told, or that and the commits under way.
//!
//! Then the disk is made to fail under the same workload: full from each
//! operation on, and each operation failing once, a sync by losing what
//! it was to make durable. Whatever the engine then answers, it must not
//! have acknowledged anything that a restart takes back. And last the
//! disk lies about its syncs, where no engine can keep its promise: then
//! the database must come back as it stood at some commit, or say that
//! its files are damaged, as far as checksums can tell.
//!
//! `FAULT_SCRIPTS=20` adds that many generated workloads to the fixed
//! one, and looks closer at each.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use common::*;
use database_engine::db::{Database, WriteTxn};
use database_engine::dump::Restore;
use database_engine::engine::{Engine, EngineOptions};
use database_engine::error::{Error, Result};
use database_engine::page::PAGE_SIZE;
use database_engine::store::Limits;
use database_engine::vfs::{sectors, Entry, Fate, Fault, MemVfs, Op, OpKind, Plan, Rng};
use database_protocol::error::ErrorCode;
use database_protocol::expr::*;
use database_protocol::message::Ddl;
use database_protocol::query::*;
use database_protocol::schema::*;
use database_protocol::value::{Type, Value};

/// The engine as these tests run it. A transaction or a batch of commits
/// of more than a few pages is written to the data file early, so that a
/// small workload does all the engine ever does; and nothing checkpoints
/// by itself, so that a workload makes the same operations, in the same
/// order, every time it runs.
fn quiet() -> EngineOptions {
    let mut options = options();
    options.db.limits = Limits { max_dirty: 6, max_private: 6 };
    options.db.checkpoint_bytes = u64::MAX;
    options
}

fn try_start(vfs: &MemVfs) -> Result<Engine> {
    try_open_with(vfs, quiet())
}

// ------------------------------------------------------------ the model

/// `notes`: a table the workload makes, changes the shape of, and drops.
#[derive(Clone, Debug, PartialEq, Default)]
struct Notes {
    rows: BTreeMap<i64, String>,
    /// Whether the column `rank` has been added. Every row then reads 7.
    ranked: bool,
    /// Whether the index `notes_text` has been made.
    indexed: bool,
}

/// What the data directory holds, as the model keeps it.
#[derive(Clone, Debug, PartialEq, Default)]
struct State {
    /// Whether the database has been made.
    exists: bool,
    /// The rows of `items`, once that table and `hits` are made.
    items: Option<Model>,
    notes: Option<Notes>,
}

/// One statement of the workload.
#[derive(Clone, Debug)]
enum Stmt {
    /// Makes `items` and `hits`, a table that need not survive a restart.
    Tables,
    Put(i64, Item),
    Change { id: i64, owner: i64, tags: Vec<String>, body: Option<String> },
    Remove(i64),
    /// Moves every item of an owner to the next: one statement, many rows.
    Move(i64),
    /// Counts in `hits`: a write that puts nothing in the log.
    Hit(i64),
    Notes,
    Note(i64, String),
    Rank,
    Index,
    DropNotes,
}

fn hits() -> TableDef {
    TableDef::new("hits").column(ColumnDef::new("key", Type::Text)).column(ColumnDef::new("n", Type::Int)).primary_key(["key"]).volatile()
}

fn notes() -> TableDef {
    TableDef::new("notes").column(ColumnDef::new("id", Type::Int)).column(ColumnDef::new("text", Type::Text)).primary_key(["id"])
}

fn run(txn: &mut WriteTxn, stmt: &Stmt) -> Result<()> {
    match stmt {
        Stmt::Tables => {
            txn.ddl(&Ddl::CreateTable { def: items(), if_not_exists: false }, None)?;
            txn.ddl(&Ddl::CreateTable { def: hits(), if_not_exists: false }, None)?;
        }
        Stmt::Put(id, item) => {
            txn.insert(&insert_of(*id, item), None)?;
        }
        Stmt::Change { id, owner, tags, body } => {
            let mut update = Update::table("items").key([*id]).set("owner", *owner).set("tags", tags.clone());
            if let Some(body) = body {
                update = update.set("body", body.as_str());
            }
            txn.update(&update, None)?;
        }
        Stmt::Remove(id) => {
            txn.delete(&Delete::from("items").key([*id]), None)?;
        }
        Stmt::Move(owner) => {
            txn.update(&Update::table("items").index("items_owner", KeyRange::eq([*owner])).set("owner", (owner + 1) % 4), None)?;
        }
        Stmt::Hit(n) => {
            txn.insert(&Insert::into("hits").set("key", format!("k{n}")).set("n", 1).on_conflict_update(PRIMARY).update("n", col("n") + 1), None)?;
        }
        Stmt::Notes => txn.ddl(&Ddl::CreateTable { def: notes(), if_not_exists: false }, None)?,
        Stmt::Note(id, text) => {
            txn.insert(&Insert::into("notes").set("id", *id).set("text", text.as_str()), None)?;
        }
        Stmt::Rank => txn.ddl(&Ddl::AddColumn { table: "notes".into(), column: ColumnDef::new("rank", Type::Int).default(7), if_not_exists: false }, None)?,
        Stmt::Index => txn.ddl(&Ddl::CreateIndex { table: "notes".into(), index: IndexDef::new("notes_text").on(["text"]), if_not_exists: false }, None)?,
        Stmt::DropNotes => txn.ddl(&Ddl::DropTable { name: "notes".into(), if_exists: false }, None)?,
    }
    Ok(())
}

/// What a statement the engine accepted does to the model.
fn apply(state: &mut State, stmt: &Stmt) {
    const ACCEPTED: &str = "the engine accepted a statement on a table the model lacks";
    match stmt {
        Stmt::Tables => state.items = Some(Model::new()),
        Stmt::Hit(_) => {}
        Stmt::Put(id, item) => {
            state.items.as_mut().expect(ACCEPTED).insert(*id, item.clone());
        }
        Stmt::Change { id, owner, tags, body } => {
            if let Some(item) = state.items.as_mut().expect(ACCEPTED).get_mut(id) {
                item.owner = *owner;
                item.tags = tags.clone();
                if let Some(body) = body {
                    item.body = body.clone();
                }
            }
        }
        Stmt::Remove(id) => {
            state.items.as_mut().expect(ACCEPTED).remove(id);
        }
        Stmt::Move(owner) => {
            for item in state.items.as_mut().expect(ACCEPTED).values_mut().filter(|item| item.owner == *owner) {
                item.owner = (owner + 1) % 4;
            }
        }
        Stmt::Notes => state.notes = Some(Notes::default()),
        Stmt::DropNotes => state.notes = None,
        Stmt::Note(id, text) => {
            state.notes.as_mut().expect(ACCEPTED).rows.insert(*id, text.clone());
        }
        Stmt::Rank => state.notes.as_mut().expect(ACCEPTED).ranked = true,
        Stmt::Index => state.notes.as_mut().expect(ACCEPTED).indexed = true,
    }
}

/// Why the data directory could not be read as the workload wrote it.
#[derive(Debug)]
enum Unread {
    /// The engine failed, and this is what it said.
    Failed(Error),
    /// The engine answered, with what no table of the workload holds.
    Wrong(String),
}

impl From<Error> for Unread {
    fn from(e: Error) -> Unread {
        Unread::Failed(e)
    }
}

impl std::fmt::Display for Unread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unread::Failed(e) => write!(f, "{e}"),
            Unread::Wrong(what) => write!(f, "it holds what the workload never wrote: {what}"),
        }
    }
}

fn wrong(what: &str, row: &[Value]) -> Unread {
    let row: String = format!("{row:?}").chars().take(160).collect();
    Unread::Wrong(format!("{what}: {row}"))
}

/// The data directory as the engine shows it.
fn try_read_state(engine: &Engine) -> std::result::Result<State, Unread> {
    let names = engine.names();
    if names.is_empty() {
        return Ok(State::default());
    }
    if names != ["test"] {
        return Err(Unread::Wrong(format!("the databases {names:?}")));
    }
    let db = engine.database("test")?;
    let txn = db.begin_read();
    let tables = txn.describe();
    let table = |name: &str| tables.iter().find(|table| table.name == name);
    if let Some(odd) = tables.iter().find(|table| !["items", "hits", "notes"].contains(&table.name.as_str())) {
        return Err(Unread::Wrong(format!("a table {:?}", odd.name)));
    }
    if table("items").is_some() != table("hits").is_some() {
        return Err(Unread::Wrong("one of two tables that were made in one transaction".into()));
    }
    let mut state = State { exists: true, ..State::default() };
    if table("items").is_some() {
        let mut rows = Model::new();
        for row in txn.query(&Query::table("items"), None)?.rows {
            let [Value::Int(id), Value::Int(owner), Value::Text(name), Value::Text(body), Value::List(tags)] = &row[..] else { return Err(wrong("a row of items", &row)) };
            let item = Item { owner: *owner, name: name.clone(), body: body.clone(), tags: tags.iter().map(|tag| tag.to_string()).collect() };
            if rows.insert(*id, item).is_some() {
                return Err(wrong("a second row of items with one key", &row));
            }
        }
        state.items = Some(rows);
    }
    if let Some(def) = table("notes") {
        let ranked = def.columns.iter().any(|column| column.name == "rank");
        let indexed = def.indexes.iter().any(|index| index.name == "notes_text");
        let mut rows = BTreeMap::new();
        for row in txn.query(&Query::table("notes"), None)?.rows {
            let text = match &row[..] {
                [Value::Int(_), Value::Text(text)] if !ranked => text,
                // The added column reads its default in every row.
                [Value::Int(_), Value::Text(text), Value::Int(7)] if ranked => text,
                _ => return Err(wrong("a row of notes", &row)),
            };
            let Value::Int(id) = row[0] else { unreachable!("matched above") };
            if rows.insert(id, text.clone()).is_some() {
                return Err(wrong("a second row of notes with one key", &row));
            }
        }
        state.notes = Some(Notes { rows, ranked, indexed });
    }
    Ok(state)
}

fn read_state(engine: &Engine) -> State {
    try_read_state(engine).unwrap_or_else(|e| panic!("the database does not read: {e}"))
}

/// Compares what the engine holds with `state`: every row by its key and
/// through every index, and every page accounted for. `restarted` says
/// the engine has just been started, so `hits` must be empty.
fn verify_state(engine: &Engine, state: &State, restarted: bool) {
    let got = read_state(engine);
    assert!(got == *state, "{}", differ(&got, std::slice::from_ref(state)));
    verify_rest(engine, state, restarted);
}

/// What `verify_state` checks beyond the rows themselves, for a state
/// that was just read from the engine.
fn verify_rest(engine: &Engine, state: &State, restarted: bool) {
    if !state.exists {
        return;
    }
    let db = engine.database("test").unwrap();
    compare_rest(&db, state, restarted);
    db.check().expect("the files are consistent");
}

/// The same without the check of the files.
fn compare_rest(db: &Arc<Database>, state: &State, restarted: bool) {
    if let Some(model) = &state.items {
        compare_indexes(db, model);
        if restarted {
            assert!(db.begin_read().query(&Query::table("hits"), None).unwrap().rows.is_empty(), "a table that need not survive a restart did");
        }
    }
    if let Some(notes) = &state.notes {
        if notes.indexed {
            let by_text = db.begin_read().query(&Query::table("notes").index("notes_text", KeyRange::all()).columns(["text"]), None).unwrap();
            let mut texts: Vec<&String> = notes.rows.values().collect();
            texts.sort();
            let got: Vec<&String> = by_text.rows.iter().map(|row| if let Value::Text(text) = &row[0] { text } else { panic!() }).collect();
            assert_eq!(got, texts, "notes by their index");
        }
    }
}

/// How `got` differs from the states it might have been, in few words:
/// the states themselves are too long to print.
fn differ(got: &State, possible: &[State]) -> String {
    let brief = |state: &State| {
        let items = state.items.as_ref().map(|items| items.keys().copied().collect::<Vec<_>>());
        let notes = state.notes.as_ref().map(|notes| (notes.rows.keys().copied().collect::<Vec<_>>(), notes.ranked, notes.indexed));
        format!("database: {}, items: {items:?}, notes: {notes:?}", state.exists)
    };
    let possible: Vec<String> = possible.iter().map(brief).collect();
    format!("it holds [{}], and could hold [{}] (rows of the same keys may differ)", brief(got), possible.join("] or ["))
}

/// Says what was being tried, if the test dies inside the engine.
struct Scene(String);

impl Drop for Scene {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("while: {}", self.0);
        }
    }
}

// --------------------------------------------------------- the workload

#[derive(Clone, Debug)]
enum Beside {
    Begin,
    Write(i64, Item),
    End,
}

/// A moment in a checkpoint, once it has let writers back in.
#[derive(Clone, Copy, Debug)]
enum When {
    /// As it writes the first of its pages.
    Pages,
    /// As it writes the header that makes it the data file's state.
    Header,
    /// As it removes the log it has made unnecessary.
    Removal,
}

impl When {
    fn is(self, op: &Op) -> bool {
        let data = op.path.extension().is_some_and(|e| e == "db");
        match (self, &op.kind) {
            (When::Pages, OpKind::Write { offset, .. }) => data && *offset >= 2 * PAGE_SIZE as u64,
            (When::Header, OpKind::Write { offset, .. }) => data && *offset < 2 * PAGE_SIZE as u64,
            (When::Removal, OpKind::Remove) => true,
            _ => false,
        }
    }
}

#[derive(Clone, Debug)]
enum Step {
    /// Starts the engine on the disk as it is.
    Start,
    /// Stops it: with a last checkpoint, or, as a killed process does,
    /// without, so that the next start replays the log.
    Stop { clean: bool },
    /// Makes the database.
    Create,
    /// Drops it.
    Drop,
    /// Loads a dump over it: the bytes, and what the dump holds.
    Restore(Arc<(Vec<u8>, State)>),
    Txn { fork: bool, stmts: Vec<Stmt>, commit: bool },
    /// Transactions that commit one after another before any of them
    /// waits for the disk, and so share a sync. `told`: each is told of
    /// it as a server's connection is, not by waiting on a thread.
    Group { txns: Vec<Vec<Stmt>>, told: bool },
    /// A checkpoint, with these transactions committed beside it, at
    /// that moment of it, as writers do while a server checkpoints.
    Checkpoint { beside: Vec<Vec<Stmt>>, when: When },
    /// A fork that stands open beside the workload, holding pages of its
    /// own, and is in the end dropped without a trace.
    Beside(Beside),
}

fn txn(stmts: impl IntoIterator<Item = Stmt>) -> Step {
    Step::Txn { fork: false, stmts: stmts.into_iter().collect(), commit: true }
}

fn fork(stmts: impl IntoIterator<Item = Stmt>) -> Step {
    Step::Txn { fork: true, stmts: stmts.into_iter().collect(), commit: true }
}

fn checkpoint() -> Step {
    Step::Checkpoint { beside: Vec::new(), when: When::Pages }
}

fn item(owner: i64, name: &str, body: &str, tags: &[&str]) -> Item {
    Item { owner, name: name.into(), body: body.into(), tags: tags.iter().map(|tag| tag.to_string()).collect() }
}

/// A dump of a database the workload's tables were made in, and what it
/// holds.
fn a_dump() -> Arc<(Vec<u8>, State)> {
    let vfs = MemVfs::new();
    let engine = try_start(&vfs).unwrap();
    let db = engine.create_database("test").unwrap();
    let mut state = State { exists: true, ..State::default() };
    let mut txn = db.begin_write(WAIT, false).unwrap();
    for stmt in [Stmt::Tables, Stmt::Put(101, item(1, "dumped", "gamma beta", &["t1"])), Stmt::Put(102, item(2, "dumped too", &"delta ".repeat(2_000), &[])), Stmt::Hit(1)] {
        run(&mut txn, &stmt).unwrap();
        apply(&mut state, &stmt);
    }
    txn.commit().unwrap();
    let mut dump = Vec::new();
    database_engine::dump::dump(&engine, &[], &|_| Vec::new(), &mut dump).unwrap();
    Arc::new((dump, state))
}

/// The fixed workload: every kind of commit and every kind of file
/// operation the engine makes, in as few steps as will hold them.
fn script() -> Vec<Step> {
    use Stmt::*;
    // More than a transaction may hold in memory: its pages are written
    // early, to numbers the last checkpoint does not use.
    let huge = "gamma delta ".repeat(4_000);
    let large = "alpha ".repeat(2_000);
    vec![
        Step::Start,
        Step::Create,
        txn([Tables]),
        txn([Put(1, item(0, "one", "alpha beta", &["t1"])), Put(2, item(1, "two", "gamma", &["t1", "t2"])), Put(3, item(1, "three", "", &[]))]),
        // Nothing for the log.
        txn([Hit(1), Hit(2)]),
        txn([Put(4, item(2, "four", &large, &["t3"]))]),
        fork([Put(5, item(3, "five", "delta gamma", &["t1"])), Change { id: 2, owner: 3, tags: vec!["t4".into()], body: None }]),
        Step::Beside(Beside::Begin),
        Step::Beside(Beside::Write(900, item(0, "beside", &large, &["t1"]))),
        // Something for the log and something not.
        txn([Hit(1), Put(6, item(0, "six", "beta gamma", &["t2"]))]),
        txn([Notes, Note(1, "first".into())]),
        // Writers go on while the checkpoint writes its pages.
        Step::Checkpoint { beside: vec![vec![Put(13, item(1, "thirteen", "gamma", &["t1"]))], vec![Hit(4)], vec![Remove(3), Note(4, "fourth".into())]], when: When::Pages },
        txn([Change { id: 4, owner: 2, tags: vec!["t1".into()], body: Some("zeta ".repeat(2_000)) }, Remove(1)]),
        Step::Group { txns: vec![vec![Put(7, item(1, "seven", "gamma", &[]))], vec![Put(8, item(2, "eight", "epsilon", &["t1"])), Hit(3)], vec![Change { id: 7, owner: 0, tags: vec!["t1".into()], body: None }]], told: false },
        Step::Txn { fork: false, stmts: vec![Put(99, item(0, "undone", "gamma", &["t1"]))], commit: false },
        Step::Beside(Beside::End),
        txn([Rank, Note(2, "second".into())]),
        txn([Index]),
        txn([Put(9, item(3, "nine", &huge, &["t1"]))]),
        Step::Stop { clean: false },
        Step::Start,
        txn([Move(1)]),
        fork([Remove(4), Remove(9)]),
        // And while it switches the header, and while it removes the log.
        Step::Checkpoint { beside: vec![vec![Put(14, item(3, "fourteen", &large, &[]))]], when: When::Header },
        txn([Put(10, item(0, "ten", "gamma alpha", &["t1", "t3"]))]),
        Step::Group { txns: vec![vec![Note(3, "third".into())], vec![Put(11, item(1, "eleven", "beta", &[]))]], told: true },
        Step::Checkpoint { beside: vec![vec![Change { id: 10, owner: 1, tags: vec![], body: None }]], when: When::Removal },
        txn([DropNotes]),
        Step::Stop { clean: true },
        Step::Start,
        // Left in the log when the database is dropped.
        txn([Put(12, item(2, "twelve", "gamma", &["t1"]))]),
        Step::Drop,
        Step::Create,
        txn([Tables, Put(15, item(0, "fifteen", "gamma", &[]))]),
        Step::Restore(a_dump()),
        txn([Put(16, item(3, "sixteen", "beta gamma", &["t1"]))]),
        checkpoint(),
    ]
}

/// A workload drawn from a seed: the same kinds of step in another order
/// and with other rows.
fn generated(seed: u64, len: usize) -> Vec<Step> {
    let mut rng = Rng::new(seed ^ 0xFA17_5EED);
    let mut steps = vec![Step::Start, Step::Create, txn([Stmt::Tables])];
    let mut live: Vec<i64> = Vec::new();
    let (mut next, mut next_note) = (1i64, 1i64);
    // 0: no table; 1: made; 2: ranked; 3: indexed.
    let mut notes = 0;
    let mut beside = false;
    let writes = |rng: &mut Rng, live: &mut Vec<i64>, next: &mut i64, commit: bool| -> Vec<Stmt> {
        let mut stmts = Vec::new();
        let mut after = live.clone();
        for _ in 0..1 + rng.below(3) {
            match rng.below(10) {
                0..=3 => {
                    stmts.push(Stmt::Put(*next, random_item(rng, *next)));
                    after.push(*next);
                    *next += 1;
                }
                4..=6 if !after.is_empty() => {
                    let id = after[rng.below(after.len() as u64) as usize];
                    let fresh = random_item(rng, id);
                    stmts.push(Stmt::Change { id, owner: fresh.owner, tags: fresh.tags, body: rng.chance(2).then_some(fresh.body) });
                }
                7 if !after.is_empty() => {
                    let id = after.swap_remove(rng.below(after.len() as u64) as usize);
                    stmts.push(Stmt::Remove(id));
                }
                8 => stmts.push(Stmt::Move(rng.below(4) as i64)),
                _ => stmts.push(Stmt::Hit(rng.below(5) as i64)),
            }
        }
        if commit {
            *live = after;
        }
        stmts
    };
    for _ in 0..len {
        match rng.below(16) {
            0 => {
                let beside = (0..rng.below(3)).map(|_| writes(&mut rng, &mut live, &mut next, true)).collect();
                let when = [When::Pages, When::Header, When::Removal][rng.below(3) as usize];
                steps.push(Step::Checkpoint { beside, when });
            }
            1 => {
                steps.push(Step::Stop { clean: rng.chance(2) });
                steps.push(Step::Start);
                beside = false;
            }
            2 => {
                let txns = (0..2 + rng.below(2)).map(|_| writes(&mut rng, &mut live, &mut next, true)).collect();
                steps.push(Step::Group { txns, told: rng.chance(2) });
            }
            3 if !beside => {
                steps.push(Step::Beside(Beside::Begin));
                beside = true;
            }
            3 => {
                if rng.chance(2) {
                    steps.push(Step::Beside(Beside::Write(1_000_000 + next, random_item(&mut rng, 1_000_000 + next))));
                } else {
                    steps.push(Step::Beside(Beside::End));
                    beside = false;
                }
            }
            4 => {
                let stmts = match notes {
                    0 => vec![Stmt::Notes, Stmt::Note(next_note, format!("note {next_note}"))],
                    1 => vec![Stmt::Rank, Stmt::Note(next_note, format!("note {next_note}"))],
                    2 => vec![Stmt::Index],
                    _ => vec![Stmt::DropNotes],
                };
                next_note += 1;
                notes = (notes + 1) % 4;
                steps.push(txn(stmts));
            }
            5 => steps.push(txn([Stmt::Hit(rng.below(5) as i64)])),
            _ => {
                let commit = !rng.chance(6);
                let stmts = writes(&mut rng, &mut live, &mut next, commit);
                steps.push(Step::Txn { fork: rng.chance(3), stmts, commit });
            }
        }
    }
    // It ends as the fixed one does: restored over, and dropped.
    steps.extend([Step::Restore(a_dump()), txn([Stmt::Put(next, random_item(&mut rng, next))]), Step::Stop { clean: false }, Step::Start, Step::Drop]);
    steps
}

/// The fixed workload, and as many generated ones as `FAULT_SCRIPTS` asks
/// for.
fn scripts() -> Vec<(String, Vec<Step>)> {
    let more: u64 = std::env::var("FAULT_SCRIPTS").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut all = vec![("the fixed workload".to_string(), script())];
    all.extend((0..more).map(|seed| (format!("generated workload {seed}"), generated(seed, 40))));
    all
}

/// Whether `FAULT_SCRIPTS` asks for the longer hunt.
fn hunting() -> bool {
    std::env::var("FAULT_SCRIPTS").is_ok()
}

fn stopped() -> Error {
    Error::db(ErrorCode::Unavailable, "the engine is stopped, or must be restarted first")
}

/// What the clients of a workload know.
struct Book {
    /// Every state the data directory has been through, to their
    /// knowledge: each commit that was acknowledged adds one.
    states: Vec<State>,
    /// What the commits that have begun and not been acknowledged would
    /// make of the last state, each on top of the one before: a crash may
    /// leave any of them, or none.
    maybe: Vec<State>,
    /// Whether a commit has failed since the engine was started. Whether
    /// it is in the log, only a restart can tell.
    failed: bool,
    /// Which states a crash may leave, by how many operations the disk
    /// had seen: from that many on, `states[lo..=hi]`.
    marks: Vec<(usize, usize, usize)>,
}

impl Book {
    fn now(&self) -> &State {
        self.states.last().unwrap()
    }

    fn mark(&mut self, vfs: &MemVfs) {
        let lo = self.states.len() - 1;
        self.marks.push((vfs.journal_len(), lo, lo + self.maybe.len()));
    }

    /// A commit begins that would leave `after`.
    fn begun(&mut self, vfs: &MemVfs, after: State) {
        self.maybe.push(after);
        self.mark(vfs);
    }

    /// The oldest `n` of the commits under way were acknowledged.
    fn acknowledged(&mut self, vfs: &MemVfs, n: usize) {
        let happened: Vec<State> = self.maybe.drain(..n).collect();
        self.states.extend(happened);
        self.mark(vfs);
    }

    /// The newest of the changes under way was acknowledged, and it was
    /// one that puts another database, or none, in the place of the one
    /// before: whatever else was under way on that one no longer matters.
    fn replaced(&mut self, vfs: &MemVfs) {
        let now = self.maybe.pop().expect("it was begun");
        self.maybe.clear();
        self.failed = false;
        self.states.push(now);
        self.mark(vfs);
    }

    /// Every state a crash now may leave, the last acknowledged first.
    fn possible(&self) -> Vec<State> {
        std::iter::once(self.now().clone()).chain(self.maybe.iter().cloned()).collect()
    }
}

/// Something to do in the middle of an operation of the engine's: asked
/// about each operation on the disk, it answers whether it has now been
/// done.
type During = Box<dyn FnMut(&Op) -> bool + Send>;

struct Hooks {
    faults: Option<Plan>,
    during: Option<During>,
}

/// One transaction of the workload, from its first statement to the
/// answer to its commit.
fn transact(vfs: &MemVfs, db: &Arc<Database>, book: &Mutex<Book>, fork: bool, stmts: &[Stmt], commit: bool) -> Result<()> {
    let mut txn = if fork { db.begin_fork()? } else { db.begin_write(WAIT, false)? };
    let mut after = {
        let book = book.lock().unwrap();
        // The engine cannot know whether a commit that failed is in the
        // log: it must take nothing more until it has restarted and
        // looked.
        assert!(!book.failed, "a writing transaction began after a commit had failed");
        book.now().clone()
    };
    for stmt in stmts {
        run(&mut txn, stmt)?;
        apply(&mut after, stmt);
    }
    if !commit {
        txn.rollback();
        return Ok(());
    }
    if fork {
        assert!(txn.attach(WAIT)?, "nothing was committed beside the fork");
    }
    book.lock().unwrap().begun(vfs, after);
    let committed = txn.commit();
    let mut book = book.lock().unwrap();
    match committed {
        Ok(()) => book.acknowledged(vfs, 1),
        Err(_) => book.failed = true,
    }
    committed
}

/// A workload under way.
struct Run {
    vfs: MemVfs,
    engine: Option<Engine>,
    db: Option<Arc<Database>>,
    beside: Option<(WriteTxn, u64)>,
    book: Arc<Mutex<Book>>,
    hooks: Arc<Mutex<Hooks>>,
    /// A creation, a drop or a restore failed half way: what the data
    /// directory holds, only a restart can tell, and until then the
    /// database is out of reach.
    unsettled: bool,
}

impl Run {
    /// A workload about to run on `vfs`, which fails as `faults` says.
    fn new(vfs: &MemVfs, faults: Option<Plan>) -> Run {
        let hooks = Arc::new(Mutex::new(Hooks { faults, during: None }));
        let shared = hooks.clone();
        vfs.set_plan(Some(Box::new(move |op| {
            let during = shared.lock().unwrap().during.take();
            if let Some(mut during) = during {
                if !during(op) {
                    shared.lock().unwrap().during = Some(during);
                }
            }
            match &mut shared.lock().unwrap().faults {
                Some(plan) => plan(op),
                None => Fault::None,
            }
        })));
        let mut book = Book { states: vec![State::default()], maybe: Vec::new(), failed: false, marks: Vec::new() };
        book.mark(vfs);
        Run { vfs: vfs.clone(), engine: None, db: None, beside: None, book: Arc::new(Mutex::new(book)), hooks, unsettled: false }
    }

    /// The states a crash may leave after `n` operations, as the first
    /// and the last of them in `states`.
    fn expected_at(&self, n: usize) -> (usize, usize) {
        let book = self.book.lock().unwrap();
        let (_, lo, hi) = book.marks.iter().rev().find(|(from, ..)| *from <= n).expect("the first mark is at nothing");
        (*lo, *hi)
    }

    /// The states a crash may leave after `n` operations.
    fn expected(&self, n: usize) -> Vec<State> {
        let (lo, hi) = self.expected_at(n);
        self.book.lock().unwrap().states[lo..=hi].to_vec()
    }

    fn possible(&self) -> Vec<State> {
        self.book.lock().unwrap().possible()
    }

    /// Every state the workload was told of, in order.
    fn states(&self) -> Vec<State> {
        self.book.lock().unwrap().states.clone()
    }

    fn db(&self) -> Result<Arc<Database>> {
        self.db.clone().ok_or_else(stopped)
    }

    fn step(&mut self, step: &Step) -> Result<()> {
        if self.unsettled && !matches!(step, Step::Start | Step::Stop { .. }) {
            return Err(stopped());
        }
        match step {
            Step::Start => self.start(),
            Step::Stop { clean } => self.stop(*clean),
            Step::Create => self.create(),
            Step::Drop => self.drop_database(),
            Step::Restore(dump) => self.restore(&dump.0, &dump.1),
            Step::Txn { fork, stmts, commit } => transact(&self.vfs, &self.db()?, &self.book, *fork, stmts, *commit),
            Step::Group { txns, told } => self.group(txns, *told),
            Step::Checkpoint { beside, when } => self.checkpoint(beside, *when),
            Step::Beside(what) => self.beside(what),
        }
    }

    fn start(&mut self) -> Result<()> {
        let engine = try_start(&self.vfs)?;
        // Whatever was under way when it stopped has happened or has not.
        let got = read_state(&engine);
        {
            let mut book = self.book.lock().unwrap();
            if got != *book.now() {
                let at = book.maybe.iter().position(|state| *state == got).unwrap_or_else(|| panic!("a restart gave a state that was never committed: {}", differ(&got, &book.possible())));
                let happened: Vec<State> = book.maybe.drain(..=at).collect();
                book.states.extend(happened);
            }
            book.maybe.clear();
            book.failed = false;
            book.mark(&self.vfs);
        }
        self.unsettled = false;
        verify_rest(&engine, &got, true);
        self.db = engine.database("test").ok();
        self.engine = Some(engine);
        Ok(())
    }

    fn stop(&mut self, clean: bool) -> Result<()> {
        self.beside = None;
        self.db = None;
        match self.engine.take() {
            Some(engine) if clean => engine.close(),
            _ => Ok(()),
        }
    }

    fn create(&mut self) -> Result<()> {
        let engine = self.engine.as_ref().ok_or_else(stopped)?;
        self.book.lock().unwrap().begun(&self.vfs, State { exists: true, ..State::default() });
        match engine.create_database("test") {
            Ok(db) => {
                self.db = Some(db);
                self.book.lock().unwrap().acknowledged(&self.vfs, 1);
                Ok(())
            }
            Err(e) => {
                self.unsettled = true;
                Err(e)
            }
        }
    }

    fn drop_database(&mut self) -> Result<()> {
        self.beside = None;
        self.db = None;
        let engine = self.engine.as_ref().ok_or_else(stopped)?;
        self.book.lock().unwrap().begun(&self.vfs, State::default());
        let dropped = engine.drop_database("test");
        match dropped {
            Ok(()) => self.book.lock().unwrap().replaced(&self.vfs),
            Err(_) => self.unsettled = true,
        }
        dropped
    }

    fn restore(&mut self, dump: &[u8], holds: &State) -> Result<()> {
        self.beside = None;
        self.db = None;
        let engine = self.engine.as_ref().ok_or_else(stopped)?;
        self.book.lock().unwrap().begun(&self.vfs, holds.clone());
        let restored = (|| {
            let mut restore = Restore::new(engine, true);
            for piece in dump.chunks(3_000) {
                restore.feed(piece)?;
            }
            restore.finish().map(|_| ())
        })();
        let mut book = self.book.lock().unwrap();
        match (&restored, engine.database("test")) {
            (Ok(()), db) => {
                book.replaced(&self.vfs);
                self.db = db.ok();
            }
            // The database it was to replace is still in place and in
            // use: the load stopped before it touched it.
            (Err(_), Ok(db)) if !db.has_failed() => {
                book.maybe.pop();
                book.mark(&self.vfs);
                self.db = Some(db);
            }
            (Err(_), _) => self.unsettled = true,
        }
        restored
    }

    fn group(&mut self, txns: &[Vec<Stmt>], told: bool) -> Result<()> {
        let db = self.db()?;
        let mut started = Vec::new();
        let mut after = self.book.lock().unwrap().now().clone();
        let mut failure = None;
        for stmts in txns {
            let ready = (|| {
                let mut txn = db.begin_write(WAIT, false)?;
                let mut next = after.clone();
                for stmt in stmts {
                    run(&mut txn, stmt)?;
                    apply(&mut next, stmt);
                }
                Ok((txn, next))
            })();
            let (txn, next) = match ready {
                Ok(ready) => ready,
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            };
            let mut book = self.book.lock().unwrap();
            assert!(!book.failed, "a writing transaction began after a commit had failed");
            book.begun(&self.vfs, next.clone());
            match txn.commit_start() {
                Ok(Some(committed)) => {
                    started.push(committed);
                    after = next;
                }
                // Its statements found nothing to change: there is no
                // commit to wait for.
                Ok(None) => {
                    book.maybe.pop();
                    book.mark(&self.vfs);
                }
                Err(e) => {
                    book.failed = true;
                    failure = Some(e);
                    break;
                }
            }
        }
        // Each waits for the disk only now, so the first sync serves all.
        let mut waiting = 0;
        for committed in started {
            waiting += 1;
            let outcome = if told {
                let (tell, told) = std::sync::mpsc::channel();
                let lead = db.when_durable(
                    committed.version(),
                    Box::new(move |outcome| {
                        let _ = tell.send(outcome);
                    }),
                );
                if lead {
                    while db.sync_waiters(1) {}
                }
                told.recv().expect("every waiter is told").map_err(Error::Db)
            } else {
                committed.wait()
            };
            let mut book = self.book.lock().unwrap();
            match outcome {
                // Durable, and so is every commit before it.
                Ok(()) => {
                    book.acknowledged(&self.vfs, waiting);
                    waiting = 0;
                }
                Err(e) => {
                    book.failed = true;
                    failure.get_or_insert(e);
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }

    fn checkpoint(&mut self, beside: &[Vec<Stmt>], when: When) -> Result<()> {
        fn commit_each(vfs: &MemVfs, db: &Arc<Database>, book: &Mutex<Book>, txns: &[Vec<Stmt>]) -> Result<()> {
            // Each is tried, whatever became of the one before.
            let mut outcome = Ok(());
            for stmts in txns {
                let committed = transact(vfs, db, book, false, stmts, true);
                outcome = outcome.and(committed);
            }
            outcome
        }
        let db = self.db()?;
        if beside.is_empty() {
            return db.checkpoint();
        }
        // The checkpoint, with the transactions committed from inside
        // the operation that marks the moment.
        let outcome: Arc<Mutex<Option<Result<()>>>> = Arc::default();
        {
            let (vfs, db, book, txns, outcome) = (self.vfs.clone(), db.clone(), self.book.clone(), beside.to_vec(), outcome.clone());
            self.hooks.lock().unwrap().during = Some(Box::new(move |op| {
                if !when.is(op) {
                    return false;
                }
                *outcome.lock().unwrap() = Some(commit_each(&vfs, &db, &book, &txns));
                true
            }));
        }
        let checkpointed = db.checkpoint();
        // A checkpoint with nothing to write never comes to that moment.
        let unfired = self.hooks.lock().unwrap().during.take().is_some();
        let committed = if unfired { commit_each(&self.vfs, &db, &self.book, beside) } else { outcome.lock().unwrap().take().expect("it was done") };
        checkpointed.and(committed)
    }

    fn beside(&mut self, what: &Beside) -> Result<()> {
        let db = self.db()?;
        match what {
            Beside::Begin => self.beside = db.begin_fork().ok().map(|fork| (fork, db.latest_version())),
            // Rows no committed transaction ever holds. A failure (the
            // disk gone, a name taken) is all the same to it.
            Beside::Write(id, item) => {
                if let Some((fork, _)) = &mut self.beside {
                    let _ = fork.insert(&insert_of(*id, item), None);
                }
            }
            // It can become the writer only if nobody has committed
            // since it began, and it is rolled back either way.
            Beside::End => {
                if let Some((mut fork, began)) = self.beside.take() {
                    if let Ok(attached) = fork.attach(WAIT) {
                        assert_eq!(attached, began == db.latest_version(), "a fork attaches exactly when no commit was made beside it");
                    }
                }
            }
        }
        Ok(())
    }
}

// ------------------------------------------------------- the power cuts

/// Whether a write is to the data file's header: the two pages a
/// checkpoint writes in turn.
fn is_header(entry: &Entry) -> bool {
    entry.op.path.extension().is_some_and(|e| e == "db") && entry.written().is_some_and(|(offset, _)| offset < 2 * PAGE_SIZE as u64)
}

/// Rebuilds the disk as it stood before each operation of `journal`, and
/// after the last, starting from `base`, and shows `visit` what each way
/// of cutting the power there would leave: how many operations had been
/// made, which way, and the disk.
fn each_cut(base: &MemVfs, journal: &[Entry], visit: &mut dyn FnMut(usize, &str, MemVfs)) {
    let disk = base.snapshot();
    for n in 0..=journal.len() {
        let last = n.checked_sub(1).map(|i| &journal[i]);
        // Killed, with the machine still up: the system keeps what the
        // process wrote.
        visit(n, "nothing lost", disk.crash_image_with(&mut |_| Fate::Kept));
        let unsynced = disk.unsynced();
        let synced = last.is_none_or(|entry| matches!(entry.op.kind, OpKind::Sync | OpKind::SyncDir));
        if unsynced > 0 {
            // What this leaves changes only when something is synced.
            if synced {
                visit(n, "everything unsynced lost", disk.crash_image_with(&mut |_| Fate::Lost));
            }
            if let Some((entry, (offset, len))) = last.and_then(|entry| Some((entry, entry.written()?))) {
                let at = entry.op.index;
                let touched = sectors(offset, len);
                let mut fates = Vec::new();
                if touched >= 2 && is_header(entry) {
                    // Every way to tear the page the next start reads first.
                    fates.extend((1..touched).map(Fate::Torn));
                    fates.extend((0..touched.min(64)).flat_map(|i| [Fate::Sectors(1 << i), Fate::Sectors(!(1 << i))]));
                } else if touched >= 2 {
                    let mut runs = vec![1, touched / 2, touched - 1];
                    runs.dedup();
                    fates.extend(runs.into_iter().map(Fate::Torn));
                    // Its end without its beginning, and every other sector.
                    fates.push(Fate::Sectors(1 << ((touched - 1) % 64)));
                    fates.push(Fate::Sectors(0xAAAA_AAAA_AAAA_AAAA));
                }
                for fate in fates {
                    visit(n, &format!("the last write {fate:?}"), disk.crash_image_with(&mut |unsynced| if unsynced.op == at { fate } else { Fate::Kept }));
                }
                if unsynced >= 2 {
                    visit(n, "only the last write kept", disk.crash_image_with(&mut |unsynced| if unsynced.op == at { Fate::Kept } else { Fate::Lost }));
                }
            }
            if unsynced >= 2 && !synced {
                for seed in 0..2 {
                    visit(n, &format!("unsynced writes scattered ({seed})"), disk.crash_image(n as u64 * 2 + seed));
                }
            }
        }
        if let Some(entry) = journal.get(n) {
            disk.apply(entry);
        }
    }
}

#[derive(Default)]
struct Tally {
    /// Disks a power cut left that were opened and compared.
    cuts: usize,
    /// Of those, cuts made while an earlier cut was being recovered from.
    in_recovery: usize,
}

/// Starts the engine on what a power cut left, and reads what it holds.
fn recover(image: &MemVfs, scene: &str) -> (Engine, State) {
    let engine = try_start(image).unwrap_or_else(|e| panic!("{scene}: the engine does not start: {e}"));
    let state = try_read_state(&engine).unwrap_or_else(|e| panic!("{scene}: the database does not read: {e}"));
    (engine, state)
}

/// How far a disk that a power cut left is followed.
#[derive(Clone, Copy, PartialEq)]
enum Follow {
    /// Started and compared.
    Start,
    /// And then written to, stopped and started again.
    Onward,
    /// And the recovery itself cut at every step.
    Recovery,
}

/// Checks one disk a power cut left: the engine starts on it, and holds
/// one of `possible`, with every index and every page in order.
fn check_cut(image: MemVfs, possible: &[State], scene: &str, follow: Follow, tally: &mut Tally) {
    let _scene = Scene(scene.to_string());
    let before = (follow == Follow::Recovery).then(|| image.snapshot());
    image.start_journal();
    let (engine, got) = recover(&image, scene);
    assert!(possible.contains(&got), "{scene}: recovery gave a state that was never committed: {}", differ(&got, possible));
    verify_rest(&engine, &got, true);
    tally.cuts += 1;
    let recovery = image.take_journal();
    if let Some(before) = before {
        // Killed again while it recovers, at every step of that: what
        // the first recovery found is what every later one must find.
        drop(engine);
        each_cut(&before, &recovery, &mut |n, what, image| {
            let scene = format!("{scene}; then, {n} operations into the recovery, a second cut with {what}");
            let _scene = Scene(scene.clone());
            let (engine, again) = recover(&image, &scene);
            assert!(again == got, "{scene}: the second recovery gave another state than the first: {}", differ(&again, std::slice::from_ref(&got)));
            verify_rest(&engine, &got, true);
            tally.cuts += 1;
            tally.in_recovery += 1;
        });
    } else if follow == Follow::Onward {
        carry_on(&image, engine, &got);
    }
}

/// A write after a recovery, a clean stop, and another start: the
/// database works on from whatever the cut left.
fn carry_on(disk: &MemVfs, engine: Engine, state: &State) {
    let mut after = state.clone();
    if state.items.is_some() {
        let probe = Stmt::Put(1_000_000, item(0, "after", "gamma", &[]));
        let db = engine.database("test").unwrap();
        let mut txn = db.begin_write(WAIT, false).unwrap();
        run(&mut txn, &probe).unwrap();
        txn.commit().unwrap();
        apply(&mut after, &probe);
    }
    engine.close().unwrap();
    verify_state(&try_start(disk).unwrap(), &after, true);
}

/// Runs a workload on a disk that does not fail, keeping every operation.
fn journaled(steps: &[Step]) -> (Run, Vec<Entry>) {
    let vfs = MemVfs::new();
    vfs.start_journal();
    let mut run = Run::new(&vfs, None);
    for (i, step) in steps.iter().enumerate() {
        run.step(step).unwrap_or_else(|e| panic!("step {i} ({step:?}) failed on a sound disk: {e}"));
    }
    run.stop(false).unwrap();
    (run, vfs.take_journal())
}

#[test]
fn a_kill_at_every_step_loses_no_commit_and_keeps_no_other() {
    let started = std::time::Instant::now();
    for (name, steps) in scripts() {
        let (run, journal) = journaled(&steps);
        // The workload made the engine do all it does to its files.
        let made = |wanted: fn(&Entry) -> bool| journal.iter().any(wanted);
        assert!(made(|entry| entry.op.kind == OpKind::Create) && made(|entry| matches!(entry.op.kind, OpKind::SetLen { .. })) && made(|entry| entry.op.kind == OpKind::Remove));
        assert!(made(|entry| matches!(entry.op.kind, OpKind::Rename { .. })) && made(|entry| entry.op.kind == OpKind::SyncDir) && made(is_header));
        // One cut in so many is followed through its recovery, step by
        // step, which costs as many more cuts as the recovery has steps
        // several times over; one in a few is written to afterwards.
        let every = if hunting() { 97 } else { 331 };
        let mut tally = Tally::default();
        let mut seen = 0usize;
        each_cut(&MemVfs::new(), &journal, &mut |n, what, image| {
            let next = journal.get(n).map_or("the end".to_string(), |entry| format!("{:?} of {}", entry.op.kind, entry.op.path.display()));
            let scene = format!("{name}, cut before operation {n} ({next}) with {what}");
            seen += 1;
            let follow = if seen.is_multiple_of(every) {
                Follow::Recovery
            } else if seen.is_multiple_of(8) {
                Follow::Onward
            } else {
                Follow::Start
            };
            check_cut(image, &run.expected(n), &scene, follow, &mut tally);
        });
        println!("{name}: {} operations, {} states, {} power cuts checked, {} of them during a recovery ({:?})", journal.len(), run.states().len(), tally.cuts, tally.in_recovery, started.elapsed());
        assert!(tally.cuts > journal.len() && tally.in_recovery > 0);
    }
}

// ------------------------------------------------------ the failing disk

/// Runs a workload on a disk that fails as `plan` says, and checks what
/// the engine made of it: nothing it acknowledged is taken back by a
/// restart, with the power cut or without, what was committed can still
/// be read while the disk fails, and after a commit has failed nothing
/// more is taken until the engine has restarted. Returns how many steps
/// of the workload failed.
///
/// `settles` says that a restart while the disk still fails, if it
/// succeeds, settles what the data directory holds: every later start
/// must then find the same. It does not after a sync of the directory
/// has failed: what that lost of the names in it is lost on the disk and
/// not to the running system, and a restart sees the system's side.
fn run_on_a_failing_disk(steps: &[Step], plan: Plan, settles: bool, scene: &str) -> usize {
    let _scene = Scene(scene.to_string());
    let vfs = MemVfs::new();
    let mut run = Run::new(&vfs, Some(plan));
    let mut failures = 0;
    for step in steps {
        if run.step(step).is_err() {
            failures += 1;
            if failures == 1 {
                still_reads(&run, scene);
            }
        }
    }
    // What readers are shown must be on disk: no restart may give less.
    let shown = still_reads(&run, scene);

    // Stopped as a killed process stops, and started while the disk may
    // still be failing: that start may fail too, but if it succeeds, what
    // it found is settled.
    let possible = run.possible().split_off(shown);
    let _ = run.stop(false);
    let mut settled = None;
    if let Ok(engine) = try_start(&vfs) {
        let got = read_state(&engine);
        assert!(possible.contains(&got), "{scene}: a restart on the failing disk gave a state that was never committed: {}", differ(&got, &possible));
        verify_rest(&engine, &got, true);
        settled = settles.then_some(got);
    }
    let possible = settled.map_or(possible, |state| vec![state]);

    // The fault is over: space has been made, the disk replaced. The
    // power may be cut before the engine is started again.
    vfs.set_plan(None);
    let cuts = [("everything unsynced lost", vfs.crash_image_with(&mut |_| Fate::Lost)), ("unsynced writes scattered", vfs.crash_image(failures as u64))];
    for (what, image) in cuts {
        let scene = format!("{scene}; then a power cut with {what}");
        let _scene = Scene(scene.clone());
        let (engine, got) = recover(&image, &scene);
        assert!(possible.contains(&got), "{scene}: recovery gave a state that was never committed: {}", differ(&got, &possible));
        verify_rest(&engine, &got, true);
    }
    // Or not: then the engine starts on the disk as the process left it,
    // and works on.
    let scene = format!("{scene}; then a restart");
    let (engine, got) = recover(&vfs, &scene);
    assert!(possible.contains(&got), "{scene}: it gave a state that was never committed: {}", differ(&got, &possible));
    verify_rest(&engine, &got, true);
    carry_on(&vfs, engine, &got);
    failures
}

/// While the disk fails, readers still see what was committed: all of
/// what was acknowledged, and of the commits that failed either nothing
/// or, if one reached the disk before its owner was told otherwise, all
/// of it. Returns which of `Run::possible` they see.
fn still_reads(run: &Run, scene: &str) -> usize {
    // A drop or a restore that failed half way leaves the database out
    // of reach until the engine has restarted.
    let Some(engine) = run.engine.as_ref().filter(|_| !run.unsettled) else { return 0 };
    let got = try_read_state(engine).unwrap_or_else(|e| panic!("{scene}: the database no longer reads: {e}"));
    let possible = run.possible();
    let shown = possible.iter().position(|state| *state == got).unwrap_or_else(|| panic!("{scene}: readers see a state that was never committed: {}", differ(&got, &possible)));
    let Some(db) = &run.db else { return shown };
    compare_rest(db, &got, false);
    // Once the disk has failed under a commit or a checkpoint, what the
    // engine holds in memory may be half of one: it then declines to
    // check its files until it has restarted. It never calls them
    // damaged, which they are not.
    match db.check() {
        Ok(_) => {}
        Err(Error::Db(e)) if e.code == ErrorCode::Unavailable && db.has_failed() => {}
        Err(e) => panic!("{scene}: the check of the files failed: {e}"),
    }
    shown
}

#[test]
fn a_full_disk_fails_commits_and_loses_none() {
    let started = std::time::Instant::now();
    for (name, steps) in scripts() {
        let (_, journal) = journaled(&steps);
        // The disk fills at each operation that needs room: from then on
        // no file can be made, and none grows.
        let (mut points, mut failures) = (0, 0);
        for entry in &journal {
            if !matches!(entry.op.kind, OpKind::Create | OpKind::Write { grows: 1.., .. }) {
                continue;
            }
            let from = entry.op.index;
            let plan: Plan = Box::new(move |op| match &op.kind {
                _ if op.index < from => Fault::None,
                OpKind::Create => Fault::Fail(std::io::ErrorKind::StorageFull),
                // What lies within the file is written; the rest is not.
                OpKind::Write { len, grows, .. } if *grows > 0 => Fault::Short(len.saturating_sub(*grows as usize)),
                _ => Fault::None,
            });
            failures += run_on_a_failing_disk(&steps, plan, true, &format!("{name}, with the disk full from operation {from} ({:?} of {})", entry.op.kind, entry.op.path.display()));
            points += 1;
        }
        println!("{name}: the disk filled at each of {points} operations, {failures} steps failed ({:?})", started.elapsed());
        assert!(points > 20 && failures >= points);
    }
}

#[test]
fn an_operation_that_fails_loses_no_commit() {
    let started = std::time::Instant::now();
    for (name, steps) in scripts() {
        let (_, journal) = journaled(&steps);
        // Each operation fails, once: a write, a truncation, a creation,
        // a removal or a renaming with an error and no effect, a sync
        // with an error after which what it covered is on the disk or is
        // not, and is no longer the system's to write.
        let (mut syncs, mut others, mut failures) = (0, 0, 0);
        for entry in &journal {
            let sync = matches!(entry.op.kind, OpKind::Sync | OpKind::SyncDir);
            let at = entry.op.index;
            let plan: Plan = Box::new(move |op| match op.index == at {
                true if sync => Fault::SyncLoses(at),
                true => Fault::Fail(std::io::ErrorKind::Other),
                false => Fault::None,
            });
            let settles = entry.op.kind != OpKind::SyncDir;
            failures += run_on_a_failing_disk(&steps, plan, settles, &format!("{name}, with operation {at} failing ({:?} of {})", entry.op.kind, entry.op.path.display()));
            *(if sync { &mut syncs } else { &mut others }) += 1;
        }
        println!("{name}: each of {syncs} syncs and {others} other operations failed once, {failures} steps failed ({:?})", started.elapsed());
        assert!(syncs > 20 && others > 20 && failures >= syncs);
    }
}

// --------------------------------------------------------- the lying disk

/// Which files' syncs a disk lies about.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Lies {
    Log,
    Data,
    All,
}

impl Lies {
    fn about(self, op: &Op) -> bool {
        let log = op.path.to_string_lossy().contains(".wal-");
        matches!(op.kind, OpKind::Sync | OpKind::SyncDir)
            && match self {
                Lies::Log => log,
                Lies::Data => op.path.extension().is_some_and(|e| e == "db"),
                Lies::All => true,
            }
    }
}

/// What became of a database on a disk that had lied.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
enum Found {
    /// The engine would not start, and said the files are damaged.
    Refused,
    /// It started, and a read or the check of the files said they are
    /// damaged, before any read had given what was never there.
    Damaged,
    /// It started whole, as it stood at an earlier commit.
    Earlier,
    /// It started whole, as it stood at the last acknowledged commit, or
    /// with the commits under way as well.
    Latest,
    /// It started, and reads gave what it never held; the check of the
    /// files said they are damaged.
    Misread,
    /// It started, reads gave what it never held, and the check of the
    /// files found nothing wrong.
    Unnoticed,
}

/// Starts the engine on what a power cut left of a disk that lied, and
/// says what was found. `states` are those the database went through,
/// the last `under_way` of them not yet acknowledged.
fn found(image: &MemVfs, states: &[State], under_way: usize, scene: &str) -> Found {
    let _scene = Scene(scene.to_string());
    // Whatever it says, it says that the files are damaged: it does not
    // blame the disk for failing now, and it does not panic.
    let damage = |e: Error, doing: &str| match e {
        Error::Corrupt(_) => {}
        Error::Db(e) if e.code == ErrorCode::Corrupt => {}
        other => panic!("{scene}: {doing} failed with an error that does not say the files are damaged: {other}"),
    };
    let engine = match try_start(image) {
        Ok(engine) => engine,
        Err(e) => {
            damage(e, "the start");
            return Found::Refused;
        }
    };
    let read = try_read_state(&engine);
    let checked = match engine.database("test") {
        Ok(db) => db.check().map(|_| ()),
        Err(_) => Ok(()),
    };
    let sound = match checked {
        Ok(()) => true,
        Err(e) => {
            damage(e, "the check");
            false
        }
    };
    let state = match read {
        Ok(state) => state,
        Err(Unread::Failed(e)) => {
            damage(e, "a read");
            return Found::Damaged;
        }
        Err(Unread::Wrong(_)) => return if sound { Found::Unnoticed } else { Found::Misread },
    };
    match (states.iter().position(|earlier| *earlier == state), sound) {
        (None, true) => Found::Unnoticed,
        (None, false) => Found::Misread,
        (Some(_), false) => Found::Damaged,
        (Some(at), true) => {
            if state.exists {
                compare_rest(&engine.database("test").unwrap(), &state, true);
            }
            if at + under_way + 1 >= states.len() {
                Found::Latest
            } else {
                Found::Earlier
            }
        }
    }
}

/// Runs a workload on disks that are honest up to some sync and lie from
/// then on, cuts the power after that, and counts what the engine finds:
/// of what was written since the first lie, each write is lost, kept or
/// torn on its own. `stride` picks the syncs that are the first to lie,
/// and `apart` how many operations lie between two cuts. Returns the
/// counts, and the cuts after which the engine gave what the database
/// never held.
fn lied_to(name: &str, steps: &[Step], lies: Lies, stride: usize, apart: usize) -> (HashMap<Found, usize>, Vec<String>) {
    let (_, honest) = journaled(steps);
    let liars: Vec<u64> = honest.iter().filter(|entry| lies.about(&entry.op)).map(|entry| entry.op.index).collect();
    let mut tally: HashMap<Found, usize> = HashMap::new();
    let mut taken_at_its_word = Vec::new();
    for from in liars.iter().step_by(stride.max(1)).copied() {
        let vfs = MemVfs::new();
        vfs.start_journal();
        let mut run = Run::new(&vfs, Some(Box::new(move |op| if op.index >= from && lies.about(op) { Fault::Lie } else { Fault::None })));
        for (i, step) in steps.iter().enumerate() {
            run.step(step).unwrap_or_else(|e| panic!("step {i} ({step:?}) failed on a disk that only lies: {e}"));
        }
        run.stop(false).unwrap();
        let journal = vfs.take_journal();
        let states = run.states();
        let first = journal.iter().position(|entry| entry.op.index > from).unwrap_or(journal.len());
        let disk = MemVfs::new();
        for n in 0..=journal.len() {
            if n > first && (n - first) % apart == 1 % apart {
                let (acknowledged, reached) = run.expected_at(n);
                let cuts = std::iter::once(("everything unsynced lost".to_string(), disk.crash_image_with(&mut |_| Fate::Lost)));
                let cuts = cuts.chain((0..2).map(|seed| (format!("unsynced writes scattered ({seed})"), disk.crash_image(n as u64 * 2 + seed))));
                for (what, image) in cuts {
                    let scene = format!("{name}, on a disk that lies about {lies:?} syncs from operation {from}, cut before operation {n} with {what}");
                    let found = found(&image, &states[..=reached], reached - acknowledged, &scene);
                    *tally.entry(found).or_default() += 1;
                    if matches!(found, Found::Misread | Found::Unnoticed) {
                        taken_at_its_word.push(scene);
                    }
                }
            }
            if let Some(entry) = journal.get(n) {
                disk.apply(entry);
            }
        }
    }
    let count = |found: Found| tally.get(&found).copied().unwrap_or(0);
    println!(
        "{name}, lying about {lies:?} syncs: {} cuts: {} refused, {} damaged, {} an earlier state, {} the latest, {} misread, {} unnoticed",
        tally.values().sum::<usize>(),
        count(Found::Refused),
        count(Found::Damaged),
        count(Found::Earlier),
        count(Found::Latest),
        count(Found::Misread),
        count(Found::Unnoticed),
    );
    (tally, taken_at_its_word)
}

#[test]
fn a_log_that_lies_about_its_syncs_gives_an_earlier_state_or_is_found_out() {
    use Stmt::*;
    // The case that needs finding out: commits that the log's sync lied
    // about, a checkpoint that begins the next log file on the strength
    // of that sync, and a commit made beside the checkpoint, which goes
    // to the new file. If the power is cut before the checkpoint is
    // done, the old file has lost its end and the new one has a record
    // that was made after it. Every sync from the first on lies in turn,
    // and the power is cut at every step after.
    let beside = vec![
        Step::Start,
        Step::Create,
        txn([Tables, Put(1, item(0, "one", "gamma", &["t1"]))]),
        checkpoint(),
        txn([Put(2, item(1, "two", "beta", &[]))]),
        txn([Change { id: 1, owner: 2, tags: vec![], body: None }]),
        Step::Checkpoint { beside: vec![vec![Put(3, item(3, "three", "gamma", &["t1"]))]], when: When::Pages },
        txn([Remove(2)]),
    ];
    let (tally, taken_at_its_word) = lied_to("a commit beside a checkpoint", &beside, Lies::Log, 1, 1);
    assert!(taken_at_its_word.is_empty(), "a log that lied was taken at its word:\n{}", taken_at_its_word.join("\n"));
    assert!(tally.get(&Found::Refused).is_some_and(|n| *n > 0), "no cut left a log with a gap in it");
    // And the workloads, less closely.
    for (name, steps) in scripts() {
        let (stride, apart) = if hunting() { (2, 5) } else { (9, 13) };
        let (tally, taken_at_its_word) = lied_to(&name, &steps, Lies::Log, stride, apart);
        // A log that lost records ends early, or is known to be damaged:
        // what it gives is the database as it stood at some commit.
        assert!(taken_at_its_word.is_empty(), "a log that lied was taken at its word:\n{}", taken_at_its_word.join("\n"));
        assert!(tally.get(&Found::Earlier).is_some_and(|n| *n > 0), "a log that lies loses acknowledged commits");
    }
}

#[test]
fn a_data_file_that_lies_about_its_syncs_is_found_out_as_far_as_checksums_can_tell() {
    for (name, steps) in scripts() {
        for lies in [Lies::Data, Lies::All] {
            let (stride, apart) = if hunting() { (2, 5) } else { (9, 13) };
            let (tally, _) = lied_to(&name, &steps, lies, stride, apart);
            // Whatever the engine made of each disk, it did not panic,
            // and it blamed nothing but the files (see `found`). More
            // cannot be asked: what a data file has lost, the checksums
            // of its pages do not always show. Where a page was not
            // written, the page that was there before is whole, and a
            // read that comes to it is given the rows of another time, or
            // of another table. The check of the files finds most of
            // these and not all. A page would have to carry what the
            // page that points to it expects of it.
            let told = [Found::Refused, Found::Damaged].iter().map(|found| tally.get(found).copied().unwrap_or(0)).sum::<usize>();
            assert!(told > 0, "no cut left a data file that was known to be damaged");
        }
    }
}
