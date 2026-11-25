//! The benchmark's statements written without their way in: what the
//! server chooses for them, against the same statements as the benchmark
//! tunes them, and against a server that chooses nothing and reads the
//! whole table, as every server did before (R-Q23, R-Q24, R-W14).
//!
//! `UNTUNED_MAILS=5000 cargo test --release -p database-server --test
//! untuned -- --nocapture` runs it at the benchmark's size and prints the
//! figures `docs/PERFORMANCE.md` gives.

mod common;
use common::*;

const USERS: i64 = 20;
const SESSIONS: i64 = 2_000;
const BASE_DATE: i64 = 1_700_000_000_000_000;

fn mails_per_user() -> i64 {
    std::env::var("UNTUNED_MAILS").ok().and_then(|n| n.parse().ok()).unwrap_or(250)
}

fn user_id(n: i64) -> Uuid {
    uuid(0x1000_0000_0000_0000_0000_0000_0000_0000u128 + n as u128)
}

fn mail_id(user: i64, n: i64) -> Uuid {
    uuid(0x2000_0000_0000_0000_0000_0000_0000_0000u128 + ((user as u128) << 64) + (n as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) as u128)
}

fn token(n: i64) -> Vec<u8> {
    (n as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15).to_le_bytes().repeat(4)
}

/// The benchmark's users, sessions and mails (`bench/src/main.rs`), with
/// bodies of a few hundred bytes in place of four thousand.
async fn load(db: &Client) -> DbResult<()> {
    db.create_table(TableDef::new("users").column(ColumnDef::new("user_id", Type::Uuid)).column(ColumnDef::new("username", Type::Text)).column(ColumnDef::new("disabled", Type::Bool).default(false)).primary_key(["user_id"]).unique("users_username", ["username"]))
        .await?;
    db.create_table(
        TableDef::new("sessions")
            .column(ColumnDef::new("id", Type::Text))
            .column(ColumnDef::new("user_id", Type::Uuid))
            .column(ColumnDef::new("token_hash", Type::Bytes))
            .column(ColumnDef::new("expires_at", Type::Timestamp))
            .primary_key(["id"])
            .unique("sessions_token", ["token_hash"])
            .foreign_key(["user_id"], "users", OnDelete::Cascade),
    )
    .await?;
    db.create_table(
        TableDef::new("mails")
            .column(ColumnDef::new("mail_id", Type::Uuid))
            .column(ColumnDef::new("user_id", Type::Uuid))
            .column(ColumnDef::new("subject", Type::Text))
            .column(ColumnDef::new("date", Type::Timestamp))
            .column(ColumnDef::new("from_text", Type::Text))
            .column(ColumnDef::new("read", Type::Bool).default(false))
            .column(ColumnDef::new("saved", Type::Bool).default(false))
            .column(ColumnDef::new("text", Type::Text).keep_start(256))
            .column(ColumnDef::new("updated", Type::Timestamp).default_now())
            .primary_key(["mail_id"])
            .index(IndexDef::new("mails_user_date").on(["user_id"]).on_desc("date").on(["mail_id"])),
    )
    .await?;
    let mut users = Insert::into("users").columns(["user_id", "username"]);
    for u in 0..USERS {
        users = users.values([val(user_id(u)), val(format!("user{u}"))]);
    }
    db.insert(users).await?;
    for chunk in 0..SESSIONS / 500 {
        let mut insert = Insert::into("sessions").columns(["id", "user_id", "token_hash", "expires_at"]);
        for n in chunk * 500..(chunk + 1) * 500 {
            insert = insert.values([val(format!("ses_{n:08}")), val(user_id(n % USERS)), val(token(n)), val(Timestamp(BASE_DATE * 2))]);
        }
        db.insert(insert).await?;
    }
    for user in 0..USERS {
        for chunk in 0..mails_per_user() / 250 {
            let mut insert = Insert::into("mails").columns(["mail_id", "user_id", "subject", "date", "from_text", "read", "text"]);
            for n in chunk * 250..(chunk + 1) * 250 {
                // Every tenth mail shares its date with the one before.
                let date = BASE_DATE + (n - i64::from(n % 10 == 9)) * 60_000_000;
                insert = insert.values([val(mail_id(user, n)), val(user_id(user)), val(format!("subject {n}")), val(Timestamp(date)), val("someone@example.com"), val(n % 3 == 0), val(format!("the body of mail {n} ").repeat(12))]);
            }
            db.insert(insert).await?;
        }
    }
    Ok(())
}

/// Runs a query as the benchmark tunes it and as written without a way
/// in, the latter also on a server that chooses nothing (`before`);
/// checks that all three answer the same; and returns what each read.
async fn three_ways(db: &Client, before: &Client, what: &str, tuned: Query, untuned: Query) -> DbResult<[(u64, u64); 3]> {
    let rows = |db: &Client, q: Query| {
        let db = db.clone();
        async move { Ok::<_, DbError>(db.query(q).await?.iter().cloned().collect::<Vec<Row>>()) }
    };
    let as_tuned = rows(db, tuned.clone()).await?;
    assert!(!as_tuned.is_empty(), "{what}");
    assert_eq!(rows(db, untuned.clone()).await?, as_tuned, "{what}: chosen for");
    assert_eq!(rows(before, untuned.clone()).await?, as_tuned, "{what}: the whole table");
    let mut read = [(0, 0); 3];
    let mut line = format!("{what}:");
    for (n, (name, db, q)) in [("as tuned", db, tuned), ("no way in named", db, untuned.clone()), ("the same, nothing chosen", before, untuned)].into_iter().enumerate() {
        // Twice, and the second is reported: its pages are in the cache,
        // as they are for a statement an application runs all day.
        db.explain(q.clone()).await?;
        let explained = db.explain(q).await?;
        read[n] = (explained.index_entries, explained.rows_read);
        line.push_str(&format!("\n  {name}: {} index entries, {} rows, {:?}; {}", explained.index_entries, explained.rows_read, explained.took, explained.way));
    }
    println!("{line}");
    Ok(read)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_benchmarks_statements_without_their_way_in_read_what_the_tuned_ones_do() -> DbResult<()> {
    // R-Q23, R-Q24, R-W14
    let (_server, db) = fresh().await;
    load(&db).await?;
    let unchosen = TestServer::start_with(TempDir::new(), |config| config.plan = false).await;
    let before = unchosen.client("test").await;
    load(&before).await?;
    let per_user = mails_per_user() as u64;
    let mails = per_user * USERS as u64;
    let user = user_id(7);

    // The session lookup: a session by its token's hash, with its user.
    let tuned = Query::table("sessions")
        .alias("s")
        .index("sessions_token", KeyRange::eq([val(token(1_234))]))
        .join(Query::table("users").alias("u").key([col("s.user_id")]).filter(col("u.disabled").eq(false)))
        .filter(col("s.expires_at").gt(now()))
        .select("id", col("s.id"))
        .select("username", col("u.username"));
    let untuned = Query::table("sessions")
        .alias("s")
        .filter(col("s.token_hash").eq(token(1_234)).and(col("s.expires_at").gt(now())))
        .join(Query::table("users").alias("u").filter(col("u.user_id").eq(col("s.user_id")).and(col("u.disabled").eq(false))))
        .select("id", col("s.id"))
        .select("username", col("u.username"));
    let read = three_ways(&db, &before, "session lookup with user", tuned, untuned).await?;
    // Chosen for nothing, every session is read, and every user for
    // each.
    assert_eq!(read, [(1, 2), (1, 2), (0, SESSIONS as u64 * (1 + USERS as u64))]);

    // The mailbox page: a user's newest 50 mails, with the start of each.
    let page = |q: Query| q.limit(50).columns(["mail_id", "subject", "date", "from_text", "read", "saved"]).select("preview", col("text").left(200));
    let tuned = page(Query::table("mails").index("mails_user_date", KeyRange::eq([user])));
    let untuned = page(Query::table("mails").filter(col("user_id").eq(user)).order_by(col("date").desc()));
    let read = three_ways(&db, &before, "mailbox page: newest 50", tuned, untuned).await?;
    assert_eq!(read, [(50, 50), (50, 50), (0, mails)]);

    // The page after it, from where the first ended.
    let cursor = Timestamp(BASE_DATE + (mails_per_user() - 50) * 60_000_000);
    let tuned = page(Query::table("mails").index("mails_user_date", KeyRange::within([user], Bound::excluded([cursor]), Bound::Unbounded)));
    let untuned = page(Query::table("mails").filter(col("user_id").eq(user).and(col("date").lt(cursor))).order_by(col("date").desc()));
    let read = three_ways(&db, &before, "mailbox page: the next 50", tuned, untuned).await?;
    assert_eq!(read, [(50, 50), (50, 50), (0, mails)]);

    // The count of a user's mails, and of the unread among them.
    let counted = |q: Query| q.select("total", count()).select("unread", count().filter(col("read").eq(false)));
    let tuned = counted(Query::table("mails").index("mails_user_date", KeyRange::eq([user])));
    let untuned = counted(Query::table("mails").filter(col("user_id").eq(user)));
    let read = three_ways(&db, &before, "count unread of a user's mails", tuned, untuned).await?;
    assert_eq!(read, [(per_user, per_user), (per_user, per_user), (0, mails)]);

    // One mail by its id, as a filter.
    let read = three_ways(&db, &before, "one mail", Query::table("mails").key([mail_id(7, 3)]).columns(["mail_id", "subject"]), Query::table("mails").filter(col("mail_id").eq(mail_id(7, 3))).columns(["mail_id", "subject"])).await?;
    assert_eq!(read, [(0, 1), (0, 1), (0, mails)]);

    // The flag update, by a filter on the key: the same row changed, and
    // found by one read.
    let flag = Update::table("mails").filter(col("mail_id").eq(mail_id(7, 3))).set("read", true).returning(["mail_id", "read"]);
    let changed = db.update(flag).await?;
    assert_eq!(changed.len(), 1);
    assert!(db.get("mails", [mail_id(7, 3)]).await?.unwrap().get::<bool>("read")?);
    // And every mail of a user marked read, found through the index.
    assert_eq!(db.update(Update::table("mails").filter(col("user_id").eq(user)).set("read", true)).await?.affected(), per_user);
    assert_eq!(db.query_one(Query::table("mails").index("mails_user_date", KeyRange::eq([user])).select("unread", count().filter(col("read").eq(false)))).await?.get::<i64>("unread")?, 0);
    assert_eq!(db.query_one(Query::table("mails").whole().select("unread", count().filter(col("read").eq(false)))).await?.get::<i64>("unread")?, ((USERS as u64 - 1) * (per_user - per_user.div_ceil(3))) as i64);
    Ok(())
}
