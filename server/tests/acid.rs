//! Atomicity, consistency and isolation, through the client, with several
//! connections at once. Durability against a killed process is in
//! `durability.rs`.

mod common;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;

async fn accounts(db: &Client, n: i64, balance: i64) -> DbResult<()> {
    db.create_table(
        TableDef::new("accounts")
            .column(ColumnDef::new("id", Type::Int))
            .column(ColumnDef::new("balance", Type::Int))
            .primary_key(["id"])
            .check("not_overdrawn", col("balance").ge(0)),
    )
    .await?;
    db.create_table(
        TableDef::new("ledger")
            .column(ColumnDef::new("id", Type::Int).auto_increment())
            .column(ColumnDef::new("from_id", Type::Int))
            .column(ColumnDef::new("to_id", Type::Int))
            .column(ColumnDef::new("amount", Type::Int))
            .primary_key(["id"])
            .foreign_key(["from_id"], "accounts", OnDelete::Restrict)
            .foreign_key(["to_id"], "accounts", OnDelete::Restrict),
    )
    .await?;
    let mut insert = Insert::into("accounts").columns(["id", "balance"]);
    for id in 1..=n {
        insert = insert.values([val(id), val(balance)]);
    }
    db.insert(insert).await?;
    Ok(())
}

async fn total(db: &impl Db) -> DbResult<i64> {
    db.query_one(Query::table("accounts").select("total", sum(col("balance")))).await?.get("total")
}

// R-X1: all of a transaction's statements, or none.
#[tokio::test(flavor = "multi_thread")]
async fn a_transaction_applies_whole_or_not_at_all() -> DbResult<()> {
    let (_server, db) = fresh().await;
    accounts(&db, 4, 100).await?;

    // Rolled back by the caller.
    let tx = db.begin().await?;
    tx.update(Update::table("accounts").key([1]).set("balance", col("balance") - 60)).await?;
    tx.update(Update::table("accounts").key([2]).set("balance", col("balance") + 60)).await?;
    assert_eq!(tx.get("accounts", [1]).await?.unwrap().get::<i64>("balance")?, 40, "a transaction sees its own writes");
    tx.rollback().await?;
    assert_eq!(db.get("accounts", [1]).await?.unwrap().get::<i64>("balance")?, 100);

    // Rolled back by a failing statement: the first update is undone with it.
    let failed: Result<(), DbError> = db
        .transaction(|tx| async move {
            tx.update(Update::table("accounts").key([1]).set("balance", col("balance") - 60)).await?;
            tx.update(Update::table("accounts").key([2]).set("balance", col("balance") - 500)).await?;
            Ok(())
        })
        .await;
    assert_eq!(failed.unwrap_err().code, ErrorCode::CheckViolation);
    assert_eq!(total(&db).await?, 400);
    assert_eq!(db.get("accounts", [1]).await?.unwrap().get::<i64>("balance")?, 100);

    // Rolled back by being dropped.
    {
        let tx = db.begin().await?;
        tx.delete(Delete::from("accounts").key([4])).await?;
    }
    assert!(db.get("accounts", [4]).await?.is_some());

    // After a failure the transaction refuses everything, commit included.
    let tx = db.begin().await?;
    tx.update(Update::table("accounts").key([3]).set("balance", 7)).await?;
    assert!(tx.insert(Insert::into("accounts").set("id", 1).set("balance", 0)).await.is_err());
    assert_eq!(tx.query(Query::table("accounts")).await.unwrap_err().code, ErrorCode::TransactionAborted);
    assert_eq!(tx.commit().await.unwrap_err().code, ErrorCode::TransactionAborted);
    assert_eq!(db.get("accounts", [3]).await?.unwrap().get::<i64>("balance")?, 100);

    // Committed.
    db.transaction(|tx| async move {
        tx.update(Update::table("accounts").key([1]).set("balance", col("balance") - 60)).await?;
        tx.update(Update::table("accounts").key([2]).set("balance", col("balance") + 60)).await?;
        Ok::<_, DbError>(())
    })
    .await?;
    assert_eq!(db.get("accounts", [2]).await?.unwrap().get::<i64>("balance")?, 160);
    Ok(())
}

// R-X1: one statement over many rows is atomic too.
#[tokio::test(flavor = "multi_thread")]
async fn a_statement_that_fails_on_its_last_row_changes_no_row() -> DbResult<()> {
    let (_server, db) = fresh().await;
    accounts(&db, 3, 100).await?;
    let mut insert = Insert::into("accounts").columns(["id", "balance"]);
    for id in 10..500 {
        insert = insert.values([val(id), val(1)]);
    }
    // The last row repeats a key.
    let err = db.insert(insert.values([val(2), val(1)])).await.unwrap_err();
    assert!(err.is_unique_violation() && err.violates(PRIMARY));
    assert_eq!(db.query_one(Query::table("accounts").select("n", count())).await?.get::<i64>("n")?, 3);

    // An update that would overdraw the last account it reaches.
    db.update(Update::table("accounts").key([3]).set("balance", 10)).await?;
    let err = db.update(Update::table("accounts").set("balance", col("balance") - 50)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::CheckViolation);
    assert_eq!(total(&db).await?, 210);

    // A delete stopped by a row that is still pointed at.
    db.insert(Insert::into("ledger").set("from_id", 1).set("to_id", 3).set("amount", 5)).await?;
    let err = db.delete(Delete::from("accounts")).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::ForeignKeyViolation);
    assert_eq!(db.query_one(Query::table("accounts").select("n", count())).await?.get::<i64>("n")?, 3);
    Ok(())
}

// R-X2: nothing uncommitted is ever visible to another connection.
#[tokio::test(flavor = "multi_thread")]
async fn readers_see_only_what_is_committed() -> DbResult<()> {
    let (_server, db) = fresh().await;
    accounts(&db, 2, 100).await?;

    let tx = db.begin().await?;
    tx.update(Update::table("accounts").key([1]).set("balance", 1)).await?;
    tx.insert(Insert::into("accounts").set("id", 3).set("balance", 5)).await?;
    tx.delete(Delete::from("accounts").key([2])).await?;
    // Another connection, while the transaction is open.
    assert_eq!(db.get("accounts", [1]).await?.unwrap().get::<i64>("balance")?, 100);
    assert!(db.get("accounts", [3]).await?.is_none());
    assert!(db.get("accounts", [2]).await?.is_some());
    assert_eq!(db.query(Query::table("accounts")).await?.len(), 2);
    tx.commit().await?;
    // And at once after it commits.
    assert_eq!(db.get("accounts", [1]).await?.unwrap().get::<i64>("balance")?, 1);
    assert!(db.get("accounts", [3]).await?.is_some());
    assert!(db.get("accounts", [2]).await?.is_none());
    Ok(())
}

// R-Q21, R-X2: a reading transaction sees one moment, whatever commits.
#[tokio::test(flavor = "multi_thread")]
async fn a_reading_transaction_keeps_its_moment() -> DbResult<()> {
    let (_server, db) = fresh().await;
    accounts(&db, 3, 100).await?;
    let reader = db.begin_read().await?;
    assert_eq!(total(&reader).await?, 300);

    db.update(Update::table("accounts").set("balance", 0)).await?;
    db.delete(Delete::from("accounts").key([3])).await?;
    db.insert(Insert::into("accounts").set("id", 9).set("balance", 1)).await?;
    db.admin().checkpoint().await?;

    assert_eq!(total(&reader).await?, 300);
    assert_eq!(reader.query(Query::table("accounts")).await?.len(), 3);
    assert!(reader.get("accounts", [9]).await?.is_none());
    // It cannot write.
    assert!(reader.insert(Insert::into("accounts").set("id", 10).set("balance", 1)).await.is_err());
    reader.rollback().await?;
    assert_eq!(total(&db).await?, 1);
    Ok(())
}

// R-X2, R-X3: money moves between accounts from many connections at once.
// Every transfer reads two balances and writes them back: without
// isolation updates would be lost and the total would drift. Meanwhile
// readers add up all balances and must always get the same total.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_transfers_never_lose_or_make_money() -> DbResult<()> {
    let (server, db) = fresh().await;
    const ACCOUNTS: i64 = 8;
    accounts(&db, ACCOUNTS, 1000).await?;
    let expected = ACCOUNTS * 1000;
    let done = Arc::new(AtomicBool::new(false));
    let moved = Arc::new(AtomicU64::new(0));

    let mut writers = Vec::new();
    for worker in 0..6u64 {
        let db = server.client("test").await;
        let moved = moved.clone();
        writers.push(tokio::spawn(async move {
            let mut seed = worker * 7919 + 1;
            for _ in 0..60 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let from = (seed >> 33) as i64 % ACCOUNTS + 1;
                let to = (seed >> 40) as i64 % ACCOUNTS + 1;
                let amount = (seed >> 50) as i64 % 300;
                if from == to {
                    continue;
                }
                let outcome: Result<(), DbError> = db
                    .transaction(|tx| async move {
                        // Read, decide, write: no lock is asked for.
                        let a: i64 = tx.get("accounts", [from]).await?.unwrap().get("balance")?;
                        let b: i64 = tx.get("accounts", [to]).await?.unwrap().get("balance")?;
                        if a < amount {
                            return Ok(());
                        }
                        tx.update(Update::table("accounts").key([from]).set("balance", a - amount)).await?;
                        tx.update(Update::table("accounts").key([to]).set("balance", b + amount)).await?;
                        tx.insert(Insert::into("ledger").set("from_id", from).set("to_id", to).set("amount", amount)).await?;
                        Ok(())
                    })
                    .await;
                outcome.expect("a transfer never fails");
                moved.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    let mut readers = Vec::new();
    for _ in 0..3 {
        let db = db.clone();
        let done = done.clone();
        readers.push(tokio::spawn(async move {
            let mut reads = 0u64;
            while !done.load(Ordering::Relaxed) {
                // One statement sees one moment.
                assert_eq!(total(&db).await.unwrap(), expected);
                // So do several statements in a reading transaction.
                let tx = db.begin_read().await.unwrap();
                let mut sum = 0;
                for id in 1..=ACCOUNTS {
                    sum += tx.get("accounts", [id]).await.unwrap().unwrap().get::<i64>("balance").unwrap();
                }
                assert_eq!(sum, expected);
                reads += 1;
            }
            reads
        }));
    }
    for writer in writers {
        writer.await.unwrap();
    }
    done.store(true, Ordering::Relaxed);
    for reader in readers {
        assert!(reader.await.unwrap() > 0);
    }
    assert_eq!(total(&db).await?, expected);
    // The ledger agrees with the balances: every transfer is in both.
    let ledger = db.query(Query::table("ledger")).await?;
    let mut balances = vec![1000i64; ACCOUNTS as usize + 1];
    for entry in &ledger {
        balances[entry.get::<i64>("from_id")? as usize] -= entry.get::<i64>("amount")?;
        balances[entry.get::<i64>("to_id")? as usize] += entry.get::<i64>("amount")?;
    }
    for id in 1..=ACCOUNTS {
        assert_eq!(db.get("accounts", [id]).await?.unwrap().get::<i64>("balance")?, balances[id as usize]);
    }
    assert!(moved.load(Ordering::Relaxed) > 100);
    db.admin().check().await?;
    Ok(())
}

// R-X3: a counter bumped by read-then-write from many tasks counts every bump.
#[tokio::test(flavor = "multi_thread")]
async fn no_update_is_lost_between_a_read_and_the_write_that_follows() -> DbResult<()> {
    let (_server, db) = fresh().await;
    db.create_table(TableDef::new("counters").column(ColumnDef::new("name", Type::Text)).column(ColumnDef::new("n", Type::Int)).primary_key(["name"])).await?;
    db.insert(Insert::into("counters").set("name", "hits").set("n", 0)).await?;
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let db = db.clone();
        tasks.push(tokio::spawn(async move {
            for round in 0..25 {
                if round % 2 == 0 {
                    // As a transaction: read, then write what was read plus one.
                    db.transaction(|tx| async move {
                        let n: i64 = tx.get("counters", ["hits"]).await?.unwrap().get("n")?;
                        tx.update(Update::table("counters").key(["hits"]).set("n", n + 1)).await?;
                        Ok::<_, DbError>(())
                    })
                    .await
                    .unwrap();
                } else {
                    // As one statement.
                    db.update(Update::table("counters").key(["hits"]).set("n", col("n") + 1)).await.unwrap();
                }
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(db.get("counters", ["hits"]).await?.unwrap().get::<i64>("n")?, 200);
    Ok(())
}

// R-X3: of many tasks inserting the same key, one wins; the unique index
// holds under concurrency.
#[tokio::test(flavor = "multi_thread")]
async fn a_unique_key_is_given_to_exactly_one_of_many_racing_inserts() -> DbResult<()> {
    let (_server, db) = fresh().await;
    db.create_table(
        TableDef::new("names")
            .column(ColumnDef::new("id", Type::Int).auto_increment())
            .column(ColumnDef::new("name", Type::Text))
            .column(ColumnDef::new("owner", Type::Int))
            .primary_key(["id"])
            .index(IndexDef::new("names_name").on_expr(col("name").lower()).unique()),
    )
    .await?;
    let mut tasks = Vec::new();
    for owner in 0..12i64 {
        let db = db.clone();
        tasks.push(tokio::spawn(async move {
            let mut won = 0;
            for n in 0..20 {
                let name = if owner % 2 == 0 { format!("Name{n}") } else { format!("name{n}") };
                match db.insert(Insert::into("names").set("name", name).set("owner", owner)).await {
                    Ok(_) => won += 1,
                    Err(e) => assert!(e.violates("names_name"), "{e}"),
                }
            }
            won
        }));
    }
    let mut won = 0;
    for task in tasks {
        won += task.await.unwrap();
    }
    assert_eq!(won, 20);
    assert_eq!(db.query(Query::table("names")).await?.len(), 20);
    Ok(())
}

// R-X8: a writer that cannot get the lock in time is told so, and the
// lock passes on when its holder ends.
#[tokio::test(flavor = "multi_thread")]
async fn a_writer_waits_for_the_write_lock_and_times_out() -> DbResult<()> {
    let (server, db) = fresh().await;
    accounts(&db, 2, 100).await?;
    let impatient = server.client_with("test", |c| c.lock_wait = Duration::from_millis(150)).await;

    let holder = db.begin().await?;
    holder.update(Update::table("accounts").key([1]).set("balance", 1)).await?;
    let started = std::time::Instant::now();
    let err = impatient.begin().await.err().expect("the lock is taken");
    assert_eq!(err.code, ErrorCode::LockTimeout);
    assert!(started.elapsed() >= Duration::from_millis(140));
    // Reading does not wait.
    assert_eq!(total(&impatient).await?, 200);

    // A patient writer gets its turn when the holder commits.
    let waiting = tokio::spawn({
        let db = db.clone();
        async move { db.update(Update::table("accounts").key([2]).set("balance", col("balance") + 1)).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiting.is_finished());
    holder.commit().await?;
    waiting.await.unwrap()?;
    assert_eq!(total(&db).await?, 102);
    Ok(())
}

// R-X8: a client that dies or dawdles with the write lock does not keep it.
#[tokio::test(flavor = "multi_thread")]
async fn an_abandoned_transaction_gives_the_lock_back() -> DbResult<()> {
    let server = TestServer::start_with(TempDir::new(), |c| c.idle_transaction = Duration::from_millis(200)).await;
    let db = server.client("test").await;
    accounts(&db, 2, 100).await?;

    // Dropped by the client: rolled back at once.
    {
        let tx = db.begin().await?;
        tx.update(Update::table("accounts").key([1]).set("balance", 0)).await?;
    }
    db.update(Update::table("accounts").key([2]).set("balance", 150)).await?;
    assert_eq!(total(&db).await?, 250);

    // Left idle: the server rolls it back after its patience runs out.
    let idle = db.begin().await?;
    idle.update(Update::table("accounts").key([1]).set("balance", 0)).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    db.update(Update::table("accounts").key([2]).set("balance", 200)).await?;
    assert_eq!(total(&db).await?, 300);
    assert_eq!(idle.commit().await.unwrap_err().code, ErrorCode::Connection);
    Ok(())
}

// R-X9: a statement that runs past its deadline is stopped and leaves
// nothing behind.
#[tokio::test(flavor = "multi_thread")]
async fn a_statement_past_its_deadline_is_stopped() -> DbResult<()> {
    let (server, db) = fresh().await;
    db.create_table(TableDef::new("big").column(ColumnDef::new("id", Type::Int)).column(ColumnDef::new("text", Type::Text)).primary_key(["id"])).await?;
    for batch in 0..20 {
        let mut insert = Insert::into("big").columns(["id", "text"]);
        for n in 0..500 {
            insert = insert.values([val(batch * 500 + n), val("lorem ipsum dolor sit amet ".repeat(40))]);
        }
        db.insert(insert).await?;
    }
    let hurried = server.client_with("test", |c| c.statement_timeout = Duration::from_millis(20)).await;
    // Every row is read and matched against a pattern that never matches.
    let slow = Query::table("big").filter(col("text").ilike("%a%b%c%d%e%f%g%h%zzz%")).select("n", count());
    assert_eq!(hurried.query(slow.clone()).await.unwrap_err().code, ErrorCode::Timeout);
    let err = hurried.update(Update::table("big").filter(col("text").ilike("%a%b%c%d%e%f%g%h%zzz%").not()).set("text", "x")).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Timeout);
    // The connection is still good, and nothing was changed.
    assert_eq!(hurried.get("big", [1]).await?.unwrap().get::<String>("text")?.len(), 27 * 40);
    assert_eq!(db.query_one(slow).await?.get::<i64>("n")?, 0);
    Ok(())
}

// R-X5: named locks.
#[tokio::test(flavor = "multi_thread")]
async fn a_named_lock_is_held_by_one_connection_at_a_time() -> DbResult<()> {
    let (server, db) = fresh().await;
    let other = server.client("test").await;

    let guard = db.lock("migrate").await?;
    assert!(other.try_lock("migrate").await?.is_none());
    assert!(other.try_lock("something-else").await?.is_some(), "locks are told apart by name");

    // A waiter gets it when it is unlocked.
    let waiter = tokio::spawn({
        let other = other.clone();
        async move { other.lock("migrate").await.map(|g| g.name().to_string()) }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiter.is_finished());
    guard.unlock().await?;
    assert_eq!(waiter.await.unwrap()?, "migrate");

    // Dropping a guard frees the lock too, as a crashed process would.
    let guard = db.lock("job").await?;
    drop(guard);
    let mut taken = None;
    for _ in 0..50 {
        taken = other.try_lock("job").await?;
        if taken.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(taken.is_some());

    // Each database has its own names.
    let elsewhere = server.client("second").await;
    assert!(elsewhere.try_lock("job").await?.is_some());
    Ok(())
}

// R-X7: the clock stands still inside a transaction and moves outside.
#[tokio::test(flavor = "multi_thread")]
async fn now_is_fixed_for_a_transaction() -> DbResult<()> {
    let (_server, db) = fresh().await;
    db.create_table(TableDef::new("stamps").column(ColumnDef::new("id", Type::Int)).column(ColumnDef::new("at", Type::Timestamp).default_now()).primary_key(["id"])).await?;
    let before = db.now().await?;
    let tx = db.begin().await?;
    let begun = tx.now().await?;
    tx.insert(Insert::into("stamps").set("id", 1)).await?;
    tokio::time::sleep(Duration::from_millis(30)).await;
    tx.insert(Insert::into("stamps").set("id", 2)).await?;
    tx.update(Update::table("stamps").key([1]).set("at", now())).await?;
    assert_eq!(tx.now().await?, begun);
    tx.commit().await?;
    let stamps: Vec<Timestamp> = db.query(Query::table("stamps").columns(["at"])).await?.column()?;
    assert_eq!(stamps, vec![begun, begun]);
    assert!(before <= begun);
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(db.now().await? > begun);
    // Outside a transaction each statement reads the clock anew.
    db.insert(Insert::into("stamps").set("id", 3)).await?;
    let third: Timestamp = db.get("stamps", [3]).await?.unwrap().get("at")?;
    assert!(third > begun);
    Ok(())
}

// R-X6: savepoints.
#[tokio::test(flavor = "multi_thread")]
async fn a_savepoint_undoes_part_of_a_transaction() -> DbResult<()> {
    let (_server, db) = fresh().await;
    accounts(&db, 3, 100).await?;
    let tx = db.begin().await?;
    tx.update(Update::table("accounts").key([1]).set("balance", 1)).await?;
    tx.savepoint("attempt").await?;
    tx.update(Update::table("accounts").key([2]).set("balance", 2)).await?;
    // An expected failure...
    assert!(tx.insert(Insert::into("accounts").set("id", 1).set("balance", 5)).await.is_err());
    assert_eq!(tx.query(Query::table("accounts")).await.unwrap_err().code, ErrorCode::TransactionAborted);
    // ...is undone, with what the savepoint covered, and the work goes on.
    tx.rollback_to("attempt").await?;
    assert_eq!(tx.get("accounts", [2]).await?.unwrap().get::<i64>("balance")?, 100);
    tx.savepoint("inner").await?;
    tx.update(Update::table("accounts").key([3]).set("balance", 3)).await?;
    tx.release("inner").await?;
    assert!(tx.rollback_to("inner").await.is_err(), "a released savepoint is gone");
    tx.rollback_to("attempt").await?;
    tx.update(Update::table("accounts").key([3]).set("balance", 33)).await?;
    tx.commit().await?;
    let balances: Vec<i64> = db.query(Query::table("accounts").columns(["balance"])).await?.column()?;
    assert_eq!(balances, vec![1, 100, 33]);
    Ok(())
}

// R-S8: schema changes are part of the transaction that makes them.
#[tokio::test(flavor = "multi_thread")]
async fn schema_changes_commit_and_roll_back_with_their_transaction() -> DbResult<()> {
    let (_server, db) = fresh().await;
    accounts(&db, 2, 100).await?;
    let tx = db.begin().await?;
    tx.create_table(TableDef::new("notes").column(ColumnDef::new("id", Type::Int)).primary_key(["id"])).await?;
    tx.insert(Insert::into("notes").set("id", 1)).await?;
    tx.add_column("accounts", ColumnDef::new("label", Type::Text).default("none")).await?;
    tx.create_index("accounts", IndexDef::new("accounts_balance").on(["balance"])).await?;
    tx.drop_table("ledger").await?;
    assert_eq!(tx.get("accounts", [1]).await?.unwrap().get::<String>("label")?, "none");
    // Nobody else sees any of it.
    assert_eq!(db.describe().await?.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["accounts", "ledger"]);
    tx.rollback().await?;
    let tables = db.describe().await?;
    assert_eq!(tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["accounts", "ledger"]);
    assert!(tables[0].column_index("label").is_none() && tables[0].indexes.is_empty());

    db.transaction(|tx| async move {
        tx.create_table(TableDef::new("notes").column(ColumnDef::new("id", Type::Int)).primary_key(["id"])).await?;
        tx.insert(Insert::into("notes").set("id", 1)).await?;
        tx.add_column("accounts", ColumnDef::new("label", Type::Text).default("none")).await?;
        Ok::<_, DbError>(())
    })
    .await?;
    assert_eq!(db.query(Query::table("notes")).await?.len(), 1);
    assert_eq!(db.get("accounts", [2]).await?.unwrap().get::<String>("label")?, "none");
    db.admin().check().await?;
    Ok(())
}

// R-X4: a commit is visible to every connection as soon as it returns.
#[tokio::test(flavor = "multi_thread")]
async fn a_commit_is_seen_by_every_connection_at_once() -> DbResult<()> {
    let (server, db) = fresh().await;
    db.create_table(TableDef::new("log").column(ColumnDef::new("id", Type::Int)).primary_key(["id"])).await?;
    let clients = [server.client("test").await, server.client("test").await, server.client("test").await];
    for id in 0..300i64 {
        clients[id as usize % 3].insert(Insert::into("log").set("id", id)).await?;
        // Whichever connection reads next, it sees the row just written.
        let seen = clients[(id as usize + 1) % 3].get("log", [id]).await?;
        assert!(seen.is_some(), "row {id}");
    }
    Ok(())
}
