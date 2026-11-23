//! A mail store as a scenario suite: users and their sessions, mailboxes,
//! messages with large bodies, flags, address lists, UID and MODSEQ
//! counters, a spam allowlist and trained word counts, conversations and
//! full-text search. The tests say through the client what a mail server
//! with HTTP, IMAP and SMTP front ends asks of its database, and check what
//! must come back.
//!
//! Such a server runs nearly everything as lone statements on whichever
//! pooled connection is free, so these tests use the plain client the same
//! way. They open a transaction only where the work needs one: a schema
//! migration, or a count and an insert that nothing may come between.
//!
//! The indexes are chosen for the queries. Each access path has the ordered
//! key it needs, and every query names the index it reads through. There
//! are no one-column indexes on booleans, which would help no query.
//!
//! Three things that SQL says directly are said another way here, with a
//! note at the place:
//!
//! - a SELECT without a FROM (an `INSERT .. SELECT <constants> WHERE ..`,
//!   a counter read without reserving, the server's clock): these tests
//!   compute the value beside a row or an aggregate that is certain to be
//!   there, or make the statement a transaction;
//! - `UPDATE mails .. FROM mail_mailbox_uid`: an update reads its own table
//!   first, so a STORE in a mapped box starts from the user's mails and
//!   looks up each one's mapping row, not the other way round;
//! - the entries of a JSON array of strings (`reference_ids ?|` in SQL):
//!   the index over them holds JSON strings, and these tests read it with
//!   JSON strings.
//!
//! Where `docs/CLIENT.md` is silent the tests expect what Postgres does:
//! `substr` and `substr_bytes` past the end give the empty value, a sum of
//! integers is an integer, a timestamp cast to text keeps its microseconds
//! and reads back as the same instant, and a full-text excerpt marks each
//! matched word as it stands in the text.

mod common;
use common::*;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

/// One row per (user, Message-ID): the duplicate-delivery signal.
const MAILS_MESSAGE: &str = "mails_user_message_unique";
/// The domain-wide IMAP UID space: (user_id, sent, uid_domain).
const MAILS_UID: &str = "mails_user_sent_uid";
/// A user's mails newest first: (user_id, date DESC, mail_id DESC).
const MAILS_DATE: &str = "mails_user_date";
/// Delta sync: (user_id, updated).
const MAILS_UPDATED: &str = "mails_user_updated";
/// Replies to a message: (user_id, in_reply_to).
const MAILS_REPLY: &str = "mails_user_in_reply_to";
/// One entry per recipient address, newest first within the address.
const MAILS_RECIPIENT: &str = "mails_recipient";
/// The same per sender address.
const MAILS_SENDER: &str = "mails_sender";
/// One entry per Message-ID a mail lists in `References`.
const MAILS_REFERENCE: &str = "mails_reference";
/// Mails without threading headers, by normalised subject.
const MAILS_SUBJECT: &str = "mails_thread_subject";
/// The words of subject, body, sender and recipients, per user.
const MAILS_SEARCH: &str = "mails_search";
/// A mapped mailbox in UID order: unique (user_id, mailbox, uid).
const BOX_UID: &str = "mail_mailbox_uid_box_uid";
/// The mailboxes a mail is in: (mail_id).
const BOX_MAIL: &str = "mail_mailbox_uid_mail";

/// Every recipient address of a mail, from its four address arrays. NULL
/// arrays count as empty.
fn recipients() -> Expr {
    col("to_address")
        .json_pluck("address")
        .list_concat(col("cc_address").json_pluck("address"))
        .list_concat(col("bcc_address").json_pluck("address"))
        .list_concat(col("envelope_to").json_pluck("address"))
}

fn schema_meta_table() -> TableDef {
    TableDef::new("schema_meta").column(ColumnDef::new("key", Type::Text)).column(ColumnDef::new("value", Type::Text)).primary_key(["key"])
}

fn users_table() -> TableDef {
    TableDef::new("users")
        .column(ColumnDef::new("user_id", Type::Uuid).default_random_uuid())
        .column(ColumnDef::new("username", Type::Text))
        .column(ColumnDef::new("password", Type::Text).nullable())
        .column(ColumnDef::new("password_version", Type::Int).nullable().default(1))
        .column(ColumnDef::new("email", Type::Text).nullable())
        .column(ColumnDef::new("expiry", Type::Timestamp).nullable())
        .column(ColumnDef::new("token", Type::Text).nullable())
        .column(ColumnDef::new("updated", Type::Timestamp).nullable().default_now())
        .column(ColumnDef::new("is_deleted", Type::Bool).nullable().default(false))
        .column(ColumnDef::new("imap_uid_validity", Type::Int).nullable())
        .column(ColumnDef::new("auth_sub", Type::Text).nullable())
        .primary_key(["user_id"])
        .unique("users_username_key", ["username"])
        // Unique among the users that have one: most have none.
        .unique("users_auth_sub_key", ["auth_sub"])
        // Linking a login compares emails without case; the user lookup
        // compares them exactly and narrows this index's rows with a filter.
        .index(IndexDef::new("users_email_lower").on_expr(col("email").lower()))
}

fn sessions_table() -> TableDef {
    let mut table = TableDef::new("sessions")
        .column(ColumnDef::new("session_id", Type::Text))
        .column(ColumnDef::new("session_user_id", Type::Uuid))
        .column(ColumnDef::new("session_username", Type::Text))
        .column(ColumnDef::new("session_email", Type::Text));
    let optional = [
        ("session_is_read_only", Type::Bool),
        ("session_authenticated_as", Type::Text),
        ("session_password_version", Type::Int),
        ("auth_sid", Type::Text),
        ("auth_refresh_token", Type::Text),
        ("auth_recheck_at", Type::Timestamp),
        ("cookie_original_max_age", Type::Int),
        ("cookie_max_age", Type::Int),
        ("cookie_signed", Type::Bool),
        ("cookie_expires", Type::Timestamp),
        ("cookie_http_only", Type::Bool),
        ("cookie_path", Type::Text),
        ("cookie_domain", Type::Text),
        ("cookie_secure", Type::Text),
        ("cookie_same_site", Type::Text),
    ];
    for (name, ty) in optional {
        table = table.column(ColumnDef::new(name, ty).nullable());
    }
    // No foreign key to users: sessions are removed by explicit deletes.
    table
        .column(ColumnDef::new("updated", Type::Timestamp).nullable().default_now())
        .primary_key(["session_id"])
        .index(IndexDef::new("sessions_user").on(["session_user_id"]))
        .index(IndexDef::new("sessions_auth_sid").on(["auth_sid"]))
        // The hourly purge reads the expired end of this; a session
        // without an expiry is not in it and so is never purged.
        .index(IndexDef::new("sessions_cookie_expires").on(["cookie_expires"]).filter(col("cookie_expires").is_not_null()))
}

fn mailboxes_table() -> TableDef {
    TableDef::new("mailboxes")
        .column(ColumnDef::new("mailbox_id", Type::Uuid).default_random_uuid())
        .column(ColumnDef::new("user_id", Type::Uuid))
        .column(ColumnDef::new("name", Type::Text))
        .column(ColumnDef::new("address", Type::Text).nullable())
        .column(ColumnDef::new("parent_id", Type::Uuid).nullable())
        .column(ColumnDef::new("uid_validity", Type::Int).default(1))
        .column(ColumnDef::new("uid_next", Type::Int).default(1))
        .column(ColumnDef::new("subscribed", Type::Bool).default(true))
        .column(ColumnDef::new("special_use", Type::Text).nullable())
        .column(ColumnDef::new("created", Type::Timestamp).nullable().default_now())
        .primary_key(["mailbox_id"])
        // Not unique: the application probes for the name first.
        .index(IndexDef::new("mailboxes_user_name").on(["user_id"]).on_expr(col("name").lower()))
        .foreign_key(["user_id"], "users", OnDelete::Cascade)
        .foreign_key(["parent_id"], "mailboxes", OnDelete::Cascade)
}

fn mails_table() -> TableDef {
    let mut table = TableDef::new("mails")
        .column(ColumnDef::new("mail_id", Type::Uuid).default_random_uuid())
        .column(ColumnDef::new("user_id", Type::Uuid))
        .column(ColumnDef::new("message_id", Type::Text))
        .column(ColumnDef::new("subject", Type::Text).default(""))
        .column(ColumnDef::new("date", Type::Timestamp).default_now())
        .column(ColumnDef::new("html", Type::Text).default(""))
        .column(ColumnDef::new("text", Type::Text).default(""));
    for header in ["from", "to", "cc", "bcc", "reply_to"] {
        table = table
            .column(ColumnDef::new(&format!("{header}_address"), Type::Json).nullable())
            .column(ColumnDef::new(&format!("{header}_text"), Type::Text).nullable());
    }
    for name in ["envelope_from", "envelope_to", "attachments"] {
        table = table.column(ColumnDef::new(name, Type::Json).nullable());
    }
    for flag in ["read", "saved", "sent", "deleted", "draft", "answered", "expunged"] {
        table = table.column(ColumnDef::new(flag, Type::Bool).default(false));
    }
    table
        .column(ColumnDef::new("insight", Type::Json).nullable())
        .column(ColumnDef::new("uid_domain", Type::Int).default(0))
        .column(ColumnDef::new("modseq", Type::Int).default(1))
        .column(ColumnDef::new("spam_score", Type::Int).default(0))
        .column(ColumnDef::new("spam_reasons", Type::Json).nullable())
        .column(ColumnDef::new("is_spam", Type::Bool).default(false))
        .column(ColumnDef::new("rfc822_size", Type::Int).nullable())
        .column(ColumnDef::new("text_line_count", Type::Int).nullable())
        .column(ColumnDef::new("html_line_count", Type::Int).nullable())
        .column(ColumnDef::new("in_reply_to", Type::Text).nullable())
        .column(ColumnDef::new("reference_ids", Type::Json).nullable())
        .column(ColumnDef::new("updated", Type::Timestamp).nullable().default_now())
        // Where Postgres would index md5(lower(btrim(regexp_replace(..)))),
        // the application normalises the subject (`normalize_subject`) and
        // stores the result. There is no `search_vector` column: the
        // full-text index below is kept by the database.
        .column(ColumnDef::new("thread_subject_key", Type::Text).nullable())
        .primary_key(["mail_id"])
        .unique(MAILS_MESSAGE, ["user_id", "message_id"])
        .index(IndexDef::new(MAILS_UID).on(["user_id", "sent", "uid_domain"]))
        .index(IndexDef::new(MAILS_DATE).on(["user_id"]).on_desc("date").on_desc("mail_id"))
        .index(IndexDef::new(MAILS_UPDATED).on(["user_id", "updated"]))
        .index(IndexDef::new(MAILS_REPLY).on(["user_id", "in_reply_to"]).filter(col("in_reply_to").is_not_null()))
        .index(IndexDef::new(MAILS_RECIPIENT).on(["user_id"]).on_each(recipients()).on_desc("date").on_desc("mail_id"))
        .index(IndexDef::new(MAILS_SENDER).on(["user_id"]).on_each(col("from_address").json_pluck("address")).on_desc("date").on_desc("mail_id"))
        .index(IndexDef::new(MAILS_REFERENCE).on(["user_id"]).on_each(call(Func::JsonElems, [col("reference_ids")])))
        .index(
            IndexDef::new(MAILS_SUBJECT)
                .on(["user_id", "thread_subject_key"])
                .on_desc("date")
                .filter(col("in_reply_to").is_null().and(col("reference_ids").is_null())),
        )
        // Like one Postgres tsvector built from these four columns:
        // unweighted, a missing header counting as no words.
        .index(IndexDef::new(MAILS_SEARCH).on(["user_id"]).full_text(
            TextConfig::English,
            [(col("subject"), 1.0), (col("text"), 1.0), (col("from_text").or_else(""), 1.0), (col("to_text").or_else(""), 1.0)],
        ))
        .foreign_key(["user_id"], "users", OnDelete::Cascade)
}

fn push_subscriptions_table() -> TableDef {
    TableDef::new("push_subscriptions")
        .column(ColumnDef::new("push_subscription_id", Type::Uuid).default_random_uuid())
        .column(ColumnDef::new("user_id", Type::Uuid))
        .column(ColumnDef::new("endpoint", Type::Text))
        .column(ColumnDef::new("keys_p256dh", Type::Text))
        .column(ColumnDef::new("keys_auth", Type::Text))
        .column(ColumnDef::new("last_notified", Type::Timestamp).nullable())
        .column(ColumnDef::new("updated", Type::Timestamp).nullable().default_now())
        .primary_key(["push_subscription_id"])
        .index(IndexDef::new("push_subscriptions_user").on(["user_id"]))
        .foreign_key(["user_id"], "users", OnDelete::Cascade)
}

fn spam_allowlist_table() -> TableDef {
    TableDef::new("spam_allowlist")
        .column(ColumnDef::new("allowlist_id", Type::Uuid).default_random_uuid())
        .column(ColumnDef::new("user_id", Type::Uuid))
        .column(ColumnDef::new("pattern", Type::Text))
        .column(ColumnDef::new("created_at", Type::Timestamp).default_now())
        .primary_key(["allowlist_id"])
        .unique("spam_allowlist_user_pattern", ["user_id", "pattern"])
        // The check made for every received mail compares lower(pattern).
        .index(IndexDef::new("spam_allowlist_user_lower").on(["user_id"]).on_expr(col("pattern").lower()))
        .foreign_key(["user_id"], "users", OnDelete::Cascade)
}

fn spam_training_table() -> TableDef {
    TableDef::new("spam_training")
        .column(ColumnDef::new("training_id", Type::Uuid).default_random_uuid())
        .column(ColumnDef::new("user_id", Type::Uuid))
        .column(ColumnDef::new("word", Type::Text))
        .column(ColumnDef::new("spam_count", Type::Int).default(0))
        .column(ColumnDef::new("ham_count", Type::Int).default(0))
        .primary_key(["training_id"])
        .unique("spam_training_user_word", ["user_id", "word"])
        .foreign_key(["user_id"], "users", OnDelete::Cascade)
        // Like VARCHAR(100): an over-long word must fail the whole statement.
        .check("spam_training_word_length", col("word").length().le(100))
}

fn mail_uid_counters_table() -> TableDef {
    // No foreign key: counter rows outlive their user.
    TableDef::new("mail_uid_counters")
        .column(ColumnDef::new("user_id", Type::Uuid))
        .column(ColumnDef::new("uid_kind", Type::Text))
        .column(ColumnDef::new("uid_scope", Type::Text))
        .column(ColumnDef::new("sent", Type::Bool))
        .column(ColumnDef::new("last_uid", Type::Int))
        .primary_key(["user_id", "uid_kind", "uid_scope", "sent"])
}

fn mail_mailbox_uid_table() -> TableDef {
    TableDef::new("mail_mailbox_uid")
        .column(ColumnDef::new("user_id", Type::Uuid))
        .column(ColumnDef::new("mailbox", Type::Text))
        .column(ColumnDef::new("mail_id", Type::Uuid))
        .column(ColumnDef::new("uid", Type::Int))
        .primary_key(["user_id", "mailbox", "mail_id"])
        .unique(BOX_UID, ["user_id", "mailbox", "uid"])
        .index(IndexDef::new(BOX_MAIL).on(["mail_id"]))
        .foreign_key(["mail_id"], "mails", OnDelete::Cascade)
}

fn delivery_events_table() -> TableDef {
    // Append-only and never read back, so no index beyond its key.
    let mut table = TableDef::new("delivery_events").column(ColumnDef::new("event_id", Type::Text)).column(ColumnDef::new("event", Type::Text));
    for name in ["message_id", "recipient", "severity", "reason"] {
        table = table.column(ColumnDef::new(name, Type::Text).nullable());
    }
    table
        .column(ColumnDef::new("occurred_at", Type::Timestamp))
        .column(ColumnDef::new("received_at", Type::Timestamp).default_now())
        .column(ColumnDef::new("raw", Type::Json))
        .column(ColumnDef::new("updated", Type::Timestamp).default_now())
        .primary_key(["event_id"])
}

/// The store's tables, parents before the tables that point at them.
fn tables() -> Vec<TableDef> {
    vec![
        schema_meta_table(),
        users_table(),
        sessions_table(),
        mailboxes_table(),
        mails_table(),
        push_subscriptions_table(),
        spam_allowlist_table(),
        spam_training_table(),
        mail_uid_counters_table(),
        mail_mailbox_uid_table(),
        delivery_events_table(),
    ]
}

/// What every boot does, whatever state the database is in: create each
/// table unless it is there.
async fn schema(db: &Client) -> DbResult<()> {
    for table in tables() {
        db.create_table_if_not_exists(table).await?;
    }
    Ok(())
}

/// `RETURNING *`: every column of a table, in the model's order.
fn all_columns(table: &TableDef) -> Vec<String> {
    table.columns.iter().map(|c| c.name.clone()).collect()
}

/// Another process of the application: its own pool on the same database.
async fn second_instance(db: &Client) -> DbResult<Client> {
    Client::connect(db.config().clone()).await
}

/// A user with a password, whose id the server picks.
async fn user(db: &Client, name: &str) -> DbResult<Uuid> {
    let insert = Insert::into("users")
        .set("updated", now())
        .set("username", name)
        .set("password", "$2b$10$abcdefghijklmnopqrstuv")
        .set("email", format!("{name}@example.com"))
        .returning(["user_id"]);
    db.insert(insert).await?.one()?.get("user_id")
}

/// The soft-delete test that every read of `users` applies. Rows from
/// before the flag had a default hold NULL, and count as not deleted.
fn not_deleted() -> Expr {
    col("is_deleted").is_null().or(col("is_deleted").eq(false))
}

/// `n` seconds after a fixed moment that has microseconds in it.
fn at(n: i64) -> Timestamp {
    ts("2026-11-15T02:55:04.123456Z").plus_secs(n)
}

/// A list of ids as a value for `in_list`.
fn id_list(ids: &[Uuid]) -> Vec<Value> {
    ids.iter().map(|id| Value::from(*id)).collect()
}

/// A row of `mails` as the application holds it before the insert. An empty
/// address list stands for a header the message did not have: NULL in both
/// the `_address` and the `_text` column.
#[derive(Clone)]
struct Mail {
    mail_id: Uuid,
    user_id: Uuid,
    message_id: String,
    subject: String,
    date: Timestamp,
    html: String,
    text: String,
    from: Vec<&'static str>,
    to: Vec<&'static str>,
    cc: Vec<&'static str>,
    bcc: Vec<&'static str>,
    envelope_to: Vec<&'static str>,
    attachments: Option<Json>,
    insight: Option<Json>,
    read: bool,
    saved: bool,
    sent: bool,
    deleted: bool,
    draft: bool,
    answered: bool,
    expunged: bool,
    is_spam: bool,
    uid_domain: i64,
    modseq: i64,
    in_reply_to: Option<String>,
    reference_ids: Option<Json>,
}

/// The `n`th test mail of a user: received, unread, addressed to
/// ann@example.com, with `n` as its id and its domain UID, and dated `n`
/// seconds after `at(0)`, so a higher `n` is a newer mail.
fn mail(user: Uuid, n: i64) -> Mail {
    Mail {
        mail_id: uuid(n as u128),
        user_id: user,
        message_id: format!("<m{n}@remote.test>"),
        subject: format!("Mail {n}"),
        date: at(n),
        html: String::new(),
        text: format!("Body of mail {n}"),
        from: vec!["bob@remote.test"],
        to: vec!["ann@example.com"],
        cc: Vec::new(),
        bcc: Vec::new(),
        envelope_to: Vec::new(),
        attachments: None,
        insight: None,
        read: false,
        saved: false,
        sent: false,
        deleted: false,
        draft: false,
        answered: false,
        expunged: false,
        is_spam: false,
        uid_domain: n,
        modseq: 1,
        in_reply_to: None,
        reference_ids: None,
    }
}

/// An address header as the parsed array a row stores: `{address, name}`.
fn address_list(addresses: &[&str]) -> Option<Json> {
    if addresses.is_empty() {
        return None;
    }
    Some(Json::Array(addresses.iter().map(|a| json!({"address": a, "name": a.split('@').next()})).collect()))
}

/// The same header as received: `ann <ann@example.com>, ...`.
fn header_text(addresses: &[&str]) -> Option<String> {
    if addresses.is_empty() {
        return None;
    }
    Some(addresses.iter().map(|a| format!("{} <{a}>", a.split('@').next().unwrap_or(a))).collect::<Vec<_>>().join(", "))
}

/// The application's subject normaliser for threading: a regexp, a trim and
/// a lower-casing, which Postgres could also evaluate inside an index. Reply
/// and forward prefixes go (`Re:`, `FWD:`, `AW[2]:`, with a plain or a
/// full-width colon), runs of whitespace become one space.
fn normalize_subject(subject: &str) -> String {
    const PREFIXES: [&str; 10] = ["re", "fwd", "fw", "aw", "wg", "sv", "vs", "antw", "tr", "rv"];
    let mut rest = subject.trim_start();
    while let Some(prefix) = PREFIXES.iter().find(|p| rest.get(..p.len()).is_some_and(|head| head.eq_ignore_ascii_case(p))) {
        let mut after = rest[prefix.len()..].trim_start();
        if let Some((count, tail)) = after.strip_prefix('[').and_then(|inner| inner.split_once(']')) {
            if !count.is_empty() && count.chars().all(|c| c.is_ascii_digit()) {
                after = tail.trim_start();
            }
        }
        let Some(tail) = after.strip_prefix(':').or_else(|| after.strip_prefix('：')) else { break };
        rest = tail.trim_start();
    }
    rest.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// The columns and values of a mail's insert, in order: the server's clock
/// for `updated`, then the 36 values the application binds, then the
/// subject key.
fn mail_values(m: &Mail) -> Vec<(&'static str, Expr)> {
    vec![
        ("updated", now()),
        ("mail_id", val(m.mail_id)),
        ("user_id", val(m.user_id)),
        ("message_id", val(&m.message_id)),
        ("subject", val(&m.subject)),
        ("date", val(m.date)),
        ("html", val(&m.html)),
        ("text", val(&m.text)),
        ("rfc822_size", null()),
        ("from_address", val(address_list(&m.from))),
        ("from_text", val(header_text(&m.from))),
        ("to_address", val(address_list(&m.to))),
        ("to_text", val(header_text(&m.to))),
        ("cc_address", val(address_list(&m.cc))),
        ("cc_text", val(header_text(&m.cc))),
        ("bcc_address", val(address_list(&m.bcc))),
        ("bcc_text", val(header_text(&m.bcc))),
        ("reply_to_address", null()),
        ("reply_to_text", null()),
        ("envelope_from", val(address_list(&m.from))),
        ("envelope_to", val(address_list(&m.envelope_to))),
        ("attachments", val(m.attachments.clone())),
        ("read", val(m.read)),
        ("saved", val(m.saved)),
        ("sent", val(m.sent)),
        ("deleted", val(m.deleted)),
        ("draft", val(m.draft)),
        ("answered", val(m.answered)),
        ("expunged", val(m.expunged)),
        ("insight", val(m.insight.clone())),
        ("uid_domain", val(m.uid_domain)),
        ("modseq", val(m.modseq)),
        ("spam_score", val(0)),
        ("spam_reasons", null()),
        ("is_spam", val(m.is_spam)),
        ("in_reply_to", val(m.in_reply_to.clone())),
        ("reference_ids", val(m.reference_ids.clone())),
        ("thread_subject_key", val(normalize_subject(&m.subject))),
    ]
}

/// The insert of one mail or, for a test's starting data, of several in
/// one statement.
fn insert_mails(mails: &[Mail]) -> Insert {
    let mut insert = Insert::into("mails").columns(mail_values(&mails[0]).into_iter().map(|(column, _)| column));
    for m in mails {
        insert = insert.values(mail_values(m).into_iter().map(|(_, value)| value));
    }
    insert.returning(["mail_id"])
}

async fn save(db: &Client, m: &Mail) -> DbResult<Rows> {
    db.insert(insert_mails(std::slice::from_ref(m))).await
}

async fn save_all(db: &Client, mails: &[Mail]) -> DbResult<()> {
    let saved = db.insert(insert_mails(mails)).await?;
    assert_eq!(saved.affected(), mails.len() as u64);
    Ok(())
}

/// Sets every mail's `updated` an hour back, so a test can tell which rows
/// a later statement stamped with the clock.
async fn rewind_updated(db: &Client, user: Uuid) -> DbResult<Timestamp> {
    let stale =
        db.update(Update::table("mails").index(MAILS_DATE, KeyRange::eq([user])).set("updated", now() - hours(1)).returning(["updated"])).await?;
    stale.first().expect("the user has mails").get("updated")
}

/// A mail's `updated`, to compare with `rewind_updated`'s answer.
async fn updated_of(db: &Client, mail_id: Uuid) -> DbResult<Timestamp> {
    db.query_one(Query::table("mails").key([mail_id]).columns(["updated"])).await?.get("updated")
}

/// Where a counter starts when it is first used: one past the highest
/// number the data already holds.
struct Seed {
    /// The rows that hold numbers of this counter.
    rows: Query,
    column: &'static str,
}

impl Seed {
    /// `COALESCE(MAX(column), 0) + 1`.
    fn value(&self) -> Expr {
        max(col(self.column)).or_else(0) + 1
    }

    /// `SELECT COALESCE(MAX(column), 0) + 1 FROM ..`, to put inside a statement.
    fn subquery(&self) -> Expr {
        scalar(self.rows.clone().select("seed", self.value()))
    }
}

/// Domain UIDs continue from `mails.uid_domain` of the user's mails on one
/// side of `sent`.
fn domain_seed(user: Uuid, sent: bool) -> Seed {
    Seed { rows: Query::table("mails").index(MAILS_UID, KeyRange::eq([val(user), val(sent)])), column: "uid_domain" }
}

/// A mapped box's UIDs continue from the mapping rows of the mailboxes
/// that share the counter (`mailbox IN (..)`).
fn box_seed(user: Uuid, mailboxes: &[&str]) -> Seed {
    let ranges = mailboxes.iter().map(|mailbox| KeyRange::eq([val(user), val(*mailbox)]));
    Seed { rows: Query::table("mail_mailbox_uid").index_ranges(BOX_UID, ranges), column: "uid" }
}

/// MODSEQ continues from the highest `modseq` of any of the user's mails.
fn modseq_seed(user: Uuid) -> Seed {
    Seed { rows: Query::table("mails").index(MAILS_DATE, KeyRange::eq([user])), column: "modseq" }
}

/// Reserves the next number of a counter. The first reservation inserts
/// the seed, read from the data by a subquery in the same statement; every
/// later one finds the row and adds one. Either way the statement returns
/// the number reserved.
fn reserve(user: Uuid, kind: &str, scope: &str, sent: bool, seed: &Seed) -> Insert {
    Insert::into("mail_uid_counters")
        .set("user_id", user)
        .set("uid_kind", kind)
        .set("uid_scope", scope)
        .set("sent", sent)
        .set("last_uid", seed.subquery())
        .on_conflict_update(PRIMARY)
        .update("last_uid", col("last_uid") + 1)
        .returning_expr("next_uid", col("last_uid"))
}

async fn reserved(db: &Client, insert: Insert) -> DbResult<i64> {
    db.insert(insert).await?.one()?.get("next_uid")
}

async fn next_domain_uid(db: &Client, user: Uuid, sent: bool) -> DbResult<i64> {
    reserved(db, reserve(user, "domain", "", sent, &domain_seed(user, sent))).await
}

/// Every mutation of a user's mail stamps a freshly reserved MODSEQ.
async fn next_modseq(db: &Client, user: Uuid) -> DbResult<i64> {
    reserved(db, reserve(user, "modseq", "", false, &modseq_seed(user))).await
}

/// UIDNEXT without allocating: the counter's next value if the counter
/// exists, else the seed it would start from. SQL can say this without a
/// FROM; here the seed's aggregate query carries the counter lookup, and an
/// aggregate without groups always gives one row.
fn peek(user: Uuid, kind: &str, scope: &str, sent: bool, seed: &Seed) -> Query {
    let counter = Query::table("mail_uid_counters").key([val(user), val(kind), val(scope), val(sent)]).select("next", col("last_uid") + 1);
    seed.rows.clone().select("uid_next", coalesce([scalar(counter), seed.value()]))
}

async fn uid_next(db: &Client, query: Query) -> DbResult<i64> {
    db.query_one(query).await?.get("uid_next")
}

/// Records a mail's UID in a mailbox, or learns the one it already has
/// there. The no-op update exists to make the statement return the row.
fn map_mail(user: Uuid, mailbox: &str, mail_id: Uuid, uid: i64) -> Insert {
    Insert::into("mail_mailbox_uid")
        .set("user_id", user)
        .set("mailbox", mailbox)
        .set("mail_id", mail_id)
        .set("uid", uid)
        .on_conflict_update(PRIMARY)
        .update("uid", col("uid"))
        .returning(["uid"])
}

async fn mapped(db: &Client, user: Uuid, mailbox: &str, mail_id: Uuid, uid: i64) -> DbResult<i64> {
    db.insert(map_mail(user, mailbox, mail_id, uid)).await?.one()?.get("uid")
}

/// Which of a user's mails a mailbox shows: its membership test. INBOX and
/// the per-account inboxes hide spam and drafts; Drafts and Junk show only
/// those; Sent, user boxes, Starred and Trash show all. The conditions name
/// the mail as `m`, as every IMAP query here does.
#[derive(Clone, Copy)]
enum Members {
    Inbox,
    Drafts,
    Junk,
    All,
}

impl Members {
    fn condition(self) -> Expr {
        match self {
            Members::Inbox => col("m.is_spam").eq(false).and(col("m.draft").eq(false)),
            Members::Drafts => col("m.draft").eq(true),
            Members::Junk => col("m.is_spam").eq(true),
            Members::All => val(true),
        }
    }
}

/// The domain UID space: a user's mails on one side of `sent`, in
/// `uid_domain` order. Used with a membership filter for INBOX, Drafts,
/// Junk and the unified Sent box.
fn domain_rows(user: Uuid, sent: bool) -> Query {
    Query::table("mails").alias("m").index(MAILS_UID, KeyRange::eq([val(user), val(sent)]))
}

/// The UIDs `low..=high` of that space.
fn domain_uids(user: Uuid, sent: bool, low: i64, high: i64) -> KeyRange {
    KeyRange::within([val(user), val(sent)], Bound::included([low]), Bound::included([high]))
}

/// What a domain view shows: not expunged, and a member.
fn domain_view(user: Uuid, sent: bool, members: Members) -> Query {
    domain_rows(user, sent).filter(col("m.expunged").eq(false).and(members.condition()))
}

/// The mapped UID space: the mapping rows of one mailbox in `uid` order,
/// each joined to its mail by primary key.
fn mapped_rows(user: Uuid, sent: bool, mailbox: &str) -> Query {
    Query::table("mail_mailbox_uid")
        .alias("x")
        .index(BOX_UID, KeyRange::eq([val(user), val(mailbox)]))
        .join(Query::table("mails").alias("m").key([col("x.mail_id")]).filter(col("m.user_id").eq(col("x.user_id")).and(col("m.sent").eq(sent))))
}

/// The UIDs `low..=high` of a mapped box.
fn mapped_uids(user: Uuid, mailbox: &str, low: i64, high: i64) -> KeyRange {
    KeyRange::within([val(user), val(mailbox)], Bound::included([low]), Bound::included([high]))
}

fn mapped_view(user: Uuid, sent: bool, mailbox: &str, members: Members) -> Query {
    mapped_rows(user, sent, mailbox).filter(col("m.expunged").eq(false).and(members.condition()))
}

/// A mailbox's message count and unseen count. The membership test sits
/// inside the aggregates, as a filter on each count.
fn counts(rows: Query, members: Members) -> Query {
    rows.filter(col("m.expunged").eq(false))
        .select("total", count().filter(members.condition()))
        .select("unread", count().filter(col("m.read").eq(false).and(members.condition())))
}

/// The five IMAP flags, as columns of the mail aliased `m`.
const FLAGS: [&str; 5] = ["m.read", "m.saved", "m.deleted", "m.draft", "m.answered"];

// R-S1, R-S2, R-S4, R-S9: every boot creates the tables that are absent, and the schema reads back as it was declared.
#[tokio::test(flavor = "multi_thread")]
async fn tables_are_created_if_absent_and_described_back() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    // Every boot issues the same statements; the second time they change nothing.
    schema(&db).await?;

    let live = db.describe().await?;
    assert_eq!(live.len(), 11);
    for want in tables() {
        let have = live.iter().find(|t| t.name == want.name).expect("every table is described");
        // What a migration compares: names, types and nullability, in order.
        let shape = |t: &TableDef| t.columns.iter().map(|c| (c.name.clone(), c.ty.clone(), c.nullable)).collect::<Vec<_>>();
        assert_eq!(shape(have), shape(&want), "{}", want.name);
        assert_eq!(have.primary_key, want.primary_key, "{}", want.name);
        for index in &want.indexes {
            let described = have.find_index(&index.name).unwrap_or_else(|| panic!("{} is described", index.name));
            assert_eq!(described.unique, index.unique, "{}", index.name);
        }
    }
    let counters = db.describe_table("mail_uid_counters").await?.expect("described");
    assert_eq!(counters.primary_key, ["user_id", "uid_kind", "uid_scope", "sent"]);
    assert!(db.describe_table("search_vectors").await?.is_none());

    // Without "if absent" a table that exists is an error, and it stays as it was.
    let again = db.create_table(TableDef::new("schema_meta").column(ColumnDef::new("key", Type::Int)).primary_key(["key"])).await.unwrap_err();
    assert_eq!(again.code, ErrorCode::AlreadyExists);
    assert_eq!(db.describe_table("schema_meta").await?.expect("still there").columns.len(), 2);
    Ok(())
}

// R-S5, R-W1, R-W11: the ids, clock stamps and flags the server fills in.
#[tokio::test(flavor = "multi_thread")]
async fn columns_left_out_take_their_defaults() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;

    let bare = || Insert::into("mails").set("user_id", ann).returning(all_columns(&mails_table()));
    let before = db.now().await?;
    let row = db.insert(bare().set("message_id", "<bare-1@remote.test>")).await?.one()?;
    let after = db.now().await?;

    let id: Uuid = row.get("mail_id")?;
    assert_eq!(id.0[6] >> 4, 4, "a generated id is a version 4 uuid");
    assert_eq!(
        (row.get::<String>("subject")?, row.get::<String>("html")?, row.get::<String>("text")?),
        (String::new(), String::new(), String::new())
    );
    for flag in ["read", "saved", "sent", "deleted", "draft", "answered", "expunged", "is_spam"] {
        assert!(!row.get::<bool>(flag)?, "{flag}");
    }
    assert_eq!((row.get::<i64>("uid_domain")?, row.get::<i64>("modseq")?, row.get::<i64>("spam_score")?), (0, 1, 0));
    for column in ["from_address", "cc_text", "attachments", "insight", "rfc822_size", "in_reply_to", "reference_ids", "thread_subject_key"] {
        assert!(row.is_null(column)?, "{column}");
    }
    // Both clock defaults of one statement read the same instant, and it
    // is the server's.
    let (date, updated): (Timestamp, Timestamp) = (row.get("date")?, row.get("updated")?);
    assert_eq!(date, updated);
    assert!(before <= date && date <= after);

    // Each row gets an id of its own.
    let second: Uuid = db.insert(bare().set("message_id", "<bare-2@remote.test>")).await?.one()?.get("mail_id")?;
    assert_ne!(second, id);

    // A column with neither a value nor a default is refused by name.
    let missing = db.insert(bare()).await.unwrap_err();
    assert_eq!(missing.code, ErrorCode::NotNullViolation);
    assert!(missing.violates("message_id"));
    Ok(())
}

/// Writes a marker, replacing the one that is there.
fn set_marker(key: &str, value: &str) -> Insert {
    Insert::into("schema_meta").set("key", key).set("value", value).on_conflict_update(PRIMARY).update_excluded(["value"])
}

async fn marker(db: &impl Db, key: &str) -> DbResult<Option<String>> {
    match db.query_opt(Query::table("schema_meta").key([key]).columns(["value"])).await? {
        Some(row) => Ok(Some(row.get("value")?)),
        None => Ok(None),
    }
}

// R-S1, R-W4, R-Q1: the stored hash that lets a boot skip all schema work.
#[tokio::test(flavor = "multi_thread")]
async fn the_schema_marker_is_read_and_replaced() -> DbResult<()> {
    let (_server, db) = fresh().await;
    // On a database that was never booted the table is not there. Any
    // error from this read is taken to mean "not at the target schema".
    let never_booted = marker(&db, "schema_hash").await.unwrap_err();
    assert_eq!(never_booted.code, ErrorCode::NotFound);

    schema(&db).await?;
    assert_eq!(marker(&db, "schema_hash").await?, None);

    assert_eq!(db.insert(set_marker("schema_hash", "0f3a9c1e5b7d2468")).await?.affected(), 1);
    assert_eq!(marker(&db, "schema_hash").await?.as_deref(), Some("0f3a9c1e5b7d2468"));
    // The next release's boot overwrites it.
    assert_eq!(db.insert(set_marker("schema_hash", "9d8c7b6a5f4e3210")).await?.affected(), 1);
    db.insert(set_marker("maintenance_hash", "77aa77aa77aa77aa")).await?;
    assert_eq!(marker(&db, "schema_hash").await?.as_deref(), Some("9d8c7b6a5f4e3210"));
    assert_eq!(db.query(Query::table("schema_meta")).await?.len(), 2);
    Ok(())
}

async fn has_column(db: &impl Db, table: &str, column: &str) -> DbResult<bool> {
    Ok(db.describe_table(table).await?.is_some_and(|t| t.column_index(column).is_some()))
}

/// The boot migration, alone on the database under a named lock: create
/// what is absent, then add each column the model has and the live table
/// lacks. Additive only; a column of another type stops the boot and
/// nothing of the attempt remains. Answers the columns it added.
async fn migrate(db: &Client, model: Vec<TableDef>, hash: &str) -> DbResult<Vec<String>> {
    let guard = db.lock("app.migrate").await?;
    let hash = hash.to_string();
    let outcome = db
        .transaction(|tx| async move {
            for table in &model {
                tx.create_table_if_not_exists(table.clone()).await?;
            }
            let live = tx.describe().await?;
            let mut added = Vec::new();
            for want in &model {
                let have = live.iter().find(|t| t.name == want.name).ok_or_else(|| DbError::not_found(format!("{} was just created", want.name)))?;
                for column in &want.columns {
                    match have.columns.iter().find(|c| c.name == column.name) {
                        None => {
                            tx.add_column(&want.name, column.clone()).await?;
                            added.push(format!("{}.{}", want.name, column.name));
                        }
                        Some(found) if found.ty != column.ty => {
                            let (is, wanted) = (found.ty.name(), column.ty.name());
                            return Err(DbError::invalid(format!("{}.{} is {is} in the database and {wanted} in the model", want.name, column.name)));
                        }
                        Some(_) => {}
                    }
                }
            }
            tx.insert(set_marker("schema_hash", &hash)).await?;
            Ok(added)
        })
        .await;
    guard.unlock().await?;
    outcome
}

// R-S1, R-S8, R-X1, R-X2, R-X5: the boot migration compares the model with the live schema and adds what is missing, in one transaction under a named lock.
#[tokio::test(flavor = "multi_thread")]
async fn a_migration_adds_missing_columns_in_a_transaction_under_a_named_lock() -> DbResult<()> {
    let (_server, db) = fresh().await;
    // A database made by an older release: sessions without the three
    // single-sign-on columns, mailboxes without `subscribed` and `special_use`.
    let mut old_sessions = sessions_table();
    old_sessions.columns.retain(|c| !c.name.starts_with("auth_"));
    old_sessions.indexes.retain(|i| i.name != "sessions_auth_sid");
    let mut old_mailboxes = mailboxes_table();
    old_mailboxes.columns.retain(|c| c.name != "subscribed" && c.name != "special_use");
    for table in [users_table(), old_sessions, old_mailboxes] {
        db.create_table(table).await?;
    }
    let ann = user(&db, "ann").await?;
    db.insert(
        Insert::into("sessions")
            .set("session_id", "sid-old")
            .set("session_user_id", ann)
            .set("session_username", "ann")
            .set("session_email", "ann@example.com"),
    )
    .await?;
    let archive: Uuid =
        db.insert(Insert::into("mailboxes").set("user_id", ann).set("name", "Archive").returning(["mailbox_id"])).await?.one()?.get("mailbox_id")?;

    let added = migrate(&db, tables(), "hash-new").await?;
    assert_eq!(
        added,
        ["sessions.auth_sid", "sessions.auth_refresh_token", "sessions.auth_recheck_at", "mailboxes.subscribed", "mailboxes.special_use"]
    );
    assert_eq!(marker(&db, "schema_hash").await?.as_deref(), Some("hash-new"));
    // The tables the old release did not know were created on the way.
    assert!(db.describe_table("mail_mailbox_uid").await?.is_some());

    // Rows written before the migration read the new columns' defaults.
    let session = db.get("sessions", ["sid-old"]).await?.expect("kept");
    assert!(session.is_null("auth_sid")? && session.is_null("auth_recheck_at")?);
    let mailbox = db.get("mailboxes", [archive]).await?.expect("kept");
    assert!(mailbox.get::<bool>("subscribed")?);
    assert!(mailbox.is_null("special_use")?);
    // And the new columns are written like any other.
    db.update(Update::table("sessions").key(["sid-old"]).set("auth_sid", "sso-1")).await?;
    db.create_index("sessions", sessions_table().find_index("sessions_auth_sid").expect("in the model").clone()).await?;
    let by_sid = db.query(Query::table("sessions").index("sessions_auth_sid", KeyRange::eq(["sso-1"]))).await?;
    assert_eq!(column::<String>(&by_sid, "session_id"), ["sid-old"]);

    // A boot on a database already at the model adds nothing.
    assert!(migrate(&db, tables(), "hash-new").await?.is_empty());

    // A model that disagrees with the database about a type fails the boot,
    // and the column added earlier in the same transaction goes with it.
    let mut wrong = tables();
    for table in &mut wrong {
        if table.name == "sessions" {
            table.columns.push(ColumnDef::new("session_note", Type::Text).nullable());
        }
        if table.name == "mails" {
            table.columns.iter_mut().find(|c| c.name == "uid_domain").expect("in the model").ty = Type::Text;
        }
    }
    let refused = migrate(&db, wrong, "hash-wrong").await.unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidRequest);
    assert!(refused.message.contains("mails.uid_domain"), "{refused}");
    assert!(!has_column(&db, "sessions", "session_note").await?);
    assert_eq!(marker(&db, "schema_hash").await?.as_deref(), Some("hash-new"));

    // Until a migration commits, only its own transaction sees its columns.
    let other = second_instance(&db).await?;
    let tx = db.begin().await?;
    tx.add_column("sessions", ColumnDef::new("session_note", Type::Text).nullable()).await?;
    tx.update(Update::table("sessions").key(["sid-old"]).set("session_note", "seen inside")).await?;
    assert!(has_column(&tx, "sessions", "session_note").await?);
    assert!(!has_column(&other, "sessions", "session_note").await?);
    tx.commit().await?;
    let session = other.get("sessions", ["sid-old"]).await?.expect("kept");
    assert_eq!(session.get::<String>("session_note")?, "seen inside");
    Ok(())
}

// R-X5: two instances start at once during a rolling deploy.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_instance_skips_maintenance_and_queues_for_migration() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let other = second_instance(&db).await?;

    // Boot maintenance only tries the lock: whoever gets it does the work.
    let running = db.try_lock("app.maintenance").await?.expect("nobody holds it yet");
    assert_eq!(running.name(), "app.maintenance");
    assert!(other.try_lock("app.maintenance").await?.is_none());
    assert!(db.try_lock("app.maintenance").await?.is_none(), "not even the holder's own process gets it twice");
    // The lock stops nothing but another holder: both instances keep
    // reading and writing, and a lock of another name is free.
    db.insert(set_marker("maintenance_hash", "77aa77aa77aa77aa")).await?;
    assert_eq!(marker(&other, "maintenance_hash").await?.as_deref(), Some("77aa77aa77aa77aa"));
    other.try_lock("app.migrate").await?.expect("another name is another lock").unlock().await?;
    running.unlock().await?;

    // A guard that is dropped, as when its process dies, frees the lock too.
    let abandoned = other.try_lock("app.maintenance").await?.expect("free again after unlock");
    drop(abandoned);
    let next = tokio::time::timeout(Duration::from_secs(10), db.lock("app.maintenance")).await.expect("a dropped guard's lock is freed")?;
    next.unlock().await?;

    // Migration waits its turn instead of skipping.
    let order = Arc::new(Mutex::new(Vec::new()));
    let first = db.lock("app.migrate").await?;
    let waiter = tokio::spawn({
        let order = order.clone();
        async move {
            let second = other.lock("app.migrate").await?;
            order.lock().unwrap().push("second instance migrates");
            second.unlock().await
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    order.lock().unwrap().push("first instance done");
    first.unlock().await?;
    waiter.await.expect("the waiting instance finishes")?;
    assert_eq!(*order.lock().unwrap(), ["first instance done", "second instance migrates"]);
    Ok(())
}

// R-S11, R-S3, R-Q17: boot maintenance drops and builds indexes on a table that already holds rows.
#[tokio::test(flavor = "multi_thread")]
async fn maintenance_builds_indexes_over_rows_that_are_already_there() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    // A database from before two of the indexes existed. Dropping one
    // that is not there is fine: the drop runs on every retry.
    db.drop_index("mails", MAILS_SEARCH).await?;
    db.drop_index("mails", MAILS_REPLY).await?;
    db.drop_index_if_exists("mails", MAILS_REPLY).await?;
    let gone = db.drop_index("mails", MAILS_REPLY).await.unwrap_err();
    assert_eq!(gone.code, ErrorCode::NotFound);

    let mut reply = Mail { in_reply_to: Some("<m1@remote.test>".into()), subject: "Re: Quarterly invoices".into(), ..mail(ann, 2) };
    save_all(&db, &[Mail { subject: "Quarterly invoices".into(), ..mail(ann, 1) }, reply.clone(), mail(ann, 3)]).await?;
    // Legacy data: two mails with the same domain UID.
    reply.mail_id = uuid(4);
    reply.message_id = "<m4@remote.test>".into();
    save(&db, &reply).await?;

    // An index that does not exist cannot be read through; it is not an empty answer.
    let replies = || Query::table("mails").index(MAILS_REPLY, KeyRange::eq([val(ann), val("<m1@remote.test>")])).columns(["mail_id"]);
    assert_eq!(db.query(replies()).await.unwrap_err().code, ErrorCode::NotFound);

    // The maintenance session is a connection of its own with a ten
    // minute statement limit instead of the pool's thirty seconds.
    let mut config = db.config().clone();
    config.pool_size = 1;
    config.statement_timeout = Duration::from_secs(600);
    let maintenance = Client::connect(config).await?;
    let model = mails_table();
    for name in [MAILS_SEARCH, MAILS_REPLY] {
        let index = model.find_index(name).expect("in the model").clone();
        maintenance.create_index_if_not_exists("mails", index.clone()).await?;
        // The next boot's attempt finds it there.
        maintenance.create_index_if_not_exists("mails", index.clone()).await?;
        assert_eq!(maintenance.create_index("mails", index).await.unwrap_err().code, ErrorCode::AlreadyExists);
    }

    // The new indexes hold the rows written before them: no rewrite of
    // the table is needed for search to find old mail.
    assert_eq!(db.query(replies()).await?.column::<Uuid>()?.into_iter().collect::<BTreeSet<_>>(), BTreeSet::from([uuid(2), uuid(4)]));
    let found = db.query(Query::table("mails").search(MAILS_SEARCH, [ann], "invoice").columns(["mail_id"])).await?;
    assert_eq!(found.column::<Uuid>()?.into_iter().collect::<BTreeSet<_>>(), BTreeSet::from([uuid(1), uuid(2), uuid(4)]));

    // A unique index over rows that hold duplicates is refused, and
    // nothing half-built is left behind to sweep up.
    let unique = IndexDef::new("mails_uid_unique").on(["user_id", "sent", "uid_domain"]).unique();
    let refused = maintenance.create_index("mails", unique.clone()).await.unwrap_err();
    assert_eq!(refused.code, ErrorCode::UniqueViolation);
    assert!(db.describe_table("mails").await?.expect("mails").find_index("mails_uid_unique").is_none());
    // Once the duplicate is gone the same index builds, and then holds.
    db.delete(Delete::from("mails").key([uuid(4)])).await?;
    maintenance.create_index("mails", unique).await?;
    let clash = save(&db, &Mail { uid_domain: 3, ..mail(ann, 5) }).await.unwrap_err();
    assert!(clash.is_unique_violation() && clash.violates("mails_uid_unique"));
    Ok(())
}

const USER_COLUMNS: [&str; 11] =
    ["user_id", "username", "password", "password_version", "email", "expiry", "token", "updated", "is_deleted", "imap_uid_validity", "auth_sub"];

/// The usernames a query over `users` finds.
async fn usernames(db: &Client, query: Query) -> DbResult<Vec<String>> {
    Ok(column(&db.query(query).await?, "username"))
}

// R-Q1, R-Q2, R-Q3, R-T9: the user lookup, run for every HTTP request, IMAP command and SMTP command.
#[tokio::test(flavor = "multi_thread")]
async fn a_user_is_found_by_id_name_email_or_subject_unless_deleted() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let insert =
        |name: &str, email: &str| Insert::into("users").set("updated", now()).set("username", name).set("password", "hash").set("email", email);
    let ann = db.insert(insert("ann", "Ann@Example.com").set("auth_sub", "oidc|ann").returning(USER_COLUMNS)).await?.one()?;
    let ann_id: Uuid = ann.get("user_id")?;
    assert_eq!(ann.get::<Option<i64>>("password_version")?, Some(1));
    assert_eq!(ann.get::<Option<bool>>("is_deleted")?, Some(false));
    assert!(ann.is_null("imap_uid_validity")? && ann.is_null("token")? && ann.is_null("expiry")?);
    // A row from before the flag had a default holds NULL; a deleted one TRUE.
    db.insert(insert("old", "old@example.com").set("is_deleted", null())).await?;
    let gone: Uuid = db.insert(insert("gone", "gone@example.com").set("is_deleted", true).returning(["user_id"])).await?.one()?.get("user_id")?;

    // By primary key, with the LIMIT the statement carries.
    assert_eq!(usernames(&db, Query::table("users").key([ann_id]).filter(not_deleted()).limit(1)).await?, ["ann"]);
    // By username and by OIDC subject: one row through a unique index.
    let by_name = |name: &str| Query::table("users").index("users_username_key", KeyRange::eq([name])).filter(not_deleted()).limit(1);
    assert_eq!(usernames(&db, by_name("ann")).await?, ["ann"]);
    assert!(usernames(&db, by_name("Ann")).await?.is_empty(), "usernames compare by their bytes");
    let by_subject = Query::table("users").index("users_auth_sub_key", KeyRange::eq(["oidc|ann"])).filter(not_deleted()).limit(1);
    assert_eq!(usernames(&db, by_subject).await?, ["ann"]);
    // By email, compared exactly: the case-folding index narrows, the filter decides.
    let by_email = |email: &str| {
        Query::table("users")
            .index("users_email_lower", KeyRange::eq([email.to_lowercase()]))
            .filter(col("email").eq(email).and(not_deleted()))
            .limit(1)
    };
    assert_eq!(usernames(&db, by_email("Ann@Example.com")).await?, ["ann"]);
    assert!(usernames(&db, by_email("ann@example.com")).await?.is_empty());
    // Any AND of the three columns: the key picks the row, the rest is a filter.
    let by_all = |name: &str| {
        Query::table("users").key([ann_id]).filter(col("username").eq(name).and(col("email").eq("Ann@Example.com")).and(not_deleted())).limit(1)
    };
    assert_eq!(usernames(&db, by_all("ann")).await?, ["ann"]);
    assert!(usernames(&db, by_all("old")).await?.is_empty());

    // NULL in the flag reads as not deleted; TRUE hides the row on every path.
    assert_eq!(usernames(&db, by_name("old")).await?, ["old"]);
    assert!(usernames(&db, by_name("gone")).await?.is_empty());
    assert!(usernames(&db, Query::table("users").key([gone]).filter(not_deleted()).limit(1)).await?.is_empty());
    assert!(usernames(&db, by_email("gone@example.com")).await?.is_empty());
    Ok(())
}

/// Writes an account by id, inserting it or bringing the stored row up to
/// date. Columns the statement does not name keep what they hold.
fn seed_account(id: Uuid, columns: &[(&str, Expr)]) -> Insert {
    let mut insert = Insert::into("users").set("updated", now()).set("user_id", id);
    for (column, value) in columns {
        insert = insert.set(column, value.clone());
    }
    insert.on_conflict_update(PRIMARY).update_excluded(columns.iter().map(|(column, _)| *column)).update("updated", now()).returning(USER_COLUMNS)
}

// R-W1, R-W4, R-W5, R-W11, R-S3: an account seeded by upsert, a pending sign-up, and a first single-sign-on login.
#[tokio::test(flavor = "multi_thread")]
async fn accounts_are_seeded_by_upsert_and_created_unless_taken() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;

    // At first boot the upsert inserts the admin account.
    let admin = uuid(0xad);
    let first = db
        .insert(seed_account(admin, &[("username", val("admin")), ("password", val("hash-1")), ("email", val("admin@example.com"))]))
        .await?
        .one()?;
    assert_eq!(first.get::<Option<i64>>("password_version")?, Some(1));
    let stale: Timestamp =
        db.update(Update::table("users").key([admin]).set("updated", now() - hours(1)).returning(["updated"])).await?.one()?.get("updated")?;
    // At a later boot the same statement repairs the row in place.
    let repaired = db.insert(seed_account(admin, &[("username", val("admin")), ("password", val("hash-2")), ("password_version", val(3))])).await?;
    assert_eq!(repaired.affected(), 1);
    let repaired = repaired.one()?;
    assert_eq!(repaired.get::<Uuid>("user_id")?, admin);
    assert_eq!((repaired.get::<String>("password")?, repaired.get::<i64>("password_version")?), ("hash-2".to_string(), 3));
    assert_eq!(repaired.get::<String>("email")?, "admin@example.com", "a column the statement left out is kept");
    assert!(repaired.get::<Timestamp>("updated")? > stale);
    assert_eq!(db.query(Query::table("users")).await?.len(), 1);
    // The upsert's target is the primary key only: another id with a taken
    // username is an error that names the index.
    let taken = db.insert(seed_account(uuid(0xae), &[("username", val("admin")), ("password", val("hash-3"))])).await.unwrap_err();
    assert_eq!(taken.code, ErrorCode::UniqueViolation);
    assert!(taken.violates("users_username_key"));

    // A pending sign-up, with an id the application made and no password yet.
    let pending = uuid(0x51);
    let sign_up = || {
        Insert::into("users")
            .set("updated", now())
            .set("user_id", pending)
            .set("username", "user_1a2b3c4d")
            .set("email", "new@example.com")
            .set("token", "sign-up-token")
            .set("expiry", at(3600))
            .returning(["user_id"])
    };
    assert_eq!(db.insert(sign_up()).await?.column::<Uuid>()?, [pending]);
    let row = db.get("users", [pending]).await?.expect("inserted");
    assert!(row.is_null("password")?);
    assert_eq!(row.get::<Timestamp>("expiry")?, at(3600));
    let twice = db.insert(sign_up()).await.unwrap_err();
    assert!(twice.is_unique_violation() && twice.violates(PRIMARY));

    // A first single-sign-on login creates the user unless any unique
    // index objects. No row back means "someone has that name or subject".
    let sso = |name: &str, subject: &str| {
        Insert::into("users")
            .set("updated", now())
            .set("username", name)
            .set("password", "!")
            .set("email", format!("{name}@example.com"))
            .set("auth_sub", subject)
            .on_conflict_do_nothing()
            .returning(USER_COLUMNS)
    };
    let created = db.insert(sso("carol", "oidc|carol")).await?;
    assert_eq!(created.affected(), 1);
    let carol: Uuid = created.one()?.get("user_id")?;
    for (name, subject) in [("carol", "oidc|someone-else"), ("carol-2", "oidc|carol")] {
        let skipped = db.insert(sso(name, subject)).await?;
        assert_eq!((skipped.affected(), skipped.len()), (0, 0), "{name} {subject}");
    }
    assert_eq!(usernames(&db, Query::table("users").index("users_auth_sub_key", KeyRange::eq(["oidc|carol"]))).await?, ["carol"]);
    assert!(db.query(Query::table("users").index("users_username_key", KeyRange::eq(["carol-2"]))).await?.is_empty());
    assert_ne!(carol, pending);
    Ok(())
}

// R-W6, R-W7, R-W10, R-X3: a password change, sign-out everywhere, and updates and deletes that answer whether the row was there.
#[tokio::test(flavor = "multi_thread")]
async fn credentials_rotate_atomically_and_an_update_tells_whether_the_row_exists() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    db.update(Update::table("users").key([ann]).set("token", "reset-token").set("expiry", at(3600))).await?;

    // A password change: the new password and the bumped version land
    // together, and the statement answers the version that every other
    // session now fails.
    let rotate = |id: Uuid, password: &str| {
        Update::table("users")
            .key([id])
            .set("password", password)
            .set("password_version", col("password_version").or_else(1) + 1)
            .set("username", "ann")
            .set("token", null())
            .set("expiry", null())
            .set("updated", now())
            .returning(["password_version"])
    };
    assert_eq!(db.update(rotate(ann, "hash-2")).await?.column::<i64>()?, [2]);
    let row = db.get("users", [ann]).await?.expect("ann");
    assert_eq!(row.get::<String>("password")?, "hash-2");
    assert!(row.is_null("token")? && row.is_null("expiry")?);
    // A legacy row without a version counts as version 1.
    db.update(Update::table("users").key([ann]).set("password_version", null())).await?;
    assert_eq!(db.update(rotate(ann, "hash-3")).await?.column::<i64>()?, [2]);
    assert!(db.update(rotate(uuid(404), "hash-x")).await?.is_empty());

    // Sign-out everywhere, addressed by OIDC subject. Eight at once each
    // add one: no increment is lost.
    db.update(Update::table("users").key([ann]).set("auth_sub", "oidc|ann")).await?;
    let bumps: Vec<_> = (0..8)
        .map(|_| {
            let db = db.clone();
            tokio::spawn(async move {
                let bump = Update::table("users")
                    .index("users_auth_sub_key", KeyRange::eq(["oidc|ann"]))
                    .set("password_version", col("password_version").or_else(1) + 1)
                    .set("updated", now())
                    .returning(["user_id", "username", "password_version"]);
                db.update(bump).await?.one()?.get::<i64>("password_version")
            })
        })
        .collect();
    let mut versions = Vec::new();
    for bump in bumps {
        versions.push(bump.await.expect("the task finishes")?);
    }
    versions.sort();
    assert_eq!(versions, (3..=10).collect::<Vec<i64>>());

    // An update by id returns the id, and that is how the application
    // learns the row exists. Token and expiry are set, and cleared, together.
    let touch = |id: Uuid, token: Option<&str>, expiry: Option<Timestamp>| {
        Update::table("users").key([id]).set("updated", now()).set("token", token).set("expiry", expiry).returning(["user_id"])
    };
    assert_eq!(db.update(touch(ann, Some("t-2"), Some(at(60)))).await?.column::<Uuid>()?, [ann]);
    assert_eq!(db.get("users", [ann]).await?.expect("ann").get::<Option<String>>("token")?.as_deref(), Some("t-2"));
    assert_eq!(db.update(touch(ann, None, None)).await?.len(), 1);
    assert!(db.get("users", [ann]).await?.expect("ann").is_null("expiry")?);
    let nobody = db.update(touch(uuid(404), Some("t"), Some(at(60)))).await?;
    assert_eq!((nobody.affected(), nobody.len()), (0, 0));
    // UIDVALIDITY is set on first IMAP use: a read, then a write.
    assert!(db.get("users", [ann]).await?.expect("ann").is_null("imap_uid_validity")?);
    db.update(Update::table("users").key([ann]).set("updated", now()).set("imap_uid_validity", 1_794_711_304_i64).returning(["user_id"])).await?;
    assert_eq!(db.get("users", [ann]).await?.expect("ann").get::<i64>("imap_uid_validity")?, 1_794_711_304);

    // The delete that reclaims an expired sign-up. The id comes back once;
    // a second delete finds nothing.
    let reclaim = || Delete::from("users").key([ann]).returning(["user_id"]);
    assert_eq!(db.delete(reclaim()).await?.column::<Uuid>()?, [ann]);
    let again = db.delete(reclaim()).await?;
    assert_eq!((again.affected(), again.len()), (0, 0));
    Ok(())
}

// R-Q3, R-Q6, R-W7, R-W11, R-T9, R-S3: linking an OIDC login to an existing account.
#[tokio::test(flavor = "multi_thread")]
async fn a_login_is_linked_to_the_one_account_with_that_email_and_only_once() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let account =
        |name: &str, email: &str| Insert::into("users").set("updated", now()).set("username", name).set("email", email).returning(["user_id"]);
    let id = |rows: Rows| rows.one()?.get::<Uuid>("user_id");
    let ann = id(db.insert(account("ann", "Ann@Example.com").set("password", "hash")).await?)?;
    // The same email on accounts that must not be offered: the reserved
    // names, a sign-up without a password, one already linked, one deleted.
    db.insert(account("admin", "ann@example.com").set("password", "hash")).await?;
    db.insert(account("user_0badc0de", "ann@example.com")).await?;
    db.insert(account("ann-sso", "ann@example.com").set("password", "hash").set("auth_sub", "oidc|taken")).await?;
    db.insert(account("ann-gone", "ann@example.com").set("password", "hash").set("is_deleted", true)).await?;
    let legacy = id(db.insert(account("legacy", "legacy@example.com").set("password", "hash").set("is_deleted", null())).await?)?;
    let bob = id(db.insert(account("bob", "bob@example.com").set("password", "hash")).await?)?;

    // The accounts a login may be linked to. LIMIT 2 is how the application
    // tells "exactly one" from "ambiguous".
    let candidates = |email: &str| {
        Query::table("users")
            .index("users_email_lower", KeyRange::eq([email.to_lowercase()]))
            .filter(all([
                col("auth_sub").is_null(),
                col("password").is_not_null(),
                col("username").not_in_list(vec!["admin", "admin-ro"]),
                not_deleted(),
            ]))
            .limit(2)
    };
    assert_eq!(usernames(&db, candidates("ANN@example.COM")).await?, ["ann"]);
    assert!(usernames(&db, candidates("nobody@example.com")).await?.is_empty());
    db.insert(account("ann-too", "ANN@EXAMPLE.COM").set("password", "hash")).await?;
    assert_eq!(usernames(&db, candidates("ann@example.com")).await?.len(), 2);

    // Link if still unlinked. The row comes back to whoever linked it.
    let link = |user: Uuid, subject: &str| {
        Update::table("users")
            .key([user])
            .filter(col("auth_sub").is_null().and(col("is_deleted").is_not_true()))
            .set("auth_sub", subject)
            .set("updated", now())
            .returning(USER_COLUMNS)
    };
    let linked = db.update(link(ann, "oidc|ann")).await?.one()?;
    assert_eq!(linked.get::<Option<String>>("auth_sub")?.as_deref(), Some("oidc|ann"));
    // A second login racing for the same account finds it taken: no row.
    let late = db.update(link(ann, "oidc|other")).await?;
    assert_eq!((late.affected(), late.len()), (0, 0));
    assert_eq!(db.get("users", [ann]).await?.expect("ann").get::<String>("auth_sub")?, "oidc|ann");
    // The same subject on a second account is refused by the unique index,
    // which the application reads as "someone else linked first".
    let stolen = db.update(link(bob, "oidc|ann")).await.unwrap_err();
    assert_eq!(stolen.code, ErrorCode::UniqueViolation);
    assert!(stolen.violates("users_auth_sub_key"));
    assert!(db.get("users", [bob]).await?.expect("bob").is_null("auth_sub")?);

    // `is_deleted IS NOT TRUE` lets a NULL flag through and stops TRUE.
    assert_eq!(db.update(link(legacy, "oidc|legacy")).await?.len(), 1);
    db.update(Update::table("users").key([bob]).set("is_deleted", true)).await?;
    assert!(db.update(link(bob, "oidc|bob")).await?.is_empty());
    // Any number of users have no subject at all: NULL is not a duplicate.
    let unlinked = db.query(Query::table("users").filter(col("auth_sub").is_null())).await?;
    assert_eq!(unlinked.len(), 5);
    Ok(())
}

/// The session store's write. `optional` are the nullable columns this
/// request carries; on a conflict the ones it leaves out keep their values.
fn put_session(id: &str, user: Uuid, max_age: i64, optional: &[(&str, Expr)]) -> Insert {
    let mut insert = Insert::into("sessions")
        .set("updated", now())
        .set("session_id", id)
        .set("session_user_id", user)
        .set("session_username", "ann")
        .set("session_email", "ann@example.com")
        .set("session_is_read_only", false)
        .set("session_password_version", 1)
        .set("cookie_original_max_age", max_age);
    for (column, value) in optional {
        insert = insert.set(column, value.clone());
    }
    let written: Vec<String> = insert.columns.iter().filter(|c| *c != "session_id" && *c != "updated").cloned().collect();
    insert.on_conflict_update(PRIMARY).update_excluded(written).update("updated", now()).returning(all_columns(&sessions_table()))
}

// R-W4, R-Q1, R-X4: the session is read and written on every HTTP request.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_is_upserted_on_every_request_and_keeps_what_a_request_leaves_out() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    let other = second_instance(&db).await?;

    // Sign-in writes the whole cookie. The two JSON-in-text columns hold
    // JSON text, quotes and all.
    let cookie = [
        ("cookie_expires", val(at(86_400))),
        ("cookie_http_only", val(true)),
        ("cookie_path", val("/")),
        ("cookie_secure", val("\"auto\"")),
        ("cookie_same_site", val("\"strict\"")),
    ];
    let created = db.insert(put_session("sid-1", ann, 86_400_000, &cookie)).await?.one()?;
    assert_eq!(created.get::<String>("cookie_secure")?, "\"auto\"");
    assert!(created.is_null("auth_sid")? && created.is_null("cookie_domain")?);
    // The session read, from another connection: what was returned is what is stored.
    let read = other.query_opt(Query::table("sessions").key(["sid-1"]).limit(1)).await?.expect("visible as soon as the insert returned");
    assert_eq!(read, created);
    assert!(other.query_opt(Query::table("sessions").key(["sid-2"]).limit(1)).await?.is_none());

    let stale: Timestamp =
        db.update(Update::table("sessions").key(["sid-1"]).set("updated", now() - hours(1)).returning(["updated"])).await?.one()?.get("updated")?;
    // Every later request rolls the expiry forward and nothing else.
    for request in 1..=5 {
        let rolled = db.insert(put_session("sid-1", ann, 86_400_000 + request, &[("cookie_expires", val(at(86_400 + request)))])).await?;
        assert_eq!(rolled.affected(), 1);
        let rolled = rolled.one()?;
        assert_eq!(rolled.get::<i64>("cookie_original_max_age")?, 86_400_000 + request);
        assert_eq!(rolled.get::<Timestamp>("cookie_expires")?, at(86_400 + request));
        assert_eq!(rolled.get::<String>("cookie_path")?, "/");
        assert_eq!(rolled.get::<Option<bool>>("cookie_http_only")?, Some(true));
        assert_eq!(rolled.get::<String>("cookie_same_site")?, "\"strict\"");
        assert!(rolled.get::<Timestamp>("updated")? > stale);
        // The next request may arrive at the other instance.
        assert_eq!(other.get("sessions", ["sid-1"]).await?.expect("still one session"), rolled);
    }
    assert_eq!(db.query(Query::table("sessions")).await?.len(), 1);
    Ok(())
}

// R-W10, R-W7, R-Q4, R-S4: sessions are deleted one at a time, by login, by owner and by expiry; one whose user is gone is removed when a request finds it.
#[tokio::test(flavor = "multi_thread")]
async fn sessions_are_deleted_by_id_owner_login_and_expiry() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob, carol) = (user(&db, "ann").await?, user(&db, "bob").await?, user(&db, "carol").await?);
    let expires = |when: Timestamp| [("cookie_expires", val(when))];
    db.insert(put_session("expired", ann, 1, &expires(at(-10)))).await?;
    db.insert(put_session("expiring-now", ann, 1, &expires(at(0)))).await?;
    db.insert(put_session("a-microsecond-left", ann, 1, &expires(Timestamp(at(0).0 + 1)))).await?;
    db.insert(put_session("browser-session", ann, 1, &[])).await?;
    db.insert(put_session("bob-1", bob, 1, &[("auth_sid", val("sso-7")), ("cookie_expires", val(at(900)))])).await?;
    db.insert(put_session("bob-2", bob, 1, &[("session_authenticated_as", val("admin"))])).await?;
    db.insert(put_session("carol-1", carol, 1, &[("auth_sid", val("sso-7")), ("session_authenticated_as", val("admin"))])).await?;
    db.insert(put_session("carol-2", carol, 1, &[("auth_sid", val("sso-8"))])).await?;
    let remaining = |rows: Rows| column::<String>(&rows, "session_id").into_iter().collect::<BTreeSet<_>>();
    let ids = |list: &[&str]| list.iter().map(|id| id.to_string()).collect::<BTreeSet<_>>();

    // The hourly purge: everything that expires at or before the
    // application's clock, which arrives as ISO text. A session with no
    // expiry is never purged.
    let purge = Delete::from("sessions")
        .index("sessions_cookie_expires", KeyRange::new(Bound::Unbounded, Bound::included([at(0).to_rfc3339()])))
        .returning(["session_id"]);
    let purged = db.delete(purge).await?;
    assert_eq!(purged.affected(), 2);
    assert_eq!(remaining(purged), ids(&["expired", "expiring-now"]));

    // Sign-out.
    let sign_out = |id: &str| Delete::from("sessions").key([id]).returning(["session_id"]);
    assert_eq!(db.delete(sign_out("a-microsecond-left")).await?.len(), 1);
    assert_eq!(db.delete(sign_out("a-microsecond-left")).await?.len(), 0);

    // Every session of one single sign-on session, of one impersonating
    // admin, of one user. The application uses the count.
    let back_channel_logout = Delete::from("sessions").index("sessions_auth_sid", KeyRange::eq(["sso-7"])).returning(["session_id"]);
    assert_eq!(remaining(db.delete(back_channel_logout).await?), ids(&["bob-1", "carol-1"]));
    let impersonated = Delete::from("sessions").filter(col("session_authenticated_as").eq("admin")).returning(["session_id"]);
    assert_eq!(remaining(db.delete(impersonated).await?), ids(&["bob-2"]));
    let of_user = |user: Uuid| Delete::from("sessions").index("sessions_user", KeyRange::eq([user])).returning(["session_id"]);
    assert_eq!(db.delete(of_user(bob)).await?.affected(), 0);
    assert_eq!(remaining(db.delete(of_user(carol)).await?), ids(&["carol-2"]));
    assert_eq!(remaining(db.query(Query::table("sessions")).await?), ids(&["browser-session"]));

    // A request with a session whose user is gone: the session is found,
    // its user is not, and the session is removed. Nothing ties the two
    // tables together, so deleting the user left the session behind.
    db.delete(Delete::from("users").key([ann])).await?;
    let session = db.query_opt(Query::table("sessions").key(["browser-session"]).limit(1)).await?.expect("no foreign key removed it");
    let owner: Uuid = session.get("session_user_id")?;
    assert!(db.query_opt(Query::table("users").key([owner]).filter(not_deleted()).limit(1)).await?.is_none());
    assert_eq!(db.delete(sign_out("browser-session")).await?.affected(), 1);
    Ok(())
}

// R-Q3, R-Q11, R-W1, R-W6, R-W10, R-S6: IMAP CREATE, RENAME, DELETE and SUBSCRIBE on a user's mailboxes.
#[tokio::test(flavor = "multi_thread")]
async fn mailboxes_are_probed_by_name_without_case_then_written_by_id() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob) = (user(&db, "ann").await?, user(&db, "bob").await?);
    let named =
        |user: Uuid, name: &str| Query::table("mailboxes").index("mailboxes_user_name", KeyRange::eq([val(user), val(name.to_lowercase())])).limit(1);
    let count_of = |user: Uuid| Query::table("mailboxes").index("mailboxes_user_name", KeyRange::eq([user])).select("count", count());
    let create = |user: Uuid, name: &str, parent: Option<Uuid>| {
        Insert::into("mailboxes")
            .set("user_id", user)
            .set("name", name)
            .set("address", format!("{}@example.com", name.to_lowercase()))
            .set("parent_id", parent)
            .set("special_use", null())
            .set("subscribed", true)
            .set("uid_validity", 1)
            .set("uid_next", 1)
            .returning(all_columns(&mailboxes_table()))
    };

    // CREATE: probe the name, count for the per-user cap, insert.
    assert!(db.query_opt(named(ann, "Archive")).await?.is_none());
    assert_eq!(db.query_one(count_of(ann)).await?.get::<i64>("count")?, 0);
    let before = db.now().await?;
    let archive = db.insert(create(ann, "Archive", None)).await?.one()?;
    let archive_id: Uuid = archive.get("mailbox_id")?;
    assert_eq!((archive.get::<i64>("uid_validity")?, archive.get::<i64>("uid_next")?, archive.get::<bool>("subscribed")?), (1, 1, true));
    assert!(archive.get::<Timestamp>("created")? >= before);
    assert!(archive.is_null("parent_id")? && archive.is_null("special_use")?);
    for name in ["Projects", "receipts"] {
        db.insert(create(ann, name, None)).await?;
    }
    db.insert(create(bob, "Archive", None)).await?;
    let child: Uuid = db.insert(create(ann, "Archive 2019", Some(archive_id))).await?.one()?.get("mailbox_id")?;

    // The probe again, as every later command issues it: any case finds it, per user.
    for spelling in ["Archive", "ARCHIVE", "archive"] {
        let found = db.query_opt(named(ann, spelling)).await?.expect("found whatever the case");
        assert_eq!(found.get::<Uuid>("mailbox_id")?, archive_id);
        assert_eq!(found.get::<String>("name")?, "Archive");
    }
    // The user's folders, listed and counted. The application sorts them itself.
    let listed = db.query(Query::table("mailboxes").index("mailboxes_user_name", KeyRange::eq([ann]))).await?;
    assert_eq!(
        column::<String>(&listed, "name").into_iter().collect::<BTreeSet<_>>(),
        BTreeSet::from(["Archive", "Archive 2019", "Projects", "receipts"].map(String::from))
    );
    assert_eq!(db.query_one(count_of(ann)).await?.get::<i64>("count")?, 4);
    assert_eq!(db.query_one(count_of(bob)).await?.get::<i64>("count")?, 1);
    // A folder found by its delivery address.
    let by_address =
        Query::table("mailboxes").index("mailboxes_user_name", KeyRange::eq([ann])).filter(col("address").eq("projects@example.com")).limit(1);
    assert_eq!(db.query_one(by_address).await?.get::<String>("name")?, "Projects");

    // A mail filed under the folder's name, to see what renaming does to it.
    save(&db, &mail(ann, 1)).await?;
    mapped(&db, ann, "Archive", uuid(1), 1).await?;

    // RENAME: probe the old name, probe the new one, then write the name
    // and the validity the application computed from the row it read.
    let validity: i64 = db.query_opt(named(ann, "archive")).await?.expect("exists").get("uid_validity")?;
    assert!(db.query_opt(named(ann, "Old Mail")).await?.is_none());
    let renamed = db
        .update(Update::table("mailboxes").key([archive_id]).set("name", "Old Mail").set("uid_validity", validity + 1).returning(["mailbox_id"]))
        .await?;
    assert_eq!(renamed.column::<Uuid>()?, [archive_id]);
    assert!(db.query_opt(named(ann, "Archive")).await?.is_none(), "the name index follows the update");
    assert_eq!(db.query_opt(named(ann, "OLD MAIL")).await?.expect("under its new name").get::<i64>("uid_validity")?, 2);
    assert!(db.query_opt(named(bob, "Archive")).await?.is_some());

    // SUBSCRIBE and UNSUBSCRIBE.
    let unsubscribed = db.update(Update::table("mailboxes").key([archive_id]).set("subscribed", false).returning(["mailbox_id"])).await?;
    assert_eq!(unsubscribed.len(), 1);
    assert!(!db.get("mailboxes", [archive_id]).await?.expect("exists").get::<bool>("subscribed")?);
    assert!(db.update(Update::table("mailboxes").key([uuid(404)]).set("subscribed", false).returning(["mailbox_id"])).await?.is_empty());

    // Nothing but the probe keeps names apart: two CREATEs that both
    // probed before either inserted leave two rows.
    db.insert(create(ann, "PROJECTS", None)).await?;
    assert_eq!(db.query(Query::table("mailboxes").index("mailboxes_user_name", KeyRange::eq([val(ann), val("projects")]))).await?.len(), 2);

    // DELETE takes the folders under it, and only touches this table: the
    // mapping rows filed under the old name are still there.
    let deleted = db.delete(Delete::from("mailboxes").key([archive_id]).returning(["mailbox_id"])).await?;
    assert_eq!(deleted.column::<Uuid>()?, [archive_id]);
    assert!(db.get("mailboxes", [child]).await?.is_none());
    assert!(db.delete(Delete::from("mailboxes").key([archive_id]).returning(["mailbox_id"])).await?.is_empty());
    assert_eq!(db.query_one(count_of(ann)).await?.get::<i64>("count")?, 3);
    assert!(db.get("mail_mailbox_uid", [val(ann), val("Archive"), val(uuid(1))]).await?.is_some());
    Ok(())
}

// R-W1, R-W6, R-W7, R-W10, R-Q6: web push subscriptions are stored, read for several users at once, refreshed, stamped and swept.
#[tokio::test(flavor = "multi_thread")]
async fn push_subscriptions_are_refreshed_listed_and_swept() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob, carol) = (user(&db, "ann").await?, user(&db, "bob").await?, user(&db, "carol").await?);
    let subscribe = |id: Uuid, user: Uuid| {
        Insert::into("push_subscriptions")
            .set("updated", now())
            .set("push_subscription_id", id)
            .set("user_id", user)
            .set("endpoint", format!("https://push.example/{id}"))
            .set("keys_p256dh", "BOr3-p256")
            .set("keys_auth", "k9-auth")
            .set("last_notified", null())
            .returning(["push_subscription_id"])
    };
    for (n, owner) in [(1, ann), (2, ann), (3, bob), (4, carol)] {
        assert_eq!(db.insert(subscribe(uuid(n), owner)).await?.column::<Uuid>()?, [uuid(n)]);
    }
    let ids = |rows: Rows| column::<Uuid>(&rows, "push_subscription_id").into_iter().collect::<BTreeSet<_>>();

    // The subscriptions of the users a new mail concerns.
    let of_users =
        |users: &[Uuid]| Query::table("push_subscriptions").index_ranges("push_subscriptions_user", users.iter().map(|u| KeyRange::eq([*u])));
    assert_eq!(ids(db.query(of_users(&[ann, bob])).await?), BTreeSet::from([uuid(1), uuid(2), uuid(3)]));
    assert!(db.query(of_users(&[uuid(404)])).await?.is_empty());

    let stale: Timestamp = db
        .update(Update::table("push_subscriptions").set("updated", now() - days(8)).returning(["updated"]))
        .await?
        .first()
        .expect("four rows")
        .get("updated")?;
    // A browser refreshes its own subscription, and only its own.
    let refresh = |id: Uuid, user: Uuid| {
        Update::table("push_subscriptions").key([id]).filter(col("user_id").eq(user)).set("updated", now()).returning(["push_subscription_id"])
    };
    assert_eq!(db.update(refresh(uuid(1), ann)).await?.len(), 1);
    assert_eq!(db.update(refresh(uuid(3), ann)).await?.len(), 0);
    // A delivery stamps the application's time of sending and the server's `updated`.
    let notified = db
        .update(
            Update::table("push_subscriptions").key([uuid(3)]).set("updated", now()).set("last_notified", at(7)).returning(["push_subscription_id"]),
        )
        .await?;
    assert_eq!(notified.len(), 1);
    let row = db.get("push_subscriptions", [uuid(3)]).await?.expect("bob's");
    assert_eq!(row.get::<Timestamp>("last_notified")?, at(7));
    assert!(row.get::<Timestamp>("updated")? > stale);

    // The daily sweep: whatever was not refreshed for a week, by the application's clock.
    let week_ago = db.now().await?.plus_secs(-7 * 86_400);
    let swept = db.delete(Delete::from("push_subscriptions").filter(col("updated").lt(week_ago)).returning(["push_subscription_id"])).await?;
    assert_eq!(swept.affected(), 2);
    assert_eq!(ids(swept), BTreeSet::from([uuid(2), uuid(4)]));

    // Unsubscribing needs the owner; a dead endpoint is dropped by id alone.
    let unsubscribe =
        |id: Uuid, user: Uuid| Delete::from("push_subscriptions").key([id]).filter(col("user_id").eq(user)).returning(["push_subscription_id"]);
    assert_eq!(db.delete(unsubscribe(uuid(1), bob)).await?.len(), 0);
    assert_eq!(db.delete(unsubscribe(uuid(1), ann)).await?.len(), 1);
    assert_eq!(db.delete(Delete::from("push_subscriptions").key([uuid(3)]).returning(["push_subscription_id"])).await?.len(), 1);
    assert!(db.query(Query::table("push_subscriptions")).await?.is_empty());
    Ok(())
}

// R-W5, R-T6: webhooks are retried; an event is stored the first time only.
#[tokio::test(flavor = "multi_thread")]
async fn a_delivery_event_is_stored_once() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let raw =
        json!({"signature": {"timestamp": "1794711304", "token": "t0k"}, "event-data": {"severity": "permanent", "event": "failed", "id": "ev-1"}});
    let event = |reason: &str| {
        Insert::into("delivery_events")
            .set("updated", now())
            .set("event_id", "ev-1")
            .set("event", "failed")
            .set("message_id", "<m1@example.com>")
            .set("recipient", "x@remote.test")
            .set("severity", "permanent")
            .set("reason", reason)
            .set("occurred_at", at(0))
            .set("received_at", at(5))
            .set("raw", raw.clone())
            .on_conflict_do_nothing_on(PRIMARY)
            .returning(all_columns(&delivery_events_table()))
    };
    let stored = db.insert(event("bounce")).await?;
    assert_eq!(stored.affected(), 1);
    let stored = stored.one()?;
    assert_eq!((stored.get::<Timestamp>("occurred_at")?, stored.get::<Timestamp>("received_at")?), (at(0), at(5)));
    // The payload comes back as jsonb gives it: keys by length, then bytes.
    assert_eq!(
        stored.get::<Json>("raw")?.to_string(),
        r#"{"signature":{"token":"t0k","timestamp":"1794711304"},"event-data":{"id":"ev-1","event":"failed","severity":"permanent"}}"#
    );

    let retried = db.insert(event("suppress-bounce")).await?;
    assert_eq!((retried.affected(), retried.len()), (0, 0));
    assert_eq!(db.get("delivery_events", ["ev-1"]).await?.expect("the first delivery").get::<String>("reason")?, "bounce");
    Ok(())
}

/// What became of an attempt to add an allowlist pattern.
#[derive(Debug, PartialEq)]
enum Added {
    Created(Uuid),
    Exists,
    Full,
}

/// Adds a pattern unless the user's list is full. SQL can put the count and
/// the insert in one `INSERT .. SELECT .. WHERE (count) < cap`; here they
/// are the two statements of a transaction, which has the database to
/// itself. When no row comes back, a probe tells "already there" from
/// "full".
async fn allow(db: &Client, user: Uuid, pattern: &str, cap: i64) -> DbResult<Added> {
    let proposed = pattern.to_string();
    let created = db
        .transaction(|tx| async move {
            let have: i64 = tx
                .query_one(Query::table("spam_allowlist").index("spam_allowlist_user_pattern", KeyRange::eq([user])).select("count", count()))
                .await?
                .get("count")?;
            if have >= cap {
                return Ok(None);
            }
            let insert = Insert::into("spam_allowlist")
                .set("user_id", user)
                .set("pattern", proposed)
                .on_conflict_do_nothing_on("spam_allowlist_user_pattern")
                .returning(all_columns(&spam_allowlist_table()));
            Ok::<_, DbError>(tx.insert(insert).await?.opt())
        })
        .await?;
    if let Some(row) = created {
        return Ok(Added::Created(row.get("allowlist_id")?));
    }
    let probe =
        Query::table("spam_allowlist").index("spam_allowlist_user_pattern", KeyRange::eq([val(user), val(pattern)])).select("one", val(1)).limit(1);
    Ok(if db.query(probe).await?.len() == 1 { Added::Exists } else { Added::Full })
}

// R-Q3, R-Q6, R-Q11, R-W5, R-W10, R-X1: the spam allowlist is listed, checked for every received mail, added to under a cap and removed from.
#[tokio::test(flavor = "multi_thread")]
async fn the_allowlist_is_matched_by_address_or_domain_and_capped() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob) = (user(&db, "ann").await?, user(&db, "bob").await?);
    // Patterns are stored lower-cased by the application; one from before that rule is not.
    for (owner, pattern, age) in
        [(ann, "bob@remote.test", 30), (ann, "*@partner.test", 20), (ann, "Carol@Remote.test", 10), (bob, "bob@remote.test", 5)]
    {
        db.insert(Insert::into("spam_allowlist").set("user_id", owner).set("pattern", pattern).set("created_at", now() - seconds(age))).await?;
    }

    // The settings page lists the newest first.
    let list =
        |user: Uuid| Query::table("spam_allowlist").index("spam_allowlist_user_pattern", KeyRange::eq([user])).order_by(col("created_at").desc());
    assert_eq!(column::<String>(&db.query(list(ann)).await?, "pattern"), ["Carol@Remote.test", "*@partner.test", "bob@remote.test"]);

    // For every received mail: is the sender, or the sender's whole
    // domain, on the list? Two keys of the lower-cased index, counted.
    let allowed = |user: Uuid, sender: &str| {
        let sender = sender.to_lowercase();
        let domain = format!("*@{}", sender.split('@').nth(1).unwrap_or_default());
        let keys = [KeyRange::eq([val(user), val(sender)]), KeyRange::eq([val(user), val(domain)])];
        Query::table("spam_allowlist").index_ranges("spam_allowlist_user_lower", keys).select("count", count())
    };
    for (owner, sender, hits) in [
        (ann, "Bob@Remote.Test", 1),
        (ann, "carol@remote.test", 1),
        (ann, "dave@partner.test", 1),
        (ann, "dave@remote.test", 0),
        (bob, "carol@remote.test", 0),
    ] {
        assert_eq!(db.query_one(allowed(owner, sender)).await?.get::<i64>("count")?, hits, "{sender}");
    }

    // Adding under a cap, which is five for the test; a real one is far higher.
    let Added::Created(erin) = allow(&db, ann, "erin@remote.test", 5).await? else { panic!("there is room for a fourth") };
    let row = db.get("spam_allowlist", [erin]).await?.expect("created");
    assert_eq!(row.get::<String>("pattern")?, "erin@remote.test");
    assert!(row.get::<Timestamp>("created_at")? <= db.now().await?);
    assert_eq!(allow(&db, ann, "erin@remote.test", 5).await?, Added::Exists);
    assert!(matches!(allow(&db, ann, "frank@remote.test", 5).await?, Added::Created(_)));
    assert_eq!(allow(&db, ann, "gina@remote.test", 5).await?, Added::Full);
    assert_eq!(allow(&db, ann, "erin@remote.test", 5).await?, Added::Exists, "a full list still knows what it holds");
    assert!(matches!(allow(&db, bob, "gina@remote.test", 5).await?, Added::Created(_)), "the cap is per user");
    assert_eq!(db.query(list(ann)).await?.len(), 5);

    // Remove by pattern, whatever case it was stored in. The count is the answer.
    let remove = |pattern: &str| Delete::from("spam_allowlist").index("spam_allowlist_user_lower", KeyRange::eq([val(ann), val(pattern)]));
    assert_eq!(db.delete(remove("carol@remote.test")).await?.affected(), 1);
    assert_eq!(db.delete(remove("carol@remote.test")).await?.affected(), 0);
    // Remove by id, only for its owner.
    let remove_id = |user: Uuid| Delete::from("spam_allowlist").key([erin]).filter(col("user_id").eq(user));
    assert_eq!(db.delete(remove_id(bob)).await?.affected(), 0);
    assert_eq!(db.delete(remove_id(ann)).await?.affected(), 1);
    assert_eq!(
        column::<String>(&db.query(list(ann)).await?, "pattern").into_iter().collect::<BTreeSet<_>>(),
        BTreeSet::from(["bob@remote.test", "*@partner.test", "frank@remote.test"].map(String::from))
    );
    Ok(())
}

// R-X2, R-X3, R-X8: allowlist adds under a race (as one statement in Postgres, two adds of different patterns could both pass the count).
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_allowlist_adds_stop_exactly_at_the_cap() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    for pattern in ["bob@remote.test", "*@partner.test"] {
        assert!(matches!(allow(&db, ann, pattern, 5).await?, Added::Created(_)));
    }

    let adds: Vec<_> = (0..12)
        .map(|n| {
            let db = db.clone();
            tokio::spawn(async move { allow(&db, ann, &format!("sender-{n}@remote.test"), 5).await })
        })
        .collect();
    let (mut created, mut full) = (0, 0);
    for add in adds {
        match add.await.expect("the task finishes")? {
            Added::Created(_) => created += 1,
            Added::Full => full += 1,
            Added::Exists => panic!("every task adds a pattern of its own"),
        }
    }
    assert_eq!((created, full), (3, 9));
    assert_eq!(db.query(Query::table("spam_allowlist").index("spam_allowlist_user_pattern", KeyRange::eq([ann]))).await?.len(), 5);
    Ok(())
}

/// Counts a trained document's words. One statement of up to 501 rows; a
/// word seen before has the new counts added to its own.
fn train(user: Uuid, words: &[String], spam: i64, ham: i64) -> Insert {
    let mut insert = Insert::into("spam_training").columns(["user_id", "word", "spam_count", "ham_count"]);
    for word in words {
        insert = insert.values([val(user), val(word), val(spam), val(ham)]);
    }
    insert
        .on_conflict_update("spam_training_user_word")
        .update("spam_count", col("spam_count") + excluded("spam_count"))
        .update("ham_count", col("ham_count") + excluded("ham_count"))
}

// R-W2, R-W4, R-Q6, R-S5: spam training writes and reads the word counts of a whole document in one statement.
#[tokio::test(flavor = "multi_thread")]
async fn training_adds_the_counts_of_501_words_in_one_statement() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob) = (user(&db, "ann").await?, user(&db, "bob").await?);
    // A document's distinct words, and the row that counts documents.
    let document = |words: std::ops::Range<u32>| words.map(|n| format!("word{n:03}")).chain(["__total__".to_string()]).collect::<Vec<_>>();

    // A spam document, then a ham one that shares half its words.
    assert_eq!(db.insert(train(ann, &document(0..500), 1, 0)).await?.affected(), 501);
    assert_eq!(db.insert(train(ann, &document(250..750), 0, 1)).await?.affected(), 501);
    db.insert(train(bob, &document(0..3), 5, 5)).await?;

    // Everything a user has trained.
    let of_user = |user: Uuid| Query::table("spam_training").index("spam_training_user_word", KeyRange::eq([user]));
    let all = db.query(of_user(ann)).await?;
    assert_eq!(all.len(), 751);
    assert_eq!(column::<Uuid>(&all, "training_id").into_iter().collect::<BTreeSet<_>>().len(), 751, "each row has an id of its own");
    let mut counts = BTreeMap::new();
    for row in &all {
        counts.insert(row.get::<String>("word")?, (row.get::<i64>("spam_count")?, row.get::<i64>("ham_count")?));
    }
    for (word, expected) in [
        ("word000", (1, 0)),
        ("word249", (1, 0)),
        ("word250", (1, 1)),
        ("word499", (1, 1)),
        ("word500", (0, 1)),
        ("word749", (0, 1)),
        ("__total__", (1, 1)),
    ] {
        assert_eq!(counts[word], expected, "{word}");
    }

    // The totals row alone.
    let totals = db
        .query_one(
            Query::table("spam_training")
                .index("spam_training_user_word", KeyRange::eq([val(ann), val("__total__")]))
                .columns(["spam_count", "ham_count"]),
        )
        .await?;
    assert_eq!((totals.get::<i64>("spam_count")?, totals.get::<i64>("ham_count")?), (1, 1));

    // For every received mail: the counts of its words, the ones never
    // trained simply missing. 501 keys in one statement.
    let mut asked: Vec<String> = (700..1200).map(|n| format!("word{n:03}")).collect();
    asked.push("__total__".into());
    let by_key = db
        .query(Query::table("spam_training").index_ranges("spam_training_user_word", asked.iter().map(|word| KeyRange::eq([val(ann), val(word)]))))
        .await?;
    let known: Vec<String> = column(&by_key, "word");
    assert_eq!(known.len(), 51);
    assert_eq!(known, asked.iter().filter(|word| counts.contains_key(*word)).cloned().collect::<Vec<_>>(), "in the order asked");
    // The same question as written in SQL: `word IN (..)` as a filter.
    let by_filter = db.query(of_user(ann).filter(col("word").in_list(asked.clone()))).await?;
    assert_eq!(column::<String>(&by_filter, "word").into_iter().collect::<BTreeSet<_>>(), known.into_iter().collect::<BTreeSet<_>>());

    // Forget everything one user trained.
    assert_eq!(db.delete(Delete::from("spam_training").index("spam_training_user_word", KeyRange::eq([ann]))).await?.affected(), 751);
    assert!(db.query(of_user(ann)).await?.is_empty());
    assert_eq!(db.query(of_user(bob)).await?.len(), 4);
    Ok(())
}

// R-S7, R-W11, R-X1: a length limit like VARCHAR(100), where Postgres' 22001 fails the statement and the application counts on it.
#[tokio::test(flavor = "multi_thread")]
async fn an_over_long_training_word_fails_the_whole_statement() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    db.insert(train(ann, &["fine".to_string()], 1, 0)).await?;

    let words = ["fine".to_string(), "new".to_string(), "x".repeat(101), "__total__".to_string()];
    let refused = db.insert(train(ann, &words, 1, 0)).await.unwrap_err();
    assert_eq!(refused.code, ErrorCode::CheckViolation);
    assert!(refused.violates("spam_training_word_length"));
    // None of its rows were written and none of its additions made.
    let rows = db.query(Query::table("spam_training").index("spam_training_user_word", KeyRange::eq([ann])).columns(["word", "spam_count"])).await?;
    assert_eq!(rows.map::<(String, i64)>()?, [("fine".to_string(), 1)]);

    // The limit counts characters, as VARCHAR did: a hundred two-byte letters fit.
    assert_eq!(db.insert(train(ann, &["é".repeat(100)], 0, 1)).await?.affected(), 1);
    let one_more = db.insert(train(ann, &["é".repeat(101)], 0, 1)).await.unwrap_err();
    assert_eq!(one_more.code, ErrorCode::CheckViolation);
    Ok(())
}

// R-W12, R-W4, R-Q10, R-Q11, R-S2: every delivery reserves two UIDs and a MODSEQ from counters, atomically.
#[tokio::test(flavor = "multi_thread")]
async fn a_counter_is_seeded_from_the_data_once_and_then_counts_up() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob) = (user(&db, "ann").await?, user(&db, "bob").await?);

    // A user with no mail: every counter starts at one.
    assert_eq!(next_domain_uid(&db, bob, false).await?, 1);
    assert_eq!(next_domain_uid(&db, bob, false).await?, 2);
    assert_eq!(next_modseq(&db, bob).await?, 1);

    // ann has mail from before the counters existed.
    save_all(
        &db,
        &[
            Mail { uid_domain: 5, modseq: 40, ..mail(ann, 1) },
            Mail { uid_domain: 9, modseq: 42, expunged: true, ..mail(ann, 2) },
            Mail { sent: true, uid_domain: 3, modseq: 7, ..mail(ann, 3) },
        ],
    )
    .await?;
    mapped(&db, ann, "INBOX/accounts/sales", uuid(1), 7).await?;
    mapped(&db, ann, "sales", uuid(2), 12).await?;
    mapped(&db, ann, "INBOX/accounts/info", uuid(1), 30).await?;

    // Received mail continues after the highest UID ever given, expunged
    // mail included; sent mail has a counter of its own.
    assert_eq!(next_domain_uid(&db, ann, false).await?, 10);
    assert_eq!(next_domain_uid(&db, ann, false).await?, 11);
    assert_eq!(next_domain_uid(&db, ann, true).await?, 4);
    // An account's box is seeded from the mapping rows under both of its names.
    let sales = box_seed(ann, &["INBOX/accounts/sales", "sales"]);
    assert_eq!(reserved(&db, reserve(ann, "account", "sales@example.com", false, &sales)).await?, 13);
    assert_eq!(reserved(&db, reserve(ann, "account", "sales@example.com", false, &sales)).await?, 14);
    // Starred has no rows yet.
    let starred = box_seed(ann, &["Starred"]);
    assert_eq!(reserved(&db, reserve(ann, "mailbox", "Starred", false, &starred)).await?, 1);
    // One MODSEQ counter per user, over all of the user's mail.
    assert_eq!(next_modseq(&db, ann).await?, 43);

    // The seed is read when the counter row is made and never again:
    // data that appears later does not move the counter.
    save(&db, &Mail { uid_domain: 500, modseq: 900, ..mail(ann, 4) }).await?;
    assert_eq!(next_domain_uid(&db, ann, false).await?, 12);
    assert_eq!(next_modseq(&db, ann).await?, 44);

    let stored = db.get("mail_uid_counters", [val(ann), val("domain"), val(""), val(false)]).await?.expect("the counter row");
    assert_eq!(stored.get::<i64>("last_uid")?, 12);
    let of_ann =
        db.query(Query::table("mail_uid_counters").range(KeyRange::eq([ann])).columns(["uid_kind", "uid_scope", "sent", "last_uid"])).await?;
    // In primary key order: kind, scope, sent.
    assert_eq!(
        of_ann.map::<(String, String, bool, i64)>()?,
        [
            ("account".to_string(), "sales@example.com".to_string(), false, 14),
            ("domain".to_string(), String::new(), false, 12),
            ("domain".to_string(), String::new(), true, 4),
            ("mailbox".to_string(), "Starred".to_string(), false, 1),
            ("modseq".to_string(), String::new(), false, 44),
        ]
    );
    Ok(())
}

// R-W12, R-X2, R-X3, R-X8: counter reservations under contention are strictly increasing per key and never reused.
#[tokio::test(flavor = "multi_thread")]
async fn twenty_tasks_reserving_at_once_get_distinct_consecutive_numbers() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    save_all(&db, &[mail(ann, 3), mail(ann, 7)]).await?;

    // No counter row exists, so the tasks also race to create it: one
    // inserts the seed, the others must take the update branch.
    let tasks: Vec<_> = (0..20)
        .map(|_| {
            let db = db.clone();
            tokio::spawn(async move {
                let mut mine = Vec::new();
                for _ in 0..20 {
                    mine.push(next_domain_uid(&db, ann, false).await?);
                }
                Ok::<_, DbError>(mine)
            })
        })
        .collect();
    let mut all = Vec::new();
    for task in tasks {
        let mine = task.await.expect("the task finishes")?;
        assert_eq!(mine.len(), 20);
        assert!(mine.windows(2).all(|pair| pair[0] < pair[1]), "a session sees its counter only grow: {mine:?}");
        all.extend(mine);
    }
    all.sort();
    // 400 reservations seeded after UID 7: each of 8..=407 exactly once.
    assert_eq!(all, (8..=407).collect::<Vec<i64>>());
    let stored = db.get("mail_uid_counters", [val(ann), val("domain"), val(""), val(false)]).await?.expect("the counter row");
    assert_eq!(stored.get::<i64>("last_uid")?, 407);
    Ok(())
}

// R-Q10, R-Q11: UIDNEXT for SELECT and STATUS, which is never MAX(uid) of the surviving rows.
#[tokio::test(flavor = "multi_thread")]
async fn uidnext_is_peeked_without_allocating() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob) = (user(&db, "ann").await?, user(&db, "bob").await?);
    save_all(&db, &[mail(ann, 5), mail(ann, 9)]).await?;
    let inbox = domain_seed(ann, false);

    // Before the first delivery there is no counter: the seed answers.
    assert_eq!(uid_next(&db, peek(ann, "domain", "", false, &inbox)).await?, 10);
    assert_eq!(uid_next(&db, peek(ann, "domain", "", false, &inbox)).await?, 10);
    assert!(db.query(Query::table("mail_uid_counters")).await?.is_empty(), "peeking makes no counter");
    // The next delivery gets the number that was announced.
    assert_eq!(next_domain_uid(&db, ann, false).await?, 10);
    assert_eq!(uid_next(&db, peek(ann, "domain", "", false, &inbox)).await?, 11);
    // From then on the counter answers, whatever becomes of the data.
    db.delete(Delete::from("mails").key([uuid(9)])).await?;
    assert_eq!(uid_next(&db, peek(ann, "domain", "", false, &inbox)).await?, 11);

    // The same for a mapped box, and for a user with nothing at all: one
    // row, never none, since the caller must not make a value up.
    let starred = box_seed(ann, &["Starred"]);
    assert_eq!(uid_next(&db, peek(ann, "mailbox", "Starred", false, &starred)).await?, 1);
    mapped(&db, ann, "Starred", uuid(5), 4).await?;
    assert_eq!(uid_next(&db, peek(ann, "mailbox", "Starred", false, &starred)).await?, 5);
    assert_eq!(reserved(&db, reserve(ann, "mailbox", "Starred", false, &starred)).await?, 5);
    assert_eq!(uid_next(&db, peek(ann, "mailbox", "Starred", false, &starred)).await?, 6);
    assert_eq!(uid_next(&db, peek(bob, "domain", "", true, &domain_seed(bob, true))).await?, 1);
    Ok(())
}

// R-W4, R-W11, R-S3, R-S6: a mail is filed in a mailbox under a UID by upsert, and taken out again by delete.
#[tokio::test(flavor = "multi_thread")]
async fn a_mapping_upsert_returns_the_uid_that_stuck() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    save_all(&db, &[mail(ann, 1), mail(ann, 2)]).await?;
    let sales = "INBOX/accounts/sales";

    let first = db.insert(map_mail(ann, sales, uuid(1), 4)).await?;
    assert_eq!(first.affected(), 1);
    assert_eq!(first.column::<i64>()?, [4]);
    // A retried save reserved a new number, and learns the mail kept its old one.
    assert_eq!(mapped(&db, ann, sales, uuid(1), 5).await?, 4);
    // Each mailbox numbers the mail for itself.
    assert_eq!(mapped(&db, ann, "INBOX", uuid(1), 4).await?, 4);
    assert_eq!(mapped(&db, ann, sales, uuid(2), 6).await?, 6);

    // A second mail claiming a UID the box has given out breaks the other
    // unique index. The upsert absorbs only its own target, so this is an
    // error. For an account's box the save that caused it switches to its
    // merge path; for the INBOX marker, where old data holds duplicate
    // domain UIDs, the application logs it and carries on without the row.
    db.delete(Delete::from("mail_mailbox_uid").key([val(ann), val(sales), val(uuid(2))])).await?;
    for mailbox in [sales, "INBOX"] {
        let clash = db.insert(map_mail(ann, mailbox, uuid(2), 4)).await.unwrap_err();
        assert_eq!(clash.code, ErrorCode::UniqueViolation, "{mailbox}");
        assert!(clash.violates(BOX_UID), "{mailbox}");
        assert!(db.get("mail_mailbox_uid", [val(ann), val(mailbox), val(uuid(2))]).await?.is_none());
    }
    // A mapping row is a mail's: without the mail it is refused.
    let orphan = db.insert(map_mail(ann, "INBOX", uuid(404), 9)).await.unwrap_err();
    assert_eq!(orphan.code, ErrorCode::ForeignKeyViolation);

    // Un-starring removes the pivot row; starring again takes a new UID.
    assert_eq!(mapped(&db, ann, "Starred", uuid(1), 1).await?, 1);
    let unstar = || Delete::from("mail_mailbox_uid").key([val(ann), val("Starred"), val(uuid(1))]);
    assert_eq!(db.delete(unstar()).await?.affected(), 1);
    assert_eq!(db.delete(unstar()).await?.affected(), 0);
    assert_eq!(mapped(&db, ann, "Starred", uuid(1), 2).await?, 2);

    let boxes = db.query(Query::table("mail_mailbox_uid").index(BOX_MAIL, KeyRange::eq([uuid(1)])).columns(["mailbox", "uid"])).await?;
    let of_mail: BTreeSet<(String, i64)> = boxes.map::<(String, i64)>()?.into_iter().collect();
    assert_eq!(of_mail, BTreeSet::from([("INBOX".to_string(), 4), (sales.to_string(), 4), ("Starred".to_string(), 2)]));
    Ok(())
}

// R-X4, R-W12, R-W1, R-Q1: saving a mail takes several statements, with no transaction, on whichever connections are free.
#[tokio::test(flavor = "multi_thread")]
async fn saving_a_mail_is_several_statements_each_visible_to_every_connection_at_once() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    let other = second_instance(&db).await?;
    let sales = "INBOX/accounts/sales";

    // The SMTP handler reserves the two UIDs side by side.
    let (uid_domain, uid_box) = tokio::try_join!(
        next_domain_uid(&db, ann, false),
        reserved(&other, reserve(ann, "account", "sales@example.com", false, &box_seed(ann, &[sales, "sales"]))),
    )?;
    assert_eq!((uid_domain, uid_box), (1, 1));
    let modseq = next_modseq(&other, ann).await?;

    let delivered = Mail { uid_domain, modseq, to: vec!["sales@example.com"], saved: true, ..mail(ann, 1) };
    let inserted = save(&db, &delivered).await?;
    assert_eq!(inserted.affected(), 1);
    assert_eq!(inserted.column::<Uuid>()?, [delivered.mail_id]);
    // Committed and visible to the other pool the moment the call returned.
    let seen = other.get("mails", [delivered.mail_id]).await?.expect("visible to every connection");
    assert_eq!((seen.get::<i64>("uid_domain")?, seen.get::<i64>("modseq")?), (1, 1));
    assert_eq!(seen.get::<String>("thread_subject_key")?, "mail 1");

    // The account box row and the INBOX marker are written in parallel,
    // on two connections, right after the insert they point at.
    let (in_box, in_inbox) =
        tokio::try_join!(mapped(&db, ann, sales, delivered.mail_id, uid_box), mapped(&other, ann, "INBOX", delivered.mail_id, uid_domain))?;
    assert_eq!((in_box, in_inbox), (1, 1));
    // A flagged mail also gets a row in Starred, numbered by its own counter.
    let starred_uid = reserved(&other, reserve(ann, "mailbox", "Starred", false, &box_seed(ann, &["Starred"]))).await?;
    assert_eq!(mapped(&db, ann, "Starred", delivered.mail_id, starred_uid).await?, 1);

    // An IDLE session on the other instance re-reads its UID lists and
    // finds the mail in each view.
    assert_eq!(other.query(domain_view(ann, false, Members::Inbox).select("uid", col("m.uid_domain"))).await?.column::<i64>()?, [1]);
    for mailbox in [sales, "Starred"] {
        assert_eq!(db.query(mapped_view(ann, false, mailbox, Members::All).select("uid", col("x.uid"))).await?.column::<i64>()?, [1], "{mailbox}");
    }

    // The second delivery continues every counter.
    let (uid_domain, uid_box) = tokio::try_join!(
        next_domain_uid(&other, ann, false),
        reserved(&db, reserve(ann, "account", "sales@example.com", false, &box_seed(ann, &[sales, "sales"])))
    )?;
    assert_eq!((uid_domain, uid_box, next_modseq(&db, ann).await?), (2, 2, 2));
    Ok(())
}

// R-W11, R-S3, R-W6, R-X4: a unique violation (Postgres' 23505) is the signal to merge a duplicate delivery.
#[tokio::test(flavor = "multi_thread")]
async fn a_duplicate_message_id_is_a_unique_violation_that_turns_into_a_merge() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob) = (user(&db, "ann").await?, user(&db, "bob").await?);
    let sales = "INBOX/accounts/sales";
    let first = Mail {
        message_id: "<newsletter-42@remote.test>".into(),
        envelope_to: vec!["sales@example.com"],
        is_spam: true,
        modseq: next_modseq(&db, ann).await?,
        ..mail(ann, 1)
    };
    save(&db, &first).await?;
    mapped(&db, ann, sales, first.mail_id, 1).await?;
    let stale = rewind_updated(&db, ann).await?;

    // The same message arrives for a second recipient of the same user.
    // Its UIDs and MODSEQ were reserved before the insert, as always.
    let second_uid = next_domain_uid(&db, ann, false).await?;
    let again = Mail { mail_id: uuid(2), uid_domain: second_uid, envelope_to: vec!["info@example.com"], ..first.clone() };
    let duplicate = save(&db, &again).await.unwrap_err();
    assert_eq!(duplicate.code, ErrorCode::UniqueViolation);
    assert!(duplicate.violates(MAILS_MESSAGE));
    assert_eq!(duplicate.table.as_deref(), Some("mails"));
    assert!(db.get("mails", [again.mail_id]).await?.is_none());
    // Another user may hold the same Message-ID: the key is per user.
    save(&db, &Mail { mail_id: uuid(3), user_id: bob, ..first.clone() }).await?;

    // Merge. First the stored row is read whole.
    let stored = db.query_one(Query::table("mails").index(MAILS_MESSAGE, KeyRange::eq([val(ann), val(&first.message_id)]))).await?;
    assert_eq!(stored.get::<Uuid>("mail_id")?, first.mail_id);
    // The application appends the envelope addresses it has not seen and writes the array back.
    let mut envelope = stored.get::<Json>("envelope_to")?.as_array().cloned().expect("an array");
    envelope.extend(address_list(&again.envelope_to).and_then(|list| list.as_array().cloned()).expect("one recipient"));
    let merged = db
        .update(
            Update::table("mails")
                .index(MAILS_MESSAGE, KeyRange::eq([val(ann), val(&first.message_id)]))
                .set("envelope_to", Json::Array(envelope))
                .set("updated", now()),
        )
        .await?;
    assert_eq!(merged.affected(), 1);
    // The second copy was not spam, so the mail leaves Junk, under a new MODSEQ.
    let modseq = next_modseq(&db, ann).await?;
    let flipped = db
        .update(
            Update::table("mails")
                .index(MAILS_MESSAGE, KeyRange::eq([val(ann), val(&first.message_id)]))
                .set("is_spam", false)
                .set("modseq", modseq)
                .set("updated", now()),
        )
        .await?;
    assert_eq!(flipped.affected(), 1);
    // Filing it in the box it is already in gives back the UID it has;
    // the number reserved for the duplicate is simply never used.
    assert_eq!(mapped(&db, ann, sales, first.mail_id, 2).await?, 1);
    assert_eq!(mapped(&db, ann, "INBOX/accounts/info", first.mail_id, 1).await?, 1);

    let row = db.get("mails", [first.mail_id]).await?.expect("the one row for that Message-ID");
    assert_eq!(
        row.get::<Json>("envelope_to")?.to_string(),
        r#"[{"name":"sales","address":"sales@example.com"},{"name":"info","address":"info@example.com"}]"#
    );
    assert_eq!((row.get::<bool>("is_spam")?, row.get::<i64>("modseq")?, row.get::<i64>("uid_domain")?), (false, 2, 1));
    assert!(row.get::<Timestamp>("updated")? > stale);
    assert_eq!(db.query(Query::table("mails").index(MAILS_DATE, KeyRange::eq([ann]))).await?.len(), 1);
    // The merged mail is now found under both recipients.
    for address in ["sales@example.com", "info@example.com"] {
        let found = db.query(Query::table("mails").index(MAILS_RECIPIENT, KeyRange::eq([val(ann), val(address)])).columns(["mail_id"])).await?;
        assert_eq!(found.column::<Uuid>()?, [first.mail_id], "{address}");
    }
    Ok(())
}

/// Sets `read` or `saved` if it is not set that way already, stamping a
/// MODSEQ. A returned row means this caller made the change.
fn set_flag(flag: &str, mail_id: Uuid, user: Uuid, on: bool, modseq: i64) -> Update {
    Update::table("mails")
        .key([mail_id])
        .filter(col("user_id").eq(user).and(col(flag).eq(!on)))
        .set(flag, on)
        .set("updated", now())
        .set("modseq", modseq)
        .returning(["mail_id"])
}

/// One flag of one mail, to tell "already so" from "no such mail".
async fn flag(db: &Client, name: &str, mail_id: Uuid, user: Uuid) -> DbResult<Option<bool>> {
    match db.query_opt(Query::table("mails").key([mail_id]).filter(col("user_id").eq(user)).columns([name]).limit(1)).await? {
        Some(row) => Ok(Some(row.get(name)?)),
        None => Ok(None),
    }
}

// R-W7, R-X3, R-X4, R-W6: marking a mail read or saved is a probe, a reservation and a compare-and-set.
#[tokio::test(flavor = "multi_thread")]
async fn marking_a_mail_read_is_a_compare_and_set_that_one_caller_wins() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob) = (user(&db, "ann").await?, user(&db, "bob").await?);
    save_all(&db, &[mail(ann, 1), mail(ann, 2)]).await?;
    let stale = rewind_updated(&db, ann).await?;

    // Opening a mail in eight tabs at once: each probes, reserves a
    // MODSEQ and tries the guarded update.
    let attempts: Vec<_> = (0..8)
        .map(|_| {
            let db = db.clone();
            tokio::spawn(async move {
                flag(&db, "read", uuid(1), ann).await?.expect("the mail is there, read or not");
                let modseq = next_modseq(&db, ann).await?;
                let changed = db.update(set_flag("read", uuid(1), ann, true, modseq)).await?;
                Ok::<_, DbError>((changed.len() == 1).then_some(modseq))
            })
        })
        .collect();
    let mut winners = Vec::new();
    for attempt in attempts {
        winners.extend(attempt.await.expect("the task finishes")?);
    }
    assert_eq!(winners.len(), 1, "exactly one update finds the flag unset");
    let row = db.get("mails", [uuid(1)]).await?.expect("the mail");
    assert!(row.get::<bool>("read")?);
    assert_eq!(row.get::<i64>("modseq")?, winners[0], "the row carries the winner's MODSEQ, not a loser's");
    assert!(row.get::<Timestamp>("updated")? > stale);
    // The losers reserved numbers that nothing will ever carry.
    assert_eq!(next_modseq(&db, ann).await?, 10);

    // A loser re-probes: "already read" and "not your mail" both updated
    // nothing, and only the probe tells them apart.
    assert!(db.update(set_flag("read", uuid(1), ann, true, 11)).await?.is_empty());
    assert_eq!(flag(&db, "read", uuid(1), ann).await?, Some(true));
    assert!(db.update(set_flag("read", uuid(2), bob, true, 11)).await?.is_empty());
    assert_eq!(flag(&db, "read", uuid(2), bob).await?, None);
    assert_eq!(flag(&db, "read", uuid(2), ann).await?, Some(false));

    // The same shape for the star, in both directions.
    assert_eq!(db.update(set_flag("saved", uuid(2), ann, true, 11)).await?.len(), 1);
    assert_eq!(db.update(set_flag("saved", uuid(2), ann, false, 12)).await?.len(), 1);
    assert!(db.update(set_flag("saved", uuid(2), ann, false, 13)).await?.is_empty());
    assert_eq!(db.get("mails", [uuid(2)]).await?.expect("the mail").get::<i64>("modseq")?, 12);

    // Caching the computed size is not a change a client should sync, so
    // it leaves `updated` and `modseq` alone.
    let before = updated_of(&db, uuid(2)).await?;
    let sized = db.update(Update::table("mails").key([uuid(2)]).filter(col("user_id").eq(ann)).set("rfc822_size", 48_213_i64)).await?;
    assert_eq!(sized.affected(), 1);
    let row = db.get("mails", [uuid(2)]).await?.expect("the mail");
    assert_eq!((row.get::<i64>("rfc822_size")?, row.get::<i64>("modseq")?, row.get::<Timestamp>("updated")?), (48_213, 12, before));
    Ok(())
}

// R-W7, R-T9, R-W6: marking a mail as spam, and telling "nothing changed" from "no such mail".
#[tokio::test(flavor = "multi_thread")]
async fn marking_spam_reports_whether_anything_changed() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob) = (user(&db, "ann").await?, user(&db, "bob").await?);
    save_all(&db, &[mail(ann, 1), Mail { expunged: true, ..mail(ann, 2) }]).await?;
    let stale = rewind_updated(&db, ann).await?;
    let mark = |mail_id: Uuid, user: Uuid, spam: bool, modseq: i64| {
        Update::table("mails")
            .key([mail_id])
            .filter(all([col("user_id").eq(user), col("expunged").eq(false), col("is_spam").is_distinct_from(spam)]))
            .set("is_spam", spam)
            .set("updated", now())
            .set("modseq", modseq)
            .returning(["mail_id"])
    };
    // When nothing changed, is the mail there at all?
    let found = |mail_id: Uuid, user: Uuid| {
        Query::table("mails").key([mail_id]).filter(col("user_id").eq(user).and(col("expunged").eq(false))).select("one", val(1)).limit(1)
    };

    assert_eq!(db.update(mark(uuid(1), ann, true, 5)).await?.affected(), 1);
    let row = db.get("mails", [uuid(1)]).await?.expect("the mail");
    assert_eq!((row.get::<bool>("is_spam")?, row.get::<i64>("modseq")?), (true, 5));
    assert!(row.get::<Timestamp>("updated")? > stale);
    // Marking it again changes nothing, so no client needs to hear of it.
    assert_eq!(db.update(mark(uuid(1), ann, true, 6)).await?.affected(), 0);
    assert_eq!(db.query(found(uuid(1), ann)).await?.len(), 1);
    assert_eq!(db.get("mails", [uuid(1)]).await?.expect("the mail").get::<i64>("modseq")?, 5);
    // An expunged mail, someone else's, or none: nothing changed, and not found.
    for (mail_id, owner) in [(uuid(2), ann), (uuid(1), bob), (uuid(404), ann)] {
        assert_eq!(db.update(mark(mail_id, owner, true, 7)).await?.affected(), 0);
        assert!(db.query(found(mail_id, owner)).await?.is_empty());
    }
    assert_eq!(updated_of(&db, uuid(2)).await?, stale);
    // And back out of Junk.
    assert_eq!(db.update(mark(uuid(1), ann, false, 8)).await?.column::<Uuid>()?, [uuid(1)]);
    Ok(())
}

// R-S6, R-W10, R-W11: hard deletes of a mail and of a user, and what the foreign keys take with them.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_mail_or_a_user_takes_what_points_at_it_and_nothing_else() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob) = (user(&db, "ann").await?, user(&db, "bob").await?);
    for owner in [ann, bob] {
        let n = if owner == ann { 1 } else { 11 };
        save_all(&db, &[mail(owner, n), mail(owner, n + 1)]).await?;
        for mail_id in [uuid(n as u128), uuid(n as u128 + 1)] {
            mapped(&db, owner, "INBOX", mail_id, mail_id.0[15] as i64).await?;
            mapped(&db, owner, "Archive", mail_id, mail_id.0[15] as i64).await?;
        }
        next_domain_uid(&db, owner, false).await?;
        next_modseq(&db, owner).await?;
        db.insert(put_session(&format!("session-{n}"), owner, 1, &[])).await?;
        db.insert(Insert::into("mailboxes").set("user_id", owner).set("name", "Archive")).await?;
        db.insert(
            Insert::into("push_subscriptions")
                .set("user_id", owner)
                .set("endpoint", "https://push.example/x")
                .set("keys_p256dh", "p")
                .set("keys_auth", "a"),
        )
        .await?;
        db.insert(Insert::into("spam_allowlist").set("user_id", owner).set("pattern", "*@partner.test")).await?;
        db.insert(train(owner, &["word".to_string()], 1, 0)).await?;
    }
    let rows = |table: &str| {
        let query = Query::table(table).select("n", count());
        let db = db.clone();
        async move { db.query_one(query).await?.get::<i64>("n") }
    };

    // The HTTP delete is a hard delete, for the owner only.
    let delete = |mail_id: Uuid, user: Uuid| Delete::from("mails").key([mail_id]).filter(col("user_id").eq(user)).returning(["mail_id"]);
    assert_eq!(db.delete(delete(uuid(1), bob)).await?.affected(), 0);
    let deleted = db.delete(delete(uuid(1), ann)).await?;
    assert_eq!(deleted.affected(), 1, "the count is of mails, not of what went with them");
    assert_eq!(deleted.column::<Uuid>()?, [uuid(1)]);
    assert_eq!(db.delete(delete(uuid(1), ann)).await?.affected(), 0);
    // Its mapping rows went with it; every other mail's are untouched.
    assert!(db.query(Query::table("mail_mailbox_uid").index(BOX_MAIL, KeyRange::eq([uuid(1)]))).await?.is_empty());
    assert_eq!((rows("mails").await?, rows("mail_mailbox_uid").await?), (3, 6));

    // A child cannot be written for a parent that is not there.
    let orphan_mail = save(&db, &mail(uuid(404), 50)).await.unwrap_err();
    assert_eq!(orphan_mail.code, ErrorCode::ForeignKeyViolation);
    let orphan_box = db.insert(Insert::into("mailboxes").set("user_id", uuid(404)).set("name", "Nowhere")).await.unwrap_err();
    assert_eq!(orphan_box.code, ErrorCode::ForeignKeyViolation);

    // Deleting a user empties five tables directly and the mapping table
    // through the mails.
    assert_eq!(db.delete(Delete::from("users").key([ann]).returning(["user_id"])).await?.affected(), 1);
    for table in ["mailboxes", "push_subscriptions", "spam_allowlist", "spam_training", "users"] {
        assert_eq!(rows(table).await?, 1, "{table} keeps only bob's row");
    }
    assert_eq!((rows("mails").await?, rows("mail_mailbox_uid").await?), (2, 4));
    assert!(db.query(Query::table("mails").index(MAILS_DATE, KeyRange::eq([ann]))).await?.is_empty());
    assert!(db.query(Query::table("mail_mailbox_uid").range(KeyRange::eq([ann]))).await?.is_empty());
    // Sessions and counters name a user without a foreign key: they stay,
    // the session until a request trips over it, the counters for good.
    assert_eq!(rows("sessions").await?, 2);
    assert_eq!(db.query(Query::table("mail_uid_counters").range(KeyRange::eq([ann]))).await?.len(), 2);
    Ok(())
}

// R-Q13, R-Q2, R-Q1: an attachment download is authorised by finding its id in a mail of the user.
#[tokio::test(flavor = "multi_thread")]
async fn an_attachment_is_authorised_by_json_containment() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob) = (user(&db, "ann").await?, user(&db, "bob").await?);
    let file = |id: &str, name: &str| json!({"filename": name, "contentType": "application/pdf", "content": {"data": id}, "size": 48213});
    save_all(
        &db,
        &[
            Mail { attachments: Some(json!([file("f-001", "invoice.pdf"), file("f-002", "terms.pdf")])), ..mail(ann, 1) },
            Mail { attachments: Some(json!([file("f-003", "old.pdf")])), expunged: true, ..mail(ann, 2) },
            mail(ann, 3),
            Mail { attachments: Some(json!([])), ..mail(ann, 4) },
            Mail { attachments: Some(json!([file("f-900", "theirs.pdf")])), ..mail(bob, 11) },
        ],
    )
    .await?;
    // `attachments @> '[{"content":{"data":"<id>"}}]'`: some element of
    // the array has that id, whatever else it and its neighbours hold.
    let holding = |user: Uuid, id: &str| {
        Query::table("mails")
            .index(MAILS_DATE, KeyRange::eq([user]))
            .filter(col("attachments").json_contains(json!([{"content": {"data": id}}])).and(col("expunged").eq(false)))
            .limit(1)
    };

    for id in ["f-001", "f-002"] {
        let owner = db.query_opt(holding(ann, id)).await?.expect("a mail of ann holds it");
        assert_eq!(owner.get::<Uuid>("mail_id")?, uuid(1));
        // The route then serves from the row it got: all of it comes back.
        assert_eq!(owner.get::<Json>("attachments")?.as_array().map(Vec::len), Some(2));
    }
    assert!(db.query_opt(holding(ann, "f-003")).await?.is_none(), "an expunged mail authorises nothing");
    assert!(db.query_opt(holding(ann, "f-900")).await?.is_none(), "nor does another user's mail");
    assert!(db.query_opt(holding(bob, "f-900")).await?.is_some());
    assert!(db.query_opt(holding(ann, "f-00")).await?.is_none(), "a string is contained whole or not at all");
    assert!(db.query_opt(holding(ann, "invoice.pdf")).await?.is_none(), "and only under the key asked for");
    // A NULL column and an empty array contain nothing, and are no error.
    assert!(db.query_opt(holding(ann, "f-404")).await?.is_none());

    // One mail, whole, for its owner.
    let whole = |mail_id: Uuid, user: Uuid| Query::table("mails").key([mail_id]).filter(col("user_id").eq(user)).limit(1);
    let row = db.query_opt(whole(uuid(1), ann)).await?.expect("ann's mail");
    assert_eq!(row.columns(), all_columns(&mails_table()));
    assert_eq!(row.get::<String>("text")?, "Body of mail 1");
    assert!(db.query_opt(whole(uuid(1), bob)).await?.is_none());
    Ok(())
}

// R-T6, R-Q13: JSON comes back in the canonical form of Postgres' JSONB, which responses and their ETags expose.
#[tokio::test(flavor = "multi_thread")]
async fn json_comes_back_with_keys_in_jsonb_order() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    let insight =
        json!({"summary": "Quarterly numbers", "tags": ["finance", "q3"], "action": null, "score": 0.75, "x": {"zz": 1, "b": 2, "aaa": 3}, "id": 7});
    save(&db, &Mail { insight: Some(insight.clone()), cc: vec!["carol@example.com", "dave@example.com"], ..mail(ann, 1) }).await?;

    let row = db.get("mails", [uuid(1)]).await?.expect("the mail");
    // Shorter keys first, equal lengths by their bytes; at every depth.
    // Arrays keep their order, and a JSON null is a value, not a missing column.
    let stored: Json = row.get("insight")?;
    assert_eq!(
        stored.to_string(),
        r#"{"x":{"b":2,"zz":1,"aaa":3},"id":7,"tags":["finance","q3"],"score":0.75,"action":null,"summary":"Quarterly numbers"}"#
    );
    assert_eq!(stored, insight, "the same document");
    assert!(!row.is_null("insight")?);
    // The address arrays were written with `address` before `name`.
    assert_eq!(
        row.get::<Json>("cc_address")?.to_string(),
        r#"[{"name":"carol","address":"carol@example.com"},{"name":"dave","address":"dave@example.com"}]"#
    );
    assert_eq!(row.get::<String>("cc_text")?, "carol <carol@example.com>, dave <dave@example.com>");

    // The JSON functions a mail store applies to these columns.
    let parts = db
        .query_one(
            Query::table("mails")
                .key([uuid(1)])
                .select("count", col("cc_address").json_len())
                .select("last", col("cc_address").json([-1]))
                .select("first_name", col("cc_address").json_text([val(0), val("name")]))
                .select("addresses", col("cc_address").json_pluck("address"))
                .select("missing", col("cc_address").json([5]))
                .select("none", col("bcc_address").json_pluck("address"))
                .select("merged", col("cc_address").json_merge(json!([{"address": "erin@example.com"}])).json_len()),
        )
        .await?;
    assert_eq!(parts.get::<i64>("count")?, 2);
    assert_eq!(parts.get::<Json>("last")?, json!({"name": "dave", "address": "dave@example.com"}));
    assert_eq!(parts.get::<String>("first_name")?, "carol");
    assert_eq!(parts.get::<Vec<String>>("addresses")?, ["carol@example.com", "dave@example.com"]);
    assert!(parts.is_null("missing")?);
    assert!(parts.get::<Vec<String>>("none")?.is_empty(), "plucking from a NULL array gives an empty list");
    assert_eq!(parts.get::<i64>("merged")?, 3);
    Ok(())
}

/// The UIDs a query selects as `uid`, in the order it gives them.
async fn uids(db: &Client, query: Query) -> DbResult<Vec<i64>> {
    db.query(query).await?.column()
}

/// A mailbox's counts: (messages, unseen).
async fn counted(db: &Client, rows: Query, members: Members) -> DbResult<(i64, i64)> {
    let row = db.query_one(counts(rows, members)).await?;
    Ok((row.get("total")?, row.get("unread")?))
}

/// HIGHESTMODSEQ, over expunged mail and non-members too, since an expunge
/// must advance it. One for a mailbox that never held anything.
async fn highest_modseq(db: &Client, rows: Query) -> DbResult<i64> {
    db.query_one(rows.select("highest", max(col("m.modseq")).or_else(1))).await?.get("highest")
}

// R-Q2, R-Q4, R-Q5, R-Q6, R-Q11, R-S4: what SELECT, STATUS, FETCH and IDLE read over the domain UID space.
#[tokio::test(flavor = "multi_thread")]
async fn a_domain_view_is_a_uid_range_of_mails_narrowed_by_membership() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob) = (user(&db, "ann").await?, user(&db, "bob").await?);
    save_all(
        &db,
        &[
            Mail { read: true, modseq: 3, ..mail(ann, 1) },
            Mail { modseq: 5, html: "<p>Grüße</p>".into(), text: "Grüße".into(), ..mail(ann, 2) },
            Mail { draft: true, modseq: 2, ..mail(ann, 3) },
            Mail { is_spam: true, modseq: 9, ..mail(ann, 4) },
            Mail { read: true, expunged: true, modseq: 12, ..mail(ann, 5) },
            Mail { saved: true, modseq: 7, ..mail(ann, 6) },
            Mail { sent: true, read: true, uid_domain: 1, modseq: 4, ..mail(ann, 7) },
            Mail { sent: true, deleted: true, uid_domain: 2, modseq: 6, ..mail(ann, 8) },
            Mail { uid_domain: 1, ..mail(bob, 901) },
        ],
    )
    .await?;
    let uid_list = |user: Uuid, sent: bool, members: Members| domain_view(user, sent, members).select("uid", col("m.uid_domain"));

    // A session's UID list on SELECT, for INBOX, Drafts, Junk and Sent.
    // The index gives UID order; nothing is sorted.
    assert_eq!(uids(&db, uid_list(ann, false, Members::Inbox)).await?, [1, 2, 6]);
    assert_eq!(uids(&db, uid_list(ann, false, Members::Drafts)).await?, [3]);
    assert_eq!(uids(&db, uid_list(ann, false, Members::Junk)).await?, [4]);
    assert_eq!(uids(&db, uid_list(ann, true, Members::All)).await?, [1, 2]);
    assert_eq!(uids(&db, uid_list(bob, false, Members::Inbox)).await?, [1]);

    // EXISTS and UNSEEN. An empty mailbox still answers, with zeros.
    assert_eq!(counted(&db, domain_rows(ann, false), Members::Inbox).await?, (3, 2));
    assert_eq!(counted(&db, domain_rows(ann, false), Members::Drafts).await?, (1, 1));
    assert_eq!(counted(&db, domain_rows(ann, false), Members::Junk).await?, (1, 1));
    assert_eq!(counted(&db, domain_rows(ann, true), Members::All).await?, (2, 1));
    assert_eq!(counted(&db, domain_rows(bob, true), Members::All).await?, (0, 0));

    // The first unseen message, found by reading the range until one matches.
    let first_unseen = |user: Uuid, sent: bool, members: Members| uid_list(user, sent, members).filter(col("m.read").eq(false)).limit(1);
    assert_eq!(uids(&db, first_unseen(ann, false, Members::Inbox)).await?, [2]);
    assert_eq!(uids(&db, first_unseen(ann, true, Members::All)).await?, [2]);
    assert!(uids(&db, first_unseen(bob, false, Members::Junk)).await?.is_empty());

    // HIGHESTMODSEQ.
    assert_eq!(highest_modseq(&db, domain_rows(ann, false)).await?, 12);
    assert_eq!(highest_modseq(&db, domain_rows(ann, true)).await?, 6);
    assert_eq!(highest_modseq(&db, domain_rows(bob, true)).await?, 1);

    // FETCH 2:* (the open end arrives as 999999999): only the columns
    // asked for, the UID under the name the session expects, and the body
    // sizes without the bodies.
    let fetch = |low: i64, high: i64| {
        domain_view(ann, false, Members::Inbox)
            .index(MAILS_UID, domain_uids(ann, false, low, high))
            .columns(["m.mail_id", "m.subject", "m.read"])
            .select("uid_mailbox", col("m.uid_domain"))
            .select("text_octets", col("m.text").octet_length())
            .select("html_octets", col("m.html").octet_length())
    };
    let fetched = db.query(fetch(2, 999_999_999)).await?;
    assert_eq!(fetched.columns(), ["mail_id", "subject", "read", "uid_mailbox", "text_octets", "html_octets"]);
    assert_eq!(
        fetched.map::<(Uuid, String, bool, i64, i64, i64)>()?,
        [
            (uuid(2), "Mail 2".to_string(), false, 2, "Grüße".len() as i64, "<p>Grüße</p>".len() as i64),
            (uuid(6), "Mail 6".to_string(), false, 6, "Body of mail 6".len() as i64, 0),
        ]
    );
    // UIDs 3 to 5 exist, but as a draft, a spam and an expunged mail.
    assert!(db.query(fetch(3, 5)).await?.is_empty());
    // CHANGEDSINCE 5.
    assert_eq!(column::<i64>(&db.query(fetch(1, 999_999_999).filter(col("m.modseq").gt(5))).await?, "uid_mailbox"), [6]);
    // The builder's by-position form: messages 2 and 3 of the mailbox.
    assert_eq!(uids(&db, uid_list(ann, false, Members::Inbox).offset(1).limit(2)).await?, [2, 6]);

    // The flags of the mails an IDLE wake-up named, by primary key, dropping
    // those that are not this side's or are expunged. No membership test.
    let named = [uuid(6), uuid(5), uuid(1), uuid(4), uuid(901), uuid(7), uuid(404)];
    let flags = Query::table("mails")
        .alias("m")
        .keys(named.iter().map(|id| [*id]))
        .filter(all([col("m.user_id").eq(ann), col("m.sent").eq(false), col("m.expunged").eq(false)]))
        .columns(["m.mail_id"])
        .select("uid", col("m.uid_domain"))
        .columns(FLAGS);
    assert_eq!(
        db.query(flags).await?.map::<(Uuid, i64, bool, bool, bool, bool, bool)>()?,
        [
            (uuid(6), 6, false, true, false, false, false),
            (uuid(1), 1, true, false, false, false, false),
            (uuid(4), 4, false, false, false, false, false)
        ]
    );
    Ok(())
}

// R-Q9, R-Q2, R-Q4, R-Q5, R-Q6, R-Q11: what SELECT, STATUS, FETCH and IDLE read over a mapped box.
#[tokio::test(flavor = "multi_thread")]
async fn a_mapped_box_is_a_uid_range_of_the_mapping_table_joined_to_its_mails() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob) = (user(&db, "ann").await?, user(&db, "bob").await?);
    let sales = "INBOX/accounts/sales";
    save_all(
        &db,
        &[
            Mail { read: true, modseq: 3, ..mail(ann, 1) },
            Mail { modseq: 5, ..mail(ann, 2) },
            Mail { expunged: true, modseq: 40, ..mail(ann, 3) },
            Mail { is_spam: true, modseq: 9, ..mail(ann, 4) },
            Mail { draft: true, modseq: 2, ..mail(ann, 5) },
            Mail { modseq: 6, ..mail(ann, 6) },
            Mail { sent: true, read: true, uid_domain: 1, modseq: 4, ..mail(ann, 7) },
            Mail { uid_domain: 1, ..mail(bob, 901) },
        ],
    )
    .await?;
    // The box numbers its mails in its own order: the oldest mail has the highest UID.
    let rows = [
        (sales, 2, 10),
        (sales, 3, 12),
        (sales, 4, 14),
        (sales, 5, 15),
        (sales, 1, 30),
        ("Archive", 2, 1),
        ("Archive", 4, 2),
        ("INBOX/accounts/info", 6, 1),
        ("Sent Messages/accounts/sales", 7, 1),
    ];
    for (mailbox, n, uid) in rows {
        mapped(&db, ann, mailbox, uuid(n), uid).await?;
    }
    mapped(&db, bob, sales, uuid(901), 11).await?;
    let uid_list = |user: Uuid, sent: bool, mailbox: &str, members: Members| mapped_view(user, sent, mailbox, members).select("uid", col("x.uid"));

    // The UID list, in the box's UID order, which is the mapping index's.
    assert_eq!(uids(&db, uid_list(ann, false, sales, Members::Inbox)).await?, [10, 30]);
    // A user-created box shows everything filed in it, spam too.
    assert_eq!(uids(&db, uid_list(ann, false, "Archive", Members::All)).await?, [1, 2]);
    // The `sent` side is part of the view: the sent box of the same account is another list.
    assert_eq!(uids(&db, uid_list(ann, true, "Sent Messages/accounts/sales", Members::All)).await?, [1]);
    assert!(uids(&db, uid_list(ann, true, sales, Members::Inbox)).await?.is_empty());
    assert_eq!(uids(&db, uid_list(bob, false, sales, Members::Inbox)).await?, [11]);
    assert!(uids(&db, uid_list(ann, false, "Trash", Members::All)).await?.is_empty());

    // EXISTS and UNSEEN.
    assert_eq!(counted(&db, mapped_rows(ann, false, sales), Members::Inbox).await?, (2, 1));
    assert_eq!(counted(&db, mapped_rows(ann, false, "Archive"), Members::All).await?, (2, 2));
    assert_eq!(counted(&db, mapped_rows(ann, false, "Trash"), Members::All).await?, (0, 0));
    // The first unseen message.
    let first_unseen = uid_list(ann, false, sales, Members::Inbox).filter(col("m.read").eq(false)).limit(1);
    assert_eq!(uids(&db, first_unseen).await?, [10]);
    // HIGHESTMODSEQ: the expunged mail's MODSEQ is the highest.
    assert_eq!(highest_modseq(&db, mapped_rows(ann, false, sales)).await?, 40);
    assert_eq!(highest_modseq(&db, mapped_rows(ann, false, "Trash")).await?, 1);

    // FETCH by UID: a range of the mapping index, projecting the mail's
    // columns and the mapping's UID.
    let fetch = |low: i64, high: i64| {
        mapped_view(ann, false, sales, Members::Inbox)
            .index(BOX_UID, mapped_uids(ann, sales, low, high))
            .columns(["m.mail_id", "m.subject"])
            .select("uid_mailbox", col("x.uid"))
            .select("text_octets", col("m.text").octet_length())
    };
    assert_eq!(db.query(fetch(10, 20)).await?.map::<(Uuid, String, i64, i64)>()?, [(uuid(2), "Mail 2".to_string(), 10, 14)]);
    assert_eq!(column::<i64>(&db.query(fetch(1, 999_999_999)).await?, "uid_mailbox"), [10, 30]);
    assert_eq!(column::<i64>(&db.query(fetch(1, 999_999_999).filter(col("m.modseq").gt(4))).await?, "uid_mailbox"), [10]);
    // By position: the second message of the box.
    assert_eq!(uids(&db, uid_list(ann, false, sales, Members::Inbox).offset(1).limit(1)).await?, [30]);

    // The flags of named mails, with the box's UIDs. Each is one mapping
    // row by primary key; the mail it joins decides whether it counts.
    let named = [uuid(1), uuid(3), uuid(2), uuid(6)];
    let flags = Query::table("mail_mailbox_uid")
        .alias("x")
        .keys(named.iter().map(|id| [val(ann), val(sales), val(*id)]))
        .join(Query::table("mails").alias("m").key([col("x.mail_id")]).filter(col("m.user_id").eq(col("x.user_id")).and(col("m.sent").eq(false))))
        .filter(col("m.expunged").eq(false))
        .columns(["m.mail_id"])
        .select("uid", col("x.uid"))
        .columns(FLAGS);
    assert_eq!(
        db.query(flags).await?.map::<(Uuid, i64, bool, bool, bool, bool, bool)>()?,
        [(uuid(1), 30, true, false, false, false, false), (uuid(2), 10, false, false, false, false, false)]
    );
    // The same as SQL would write it, `m.mail_id = ANY(..)`, is a filter
    // over the whole box, and answers in UID order.
    let scanned = mapped_rows(ann, false, sales)
        .filter(col("m.expunged").eq(false).and(col("m.mail_id").in_list(id_list(&named))))
        .columns(["m.mail_id"])
        .select("uid", col("x.uid"));
    assert_eq!(db.query(scanned).await?.map::<(Uuid, i64)>()?, [(uuid(2), 10), (uuid(1), 30)]);
    Ok(())
}

/// An IMAP SEARCH key, as the query builder gets it from the parser.
enum Key {
    /// ALL, OLD, RECENT: constrains nothing.
    All,
    /// KEYWORD, LARGER, SMALLER, an unknown key: matches nothing.
    Never,
    /// SEEN is `Flag("read", true)`; UNFLAGGED is `Flag("saved", false)`.
    Flag(&'static str, bool),
    /// SUBJECT, FROM, TO, CC, BCC, BODY, HEADER: the column holds the text.
    Contains(&'static str, &'static str),
    /// TEXT: the subject, the sender, the recipients or the body holds it.
    Text(&'static str),
    Before(Timestamp),
    /// The day that starts at this instant.
    On(Timestamp),
    Since(Timestamp),
    /// A UID set: ranges, both ends included.
    Uid(Vec<(i64, i64)>),
    Not(Box<Key>),
    Or(Box<Key>, Box<Key>),
}

fn not(key: Key) -> Key {
    Key::Not(Box::new(key))
}

fn or(a: Key, b: Key) -> Key {
    Key::Or(Box::new(a), Box::new(b))
}

/// What a key comes to: nothing to test, nothing can match, or a condition.
enum Compiled {
    Unconstrained,
    Nothing,
    Condition(Expr),
}

/// A SEARCH builder, with expressions where a SQL one has fragments. The
/// user's text goes into the pattern as typed: `%`, `_` and `\` keep their
/// LIKE meaning, as they do in Postgres. NOT and OR nest without limit.
fn compile(key: &Key, uid: &Expr) -> Compiled {
    let holds = |column: &str, text: &str| col(&format!("m.{column}")).ilike(format!("%{text}%"));
    match key {
        Key::All => Compiled::Unconstrained,
        Key::Never => Compiled::Nothing,
        Key::Flag(column, on) => Compiled::Condition(col(&format!("m.{column}")).eq(*on)),
        Key::Contains(column, text) => Compiled::Condition(holds(column, text)),
        Key::Text(text) => Compiled::Condition(any(["subject", "from_text", "to_text", "text"].map(|column| holds(column, text)))),
        Key::Before(day) => Compiled::Condition(col("m.date").lt(*day)),
        Key::On(day) => Compiled::Condition(col("m.date").ge(*day).and(col("m.date").lt(day.plus_secs(86_400)))),
        Key::Since(day) => Compiled::Condition(col("m.date").ge(*day)),
        Key::Uid(ranges) => {
            Compiled::Condition(any(ranges.iter().map(|&(low, high)| if low == high { uid.clone().eq(low) } else { uid.clone().between(low, high) })))
        }
        Key::Not(inner) => match compile(inner, uid) {
            Compiled::Unconstrained => Compiled::Nothing,
            Compiled::Nothing => Compiled::Unconstrained,
            Compiled::Condition(condition) => Compiled::Condition(condition.not()),
        },
        Key::Or(a, b) => match (compile(a, uid), compile(b, uid)) {
            (Compiled::Unconstrained, _) | (_, Compiled::Unconstrained) => Compiled::Unconstrained,
            (Compiled::Nothing, other) | (other, Compiled::Nothing) => other,
            (Compiled::Condition(a), Compiled::Condition(b)) => Compiled::Condition(a.or(b)),
        },
    }
}

/// The UIDs of a view's mails that satisfy every key, ascending, however
/// many there are. `uid` is the view's UID column.
fn search(view: Query, uid: Expr, keys: &[Key]) -> Query {
    let mut query = view;
    for key in keys {
        query = match compile(key, &uid) {
            Compiled::Unconstrained => query,
            Compiled::Nothing => query.filter(val(false)),
            Compiled::Condition(condition) => query.filter(condition),
        };
    }
    query.select("uid", uid)
}

/// SEARCH in ann's INBOX.
async fn found(db: &Client, user: Uuid, keys: &[Key]) -> DbResult<Vec<i64>> {
    uids(db, search(domain_view(user, false, Members::Inbox), col("m.uid_domain"), keys)).await
}

/// A mailbox to search: five mails in INBOX (UIDs 1, 2, 3, 4 and 6), a
/// spam and an expunged mail that no INBOX search may find.
async fn searchable(db: &Client) -> DbResult<Uuid> {
    let ann = user(db, "ann").await?;
    let mails = [
        Mail {
            subject: "Invoice 100% due".into(),
            text: "Please PAY THE invoice by Friday".into(),
            read: true,
            date: ts("2026-11-10T08:00:00Z"),
            ..mail(ann, 1)
        },
        Mail {
            subject: "Lunch?".into(),
            from: vec!["carol@remote.test"],
            cc: vec!["dave@remote.test"],
            text: "Shall we eat at noon".into(),
            saved: true,
            date: ts("2026-11-11T00:00:00Z"),
            ..mail(ann, 2)
        },
        Mail {
            subject: "RE: lunch".into(),
            cc: vec!["erin@remote.test"],
            text: "Yes, 50% of us can".into(),
            answered: true,
            date: ts("2026-11-11T23:59:59.999999Z"),
            ..mail(ann, 3)
        },
        Mail {
            subject: "a_c test".into(),
            from: vec!["dave@remote.test"],
            text: "abc for 500 people".into(),
            read: true,
            deleted: true,
            date: ts("2026-11-12T00:00:00Z"),
            ..mail(ann, 4)
        },
        Mail { subject: "Lunch offer".into(), is_spam: true, ..mail(ann, 5) },
        Mail {
            subject: "abc news".into(),
            from: vec!["frank@remote.test"],
            bcc: vec!["hidden@example.com"],
            text: "Top 1000 tips".into(),
            read: true,
            date: ts("2026-11-13T09:00:00Z"),
            ..mail(ann, 6)
        },
        Mail { subject: "Lunch leftovers".into(), expunged: true, ..mail(ann, 7) },
    ];
    save_all(db, &mails).await?;
    Ok(ann)
}

// R-Q20, R-Q2, R-Q4: IMAP SEARCH (criteria AND-ed, NOT and OR nested, the builder's rules for keys that constrain nothing or match nothing).
#[tokio::test(flavor = "multi_thread")]
async fn search_combines_keys_with_and_or_and_not_to_any_depth() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = searchable(&db).await?;
    let (seen, unseen, flagged, deleted, answered) = (
        || Key::Flag("read", true),
        || Key::Flag("read", false),
        || Key::Flag("saved", true),
        || Key::Flag("deleted", true),
        || Key::Flag("answered", true),
    );

    // No key at all is every message of the view, in UID order.
    assert_eq!(found(&db, ann, &[]).await?, [1, 2, 3, 4, 6]);
    assert_eq!(found(&db, ann, &[unseen()]).await?, [2, 3]);
    assert_eq!(found(&db, ann, &[Key::Flag("draft", false), Key::Flag("answered", false), seen()]).await?, [1, 4, 6]);
    // Keys side by side must all hold.
    assert_eq!(found(&db, ann, &[unseen(), flagged()]).await?, [2]);
    assert!(found(&db, ann, &[unseen(), flagged(), deleted()]).await?.is_empty());
    // OR and NOT.
    assert_eq!(found(&db, ann, &[or(deleted(), answered())]).await?, [3, 4]);
    assert_eq!(found(&db, ann, &[not(or(deleted(), answered()))]).await?, [1, 2, 6]);
    assert_eq!(found(&db, ann, &[or(not(seen()), Key::Contains("subject", "invoice"))]).await?, [1, 2, 3]);
    assert_eq!(found(&db, ann, &[not(not(flagged()))]).await?, [2]);
    // NOT (OR (NOT SEEN) (OR DELETED KEYWORD x)): seen and not deleted.
    assert_eq!(found(&db, ann, &[not(or(not(seen()), or(deleted(), Key::Never)))]).await?, [1, 6]);
    // Nested six deep.
    let deep = or(not(or(not(or(unseen(), deleted())), not(or(flagged(), not(answered()))))), Key::Contains("subject", "news"));
    assert_eq!(found(&db, ann, &[deep, not(Key::Contains("text", "eat"))]).await?, [4, 6]);

    // What the builder does with keys that are not conditions.
    assert_eq!(found(&db, ann, &[Key::All]).await?, [1, 2, 3, 4, 6]);
    assert!(found(&db, ann, &[Key::Never]).await?.is_empty());
    assert!(found(&db, ann, &[not(Key::All)]).await?.is_empty());
    assert_eq!(found(&db, ann, &[seen(), not(Key::Never)]).await?, [1, 4, 6]);
    assert_eq!(found(&db, ann, &[or(Key::All, Key::Never)]).await?, [1, 2, 3, 4, 6]);
    assert_eq!(found(&db, ann, &[or(Key::Never, flagged())]).await?, [2]);
    assert!(found(&db, ann, &[or(Key::Never, Key::Never)]).await?.is_empty());

    // The other views search their own members.
    assert_eq!(uids(&db, search(domain_view(ann, false, Members::Junk), col("m.uid_domain"), &[Key::Contains("subject", "lunch")])).await?, [5]);
    Ok(())
}

// R-Q7: IMAP SEARCH (case-insensitive substring match on headers and bodies, with the user's wildcards left as wildcards).
#[tokio::test(flavor = "multi_thread")]
async fn search_matches_substrings_without_case_and_keeps_the_users_wildcards() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = searchable(&db).await?;

    assert_eq!(found(&db, ann, &[Key::Contains("subject", "LUNCH")]).await?, [2, 3]);
    assert_eq!(found(&db, ann, &[Key::Contains("from_text", "bob@REMOTE")]).await?, [1, 3]);
    assert_eq!(found(&db, ann, &[Key::Contains("to_text", "Ann <ann@")]).await?, [1, 2, 3, 4, 6]);
    assert_eq!(found(&db, ann, &[Key::Contains("cc_text", "dave")]).await?, [2]);
    assert_eq!(found(&db, ann, &[Key::Contains("bcc_text", "hidden")]).await?, [6]);
    // BODY scans the whole text.
    assert_eq!(found(&db, ann, &[Key::Contains("text", "pay the")]).await?, [1]);
    assert_eq!(found(&db, ann, &[Key::Contains("text", "TIPS")]).await?, [6]);
    // HEADER Message-ID.
    assert_eq!(found(&db, ann, &[Key::Contains("message_id", "<M3@")]).await?, [3]);
    // TEXT looks in the subject, the sender, the recipients and the body.
    assert_eq!(found(&db, ann, &[Key::Text("lunch")]).await?, [2, 3]);
    assert_eq!(found(&db, ann, &[Key::Text("carol")]).await?, [2]);
    assert_eq!(found(&db, ann, &[Key::Text("friday")]).await?, [1]);
    assert_eq!(found(&db, ann, &[Key::Text("lunch"), Key::Contains("from_text", "bob")]).await?, [3]);

    // The text is not escaped. An underscore stands for any one character,
    assert_eq!(found(&db, ann, &[Key::Contains("subject", "a_c")]).await?, [4, 6]);
    // a percent sign for any run of them,
    assert_eq!(found(&db, ann, &[Key::Contains("subject", "in%due")]).await?, [1]);
    assert_eq!(found(&db, ann, &[Key::Contains("text", "50%")]).await?, [3, 4]);
    // and a backslash makes the next character literal.
    assert_eq!(found(&db, ann, &[Key::Contains("text", "50\\%")]).await?, [3]);
    assert_eq!(found(&db, ann, &[Key::Contains("subject", "a\\_c")]).await?, [4]);
    // Had the builder escaped the text, as `like_escape` does, the wildcards would be gone.
    let literal = |text: &str| {
        domain_view(ann, false, Members::Inbox).filter(col("m.subject").ilike(format!("%{}%", like_escape(text)))).select("uid", col("m.uid_domain"))
    };
    assert_eq!(uids(&db, literal("a_c")).await?, [4]);
    assert!(uids(&db, literal("in%due")).await?.is_empty());
    assert_eq!(uids(&db, literal("100%")).await?, [1]);
    // LIKE with case, which other queries here use on Message-IDs, tells `RE` from `re`.
    let cased = domain_view(ann, false, Members::Inbox).filter(col("m.subject").like("RE:%")).select("uid", col("m.uid_domain"));
    assert_eq!(uids(&db, cased).await?, [3]);
    Ok(())
}

// R-T9: IMAP SEARCH (`NOT (cc_text ILIKE ..)` is not true of a mail that has no Cc).
#[tokio::test(flavor = "multi_thread")]
async fn search_follows_sql_null_logic_under_not_and_or() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = searchable(&db).await?;
    let (cc_dave, seen) = (|| Key::Contains("cc_text", "dave"), |on: bool| Key::Flag("read", on));
    let without_cc = domain_view(ann, false, Members::Inbox).filter(col("m.cc_text").is_null()).select("uid", col("m.uid_domain"));
    assert_eq!(uids(&db, without_cc).await?, [1, 4, 6]);

    // CC dave and NOT CC dave do not add up to the mailbox: for the three
    // mails without a Cc header the test is neither true nor false.
    assert_eq!(found(&db, ann, &[cc_dave()]).await?, [2]);
    assert_eq!(found(&db, ann, &[not(cc_dave())]).await?, [3]);
    assert_eq!(found(&db, ann, &[not(not(cc_dave()))]).await?, [2]);
    // Unknown OR true is true; unknown OR false stays unknown.
    assert_eq!(found(&db, ann, &[or(cc_dave(), seen(true))]).await?, [1, 2, 4, 6]);
    assert_eq!(found(&db, ann, &[or(cc_dave(), seen(false))]).await?, [2, 3]);
    assert_eq!(found(&db, ann, &[or(not(cc_dave()), not(seen(true)))]).await?, [2, 3]);
    // NOT of unknown is unknown; NOT of (unknown OR true) is false.
    assert_eq!(found(&db, ann, &[not(or(cc_dave(), seen(true)))]).await?, [3]);
    assert!(found(&db, ann, &[not(or(cc_dave(), seen(false)))]).await?.is_empty());
    // Unknown AND false is false, so its negation is true: the builder
    // never emits this, but an expression may.
    let not_both = domain_view(ann, false, Members::Inbox)
        .filter(col("m.cc_text").ilike("%dave%").and(col("m.read").eq(false)).not())
        .select("uid", col("m.uid_domain"));
    assert_eq!(uids(&db, not_both).await?, [1, 3, 4, 6]);
    Ok(())
}

// R-Q4, R-Q9, R-T3: IMAP SEARCH (dates as half-open ranges, UID sets as unions of ranges, and the same search over a mapped box).
#[tokio::test(flavor = "multi_thread")]
async fn search_dates_are_half_open_and_uid_sets_are_unions_of_ranges() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = searchable(&db).await?;
    let day = |d: u32| ts(&format!("2026-11-{d}T00:00:00Z"));

    // Mail 2 is dated the first microsecond of the 11th, mail 3 the last,
    // mail 4 the first of the 12th.
    assert_eq!(found(&db, ann, &[Key::Before(day(11))]).await?, [1]);
    assert_eq!(found(&db, ann, &[Key::On(day(11))]).await?, [2, 3]);
    assert_eq!(found(&db, ann, &[Key::On(day(12))]).await?, [4]);
    assert_eq!(found(&db, ann, &[Key::Since(day(12))]).await?, [4, 6]);
    assert_eq!(found(&db, ann, &[Key::Since(day(11)), Key::Before(day(13))]).await?, [2, 3, 4]);
    assert_eq!(found(&db, ann, &[not(Key::On(day(11)))]).await?, [1, 4, 6]);
    assert!(found(&db, ann, &[Key::On(day(14))]).await?.is_empty());
    // The application binds dates as ISO text; compared with a timestamp it is read as one.
    let before = domain_view(ann, false, Members::Inbox).filter(col("m.date").lt("2026-11-11T00:00:00.000Z")).select("uid", col("m.uid_domain"));
    assert_eq!(uids(&db, before).await?, [1]);

    // UID 2,4:6 is `uid = 2 OR (uid >= 4 AND uid <= 6)`.
    assert_eq!(found(&db, ann, &[Key::Uid(vec![(2, 2), (4, 6)])]).await?, [2, 4, 6]);
    assert_eq!(found(&db, ann, &[Key::Uid(vec![(1, 3)]), Key::Flag("read", false)]).await?, [2, 3]);
    assert_eq!(found(&db, ann, &[not(Key::Uid(vec![(2, 4)]))]).await?, [1, 6]);
    assert_eq!(found(&db, ann, &[or(Key::Uid(vec![(1, 1)]), Key::Before(day(12))), not(Key::Uid(vec![(3, 3)]))]).await?, [1, 2]);
    assert!(found(&db, ann, &[Key::Uid(vec![(7, 999_999_999)])]).await?.is_empty());

    // In a mapped box the same keys test the same mail columns, and the
    // UID keys and the answer use the box's numbers.
    let sales = "INBOX/accounts/sales";
    for n in 1..=7 {
        mapped(&db, ann, sales, uuid(n), 20 + n as i64).await?;
    }
    let in_box = |keys: &[Key]| search(mapped_view(ann, false, sales, Members::Inbox), col("x.uid"), keys);
    assert_eq!(uids(&db, in_box(&[])).await?, [21, 22, 23, 24, 26]);
    assert_eq!(uids(&db, in_box(&[Key::Uid(vec![(22, 23), (26, 26), (27, 40)])])).await?, [22, 23, 26]);
    assert_eq!(uids(&db, in_box(&[Key::Uid(vec![(21, 24)]), Key::Contains("subject", "lunch")])).await?, [22, 23]);
    assert_eq!(uids(&db, in_box(&[not(Key::Contains("cc_text", "dave")), Key::Since(day(11))])).await?, [23]);
    assert_eq!(uids(&db, in_box(&[or(Key::On(day(10)), Key::Text("tips"))])).await?, [21, 26]);
    Ok(())
}

/// A message as STORE reports it: mail, UID, the five flags, MODSEQ.
type Stored = (Uuid, i64, bool, bool, bool, bool, bool, i64);

/// What STORE returns for each message it changed, the same columns a read
/// of the current state selects. `uid` is the view's UID column.
fn store_returning(update: Update, uid: &str) -> Update {
    update.returning(["m.mail_id"]).returning_expr("uid", col(uid)).returning(FLAGS).returning(["m.modseq"])
}

/// The rows a statement returned, in UID order.
fn stored(rows: Rows) -> DbResult<Vec<Stored>> {
    let mut rows = rows.map::<Stored>()?;
    rows.sort_by_key(|row| row.1);
    Ok(rows)
}

/// One mail's flags and MODSEQ as they are now.
async fn state(db: &Client, mail_id: Uuid) -> DbResult<Stored> {
    let query = Query::table("mails")
        .alias("m")
        .key([mail_id])
        .columns(["m.mail_id"])
        .select("uid", col("m.uid_domain"))
        .columns(FLAGS)
        .columns(["m.modseq"]);
    Ok(db.query(query).await?.map::<Stored>()?.remove(0))
}

// R-W6, R-W7, R-Q4, R-X4: STORE in a domain view, with CONDSTORE's UNCHANGEDSINCE guard.
#[tokio::test(flavor = "multi_thread")]
async fn store_stamps_every_message_it_addresses_and_skips_those_changed_since() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob) = (user(&db, "ann").await?, user(&db, "bob").await?);
    save_all(
        &db,
        &[
            mail(ann, 1),
            Mail { modseq: 2, ..mail(ann, 2) },
            Mail { read: true, modseq: 3, ..mail(ann, 3) },
            Mail { modseq: 4, ..mail(ann, 4) },
            Mail { expunged: true, modseq: 5, ..mail(ann, 5) },
            Mail { draft: true, ..mail(ann, 6) },
            Mail { sent: true, uid_domain: 2, ..mail(ann, 7) },
            Mail { uid_domain: 2, ..mail(bob, 902) },
        ],
    )
    .await?;
    let stale = rewind_updated(&db, ann).await?;
    // The messages a UID range addresses in INBOX.
    let addressed = |low: i64, high: i64| domain_view(ann, false, Members::Inbox).index(MAILS_UID, domain_uids(ann, false, low, high));

    // UID STORE 1:5 (UNCHANGEDSINCE 3) +FLAGS (\Seen). One MODSEQ is
    // reserved for the whole command.
    let modseq = next_modseq(&db, ann).await?;
    assert_eq!(modseq, 6);
    // First a read of what the range addresses, to report the ones the guard skips.
    assert_eq!(uids(&db, addressed(1, 5).select("uid", col("m.uid_domain"))).await?, [1, 2, 3, 4]);
    // Then the update: the guard is one more condition of the same
    // statement, and the rows come back as they are after the change.
    let store = Update::rows(addressed(1, 5)).filter(col("m.modseq").le(3)).set("read", true).set("updated", now()).set("modseq", modseq);
    let updated = db.update(store_returning(store, "m.uid_domain")).await?;
    assert_eq!(updated.affected(), 3);
    assert_eq!(
        stored(updated)?,
        [
            (uuid(1), 1, true, false, false, false, false, 6),
            (uuid(2), 2, true, false, false, false, false, 6),
            (uuid(3), 3, true, false, false, false, false, 6)
        ]
    );
    // Mail 3 was read already and is stamped all the same. Mail 4 was
    // changed after the client last looked, so it is MODIFIED, untouched.
    assert!(updated_of(&db, uuid(3)).await? > stale);
    assert_eq!(state(&db, uuid(4)).await?, (uuid(4), 4, false, false, false, false, false, 4));
    assert_eq!(updated_of(&db, uuid(4)).await?, stale);
    // Nor did it reach the expunged mail, the draft with a UID outside
    // the view, the sent mail or another user's mail with UIDs in range.
    for (mail_id, expected) in [(uuid(5), 5), (uuid(6), 1), (uuid(7), 1), (uuid(902), 1)] {
        let (_, _, read, .., untouched) = state(&db, mail_id).await?;
        assert_eq!((read, untouched), (false, expected), "{mail_id}");
    }

    // UID STORE 2 FLAGS (\Flagged \Answered): all five are assigned.
    let modseq = next_modseq(&db, ann).await?;
    let replace = Update::rows(addressed(2, 2)).set("read", false).set("saved", true).set("deleted", false).set("draft", false).set("answered", true);
    let replaced = db.update(store_returning(replace.set("updated", now()).set("modseq", modseq), "m.uid_domain")).await?;
    assert_eq!(stored(replaced)?, [(uuid(2), 2, false, true, false, false, true, 7)]);

    // UID STORE 1:2 -FLAGS (\Flagged): mail 1 had no star to lose, and
    // still carries the command's MODSEQ afterwards.
    let modseq = next_modseq(&db, ann).await?;
    let unflag = Update::rows(addressed(1, 2)).set("saved", false).set("updated", now()).set("modseq", modseq);
    assert_eq!(
        stored(db.update(store_returning(unflag, "m.uid_domain")).await?)?,
        [(uuid(1), 1, true, false, false, false, false, 8), (uuid(2), 2, false, false, false, false, true, 8)]
    );

    // A STORE that names no flag changes nothing and reports the current
    // state; one that addresses nothing returns nothing.
    let current = addressed(1, 999_999_999).columns(["m.mail_id"]).select("uid", col("m.uid_domain")).columns(FLAGS).columns(["m.modseq"]);
    assert_eq!(
        db.query(current).await?.map::<Stored>()?,
        [
            (uuid(1), 1, true, false, false, false, false, 8),
            (uuid(2), 2, false, false, false, false, true, 8),
            (uuid(3), 3, true, false, false, false, false, 6),
            (uuid(4), 4, false, false, false, false, false, 4),
        ]
    );
    let nothing =
        db.update(store_returning(Update::rows(addressed(50, 60)).set("read", true).set("updated", now()).set("modseq", 9), "m.uid_domain")).await?;
    assert_eq!((nothing.affected(), nothing.len()), (0, 0));

    // The Drafts view addresses the draft by the same UID space.
    let draft = Update::rows(domain_view(ann, false, Members::Drafts).index(MAILS_UID, domain_uids(ann, false, 1, 999_999_999)))
        .set("deleted", true)
        .set("updated", now())
        .set("modseq", 9);
    assert_eq!(stored(db.update(store_returning(draft, "m.uid_domain")).await?)?, [(uuid(6), 6, false, false, true, true, false, 9)]);
    Ok(())
}

/// The mails of a mapped box with UIDs `low..=high`, as rows of `mails`:
/// an update changes its query's own table, so the mails come first and
/// each is matched to its mapping row by that row's primary key.
fn mapped_mails(user: Uuid, sent: bool, mailbox: &str, members: Members, low: i64, high: i64) -> Query {
    domain_rows(user, sent)
        .join(
            Query::table("mail_mailbox_uid")
                .alias("x")
                .key([col("m.user_id"), val(mailbox), col("m.mail_id")])
                .filter(col("x.uid").between(low, high)),
        )
        .filter(col("m.expunged").eq(false).and(members.condition()))
}

// R-W8, R-W7, R-Q9, R-W4: STORE in a mapped box (SQL's `UPDATE .. FROM`, returning the joined table's UID).
#[tokio::test(flavor = "multi_thread")]
async fn store_in_a_mapped_box_updates_mails_through_a_join_and_returns_the_box_uid() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    let sales = "INBOX/accounts/sales";
    save_all(
        &db,
        &[
            mail(ann, 1),
            Mail { modseq: 2, ..mail(ann, 2) },
            Mail { modseq: 8, ..mail(ann, 3) },
            Mail { expunged: true, ..mail(ann, 4) },
            Mail { is_spam: true, ..mail(ann, 5) },
            mail(ann, 6),
        ],
    )
    .await?;
    for (mailbox, n, uid) in [(sales, 1, 10), (sales, 2, 11), (sales, 3, 12), (sales, 4, 13), (sales, 5, 14), ("Archive", 6, 11), ("Archive", 1, 12)]
    {
        mapped(&db, ann, mailbox, uuid(n), uid).await?;
    }
    let stale = rewind_updated(&db, ann).await?;

    // UID STORE 10:14 (UNCHANGEDSINCE 5) +FLAGS (\Flagged) in the account's inbox.
    let modseq = next_modseq(&db, ann).await?;
    assert_eq!(modseq, 9);
    // The read of what is addressed takes the box's own UID range.
    let matched = mapped_view(ann, false, sales, Members::Inbox).index(BOX_UID, mapped_uids(ann, sales, 10, 14)).select("uid", col("x.uid"));
    assert_eq!(uids(&db, matched).await?, [10, 11, 12]);
    // The update changes `mails` and answers with the UID from the mapping row.
    let store = Update::rows(mapped_mails(ann, false, sales, Members::Inbox, 10, 14))
        .filter(col("m.modseq").le(5))
        .set("saved", true)
        .set("updated", now())
        .set("modseq", modseq);
    let updated = db.update(store_returning(store, "x.uid")).await?;
    assert_eq!(updated.affected(), 2);
    let updated = stored(updated)?;
    assert_eq!(updated, [(uuid(1), 10, false, true, false, false, false, 9), (uuid(2), 11, false, true, false, false, false, 9)]);
    // UID 12 was changed since, 13 is expunged, 14 is in Junk; and UID 11
    // of another box is another mail.
    for (mail_id, expected) in [(uuid(3), 8), (uuid(4), 1), (uuid(5), 1), (uuid(6), 1)] {
        let (_, _, _, saved, .., untouched) = state(&db, mail_id).await?;
        assert_eq!((saved, untouched), (false, expected), "{mail_id}");
        assert_eq!(updated_of(&db, mail_id).await?, stale);
    }
    // Only `mails` was written: the mapping rows are as they were.
    assert_eq!(
        db.query(Query::table("mail_mailbox_uid").index(BOX_UID, KeyRange::eq([val(ann), val(sales)])).columns(["uid"])).await?.column::<i64>()?,
        [10, 11, 12, 13, 14]
    );

    // Per updated row whose star changed, a row in Starred: reserve a UID, then file the mail.
    let starred = box_seed(ann, &["Starred"]);
    for (mail_id, ..) in &updated {
        let uid = reserved(&db, reserve(ann, "mailbox", "Starred", false, &starred)).await?;
        assert_eq!(mapped(&db, ann, "Starred", *mail_id, uid).await?, uid);
    }
    assert_eq!(uids(&db, mapped_view(ann, false, "Starred", Members::All).select("uid", col("x.uid"))).await?, [1, 2]);

    // The same mail addressed through another box answers with that box's UID.
    let modseq = next_modseq(&db, ann).await?;
    let seen = Update::rows(mapped_mails(ann, false, "Archive", Members::All, 12, 12)).set("read", true).set("updated", now()).set("modseq", modseq);
    assert_eq!(stored(db.update(store_returning(seen, "x.uid")).await?)?, [(uuid(1), 12, true, true, false, false, false, 10)]);

    // UID STORE 10 -FLAGS (\Flagged), then a delete takes the mail out of Starred.
    let modseq = next_modseq(&db, ann).await?;
    let unflag =
        Update::rows(mapped_mails(ann, false, sales, Members::Inbox, 10, 10)).set("saved", false).set("updated", now()).set("modseq", modseq);
    assert_eq!(stored(db.update(store_returning(unflag, "x.uid")).await?)?, [(uuid(1), 10, true, false, false, false, false, 11)]);
    assert_eq!(db.delete(Delete::from("mail_mailbox_uid").key([val(ann), val("Starred"), val(uuid(1))])).await?.affected(), 1);
    assert_eq!(uids(&db, mapped_view(ann, false, "Starred", Members::All).select("uid", col("x.uid"))).await?, [2]);
    Ok(())
}

// R-W9, R-W8, R-Q5, R-Q10: STORE by message sequence number (`mail_id IN (SELECT .. ORDER BY uid OFFSET .. LIMIT ..)`).
#[tokio::test(flavor = "multi_thread")]
async fn store_can_address_messages_by_their_position_in_the_mailbox() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    let sales = "INBOX/accounts/sales";
    save_all(
        &db,
        &[mail(ann, 1), mail(ann, 2), Mail { expunged: true, ..mail(ann, 3) }, mail(ann, 4), Mail { draft: true, ..mail(ann, 5) }, mail(ann, 7)],
    )
    .await?;
    for (n, uid) in [(7, 3), (4, 5), (2, 8), (3, 9), (1, 12)] {
        mapped(&db, ann, sales, uuid(n), uid).await?;
    }

    // INBOX holds UIDs 1, 2, 4, 7. Messages 2:3 are the second and third
    // in that order: the update takes its rows from a query with an
    // offset and a limit.
    let second_and_third =
        Update::rows(domain_view(ann, false, Members::Inbox).offset(1).limit(2)).set("read", true).set("updated", now()).set("modseq", 2);
    assert_eq!(
        stored(db.update(store_returning(second_and_third, "m.uid_domain")).await?)?,
        [(uuid(2), 2, true, false, false, false, false, 2), (uuid(4), 4, true, false, false, false, false, 2)]
    );

    // In the account's box the order is the box's: UIDs 3, 5, 8, 12 are
    // mails 7, 4, 2, 1. The positions are a subquery over the mapping
    // index joined to the live mails; the statement takes the mails
    // whose id is in it.
    let positions = Query::table("mail_mailbox_uid")
        .alias("y")
        .index(BOX_UID, KeyRange::eq([val(ann), val(sales)]))
        .join(Query::table("mails").alias("z").key([col("y.mail_id")]).filter(all([
            col("z.user_id").eq(col("y.user_id")),
            col("z.sent").eq(false),
            col("z.expunged").eq(false),
            col("z.is_spam").eq(false),
            col("z.draft").eq(false),
        ])))
        .offset(1)
        .limit(2)
        .select("mail_id", col("y.mail_id"));
    // The current state of messages 2:3.
    let current = mapped_rows(ann, false, sales)
        .filter(col("m.mail_id").in_list(list_of(positions.clone())))
        .columns(["m.mail_id"])
        .select("uid", col("x.uid"))
        .columns(FLAGS)
        .columns(["m.modseq"]);
    assert_eq!(
        db.query(current).await?.map::<Stored>()?,
        [(uuid(4), 5, true, false, false, false, false, 2), (uuid(2), 8, true, false, false, false, false, 2)]
    );
    // The same two, flagged.
    let by_position = domain_rows(ann, false)
        .join(Query::table("mail_mailbox_uid").alias("x").key([col("m.user_id"), val(sales), col("m.mail_id")]))
        .filter(col("m.mail_id").in_list(list_of(positions)));
    let flagged = Update::rows(by_position).set("saved", true).set("updated", now()).set("modseq", 3);
    assert_eq!(
        stored(db.update(store_returning(flagged, "x.uid")).await?)?,
        [(uuid(4), 5, true, true, false, false, false, 3), (uuid(2), 8, true, true, false, false, false, 3)]
    );
    assert_eq!(state(&db, uuid(7)).await?, (uuid(7), 7, false, false, false, false, false, 1));
    Ok(())
}

// R-W6, R-W7, R-Q6, R-Q4: EXPUNGE of the Sent box and the source side of MOVE, each one statement.
#[tokio::test(flavor = "multi_thread")]
async fn expunge_in_a_domain_view_marks_rows_and_answers_their_uids() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob) = (user(&db, "ann").await?, user(&db, "bob").await?);
    save_all(
        &db,
        &[
            Mail { sent: true, deleted: true, ..mail(ann, 1) },
            Mail { sent: true, ..mail(ann, 2) },
            Mail { sent: true, deleted: true, expunged: true, modseq: 3, ..mail(ann, 3) },
            Mail { sent: true, deleted: true, ..mail(ann, 4) },
            Mail { deleted: true, ..mail(ann, 5) },
            mail(ann, 6),
            Mail { is_spam: true, ..mail(ann, 7) },
            mail(ann, 8),
            Mail { sent: true, deleted: true, uid_domain: 1, ..mail(bob, 901) },
        ],
    )
    .await?;
    let stale = rewind_updated(&db, ann).await?;

    // EXPUNGE in Sent Messages: soft-delete what is flagged \Deleted and
    // not gone already, under a fresh MODSEQ; the UIDs go to the client.
    let modseq = next_modseq(&db, ann).await?;
    assert_eq!(modseq, 4);
    let expunge = Update::table("mails")
        .index(MAILS_UID, KeyRange::eq([val(ann), val(true)]))
        .filter(col("deleted").eq(true).and(col("expunged").eq(false)))
        .set("expunged", true)
        .set("updated", now())
        .set("modseq", modseq)
        .returning_expr("uid", col("uid_domain"));
    let gone = db.update(expunge.clone()).await?;
    assert_eq!(gone.affected(), 2);
    assert_eq!(gone.column::<i64>()?.into_iter().collect::<BTreeSet<_>>(), BTreeSet::from([1, 4]));
    // A second EXPUNGE, or another session's, finds nothing left to do.
    assert_eq!(db.update(expunge).await?.affected(), 0);
    // The session rebuilds its sequence numbers, and HIGHESTMODSEQ has moved.
    assert_eq!(uids(&db, domain_view(ann, true, Members::All).select("uid", col("m.uid_domain"))).await?, [2]);
    assert_eq!(highest_modseq(&db, domain_rows(ann, true)).await?, 4);
    // The rows are kept, marked; the one expunged long ago kept its MODSEQ.
    assert_eq!(state(&db, uuid(1)).await?, (uuid(1), 1, false, false, true, false, false, 4));
    assert!(db.get("mails", [uuid(4)]).await?.expect("soft-deleted, not removed").get::<bool>("expunged")?);
    assert_eq!(state(&db, uuid(3)).await?.7, 3);
    assert_eq!(updated_of(&db, uuid(3)).await?, stale);
    // Deleted mail on the received side, and bob's sent mail, are not this view's.
    assert!(!db.get("mails", [uuid(5)]).await?.expect("ann's").get::<bool>("expunged")?);
    assert!(!db.get("mails", [uuid(901)]).await?.expect("bob's").get::<bool>("expunged")?);

    // UID MOVE 6:8,99 out of INBOX: the source rows go the same way,
    // named by UID (`uid_domain IN (..)`), flagged \Deleted or not. UID 7
    // is in Junk and UID 99 does not exist.
    let modseq = next_modseq(&db, ann).await?;
    let sources = Query::table("mails").alias("m").index_ranges(MAILS_UID, [6, 7, 8, 99].map(|uid| KeyRange::eq([val(ann), val(false), val(uid)])));
    let moved = Update::rows(sources.filter(col("m.expunged").eq(false).and(Members::Inbox.condition())))
        .set("expunged", true)
        .set("updated", now())
        .set("modseq", modseq)
        .returning_expr("uid", col("m.uid_domain"));
    assert_eq!(db.update(moved).await?.column::<i64>()?.into_iter().collect::<BTreeSet<_>>(), BTreeSet::from([6, 8]));
    assert_eq!(uids(&db, domain_view(ann, false, Members::Inbox).select("uid", col("m.uid_domain"))).await?, [5]);
    assert_eq!(uids(&db, domain_view(ann, false, Members::Junk).select("uid", col("m.uid_domain"))).await?, [7]);
    Ok(())
}

// R-Q10, R-Q7, R-W7, R-X4: which deleted mails an EXPUNGE in INBOX may take.
#[tokio::test(flavor = "multi_thread")]
async fn inbox_expunge_takes_only_the_mails_inbox_owns() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob) = (user(&db, "ann").await?, user(&db, "bob").await?);
    let copy = |n: i64| Mail { message_id: format!("{n:016x}.copy@server"), deleted: true, ..mail(ann, n) };
    let deleted = |n: i64| Mail { deleted: true, ..mail(ann, n) };
    save_all(
        &db,
        &[
            deleted(1),
            copy(2),
            deleted(3),
            deleted(4),
            copy(5),
            mail(ann, 6),
            deleted(7),
            Mail { is_spam: true, ..deleted(8) },
            deleted(9),
            copy(10),
            Mail { expunged: true, ..deleted(11) },
            Mail { deleted: true, uid_domain: 1, ..mail(bob, 901) },
        ],
    )
    .await?;
    let filed = [
        (1, "INBOX"),
        (2, "Archive"),
        (4, "Starred"),
        (6, "INBOX"),
        (7, "INBOX/accounts/sales"),
        (8, "INBOX"),
        (9, "Archive"),
        (9, "Trash"),
        (10, "INBOX/accounts/info"),
        (11, "INBOX"),
    ];
    for (n, mailbox) in filed {
        mapped(&db, ann, mailbox, uuid(n), n as i64).await?;
    }
    mapped(&db, bob, "INBOX", uuid(901), 1).await?;

    // A deleted mail is INBOX's to expunge if a mapping row files it under
    // INBOX or an account's inbox; or if it is not an IMAP copy and no
    // mapping row files it anywhere but Starred and Trash.
    let boxes_of_the_mail = || {
        Query::table("mail_mailbox_uid").alias("x").index(BOX_MAIL, KeyRange::eq([col("m.mail_id")])).filter(col("x.user_id").eq(col("m.user_id")))
    };
    let in_an_inbox = exists(boxes_of_the_mail().filter(col("x.mailbox").eq("INBOX").or(col("x.mailbox").like("INBOX/accounts/%"))));
    let filed_elsewhere = exists(boxes_of_the_mail().filter(col("x.mailbox").not_in_list(vec!["Starred", "Trash"])));
    let candidates = domain_view(ann, false, Members::Inbox)
        .filter(col("m.deleted").eq(true))
        .filter(in_an_inbox.or(col("m.message_id").not_like("%.copy@server").and(filed_elsewhere.not())))
        .columns(["m.mail_id"])
        .select("uid", col("m.uid_domain"));
    let looked_up = db.query(candidates).await?.map::<(Uuid, i64)>()?;
    // 1: under INBOX. 3: filed nowhere. 4: only starred. 7 and 10: in an
    // account's inbox, 10 although it is a copy. Not 2 and 9 (Archive's),
    // 5 (a copy filed nowhere), 6 (not deleted), 8 (Junk's), 11 (gone).
    assert_eq!(looked_up, [(uuid(1), 1), (uuid(3), 3), (uuid(4), 4), (uuid(7), 7), (uuid(10), 10)]);

    // Between the lookup and the mark, another session takes \Deleted off mail 3.
    db.update(Update::table("mails").key([uuid(3)]).set("deleted", false)).await?;
    let mappings_before = db.query(Query::table("mail_mailbox_uid").range(KeyRange::eq([ann]))).await?.len();

    // Reserve a MODSEQ, then mark the mails found, re-checking that each is
    // still deleted and not expunged. The ids that come back are the ones
    // this EXPUNGE removed; the session maps them to the UIDs it looked up.
    let modseq = next_modseq(&db, ann).await?;
    let mark = Update::rows(Query::table("mails").keys(looked_up.iter().map(|(mail_id, _)| [*mail_id])).filter(all([
        col("user_id").eq(ann),
        col("deleted").eq(true),
        col("expunged").eq(false),
    ])))
    .set("expunged", true)
    .set("updated", now())
    .set("modseq", modseq)
    .returning(["mail_id"]);
    let marked = db.update(mark).await?;
    assert_eq!(marked.affected(), 4);
    let marked: BTreeSet<Uuid> = marked.column::<Uuid>()?.into_iter().collect();
    let expunged: Vec<i64> = looked_up.iter().filter(|(mail_id, _)| marked.contains(mail_id)).map(|(_, uid)| *uid).collect();
    assert_eq!(expunged, [1, 4, 7, 10]);

    // What the view holds afterwards.
    assert_eq!(uids(&db, domain_view(ann, false, Members::Inbox).select("uid", col("m.uid_domain"))).await?, [2, 3, 5, 6, 9]);
    assert_eq!(highest_modseq(&db, domain_rows(ann, false)).await?, modseq);
    // Expunging never removes a mapping row: the UID stays retired.
    assert_eq!(db.query(Query::table("mail_mailbox_uid").range(KeyRange::eq([ann]))).await?.len(), mappings_before);
    assert!(db.get("mail_mailbox_uid", [val(ann), val("INBOX"), val(uuid(1))]).await?.is_some());
    assert!(!db.get("mails", [uuid(901)]).await?.expect("bob's").get::<bool>("expunged")?);
    Ok(())
}

/// The Message-ID of an IMAP copy, `<16 hex digits>.copy@server`: a
/// function of the source's id and the destination, so that a retried COPY
/// collides with its first attempt. A server would take the digits from a
/// SHA-256; any stable digest shows the same thing.
fn copy_message_id(source: &str, destination: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (source, destination).hash(&mut hasher);
    format!("{:016x}.copy@server", hasher.finish())
}

/// COPY's insert: the source row as it was read, bodies and all, under a
/// new id, Message-ID, domain UID and MODSEQ.
fn clone_mail(source: &Row, mail_id: Uuid, message_id: &str, uid_domain: i64, modseq: i64) -> Insert {
    let mut insert = Insert::into("mails");
    for (column, value) in source.columns().iter().zip(source.values()) {
        insert = match column.as_str() {
            "mail_id" => insert.set(column, mail_id),
            "message_id" => insert.set(column, message_id),
            "uid_domain" => insert.set(column, uid_domain),
            "modseq" => insert.set(column, modseq),
            "updated" => insert.set(column, now()),
            _ => insert.set(column, value.clone()),
        };
    }
    insert.returning(["mail_id"])
}

// R-Q9, R-Q6, R-W7, R-W1, R-W11, R-X4: EXPUNGE, MOVE and COPY in a mapped box.
#[tokio::test(flavor = "multi_thread")]
async fn a_mapped_box_expunges_and_moves_by_its_own_uids() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    save_all(
        &db,
        &[
            Mail { deleted: true, ..mail(ann, 1) },
            Mail { read: true, answered: true, ..mail(ann, 2) },
            Mail { deleted: true, ..mail(ann, 3) },
            Mail { deleted: true, expunged: true, ..mail(ann, 4) },
            mail(ann, 5),
            Mail { deleted: true, ..mail(ann, 6) },
        ],
    )
    .await?;
    for n in 1..=5 {
        mapped(&db, ann, "Archive", uuid(n), 10 * n as i64).await?;
    }
    mapped(&db, ann, "INBOX/accounts/sales", uuid(6), 10).await?;
    let view = |mailbox: &str| mapped_view(ann, false, mailbox, Members::All).select("uid", col("x.uid"));

    // EXPUNGE in Archive. A lookup finds the box's deleted mails with their UIDs.
    let lookup =
        mapped_view(ann, false, "Archive", Members::All).filter(col("m.deleted").eq(true)).columns(["m.mail_id"]).select("uid", col("x.uid"));
    let looked_up = db.query(lookup).await?.map::<(Uuid, i64)>()?;
    assert_eq!(looked_up, [(uuid(1), 10), (uuid(3), 30)]);
    let modseq = next_modseq(&db, ann).await?;
    let mark = Update::rows(Query::table("mails").keys(looked_up.iter().map(|(mail_id, _)| [*mail_id])).filter(all([
        col("user_id").eq(ann),
        col("deleted").eq(true),
        col("expunged").eq(false),
    ])))
    .set("expunged", true)
    .set("updated", now())
    .set("modseq", modseq)
    .returning(["mail_id"]);
    assert_eq!(db.update(mark).await?.column::<Uuid>()?.into_iter().collect::<BTreeSet<_>>(), BTreeSet::from([uuid(1), uuid(3)]));
    assert_eq!(uids(&db, view("Archive")).await?, [20, 50]);
    // The deleted mail of another box was not Archive's to expunge, and
    // Archive's five mapping rows are all still there.
    assert_eq!(uids(&db, view("INBOX/accounts/sales")).await?, [10]);
    assert_eq!(db.query(Query::table("mail_mailbox_uid").index(BOX_UID, KeyRange::eq([val(ann), val("Archive")]))).await?.len(), 5);

    // UID MOVE 20,40,50,60 from Archive to Projects. The UIDs are first
    // resolved to live mails: 40 is expunged, 60 was never given out.
    let named = [20, 40, 50, 60].map(|uid| KeyRange::eq([val(ann), val("Archive"), val(uid)]));
    let sources = db
        .query(mapped_view(ann, false, "Archive", Members::All).index_ranges(BOX_UID, named).columns(["m.mail_id"]).select("uid", col("x.uid")))
        .await?
        .map::<(Uuid, i64)>()?;
    assert_eq!(sources, [(uuid(2), 20), (uuid(5), 50)]);
    // Per message: read the source whole, reserve its three numbers,
    // insert the clone, file it.
    let projects = box_seed(ann, &["Projects"]);
    let mut copies = Vec::new();
    for (n, (source_id, source_uid)) in sources.iter().enumerate() {
        let source = db
            .query_one(
                mapped_view(ann, false, "Archive", Members::All)
                    .index(BOX_UID, mapped_uids(ann, "Archive", *source_uid, *source_uid))
                    .columns(all_columns(&mails_table()).iter().map(|c| format!("m.{c}"))),
            )
            .await?;
        assert_eq!(source.get::<Uuid>("mail_id")?, *source_id);
        let uid_domain = next_domain_uid(&db, ann, false).await?;
        let uid_box = reserved(&db, reserve(ann, "account", "projects@example.com", false, &projects)).await?;
        let modseq = next_modseq(&db, ann).await?;
        let copy_id = uuid(100 + n as u128);
        let message_id = copy_message_id(&source.get::<String>("message_id")?, "Projects");
        db.insert(clone_mail(&source, copy_id, &message_id, uid_domain, modseq)).await?;
        assert_eq!(mapped(&db, ann, "Projects", copy_id, uid_box).await?, uid_box);
        copies.push((copy_id, uid_domain, uid_box, message_id));
    }
    assert_eq!(copies.iter().map(|(_, uid_domain, uid_box, _)| (*uid_domain, *uid_box)).collect::<Vec<_>>(), [(7, 1), (8, 2)]);
    // MOVE then retires the sources, under a new MODSEQ and without the
    // \Deleted re-check, and both sessions re-read their lists.
    let modseq = next_modseq(&db, ann).await?;
    let retire = Update::rows(Query::table("mails").keys(sources.iter().map(|(mail_id, _)| [*mail_id])))
        .set("expunged", true)
        .set("updated", now())
        .set("modseq", modseq)
        .returning(["mail_id"]);
    assert_eq!(db.update(retire).await?.column::<Uuid>()?.into_iter().collect::<BTreeSet<_>>(), BTreeSet::from([uuid(2), uuid(5)]));
    assert!(uids(&db, view("Archive")).await?.is_empty());
    assert_eq!(uids(&db, view("Projects")).await?, [1, 2]);

    // The clone is the source in everything but its identity and numbers.
    let (copy_id, _, _, message_id) = &copies[0];
    let clone = db.get("mails", [*copy_id]).await?.expect("the copy");
    let original = db.get("mails", [uuid(2)]).await?.expect("the source, kept as an expunged row");
    for column in ["user_id", "subject", "date", "text", "from_address", "to_text", "read", "answered", "thread_subject_key"] {
        assert_eq!(clone.value(column)?, original.value(column)?, "{column}");
    }
    assert_eq!(clone.get::<String>("message_id")?, *message_id);
    assert!(message_id.ends_with(".copy@server") && message_id.len() == 28);
    assert!(original.get::<bool>("expunged")? && !clone.get::<bool>("expunged")?);
    assert_eq!(db.query(Query::table("mails").index(MAILS_DATE, KeyRange::eq([ann]))).await?.len(), 8);

    // A COPY retried after a lost reply builds the same Message-ID, hits
    // the unique index, and merges: filing it again answers the UID the
    // first try filed.
    let retried =
        db.insert(clone_mail(&original, uuid(200), &copy_message_id(&original.get::<String>("message_id")?, "Projects"), 9, 9)).await.unwrap_err();
    assert!(retried.is_unique_violation() && retried.violates(MAILS_MESSAGE));
    assert_eq!(mapped(&db, ann, "Projects", *copy_id, 3).await?, 1);
    Ok(())
}

/// A body of three megabytes in which characters and bytes do not line up:
/// accented Latin letters (two bytes each), kana and kanji (three), an
/// emoji (four), on numbered lines so that no two stretches are alike.
fn large_body() -> String {
    let mut body = String::new();
    let mut line = 0;
    while body.len() < 3 * 1024 * 1024 {
        line += 1;
        body.push_str(&format!("{line:06} Grüße aus Köln, naïve café — 日本語のメール 📧\r\n"));
    }
    body
}

// R-T8, R-Q15, R-Q1: a body is measured, previewed and streamed without being loaded.
#[tokio::test(flavor = "multi_thread")]
async fn a_large_body_is_measured_and_read_in_pieces() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob) = (user(&db, "ann").await?, user(&db, "bob").await?);
    let body = large_body();
    let html = format!("<html><body><pre>{body}</pre></body></html>");
    let characters = body.chars().count() as i64;
    assert!(body.len() > 3 * 1024 * 1024 && (characters as usize) < body.len());
    save(&db, &Mail { text: body.clone(), html: html.clone(), ..mail(ann, 1) }).await?;

    // FETCH (RFC822.SIZE): the sizes come back, the bodies do not.
    let sizes = db
        .query_one(
            domain_view(ann, false, Members::Inbox)
                .index(MAILS_UID, domain_uids(ann, false, 1, 1))
                .columns(["m.mail_id"])
                .select("text_octets", col("m.text").octet_length())
                .select("html_octets", col("m.html").octet_length())
                .select("text_characters", col("m.text").length()),
        )
        .await?;
    assert_eq!(sizes.columns(), ["mail_id", "text_octets", "html_octets", "text_characters"]);
    assert_eq!(
        (sizes.get::<i64>("text_octets")?, sizes.get::<i64>("html_octets")?, sizes.get::<i64>("text_characters")?),
        (body.len() as i64, html.len() as i64, characters)
    );

    // A list row previews the first 400 characters, not bytes.
    let preview: String =
        db.query_one(Query::table("mails").key([uuid(1)]).select("text_prefix", col("text").left(400))).await?.get("text_prefix")?;
    assert_eq!(preview, body.chars().take(400).collect::<String>());

    // IMAP BODY[TEXT]: 12 000 characters per round trip until a short
    // chunk, each asked for by its character position, from 1.
    let characters_from = |user: Uuid, start: i64, length: i64| {
        Query::table("mails").key([uuid(1)]).filter(col("user_id").eq(user)).select("chunk", col("text").substr(start, length))
    };
    let mut text = String::new();
    let mut next = 1;
    loop {
        let chunk: String = db.query_one(characters_from(ann, next, 12_000)).await?.get("chunk")?;
        let got = chunk.chars().count() as i64;
        text.push_str(&chunk);
        next += got;
        if got < 12_000 {
            break;
        }
    }
    assert_eq!(next - 1, characters);
    assert!(text == body, "the chunks put together are the body");
    // A chunk is cut between characters wherever it starts, and past the
    // end there is the empty text, which is what ends the loop for a
    // body whose length is a multiple of the chunk.
    let middle: String = db.query_one(characters_from(ann, 1_000_000, 7)).await?.get("chunk")?;
    assert_eq!(middle, body.chars().skip(999_999).take(7).collect::<String>());
    let past: String = db.query_one(characters_from(ann, characters + 1, 12_000)).await?.get("chunk")?;
    assert_eq!(past, "");
    // The statement names the owner: for anyone else there is no row.
    assert!(db.query_opt(characters_from(bob, 1, 12_000)).await?.is_none());

    // A partial fetch `BODY[]<start.length>`: 48 KiB of the UTF-8 bytes
    // per round trip, from any byte, as bytes.
    let bytes_from = |column: &str, start: i64, length: i64| {
        Query::table("mails").key([uuid(1)]).filter(col("user_id").eq(ann)).select("chunk", col(column).substr_bytes(start, length))
    };
    let mut raw = Vec::new();
    loop {
        let chunk: Vec<u8> = db.query_one(bytes_from("html", raw.len() as i64 + 1, 49_152)).await?.get("chunk")?;
        raw.extend_from_slice(&chunk);
        if chunk.len() < 49_152 {
            break;
        }
    }
    assert!(raw == html.as_bytes(), "the byte chunks put together are the HTML body");
    // A seek into the third megabyte that lands inside a character.
    let inside = (2_000_000..).find(|byte| !body.is_char_boundary(*byte)).expect("a multi-byte character after the second megabyte");
    let piece: Vec<u8> = db.query_one(bytes_from("text", inside as i64 + 1, 100)).await?.get("chunk")?;
    assert_eq!(piece, &body.as_bytes()[inside..inside + 100]);
    let tail: Vec<u8> = db.query_one(bytes_from("text", body.len() as i64 - 9, 49_152)).await?.get("chunk")?;
    assert_eq!(tail, &body.as_bytes()[body.len() - 10..]);
    let past: Vec<u8> = db.query_one(bytes_from("text", body.len() as i64 + 1, 49_152)).await?.get("chunk")?;
    assert!(past.is_empty());

    // The HTTP body view does read the mail whole.
    let mut whole = db.query_one(Query::table("mails").key([uuid(1)]).filter(col("user_id").eq(ann)).limit(1)).await?;
    assert!(whole.take::<String>("text")? == body && whole.take::<String>("html")? == html);
    Ok(())
}

// R-T8, R-W1, R-W11, R-Q9: COPY reads the source row with its bodies and inserts it again.
#[tokio::test(flavor = "multi_thread")]
async fn copying_a_mail_with_a_large_body_clones_it_exactly() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    let body = large_body();
    save(&db, &Mail { text: body.clone(), html: body.to_uppercase(), read: true, cc: vec!["carol@example.com"], ..mail(ann, 1) }).await?;

    // The fetch as COPY issues it: the source by UID, every column.
    let source = db.query_one(domain_view(ann, false, Members::Inbox).index(MAILS_UID, domain_uids(ann, false, 1, 1))).await?;
    assert_eq!(source.columns(), all_columns(&mails_table()));
    let (uid_domain, modseq) = (next_domain_uid(&db, ann, false).await?, next_modseq(&db, ann).await?);
    let message_id = copy_message_id(&source.get::<String>("message_id")?, "Archive");
    let copied = db.insert(clone_mail(&source, uuid(2), &message_id, uid_domain, modseq)).await?;
    assert_eq!(copied.column::<Uuid>()?, [uuid(2)]);
    assert_eq!(mapped(&db, ann, "Archive", uuid(2), 1).await?, 1);

    // The two rows side by side, compared where they are: a join of the
    // table with itself, each side one row by primary key.
    let side_by_side = Query::table("mails")
        .alias("copy")
        .key([uuid(2)])
        .join(Query::table("mails").alias("source").key([uuid(1)]))
        .select("same_text", col("copy.text").eq(col("source.text")))
        .select("same_html", col("copy.html").eq(col("source.html")))
        .select("same_cc", col("copy.cc_address").eq(col("source.cc_address")))
        .select("text_octets", col("copy.text").octet_length())
        .select("uid_domain", col("copy.uid_domain"))
        .select("source_uid", col("source.uid_domain"));
    let compared = db.query_one(side_by_side).await?;
    assert!(compared.get::<bool>("same_text")? && compared.get::<bool>("same_html")? && compared.get::<bool>("same_cc")?);
    assert_eq!(
        (compared.get::<i64>("text_octets")?, compared.get::<i64>("uid_domain")?, compared.get::<i64>("source_uid")?),
        (body.len() as i64, 2, 1)
    );
    // And once more on this side of the wire.
    let mut copy = db.get("mails", [uuid(2)]).await?.expect("the copy");
    assert!(copy.take::<String>("text")? == body);
    assert!(copy.get::<bool>("read")?);
    assert_eq!(copy.get::<String>("message_id")?, message_id);

    // A retry of the same COPY is refused by the unique index and adds
    // no third row of three megabytes.
    let retried = db.insert(clone_mail(&source, uuid(3), &message_id, 3, 3)).await.unwrap_err();
    assert!(retried.is_unique_violation() && retried.violates(MAILS_MESSAGE));
    assert_eq!(db.query(Query::table("mails").index(MAILS_DATE, KeyRange::eq([ann])).columns(["mail_id"])).await?.len(), 2);
    Ok(())
}

const HEADER_COLUMNS: [&str; 20] = [
    "mail_id",
    "user_id",
    "subject",
    "date",
    "from_address",
    "from_text",
    "to_address",
    "to_text",
    "cc_address",
    "cc_text",
    "bcc_address",
    "bcc_text",
    "envelope_to",
    "read",
    "saved",
    "sent",
    "is_spam",
    "insight",
    "message_id",
    "in_reply_to",
];

/// What a list row carries. Never the bodies: 400 characters of the text;
/// and of a long References list its first and its last two entries.
fn headers(query: Query) -> Query {
    let references = || col("reference_ids");
    let trimmed = call(Func::JsonArray, [references().json([0]), references().json([-2]), references().json([-1])]);
    query
        .columns(HEADER_COLUMNS)
        .select("text_prefix", col("text").left(400))
        .select("reference_ids", when(references().json_len().gt(3), trimmed, references()))
}

/// The account test for received mail: one of the four recipient arrays
/// holds an entry with this address (`to_address @> '[{"address": ..}]' OR ..`).
fn addressed_to(address: &str) -> Expr {
    let entry = json!([{ "address": address }]);
    any(["to_address", "cc_address", "bcc_address", "envelope_to"].map(|column| col(column).json_contains(entry.clone())))
}

/// The account test for sent mail: the sender array holds the address.
fn sent_from(address: &str) -> Expr {
    col("from_address").json_contains(json!([{ "address": address }]))
}

/// What every list leaves out: expunged mail and drafts.
fn listed() -> Expr {
    col("expunged").eq(false).and(col("draft").eq(false))
}

/// The ids of a result, in its order.
fn mail_ids(rows: &Rows) -> Vec<Uuid> {
    column(rows, "mail_id")
}

// R-S9, R-Q13, R-Q5, R-Q4: the per-account list in its received, sent, saved and spam forms.
#[tokio::test(flavor = "multi_thread")]
async fn an_account_list_reads_one_address_of_the_recipients_index_newest_first() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob) = (user(&db, "ann").await?, user(&db, "bob").await?);
    let sales = "sales@example.com";
    let to_sales = |n: i64| Mail { to: vec![sales], ..mail(ann, n) };
    save_all(
        &db,
        &[
            to_sales(1),
            Mail { to: vec!["other@remote.test"], cc: vec![sales], ..mail(ann, 2) },
            Mail { to: vec!["other@remote.test"], bcc: vec![sales], ..mail(ann, 3) },
            Mail { to: vec!["list@remote.test"], envelope_to: vec![sales], ..mail(ann, 4) },
            Mail { to: vec!["info@example.com"], ..mail(ann, 5) },
            Mail { expunged: true, ..to_sales(6) },
            Mail { draft: true, ..to_sales(7) },
            Mail { is_spam: true, ..to_sales(8) },
            Mail { sent: true, from: vec![sales], to: vec!["x@remote.test"], ..mail(ann, 9) },
            Mail { cc: vec![sales, "info@example.com"], ..to_sales(10) },
            Mail { saved: true, ..to_sales(11) },
            Mail { sent: true, saved: true, from: vec![sales], to: vec!["y@remote.test"], ..mail(ann, 12) },
            Mail { to: vec!["Sales@Example.com"], ..mail(ann, 13) },
            Mail { to: vec![sales], ..mail(bob, 901) },
        ],
    )
    .await?;
    let received = |address: &str| Query::table("mails").index(MAILS_RECIPIENT, KeyRange::eq([val(ann), val(address)])).filter(listed());
    let ids = |n: &[u128]| n.iter().map(|n| uuid(*n)).collect::<Vec<_>>();

    // The inbox of one account: every mail that names the address in To,
    // Cc, Bcc or the envelope, newest first, straight off the index. The
    // full id list is capped at 10 000.
    let inbox = received(sales).filter(col("is_spam").eq(false)).columns(["mail_id"]).offset(0).limit(10_000);
    assert_eq!(mail_ids(&db.query(inbox.clone()).await?), ids(&[11, 10, 4, 3, 2, 1]));
    assert_eq!(mail_ids(&db.query(received("info@example.com").filter(col("is_spam").eq(false)).columns(["mail_id"])).await?), ids(&[10, 5]));
    // Addresses are compared by their bytes, as `@>` does.
    assert_eq!(mail_ids(&db.query(received("Sales@Example.com").columns(["mail_id"])).await?), ids(&[13]));
    assert!(db.query(received("nobody@example.com")).await?.is_empty());

    // The same list as SQL would write it: containment tested on every mail
    // of the user. Same rows, same order; the index only saves the scan.
    let scanned = Query::table("mails")
        .index(MAILS_DATE, KeyRange::eq([ann]))
        .filter(addressed_to(sales).and(listed()).and(col("is_spam").eq(false)))
        .columns(["mail_id"]);
    assert_eq!(mail_ids(&db.query(scanned).await?), ids(&[11, 10, 4, 3, 2, 1]));

    // A page by offset, with the number of rows the whole list has.
    let page = db.query(inbox.offset(2).limit(2).with_total()).await?;
    assert_eq!((mail_ids(&page), page.total()), (ids(&[4, 3]), Some(6)));

    // Junk, starred and sent variants of the same statement.
    let junk = received(sales).filter(col("is_spam").eq(true).and(col("sent").eq(false))).columns(["mail_id"]);
    assert_eq!(mail_ids(&db.query(junk).await?), ids(&[8]));
    let starred = received(sales).filter(col("saved").eq(true).and(col("is_spam").eq(false))).columns(["mail_id"]);
    assert_eq!(mail_ids(&db.query(starred).await?), ids(&[11]));
    let sent = Query::table("mails")
        .index(MAILS_SENDER, KeyRange::eq([val(ann), val(sales)]))
        .filter(listed().and(col("is_spam").eq(false)))
        .columns(["mail_id"]);
    assert_eq!(mail_ids(&db.query(sent).await?), ids(&[12, 9]));
    // Starred across both directions is `(sent OR received)`: two
    // indexes cannot be read as one range, so this one is the filter.
    let any_starred = Query::table("mails")
        .index(MAILS_DATE, KeyRange::eq([ann]))
        .filter(sent_from(sales).or(addressed_to(sales)).and(listed()).and(col("saved").eq(true)).and(col("is_spam").eq(false)))
        .columns(["mail_id"]);
    assert_eq!(mail_ids(&db.query(any_starred).await?), ids(&[12, 11]));
    Ok(())
}

/// A page of an account's list after a cursor, one row longer than the
/// page so the caller knows whether another follows. The cursor is the
/// last row's date, as text with its microseconds, and its id.
fn account_page(user: Uuid, address: &str, cursor: Option<&(String, String)>, size: u64) -> Query {
    let after = match cursor {
        Some((date, mail_id)) => Bound::excluded([date.clone(), mail_id.clone()]),
        None => Bound::Unbounded,
    };
    Query::table("mails")
        .index(MAILS_RECIPIENT, KeyRange::within([val(user), val(address)], after, Bound::Unbounded))
        .filter(listed().and(col("is_spam").eq(false)))
        .columns(["mail_id"])
        .select("cursor_date", col("date").cast(Type::Text))
        .limit(size + 1)
}

// R-Q4, R-T2, R-T3, R-Q22: keyset paging (`(date, mail_id) < (cursor_date, cursor_id)` on microsecond timestamps with a text cursor).
#[tokio::test(flavor = "multi_thread")]
async fn keyset_pages_lose_and_repeat_nothing() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    let sales = "sales@example.com";
    // Dates a microsecond apart, three mails in the very same microsecond,
    // and two more sharing another.
    let offsets = [0, 1, 1, 1, 2, 1_000_000, 1_000_001, -1, 2, 3];
    let mails: Vec<Mail> = offsets
        .iter()
        .enumerate()
        .map(|(i, offset)| Mail { date: Timestamp(at(0).0 + offset), to: vec![sales], ..mail(ann, i as i64 + 1) })
        .collect();
    save_all(&db, &mails).await?;
    // Newest first; within one instant, the higher id first.
    let mut expected: Vec<(Timestamp, Uuid)> = mails.iter().map(|m| (m.date, m.mail_id)).collect();
    expected.sort_by(|a, b| b.cmp(a));
    assert_eq!(expected.iter().map(|(_, id)| id.0[15]).collect::<Vec<_>>(), [7, 6, 10, 9, 5, 4, 3, 2, 1, 8]);

    let mut seen = Vec::new();
    let mut cursor: Option<(String, String)> = None;
    let mut pages = 0;
    loop {
        let rows = db.query(account_page(ann, sales, cursor.as_ref(), 3)).await?.map::<(Uuid, String)>()?;
        pages += 1;
        for (mail_id, date) in rows.iter().take(3) {
            // The text form of the date carries every microsecond.
            seen.push((Timestamp::parse(date).expect("the cursor date reads back as a timestamp"), *mail_id));
        }
        if rows.len() <= 3 {
            break;
        }
        let (mail_id, date) = &rows[2];
        cursor = Some((date.clone(), mail_id.to_string()));
        if pages == 1 {
            // A delivery while the client pages must not shift the pages still to come.
            save(&db, &Mail { date: at(60), to: vec![sales], ..mail(ann, 99) }).await?;
        }
    }
    // Ten mails in pages of three: no row twice, none missing, order kept
    // across the page that ends inside the three simultaneous mails.
    assert_eq!(pages, 4);
    assert_eq!(seen, expected);
    // Only a new first page shows the new mail.
    assert_eq!(mail_ids(&db.query(account_page(ann, sales, None, 3)).await?), [uuid(99), uuid(7), uuid(6), uuid(10)]);

    // The cursor may equally be given as values, and a cursor on the
    // last row leaves nothing.
    let (last_date, last_id) = expected[9];
    let after_last = Query::table("mails")
        .index(MAILS_RECIPIENT, KeyRange::within([val(ann), val(sales)], Bound::excluded([val(last_date), val(last_id)]), Bound::Unbounded));
    assert!(db.query(after_last).await?.is_empty());
    let (date, id) = expected[4];
    let after_fifth = Query::table("mails")
        .index(MAILS_RECIPIENT, KeyRange::within([val(ann), val(sales)], Bound::excluded([val(date), val(id)]), Bound::Unbounded))
        .columns(["mail_id"]);
    assert_eq!(mail_ids(&db.query(after_fifth).await?), expected[5..].iter().map(|(_, id)| *id).collect::<Vec<_>>());
    Ok(())
}

// R-Q6, R-Q13, R-Q15, R-W2: list rows are fetched for 500 ids per statement.
#[tokio::test(flavor = "multi_thread")]
async fn header_rows_are_fetched_500_ids_at_a_time_without_their_bodies() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob) = (user(&db, "ann").await?, user(&db, "bob").await?);
    let long = "héllo wörld, ".repeat(100);
    let mut mails: Vec<Mail> = (1..=600).map(|n| mail(ann, n)).collect();
    mails[2].text = long.clone();
    mails[2].reference_ids = Some(json!(["<a@x>", "<b@x>", "<c@x>", "<d@x>", "<e@x>", "<f@x>"]));
    mails[3].reference_ids = Some(json!(["<a@x>", "<b@x>", "<c@x>"]));
    mails[4].reference_ids = Some(json!(["<a@x>", "<b@x>", "<c@x>", "<d@x>"]));
    mails[6].expunged = true;
    // Six hundred rows in one statement.
    save_all(&db, &mails).await?;
    save(&db, &mail(bob, 9001)).await?;

    // The list query gave ids, newest first; among them here one of
    // another user's mails and one that does not exist.
    let mut wanted: Vec<Uuid> = (1..=600).rev().map(uuid).collect();
    wanted.insert(10, uuid(9001));
    wanted.push(uuid(404_404));
    let mut rows = Vec::new();
    let mut statements = 0;
    for batch in wanted.chunks(500) {
        let by_id = Query::table("mails").keys(batch.iter().map(|id| [*id])).filter(col("user_id").eq(ann).and(col("expunged").eq(false)));
        rows.extend(db.query(headers(by_id)).await?);
        statements += 1;
    }
    assert_eq!((statements, rows.len()), (2, 599));
    // Rows come in the order the ids were given, so the application need not
    // re-sort; the expunged mail, the foreign one and the unknown one are absent.
    let got = rows.iter().map(|row| row.get::<Uuid>("mail_id")).collect::<DbResult<Vec<_>>>()?;
    let hidden = [uuid(7), uuid(9001), uuid(404_404)];
    assert_eq!(got, wanted.iter().copied().filter(|id| !hidden.contains(id)).collect::<Vec<_>>());

    let row = |n: u128| rows.iter().find(|row| row.get::<Uuid>("mail_id").ok() == Some(uuid(n))).expect("fetched");
    let mut names: Vec<&str> = HEADER_COLUMNS.to_vec();
    names.extend(["text_prefix", "reference_ids"]);
    assert_eq!(row(3).columns(), names, "no `text`, no `html`");
    // 400 characters of a text of 1300, counted in characters.
    assert_eq!(row(3).get::<String>("text_prefix")?, long.chars().take(400).collect::<String>());
    assert_eq!(row(1).get::<String>("text_prefix")?, "Body of mail 1");
    // Six references are cut to the first and the last two; four as
    // well; three are left alone; none stays NULL.
    assert_eq!(row(3).get::<Json>("reference_ids")?, json!(["<a@x>", "<e@x>", "<f@x>"]));
    assert_eq!(row(5).get::<Json>("reference_ids")?, json!(["<a@x>", "<c@x>", "<d@x>"]));
    assert_eq!(row(4).get::<Json>("reference_ids")?, json!(["<a@x>", "<b@x>", "<c@x>"]));
    assert!(row(1).is_null("reference_ids")?);
    assert_eq!(row(600).get::<Json>("to_address")?.to_string(), r#"[{"name":"ann","address":"ann@example.com"}]"#);
    Ok(())
}

// R-Q8, R-O3, R-Q4, R-Q13, R-X4: delta sync of a list (`updated >` a watermark) on the server's clock.
#[tokio::test(flavor = "multi_thread")]
async fn delta_sync_reads_what_changed_since_a_watermark_from_the_servers_clock() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    let sales = "sales@example.com";
    let to_sales = |n: i64| Mail { to: vec![sales], ..mail(ann, n) };
    save_all(
        &db,
        &[
            to_sales(1),
            to_sales(2),
            to_sales(3),
            to_sales(4),
            Mail { to: vec!["info@example.com"], ..mail(ann, 5) },
            Mail { is_spam: true, ..to_sales(6) },
        ],
    )
    .await?;
    let stale = rewind_updated(&db, ann).await?;

    // The watermark handed to the client is the server's clock two seconds
    // back, so a mutation committing right now is not missed by the next
    // poll. It is computed where `updated` is stamped, here beside a row
    // that is certain to be there.
    let before = db.now().await?;
    let as_of: Timestamp = db.query_one(Query::table("users").key([ann]).select("as_of", now() - seconds(2))).await?.get("as_of")?;
    let after = db.now().await?;
    assert!(before.plus_secs(-2) <= as_of && as_of <= after.plus_secs(-2));
    assert!(stale < as_of);

    // The delta of the account's inbox: rows stamped after the watermark,
    // through the (user_id, updated) index, then the list's own tests.
    let since =
        |watermark: Timestamp| Query::table("mails").index(MAILS_UPDATED, KeyRange::within([ann], Bound::excluded([watermark]), Bound::Unbounded));
    let changed = |watermark: Timestamp| {
        headers(
            since(watermark)
                .filter(addressed_to(sales).and(listed()).and(col("is_spam").eq(false)))
                .order_by(col("date").desc())
                .order_by(col("mail_id").desc()),
        )
    };
    // The mails the client should drop from this list.
    let evicted = |watermark: Timestamp| {
        since(watermark).filter(addressed_to(sales).and(col("expunged").eq(true).or(col("is_spam").eq(true)))).columns(["mail_id"])
    };
    assert!(db.query(changed(as_of)).await?.is_empty());
    assert!(db.query(evicted(as_of)).await?.is_empty());

    // Each of these statements stamps `updated` from the same clock.
    db.update(set_flag("read", uuid(1), ann, true, 2)).await?;
    db.update(set_flag("saved", uuid(3), ann, true, 3)).await?;
    db.update(Update::table("mails").key([uuid(2)]).set("expunged", true).set("updated", now()).set("modseq", 4)).await?;
    db.update(Update::table("mails").key([uuid(4)]).set("is_spam", true).set("updated", now()).set("modseq", 5)).await?;
    db.update(set_flag("read", uuid(5), ann, true, 6)).await?;
    save(&db, &to_sales(7)).await?;
    // Caching a size is not a change to sync.
    db.update(Update::table("mails").key([uuid(6)]).set("rfc822_size", 1024)).await?;

    let delta = db.query(changed(as_of)).await?;
    assert_eq!(mail_ids(&delta), [uuid(7), uuid(3), uuid(1)]);
    assert_eq!(column::<bool>(&delta, "read"), [false, false, true]);
    assert_eq!(column::<bool>(&delta, "saved"), [false, true, false]);
    assert_eq!(mail_ids(&db.query(evicted(as_of)).await?).into_iter().collect::<BTreeSet<_>>(), BTreeSet::from([uuid(2), uuid(4)]));
    // The Junk list's evictions are the mirror image: expunged, or no longer spam.
    let left_junk = since(as_of).filter(addressed_to(sales).and(col("expunged").eq(true).or(col("is_spam").eq(false)))).columns(["mail_id"]);
    assert_eq!(mail_ids(&db.query(left_junk).await?).into_iter().collect::<BTreeSet<_>>(), BTreeSet::from([uuid(1), uuid(2), uuid(3), uuid(7)]));

    // A watermark read now is not before any of those stamps: polling
    // with it finds nothing, and nothing was stamped by another clock.
    let now_on_server = db.now().await?;
    assert!(db.query(changed(now_on_server)).await?.is_empty());
    assert!(db.query(evicted(now_on_server)).await?.is_empty());
    assert!(updated_of(&db, uuid(7)).await? > as_of && updated_of(&db, uuid(7)).await? <= now_on_server);
    assert_eq!(updated_of(&db, uuid(6)).await?, stale);
    Ok(())
}

// R-Q14, R-Q4, R-Q11, R-Q13: the unified inbox (any of the user's addresses among the recipients).
#[tokio::test(flavor = "multi_thread")]
async fn the_unified_list_filters_by_any_of_the_users_addresses() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    let (sales, info) = ("sales@example.com", "info@example.com");
    save_all(
        &db,
        &[
            Mail { to: vec![sales], read: true, ..mail(ann, 1) },
            Mail { to: vec![info], ..mail(ann, 2) },
            Mail { to: vec!["other@remote.test"], cc: vec![info], read: true, ..mail(ann, 3) },
            Mail { to: vec!["list@remote.test"], ..mail(ann, 4) },
            Mail { to: vec!["list@remote.test"], envelope_to: vec![sales], ..mail(ann, 5) },
            Mail { to: vec![sales], is_spam: true, ..mail(ann, 6) },
            Mail { to: vec![sales], draft: true, ..mail(ann, 7) },
            Mail { to: vec![info], expunged: true, ..mail(ann, 8) },
            Mail { sent: true, from: vec![sales], to: vec!["x@remote.test"], ..mail(ann, 9) },
            Mail { to: vec![], ..mail(ann, 10) },
            Mail { to: vec![sales, info], bcc: vec![sales], ..mail(ann, 11) },
        ],
    )
    .await?;
    let stale = rewind_updated(&db, ann).await?;
    // In SQL an EXISTS over the unnested recipient arrays with an array
    // parameter; here "the two lists share an item". A mail whose arrays
    // are all NULL has an empty list, and is simply not matched.
    let mine = || recipients().list_overlaps(vec![sales, info]);
    let unified = || Query::table("mails").index(MAILS_DATE, KeyRange::eq([ann])).filter(mine().and(listed()).and(col("is_spam").eq(false)));

    // Unpaged: unread first, then newest first; false sorts before true.
    let front_page = unified().order_by(col("read")).order_by(col("date").desc()).columns(["mail_id"]).offset(0).limit(10_000);
    assert_eq!(mail_ids(&db.query(front_page).await?), [uuid(11), uuid(5), uuid(2), uuid(3), uuid(1)]);
    // In date order the index's own order serves, and a limit stops the read.
    assert_eq!(mail_ids(&db.query(unified().columns(["mail_id"]).limit(2)).await?), [uuid(11), uuid(5)]);

    // Keyset pages over the user's date index, the filter applied to what the range gives.
    let page = |cursor: Option<(Timestamp, Uuid)>| {
        let after = cursor.map_or(Bound::Unbounded, |(date, mail_id)| Bound::excluded([val(date), val(mail_id)]));
        Query::table("mails")
            .index(MAILS_DATE, KeyRange::within([ann], after, Bound::Unbounded))
            .filter(mine().and(listed()).and(col("is_spam").eq(false)))
            .columns(["mail_id", "date"])
            .limit(3)
    };
    let first = db.query(page(None)).await?.map::<(Uuid, Timestamp)>()?;
    assert_eq!(first, [(uuid(11), at(11)), (uuid(5), at(5)), (uuid(3), at(3))]);
    // The client asks for more from the second row on.
    let (cursor_id, cursor_date) = first[1];
    let second = db.query(page(Some((cursor_date, cursor_id)))).await?.map::<(Uuid, Timestamp)>()?;
    assert_eq!(second, [(uuid(3), at(3)), (uuid(2), at(2)), (uuid(1), at(1))]);

    // The unified delta (`updated >` a watermark) and its evictions.
    let as_of = db.now().await?.plus_secs(-2);
    assert!(stale < as_of);
    let modseq = next_modseq(&db, ann).await?;
    db.update(set_flag("read", uuid(2), ann, true, modseq)).await?;
    db.update(Update::table("mails").key([uuid(1)]).set("is_spam", true).set("updated", now())).await?;
    db.update(Update::table("mails").key([uuid(5)]).set("expunged", true).set("updated", now())).await?;
    db.update(Update::table("mails").key([uuid(4)]).set("read", true).set("updated", now())).await?;
    let since = || Query::table("mails").index(MAILS_UPDATED, KeyRange::within([ann], Bound::excluded([as_of]), Bound::Unbounded));
    let changed = since().filter(mine().and(listed()).and(col("is_spam").eq(false))).columns(["mail_id"]);
    assert_eq!(mail_ids(&db.query(changed).await?), [uuid(2)]);
    let evicted = since().filter(mine().and(col("expunged").eq(true).or(col("is_spam").eq(true)))).columns(["mail_id"]);
    assert_eq!(mail_ids(&db.query(evicted).await?).into_iter().collect::<BTreeSet<_>>(), BTreeSet::from([uuid(1), uuid(5)]));
    Ok(())
}

/// One line of the account sidebar: address, mails, unread, starred, newest.
type Account = (String, i64, i64, i64, Timestamp);

/// The addresses a user has mail for, with their counters, the most
/// recently used first. SQL would unnest the address arrays in a CTE with
/// DISTINCT and group that; `each` shows a mail once under each distinct
/// address of the list, which is the same thing.
fn sidebar(mails: Query, addresses: Expr, placement: Expr, domain: Option<&str>) -> Query {
    let mut query = mails.each("address", addresses).filter(listed().and(placement)).filter(col("address").is_not_null());
    if let Some(domain) = domain {
        // `address ILIKE '%@' || domain`, with the domain left unescaped.
        query = query.filter(col("address").ilike(concat([val("%@"), val(domain)])));
    }
    query
        .group_by([col("address")])
        .select("address", col("address"))
        .select("count", count())
        // `SUM(CASE WHEN read = FALSE THEN 1 ELSE 0 END)` as SQL says it,
        // and the same idea for `saved` as a filtered count.
        .select("unread", sum(when(col("read").eq(false), 1, 0)))
        .select("saved_count", count().filter(col("saved").eq(true)))
        .select("latest", max(col("date")))
        .order_by(col("latest").desc())
}

// R-Q12, R-Q11, R-Q7, R-Q17: the account sidebar, read several times per page load and for every IMAP LIST, SELECT and STATUS.
#[tokio::test(flavor = "multi_thread")]
async fn the_sidebar_counts_each_mail_once_under_each_address_it_names() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob) = (user(&db, "ann").await?, user(&db, "bob").await?);
    let (sales, info) = ("sales@example.com", "info@example.com");
    save_all(
        &db,
        &[
            Mail { to: vec![sales], subject: "Invoice for March".into(), ..mail(ann, 1) },
            Mail { to: vec![sales], cc: vec![sales], envelope_to: vec![sales], read: true, saved: true, ..mail(ann, 2) },
            Mail { to: vec![info], cc: vec![sales], subject: "Invoice reminder".into(), ..mail(ann, 3) },
            Mail { to: vec![info], envelope_to: vec![info], read: true, subject: "Paid invoices".into(), ..mail(ann, 4) },
            Mail { to: vec!["ann@other.org"], ..mail(ann, 5) },
            Mail { to: vec![sales], is_spam: true, ..mail(ann, 6) },
            Mail { to: vec![sales], draft: true, ..mail(ann, 7) },
            Mail { to: vec![info], expunged: true, ..mail(ann, 8) },
            Mail { sent: true, read: true, from: vec![sales], to: vec!["x@remote.test"], ..mail(ann, 9) },
            Mail { to: vec!["Info@EXAMPLE.com"], ..mail(ann, 10) },
            Mail { to: vec![], ..mail(ann, 11) },
            Mail { to: vec![sales], ..mail(bob, 901) },
        ],
    )
    .await?;
    let mails_of = |user: Uuid| Query::table("mails").index(MAILS_DATE, KeyRange::eq([user]));
    let not_spam = || col("is_spam").eq(false);
    let line = |address: &str, mails: i64, unread: i64, saved: i64, latest: i64| (address.to_string(), mails, unread, saved, at(latest));

    // The accounts that received mail, in the user's own domain. Mail 2
    // names sales three times and counts once; mail 3 counts for both
    // addresses. The domain test ignores case; the grouping does not.
    let received = db.query(sidebar(mails_of(ann), recipients(), not_spam(), Some("example.com"))).await?.map::<Account>()?;
    assert_eq!(received, [line("Info@EXAMPLE.com", 1, 1, 0, 10), line(info, 2, 1, 0, 4), line(sales, 3, 2, 1, 3)]);
    // Without the domain every recipient address is a line; the mail
    // without any recipient is under none.
    let everyone = db.query(sidebar(mails_of(ann), recipients(), not_spam(), None)).await?.map::<Account>()?;
    assert_eq!(
        everyone.iter().map(|account| account.0.as_str()).collect::<Vec<_>>(),
        ["Info@EXAMPLE.com", "x@remote.test", "ann@other.org", info, sales]
    );
    assert_eq!(everyone.iter().map(|account| account.1).sum::<i64>(), 8);

    // The Junk lines, and the lines for sent mail by sender address.
    let junk = db
        .query(sidebar(mails_of(ann), recipients(), col("is_spam").eq(true).and(col("sent").eq(false)), Some("example.com")))
        .await?
        .map::<Account>()?;
    assert_eq!(junk, [line(sales, 1, 1, 0, 6)]);
    let senders =
        db.query(sidebar(mails_of(ann), col("from_address").json_pluck("address"), not_spam(), Some("example.com"))).await?.map::<Account>()?;
    assert_eq!(senders, [line(sales, 1, 0, 0, 9)]);
    assert_eq!(db.query(sidebar(mails_of(bob), recipients(), not_spam(), Some("example.com"))).await?.map::<Account>()?, [line(sales, 1, 1, 0, 901)]);

    // The same lines for the mails a search finds.
    let hits = Query::table("mails").search(MAILS_SEARCH, [ann], "invoice");
    let searched = db.query(sidebar(hits, recipients(), not_spam(), Some("example.com"))).await?.map::<Account>()?;
    assert_eq!(searched, [line(info, 2, 1, 0, 4), line(sales, 2, 2, 0, 3)]);
    Ok(())
}

// R-Q11, R-Q6: the unread badge sent with a web push.
#[tokio::test(flavor = "multi_thread")]
async fn the_unread_badge_is_counted_per_user_in_one_statement() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob, carol) = (user(&db, "ann").await?, user(&db, "bob").await?, user(&db, "carol").await?);
    save_all(
        &db,
        &[
            mail(ann, 1),
            Mail { read: true, ..mail(ann, 2) },
            mail(ann, 3),
            Mail { is_spam: true, ..mail(ann, 4) },
            Mail { draft: true, ..mail(ann, 5) },
            Mail { expunged: true, ..mail(ann, 6) },
            Mail { sent: true, ..mail(ann, 7) },
            Mail { read: true, ..mail(bob, 901) },
        ],
    )
    .await?;
    // `user_id IN (..) AND sent = FALSE`: one range of the UID index per user.
    let badge = |users: &[Uuid]| {
        Query::table("mails")
            .index_ranges(MAILS_UID, users.iter().map(|user| KeyRange::eq([val(*user), val(false)])))
            .filter(all([col("is_spam").eq(false), col("expunged").eq(false), col("draft").eq(false)]))
            .group_by([col("user_id")])
            .select("user_id", col("user_id"))
            .select("unread_count", count().filter(col("read").eq(false)))
            .select("latest", max(col("date")))
    };
    let badges: BTreeMap<Uuid, (i64, Timestamp)> = db
        .query(badge(&[ann, bob, carol]))
        .await?
        .map::<(Uuid, i64, Timestamp)>()?
        .into_iter()
        .map(|(user, unread, latest)| (user, (unread, latest)))
        .collect();
    assert_eq!(badges.get(&ann), Some(&(2, at(3))));
    // A user with nothing unread has a line with zero; a user with no mails has none.
    assert_eq!(badges.get(&bob), Some(&(0, at(901))));
    assert_eq!(badges.get(&carol), None);
    assert!(db.query(badge(&[carol])).await?.is_empty());
    Ok(())
}

/// `ts_headline('english', column, query, 'StartSel=<em>, StopSel=</em>, MaxWords=50, MinWords=10')`.
fn highlight(column: &str, words: &str) -> Expr {
    call(Func::Headline, [col(column), val(words), val("english"), val("<em>"), val("</em>"), val(50), val(10)])
}

/// The user's mails that hold every word, best match first and newest
/// first among equals, with excerpts; at most a thousand.
fn full_text(user: Uuid, words: &str) -> Query {
    headers(Query::table("mails").search(MAILS_SEARCH, [user], words).filter(listed()))
        .select("rank", Expr::Rank)
        .select("subject_highlight", highlight("subject", words))
        .select("text_highlight", highlight("text", words))
        .order_by(Expr::Rank.desc())
        .order_by(col("date").desc())
        .limit(1000)
}

// R-Q17, R-Q15: full-text search (English stemming, all words required, rank then date, `<em>` excerpts).
#[tokio::test(flavor = "multi_thread")]
async fn full_text_search_stems_ranks_and_highlights() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    let filler = "lorem ipsum dolor sit amet ".repeat(60);
    let reminder = |n: i64| Mail { subject: "Payment reminder".into(), text: "One invoice is overdue.".into(), ..mail(ann, n) };
    save_all(
        &db,
        &[
            Mail {
                subject: "Invoice overdue".into(),
                text: "Your invoices are overdue. Please pay the invoice today, or the overdue fee applies to every invoice.".into(),
                ..mail(ann, 1)
            },
            Mail { subject: "Lunch".into(), text: "The invoice for lunch is attached.".into(), ..mail(ann, 2) },
            Mail { subject: "Overdue library books".into(), text: "Please return them.".into(), ..mail(ann, 3) },
            reminder(4),
            reminder(5),
            Mail { subject: "Hello".into(), from: vec!["zebulon@remote.test"], text: "Nothing of note.".into(), ..mail(ann, 6) },
            Mail { subject: "Travel notes".into(), text: format!("{filler}A zeppelin crossed the lake. {filler}"), ..mail(ann, 7) },
        ],
    )
    .await?;
    let found = |rows: &Rows| mail_ids(rows).into_iter().collect::<BTreeSet<_>>();
    let ids = |n: &[u128]| n.iter().map(|n| uuid(*n)).collect::<BTreeSet<_>>();

    // Every word must be there, in any of the four indexed columns. Mail 1
    // says both words more often than the two reminders, which say the
    // same as each other and so fall back on their dates.
    let both = db.query(full_text(ann, "invoice overdue")).await?;
    assert_eq!(mail_ids(&both), [uuid(1), uuid(5), uuid(4)]);
    let ranks: Vec<f64> = column(&both, "rank");
    assert!(ranks[0] > ranks[1] && ranks[1] == ranks[2] && ranks[2] > 0.0, "{ranks:?}");
    // The list columns ride along; the bodies do not.
    assert_eq!(both.columns().len(), HEADER_COLUMNS.len() + 5);
    assert_eq!(column::<String>(&both, "text_prefix")[1], "One invoice is overdue.");

    // Words are matched by their stem, in either direction, without
    // regard to case; common words in the query ask for nothing.
    assert_eq!(found(&db.query(full_text(ann, "invoices")).await?), ids(&[1, 2, 4, 5]));
    assert_eq!(found(&db.query(full_text(ann, "INVOICE")).await?), ids(&[1, 2, 4, 5]));
    assert_eq!(found(&db.query(full_text(ann, "the invoices of a")).await?), ids(&[1, 2, 4, 5]));
    assert_eq!(found(&db.query(full_text(ann, "Overdue BOOK")).await?), ids(&[3]));
    // One word from the subject and one from the body: still one mail.
    assert_eq!(found(&db.query(full_text(ann, "lunch invoice")).await?), ids(&[2]));
    assert!(db.query(full_text(ann, "overdue lunch")).await?.is_empty());
    // The sender and the recipients are searched too.
    assert_eq!(found(&db.query(full_text(ann, "zebulon")).await?), ids(&[6]));
    assert_eq!(db.query(full_text(ann, "ann example")).await?.len(), 7);
    // The limit cuts after the ordering.
    assert_eq!(mail_ids(&db.query(full_text(ann, "invoice overdue").limit(2)).await?), [uuid(1), uuid(5)]);

    // Excerpts mark each matched word as it stands in the text.
    let best = both.first().expect("mail 1");
    let (subject, text): (String, String) = (best.get("subject_highlight")?, best.get("text_highlight")?);
    assert!(subject.contains("<em>Invoice</em>") && subject.contains("<em>overdue</em>"), "{subject}");
    assert!(text.contains("<em>invoices</em>") && text.contains("<em>overdue</em>") && text.contains("<em>invoice</em>"), "{text}");
    // A column without a match has no marker, and the application then shows no excerpt for it.
    let reminder_subject: String = both.iter().nth(1).expect("mail 5").get("subject_highlight")?;
    assert!(!reminder_subject.contains("<em>"), "{reminder_subject}");
    // Of a long body the excerpt is the neighbourhood of the match: at most fifty words.
    let long = db.query_one(full_text(ann, "zeppelins")).await?;
    let excerpt: String = long.get("text_highlight")?;
    assert!(excerpt.contains("<em>zeppelin</em>"), "{excerpt}");
    assert!(excerpt.split_whitespace().count() <= 50, "{excerpt}");
    Ok(())
}

// R-Q17, R-X4, R-W10: which mails a full-text search may show; the index follows every write, as a trigger-kept tsvector column would.
#[tokio::test(flavor = "multi_thread")]
async fn full_text_search_shows_spam_but_not_drafts_or_expunged_mail() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob) = (user(&db, "ann").await?, user(&db, "bob").await?);
    let contract = |user: Uuid, n: i64| Mail { subject: "Signed contracts".into(), ..mail(user, n) };
    save_all(
        &db,
        &[
            contract(ann, 1),
            Mail { is_spam: true, ..contract(ann, 2) },
            Mail { draft: true, ..contract(ann, 3) },
            Mail { expunged: true, ..contract(ann, 4) },
            Mail { sent: true, ..contract(ann, 5) },
            contract(bob, 901),
        ],
    )
    .await?;
    let hits = |user: Uuid| full_text(user, "contract signing");

    // Spam is found and flagged for the page to mark; sent mail is
    // found; drafts and expunged mail are not; nor is anyone else's.
    let rows = db.query(hits(ann)).await?;
    assert_eq!(mail_ids(&rows), [uuid(5), uuid(2), uuid(1)], "equal matches, newest first");
    assert_eq!(column::<bool>(&rows, "is_spam"), [false, true, false]);
    assert_eq!(mail_ids(&db.query(hits(bob)).await?), [uuid(901)]);

    // The index is part of each write: a delivery is found by the next
    // statement, from any connection.
    let other = second_instance(&db).await?;
    save(&db, &contract(ann, 6)).await?;
    assert_eq!(mail_ids(&other.query(hits(ann)).await?), [uuid(6), uuid(5), uuid(2), uuid(1)]);
    // An expunge is an update of an unindexed column, and hides the mail
    // through the filter; a hard delete takes the mail's words with it.
    db.update(Update::table("mails").key([uuid(6)]).set("expunged", true).set("updated", now())).await?;
    db.delete(Delete::from("mails").key([uuid(5)])).await?;
    assert_eq!(mail_ids(&other.query(hits(ann)).await?), [uuid(2), uuid(1)]);
    // Words that were rewritten are found under the new text only.
    db.update(Update::table("mails").key([uuid(1)]).set("subject", "Signed agreements")).await?;
    assert_eq!(mail_ids(&db.query(hits(ann)).await?), [uuid(2)]);
    assert_eq!(mail_ids(&db.query(full_text(ann, "agreement")).await?), [uuid(1)]);
    Ok(())
}

const THREAD_COLUMNS: [&str; 20] = [
    "mail_id",
    "message_id",
    "in_reply_to",
    "reference_ids",
    "subject",
    "date",
    "from_address",
    "from_text",
    "to_address",
    "to_text",
    "cc_address",
    "cc_text",
    "bcc_address",
    "bcc_text",
    "envelope_to",
    "read",
    "saved",
    "sent",
    "is_spam",
    "insight",
];

/// A conversation row. All of References this time, and the same 400
/// characters of text.
fn thread_row(query: Query) -> Query {
    query.columns(THREAD_COLUMNS).select("text_prefix", col("text").left(400))
}

/// What no conversation shows: expunged mail, drafts, IMAP copies, and
/// spam unless the mail the conversation is opened from is spam itself.
fn threadable(with_spam: bool) -> Expr {
    let shown = listed().and(col("message_id").not_like("%.copy@server"));
    if with_spam {
        shown
    } else {
        shown.and(col("is_spam").eq(false))
    }
}

/// Each Message-ID as headers carry it and without its angle brackets:
/// old rows hold either form.
fn both_forms(ids: &[&str]) -> Vec<String> {
    ids.iter().flat_map(|id| [id.trim_matches(['<', '>']).to_string(), id.to_string()]).collect()
}

// R-Q6, R-Q13, R-S9, R-Q2, R-Q4: a conversation is gathered by following Message-ID, In-Reply-To and References.
#[tokio::test(flavor = "multi_thread")]
async fn a_conversation_follows_message_ids_replies_and_references() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob) = (user(&db, "ann").await?, user(&db, "bob").await?);
    let reply = |user: Uuid, n: i64, to: &str| Mail { in_reply_to: Some(to.to_string()), ..mail(user, n) };
    save_all(
        &db,
        &[
            Mail { message_id: "<root@x.test>".into(), subject: "Plan".into(), text: "é".repeat(500), ..mail(ann, 1) },
            Mail { message_id: "<r1@x.test>".into(), reference_ids: Some(json!(["<root@x.test>"])), ..reply(ann, 2, "<root@x.test>") },
            Mail { message_id: "<r2@x.test>".into(), reference_ids: Some(json!(["<root@x.test>", "<r1@x.test>"])), ..reply(ann, 3, "<r1@x.test>") },
            Mail { message_id: "<r3@x.test>".into(), reference_ids: Some(json!(["<elsewhere@x.test>", "<r2@x.test>"])), ..mail(ann, 4) },
            mail(ann, 5),
            Mail { message_id: "00000000000000ab.copy@server".into(), ..reply(ann, 6, "<root@x.test>") },
            Mail { draft: true, ..reply(ann, 7, "<root@x.test>") },
            Mail {
                expunged: true,
                message_id: "<gone@x.test>".into(),
                reference_ids: Some(json!(["<root@x.test>"])),
                ..reply(ann, 8, "<root@x.test>")
            },
            Mail { is_spam: true, ..reply(ann, 9, "<root@x.test>") },
            reply(ann, 10, "root@x.test"),
            reply(bob, 901, "<root@x.test>"),
        ],
    )
    .await?;

    // The mail the conversation is opened from.
    let seed =
        |mail_id: Uuid, user: Uuid| thread_row(Query::table("mails").key([mail_id]).filter(col("user_id").eq(user).and(col("expunged").eq(false))));
    let opened = db.query_one(seed(uuid(2), ann)).await?;
    assert_eq!(opened.columns().len(), 21);
    assert_eq!(
        (opened.get::<String>("message_id")?, opened.get::<Option<String>>("in_reply_to")?),
        ("<r1@x.test>".to_string(), Some("<root@x.test>".to_string()))
    );
    assert_eq!(opened.get::<Json>("reference_ids")?, json!(["<root@x.test>"]));
    assert_eq!(db.query_one(seed(uuid(1), ann)).await?.get::<String>("text_prefix")?, "é".repeat(400));
    assert!(db.query_opt(seed(uuid(2), bob)).await?.is_none());
    assert!(db.query_opt(seed(uuid(8), ann)).await?.is_none());

    // The links as SQL would follow them: one pass over the user's mails,
    // newest first, keeping what any of the three links ties to the ids
    // known so far. `in_reply_to = ANY(..)` is unknown for a mail without
    // the header, which the OR survives.
    let linked = |ids: &[&str], with_spam: bool| {
        let ids = both_forms(ids);
        let tied = any([
            col("message_id").in_list(ids.clone()),
            col("in_reply_to").in_list(ids.clone()),
            col("reference_ids").is_not_null().and(col("reference_ids").json_has_any(ids)),
        ]);
        thread_row(Query::table("mails").index(MAILS_DATE, KeyRange::eq([ann])).filter(threadable(with_spam)).filter(tied).limit(300))
    };
    // Round one, from the seed's own id and the ids it names: the root,
    // the seed, the reply to it and the reply that names the root bare.
    let round_one = db.query(linked(&["<r1@x.test>", "<root@x.test>"], false)).await?;
    assert_eq!(mail_ids(&round_one), [uuid(10), uuid(3), uuid(2), uuid(1)]);
    // Opened from a spam mail, the conversation shows spam too.
    assert_eq!(mail_ids(&db.query(linked(&["<r1@x.test>", "<root@x.test>"], true)).await?), [uuid(10), uuid(9), uuid(3), uuid(2), uuid(1)]);
    // Round two adds the ids of what round one found, and reaches the
    // mail that only names the second reply in its References.
    let known: Vec<String> = column(&round_one, "message_id");
    let known: Vec<&str> = known.iter().map(String::as_str).collect();
    let round_two = db.query(linked(&known, false)).await?;
    assert_eq!(mail_ids(&round_two), [uuid(10), uuid(4), uuid(3), uuid(2), uuid(1)]);

    // The same links through the three indexes that exist for them.
    let ids = both_forms(&["<r1@x.test>", "<root@x.test>"]);
    let found = |rows: Rows| mail_ids(&rows).into_iter().collect::<BTreeSet<_>>();
    let by_message = Query::table("mails")
        .index_ranges(MAILS_MESSAGE, ids.iter().map(|id| KeyRange::eq([val(ann), val(id)])))
        .filter(threadable(false))
        .columns(["mail_id"]);
    let by_reply = Query::table("mails")
        .index_ranges(MAILS_REPLY, ids.iter().map(|id| KeyRange::eq([val(ann), val(id)])))
        .filter(threadable(false))
        .columns(["mail_id"]);
    // A References entry is a JSON string, and so is the key that finds
    // it. A mail that lists two of the ids is under both keys: `distinct`
    // makes it one row.
    let by_reference = Query::table("mails")
        .index_ranges(MAILS_REFERENCE, ids.iter().map(|id| KeyRange::eq([val(ann), val(Json::String(id.clone()))])))
        .filter(threadable(false))
        .columns(["mail_id"])
        .distinct();
    let (by_message, by_reply, by_reference) = (db.query(by_message).await?, db.query(by_reply).await?, db.query(by_reference).await?);
    assert_eq!(by_reference.len(), 2);
    let (by_message, by_reply, by_reference) = (found(by_message), found(by_reply), found(by_reference));
    assert_eq!(by_message, BTreeSet::from([uuid(1), uuid(2)]));
    assert_eq!(by_reply, BTreeSet::from([uuid(2), uuid(3), uuid(10)]));
    assert_eq!(by_reference, BTreeSet::from([uuid(2), uuid(3)]));
    let union: BTreeSet<Uuid> = by_message.into_iter().chain(by_reply).chain(by_reference).collect();
    assert_eq!(union, found(round_one));

    // The References of the mail being replied to, known by either form
    // of its id. An expunged mail lends none.
    let references = |id: &str| {
        Query::table("mails")
            .index_ranges(MAILS_MESSAGE, both_forms(&[id]).into_iter().map(|id| KeyRange::eq([val(ann), val(id)])))
            .filter(col("expunged").eq(false))
            .columns(["reference_ids"])
            .limit(1)
    };
    assert_eq!(db.query_one(references("<r2@x.test>")).await?.get::<Json>("reference_ids")?, json!(["<root@x.test>", "<r1@x.test>"]));
    assert!(db.query_one(references("<root@x.test>")).await?.is_null("reference_ids")?);
    assert!(db.query_opt(references("<gone@x.test>")).await?.is_none());
    Ok(())
}

// R-S3, R-S4, R-Q10, R-Q2: mails without threading headers are tied by their normalised subject.
#[tokio::test(flavor = "multi_thread")]
async fn a_headerless_conversation_is_found_by_its_subject_key() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let (ann, bob) = (user(&db, "ann").await?, user(&db, "bob").await?);
    // The normaliser is the application's. What matters to the database is
    // only that the application stores its result and asks for it again.
    for (subject, key) in [
        ("Quarterly   Report", "quarterly report"),
        ("Re: quarterly report", "quarterly report"),
        ("FWD: RE[2]: Quarterly report", "quarterly report"),
        ("AW： Quarterly Report ", "quarterly report"),
        ("Report: quarterly", "report: quarterly"),
        ("Re[x]: odd", "re[x]: odd"),
    ] {
        assert_eq!(normalize_subject(subject), key, "{subject}");
    }
    let titled = |user: Uuid, n: i64, subject: &str| Mail { subject: subject.to_string(), ..mail(user, n) };
    save_all(
        &db,
        &[
            titled(ann, 1, "Quarterly   Report"),
            titled(ann, 2, "Re: quarterly report"),
            titled(ann, 3, "FWD: RE[2]: Quarterly report"),
            titled(ann, 4, "AW： Quarterly Report "),
            Mail { in_reply_to: Some("<m1@remote.test>".into()), ..titled(ann, 5, "Re: Quarterly report") },
            titled(ann, 6, "Quarterly report card"),
            titled(ann, 7, "Report: quarterly"),
            Mail { draft: true, ..titled(ann, 8, "Quarterly report") },
            Mail { message_id: "00000000000000cd.copy@server".into(), ..titled(ann, 9, "Quarterly report") },
            Mail { is_spam: true, ..titled(ann, 10, "Quarterly report") },
            Mail { reference_ids: Some(json!(["<m1@remote.test>"])), ..titled(ann, 11, "Quarterly report") },
            Mail { expunged: true, ..titled(ann, 12, "Quarterly report") },
            titled(bob, 901, "Quarterly report"),
        ],
    )
    .await?;

    // The index holds only mails with neither header: 5 and 11 are tied to
    // their conversations by those headers and must not be tied by subject too.
    let headerless = db.query(Query::table("mails").index(MAILS_SUBJECT, KeyRange::eq([ann])).columns(["mail_id"])).await?;
    assert_eq!(
        mail_ids(&headerless).into_iter().collect::<BTreeSet<_>>(),
        [1, 2, 3, 4, 6, 7, 8, 9, 10, 12].map(uuid).into_iter().collect::<BTreeSet<_>>()
    );

    // Opened from mail 2: the application normalises the subject of the row
    // it holds and reads that key, newest first, a hundred at most.
    let seed = db.query_one(thread_row(Query::table("mails").key([uuid(2)]).filter(col("user_id").eq(ann).and(col("expunged").eq(false))))).await?;
    let key = normalize_subject(&seed.get::<String>("subject")?);
    let same_subject = |with_spam: bool| {
        thread_row(Query::table("mails").index(MAILS_SUBJECT, KeyRange::eq([val(ann), val(&key)])).filter(threadable(with_spam)).limit(100))
    };
    assert_eq!(mail_ids(&db.query(same_subject(false)).await?), [uuid(4), uuid(3), uuid(2), uuid(1)]);
    assert_eq!(mail_ids(&db.query(same_subject(true)).await?), [uuid(10), uuid(4), uuid(3), uuid(2), uuid(1)]);
    assert_eq!(mail_ids(&db.query(same_subject(false).limit(2)).await?), [uuid(4), uuid(3)]);

    // The one-statement form, where the seed's key is a scalar subquery
    // (SQL's `WITH seed AS ..`): the same mails.
    let seed_key = scalar(
        Query::table("mails").alias("seed").key([uuid(2)]).filter(col("seed.user_id").eq(ann)).select("subject", col("seed.thread_subject_key")),
    );
    let in_one = Query::table("mails")
        .alias("m")
        .index(MAILS_SUBJECT, KeyRange::eq([ann]))
        .filter(col("m.thread_subject_key").eq(seed_key).and(threadable(false)))
        .order_by(col("m.date").desc())
        .columns(["m.mail_id"])
        .limit(100);
    assert_eq!(mail_ids(&db.query(in_one).await?), [uuid(4), uuid(3), uuid(2), uuid(1)]);

    // Once a mail gains a header it leaves the index, and the lookup.
    db.update(Update::table("mails").key([uuid(3)]).set("in_reply_to", "<m2@remote.test>")).await?;
    assert_eq!(mail_ids(&db.query(same_subject(false)).await?), [uuid(4), uuid(2), uuid(1)]);
    Ok(())
}

// R-T2, R-T3, R-Q22, R-W11, R-O3: parameters are typed by the statement; a malformed one is an error the caller absorbs.
#[tokio::test(flavor = "multi_thread")]
async fn text_is_read_as_the_columns_type_or_refused() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;

    // Ids and times reach the application as text, from URLs, cursors and
    // headers, and are written and compared as text.
    let id_text = "6F9619FF-8B86-D011-B42D-00C04FC964FF";
    let delivered = Insert::into("mails")
        .set("mail_id", id_text)
        .set("user_id", ann.to_string())
        .set("message_id", "<typed@remote.test>")
        .set("date", "2026-11-15 03:55:04.123456+01")
        .set("uid_domain", 7)
        .returning(["mail_id", "date"]);
    let row = db.insert(delivered).await?.one()?;
    let (id, date): (Uuid, Timestamp) = (row.get("mail_id")?, row.get("date")?);
    // A uuid prints in lower case whatever case it came in.
    assert_eq!(id.to_string(), "6f9619ff-8b86-d011-b42d-00c04fc964ff");
    assert_eq!(date, ts("2026-11-15T02:55:04.123456Z"));
    // What the API sends is the application's doing: ISO text cut to milliseconds.
    assert_eq!(Timestamp::from_millis(date.millis()).to_rfc3339(), "2026-11-15T02:55:04.123000Z");
    assert!(db.get("mails", [id_text]).await?.is_some());
    assert!(db.get("mails", [id_text.to_lowercase()]).await?.is_some());
    let compared = Query::table("mails")
        .index(MAILS_DATE, KeyRange::eq([ann.to_string()]))
        .filter(col("mail_id").eq(id_text).and(col("date").ge("2026-11-15T02:55:04.123456Z")).and(col("date").lt("2026-11-15T02:55:04.123457Z")));
    assert_eq!(db.query(compared).await?.len(), 1);

    // Text that is not what the column holds is an error, never an empty
    // answer and never a stored guess.
    let not_an_id = db.get("mails", ["not-a-uuid"]).await.unwrap_err();
    assert_eq!(not_an_id.code, ErrorCode::TypeMismatch);
    let no_such_day = db
        .insert(Insert::into("mails").set("user_id", ann).set("message_id", "<bad-date@remote.test>").set("date", "2026-02-30T00:00:00Z"))
        .await
        .unwrap_err();
    assert_eq!(no_such_day.code, ErrorCode::TypeMismatch);
    let number_as_text =
        db.insert(Insert::into("mails").set("user_id", ann).set("message_id", "<bad-uid@remote.test>").set("uid_domain", "12")).await.unwrap_err();
    assert_eq!(number_as_text.code, ErrorCode::TypeMismatch);
    // A STORE whose open range end was printed as "Infinity": the server
    // refuses the comparison, and the IMAP layer answers that the command
    // addressed nothing.
    let infinite = db.query(domain_view(ann, false, Members::Inbox).filter(col("m.uid_domain").le("Infinity"))).await.unwrap_err();
    assert_eq!(infinite.code, ErrorCode::TypeMismatch);
    let unknown_column = db.query(domain_view(ann, false, Members::Inbox).filter(col("m.flagged").eq(true))).await.unwrap_err();
    assert_eq!(unknown_column.code, ErrorCode::NotFound);

    // None of the failures wrote anything or cost the pool a connection:
    // the health probe and the next statement go through.
    db.ping().await?;
    assert_eq!(db.query(Query::table("mails").index(MAILS_DATE, KeyRange::eq([ann]))).await?.len(), 1);
    Ok(())
}

// R-X9, R-X8, R-X1: every connection has a statement timeout; one that fires fails that call and nothing else.
#[tokio::test(flavor = "multi_thread")]
async fn a_statement_past_its_deadline_is_stopped_and_leaves_nothing_behind() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = user(&db, "ann").await?;
    let words: Vec<String> = (0..2000).map(|n| format!("word{n:04}")).collect();
    for document in words.chunks(500) {
        db.insert(train(ann, document, 1, 0)).await?;
    }

    // A pool would give a statement thirty seconds; this client gives it
    // fifty milliseconds. The deadline is all that stops a statement in
    // these tests: none of them cancels another connection's statement, the
    // way a Postgres application can end its maintenance at shutdown.
    let mut config = db.config().clone();
    config.statement_timeout = Duration::from_millis(50);
    let impatient = Client::connect(config).await?;
    // Every word against every word: four million pairs, each with a
    // pattern to match, and for most words no pair that matches.
    let pairs = || {
        Query::table("spam_training").alias("a").index("spam_training_user_word", KeyRange::eq([ann])).join(
            Query::table("spam_training")
                .alias("b")
                .index("spam_training_user_word", KeyRange::eq([col("a.user_id")]))
                .filter(concat([col("a.word"), col("b.word")]).ilike("%9%9%9%9%")),
        )
    };
    let stopped = impatient.query(pairs().select("pairs", count())).await.unwrap_err();
    assert_eq!(stopped.code, ErrorCode::Timeout);
    // A write that runs out of time is undone whole.
    let stopped = impatient.update(Update::rows(pairs()).set("spam_count", col("a.spam_count") + 1)).await.unwrap_err();
    assert_eq!(stopped.code, ErrorCode::Timeout);
    let trained = Query::table("spam_training")
        .index("spam_training_user_word", KeyRange::eq([ann]))
        .select("words", count())
        .select("spam", sum(col("spam_count")));
    let totals = db.query_one(trained.clone()).await?;
    assert_eq!((totals.get::<i64>("words")?, totals.get::<i64>("spam")?), (2000, 2000));

    // The timeout was that statement's alone: the same client answers
    // the next one, and nobody waits behind a lock the dead write left.
    impatient.ping().await?;
    assert_eq!(impatient.query_one(trained).await?.get::<i64>("words")?, 2000);
    assert_eq!(impatient.insert(train(ann, &words[..10], 0, 1)).await?.affected(), 10);
    Ok(())
}
