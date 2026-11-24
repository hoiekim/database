//! Optimistic transactions: ones that run beside other writers, on a
//! copy of their own, and are checked when they commit (R-X1, R-X3).
//!
//! `acid.rs` runs transfers and counters through `Client::transaction`,
//! which uses these; here each path a commit can take is driven on its
//! own: the copy committed as it stands, the requests run again, and the
//! conflict.

mod common;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;

async fn counters(db: &Client) -> DbResult<()> {
    db.create_table(
        TableDef::new("counters")
            .column(ColumnDef::new("name", Type::Text))
            .column(ColumnDef::new("n", Type::Int))
            .primary_key(["name"]),
    )
    .await?;
    db.insert(
        Insert::into("counters")
            .columns(["name", "n"])
            .values([val("a"), val(0)])
            .values([val("b"), val(0)]),
    )
    .await?;
    Ok(())
}

async fn n(db: &impl Db, name: &str) -> DbResult<i64> {
    db.get("counters", [name.to_string()])
        .await?
        .expect("the counter is there")
        .get("n")
}

/// How the commits of optimistic transactions went so far: as they
/// stood, run again, in conflict.
async fn commits(db: &Client) -> (u64, u64, u64) {
    let stats = db.admin().stats().await.unwrap();
    let line = stats
        .lines()
        .find(|l| l.starts_with("optimistic transactions:"))
        .unwrap_or_else(|| panic!("no line about optimistic transactions in {stats:?}"));
    let numbers: Vec<u64> = line
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().unwrap())
        .collect();
    (numbers[0], numbers[1], numbers[2])
}

#[tokio::test(flavor = "multi_thread")]
async fn alone_it_commits_as_it_stands() -> DbResult<()> {
    let (_server, db) = fresh().await;
    counters(&db).await?;
    let tx = db.begin_optimistic().await?;
    tx.update(Update::table("counters").key(["a"]).set("n", 5))
        .await?;
    tx.insert(Insert::into("counters").set("name", "c").set("n", 1))
        .await?;
    assert_eq!(n(&tx, "a").await?, 5, "it sees its own writes");
    assert_eq!(n(&db, "a").await?, 0, "nobody else does");
    assert!(db.get("counters", ["c"]).await?.is_none());
    tx.commit().await?;
    assert_eq!((n(&db, "a").await?, n(&db, "c").await?), (5, 1));
    assert_eq!(commits(&db).await, (1, 0, 0));

    // Rolled back, by the caller or by being dropped, it leaves nothing.
    let tx = db.begin_optimistic().await?;
    tx.update(Update::table("counters").key(["a"]).set("n", 99))
        .await?;
    tx.rollback().await?;
    {
        let tx = db.begin_optimistic().await?;
        tx.delete(Delete::from("counters").key(["b"])).await?;
    }
    assert_eq!((n(&db, "a").await?, n(&db, "b").await?), (5, 0));
    db.admin().check().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn beside_a_commit_that_leaves_its_reads_alone_it_is_run_again() -> DbResult<()> {
    let (_server, db) = fresh().await;
    counters(&db).await?;
    let tx = db.begin_optimistic().await?;
    let a = n(&tx, "a").await?;
    tx.update(Update::table("counters").key(["a"]).set("n", a + 1))
        .await?;
    // Another writer commits meanwhile, and waits for nobody.
    db.update(Update::table("counters").key(["b"]).set("n", 40))
        .await?;
    assert_eq!(
        n(&tx, "a").await?,
        1,
        "the transaction reads on in the moment it began, and its own writes"
    );
    tx.commit().await?;
    assert_eq!((n(&db, "a").await?, n(&db, "b").await?), (1, 40));
    assert_eq!(commits(&db).await, (0, 1, 0));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_commit_that_changes_what_it_read_makes_it_conflict() -> DbResult<()> {
    let (_server, db) = fresh().await;
    counters(&db).await?;

    // Read, then written from what was read: the read no longer holds.
    let tx = db.begin_optimistic().await?;
    let a = n(&tx, "a").await?;
    tx.update(Update::table("counters").key(["a"]).set("n", a + 1))
        .await?;
    tx.update(Update::table("counters").key(["b"]).set("n", 7))
        .await?;
    db.update(Update::table("counters").key(["a"]).set("n", 10))
        .await?;
    assert_eq!(tx.commit().await.unwrap_err().code, ErrorCode::Conflict);
    assert_eq!(
        (n(&db, "a").await?, n(&db, "b").await?),
        (10, 0),
        "nothing of it took effect"
    );
    assert_eq!(commits(&db).await, (0, 0, 1));

    // A statement's answer counts as a read: here the new value.
    let tx = db.begin_optimistic().await?;
    let told: i64 = tx
        .update(
            Update::table("counters")
                .key(["a"])
                .set("n", col("n") + 1)
                .returning(["n"]),
        )
        .await?
        .one()?
        .get("n")?;
    assert_eq!(told, 11);
    db.update(Update::table("counters").key(["a"]).set("n", 20))
        .await?;
    assert_eq!(tx.commit().await.unwrap_err().code, ErrorCode::Conflict);

    // Told nothing of the row, it has nothing to be wrong about: its
    // change is made to the row as it now is.
    let tx = db.begin_optimistic().await?;
    assert_eq!(
        tx.update(Update::table("counters").key(["a"]).set("n", col("n") + 1))
            .await?
            .affected(),
        1
    );
    db.update(Update::table("counters").key(["a"]).set("n", 30))
        .await?;
    tx.commit().await?;
    assert_eq!(n(&db, "a").await?, 31);

    // One that only read never conflicts: it saw one moment.
    let tx = db.begin_optimistic().await?;
    assert_eq!(n(&tx, "a").await?, 31);
    db.update(Update::table("counters").key(["a"]).set("n", 0))
        .await?;
    assert_eq!(n(&tx, "a").await?, 31);
    tx.commit().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn what_it_was_told_is_what_is_stored() -> DbResult<()> {
    let (_server, db) = fresh().await;
    db.create_table(
        TableDef::new("things")
            .column(ColumnDef::new("id", Type::Int).auto_increment())
            .column(ColumnDef::new("token", Type::Uuid).default_random_uuid())
            .column(ColumnDef::new("at", Type::Timestamp).default_now())
            .column(ColumnDef::new("name", Type::Text))
            .primary_key(["id"]),
    )
    .await?;
    let insert = |name: &str| {
        Insert::into("things")
            .set("name", name)
            .returning(["id", "token", "at"])
    };

    // Two transactions beside each other, and a lone insert between: each
    // is given a number of its own, a random value and the time.
    let first = db.begin_optimistic().await?;
    let second = db.begin_optimistic().await?;
    let one = first.insert(insert("one")).await?.one()?;
    let two = second.insert(insert("two")).await?.one()?;
    let lone = db.insert(insert("lone")).await?.one()?;
    let mut ids = vec![
        one.get::<i64>("id")?,
        two.get::<i64>("id")?,
        lone.get::<i64>("id")?,
    ];
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 3, "three rows, three numbers");
    assert_eq!(
        first.now().await?,
        one.get::<Timestamp>("at")?,
        "now() is the moment the transaction began"
    );
    tokio::time::sleep(Duration::from_millis(5)).await;
    // Both are run again when they commit (the lone insert came between)
    // and must then store exactly what they answered.
    second.commit().await?;
    first.commit().await?;
    assert_eq!(commits(&db).await, (0, 2, 0));
    for told in [&one, &two, &lone] {
        let stored = db
            .get("things", [told.get::<i64>("id")?])
            .await?
            .expect("the row is there");
        assert_eq!(stored.get::<Uuid>("token")?, told.get::<Uuid>("token")?);
        assert_eq!(stored.get::<Timestamp>("at")?, told.get::<Timestamp>("at")?);
    }
    assert_eq!(db.query(Query::table("things")).await?.len(), 3);
    // The counter goes on above every number given.
    let next: i64 = db.insert(insert("next")).await?.one()?.get("id")?;
    assert!(next > *ids.last().unwrap());
    db.admin().check().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn failures_and_savepoints_are_run_again_too() -> DbResult<()> {
    let (_server, db) = fresh().await;
    counters(&db).await?;
    let insert = |name: &str| Insert::into("counters").set("name", name).set("n", 1);

    // A statement failed, the transaction went back to a savepoint and
    // on: run again, it fails and goes back the same way.
    let tx = db.begin_optimistic().await?;
    tx.insert(insert("c")).await?;
    tx.savepoint("try").await?;
    assert_eq!(
        tx.insert(insert("a")).await.unwrap_err().code,
        ErrorCode::UniqueViolation
    );
    assert_eq!(
        tx.insert(insert("d")).await.unwrap_err().code,
        ErrorCode::TransactionAborted
    );
    tx.rollback_to("try").await?;
    tx.insert(insert("d")).await?;
    db.insert(insert("z")).await?;
    tx.commit().await?;
    let names: Vec<String> = db
        .query(Query::table("counters").columns(["name"]))
        .await?
        .column()?;
    assert_eq!(names, ["a", "b", "c", "d", "z"]);
    assert_eq!(commits(&db).await, (0, 1, 0));

    // A failure that would not happen now is a conflict like any other
    // answer that changed.
    let tx = db.begin_optimistic().await?;
    tx.savepoint("try").await?;
    assert!(tx.insert(insert("z")).await.is_err());
    tx.rollback_to("try").await?;
    tx.insert(insert("e")).await?;
    db.delete(Delete::from("counters").key(["z"])).await?;
    assert_eq!(tx.commit().await.unwrap_err().code, ErrorCode::Conflict);
    assert!(db.get("counters", ["e"]).await?.is_none());

    // One left failed cannot commit, beside others or not.
    let tx = db.begin_optimistic().await?;
    assert!(tx.insert(insert("a")).await.is_err());
    assert_eq!(
        tx.commit().await.unwrap_err().code,
        ErrorCode::TransactionAborted
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn asked_for_much_it_becomes_the_only_writer() -> DbResult<()> {
    let (server, db) = fresh().await;
    counters(&db).await?;
    let impatient = server
        .client_with("test", |config| {
            config.lock_wait = Duration::from_millis(150)
        })
        .await;
    let many = |from: i64| {
        let mut insert = Insert::into("counters").columns(["name", "n"]);
        for i in from..from + 20 {
            insert = insert.values([val(format!("row{i}")), val(i)]);
        }
        insert
    };

    // A statement too large to be worth running twice: before it runs,
    // the transaction takes the write lock and keeps it.
    let tx = db.begin_optimistic().await?;
    let a = n(&tx, "a").await?;
    db.update(Update::table("counters").key(["b"]).set("n", 3))
        .await?;
    tx.insert(many(0)).await?;
    assert_eq!(
        n(&tx, "b").await?,
        3,
        "run again on the newest version, it reads from there"
    );
    assert_eq!(
        impatient
            .begin()
            .await
            .err()
            .expect("the lock is taken")
            .code,
        ErrorCode::LockTimeout
    );
    tx.update(Update::table("counters").key(["a"]).set("n", a + 1))
        .await?;
    tx.commit().await?;
    assert_eq!(
        (
            n(&db, "a").await?,
            n(&db, "b").await?,
            n(&db, "row19").await?
        ),
        (1, 3, 19)
    );
    impatient
        .update(Update::table("counters").key(["b"]).set("n", 4))
        .await?;

    // So does a change of schema.
    let tx = db.begin_optimistic().await?;
    tx.add_column("counters", ColumnDef::new("note", Type::Text).nullable())
        .await?;
    assert_eq!(
        impatient
            .begin()
            .await
            .err()
            .expect("the lock is taken")
            .code,
        ErrorCode::LockTimeout
    );
    tx.rollback().await?;
    assert!(db
        .describe_table("counters")
        .await?
        .unwrap()
        .column_index("note")
        .is_none());

    // If what it read has changed by then, the statement fails with the
    // conflict and the transaction is over: nothing more runs in it, and
    // nothing it did remains.
    let tx = db.begin_optimistic().await?;
    let a = n(&tx, "a").await?;
    tx.update(Update::table("counters").key(["b"]).set("n", a + 100))
        .await?;
    db.update(Update::table("counters").key(["a"]).set("n", 50))
        .await?;
    assert_eq!(
        tx.insert(many(200)).await.unwrap_err().code,
        ErrorCode::Conflict
    );
    assert!(tx.conflicted());
    assert_eq!(
        n(&tx, "a").await.unwrap_err().code,
        ErrorCode::TransactionAborted
    );
    assert_eq!(
        tx.insert(many(300)).await.unwrap_err().code,
        ErrorCode::TransactionAborted
    );
    assert_eq!(tx.commit().await.unwrap_err().code, ErrorCode::Conflict);
    assert_eq!((n(&db, "a").await?, n(&db, "b").await?), (50, 4));
    assert!(db.get("counters", ["row200"]).await?.is_none());
    // The write lock went back with it.
    impatient
        .update(Update::table("counters").key(["b"]).set("n", 5))
        .await?;
    db.admin().check().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn its_commit_waits_its_turn_behind_an_exclusive_transaction() -> DbResult<()> {
    let (server, db) = fresh().await;
    counters(&db).await?;
    let impatient = server
        .client_with("test", |config| {
            config.lock_wait = Duration::from_millis(150)
        })
        .await;

    let exclusive = db.begin().await?;
    exclusive
        .update(Update::table("counters").key(["a"]).set("n", 1))
        .await?;
    // Its statements run all the same.
    let tx = impatient.begin_optimistic().await?;
    tx.update(Update::table("counters").key(["b"]).set("n", 2))
        .await?;
    assert_eq!(n(&tx, "b").await?, 2);
    // Its commit does not, and gives up when its patience ends.
    assert_eq!(tx.commit().await.unwrap_err().code, ErrorCode::LockTimeout);
    exclusive.commit().await?;
    assert_eq!((n(&db, "a").await?, n(&db, "b").await?), (1, 0));

    // With patience, it commits as soon as the lock is free.
    let exclusive = db.begin().await?;
    exclusive
        .update(Update::table("counters").key(["a"]).set("n", 2))
        .await?;
    let tx = db.begin_optimistic().await?;
    tx.update(Update::table("counters").key(["b"]).set("n", 3))
        .await?;
    let committing = tokio::spawn(async move { tx.commit().await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!committing.is_finished());
    exclusive.commit().await?;
    committing.await.unwrap()?;
    assert_eq!((n(&db, "a").await?, n(&db, "b").await?), (2, 3));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_helper_runs_the_work_again_after_a_conflict() -> DbResult<()> {
    let (_server, db) = fresh().await;
    counters(&db).await?;
    let runs = Arc::new(AtomicUsize::new(0));

    // Another writer gets in the way of the first two runs. The third
    // holds the write lock from its start, and nothing can.
    let seen = {
        let (other, runs) = (db.clone(), runs.clone());
        db.transaction(move |tx| {
            let (other, runs) = (other.clone(), runs.clone());
            async move {
                let run = runs.fetch_add(1, Ordering::SeqCst);
                let a = n(&tx, "a").await?;
                if run < 2 {
                    other
                        .update(Update::table("counters").key(["a"]).set("n", col("n") + 10))
                        .await?;
                }
                tx.update(Update::table("counters").key(["a"]).set("n", a + 1))
                    .await?;
                Ok::<_, DbError>(a)
            }
        })
        .await?
    };
    assert_eq!(runs.load(Ordering::SeqCst), 3);
    assert_eq!(
        seen, 20,
        "the run that counted read what the others had left"
    );
    assert_eq!(n(&db, "a").await?, 21);

    // An error that is not a conflict is the work's own: returned at
    // once, the work not run again.
    runs.store(0, Ordering::SeqCst);
    let failed: Result<(), DbError> = {
        let runs = runs.clone();
        db.transaction(move |tx| {
            let runs = runs.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                tx.insert(Insert::into("counters").set("name", "a").set("n", 0))
                    .await?;
                Ok(())
            }
        })
        .await
    };
    assert_eq!(failed.unwrap_err().code, ErrorCode::UniqueViolation);
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    // `exclusive` runs its work once, whatever happens beside it.
    runs.store(0, Ordering::SeqCst);
    let counted = runs.clone();
    db.exclusive(|tx| async move {
        counted.fetch_add(1, Ordering::SeqCst);
        tx.update(Update::table("counters").key(["b"]).set("n", 1))
            .await?;
        Ok::<_, DbError>(())
    })
    .await?;
    assert_eq!((runs.load(Ordering::SeqCst), n(&db, "b").await?), (1, 1));
    Ok(())
}

// Many transactions that all read and write the same few rows, mixed
// with lone statements: every one of them counts, exactly once.
#[tokio::test(flavor = "multi_thread")]
async fn under_contention_every_transaction_counts_once() -> DbResult<()> {
    let (server, db) = fresh().await;
    counters(&db).await?;
    db.create_table(
        TableDef::new("log")
            .column(ColumnDef::new("id", Type::Int).auto_increment())
            .column(ColumnDef::new("who", Type::Int))
            .column(ColumnDef::new("saw", Type::Int))
            .primary_key(["id"]),
    )
    .await?;
    let mut tasks = Vec::new();
    for who in 0..8i64 {
        let db = server.client("test").await;
        tasks.push(tokio::spawn(async move {
            for round in 0..40 {
                let name = if (who + round) % 3 == 0 { "b" } else { "a" };
                if round % 4 == 3 {
                    db.update(Update::table("counters").key([name]).set("n", col("n") + 1))
                        .await
                        .unwrap();
                    db.insert(Insert::into("log").set("who", who).set("saw", -1))
                        .await
                        .unwrap();
                    continue;
                }
                db.transaction(|tx| async move {
                    let saw = n(&tx, name).await?;
                    tx.update(Update::table("counters").key([name]).set("n", saw + 1))
                        .await?;
                    let id: i64 = tx
                        .insert(
                            Insert::into("log")
                                .set("who", who)
                                .set("saw", saw)
                                .returning(["id"]),
                        )
                        .await?
                        .one()?
                        .get("id")?;
                    // The row is there under the number it was given.
                    assert_eq!(
                        tx.get("log", [id])
                            .await?
                            .expect("its own row")
                            .get::<i64>("who")?,
                        who
                    );
                    Ok::<_, DbError>(())
                })
                .await
                .unwrap();
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(n(&db, "a").await? + n(&db, "b").await?, 8 * 40);
    let log = db.query(Query::table("log")).await?;
    assert_eq!(log.len(), 8 * 40);
    // What each transaction read, it read alone: no two saw the same
    // count of the same counter, which they could only have by one
    // overwriting the other.
    let (as_stood, again, conflicts) = commits(&db).await;
    assert!(
        as_stood + again >= 1,
        "some transaction committed beside the others ({as_stood}, {again}, {conflicts})"
    );
    db.admin().check().await?;
    Ok(())
}
