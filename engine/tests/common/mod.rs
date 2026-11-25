//! What the tests that crash the engine share: the table they write to,
//! the model of what it should hold, and the comparison of the two.

#![allow(dead_code)]

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

pub const DIR: &str = "/data";
pub const WAIT: Duration = Duration::from_secs(5);

pub fn options() -> EngineOptions {
    // Small limits, so that pages are written early, evicted and read
    // back, and checkpoints happen, within a short test; and full-text
    // segments of a handful of rows, so that they are made, merged and
    // cleaned of dead rows all the time.
    database_engine::fts::set_pending_bytes(400);
    // The engine's own thread merges no full-text segments here: what it
    // does on the disk, and when, would differ from one run to the next,
    // and these tests have every run be the same. A test that wants
    // merges beside its writers makes them by hand, or lets the thread go.
    EngineOptions { cache_bytes: 64 * 8192, db: Options { sync: true, limits: Limits { max_dirty: 24, max_private: 24 }, checkpoint_bytes: 64 * 1024, ..Options::default() }, merging_paused: true, ..EngineOptions::default() }
}

pub fn try_open_with(vfs: &MemVfs, options: EngineOptions) -> database_engine::error::Result<Engine> {
    Engine::open_with(Arc::new(vfs.clone()), Path::new(DIR), options, None)
}

pub fn try_open(vfs: &MemVfs) -> database_engine::error::Result<Engine> {
    try_open_with(vfs, options())
}

pub fn open(vfs: &MemVfs) -> Engine {
    try_open(vfs).expect("the engine opens")
}

pub fn items() -> TableDef {
    TableDef::new("items")
        .column(ColumnDef::new("id", Type::Int))
        .column(ColumnDef::new("owner", Type::Int))
        .column(ColumnDef::new("name", Type::Text))
        .column(ColumnDef::new("body", Type::Text).default("").keep_start(48))
        .column(ColumnDef::new("tags", Type::list(Type::Text)).default(Vec::<String>::new()))
        .primary_key(["id"])
        .unique("items_name", ["name"])
        .index(IndexDef::new("items_owner").on(["owner", "id"]))
        .index(IndexDef::new("items_tags").on(["owner"]).on_each(col("tags")))
        .index(IndexDef::new("items_search").on(["owner"]).full_text(TextConfig::Simple, [(col("body"), 1.0)]))
}

/// What a row of `items` holds, as the model keeps it.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub owner: i64,
    pub name: String,
    pub body: String,
    pub tags: Vec<String>,
}

pub type Model = BTreeMap<i64, Item>;

pub fn insert_of(id: i64, item: &Item) -> Insert {
    Insert::into("items").set("id", id).set("owner", item.owner).set("name", item.name.as_str()).set("body", item.body.as_str()).set("tags", item.tags.clone())
}

pub fn random_item(rng: &mut Rng, id: i64) -> Item {
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
pub fn read_model(db: &Arc<Database>) -> Model {
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
pub fn verify(db: &Arc<Database>, model: &Model) {
    let got = read_model(db);
    assert_eq!(got.len(), model.len(), "row count");
    for (id, item) in model {
        assert_eq!(got.get(id), Some(item), "row {id}");
    }
    verify_indexes(db, model);
}

/// Compares what each index of the table gives with the model, and checks
/// that the files are consistent.
pub fn verify_indexes(db: &Arc<Database>, model: &Model) {
    compare_indexes(db, model);
    db.check().expect("the files are consistent");
}

/// Compares what each index of the table gives with the model.
pub fn compare_indexes(db: &Arc<Database>, model: &Model) {
    let txn = db.begin_read();
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
    // The start a row keeps of a large body is how the body starts.
    let heads = txn.query(&Query::table("items").select("id", col("id")).select("head", col("body").left(40)), None).unwrap();
    for r in &heads.rows {
        let (Value::Int(id), Value::Text(head)) = (&r[0], &r[1]) else { panic!("unexpected row {r:?}") };
        assert_eq!(*head, model[id].body.chars().take(40).collect::<String>(), "the start of body {id}");
    }
}

pub fn create(engine: &Engine) -> Arc<Database> {
    let db = engine.create_database("test").unwrap();
    let mut txn = db.begin_write(WAIT, false).unwrap();
    txn.ddl(&Ddl::CreateTable { def: items(), if_not_exists: false }, None).unwrap();
    txn.commit().unwrap();
    db
}
