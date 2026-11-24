//! What a request costs, and how to send fewer of them: statements the
//! server keeps and runs again with new values (R-X11), a transaction
//! sent whole as a batch (R-X12), a transaction that begins with its
//! first statement, a client that goes straight to the write lock where
//! conflicts are the rule, and a Unix domain socket for clients on the
//! server's own host (R-O6).

mod common;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use database_protocol::message::{decode_response, encode_request, BatchItem, Request, Response, TxnKind, OLDEST_PROTOCOL_VERSION, PROTOCOL_VERSION};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn accounts(db: &Client, n: i64, balance: i64) -> DbResult<()> {
    db.create_table(TableDef::new("accounts").column(ColumnDef::new("id", Type::Int)).column(ColumnDef::new("balance", Type::Int)).primary_key(["id"]).check("not_negative", col("balance").ge(0))).await?;
    db.create_table(TableDef::new("ledger").column(ColumnDef::new("id", Type::Int).auto_increment()).column(ColumnDef::new("from_id", Type::Int)).column(ColumnDef::new("to_id", Type::Int)).column(ColumnDef::new("amount", Type::Int)).primary_key(["id"])).await?;
    let mut insert = Insert::into("accounts").columns(["id", "balance"]);
    for id in 0..n {
        insert = insert.values([val(id), val(balance)]);
    }
    db.insert(insert).await?;
    Ok(())
}

async fn balance(db: &impl Db, id: i64) -> DbResult<i64> {
    db.get("accounts", [id]).await?.expect("the account is there").get("balance")
}

async fn total(db: &impl Db) -> DbResult<i64> {
    db.query_one(Query::table("accounts").select("total", sum(col("balance")))).await?.get("total")
}

/// A number from the server's counters: the first on the line that
/// starts with `label`, or the `nth` after it.
async fn stat(db: &Client, label: &str, nth: usize) -> u64 {
    let stats = db.admin().stats().await.unwrap();
    let line = stats.lines().find(|l| l.starts_with(label)).unwrap_or_else(|| panic!("no line {label:?} in {stats:?}"));
    line.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).nth(nth).unwrap().parse().unwrap()
}

/// Requests the server has received.
async fn requests(db: &Client) -> u64 {
    stat(db, "requests:", 0).await
}

/// Commits of optimistic transactions that ended in conflict.
async fn conflicts(db: &Client) -> u64 {
    stat(db, "optimistic transactions:", 2).await
}

/// A client that keeps to one connection, so that what one request
/// leaves on the connection the next one meets.
async fn one_connection(server: &TestServer) -> Client {
    server.client("test").await;
    server.client_with("test", |config| config.pool_size = 1).await
}

// ------------------------------------------------------------- prepared

#[tokio::test(flavor = "multi_thread")]
async fn a_prepared_statement_runs_again_with_other_values() -> DbResult<()> {
    // R-X11
    let (_server, db) = fresh().await;
    accounts(&db, 5, 100).await?;
    let by_id = Prepared::new(Query::table("accounts").key([param(0)]).columns(["balance"]));
    for id in 0..5 {
        assert_eq!(db.execute(&by_id, [id]).await?.one()?.get::<i64>("balance")?, 100);
    }
    assert!(db.execute(&by_id, [99]).await?.is_empty());

    // Every kind of statement, with values in keys, filters, assignments
    // and inserted rows.
    let add = Prepared::new(Insert::into("accounts").columns(["id", "balance"]).values([param(0), param(1)]).returning(["id", "balance"]));
    let row = db.execute(&add, [7, 70]).await?.one()?;
    assert_eq!((row.get::<i64>("id")?, row.get::<i64>("balance")?), (7, 70));
    let credit = Prepared::new(Update::table("accounts").key([param(0)]).filter(col("balance").lt(param(2))).set("balance", col("balance") + param(1)).returning(["balance"]));
    assert_eq!(db.execute(&credit, [7, 5, 1000]).await?.one()?.get::<i64>("balance")?, 75);
    assert_eq!(db.execute(&credit, [7, 5, 10]).await?.affected(), 0, "the filter read its own value");
    let remove = Prepared::new(Delete::from("accounts").key([param(0)]));
    assert_eq!(db.execute(&remove, [7]).await?.affected(), 1);
    assert_eq!(db.execute(&remove, [7]).await?.affected(), 0);

    // A value of the wrong kind is the error it would be if written into
    // the statement, and a constraint is named as usual.
    let e = db.execute(&by_id, ["seven"]).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::TypeMismatch, "{e}");
    let e = db.execute(&add, [1, 1]).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::UniqueViolation);
    let e = db.execute(&add, [8, -1]).await.unwrap_err();
    assert_eq!((e.code, e.constraint.as_deref()), (ErrorCode::CheckViolation, Some("not_negative")));

    // Too few values is said so; a value nothing reads is ignored.
    let e = db.execute(&credit, [1, 1]).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidRequest, "{e}");
    assert_eq!(db.execute(&by_id, [1, 2, 3]).await?.len(), 1);
    // A statement sent as it is has no values to read.
    let e = db.query(Query::table("accounts").key([param(0)])).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidRequest, "{e}");
    // Nor has a definition: a default, a check or an index is the same
    // for every statement.
    let e = db.create_table(TableDef::new("t").column(ColumnDef::new("id", Type::Int)).primary_key(["id"]).check("known", col("id").lt(param(0)))).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidRequest, "{e}");
    db.admin().check().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prepared_statement_is_sent_once_to_each_connection() -> DbResult<()> {
    // R-X11
    let server = TestServer::start().await;
    let db = one_connection(&server).await;
    accounts(&db, 3, 100).await?;
    let by_id = Prepared::new(Query::table("accounts").key([param(0)]).columns(["balance"]));
    assert_eq!(db.execute(&by_id, [1]).await?.len(), 1);
    // From here on its number is enough: asked for by number alone, the
    // connection finds it. (The client does this; here it is done by hand
    // with a number the connection was never sent, and one it was.)
    let unknown = db.call(Request::Prepared { id: u64::MAX, statement: None, params: vec![Value::Int(1)] }).await.unwrap_err();
    assert_eq!(unknown.code, ErrorCode::NotPrepared, "{unknown}");
    let again = db.execute(&by_id, [2]).await?;
    assert_eq!(again.one()?.get::<i64>("balance")?, 100);

    // A new connection is sent the statement again, without being asked.
    db.close();
    assert_eq!(db.execute(&by_id, [0]).await?.len(), 1);
    // So is another client's.
    let other = server.client("test").await;
    assert_eq!(other.execute(&by_id, [0]).await?.len(), 1);

    // A connection keeps 256. Past that the ones used longest ago make
    // way, and are sent again when next used: nothing the caller sees.
    let many: Vec<Prepared> = (0..300).map(|n| Prepared::new(Query::table("accounts").key([param(0)]).select("n", val(n)))).collect();
    for (n, prepared) in many.iter().enumerate() {
        assert_eq!(db.execute(prepared, [1]).await?.one()?.get::<i64>("n")?, n as i64);
    }
    assert_eq!(db.execute(&by_id, [1]).await?.len(), 1);
    for (n, prepared) in many.iter().enumerate() {
        assert_eq!(db.execute(prepared, [1]).await?.one()?.get::<i64>("n")?, n as i64);
    }

    // What cannot be kept is refused, and nothing is kept in its place.
    let e = db.call(Request::Prepared { id: 1 << 40, statement: Some(Arc::new(Request::Describe)), params: Vec::new() }).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidRequest, "{e}");
    let huge = Prepared::new(Query::table("accounts").filter(col("id").in_list((0..200_000).collect::<Vec<i64>>())));
    let e = db.execute(&huge, Vec::<i64>::new()).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidRequest, "{e}");
    assert_eq!(db.execute(&by_id, [1]).await?.len(), 1, "the connection is as usable as before");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn prepared_statements_run_in_every_kind_of_transaction() -> DbResult<()> {
    // R-X11
    let (_server, db) = fresh().await;
    accounts(&db, 4, 100).await?;
    let read = Prepared::new(Query::table("accounts").key([param(0)]).columns(["balance"]));
    let add = Prepared::new(Update::table("accounts").key([param(0)]).set("balance", col("balance") + param(1)));
    let log = Prepared::new(Insert::into("ledger").columns(["from_id", "to_id", "amount"]).values([param(0), param(1), param(2)]).returning(["id"]));

    // The first statement of a transaction, and later ones.
    for exclusive in [false, true] {
        let work = |tx: Transaction| {
            let (read, add, log) = (read.clone(), add.clone(), log.clone());
            async move {
                let before: i64 = tx.execute(&read, [0]).await?.one()?.get("balance")?;
                tx.execute(&add, [0, -10]).await?;
                tx.execute(&add, [1, 10]).await?;
                tx.execute(&log, [0, 1, 10]).await?;
                let after: i64 = tx.execute(&read, [0]).await?.one()?.get("balance")?;
                assert_eq!(after, before - 10, "it reads its own writes");
                Ok::<_, DbError>(())
            }
        };
        if exclusive {
            db.exclusive(work).await?;
        } else {
            db.transaction(work).await?;
        }
    }
    assert_eq!((balance(&db, 0).await?, balance(&db, 1).await?), (80, 120));
    db.read(|tx| {
        let read = read.clone();
        async move {
            assert_eq!(tx.execute(&read, [1]).await?.one()?.get::<i64>("balance")?, 120);
            Ok::<_, DbError>(())
        }
    })
    .await?;

    // A transaction that ran beside another writer is run again by the
    // server with the values each statement had, and what it drew: the
    // ledger's number it was told is the number stored.
    let tx = db.begin_optimistic().await?;
    tx.execute(&add, [2, -1]).await?;
    let told: i64 = tx.execute(&log, [2, 3, 1]).await?.one()?.get("id")?;
    db.execute(&add, [3, 1000]).await?;
    tx.execute(&add, [3, 1]).await?;
    tx.commit().await?;
    assert_eq!((balance(&db, 2).await?, balance(&db, 3).await?), (99, 1101));
    let stored = db.query_one(Query::table("ledger").filter(col("from_id").eq(2))).await?;
    assert_eq!(stored.get::<i64>("id")?, told);

    // And one that read what the other changed conflicts, as ever.
    let tx = db.begin_optimistic().await?;
    let seen: i64 = tx.execute(&read, [3]).await?.one()?.get("balance")?;
    db.execute(&add, [3, 1]).await?;
    tx.execute(&add, [2, seen]).await?;
    assert_eq!(tx.commit().await.unwrap_err().code, ErrorCode::Conflict);
    assert_eq!(balance(&db, 2).await?, 99);
    db.admin().check().await?;
    Ok(())
}

// ---------------------------------------------------------------- batch

fn transfer(from: i64, to: i64, amount: i64) -> Batch {
    Batch::new()
        .add(Update::table("accounts").key([from]).filter(col("balance").ge(amount)).set("balance", col("balance") - amount))
        .expect(1)
        .add(Update::table("accounts").key([to]).set("balance", col("balance") + amount))
        .expect(1)
        .add(Insert::into("ledger").set("from_id", from).set("to_id", to).set("amount", amount).returning(["id"]))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_is_one_transaction_in_one_request() -> DbResult<()> {
    // R-X12
    let server = TestServer::start().await;
    let db = one_connection(&server).await;
    accounts(&db, 3, 100).await?;
    let before = requests(&db).await;
    let answers = db.batch(transfer(0, 1, 30)).await?;
    // One request for the three statements (and one to ask how many).
    assert_eq!(requests(&db).await, before + 2);
    assert_eq!(answers.len(), 3);
    assert_eq!((answers[0].affected(), answers[1].affected(), answers[2].affected()), (1, 1, 1));
    assert_eq!(answers[2].first().unwrap().get::<i64>("id")?, 1);
    assert_eq!((balance(&db, 0).await?, balance(&db, 1).await?), (70, 130));

    // A statement that does not affect the rows it must ends the batch,
    // and what ran before it is undone.
    let e = db.batch(transfer(0, 1, 500)).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Unmet, "{e}");
    let e = db.batch(transfer(1, 99, 10)).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Unmet, "{e}");
    assert!(e.message.contains("statement 2"), "{e}");
    assert_eq!((balance(&db, 0).await?, balance(&db, 1).await?), (70, 130));

    // So does one that fails, with the error it would give alone.
    let e = db
        .batch(Batch::new().add(Update::table("accounts").key([0]).set("balance", 0)).add(Insert::into("accounts").set("id", 1).set("balance", 5)).add(Update::table("accounts").key([2]).set("balance", 0)))
        .await
        .unwrap_err();
    assert_eq!((e.code, e.constraint.as_deref()), (ErrorCode::UniqueViolation, Some(PRIMARY)));
    let e = db.batch(Batch::new().add(Update::table("accounts").key([2]).set("balance", 1)).add(Update::table("accounts").key([0]).set("balance", col("balance") - 1000))).await.unwrap_err();
    assert_eq!((e.code, e.constraint.as_deref()), (ErrorCode::CheckViolation, Some("not_negative")));
    assert_eq!((balance(&db, 0).await?, balance(&db, 2).await?), (70, 100));
    assert_eq!(db.query(Query::table("ledger")).await?.len(), 1);

    // Statements see what those before them did; results come in order,
    // a large one whole.
    let answers = db
        .batch(
            Batch::new()
                .add(Insert::into("accounts").set("id", 10).set("balance", 1))
                .add(Query::table("accounts").key([10]).columns(["balance"]))
                .expect(1)
                .add(Delete::from("accounts").key([10]).returning(["id"]))
                .add(Query::table("accounts").key([10])),
        )
        .await?;
    assert_eq!(answers[1].first().unwrap().get::<i64>("balance")?, 1);
    assert_eq!((answers[2].len(), answers[3].len()), (1, 0));

    // Prepared statements in a batch: sent the first time, named after.
    let credit = Prepared::new(Update::table("accounts").key([param(0)]).set("balance", col("balance") + param(1)));
    for _ in 0..3 {
        let answers = db.batch(Batch::new().add_prepared(&credit, [0, 1]).expect(1).add_prepared(&credit, [1, -1]).expect(1)).await?;
        assert_eq!(answers.len(), 2);
    }
    assert_eq!((balance(&db, 0).await?, balance(&db, 1).await?), (73, 127));

    // No statements, no request.
    assert!(db.batch(Batch::new()).await?.is_empty());
    assert_eq!(total(&db).await?, 300);
    db.admin().check().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_of_queries_reads_one_moment_and_large_results_come_whole() -> DbResult<()> {
    // R-X12
    let (_server, db) = fresh().await;
    db.create_table(TableDef::new("notes").column(ColumnDef::new("id", Type::Int)).column(ColumnDef::new("body", Type::Text)).primary_key(["id"])).await?;
    // Results of several megabytes each, which travel as several frames.
    for chunk in 0..4 {
        let mut insert = Insert::into("notes").columns(["id", "body"]);
        for id in chunk * 25..(chunk + 1) * 25 {
            insert = insert.values([val(id), val("x".repeat(100_000))]);
        }
        db.insert(insert).await?;
    }
    let answers = db.batch(Batch::new().add(Query::table("notes")).add(Query::table("notes").select("n", count())).expect(1).add(Query::table("notes").reverse().limit(60))).await?;
    assert_eq!((answers[0].len(), answers[2].len()), (100, 60));
    assert_eq!(answers[1].first().unwrap().get::<i64>("n")?, 100);
    assert_eq!(answers[2].first().unwrap().get::<i64>("id")?, 99);
    assert!(answers[0].iter().all(|row| row.get::<String>("body").unwrap().len() == 100_000));
    // The same with a write among them.
    let answers = db.batch(Batch::new().add(Query::table("notes")).add(Delete::from("notes").key([0])).add(Query::table("notes"))).await?;
    assert_eq!((answers[0].len(), answers[1].affected(), answers[2].len()), (100, 1, 99));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn batches_from_many_connections_never_lose_or_make_money() -> DbResult<()> {
    // R-X12, R-X2
    let (server, db) = fresh().await;
    accounts(&db, 8, 50).await?;
    let debit = Prepared::new(Update::table("accounts").key([param(0)]).filter(col("balance").ge(param(1))).set("balance", col("balance") - param(1)));
    let credit = Prepared::new(Update::table("accounts").key([param(0)]).set("balance", col("balance") + param(1)));
    let mut tasks = Vec::new();
    for worker in 0..8i64 {
        let db = server.client("test").await;
        let (debit, credit) = (debit.clone(), credit.clone());
        tasks.push(tokio::spawn(async move {
            let (mut done, mut refused) = (0, 0);
            for n in 0..150i64 {
                let (from, to, amount) = ((worker + n) % 8, (worker * 3 + n * 5 + 1) % 8, 1 + (n % 40));
                if from == to {
                    continue;
                }
                let batch = Batch::new()
                    .add_prepared(&debit, [from, amount])
                    .expect(1)
                    .add_prepared(&credit, [to, amount])
                    .expect(1)
                    .add(Insert::into("ledger").set("from_id", from).set("to_id", to).set("amount", amount));
                match db.batch(batch).await {
                    Ok(_) => done += 1,
                    Err(e) if e.code == ErrorCode::Unmet => refused += 1,
                    Err(e) => panic!("{e}"),
                }
            }
            (done, refused)
        }));
    }
    let (mut done, mut refused) = (0, 0);
    for task in tasks {
        let (d, r) = task.await.unwrap();
        done += d;
        refused += r;
    }
    assert!(done > 0 && refused > 0, "{done} done, {refused} refused: the test means to see both");
    assert_eq!(total(&db).await?, 8 * 50);
    // A transfer is in the ledger exactly when it happened.
    assert_eq!(db.query_one(Query::table("ledger").select("n", count())).await?.get::<i64>("n")?, done);
    let lowest: i64 = db.query_one(Query::table("accounts").select("low", min(col("balance")))).await?.get("low")?;
    assert!(lowest >= 0);
    db.admin().check().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_holds_statements_and_stands_alone() -> DbResult<()> {
    // R-X12
    let server = TestServer::start().await;
    let db = one_connection(&server).await;
    accounts(&db, 2, 100).await?;
    let item = |statement: Request| BatchItem { statement, expect: None };
    let update = || Request::Update(Update::table("accounts").key([0]).set("balance", 1));
    // Only statements: nothing that begins or ends a transaction, changes
    // the schema, or is about the connection.
    for odd in [
        Request::Commit,
        Request::Begin { kind: TxnKind::Read, lock_wait_ms: 0 },
        Request::Ping,
        Request::Savepoint("s".into()),
        Request::Batch(vec![item(update())]),
        Request::BeginWith { kind: TxnKind::Read, lock_wait_ms: 0, statement: Box::new(Request::Describe) },
    ] {
        let e = db.call(Request::Batch(vec![item(update()), item(odd.clone())])).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidRequest, "{odd:?}: {e}");
    }
    let e = db.call(Request::Batch(Vec::new())).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidRequest, "{e}");
    // Not inside a transaction: it is one.
    let tx = db.begin().await?;
    let e = tx.call(Request::Batch(vec![item(update())])).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidRequest, "{e}");
    tx.rollback().await?;
    // A prepared statement the connection does not keep is said so before
    // anything runs.
    let e = db.call(Request::Batch(vec![item(update()), item(Request::Prepared { id: u64::MAX, statement: None, params: Vec::new() })])).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::NotPrepared, "{e}");
    assert_eq!(balance(&db, 0).await?, 100, "none of the refused batches ran");
    Ok(())
}

// ------------------------------------------------- begin with a statement

#[tokio::test(flavor = "multi_thread")]
async fn a_transaction_begins_with_its_first_statement() -> DbResult<()> {
    let server = TestServer::start().await;
    let db = one_connection(&server).await;
    accounts(&db, 3, 100).await?;
    // Two statements are three requests: the first brings the begin.
    let before = requests(&db).await;
    db.transaction(|tx| async move {
        tx.update(Update::table("accounts").key([0]).set("balance", col("balance") - 1)).await?;
        tx.update(Update::table("accounts").key([1]).set("balance", col("balance") + 1)).await?;
        Ok::<_, DbError>(())
    })
    .await?;
    assert_eq!(requests(&db).await, before + 4, "begin with the first statement, the second, the commit; and this count");
    assert_eq!((balance(&db, 0).await?, balance(&db, 1).await?), (99, 101));

    // One that runs no statement sends nothing, and still ends well.
    let before = requests(&db).await;
    db.transaction(|_tx| async move { Ok::<_, DbError>(()) }).await?;
    db.read(|_tx| async move { Ok::<_, DbError>(()) }).await?;
    let _: DbResult<()> = db.exclusive(|_tx| async move { Err(DbError::invalid("changed my mind")) }).await;
    assert_eq!(requests(&db).await, before + 1);

    // What it sees is the database as of its first statement.
    let other = server.client("test").await;
    db.read(|tx| {
        let other = other.clone();
        async move {
            other.update(Update::table("accounts").key([2]).set("balance", 7)).await?;
            assert_eq!(balance(&tx, 2).await?, 7, "written before the transaction's first statement");
            other.update(Update::table("accounts").key([2]).set("balance", 8)).await?;
            assert_eq!(balance(&tx, 2).await?, 7, "written after it");
            Ok::<_, DbError>(())
        }
    })
    .await?;

    // Every kind, and every kind of first statement.
    db.exclusive(|tx| async move {
        tx.savepoint("start").await?;
        tx.add_column("accounts", ColumnDef::new("note", Type::Text).default("")).await?;
        tx.rollback_to("start").await?;
        assert_eq!(tx.describe_table("accounts").await?.unwrap().columns.len(), 2);
        Ok::<_, DbError>(())
    })
    .await?;
    db.transaction(|tx| async move {
        let now = tx.now().await?;
        assert_eq!(tx.now().await?, now);
        Ok::<_, DbError>(())
    })
    .await?;
    db.admin().check().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_first_statement_that_fails_leaves_a_transaction_to_end_not_a_statement_run_alone() -> DbResult<()> {
    let server = TestServer::start().await;
    let db = one_connection(&server).await;
    accounts(&db, 2, 100).await?;

    // The statement fails: the transaction is open and can only be
    // rolled back, as after any failed statement.
    let result: DbResult<()> = db
        .transaction(|tx| async move {
            let e = tx.insert(Insert::into("accounts").set("id", 0).set("balance", 1)).await.unwrap_err();
            assert_eq!(e.code, ErrorCode::UniqueViolation);
            // Swallowed, and the work carries on: nothing of it may stay.
            let e = tx.update(Update::table("accounts").key([1]).set("balance", 0)).await.unwrap_err();
            assert_eq!(e.code, ErrorCode::TransactionAborted, "{e}");
            Ok(())
        })
        .await;
    assert_eq!(result.unwrap_err().code, ErrorCode::TransactionAborted);
    assert_eq!(balance(&db, 1).await?, 100);

    // The transaction cannot begin: another holds the write lock for
    // longer than this one waits. What follows the first statement was
    // meant for the transaction too, and must not run outside one.
    let holder = server.client("test").await.begin().await?;
    holder.update(Update::table("accounts").key([0]).set("balance", 50)).await?;
    let impatient = server.client_with("test", |config| {
        config.pool_size = 1;
        config.lock_wait = Duration::from_millis(50);
    })
    .await;
    let result: DbResult<()> = impatient
        .exclusive(|tx| async move {
            let e = tx.update(Update::table("accounts").key([1]).set("balance", 1)).await.unwrap_err();
            assert_eq!(e.code, ErrorCode::LockTimeout, "{e}");
            let e = tx.update(Update::table("accounts").key([1]).set("balance", 2)).await.unwrap_err();
            assert_eq!(e.code, ErrorCode::TransactionAborted, "{e}");
            let e = tx.query(Query::table("accounts")).await.unwrap_err();
            assert_eq!(e.code, ErrorCode::TransactionAborted, "{e}");
            Ok(())
        })
        .await;
    assert_eq!(result.unwrap_err().code, ErrorCode::LockTimeout, "the commit says why the transaction is gone");
    holder.rollback().await?;
    assert_eq!(balance(&db, 1).await?, 100, "neither update ran");
    // The connection it was on is as good as new.
    impatient.update(Update::table("accounts").key([1]).set("balance", 3)).await?;
    assert_eq!(balance(&impatient, 1).await?, 3);

    // The first statement is refused before the transaction could begin:
    // here one too large for a connection to keep. The client has no way
    // to tell that from a statement that failed, and goes on sending the
    // transaction's statements: they must find a transaction that can
    // only be ended, not run and commit one by one.
    let too_large = Prepared::new(Query::table("accounts").filter(col("id").ne(val("x".repeat(70_000)))));
    for exclusive in [true, false] {
        let work = |tx: Transaction| {
            let too_large = too_large.clone();
            async move {
                let e = tx.execute(&too_large, Vec::<i64>::new()).await.unwrap_err();
                assert_eq!(e.code, ErrorCode::InvalidRequest, "{e}");
                let e = tx.update(Update::table("accounts").key([1]).set("balance", 0)).await.unwrap_err();
                assert_eq!(e.code, ErrorCode::TransactionAborted, "{e}");
                Ok::<_, DbError>(())
            }
        };
        let result = if exclusive { db.exclusive(work).await } else { db.transaction(work).await };
        assert_eq!(result.unwrap_err().code, ErrorCode::InvalidRequest, "the commit says why the transaction is gone");
        assert_eq!(balance(&db, 1).await?, 3, "the update did not run alone");
    }

    // The first statement is refused by the client itself, before it is
    // sent: it is larger than a frame may be. The server has heard of no
    // transaction, so the client must not take one for begun: the next
    // statement begins it, and is undone with it.
    for commit in [false, true] {
        let result: DbResult<()> = db
            .exclusive(|tx| async move {
                let e = tx.query(Query::table("accounts").filter(col("id").ne(val("x".repeat(257 * 1024 * 1024))))).await.unwrap_err();
                assert_eq!(e.code, ErrorCode::InvalidRequest, "{e}");
                tx.update(Update::table("accounts").key([1]).set("balance", col("balance") + 10)).await?;
                assert_eq!(balance(&tx, 1).await?, 13, "inside the transaction");
                if commit {
                    Ok(())
                } else {
                    Err(DbError::invalid("on second thoughts"))
                }
            })
            .await;
        assert_eq!(result.is_ok(), commit);
        assert_eq!(balance(&db, 1).await?, if commit { 13 } else { 3 }, "the update stands or falls with its transaction");
    }
    db.update(Update::table("accounts").key([1]).set("balance", 3)).await?;

    // By hand. What is not a statement cannot begin a transaction, and
    // leaves a dead one all the same, until it is ended.
    let e = db.call(Request::BeginWith { kind: TxnKind::Optimistic, lock_wait_ms: 0, statement: Box::new(Request::Commit) }).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidRequest, "{e}");
    let e = db.query(Query::table("accounts")).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::TransactionAborted, "{e}");
    assert!(matches!(db.call(Request::Rollback).await?, Response::Ok));
    assert_eq!(db.query(Query::table("accounts")).await?.len(), 2);
    // Only a statement the connection does not keep is refused with
    // nothing begun: the client sends the request again with it.
    let e = db.call(Request::BeginWith { kind: TxnKind::Optimistic, lock_wait_ms: 0, statement: Box::new(Request::Prepared { id: u64::MAX, statement: None, params: Vec::new() }) }).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::NotPrepared, "{e}");
    assert_eq!(db.query(Query::table("accounts")).await?.len(), 2, "no transaction is open, dead or alive");
    assert_eq!(db.call(Request::Commit).await.unwrap_err().code, ErrorCode::TransactionAborted, "nor is there one to commit");
    // And inside a transaction there is none to begin.
    let tx = db.begin().await?;
    let e = tx.call(Request::BeginWith { kind: TxnKind::Read, lock_wait_ms: 0, statement: Box::new(Request::Describe) }).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidRequest, "{e}");
    assert_eq!(balance(&tx, 0).await?, 100, "and the one that was open goes on");
    tx.commit().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_statement_the_server_let_go_is_sent_again_as_a_transaction_begins() -> DbResult<()> {
    // R-X11
    let server = TestServer::start().await;
    let db = one_connection(&server).await;
    accounts(&db, 2, 100).await?;
    let credit = Prepared::new(Update::table("accounts").key([param(0)]).set("balance", col("balance") + param(1)));
    db.execute(&credit, [0, 1]).await?;
    // Enough other statements that the connection lets that one go.
    for n in 0..300 {
        db.execute(&Prepared::new(Query::table("accounts").key([param(0)]).select("n", val(n))), [0]).await?;
    }
    db.transaction(|tx| {
        let credit = credit.clone();
        async move {
            tx.execute(&credit, [0, 1]).await?;
            tx.execute(&credit, [1, 1]).await?;
            Ok::<_, DbError>(())
        }
    })
    .await?;
    assert_eq!((balance(&db, 0).await?, balance(&db, 1).await?), (102, 101));
    // Nothing was left open by the request that was refused and sent again.
    db.begin().await?.commit().await?;
    Ok(())
}

// ------------------------------------------------- straight to the lock

/// Runs a transaction that reads account 0 and adds one to it. On the
/// runs that `meddle` picks (counted from 1), another client changes the
/// account between the read and the write, so that the run conflicts.
/// Returns how many times the work was called.
async fn bump(db: &Client, other: &Client, meddle: fn(usize) -> bool) -> DbResult<usize> {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    db.transaction(|tx| {
        let (other, calls) = (other.clone(), counted.clone());
        async move {
            let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
            let seen = balance(&tx, 0).await?;
            if meddle(call) {
                other.update(Update::table("accounts").key([0]).set("balance", col("balance") + 1000)).await?;
            }
            tx.update(Update::table("accounts").key([0]).set("balance", seen + 1)).await?;
            Ok::<_, DbError>(())
        }
    })
    .await?;
    Ok(calls.load(Ordering::SeqCst))
}

#[tokio::test(flavor = "multi_thread")]
async fn how_often_a_transaction_runs_beside_the_others_is_the_clients_to_say() -> DbResult<()> {
    let server = TestServer::start().await;
    let other = server.client("test").await;
    accounts(&other, 1, 0).await?;

    // Two runs beside the others unless told otherwise: the second, left
    // alone, commits.
    let db = server.client("test").await;
    assert_eq!(bump(&db, &other, |call| call == 1).await?, 2);
    assert_eq!(conflicts(&other).await, 1);
    assert_eq!(balance(&other, 0).await?, 1001);

    // One: after its first conflict it is the only writer, and nobody
    // can be in its way. (Had it run beside the others again, the meddler
    // would have made it conflict again.)
    let once = server.client_with("test", |config| config.optimistic_runs = 1).await;
    assert_eq!(bump(&once, &other, |call| call == 1).await?, 2);
    assert_eq!(conflicts(&other).await, 2);
    assert_eq!(balance(&other, 0).await?, 2002);

    // None: it takes the lock from the start, and conflicts with nothing.
    let never = server.client_with("test", |config| config.optimistic_runs = 0).await;
    assert_eq!(bump(&never, &other, |_| false).await?, 1);
    assert_eq!(conflicts(&other).await, 2);
    assert_eq!(balance(&other, 0).await?, 2003);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn where_conflicts_are_the_rule_a_transaction_goes_straight_to_the_lock() -> DbResult<()> {
    // A statement on its own waits this long for the write lock: short,
    // for the one below that is meant to give up.
    let server = TestServer::start_with(TempDir::new(), |config| config.lock_wait = Duration::from_millis(100)).await;
    let other = server.client("test").await;
    accounts(&other, 1, 0).await?;
    let db = server.client("test").await;

    // While its first runs mostly commit, a transaction that conflicts is
    // run beside the others once more: here that run conflicts too, and
    // the third is alone.
    assert_eq!(bump(&db, &other, |call| call <= 2).await?, 3);
    assert_eq!(conflicts(&other).await, 2);
    // After several whose first run conflicted, the client stops trying
    // twice: one conflict, then the lock.
    for _ in 0..3 {
        bump(&db, &other, |call| call == 1).await?;
    }
    let before = conflicts(&other).await;
    assert_eq!(bump(&db, &other, |call| call == 1).await?, 2);
    assert_eq!(conflicts(&other).await, before + 1);
    // Proof that the second run held the lock: the meddler, asked to
    // write during it, could not have been let in. It is asked on every
    // run here, and gives up waiting.
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    db.transaction(|tx| {
        let (other, calls) = (other.clone(), counted.clone());
        async move {
            let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
            let seen = balance(&tx, 0).await?;
            let meddled = other.update(Update::table("accounts").key([0]).set("balance", col("balance") + 1000)).await;
            match call {
                1 => assert!(meddled.is_ok(), "beside the others, the meddler writes"),
                _ => assert_eq!(meddled.unwrap_err().code, ErrorCode::LockTimeout, "the second run is the only writer"),
            }
            tx.update(Update::table("accounts").key([0]).set("balance", seen + 1)).await?;
            Ok::<_, DbError>(())
        }
    })
    .await?;
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    // When first runs commit again, so does the second chance come back.
    for _ in 0..20 {
        assert_eq!(bump(&db, &other, |_| false).await?, 1);
    }
    let before = conflicts(&other).await;
    assert_eq!(bump(&db, &other, |call| call <= 2).await?, 3);
    assert_eq!(conflicts(&other).await, before + 2);
    Ok(())
}

// ----------------------------------------------------------- unix socket

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn clients_on_the_same_host_connect_through_a_unix_socket() -> DbResult<()> {
    // R-O6
    let dir = TempDir::new();
    let path = dir.path().join("database.sock");
    let socket = path.clone();
    let server = TestServer::start_with(dir, |config| config.socket = Some(socket)).await;
    server.client("test").await;
    let url = format!("database://test:{PASSWORD}@localhost?socket={}", path.display());
    let db = Client::connect_url(&url).await?;
    assert_eq!(db.config().socket.as_deref(), Some(path.as_path()));
    accounts(&db, 3, 100).await?;
    // Everything a connection does, it does here: statements, results of
    // many frames, transactions, prepared statements, batches, locks.
    db.batch(transfer(0, 1, 10)).await?;
    db.transaction(|tx| async move {
        let seen = balance(&tx, 2).await?;
        tx.update(Update::table("accounts").key([2]).set("balance", seen + 1)).await?;
        Ok::<_, DbError>(())
    })
    .await?;
    let by_id = Prepared::new(Query::table("accounts").key([param(0)]));
    assert_eq!(db.execute(&by_id, [2]).await?.one()?.get::<i64>("balance")?, 101);
    db.create_table(TableDef::new("notes").column(ColumnDef::new("id", Type::Int)).column(ColumnDef::new("body", Type::Text)).primary_key(["id"])).await?;
    for id in 0..12 {
        db.insert(Insert::into("notes").set("id", id).set("body", "y".repeat(1_000_000))).await?;
    }
    assert_eq!(db.query(Query::table("notes")).await?.len(), 12);
    let guard = db.lock("job").await?;
    assert!(db.try_lock("job").await?.is_none());
    guard.unlock().await?;
    // The password is asked for here as on TCP.
    let wrong = Client::connect_url(&format!("database://test:wrong@localhost?socket={}", path.display())).await;
    assert_eq!(wrong.err().map(|e| e.code), Some(ErrorCode::AccessDenied));
    // TCP still answers, and both see the same database.
    let tcp = Client::connect_url(&server.url("test")).await?;
    assert_eq!(balance(&tcp, 0).await?, 90);

    // A second server is not given a socket that one is listening on.
    let elsewhere = TempDir::new();
    let mut config = database_server::ServerConfig::new(elsewhere.path(), ADMIN_PASSWORD);
    config.socket = Some(path.clone());
    let second = database_server::Server::start(config).await;
    assert_eq!(second.err().map(|e| e.kind()), Some(std::io::ErrorKind::AddrInUse));
    assert_eq!(balance(&db, 0).await?, 90, "and the first goes on listening");

    // Stopped, the server takes its socket with it; started again, it is
    // there again, also over a file that a server that died left behind.
    drop((db, tcp));
    let dir = server.restart_dir().await;
    assert!(!path.exists());
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    assert!(path.exists(), "a socket file with nobody listening");
    let socket = path.clone();
    let server = TestServer::start_with(dir, |config| config.socket = Some(socket)).await;
    let db = Client::connect_url(&url).await?;
    assert_eq!(balance(&db, 0).await?, 90);
    // Nothing but a socket is ever removed to make room.
    drop(db);
    let dir = server.restart_dir().await;
    std::fs::write(&path, b"not a socket").unwrap();
    let mut config = database_server::ServerConfig::new(dir.path(), ADMIN_PASSWORD);
    config.socket = Some(path.clone());
    let refused = database_server::Server::start(config).await;
    assert!(refused.err().is_some_and(|e| e.to_string().contains("not a socket")));
    assert_eq!(std::fs::read(&path).unwrap(), b"not a socket");
    Ok(())
}

/// Sends frames as they are and reads one answer to each.
async fn raw(port: u16, frames: &[Vec<u8>]) -> Vec<Response> {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut answers = Vec::new();
    for frame in frames {
        stream.write_all(&(frame.len() as u32).to_le_bytes()).await.unwrap();
        stream.write_all(frame).await.unwrap();
        let mut len = [0u8; 4];
        if stream.read_exact(&mut len).await.is_err() {
            break;
        }
        let mut answer = vec![0u8; u32::from_le_bytes(len) as usize];
        stream.read_exact(&mut answer).await.unwrap();
        answers.push(decode_response(&answer).unwrap());
    }
    answers
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_answers_older_clients_and_refuses_newer_ones() -> DbResult<()> {
    let (server, db) = fresh().await;
    accounts(&db, 1, 100).await?;
    let hello = |version: u32| encode_request(&Request::Hello { version, database: "test".into(), password: PASSWORD.into(), client: "raw".into() }, 0);
    // A client from before these requests existed says an older version,
    // and everything it sends means what it meant.
    const { assert!(OLDEST_PROTOCOL_VERSION < PROTOCOL_VERSION) };
    for version in OLDEST_PROTOCOL_VERSION..=PROTOCOL_VERSION {
        let answers = raw(server.port, &[hello(version), encode_request(&Request::Query(Query::table("accounts").key([0])), 0), encode_request(&Request::Begin { kind: TxnKind::Read, lock_wait_ms: 0 }, 0), encode_request(&Request::Commit, 0)]).await;
        assert!(matches!(answers[0], Response::Hello { .. }), "version {version}: {:?}", answers[0]);
        assert!(matches!(&answers[1], Response::Rows { set, .. } if set.rows.len() == 1), "version {version}");
        assert!(matches!(answers[2..], [Response::Ok, Response::Ok]), "version {version}");
    }
    // One that is older still, or newer than the server, is told so at
    // once, before it can send what the server would not understand.
    for version in [OLDEST_PROTOCOL_VERSION - 1, PROTOCOL_VERSION + 1] {
        let answers = raw(server.port, &[hello(version)]).await;
        assert!(matches!(&answers[0], Response::Error(e) if e.code == ErrorCode::ProtocolMismatch), "version {version}: {:?}", answers[0]);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_answers_to_the_new_requests_are_what_the_old_ones_gave() -> DbResult<()> {
    // The same statement gives the same answer however it is sent.
    let server = TestServer::start().await;
    let db = one_connection(&server).await;
    accounts(&db, 3, 100).await?;
    let q = Query::table("accounts").key([1]).columns(["id", "balance"]);
    let plain = db.call(Request::Query(q.clone())).await?;
    let prepared = db.call(Request::Prepared { id: 5, statement: Some(Arc::new(Request::Query(Query::table("accounts").key([param(0)]).columns(["id", "balance"])))), params: vec![Value::Int(1)] }).await?;
    let begun = db.call(Request::BeginWith { kind: TxnKind::Read, lock_wait_ms: 0, statement: Box::new(Request::Query(q.clone())) }).await?;
    assert!(matches!(db.call(Request::Commit).await?, Response::Ok));
    assert_eq!(plain, prepared);
    assert_eq!(plain, begun);
    let Response::Rows { set, .. } = plain else { panic!("rows") };
    assert_eq!(set.rows, vec![vec![Value::Int(1), Value::Int(100)]]);
    Ok(())
}
