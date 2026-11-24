//! An identity service's use of its database, said through the client.
//!
//! The scenario is a service that signs people in for other services:
//! users, sessions, tokens, access roles, an audit log, an outbox of events
//! and rate limits. Each test builds the statements such a service runs
//! with the client and asserts their results: lookups by key and by index,
//! joins, upserts, conditional updates, cascading deletes, transactions and
//! migrations.
//!
//! Where an application on a SQL database takes a row lock, reads with
//! `SKIP LOCKED` or takes a transaction-scoped advisory lock, the
//! statements here run in a writing transaction, which has the database to
//! itself. A lock that is held outside a transaction, or only tried, is a
//! named lock.
//!
//! The functions before the tests are the service's statements: one
//! function per shape that several tests share, named for what it does.

mod common;
use common::*;

use std::time::Duration;

use serde_json::json;

/// The columns the service names when it reads a user, so that a column a
/// later migration adds changes no result.
const USER_COLUMNS: [&str; 10] =
    ["id", "username", "name", "email", "email_verified_at", "kind", "owner_id", "password_hash", "disabled_at", "created_at"];

const INVITE_COLUMNS: [&str; 8] = ["id", "email", "service", "role", "invited_by", "expires_at", "accepted_by", "accepted_at"];

/// One day in the microseconds timestamps count in.
const DAY: i64 = 86_400_000_000;

/// Thirty-two bytes that stand in for the SHA-256 digest the service stores
/// in place of a secret: the same text always gives the same bytes.
fn digest(secret: &str) -> Vec<u8> {
    let mut state: u64 = 0xcbf2_9ce4_8422_2325;
    let mut out = Vec::with_capacity(32);
    for round in 0u8..4 {
        for byte in secret.bytes().chain([round]) {
            state = (state ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
        out.extend_from_slice(&state.to_be_bytes());
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// An empty `text[]`.
fn no_texts() -> Vec<String> {
    Vec::new()
}

fn text(name: &str) -> ColumnDef {
    ColumnDef::new(name, Type::Text)
}

fn bytes(name: &str) -> ColumnDef {
    ColumnDef::new(name, Type::Bytes)
}

fn int(name: &str) -> ColumnDef {
    ColumnDef::new(name, Type::Int)
}

fn time(name: &str) -> ColumnDef {
    ColumnDef::new(name, Type::Timestamp)
}

fn texts(name: &str) -> ColumnDef {
    ColumnDef::new(name, Type::list(Type::Text))
}

/// The table the migrator keeps its own record in.
fn schema_migrations() -> TableDef {
    TableDef::new("schema_migrations").column(text("version")).column(time("applied_at").default_now()).primary_key(["version"])
}

/// The service's tables as its first two migrations leave them, parents
/// before children. Unique indexes carry the names Postgres would give
/// them, because the service tells two of them apart by name. Checks that
/// would be a regular expression are left to the application, which
/// validates first anyway.
fn tables() -> Vec<TableDef> {
    vec![
        TableDef::new("users")
            .column(text("id"))
            .column(text("username"))
            .column(text("name").default(""))
            .column(text("email").nullable())
            .column(time("email_verified_at").nullable())
            .column(text("kind").default("person"))
            .column(text("owner_id").nullable())
            .column(text("password_hash").nullable())
            .column(time("disabled_at").nullable())
            .column(time("created_at").default_now())
            .column(time("updated_at").default_now())
            .primary_key(["id"])
            .unique("users_username_key", ["username"])
            .index(IndexDef::new("users_email").on_expr(col("email").lower()).unique().filter(col("email").is_not_null()))
            .index(IndexDef::new("users_owner").on(["owner_id"]).filter(col("owner_id").is_not_null()))
            // Postgres' NO ACTION: a user who still owns bots cannot go.
            .foreign_key(["owner_id"], "users", OnDelete::Restrict)
            .check("users_kind_check", col("kind").in_list(vec!["person", "bot"])),
        TableDef::new("access")
            .column(text("user_id"))
            .column(text("service"))
            .column(text("role"))
            .column(text("granted_by"))
            .column(time("granted_at").default_now())
            .primary_key(["user_id", "service"])
            .index(IndexDef::new("access_service").on(["service", "user_id"]))
            .foreign_key(["user_id"], "users", OnDelete::Cascade),
        TableDef::new("totp")
            .column(text("user_id"))
            .column(bytes("secret_enc"))
            .column(int("last_step").default(0))
            .column(time("created_at").default_now())
            .primary_key(["user_id"])
            .foreign_key(["user_id"], "users", OnDelete::Cascade),
        TableDef::new("passkeys")
            .column(text("id"))
            .column(text("user_id"))
            .column(bytes("credential_id"))
            .column(bytes("public_key"))
            .column(int("sign_count").default(0))
            .column(text("name"))
            .column(time("created_at").default_now())
            .column(time("last_used_at").nullable())
            .primary_key(["id"])
            .unique("passkeys_credential_id_key", ["credential_id"])
            .index(IndexDef::new("passkeys_user").on(["user_id"]))
            .foreign_key(["user_id"], "users", OnDelete::Cascade),
        TableDef::new("recovery_codes")
            .column(text("user_id"))
            .column(bytes("code_hash"))
            .column(time("used_at").nullable())
            .primary_key(["user_id", "code_hash"])
            .foreign_key(["user_id"], "users", OnDelete::Cascade),
        TableDef::new("sessions")
            .column(text("id"))
            .column(text("user_id"))
            .column(bytes("token_hash"))
            .column(time("auth_time"))
            .column(texts("amr"))
            // Postgres' inet: written and read back as text, never compared.
            .column(text("ip").nullable())
            .column(text("user_agent").nullable())
            .column(time("created_at").default_now())
            .column(time("last_seen_at").default_now())
            .column(time("expires_at"))
            .column(time("ends_at"))
            .primary_key(["id"])
            .unique("sessions_token_hash_key", ["token_hash"])
            .index(IndexDef::new("sessions_user").on(["user_id"]))
            .foreign_key(["user_id"], "users", OnDelete::Cascade),
        TableDef::new("refresh_tokens")
            .column(bytes("token_hash"))
            .column(text("family_id"))
            .column(text("session_id").nullable())
            .column(text("user_id"))
            .column(text("client_id"))
            .column(time("created_at").default_now())
            .column(time("expires_at"))
            .column(time("rotated_at").nullable())
            .column(time("revoked_at").nullable())
            .column(texts("amr").default(no_texts()))
            .primary_key(["token_hash"])
            .index(IndexDef::new("refresh_tokens_session").on(["session_id"]).filter(col("session_id").is_not_null()))
            .index(IndexDef::new("refresh_tokens_family").on(["family_id"]))
            .index(IndexDef::new("refresh_tokens_user").on(["user_id"]))
            .foreign_key(["session_id"], "sessions", OnDelete::Cascade)
            .foreign_key(["user_id"], "users", OnDelete::Cascade),
        TableDef::new("auth_codes")
            .column(bytes("code_hash"))
            .column(text("client_id"))
            .column(text("session_id"))
            .column(text("redirect_uri"))
            .column(text("code_challenge"))
            .column(text("nonce").nullable())
            .column(time("expires_at"))
            .column(time("used_at").nullable())
            .primary_key(["code_hash"])
            .foreign_key(["session_id"], "sessions", OnDelete::Cascade),
        TableDef::new("credentials")
            .column(text("id"))
            .column(text("user_id"))
            .column(text("kind"))
            .column(text("service"))
            .column(texts("scopes"))
            .column(text("name"))
            .column(bytes("secret_hash"))
            .column(time("created_at").default_now())
            .column(time("last_used_at").nullable())
            .column(time("expires_at").nullable())
            .column(time("revoked_at").nullable())
            .primary_key(["id"])
            .unique("credentials_secret_hash_key", ["secret_hash"])
            .index(IndexDef::new("credentials_user").on(["user_id"]))
            .foreign_key(["user_id"], "users", OnDelete::Cascade)
            .check("credentials_kind_check", col("kind").in_list(vec!["app_password", "api_key"])),
        TableDef::new("client_secrets")
            .column(text("client_id"))
            .column(bytes("secret_hash"))
            .column(time("created_at").default_now())
            .column(time("revoked_at").nullable())
            .primary_key(["client_id", "secret_hash"]),
        TableDef::new("invites")
            .column(text("id"))
            .column(text("email"))
            .column(text("service"))
            .column(text("role"))
            .column(bytes("token_hash"))
            .column(text("invited_by"))
            .column(time("expires_at"))
            .column(text("accepted_by").nullable())
            .column(time("accepted_at").nullable())
            .primary_key(["id"])
            .unique("invites_token_hash_key", ["token_hash"])
            .index(IndexDef::new("invites_service").on(["service", "id"]))
            .foreign_key(["accepted_by"], "users", OnDelete::SetNull),
        TableDef::new("one_time_tokens")
            .column(bytes("token_hash"))
            .column(text("purpose"))
            .column(text("user_id"))
            .column(text("email").nullable())
            .column(time("expires_at"))
            .column(time("used_at").nullable())
            .primary_key(["token_hash"])
            .index(IndexDef::new("one_time_tokens_user").on(["user_id"]))
            .foreign_key(["user_id"], "users", OnDelete::Cascade)
            .check("one_time_tokens_purpose_check", col("purpose").in_list(vec!["reset", "verify_email", "activate"])),
        TableDef::new("signing_keys")
            .column(text("kid"))
            .column(ColumnDef::new("public_jwk", Type::Json))
            .column(bytes("private_key_enc"))
            .column(time("publish_at"))
            .column(time("sign_from"))
            .column(time("retire_at").nullable())
            .primary_key(["kid"]),
        TableDef::new("outbox")
            .column(int("id").auto_increment())
            .column(text("service"))
            .column(text("token"))
            .column(int("attempts").default(0))
            .column(time("next_attempt_at").default_now())
            .column(time("delivered_at").nullable())
            .column(time("failed_at").nullable())
            .primary_key(["id"])
            .index(IndexDef::new("outbox_pending").on(["next_attempt_at"]).filter(col("delivered_at").is_null().and(col("failed_at").is_null()))),
        TableDef::new("audit")
            .column(int("id").auto_increment())
            .column(time("at").default_now())
            .column(text("actor").nullable())
            .column(text("action"))
            // No foreign key: the log outlives a deleted user.
            .column(text("user_id").nullable())
            .column(text("ip").nullable())
            .column(ColumnDef::new("detail", Type::Json).default(json!({})))
            .primary_key(["id"])
            .index(IndexDef::new("audit_user").on(["user_id", "id"]))
            .index(IndexDef::new("audit_at").on(["at"])),
        // Postgres' UNLOGGED: the counters may be lost in a crash.
        TableDef::new("throttle").column(text("key")).column(time("window_start")).column(int("hits")).primary_key(["key"]).volatile(),
        TableDef::new("services")
            .column(text("name"))
            .column(texts("roles"))
            .column(texts("mfa_roles"))
            .column(text("signup"))
            .column(text("default_role").nullable())
            .column(ColumnDef::new("manages_own_members", Type::Bool).default(false))
            .column(texts("api_key_scopes").default(no_texts()))
            .column(texts("app_password_scopes").default(no_texts()))
            .column(text("events_url").nullable())
            .column(ColumnDef::new("held", Type::Bool).default(false))
            .column(text("created_by"))
            .column(time("created_at").default_now())
            .column(time("updated_at").default_now())
            .primary_key(["name"])
            // The half of the name's check that is not a regular
            // expression: no service takes the identity service's own name.
            .check("services_name_check", col("name").ne("identity"))
            .check("services_signup_check", col("signup").in_list(vec!["closed", "invite", "open"])),
        TableDef::new("clients")
            .column(text("id"))
            .column(text("service"))
            .column(text("type"))
            .column(texts("redirect_uris"))
            .column(texts("post_logout_redirect_uris").default(no_texts()))
            .column(time("created_at").default_now())
            .primary_key(["id"])
            .index(IndexDef::new("clients_service").on(["service"]))
            .foreign_key(["service"], "services", OnDelete::Cascade)
            .check("clients_type_check", col("type").in_list(vec!["confidential", "public"])),
        // A boolean key that must be true: at most one row.
        TableDef::new("services_revision")
            .column(ColumnDef::new("id", Type::Bool).default(true))
            .column(int("revision").default(0))
            .primary_key(["id"])
            .check("services_revision_id_check", col("id").eq(true)),
    ]
}

/// Everything the service's migrations create: its twenty tables, and the
/// one row the services migration seeds.
async fn schema(db: &Client) -> DbResult<()> {
    db.create_table_if_not_exists(schema_migrations()).await?;
    for table in tables() {
        db.create_table(table).await?;
    }
    db.insert(Insert::into("services_revision").set("revision", 0)).await?;
    Ok(())
}

/// How many rows a query finds.
async fn rows_of(db: &impl Db, query: Query) -> DbResult<i64> {
    db.query_one(query.select("n", count())).await?.get("n")
}

/// The first column of a query's rows, sorted: for a result whose order
/// the query does not fix.
async fn sorted(db: &impl Db, query: Query) -> DbResult<Vec<String>> {
    let mut values: Vec<String> = db.query(query).await?.column()?;
    values.sort();
    Ok(values)
}

fn user_row(id: &str, username: &str, email: Option<&str>, kind: &str, owner: Option<&str>, password_hash: Option<&str>) -> Insert {
    Insert::into("users")
        .set("id", id)
        .set("username", username)
        .set("name", username)
        .set("email", email)
        .set("email_verified_at", null())
        .set("kind", kind)
        .set("owner_id", owner)
        .set("password_hash", password_hash)
}

/// A person who signed up with a password.
fn person(id: &str, username: &str, email: Option<&str>) -> Insert {
    user_row(id, username, email, "person", None, Some("$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA"))
}

/// A bot, which has an owner and no password.
fn bot(id: &str, username: &str, owner: &str) -> Insert {
    user_row(id, username, None, "bot", Some(owner), None)
}

/// A session that signed in now, idle for at most seven days and ending
/// after thirty, all by the server's clock.
fn new_session(id: &str, user: &str, token: &str) -> Insert {
    Insert::into("sessions")
        .set("id", id)
        .set("user_id", user)
        .set("token_hash", digest(token))
        .set("auth_time", now())
        .set("amr", vec!["pwd"])
        .set("ip", "203.0.113.7")
        .set("user_agent", "Firefox")
        .set("expires_at", now() + days(7))
        .set("ends_at", now() + days(30))
}

/// Gives a user a role on a service, replacing the one they had.
fn grant_role(user: &str, service: &str, role: &str, by: &str) -> Insert {
    Insert::into("access")
        .set("user_id", user)
        .set("service", service)
        .set("role", role)
        .set("granted_by", by)
        .on_conflict_update(PRIMARY)
        .update_excluded(["role", "granted_by"])
        .update("granted_at", now())
        .returning(["service", "role", "granted_by", "granted_at"])
}

/// The role of a user who may still sign in.
fn role_of(user: &str, service: &str) -> Query {
    Query::table("access")
        .alias("a")
        .key([user, service])
        .join(Query::table("users").alias("u").key([col("a.user_id")]).filter(col("u.disabled_at").is_null()))
        .select("role", col("a.role"))
}

/// One line of the audit log. Its number and its time are the server's.
fn audit_entry(actor: Option<&str>, action: &str, user: Option<&str>, detail: Json) -> Insert {
    Insert::into("audit").set("actor", actor).set("action", action).set("user_id", user).set("ip", "203.0.113.7").set("detail", detail)
}

/// An event for a service, queued by the transaction that caused it.
fn event(service: &str, token: &str) -> Insert {
    Insert::into("outbox").set("service", service).set("token", token)
}

/// A service with two roles, which issues API keys and no app passwords.
fn new_service(name: &str, signup: &str) -> Insert {
    Insert::into("services")
        .set("name", name)
        .set("roles", vec!["admin", "member"])
        .set("mfa_roles", vec!["admin"])
        .set("signup", signup)
        .set("default_role", "member")
        .set("manages_own_members", false)
        .set("api_key_scopes", vec!["read", "write"])
        .set("app_password_scopes", no_texts())
        .set("events_url", format!("https://{name}.example/events"))
        .set("held", false)
        .set("created_by", "usr_admin")
}

/// A client of a service, with one redirect URI.
fn new_client(id: &str, service: &str, kind: &str) -> Insert {
    Insert::into("clients")
        .set("id", id)
        .set("service", service)
        .set("type", kind)
        .set("redirect_uris", vec![format!("https://{service}.example/callback")])
        .set("post_logout_redirect_uris", no_texts())
}

/// Every change to the services starts by counting it.
fn bump_revision() -> Update {
    Update::table("services_revision").set("revision", col("revision") + 1)
}

/// A refresh token of a public client, in a family of its own and tied to
/// no session.
fn public_token(token: &str, family: &str, user: &str, client: &str) -> Insert {
    Insert::into("refresh_tokens")
        .set("token_hash", digest(token))
        .set("family_id", family)
        .set("session_id", null())
        .set("user_id", user)
        .set("client_id", client)
        .set("expires_at", now() + days(30))
        .set("amr", vec!["pwd"])
}

/// A refresh token of a confidential client, which copies what it needs
/// from the session row and lives exactly as long.
fn session_token(token: &str, client: &str, session: &str) -> Insert {
    Insert::into("refresh_tokens").columns(["token_hash", "family_id", "session_id", "user_id", "client_id", "expires_at", "amr"]).from_query(
        Query::table("sessions")
            .key([session])
            .select("token_hash", digest(token))
            .select("family_id", col("id"))
            .select("session_id", col("id"))
            .select("user_id", col("user_id"))
            .select("client_id", client)
            .select("expires_at", col("ends_at"))
            .select("amr", col("amr")),
    )
}

/// The clients holding tokens of a session, or of a public client's family.
/// The two columns have an index each, and a query reads one, so this reads
/// the table.
fn clients_of(session_or_family: &str) -> Query {
    Query::table("refresh_tokens")
        .filter(col("session_id").eq(session_or_family).or(col("family_id").eq(session_or_family)))
        .select("client_id", col("client_id"))
        .distinct()
}

/// An authorization code issued to a client for a session, good for
/// `lifetime_seconds` by the server's clock.
fn new_code(code: &str, client: &str, session: &str, lifetime_seconds: i64) -> Insert {
    Insert::into("auth_codes")
        .set("code_hash", digest(code))
        .set("client_id", client)
        .set("session_id", session)
        .set("redirect_uri", "https://files.example/callback")
        .set("code_challenge", "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM")
        .set("nonce", "n-0S6_WzA2Mj")
        .set("expires_at", now() + seconds(lifetime_seconds))
}

/// Spends a code if nobody has, and says whether it was still live.
fn redeem(code: &str) -> Update {
    Update::table("auth_codes")
        .key([digest(code)])
        .filter(col("used_at").is_null())
        .set("used_at", now())
        .returning(["client_id", "session_id", "redirect_uri", "code_challenge", "nonce"])
        .returning_expr("live", col("expires_at").gt(now()))
}

/// An API key or app password. The expiry arrives as text and the server
/// reads it.
fn new_credential(id: &str, user: &str, kind: &str, service: &str, secret: &str, expires: Option<&str>) -> Insert {
    Insert::into("credentials")
        .set("id", id)
        .set("user_id", user)
        .set("kind", kind)
        .set("service", service)
        .set("scopes", vec!["read", "write"])
        .set("name", "laptop")
        .set("secret_hash", digest(secret))
        .set("expires_at", expires)
        .returning(["id", "kind", "service", "scopes", "name", "created_at", "last_used_at", "expires_at"])
}

/// A one-time token for one purpose, good for `lifetime_seconds` by the
/// server's clock.
fn one_time_token(token: &str, purpose: &str, user: &str, email: Option<&str>, lifetime_seconds: i64) -> Insert {
    Insert::into("one_time_tokens")
        .set("token_hash", digest(token))
        .set("purpose", purpose)
        .set("user_id", user)
        .set("email", email)
        .set("expires_at", now() + seconds(lifetime_seconds))
}

/// Uses a token once, for the purpose it was made for.
fn consume_token(token: &str, purpose: &str) -> Update {
    Update::table("one_time_tokens")
        .key([digest(token)])
        .filter(all([col("purpose").eq(purpose), col("used_at").is_null(), col("expires_at").gt(now())]))
        .set("used_at", now())
        .returning(["user_id", "email"])
}

// R-S1, R-S2, R-S3, R-S4, R-S5, R-S6, R-S7, R-S10: the schema is described
// as it was created: tables, keys, defaults, indexes on an expression and
// over part of a table, checks and foreign keys.
#[tokio::test(flavor = "multi_thread")]
async fn the_schema_reads_back_as_it_was_defined() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;

    let mut names: Vec<String> = db.describe().await?.into_iter().map(|table| table.name).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "access",
            "audit",
            "auth_codes",
            "client_secrets",
            "clients",
            "credentials",
            "invites",
            "one_time_tokens",
            "outbox",
            "passkeys",
            "recovery_codes",
            "refresh_tokens",
            "schema_migrations",
            "services",
            "services_revision",
            "sessions",
            "signing_keys",
            "throttle",
            "totp",
            "users",
        ]
    );

    let users = db.describe_table("users").await?.expect("users is there");
    let columns: Vec<&str> = users.columns.iter().map(|column| column.name.as_str()).collect();
    assert_eq!(
        columns,
        ["id", "username", "name", "email", "email_verified_at", "kind", "owner_id", "password_hash", "disabled_at", "created_at", "updated_at"]
    );
    assert_eq!(users.primary_key, ["id"]);
    assert!(!users.columns[1].nullable && users.columns[3].nullable);
    assert_eq!(users.columns[5].default, Some(val("person")));
    assert_eq!(users.columns[9].default, Some(now()));
    assert!(!users.volatile);

    // The index that tells one sign-up error from another: unique, on an
    // expression, over part of the table.
    let email = users.find_index("users_email").expect("the email index is there");
    assert!(email.unique);
    assert_eq!(email.keys.len(), 1);
    assert_eq!(email.keys[0].expr, col("email").lower());
    assert_eq!(email.filter, Some(col("email").is_not_null()));
    assert!(users.find_index("users_username_key").expect("the username index is there").unique);
    assert!(!users.find_index("users_owner").expect("the owner index is there").unique);
    assert_eq!(users.checks.iter().map(|check| check.name.as_str()).collect::<Vec<_>>(), ["users_kind_check"]);
    let owner = &users.foreign_keys[0];
    assert_eq!(
        (owner.columns.as_slice(), owner.ref_table.as_str(), owner.on_delete),
        (["owner_id".to_string()].as_slice(), "users", OnDelete::Restrict)
    );

    // Composite keys, one of them over bytes.
    assert_eq!(db.describe_table("recovery_codes").await?.expect("recovery_codes is there").primary_key, ["user_id", "code_hash"]);
    assert_eq!(db.describe_table("access").await?.expect("access is there").primary_key, ["user_id", "service"]);

    let invites = db.describe_table("invites").await?.expect("invites is there");
    assert_eq!(invites.foreign_keys[0].on_delete, OnDelete::SetNull);
    let tokens = db.describe_table("refresh_tokens").await?.expect("refresh_tokens is there");
    let parents: Vec<(&str, OnDelete)> = tokens.foreign_keys.iter().map(|key| (key.ref_table.as_str(), key.on_delete)).collect();
    assert_eq!(parents, [("sessions", OnDelete::Cascade), ("users", OnDelete::Cascade)]);
    assert_eq!(tokens.columns[9].default, Some(val(no_texts())));

    let outbox = db.describe_table("outbox").await?.expect("outbox is there");
    assert!(outbox.columns[0].auto_increment);
    let pending = outbox.find_index("outbox_pending").expect("the queue's index is there");
    assert_eq!(pending.filter, Some(col("delivered_at").is_null().and(col("failed_at").is_null())));

    assert!(db.describe_table("audit").await?.expect("audit is there").columns[0].auto_increment);
    assert!(db.describe_table("throttle").await?.expect("throttle is there").volatile);
    assert!(db.describe_table("identities").await?.is_none());

    // The migrator creates its table at every start: the second time is
    // not an error and changes nothing, where a plain create is refused.
    db.create_table_if_not_exists(schema_migrations()).await?;
    let again = db.create_table(schema_migrations()).await.unwrap_err();
    assert_eq!(again.code, ErrorCode::AlreadyExists);
    assert_eq!(db.describe().await?.len(), 20);
    Ok(())
}

// R-S7, R-W11: a check on a set of values, NOT NULL and a foreign key each
// refuse a row with an error code of their own, and a refused update changes
// nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_row_that_breaks_a_check_a_not_null_or_a_foreign_key_is_refused() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", None)).await?;
    db.insert(new_service("files", "invite")).await?;

    let outside_the_set = [
        (user_row("usr_x", "xavier", None, "robot", None, None), "users_kind_check"),
        (new_credential("key_x", "usr_ann", "password", "files", "secret", None), "credentials_kind_check"),
        (one_time_token("token", "login", "usr_ann", None, 60), "one_time_tokens_purpose_check"),
        (new_service("chat", "maybe"), "services_signup_check"),
        (new_service("identity", "closed"), "services_name_check"),
        (new_client("files-tv", "files", "native"), "clients_type_check"),
        (Insert::into("services_revision").set("id", false), "services_revision_id_check"),
    ];
    for (insert, check) in outside_the_set {
        let err = db.insert(insert).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::CheckViolation, "{check}");
        assert!(err.violates(check), "{check}: {err}");
    }

    // An update is held to the same checks, and changes nothing when it
    // breaks one.
    let err = db.update(Update::table("users").key(["usr_ann"]).set("kind", "robot").set("name", "Robot")).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::CheckViolation);
    let ann = db.get("users", ["usr_ann"]).await?.expect("ann is there");
    assert_eq!((ann.get::<String>("kind")?, ann.get::<String>("name")?), ("person".to_string(), "ann".to_string()));

    let err = db.insert(Insert::into("users").set("id", "usr_x").set("username", null())).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotNullViolation);
    assert_eq!(err.constraint.as_deref(), Some("username"));
    let err = db.update(Update::table("users").key(["usr_ann"]).set("name", null())).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotNullViolation);
    assert_eq!(err.constraint.as_deref(), Some("name"));

    // A child needs its parent: a grant, a session and a client each
    // point at a row that is not there.
    for orphan in [
        grant_role("usr_nobody", "files", "member", "host-cli"),
        new_session("ses_x", "usr_nobody", "cookie"),
        new_client("wiki-web", "wiki", "public"),
    ] {
        let err = db.insert(orphan).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::ForeignKeyViolation);
    }
    let err = db.update(Update::table("users").key(["usr_ann"]).set("owner_id", "usr_nobody")).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::ForeignKeyViolation);

    for table in ["credentials", "one_time_tokens", "clients", "access", "sessions"] {
        assert_eq!(rows_of(&db, Query::table(table)).await?, 0, "{table}");
    }
    assert_eq!((rows_of(&db, Query::table("users")).await?, rows_of(&db, Query::table("services")).await?), (1, 1));
    Ok(())
}

// R-Q1, R-S7, R-W6, R-X2: a check holds a table to one row, and of ten
// concurrent transactions that count it up each sees a number of its own.
#[tokio::test(flavor = "multi_thread")]
async fn the_services_revision_is_one_row_that_every_change_counts_up() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;

    // Read on every request.
    let revision = Query::table("services_revision").columns(["revision"]);
    assert_eq!(db.query_one(revision.clone()).await?.get::<i64>("revision")?, 0);

    // There is never a second row: the default key is taken, and the only
    // other key a boolean has is refused by the check.
    let second = db.insert(Insert::into("services_revision").set("revision", 5)).await.unwrap_err();
    assert_eq!(second.code, ErrorCode::UniqueViolation);
    assert!(second.violates(PRIMARY));
    let other = db.insert(Insert::into("services_revision").set("id", false).set("revision", 5)).await.unwrap_err();
    assert_eq!(other.code, ErrorCode::CheckViolation);
    assert_eq!(db.query(revision.clone()).await?.len(), 1);

    let bumped = db.update(bump_revision().returning(["revision"]).returning_expr("was", old("revision"))).await?;
    assert_eq!(bumped.affected(), 1);
    let bumped = bumped.one()?;
    assert_eq!((bumped.get::<i64>("was")?, bumped.get::<i64>("revision")?), (0, 1));

    // On Postgres this update is also the lock that puts changes to the
    // services one after another. Ten changes at once each see a number of
    // their own.
    let mut changes = Vec::new();
    for _ in 0..10 {
        let db = db.clone();
        let revision = revision.clone();
        changes.push(tokio::spawn(async move {
            db.exclusive(|tx| async move {
                tx.update(bump_revision()).await?;
                tx.query_one(revision).await?.get::<i64>("revision")
            })
            .await
        }));
    }
    let mut seen = Vec::new();
    for change in changes {
        seen.push(change.await.expect("the task ran")?);
    }
    seen.sort();
    assert_eq!(seen, (2..=11).collect::<Vec<i64>>());
    assert_eq!(db.query_one(revision).await?.get::<i64>("revision")?, 11);
    Ok(())
}

// R-Q1, R-Q2, R-Q3, R-Q11: lookups by primary key, by a unique index, by an
// index on an expression and by an index over part of the table.
#[tokio::test(flavor = "multi_thread")]
async fn a_user_is_found_by_id_by_username_and_by_email_in_any_case() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let before = db.now().await?;
    db.insert(person("usr_ann", "ann", Some("Ann@Example.com"))).await?;
    db.insert(person("usr_bob", "bob", None)).await?;
    db.insert(bot("usr_bot1", "ann-bot-1", "usr_ann")).await?;
    db.insert(bot("usr_bot2", "ann-bot-2", "usr_ann")).await?;
    let after = db.now().await?;

    // By id, naming the columns wanted.
    let ann = db.query_one(Query::table("users").key(["usr_ann"]).columns(USER_COLUMNS)).await?;
    assert_eq!(ann.columns(), USER_COLUMNS);
    assert_eq!(ann.get::<String>("username")?, "ann");
    assert_eq!(ann.get::<Option<String>>("email")?.as_deref(), Some("Ann@Example.com"));
    assert_eq!(ann.get::<String>("kind")?, "person");
    assert_eq!(ann.get::<Option<Timestamp>>("email_verified_at")?, None);
    assert_eq!(ann.get::<Option<String>>("owner_id")?, None);
    let created: Timestamp = ann.get("created_at")?;
    assert!(before <= created && created <= after);
    assert!(db.query_opt(Query::table("users").key(["usr_nobody"]).columns(USER_COLUMNS)).await?.is_none());

    // By address: the server folds what was typed the way it folded what is
    // stored, so one function decides what "the same address" means.
    for typed in ["ann@example.com", "ANN@EXAMPLE.COM", "Ann@Example.com"] {
        let found = db.query_one(Query::table("users").index("users_email", KeyRange::eq([val(typed).lower()])).columns(USER_COLUMNS)).await?;
        assert_eq!(found.get::<String>("id")?, "usr_ann", "{typed}");
    }

    // Whether an address is taken.
    let taken = |address: &str| Query::table("users").index("users_email", KeyRange::eq([address.to_lowercase()])).select("one", 1);
    assert_eq!(db.query(taken("ANN@example.com")).await?.len(), 1);
    assert!(db.query(taken("bob@example.com")).await?.is_empty());

    // By username. The service lower-cases what was typed; the index itself
    // is exact.
    let bob = db.query_one(Query::table("users").index("users_username_key", KeyRange::eq(["bob"])).columns(USER_COLUMNS)).await?;
    assert_eq!(bob.get::<String>("id")?, "usr_bob");
    assert!(bob.is_null("email")?);
    assert!(db.query_opt(Query::table("users").index("users_username_key", KeyRange::eq(["Bob"]))).await?.is_none());

    // Who a token is issued for, unless they were disabled.
    let holder = |id: &str| {
        Query::table("users")
            .key([id])
            .filter(col("disabled_at").is_null())
            .columns(["id", "username", "name", "email"])
            .select("email_verified", col("email_verified_at").is_not_null())
    };
    let ann = db.query_one(holder("usr_ann")).await?;
    assert_eq!(ann.columns(), ["id", "username", "name", "email", "email_verified"]);
    assert!(!ann.get::<bool>("email_verified")?);
    db.update(Update::table("users").key(["usr_ann"]).set("email_verified_at", now())).await?;
    assert!(db.query_one(holder("usr_ann")).await?.get::<bool>("email_verified")?);
    db.update(Update::table("users").key(["usr_ann"]).set("disabled_at", Timestamp::now())).await?;
    assert!(db.query_opt(holder("usr_ann")).await?.is_none());

    // The bots a user owns, through the index that holds only bots.
    let bots = |owner: &str| Query::table("users").index("users_owner", KeyRange::eq([owner]));
    assert_eq!(rows_of(&db, bots("usr_ann")).await?, 2);
    assert_eq!(rows_of(&db, bots("usr_bob")).await?, 0);
    Ok(())
}

/// Confirming a new address, as one transaction: the token is used and its
/// address becomes the user's.
async fn confirm_email(db: &Client, token: &str, user: &str) -> DbResult<()> {
    db.transaction(|tx| async move {
        let token = tx.update(consume_token(token, "verify_email")).await?.one()?;
        let address: String = token.get("email")?;
        tx.update(Update::table("users").key([user]).set("email", address).set("email_verified_at", now()).set("updated_at", now())).await?;
        Ok(())
    })
    .await
}

// R-S3, R-T9, R-W11, R-X1: a unique violation names the index it broke, and
// a transaction in which a statement failed can only be given up.
#[tokio::test(flavor = "multi_thread")]
async fn a_taken_username_or_email_is_reported_by_the_name_of_its_index() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", Some("Ann@Example.com"))).await?;

    // The service answers these two with different messages, and has only
    // the index's name to tell them apart.
    let err = db.insert(person("usr_two", "ann", Some("other@example.com"))).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::UniqueViolation);
    assert!(err.violates("users_username_key") && !err.violates("users_email"), "{err}");

    let err = db.insert(person("usr_two", "anna", Some("ANN@example.COM"))).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::UniqueViolation);
    assert!(err.violates("users_email") && !err.violates("users_username_key"), "{err}");

    let err = db.insert(person("usr_ann", "anna", None)).await.unwrap_err();
    assert!(err.is_unique_violation() && err.violates(PRIMARY), "{err}");
    assert_eq!(rows_of(&db, Query::table("users")).await?, 1);

    // Any number of users have no address: NULL is not a key.
    db.insert(person("usr_bob", "bob", None)).await?;
    db.insert(person("usr_cy", "cy", None)).await?;

    // A row does not collide with itself.
    db.update(Update::table("users").key(["usr_ann"]).set("email", "ANN@EXAMPLE.COM").set("updated_at", now())).await?;

    // Patching a user, a rename and then a new address: the taken address
    // fails the second statement, the transaction can then only be given
    // up, and the rename before it goes too.
    let tx = db.begin().await?;
    tx.update(Update::table("users").key(["usr_bob"]).set("name", "Robert").set("updated_at", now())).await?;
    let take_address =
        Update::table("users").key(["usr_bob"]).set("email", "ann@example.com").set("email_verified_at", null()).set("updated_at", now());
    let err = tx.update(take_address).await.unwrap_err();
    assert!(err.is_unique_violation() && err.violates("users_email"), "{err}");
    assert_eq!(tx.query(Query::table("users")).await.unwrap_err().code, ErrorCode::TransactionAborted);
    assert_eq!(tx.commit().await.unwrap_err().code, ErrorCode::TransactionAborted);
    assert_eq!(db.get("users", ["usr_bob"]).await?.expect("bob is there").get::<String>("name")?, "bob");

    // Confirming an address somebody else has meanwhile taken: the error
    // names the index, and the token is not used up.
    db.insert(one_time_token("verify-cy", "verify_email", "usr_cy", Some("Ann@example.com"), 3600)).await?;
    let err = confirm_email(&db, "verify-cy", "usr_cy").await.unwrap_err();
    assert!(err.is_unique_violation() && err.violates("users_email"), "{err}");
    let token = db.get("one_time_tokens", [digest("verify-cy")]).await?.expect("the token is there");
    assert!(token.is_null("used_at")?);

    // An address given up is free again, and the same confirmation then
    // goes through.
    db.update(Update::table("users").key(["usr_ann"]).set("email", null()).set("email_verified_at", null()).set("updated_at", now())).await?;
    confirm_email(&db, "verify-cy", "usr_cy").await?;
    let cy = db.get("users", ["usr_cy"]).await?.expect("cy is there");
    assert_eq!(cy.get::<Option<String>>("email")?.as_deref(), Some("Ann@example.com"));
    assert!(!cy.is_null("email_verified_at")?);
    let token = db.get("one_time_tokens", [digest("verify-cy")]).await?.expect("the token is there");
    assert!(!token.is_null("used_at")?);
    Ok(())
}

/// The page of users after `cursor`, narrowed to those whose username, name
/// or email starts with what was typed. One row more than the page is asked
/// for: if it comes, there is another page.
fn user_page(cursor: &str, typed: &str, page: u64) -> Query {
    let pattern = format!("{}%", like_escape(&typed.to_lowercase()));
    Query::table("users")
        .range(KeyRange::new(Bound::excluded([cursor]), Bound::Unbounded))
        .filter(any([
            val(typed).eq(""),
            col("username").like(pattern.as_str()),
            col("name").lower().like(pattern.as_str()),
            col("email").lower().like(pattern.as_str()),
        ]))
        .columns(USER_COLUMNS)
        .limit(page + 1)
}

// R-Q4, R-Q7, R-T9: paging by a cursor over the primary key, with a search
// by prefix whose pattern is escaped.
#[tokio::test(flavor = "multi_thread")]
async fn the_admin_user_list_pages_by_id_and_searches_by_an_escaped_prefix() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let people = [
        ("usr_01", "ann", "Ann Lee", Some("lee@example.com")),
        ("usr_02", "anders", "Bo Anders", Some("bo@example.com")),
        ("usr_03", "carl", "ANNabel Carl", Some("carl@example.com")),
        ("usr_04", "dora", "Dora", Some("ANNie@example.com")),
        ("usr_05", "eve", "100% Eve", Some("eve@example.com")),
        ("usr_06", "finn", "100 Finn", Some("f_n@example.com")),
        ("usr_07", "gus", "back\\slash", Some("fxn@example.com")),
        ("usr_08", "hal", "Hal", None),
    ];
    for (id, username, name, email) in people {
        db.insert(person(id, username, email)).await?;
        db.update(Update::table("users").key([id]).set("name", name).set("updated_at", now())).await?;
    }
    let ids = |rows: Rows| column::<String>(&rows, "id");

    // Pages of three. The fourth row only says that more follow, and the
    // last id shown is the next cursor.
    assert_eq!(ids(db.query(user_page("", "", 3)).await?), ["usr_01", "usr_02", "usr_03", "usr_04"]);
    assert_eq!(ids(db.query(user_page("usr_03", "", 3)).await?), ["usr_04", "usr_05", "usr_06", "usr_07"]);
    assert_eq!(ids(db.query(user_page("usr_06", "", 3)).await?), ["usr_07", "usr_08"]);
    // A cursor that is not an id lists what sorts after it.
    assert_eq!(ids(db.query(user_page("usr_045", "", 3)).await?), ["usr_05", "usr_06", "usr_07", "usr_08"]);
    assert!(db.query(user_page("zzz", "", 3)).await?.is_empty());

    // What was typed is matched, lower-cased, against the start of the
    // username, of the name and of the address. A user without an address
    // is neither matched nor an error.
    let ann = ["usr_01", "usr_03", "usr_04"];
    assert_eq!(ids(db.query(user_page("", "ann", 10)).await?), ann);
    assert_eq!(ids(db.query(user_page("", "ANN", 10)).await?), ann);
    assert_eq!(ids(db.query(user_page("", "hal", 10)).await?), ["usr_08"]);
    assert_eq!(ids(db.query(user_page("", "ann", 1)).await?), ["usr_01", "usr_03"]);
    assert_eq!(ids(db.query(user_page("usr_01", "ann", 1)).await?), ["usr_03", "usr_04"]);
    assert_eq!(ids(db.query(user_page("usr_03", "ann", 1)).await?), ["usr_04"]);

    // The characters a pattern gives meaning to are matched as themselves.
    assert_eq!(ids(db.query(user_page("", "100%", 10)).await?), ["usr_05"]);
    assert_eq!(ids(db.query(user_page("", "f_n", 10)).await?), ["usr_06"]);
    assert_eq!(ids(db.query(user_page("", "back\\", 10)).await?), ["usr_07"]);
    assert!(db.query(user_page("", "%", 10)).await?.is_empty());

    // Unescaped they are wildcards, and a pattern minds the case.
    let loose = db.query(Query::table("users").filter(col("email").like("f_n%")).columns(["id"])).await?;
    assert_eq!(ids(loose), ["usr_06", "usr_07"]);
    assert!(db.query(Query::table("users").filter(col("name").like("ann%"))).await?.is_empty());
    Ok(())
}

/// The members of a service as its client may list them: enabled users
/// only, found by the start of a username or name or by a whole address.
/// Reading `access` through its `(service, user_id)` index gives them in
/// the order of their ids.
fn member_page(service: &str, cursor: &str, typed: &str, page: u64) -> Query {
    let pattern = format!("{}%", like_escape(&typed.to_lowercase()));
    Query::table("access")
        .alias("a")
        .index("access_service", KeyRange::within([service], Bound::excluded([cursor]), Bound::Unbounded))
        .join(Query::table("users").alias("u").key([col("a.user_id")]).filter(col("u.disabled_at").is_null()))
        .filter(any([
            val(typed).eq(""),
            col("u.username").like(pattern.as_str()),
            col("u.name").lower().like(pattern.as_str()),
            col("u.email").lower().eq(val(typed).lower()),
        ]))
        .select("id", col("u.id"))
        .select("username", col("u.username"))
        .select("name", col("u.name"))
        .select("role", col("a.role"))
        .limit(page + 1)
}

/// A user and, if they have one, their role on a service.
fn with_role(user: Query, service: &str) -> Query {
    user.alias("u")
        .left_join(Query::table("access").alias("a").key([col("u.id"), val(service)]))
        .select("id", col("u.id"))
        .select("role", col("a.role"))
}

// R-Q2, R-Q4, R-Q7, R-Q9: an inner join and a left join by key, paged by a
// cursor through a two-column index.
#[tokio::test(flavor = "multi_thread")]
async fn a_clients_user_list_joins_roles_to_enabled_users() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let people = [
        ("usr_01", "ann", "Ann Lee", Some("Ann@Example.com"), Some("admin")),
        ("usr_02", "bob", "Bob", None, Some("member")),
        ("usr_03", "cy", "Cy", Some("cy@example.com"), Some("member")),
        ("usr_04", "dee", "Andrea Dee", Some("dee@example.com"), Some("member")),
        ("usr_05", "eli", "Eli", Some("eli@example.com"), None),
    ];
    for (id, username, name, email, files_role) in people {
        db.insert(person(id, username, email)).await?;
        db.update(Update::table("users").key([id]).set("name", name).set("updated_at", now())).await?;
        if let Some(role) = files_role {
            db.insert(grant_role(id, "files", role, "usr_admin")).await?;
        }
    }
    db.insert(grant_role("usr_05", "chat", "member", "usr_admin")).await?;
    db.insert(grant_role("usr_01", "chat", "member", "usr_admin")).await?;
    db.update(Update::table("users").key(["usr_03"]).set("disabled_at", Timestamp::now()).set("updated_at", now())).await?;

    // Each row carries columns of both tables. Cy has a role but is
    // disabled; Eli is enabled but has no role on files.
    let files = db.query(member_page("files", "", "", 10)).await?;
    assert_eq!(files.columns(), ["id", "username", "name", "role"]);
    assert_eq!(column::<String>(&files, "id"), ["usr_01", "usr_02", "usr_04"]);
    assert_eq!(column::<String>(&files, "name"), ["Ann Lee", "Bob", "Andrea Dee"]);
    assert_eq!(column::<String>(&files, "role"), ["admin", "member", "member"]);
    let chat = db.query(member_page("chat", "", "", 10)).await?;
    assert_eq!(column::<String>(&chat, "id"), ["usr_01", "usr_05"]);
    assert_eq!(column::<String>(&chat, "role"), ["member", "member"]);

    // The limit counts rows that survived the join and the filter.
    let ids = |rows: Rows| column::<String>(&rows, "id");
    assert_eq!(ids(db.query(member_page("files", "", "", 1)).await?), ["usr_01", "usr_02"]);
    assert_eq!(ids(db.query(member_page("files", "usr_01", "", 1)).await?), ["usr_02", "usr_04"]);
    assert_eq!(ids(db.query(member_page("files", "usr_02", "", 1)).await?), ["usr_04"]);
    assert!(db.query(member_page("files", "usr_04", "", 1)).await?.is_empty());
    assert!(db.query(member_page("wiki", "", "", 10)).await?.is_empty());

    // A prefix finds usernames and names; an address only whole, in any
    // case. Bob has no address, which matches nothing and breaks nothing.
    assert_eq!(ids(db.query(member_page("files", "", "an", 10)).await?), ["usr_01", "usr_04"]);
    assert_eq!(ids(db.query(member_page("files", "", "ANN@EXAMPLE.COM", 10)).await?), ["usr_01"]);
    assert!(db.query(member_page("files", "", "ann@", 10)).await?.is_empty());
    assert_eq!(ids(db.query(member_page("files", "", "bob", 10)).await?), ["usr_02"]);
    assert!(db.query(member_page("files", "", "cy", 10)).await?.is_empty());

    // The left join keeps a user who has no role.
    let eli = db.query_one(with_role(Query::table("users").key(["usr_05"]), "files")).await?;
    assert_eq!((eli.get::<String>("id")?, eli.get::<Option<String>>("role")?), ("usr_05".to_string(), None));
    let ann = db.query_one(with_role(Query::table("users").index("users_username_key", KeyRange::eq(["ann"])), "files")).await?;
    assert_eq!((ann.get::<String>("id")?, ann.get::<Option<String>>("role")?), ("usr_01".to_string(), Some("admin".to_string())));
    assert!(db.query_opt(with_role(Query::table("users").key(["usr_nobody"]), "files")).await?.is_none());
    Ok(())
}

// R-Q3, R-Q6, R-Q15: many lookups in one index by one query, as many as ten
// thousand, and a filter on a function of a column.
#[tokio::test(flavor = "multi_thread")]
async fn an_import_learns_in_one_read_which_names_addresses_and_keys_exist() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", Some("Ann@Example.com"))).await?;
    db.insert(person("usr_bob", "bob", None)).await?;
    db.insert(person("usr_zoe", "zoe", Some("ZOË@Example.com"))).await?;
    db.insert(new_credential("key_1", "usr_ann", "api_key", "files", "secret-1", None)).await?;
    db.insert(new_credential("key_2", "usr_ann", "api_key", "files", "secret-2", None)).await?;

    // One index lookup per name, answered in the order asked.
    let wanted = ["zoe", "nobody", "ann", "zed"];
    let taken =
        db.query(Query::table("users").index_ranges("users_username_key", wanted.map(|name| KeyRange::eq([name]))).columns(["username"])).await?;
    assert_eq!(taken.column::<String>()?, ["zoe", "ann"]);
    // The same as a filter reads the table, in the order of its key.
    let scanned = db.query(Query::table("users").filter(col("username").in_list(wanted.to_vec())).columns(["username"])).await?;
    assert_eq!(scanned.column::<String>()?, ["ann", "zoe"]);

    // An import checks up to ten thousand names at once.
    let mut many: Vec<String> = (0..10_000).map(|n| format!("user-{n:05}")).collect();
    many[5_000] = "bob".to_string();
    let taken = db
        .query(Query::table("users").index_ranges("users_username_key", many.iter().map(|name| KeyRange::eq([name.as_str()]))).columns(["username"]))
        .await?;
    assert_eq!(taken.column::<String>()?, ["bob"]);

    // The caller lower-cases the addresses it asks about and the server
    // lower-cased the ones it indexed. The two must agree beyond ASCII.
    let asked: Vec<String> = ["ANN@example.com", "Zoë@EXAMPLE.com", "new@example.com"].iter().map(|address| address.to_lowercase()).collect();
    let used = db
        .query(
            Query::table("users")
                .index_ranges("users_email", asked.iter().map(|address| KeyRange::eq([address.as_str()])))
                .select("email", col("email").lower()),
        )
        .await?;
    assert_eq!(used.column::<String>()?, ["ann@example.com", "zoë@example.com"]);

    // The keys of an import that are already there, compared as the hex
    // text the import carries.
    let carried = vec![hex(&digest("secret-2")), hex(&digest("secret-9"))];
    let as_hex = || call(Func::Hex, [col("secret_hash")]);
    let known = db.query(Query::table("credentials").filter(as_hex().in_list(carried.clone())).select("h", as_hex())).await?;
    assert_eq!(known.column::<String>()?, [hex(&digest("secret-2"))]);
    // The same through the index, which is over the bytes themselves.
    let candidates = [digest("secret-2"), digest("secret-9")].map(|hash| KeyRange::eq([hash]));
    let known = db.query(Query::table("credentials").index_ranges("credentials_secret_hash_key", candidates).select("h", as_hex())).await?;
    assert_eq!(known.column::<String>()?, [hex(&digest("secret-2"))]);
    Ok(())
}

// R-W6, R-Q8, R-T3: updates by key set columns to a value, to the server's
// time and to NULL, and return the old value beside the new.
#[tokio::test(flavor = "multi_thread")]
async fn a_user_update_writes_its_columns_and_the_servers_time() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", Some("ann@example.com"))).await?;
    db.insert(person("usr_bob", "bob", None)).await?;
    db.insert(bot("usr_bot", "ann-bot", "usr_ann")).await?;
    let ann = db.get("users", ["usr_ann"]).await?.expect("ann is there");
    let (created, first_update): (Timestamp, Timestamp) = (ann.get("created_at")?, ann.get("updated_at")?);

    // A rename, here also returning the name that was replaced.
    let rename = Update::table("users").key(["usr_ann"]).set("name", "Ann Lee").set("updated_at", now());
    let renamed = db.update(rename.returning(["name", "created_at", "updated_at"]).returning_expr("was", old("name"))).await?;
    assert_eq!(renamed.affected(), 1);
    let renamed = renamed.one()?;
    assert_eq!((renamed.get::<String>("was")?, renamed.get::<String>("name")?), ("ann".to_string(), "Ann Lee".to_string()));
    assert_eq!(renamed.get::<Timestamp>("created_at")?, created);
    let updated: Timestamp = renamed.get("updated_at")?;
    assert!(updated >= first_update && updated <= db.now().await?);

    // Disabling stores the caller's own clock, to the microsecond; enabling
    // stores NULL.
    let at = ts("2026-03-05T10:20:30.123456Z");
    db.update(Update::table("users").key(["usr_ann"]).set("disabled_at", at).set("updated_at", now())).await?;
    assert_eq!(db.get("users", ["usr_ann"]).await?.expect("ann is there").get::<Option<Timestamp>>("disabled_at")?, Some(at));
    db.update(Update::table("users").key(["usr_ann"]).set("disabled_at", null()).set("updated_at", now())).await?;
    assert!(db.get("users", ["usr_ann"]).await?.expect("ann is there").is_null("disabled_at")?);

    // A new password hash leaves the other columns alone.
    db.update(Update::table("users").key(["usr_ann"]).set("password_hash", "$argon2id$rehashed").set("updated_at", now())).await?;
    let ann = db.get("users", ["usr_ann"]).await?.expect("ann is there");
    assert_eq!(ann.get::<Option<String>>("password_hash")?.as_deref(), Some("$argon2id$rehashed"));
    assert_eq!(ann.get::<String>("name")?, "Ann Lee");

    // The bot changes hands, and the index of owners follows.
    let owned_by = |owner: &str| Query::table("users").index("users_owner", KeyRange::eq([owner])).columns(["id"]);
    assert_eq!(db.query(owned_by("usr_ann")).await?.column::<String>()?, ["usr_bot"]);
    db.update(Update::table("users").key(["usr_bot"]).set("owner_id", "usr_bob").set("updated_at", now())).await?;
    assert!(db.query(owned_by("usr_ann")).await?.is_empty());
    assert_eq!(db.query(owned_by("usr_bob")).await?.column::<String>()?, ["usr_bot"]);

    // An update of a user who is not there changes nothing and is not an
    // error.
    let nobody = db.update(Update::table("users").key(["usr_nobody"]).set("name", "Nobody").set("updated_at", now())).await?;
    assert_eq!(nobody.affected(), 0);
    Ok(())
}

/// Activating an account: the token is used, the password set if the
/// account still waits for one at the token's address, the change logged.
/// When the update of the account matches nothing the whole transaction is
/// given up, so that the token is not used.
async fn activate(db: &Client, token: &str, password_hash: &str) -> DbResult<bool> {
    let tx = db.begin().await?;
    let Some(used) = tx.update(consume_token(token, "activate")).await?.opt() else {
        tx.rollback().await?;
        return Ok(false);
    };
    let (user, address): (String, Option<String>) = (used.get("user_id")?, used.get("email")?);
    let activated = tx
        .update(
            Update::table("users")
                .key([user.as_str()])
                .filter(all([col("password_hash").is_null(), col("email").is_not_null(), col("email").lower().eq(val(address).lower())]))
                .set("password_hash", password_hash)
                .set("email_verified_at", now())
                .set("updated_at", now())
                .returning_expr("one", 1),
        )
        .await?;
    if activated.is_empty() {
        tx.rollback().await?;
        return Ok(false);
    }
    tx.insert(audit_entry(Some(&user), "user.activate", Some(&user), json!({}))).await?;
    tx.commit().await?;
    Ok(true)
}

// R-W7, R-X1, R-X7: when a conditional update matches nothing its
// transaction is given up, and the writes of one transaction carry one time.
#[tokio::test(flavor = "multi_thread")]
async fn an_activation_that_matches_no_account_leaves_its_token_unused() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(user_row("usr_new", "new", Some("New@Example.com"), "person", None, None)).await?;
    db.insert(one_time_token("activate-new", "activate", "usr_new", Some("new@example.com"), 3600)).await?;

    // The account's address was changed after the link was sent: nothing
    // is activated, and nothing of the attempt remains.
    db.update(Update::table("users").key(["usr_new"]).set("email", "elsewhere@example.com").set("updated_at", now())).await?;
    assert!(!activate(&db, "activate-new", "$argon2id$first").await?);
    let token = db.get("one_time_tokens", [digest("activate-new")]).await?.expect("the token is there");
    assert!(token.is_null("used_at")?);
    assert!(db.get("users", ["usr_new"]).await?.expect("the user is there").is_null("password_hash")?);
    assert_eq!(rows_of(&db, Query::table("audit")).await?, 0);

    // With the address back, the same token activates the account. The
    // three writes carry one and the same time.
    db.update(Update::table("users").key(["usr_new"]).set("email", "New@Example.com").set("updated_at", now())).await?;
    assert!(activate(&db, "activate-new", "$argon2id$first").await?);
    let user = db.get("users", ["usr_new"]).await?.expect("the user is there");
    assert_eq!(user.get::<Option<String>>("password_hash")?.as_deref(), Some("$argon2id$first"));
    let verified: Timestamp = user.get("email_verified_at")?;
    let token = db.get("one_time_tokens", [digest("activate-new")]).await?.expect("the token is there");
    assert_eq!(token.get::<Timestamp>("used_at")?, verified);
    let logged = db.query_one(Query::table("audit")).await?;
    assert_eq!((logged.get::<String>("action")?, logged.get::<Timestamp>("at")?), ("user.activate".to_string(), verified));

    // A second visit finds the token used, and a password already set
    // would not be replaced either.
    assert!(!activate(&db, "activate-new", "$argon2id$second").await?);
    db.insert(one_time_token("activate-again", "activate", "usr_new", Some("new@example.com"), 3600)).await?;
    assert!(!activate(&db, "activate-again", "$argon2id$second").await?);
    assert!(db.get("one_time_tokens", [digest("activate-again")]).await?.expect("the token is there").is_null("used_at")?);
    assert_eq!(
        db.get("users", ["usr_new"]).await?.expect("the user is there").get::<Option<String>>("password_hash")?.as_deref(),
        Some("$argon2id$first")
    );
    Ok(())
}

/// An invitation, good for a while by the server's clock.
fn new_invite(id: &str, email: &str, service: &str, role: &str, token: &str, lifetime_seconds: i64) -> Insert {
    Insert::into("invites")
        .set("id", id)
        .set("email", email)
        .set("service", service)
        .set("role", role)
        .set("token_hash", digest(token))
        .set("invited_by", "usr_admin")
        .set("expires_at", now() + seconds(lifetime_seconds))
        .returning(INVITE_COLUMNS)
}

/// Accepts an invitation once, while it is good.
fn accept_invite(token: &str, user: &str) -> Update {
    Update::table("invites")
        .index("invites_token_hash_key", KeyRange::eq([digest(token)]))
        .filter(col("accepted_at").is_null().and(col("expires_at").gt(now())))
        .set("accepted_by", user)
        .set("accepted_at", now())
        .returning(INVITE_COLUMNS)
}

// R-S6, R-W10, R-X1: deleting a row cascades, sets NULL or is refused, as
// each foreign key says, and a rollback brings every row back.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_user_empties_every_table_that_points_at_them() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    for name in ["ann", "bob"] {
        let user = format!("usr_{name}");
        let session = format!("ses_{name}");
        db.insert(person(&user, name, Some(&format!("{name}@example.com")))).await?;
        db.insert(grant_role(&user, "files", "member", "usr_admin")).await?;
        db.insert(grant_role(&user, "chat", "member", "usr_admin")).await?;
        db.insert(Insert::into("totp").set("user_id", user.as_str()).set("secret_enc", digest("totp"))).await?;
        let passkey = Insert::into("passkeys").set("id", format!("pk_{name}")).set("user_id", user.as_str()).set("name", "laptop");
        db.insert(passkey.set("credential_id", digest(&format!("passkey-{name}"))).set("public_key", digest("cose"))).await?;
        db.insert(Insert::into("recovery_codes").set("user_id", user.as_str()).set("code_hash", digest(&format!("recovery-{name}")))).await?;
        db.insert(new_session(&session, &user, &format!("cookie-{name}"))).await?;
        db.insert(session_token(&format!("web-{name}"), "files-web", &session)).await?;
        db.insert(new_code(&format!("code-{name}"), "files-web", &session, 60)).await?;
        db.insert(public_token(&format!("ios-{name}"), &format!("ses_ios_{name}"), &user, "files-ios")).await?;
        db.insert(new_credential(&format!("key_{name}"), &user, "api_key", "files", &format!("secret-{name}"), None)).await?;
        db.insert(one_time_token(&format!("reset-{name}"), "reset", &user, None, 3600)).await?;
        db.insert(new_invite(&format!("inv_{name}"), &format!("{name}@example.com"), "files", "member", &format!("invite-{name}"), 3600)).await?;
        assert_eq!(db.update(accept_invite(&format!("invite-{name}"), &user)).await?.affected(), 1);
        db.insert(audit_entry(Some(&user), "login.ok", Some(&user), json!({}))).await?;
    }
    db.insert(bot("usr_bot", "ann-bot", "usr_ann")).await?;
    let children = [
        ("access", 2),
        ("totp", 1),
        ("passkeys", 1),
        ("recovery_codes", 1),
        ("sessions", 1),
        ("refresh_tokens", 2),
        ("credentials", 1),
        ("one_time_tokens", 1),
    ];
    let of = |table: &str, user: &str| Query::table(table).filter(col("user_id").eq(user));

    // A user who still owns a bot cannot be deleted, and the refused
    // statement takes nothing with it. The service counts the bots first.
    let err = db.delete(Delete::from("users").key(["usr_ann"])).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::ForeignKeyViolation);
    for (table, rows) in children {
        assert_eq!(rows_of(&db, of(table, "usr_ann")).await?, rows, "{table}");
    }
    db.update(Update::table("users").key(["usr_bot"]).set("owner_id", "usr_bob").set("updated_at", now())).await?;

    // The delete, the services' event and the audit line are one
    // transaction.
    db.transaction(|tx| async move {
        assert_eq!(rows_of(&tx, Query::table("users").index("users_owner", KeyRange::eq(["usr_ann"]))).await?, 0);
        tx.insert(event("files", "user-deleted")).await?;
        let gone = tx.delete(Delete::from("users").key(["usr_ann"])).await?;
        assert_eq!(gone.affected(), 1);
        tx.insert(audit_entry(Some("usr_admin"), "user.delete", Some("usr_ann"), json!({"username": "ann"}))).await?;
        Ok::<_, DbError>(())
    })
    .await?;

    assert!(db.get("users", ["usr_ann"]).await?.is_none());
    for (table, rows) in children {
        assert_eq!(rows_of(&db, of(table, "usr_ann")).await?, 0, "{table}");
        assert_eq!(rows_of(&db, of(table, "usr_bob")).await?, rows, "{table}");
    }
    // Through the session: a code names no user, only the session that
    // went with her.
    assert!(db.get("auth_codes", [digest("code-ann")]).await?.is_none());
    assert!(db.get("auth_codes", [digest("code-bob")]).await?.is_some());
    assert!(db.get("refresh_tokens", [digest("web-ann")]).await?.is_none());

    // The invitation she accepted stays, no longer saying who accepted.
    let invite = db.get("invites", ["inv_ann"]).await?.expect("the invitation is kept");
    assert!(invite.is_null("accepted_by")? && !invite.is_null("accepted_at")?);
    let invite = db.get("invites", ["inv_bob"]).await?.expect("the invitation is kept");
    assert_eq!(invite.get::<Option<String>>("accepted_by")?.as_deref(), Some("usr_bob"));

    // The log has no foreign key and keeps her lines.
    let logged = db.query(Query::table("audit").index("audit_user", KeyRange::eq(["usr_ann"])).columns(["action"])).await?;
    assert_eq!(logged.column::<String>()?, ["login.ok", "user.delete"]);
    assert_eq!(rows_of(&db, Query::table("outbox")).await?, 1);

    // A delete that is rolled back brings the children back with it.
    let tx = db.begin().await?;
    tx.update(Update::table("users").key(["usr_bot"]).set("owner_id", null())).await?;
    assert_eq!(tx.delete(Delete::from("users").key(["usr_bob"])).await?.affected(), 1);
    assert_eq!(rows_of(&tx, of("access", "usr_bob")).await?, 0);
    tx.rollback().await?;
    for (table, rows) in children {
        assert_eq!(rows_of(&db, of(table, "usr_bob")).await?, rows, "{table}");
    }
    assert_eq!(db.get("users", ["usr_bot"]).await?.expect("the bot is there").get::<Option<String>>("owner_id")?.as_deref(), Some("usr_bob"));
    Ok(())
}

// R-Q1, R-Q9, R-W4, R-W7: an upsert on a two-column key, a read through a
// join, and a delete that returns what it removed.
#[tokio::test(flavor = "multi_thread")]
async fn a_role_is_granted_by_upsert_read_for_enabled_users_and_revoked_once() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", None)).await?;
    db.insert(person("usr_bob", "bob", None)).await?;

    // The first grant inserts.
    let first = db.insert(grant_role("usr_ann", "files", "member", "usr_admin")).await?;
    assert_eq!(first.affected(), 1);
    let first = first.one()?;
    assert_eq!(first.columns(), ["service", "role", "granted_by", "granted_at"]);
    assert_eq!((first.get::<String>("role")?, first.get::<String>("granted_by")?), ("member".to_string(), "usr_admin".to_string()));

    // The role is read and then replaced in one transaction, where Postgres
    // would lock the row in between. The second grant updates the row it
    // found: role and granter from the proposed row, the time from the
    // clock.
    let second = db
        .transaction(|tx| async move {
            let current = tx.query_one(Query::table("access").key(["usr_ann", "files"]).columns(["role"])).await?;
            assert_eq!(current.get::<String>("role")?, "member");
            tx.insert(grant_role("usr_ann", "files", "admin", "cli_files")).await?.one()
        })
        .await?;
    assert_eq!((second.get::<String>("role")?, second.get::<String>("granted_by")?), ("admin".to_string(), "cli_files".to_string()));
    assert!(second.get::<Timestamp>("granted_at")? >= first.get::<Timestamp>("granted_at")?);
    assert_eq!(rows_of(&db, Query::table("access")).await?, 1);

    // The row as it now is, and whether there is one.
    let row = db.query_one(Query::table("access").key(["usr_ann", "files"]).columns(["service", "role", "granted_by", "granted_at"])).await?;
    assert_eq!(row, second);
    let has_access = |user: &str, service: &str| Query::table("access").key([user, service]).select("one", 1);
    assert_eq!(db.query(has_access("usr_ann", "files")).await?.len(), 1);
    assert!(db.query(has_access("usr_ann", "chat")).await?.is_empty());
    assert!(db.query(has_access("usr_bob", "files")).await?.is_empty());

    // A user's roles come in the order of the service's name, which is the
    // order of the key.
    db.insert(grant_role("usr_ann", "wiki", "member", "usr_admin")).await?;
    db.insert(grant_role("usr_ann", "chat", "owner", "usr_admin")).await?;
    db.insert(grant_role("usr_bob", "files", "member", "usr_admin")).await?;
    let roles = db.query(Query::table("access").range(KeyRange::eq(["usr_ann"])).columns(["service", "role", "granted_by", "granted_at"])).await?;
    assert_eq!(column::<String>(&roles, "service"), ["chat", "files", "wiki"]);
    assert_eq!(column::<String>(&roles, "role"), ["owner", "admin", "member"]);

    // A disabled user has no role to speak of, though the row is still
    // there.
    let view = |user: &str, service: &str| {
        Query::table("access")
            .alias("a")
            .key([user, service])
            .join(Query::table("users").alias("u").key([col("a.user_id")]).filter(col("u.disabled_at").is_null()))
            .select("service", col("a.service"))
            .select("role", col("a.role"))
            .select("granted_by", col("a.granted_by"))
            .select("granted_at", col("a.granted_at"))
    };
    assert_eq!(db.query_one(role_of("usr_ann", "files")).await?.get::<String>("role")?, "admin");
    assert_eq!(db.query_one(view("usr_ann", "files")).await?, second);
    assert!(db.query_opt(role_of("usr_ann", "calendar")).await?.is_none());
    db.update(Update::table("users").key(["usr_ann"]).set("disabled_at", Timestamp::now()).set("updated_at", now())).await?;
    assert!(db.query_opt(role_of("usr_ann", "files")).await?.is_none());
    assert!(db.query_opt(view("usr_ann", "files")).await?.is_none());
    assert_eq!(db.query(has_access("usr_ann", "files")).await?.len(), 1);
    assert_eq!(db.query_one(role_of("usr_bob", "files")).await?.get::<String>("role")?, "member");

    // The delete says what the role was, and only once.
    let revoked = db.delete(Delete::from("access").key(["usr_ann", "files"]).returning(["role"])).await?;
    assert_eq!(revoked.affected(), 1);
    assert_eq!(revoked.one()?.get::<String>("role")?, "admin");
    let again = db.delete(Delete::from("access").key(["usr_ann", "files"]).returning(["role"])).await?;
    assert!(again.is_empty());
    assert_eq!(again.affected(), 0);
    assert_eq!(rows_of(&db, Query::table("access").range(KeyRange::eq(["usr_ann"]))).await?, 2);
    Ok(())
}

/// Enrols a user in TOTP, replacing what they had.
fn enrol_totp(user: &str, sealed_secret: &[u8], step: i64) -> Insert {
    Insert::into("totp")
        .set("user_id", user)
        .set("secret_enc", sealed_secret)
        .set("last_step", step)
        .on_conflict_update(PRIMARY)
        .update_excluded(["secret_enc", "last_step"])
        .update("created_at", now())
}

/// Registers a passkey unless its credential id is already known.
fn new_passkey(id: &str, user: &str, credential_id: &[u8], name: &str) -> Insert {
    Insert::into("passkeys")
        .set("id", id)
        .set("user_id", user)
        .set("credential_id", credential_id)
        .set("public_key", digest("cose-key"))
        .set("sign_count", 0)
        .set("name", name)
        .on_conflict_do_nothing_on("passkeys_credential_id_key")
        .returning(["id"])
}

/// One recovery code of a user, not yet spent.
fn recovery_code(user: &str, code: &str) -> Insert {
    Insert::into("recovery_codes").set("user_id", user).set("code_hash", digest(code))
}

/// Spends a recovery code that has not been spent.
fn spend_code(user: &str, code: &str) -> Update {
    Update::table("recovery_codes")
        .key([val(user), val(digest(code))])
        .filter(col("used_at").is_null())
        .set("used_at", now())
        .returning_expr("one", 1)
}

/// Which second factors a user has, in one row. SQL would select three
/// subqueries from nothing; a query here starts from a table, so this one
/// starts from the user's own row and the subqueries read it.
fn factors(user: &str) -> Query {
    let unused_codes = Query::table("recovery_codes").range(KeyRange::eq([col("u.id")])).filter(col("used_at").is_null());
    Query::table("users")
        .alias("u")
        .key([user])
        .select("totp", exists(Query::table("totp").key([col("u.id")])))
        .select("passkeys", scalar(Query::table("passkeys").index("passkeys_user", KeyRange::eq([col("u.id")])).select("n", count())))
        .select("recovery_codes", scalar(unused_codes.select("n", count())))
}

// R-Q10, R-Q11: one row made of an EXISTS and two counting subqueries, each
// reading the outer row.
#[tokio::test(flavor = "multi_thread")]
async fn a_users_second_factors_are_counted_in_one_row() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", None)).await?;
    db.insert(person("usr_bob", "bob", None)).await?;
    let summary = |row: Row| -> DbResult<(bool, i64, i64)> { Ok((row.get("totp")?, row.get("passkeys")?, row.get("recovery_codes")?)) };

    let nothing = db.query_one(factors("usr_ann")).await?;
    assert_eq!(nothing.columns(), ["totp", "passkeys", "recovery_codes"]);
    assert_eq!(summary(nothing)?, (false, 0, 0));

    db.insert(enrol_totp("usr_ann", b"v1:ann", 0)).await?;
    db.insert(new_passkey("pk_1", "usr_ann", b"credential-1", "laptop")).await?;
    db.insert(new_passkey("pk_2", "usr_ann", b"credential-2", "phone")).await?;
    for n in 0..10 {
        db.insert(recovery_code("usr_ann", &format!("ann-{n}"))).await?;
    }
    for n in 0..3 {
        db.update(spend_code("usr_ann", &format!("ann-{n}"))).await?;
    }
    // Somebody else's factors are not hers.
    db.insert(new_passkey("pk_3", "usr_bob", b"credential-3", "tablet")).await?;
    for n in 0..5 {
        db.insert(recovery_code("usr_bob", &format!("bob-{n}"))).await?;
    }

    assert_eq!(summary(db.query_one(factors("usr_ann")).await?)?, (true, 2, 7));
    assert_eq!(summary(db.query_one(factors("usr_bob")).await?)?, (false, 1, 5));
    assert!(db.query_opt(factors("usr_nobody")).await?.is_none());

    // The codes a user has left.
    let left = Query::table("recovery_codes").range(KeyRange::eq(["usr_ann"])).filter(col("used_at").is_null());
    assert_eq!(rows_of(&db, left).await?, 7);
    Ok(())
}

// R-W4, R-W7, R-X1, R-X3: a compare-and-set update that one of several
// concurrent requests wins, an upsert, and a read and a write of one row in
// one transaction.
#[tokio::test(flavor = "multi_thread")]
async fn a_totp_step_is_accepted_once_however_many_requests_carry_it() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", None)).await?;
    db.insert(person("usr_bob", "bob", None)).await?;

    // Enrolling again replaces the secret and the step.
    assert_eq!(db.insert(enrol_totp("usr_ann", b"v1:first", 0)).await?.affected(), 1);
    assert_eq!(db.insert(enrol_totp("usr_ann", b"v1:second", 100)).await?.affected(), 1);
    assert_eq!(rows_of(&db, Query::table("totp")).await?, 1);
    let stored = db.query_one(Query::table("totp").key(["usr_ann"]).columns(["secret_enc", "last_step"])).await?;
    assert_eq!((stored.get::<Vec<u8>>("secret_enc")?, stored.get::<i64>("last_step")?), (b"v1:second".to_vec(), 100));

    // A step is taken only if it is later than the last one taken.
    let accept = |step: i64| Update::table("totp").key(["usr_ann"]).filter(col("last_step").lt(step)).set("last_step", step).returning_expr("one", 1);
    assert_eq!(db.update(accept(101)).await?.len(), 1);
    assert!(db.update(accept(101)).await?.is_empty());
    assert!(db.update(accept(100)).await?.is_empty());

    // Five requests with the same code at once: one signs in.
    let mut requests = Vec::new();
    for _ in 0..5 {
        let db = db.clone();
        requests.push(tokio::spawn(async move { db.update(accept(102)).await.map(|rows| rows.affected()) }));
    }
    let mut accepted = 0;
    for request in requests {
        accepted += request.await.expect("the task ran")?;
    }
    assert_eq!(accepted, 1);
    assert_eq!(db.get("totp", ["usr_ann"]).await?.expect("ann is enrolled").get::<i64>("last_step")?, 102);

    // Re-encrypting the secrets under a new key reads the ids in order,
    // then reads and rewrites each row in a transaction of its own, so that
    // no secret changes between its read and its write.
    db.insert(enrol_totp("usr_bob", b"v1:bob", 7)).await?;
    let ids: Vec<String> = db.query(Query::table("totp").select("id", col("user_id"))).await?.column()?;
    assert_eq!(ids, ["usr_ann", "usr_bob"]);
    for id in &ids {
        db.transaction(|tx| async move {
            let sealed: Vec<u8> = tx.query_one(Query::table("totp").key([id]).select("sealed", col("secret_enc"))).await?.get("sealed")?;
            let resealed = [b"v2:".as_slice(), &sealed[3..]].concat();
            tx.update(Update::table("totp").key([id]).set("secret_enc", resealed)).await?;
            Ok::<_, DbError>(())
        })
        .await?;
    }
    let sealed: Vec<Vec<u8>> = db.query(Query::table("totp").columns(["secret_enc"])).await?.column()?;
    assert_eq!(sealed, [b"v2:second".to_vec(), b"v2:bob".to_vec()]);
    assert_eq!(db.get("totp", ["usr_ann"]).await?.expect("ann is enrolled").get::<i64>("last_step")?, 102);

    // Removing it says whether there was anything to remove.
    let remove = || Delete::from("totp").key(["usr_ann"]).returning_expr("one", 1);
    assert_eq!(db.delete(remove()).await?.len(), 1);
    assert!(db.delete(remove()).await?.is_empty());
    assert_eq!(rows_of(&db, Query::table("totp")).await?, 1);
    Ok(())
}

/// The passkey with this credential id, if its owner is a person who may
/// sign in.
fn passkey_for(credential_id: &[u8]) -> Query {
    Query::table("passkeys")
        .alias("p")
        .index("passkeys_credential_id_key", KeyRange::eq([credential_id]))
        .join(Query::table("users").alias("u").key([col("p.user_id")]).filter(col("u.disabled_at").is_null().and(col("u.kind").eq("person"))))
        .select("id", col("p.id"))
        .select("user_id", col("p.user_id"))
        .select("credential_id", col("p.credential_id"))
        .select("public_key", col("p.public_key"))
        .select("sign_count", col("p.sign_count"))
}

// R-Q1, R-Q9, R-W5, R-W7: an insert that does nothing on a conflict in one
// named index, and byte strings up to 1023 bytes long as unique keys.
#[tokio::test(flavor = "multi_thread")]
async fn a_passkey_is_registered_once_and_found_by_its_credential_id() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", None)).await?;
    db.insert(person("usr_bob", "bob", None)).await?;

    // The insert answers with the id it stored, or with nothing when the
    // authenticator is already registered, to anyone.
    let first = db.insert(new_passkey("pk_1", "usr_ann", b"credential-a", "laptop")).await?;
    assert_eq!((first.affected(), first.column::<String>()?), (1, vec!["pk_1".to_string()]));
    let taken = db.insert(new_passkey("pk_2", "usr_bob", b"credential-a", "borrowed")).await?;
    assert!(taken.is_empty());
    assert_eq!(taken.affected(), 0);
    assert_eq!(rows_of(&db, Query::table("passkeys")).await?, 1);
    // Only that index is forgiven: the same id again is an error.
    let err = db.insert(new_passkey("pk_1", "usr_ann", b"credential-b", "phone")).await.unwrap_err();
    assert!(err.is_unique_violation() && err.violates(PRIMARY), "{err}");

    // A credential id may be as long as 1023 bytes, and is still a key.
    let long = vec![7u8; 1023];
    let mut longer = long.clone();
    longer[1022] = 8;
    assert_eq!(db.insert(new_passkey("pk_3", "usr_ann", &long, "security key")).await?.len(), 1);
    assert_eq!(db.insert(new_passkey("pk_4", "usr_bob", &longer, "other key")).await?.len(), 1);
    assert!(db.insert(new_passkey("pk_5", "usr_bob", &long, "copy")).await?.is_empty());

    // Found by its credential id, however long.
    let found = db.query_one(passkey_for(&long)).await?;
    assert_eq!(
        (found.get::<String>("id")?, found.get::<String>("user_id")?, found.get::<i64>("sign_count")?),
        ("pk_3".to_string(), "usr_ann".to_string(), 0)
    );
    assert_eq!(found.get::<Vec<u8>>("credential_id")?, long);
    assert_eq!(found.get::<Vec<u8>>("public_key")?, digest("cose-key"));
    assert_eq!(db.query_one(passkey_for(b"credential-a")).await?.get::<String>("id")?, "pk_1");
    assert!(db.query_opt(passkey_for(b"credential-z")).await?.is_none());

    // A use is counted and timed.
    db.update(Update::table("passkeys").key(["pk_1"]).set("sign_count", 41).set("last_used_at", now())).await?;

    // A user's passkeys, oldest first; then just their credential ids.
    let listed = db
        .query(Query::table("passkeys").index("passkeys_user", KeyRange::eq(["usr_ann"])).order_by(col("created_at")).columns([
            "id",
            "name",
            "created_at",
            "last_used_at",
        ]))
        .await?;
    assert_eq!(column::<String>(&listed, "id"), ["pk_1", "pk_3"]);
    assert_eq!(column::<String>(&listed, "name"), ["laptop", "security key"]);
    let used = column::<Option<Timestamp>>(&listed, "last_used_at");
    assert!(used[0].is_some() && used[1].is_none());
    let mut credential_ids: Vec<Vec<u8>> =
        db.query(Query::table("passkeys").index("passkeys_user", KeyRange::eq(["usr_ann"])).columns(["credential_id"])).await?.column()?;
    credential_ids.sort();
    assert_eq!(credential_ids, [long.clone(), b"credential-a".to_vec()]);

    // A passkey of a disabled user is not offered.
    db.update(Update::table("users").key(["usr_bob"]).set("disabled_at", Timestamp::now()).set("updated_at", now())).await?;
    assert!(db.query_opt(passkey_for(&longer)).await?.is_none());

    // Only its owner deletes a passkey, and learns its name.
    let delete = |id: &str, user: &str| Delete::from("passkeys").key([id]).filter(col("user_id").eq(user)).returning(["name"]);
    assert!(db.delete(delete("pk_1", "usr_bob")).await?.is_empty());
    assert_eq!(db.delete(delete("pk_1", "usr_ann")).await?.column::<String>()?, ["laptop"]);
    assert!(db.delete(delete("pk_1", "usr_ann")).await?.is_empty());

    // Clearing a user's second factors removes the rest of theirs.
    assert_eq!(db.delete(Delete::from("passkeys").index("passkeys_user", KeyRange::eq(["usr_ann"]))).await?.affected(), 1);
    assert_eq!(db.query(Query::table("passkeys").columns(["id"])).await?.column::<String>()?, ["pk_4"]);
    Ok(())
}

/// Replacing a user's recovery codes: the old ones go and ten new ones are
/// inserted one by one, in one transaction.
async fn regenerate_codes(db: &Client, user: &str, batch: &str) -> DbResult<()> {
    let tx = db.begin().await?;
    tx.delete(Delete::rows(Query::table("recovery_codes").range(KeyRange::eq([user])))).await?;
    for n in 0..10 {
        tx.insert(recovery_code(user, &format!("{batch}-{n}"))).await?;
    }
    tx.commit().await
}

// R-S2, R-W2, R-W7, R-X1, R-X2: a set of rows is replaced in one
// transaction that nobody sees half done, and a row is spent once by a
// conditional update.
#[tokio::test(flavor = "multi_thread")]
async fn recovery_codes_are_replaced_together_and_spent_once() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", None)).await?;
    db.insert(person("usr_bob", "bob", None)).await?;
    let codes = |user: &str| Query::table("recovery_codes").range(KeyRange::eq([user]));
    let unspent =
        |user: &str, code: &str| Query::table("recovery_codes").key([val(user), val(digest(code))]).filter(col("used_at").is_null()).select("one", 1);

    regenerate_codes(&db, "usr_ann", "first").await?;
    regenerate_codes(&db, "usr_bob", "bob").await?;
    assert_eq!(rows_of(&db, codes("usr_ann")).await?, 10);
    regenerate_codes(&db, "usr_ann", "second").await?;
    assert_eq!(rows_of(&db, codes("usr_ann")).await?, 10);
    assert!(db.query(unspent("usr_ann", "first-0")).await?.is_empty());
    assert_eq!(db.query(unspent("usr_ann", "second-0")).await?.len(), 1);
    assert_eq!(rows_of(&db, codes("usr_bob")).await?, 10);

    // Halfway through a replacement nobody else sees zero codes, or a
    // mixture: the old ten until the commit. Given up, they stay.
    let tx = db.begin().await?;
    tx.delete(Delete::rows(codes("usr_ann"))).await?;
    tx.insert(recovery_code("usr_ann", "third-0")).await?;
    assert_eq!(rows_of(&tx, codes("usr_ann")).await?, 1);
    assert_eq!(rows_of(&db, codes("usr_ann")).await?, 10);
    assert_eq!(db.query(unspent("usr_ann", "second-0")).await?.len(), 1);
    tx.rollback().await?;
    assert_eq!(rows_of(&db, codes("usr_ann")).await?, 10);
    assert!(db.query(unspent("usr_ann", "third-0")).await?.is_empty());

    // The same code twice in one set is refused by the two-column key.
    let err = db.insert(recovery_code("usr_ann", "second-0")).await.unwrap_err();
    assert!(err.is_unique_violation() && err.violates(PRIMARY), "{err}");

    // Looking does not spend a code; spending does, once, and only the code
    // of the user it belongs to.
    assert_eq!(db.query(unspent("usr_ann", "second-3")).await?.len(), 1);
    assert_eq!(db.query(unspent("usr_ann", "second-3")).await?.len(), 1);
    assert!(db.update(spend_code("usr_bob", "second-3")).await?.is_empty());
    assert_eq!(db.update(spend_code("usr_ann", "second-3")).await?.len(), 1);
    assert!(db.update(spend_code("usr_ann", "second-3")).await?.is_empty());
    assert!(db.query(unspent("usr_ann", "second-3")).await?.is_empty());
    assert!(db.update(spend_code("usr_ann", "never-issued")).await?.is_empty());

    // Nine of her ten codes are left.
    assert_eq!(rows_of(&db, codes("usr_ann").filter(col("used_at").is_null())).await?, 9);
    assert_eq!(rows_of(&db, codes("usr_ann")).await?, 10);
    Ok(())
}

/// The session behind a cookie, if it is live by the server's clock and its
/// user is a person who may sign in. Run for every request that carries a
/// session cookie.
fn load_session(token: &str) -> Query {
    Query::table("sessions")
        .alias("s")
        .index("sessions_token_hash_key", KeyRange::eq([digest(token)]))
        .join(Query::table("users").alias("u").key([col("s.user_id")]).filter(col("u.disabled_at").is_null().and(col("u.kind").eq("person"))))
        .filter(col("s.expires_at").gt(now()).and(col("s.ends_at").gt(now())))
        .select("id", col("s.id"))
        .select("user_id", col("s.user_id"))
        .select("amr", col("s.amr"))
        .select("auth_age", (now() - col("s.auth_time")) / seconds(1))
        .select("stale", col("s.last_seen_at").lt(now() - minutes(1)))
        .select("username", col("u.username"))
        .select("name", col("u.name"))
        .select("email", col("u.email"))
}

// R-Q1, R-Q8, R-Q9, R-T7: a lookup in a unique index over bytes, joined to
// another table by key, with arithmetic on times and a list of texts.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_is_loaded_by_its_token_hash_with_its_user_and_its_age() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", Some("ann@example.com"))).await?;
    db.insert(person("usr_off", "off", None)).await?;
    db.insert(bot("usr_bot", "ann-bot", "usr_ann")).await?;

    // The insert relies on the defaults for when the session was made and
    // seen.
    let before = db.now().await?;
    db.insert(new_session("ses_1", "usr_ann", "cookie-1")).await?;
    let after = db.now().await?;
    let stored = db.get("sessions", ["ses_1"]).await?.expect("the session is there");
    let (made, seen, signed_in): (Timestamp, Timestamp, Timestamp) =
        (stored.get("created_at")?, stored.get("last_seen_at")?, stored.get("auth_time")?);
    assert!(before <= made && made <= after);
    assert_eq!((seen, signed_in), (made, made));
    assert_eq!(stored.get::<Timestamp>("expires_at")?.micros() - made.micros(), 7 * DAY);
    assert_eq!(stored.get::<Timestamp>("ends_at")?.micros() - made.micros(), 30 * DAY);

    let loaded = db.query_one(load_session("cookie-1")).await?;
    assert_eq!(loaded.columns(), ["id", "user_id", "amr", "auth_age", "stale", "username", "name", "email"]);
    assert_eq!((loaded.get::<String>("id")?, loaded.get::<String>("user_id")?), ("ses_1".to_string(), "usr_ann".to_string()));
    assert_eq!(loaded.get::<Vec<String>>("amr")?, ["pwd"]);
    assert!((0..5).contains(&loaded.get::<i64>("auth_age")?));
    assert!(!loaded.get::<bool>("stale")?);
    assert_eq!((loaded.get::<String>("username")?, loaded.get::<Option<String>>("email")?), ("ann".to_string(), Some("ann@example.com".to_string())));

    // Signed in two minutes ago with two factors, last seen over a minute
    // ago: the age is whole seconds, and the session is due a touch.
    db.insert(new_session("ses_2", "usr_ann", "cookie-2")).await?;
    let aged = Update::table("sessions").key(["ses_2"]).set("auth_time", now() - seconds(125)).set("last_seen_at", now() - seconds(61));
    db.update(aged.set("amr", vec!["pwd", "otp", "mfa"])).await?;
    let loaded = db.query_one(load_session("cookie-2")).await?;
    assert!((125..130).contains(&loaded.get::<i64>("auth_age")?));
    assert!(loaded.get::<bool>("stale")?);
    assert_eq!(loaded.get::<Vec<String>>("amr")?, ["pwd", "otp", "mfa"]);
    db.update(Update::table("sessions").key(["ses_2"]).set("last_seen_at", now() - seconds(30))).await?;
    assert!(!db.query_one(load_session("cookie-2")).await?.get::<bool>("stale")?);

    // No session: an unknown cookie, one idle too long, one past its
    // absolute end, one of a disabled user, one of a bot. The rows are
    // still there; it is the read that leaves them out.
    assert!(db.query_opt(load_session("cookie-9")).await?.is_none());
    db.insert(new_session("ses_3", "usr_ann", "cookie-3")).await?;
    db.update(Update::table("sessions").key(["ses_3"]).set("expires_at", now() - seconds(1))).await?;
    db.insert(new_session("ses_4", "usr_ann", "cookie-4")).await?;
    db.update(Update::table("sessions").key(["ses_4"]).set("ends_at", now() - seconds(1))).await?;
    db.insert(new_session("ses_5", "usr_off", "cookie-5")).await?;
    assert!(db.query_opt(load_session("cookie-5")).await?.is_some());
    db.update(Update::table("users").key(["usr_off"]).set("disabled_at", Timestamp::now()).set("updated_at", now())).await?;
    db.insert(new_session("ses_6", "usr_bot", "cookie-6")).await?;
    for cookie in ["cookie-3", "cookie-4", "cookie-5", "cookie-6"] {
        assert!(db.query_opt(load_session(cookie)).await?.is_none(), "{cookie}");
    }
    assert_eq!(rows_of(&db, Query::table("sessions")).await?, 6);
    Ok(())
}

/// A session seen now stays good for `idle_days` more, but never past its
/// absolute end.
fn touch_session(id: &str, idle_days: i64) -> Update {
    Update::table("sessions").key([id]).set("last_seen_at", now()).set("expires_at", least([now() + days(idle_days), col("ends_at")]))
}

/// Signing in again in a browser that has a session keeps the session's id
/// and replaces everything else.
fn sign_in_again(id: &str, token: &str, amr: Vec<&str>) -> Update {
    Update::table("sessions")
        .key([id])
        .set("token_hash", digest(token))
        .set("auth_time", now())
        .set("amr", amr)
        .set("ip", "198.51.100.9")
        .set("user_agent", "Safari")
        .set("last_seen_at", now())
        .set("expires_at", now() + days(7))
        .set("ends_at", now() + days(30))
        .returning(["id"])
}

/// A live session of an enabled user, with the time of its sign-in in whole
/// seconds since 1970 as a token carries it.
fn live_session(id: &str) -> Query {
    Query::table("sessions")
        .alias("s")
        .key([id])
        .join(Query::table("users").alias("u").key([col("s.user_id")]).filter(col("u.disabled_at").is_null()))
        .filter(col("s.expires_at").gt(now()).and(col("s.ends_at").gt(now())))
        .select("id", col("s.id"))
        .select("user_id", col("s.user_id"))
        .select("amr", col("s.amr"))
        .select("auth_time", call(Func::Floor, [call(Func::DatePart, [val("epoch"), col("s.auth_time")])]).cast(Type::Int))
}

// R-Q16, R-S3, R-W6, R-W7: updates that compute times with LEAST and return
// what they wrote, a time read as whole seconds since 1970, and a unique
// column that stays unique under update.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_slides_to_its_absolute_end_and_keeps_its_id_through_a_new_sign_in() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", None)).await?;
    db.insert(person("usr_bob", "bob", None)).await?;
    db.insert(new_session("ses_1", "usr_ann", "cookie-1")).await?;
    let made: Timestamp = db.get("sessions", ["ses_1"]).await?.expect("the session is there").get("created_at")?;

    // Far from its end, the session gets its seven idle days from the
    // moment it is seen.
    let touched = db.update(touch_session("ses_1", 7).returning(["last_seen_at", "expires_at", "ends_at"])).await?.one()?;
    let (seen, expires, ends): (Timestamp, Timestamp, Timestamp) =
        (touched.get("last_seen_at")?, touched.get("expires_at")?, touched.get("ends_at")?);
    assert!(seen >= made);
    assert_eq!(expires.micros() - seen.micros(), 7 * DAY);
    assert_eq!(ends.micros() - made.micros(), 30 * DAY);
    // Two days before its end, it gets no more than those two days.
    db.update(Update::table("sessions").key(["ses_1"]).set("ends_at", now() + days(2))).await?;
    let touched = db.update(touch_session("ses_1", 7).returning(["expires_at", "ends_at"])).await?.one()?;
    assert_eq!(touched.get::<Timestamp>("expires_at")?, touched.get::<Timestamp>("ends_at")?);
    assert_eq!(db.update(touch_session("ses_gone", 7)).await?.affected(), 0);

    // The old cookie stops working, the new one finds the same session with
    // a fresh sign-in and a full life.
    let before = db.now().await?;
    let kept = db.update(sign_in_again("ses_1", "cookie-2", vec!["pwd", "otp", "mfa"])).await?;
    let after = db.now().await?;
    assert_eq!(kept.column::<String>()?, ["ses_1"]);
    assert!(db.query_opt(load_session("cookie-1")).await?.is_none());
    assert_eq!(db.query_one(load_session("cookie-2")).await?.get::<String>("id")?, "ses_1");
    let stored = db.get("sessions", ["ses_1"]).await?.expect("the session is there");
    let signed_in: Timestamp = stored.get("auth_time")?;
    assert!(before <= signed_in && signed_in <= after);
    assert_eq!(stored.get::<Timestamp>("created_at")?, made);
    assert_eq!(stored.get::<Timestamp>("ends_at")?.micros() - signed_in.micros(), 30 * DAY);
    assert_eq!(stored.get::<Option<String>>("ip")?.as_deref(), Some("198.51.100.9"));
    // The session was ended in another tab meanwhile: nothing comes
    // back, and the caller makes a new one instead.
    assert!(db.update(sign_in_again("ses_gone", "cookie-9", vec!["pwd"])).await?.is_empty());

    // For a token: the factors, and the time of the sign-in in whole
    // seconds.
    let live = db.query_one(live_session("ses_1")).await?;
    assert_eq!(live.get::<Vec<String>>("amr")?, ["pwd", "otp", "mfa"]);
    assert_eq!(live.get::<i64>("auth_time")?, signed_in.micros().div_euclid(1_000_000));
    assert!(db.query_opt(live_session("ses_gone")).await?.is_none());

    // A step-up changes the cookie and the factors, not the time of the
    // sign-in.
    db.update(Update::table("sessions").key(["ses_1"]).set("token_hash", digest("cookie-3")).set("amr", vec!["pwd", "hwk", "mfa"])).await?;
    let stepped = db.query_one(load_session("cookie-3")).await?;
    assert_eq!(stepped.get::<Vec<String>>("amr")?, ["pwd", "hwk", "mfa"]);
    assert_eq!(db.get("sessions", ["ses_1"]).await?.expect("the session is there").get::<Timestamp>("auth_time")?, signed_in);

    // Two sessions never share a cookie.
    db.insert(new_session("ses_2", "usr_ann", "cookie-b")).await?;
    db.insert(new_session("ses_3", "usr_bob", "cookie-c")).await?;
    let err = db.update(Update::table("sessions").key(["ses_2"]).set("token_hash", digest("cookie-3")).set("amr", vec!["pwd"])).await.unwrap_err();
    assert!(err.is_unique_violation() && err.violates("sessions_token_hash_key"), "{err}");

    // A user's sessions, and whether a session is theirs.
    assert_eq!(sorted(&db, Query::table("sessions").index("sessions_user", KeyRange::eq(["usr_ann"])).columns(["id"])).await?, ["ses_1", "ses_2"]);
    let owns = |id: &str, user: &str| Query::table("sessions").key([id]).filter(col("user_id").eq(user)).select("one", 1);
    assert_eq!(db.query(owns("ses_2", "usr_ann")).await?.len(), 1);
    assert!(db.query(owns("ses_3", "usr_ann")).await?.is_empty());

    // A session of a disabled user is not live for a token.
    db.update(Update::table("users").key(["usr_ann"]).set("disabled_at", Timestamp::now()).set("updated_at", now())).await?;
    assert!(db.query_opt(live_session("ses_1")).await?.is_none());
    assert!(db.query_opt(live_session("ses_3")).await?.is_some());
    Ok(())
}

// R-Q2, R-Q11, R-T3: an index range read in another order, groups with MIN
// and MAX ordered by an aggregate, and DISTINCT.
#[tokio::test(flavor = "multi_thread")]
async fn the_device_list_shows_live_sessions_and_token_families_latest_first() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", None)).await?;
    db.insert(person("usr_bob", "bob", None)).await?;
    let base = db.now().await?;
    let ago = |hours: i64| base.plus_secs(-hours * 3600);

    for (id, user, seen_hours_ago) in
        [("ses_a", "usr_ann", 3), ("ses_b", "usr_ann", 1), ("ses_c", "usr_ann", 2), ("ses_d", "usr_ann", 4), ("ses_e", "usr_bob", 0)]
    {
        db.insert(new_session(id, user, id)).await?;
        db.update(Update::table("sessions").key([id]).set("last_seen_at", ago(seen_hours_ago))).await?;
    }
    db.update(Update::table("sessions").key(["ses_d"]).set("expires_at", now() - seconds(1))).await?;
    db.update(Update::table("sessions").key(["ses_c"]).set("ip", null()).set("user_agent", null())).await?;

    // Her live sessions, the one seen last first.
    let devices = db
        .query(
            Query::table("sessions")
                .index("sessions_user", KeyRange::eq(["usr_ann"]))
                .filter(col("expires_at").gt(now()).and(col("ends_at").gt(now())))
                .order_by(col("last_seen_at").desc())
                .columns(["id", "created_at", "last_seen_at", "ip", "user_agent", "amr"]),
        )
        .await?;
    assert_eq!(column::<String>(&devices, "id"), ["ses_b", "ses_c", "ses_a"]);
    assert_eq!(column::<Timestamp>(&devices, "last_seen_at"), [ago(1), ago(2), ago(3)]);
    assert_eq!(column::<Option<String>>(&devices, "ip"), [Some("203.0.113.7".to_string()), None, Some("203.0.113.7".to_string())]);
    assert_eq!(column::<Vec<String>>(&devices, "amr")[0], ["pwd"]);

    // Apps on public clients have no session: each is a family of
    // refresh tokens, a new token per rotation.
    let tokens = [
        ("ios-1", "ses_ios", "files-ios", 240),
        ("ios-2", "ses_ios", "files-ios", 120),
        ("ios-3", "ses_ios", "files-ios", 1),
        ("android-1", "ses_android", "files-android", 48),
        ("android-2", "ses_android", "files-android", 3),
        ("revoked-1", "ses_revoked", "files-ios", 5),
        ("expired-1", "ses_expired", "files-ios", 6),
    ];
    for (token, family, client, made_hours_ago) in tokens {
        db.insert(public_token(token, family, "usr_ann", client).set("created_at", ago(made_hours_ago))).await?;
    }
    // Left out: a revoked token even in a live family, a family that is
    // all revoked or all expired, a token tied to a session, and anybody
    // else's.
    db.insert(public_token("ios-0", "ses_ios", "usr_ann", "files-ios").set("created_at", ago(480)).set("revoked_at", ago(240))).await?;
    db.update(Update::table("refresh_tokens").key([digest("revoked-1")]).set("revoked_at", now())).await?;
    db.update(Update::table("refresh_tokens").key([digest("expired-1")]).set("expires_at", now() - seconds(1))).await?;
    db.insert(session_token("web-1", "files-web", "ses_a")).await?;
    db.insert(public_token("bob-1", "ses_bob", "usr_bob", "files-ios")).await?;

    // One row per family and client, with the family's first and latest
    // token, the latest used family first.
    let families = db
        .query(
            Query::table("refresh_tokens")
                .index("refresh_tokens_user", KeyRange::eq(["usr_ann"]))
                .filter(all([col("session_id").is_null(), col("revoked_at").is_null(), col("expires_at").gt(now())]))
                .group_by([col("family_id"), col("client_id")])
                .select("id", col("family_id"))
                .select("client_id", col("client_id"))
                .select("created_at", min(col("created_at")))
                .select("last_seen_at", max(col("created_at")))
                .order_by(max(col("created_at")).desc()),
        )
        .await?;
    assert_eq!(families.columns(), ["id", "client_id", "created_at", "last_seen_at"]);
    assert_eq!(column::<String>(&families, "id"), ["ses_ios", "ses_android"]);
    assert_eq!(column::<String>(&families, "client_id"), ["files-ios", "files-android"]);
    assert_eq!(column::<Timestamp>(&families, "created_at"), [ago(240), ago(48)]);
    assert_eq!(column::<Timestamp>(&families, "last_seen_at"), [ago(1), ago(3)]);

    // The families that are not revoked, each once, whether or not they
    // have expired.
    let live = Query::table("refresh_tokens")
        .index("refresh_tokens_user", KeyRange::eq(["usr_ann"]))
        .filter(col("session_id").is_null().and(col("revoked_at").is_null()))
        .select("family_id", col("family_id"))
        .distinct();
    assert_eq!(sorted(&db, live).await?, ["ses_android", "ses_expired", "ses_ios"]);
    Ok(())
}

// R-Q2, R-S6, R-T9, R-W10, R-X1: a delete cascades into two tables, for one
// row and for many, and NULL equals nothing in a filter.
#[tokio::test(flavor = "multi_thread")]
async fn ending_a_session_takes_its_tokens_and_codes_with_it() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", None)).await?;
    db.insert(new_session("ses_1", "usr_ann", "cookie-1")).await?;
    db.insert(new_session("ses_2", "usr_ann", "cookie-2")).await?;
    db.insert(session_token("files-1", "files-web", "ses_1")).await?;
    db.insert(session_token("files-1b", "files-web", "ses_1")).await?;
    db.insert(session_token("chat-1", "chat-web", "ses_1")).await?;
    db.insert(session_token("files-2", "files-web", "ses_2")).await?;
    db.insert(new_code("code-1", "files-web", "ses_1", 60)).await?;
    db.insert(new_code("code-2", "files-web", "ses_2", 60)).await?;
    db.insert(public_token("ios-1", "ses_ios", "usr_ann", "files-ios")).await?;

    // Each client once. A public client's token has no session, and NULL is
    // not equal to anything: it is found by its family alone.
    assert_eq!(sorted(&db, clients_of("ses_1")).await?, ["chat-web", "files-web"]);
    assert_eq!(sorted(&db, clients_of("ses_2")).await?, ["files-web"]);
    assert_eq!(sorted(&db, clients_of("ses_ios")).await?, ["files-ios"]);
    assert!(sorted(&db, clients_of("ses_gone")).await?.is_empty());

    // Signing out: the clients are told, the session goes, the log says
    // so. The delete counts the session; its children go unseen.
    db.transaction(|tx| async move {
        for client in tx.query(clients_of("ses_1")).await?.column::<String>()? {
            tx.insert(event(&client, "session-ended")).await?;
        }
        let ended = tx.delete(Delete::from("sessions").key(["ses_1"])).await?;
        assert_eq!(ended.affected(), 1);
        tx.insert(audit_entry(Some("usr_ann"), "logout", Some("usr_ann"), json!({"session": "ses_1"}))).await?;
        Ok::<_, DbError>(())
    })
    .await?;
    for token in ["files-1", "files-1b", "chat-1"] {
        assert!(db.get("refresh_tokens", [digest(token)]).await?.is_none(), "{token}");
    }
    assert!(db.get("auth_codes", [digest("code-1")]).await?.is_none());
    assert!(db.get("refresh_tokens", [digest("files-2")]).await?.is_some());
    assert!(db.get("auth_codes", [digest("code-2")]).await?.is_some());
    assert!(db.get("refresh_tokens", [digest("ios-1")]).await?.is_some());
    assert_eq!((rows_of(&db, Query::table("outbox")).await?, rows_of(&db, Query::table("audit")).await?), (2, 1));
    assert_eq!(db.delete(Delete::from("sessions").key(["ses_1"])).await?.affected(), 0);

    // Housekeeping removes sessions past either expiry, and the same
    // children go with them.
    db.insert(new_session("ses_3", "usr_ann", "cookie-3")).await?;
    db.insert(session_token("files-3", "files-web", "ses_3")).await?;
    db.update(Update::table("sessions").key(["ses_2"]).set("expires_at", now() - seconds(1))).await?;
    db.update(Update::table("sessions").key(["ses_3"]).set("ends_at", now() - seconds(1))).await?;
    db.insert(new_session("ses_4", "usr_ann", "cookie-4")).await?;
    db.insert(session_token("files-4", "files-web", "ses_4")).await?;
    let swept = db.delete(Delete::from("sessions").filter(col("expires_at").lt(now()).or(col("ends_at").lt(now())))).await?;
    assert_eq!(swept.affected(), 2);
    assert_eq!(db.query(Query::table("sessions").columns(["id"])).await?.column::<String>()?, ["ses_4"]);
    assert_eq!(sorted(&db, Query::table("refresh_tokens").columns(["client_id"])).await?, ["files-ios", "files-web"]);
    assert_eq!(rows_of(&db, Query::table("auth_codes")).await?, 0);
    Ok(())
}

async fn refresh_token(db: &impl Db, token: &str) -> DbResult<Option<Row>> {
    db.get("refresh_tokens", [digest(token)]).await
}

// R-Q1, R-S5, R-W3, R-W7: an insert whose row is selected from another
// table, the defaults of columns left out, and an update that happens once.
#[tokio::test(flavor = "multi_thread")]
async fn a_refresh_token_copies_its_session_or_starts_a_family_of_its_own() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", None)).await?;
    db.insert(new_session("ses_1", "usr_ann", "cookie-1")).await?;
    db.update(Update::table("sessions").key(["ses_1"]).set("amr", vec!["pwd", "otp", "mfa"])).await?;
    let session = db.get("sessions", ["ses_1"]).await?.expect("the session is there");

    // The session's row supplies the family, the user, the expiry and the
    // factors; the statement supplies the rest.
    let before = db.now().await?;
    assert_eq!(db.insert(session_token("web-1", "files-web", "ses_1")).await?.affected(), 1);
    let after = db.now().await?;
    let token = refresh_token(&db, "web-1").await?.expect("the token is there");
    assert_eq!(token.get::<String>("family_id")?, "ses_1");
    assert_eq!(token.get::<Option<String>>("session_id")?.as_deref(), Some("ses_1"));
    assert_eq!((token.get::<String>("user_id")?, token.get::<String>("client_id")?), ("usr_ann".to_string(), "files-web".to_string()));
    assert_eq!(token.get::<Timestamp>("expires_at")?, session.get::<Timestamp>("ends_at")?);
    assert_eq!(token.get::<Vec<String>>("amr")?, ["pwd", "otp", "mfa"]);
    assert!(token.is_null("rotated_at")? && token.is_null("revoked_at")?);
    let made: Timestamp = token.get("created_at")?;
    assert!(before <= made && made <= after);

    // The session ended a moment before: no row to copy, so no token,
    // and no error either.
    assert_eq!(db.insert(session_token("web-2", "files-web", "ses_gone")).await?.affected(), 0);
    assert!(refresh_token(&db, "web-2").await?.is_none());
    let err = db.insert(session_token("web-1", "chat-web", "ses_1")).await.unwrap_err();
    assert!(err.is_unique_violation() && err.violates(PRIMARY), "{err}");

    // Thirty days from the moment it is made, tied to no session.
    db.insert(public_token("ios-1", "ses_ios", "usr_ann", "files-ios")).await?;
    let token = refresh_token(&db, "ios-1").await?.expect("the token is there");
    assert!(token.is_null("session_id")?);
    assert_eq!(token.get::<Timestamp>("expires_at")?.micros() - token.get::<Timestamp>("created_at")?.micros(), 30 * DAY);
    // Left out, the factors are an empty list, not NULL.
    let bare = Insert::into("refresh_tokens")
        .set("token_hash", digest("bare"))
        .set("family_id", "ses_bare")
        .set("user_id", "usr_ann")
        .set("client_id", "files-ios");
    db.insert(bare.set("expires_at", now() + days(30))).await?;
    assert_eq!(refresh_token(&db, "bare").await?.expect("the token is there").get::<Vec<String>>("amr")?, no_texts());

    // The family of a token, read by the token.
    let family = |token: &str| Query::table("refresh_tokens").key([digest(token)]).columns(["family_id"]);
    assert_eq!(db.query_one(family("ios-1")).await?.get::<String>("family_id")?, "ses_ios");
    assert!(db.query_opt(family("never-issued")).await?.is_none());

    // A client may revoke its own tokens only. Revoking is done once, and
    // keeps the time of the first.
    let revocable = |token: &str, client: &str| {
        Query::table("refresh_tokens")
            .key([digest(token)])
            .filter(col("client_id").eq(client).and(col("revoked_at").is_null()))
            .columns(["family_id", "user_id"])
    };
    let revoke = |token: &str| Update::table("refresh_tokens").key([digest(token)]).filter(col("revoked_at").is_null()).set("revoked_at", now());
    let own = db.query_one(revocable("web-1", "files-web")).await?;
    assert_eq!((own.get::<String>("family_id")?, own.get::<String>("user_id")?), ("ses_1".to_string(), "usr_ann".to_string()));
    assert!(db.query_opt(revocable("web-1", "chat-web")).await?.is_none());
    assert_eq!(db.update(revoke("web-1")).await?.affected(), 1);
    let revoked: Timestamp = refresh_token(&db, "web-1").await?.expect("the token is there").get("revoked_at")?;
    assert_eq!(db.update(revoke("web-1")).await?.affected(), 0);
    assert_eq!(refresh_token(&db, "web-1").await?.expect("the token is there").get::<Timestamp>("revoked_at")?, revoked);
    assert!(db.query_opt(revocable("web-1", "files-web")).await?.is_none());
    Ok(())
}

/// Everything the refresh grant needs to know about a token, with the
/// questions of time answered by the server's clock. A public client's
/// token has no session: the join then gives NULLs, and the two session
/// columns are NULL rather than false.
fn refresh_read(token: &str, grace_seconds: f64) -> Query {
    Query::table("refresh_tokens")
        .alias("r")
        .key([digest(token)])
        .left_join(Query::table("sessions").alias("s").key([col("r.session_id")]))
        .select("family_id", col("r.family_id"))
        .select("session_id", col("r.session_id"))
        .select("user_id", col("r.user_id"))
        .select("client_id", col("r.client_id"))
        .select("revoked", col("r.revoked_at").is_not_null())
        .select("live", col("r.expires_at").gt(now()))
        .select("rotated", col("r.rotated_at").is_not_null())
        .select("in_grace", col("r.rotated_at").gt(now() - seconds_f64(grace_seconds)))
        .select("amr", col("r.amr"))
        .select("session_amr", col("s.amr"))
        .select("session_live", col("s.expires_at").gt(now()).and(col("s.ends_at").gt(now())))
}

/// The flags `refresh_read` answers with: revoked, live, rotated, rotated
/// within the grace period, session live. The last two can be NULL.
type RefreshFlags = (bool, bool, bool, Option<bool>, Option<bool>);

fn refresh_flags(row: &Row) -> DbResult<RefreshFlags> {
    Ok((row.get("revoked")?, row.get("live")?, row.get("rotated")?, row.get("in_grace")?, row.get("session_live")?))
}

// R-Q1, R-Q8, R-Q9, R-T9: a left join whose missing side reads as NULL, and
// comparisons with the server's clock that answer true, false or NULL.
#[tokio::test(flavor = "multi_thread")]
async fn the_refresh_read_joins_the_session_if_any_and_answers_on_server_time() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", None)).await?;
    db.insert(new_session("ses_1", "usr_ann", "cookie-1")).await?;
    db.update(Update::table("sessions").key(["ses_1"]).set("amr", vec!["pwd", "otp"])).await?;
    db.insert(session_token("web-1", "files-web", "ses_1")).await?;
    db.insert(public_token("ios-1", "ses_ios", "usr_ann", "files-ios")).await?;

    let web = db.query_one(refresh_read("web-1", 10.0)).await?;
    assert_eq!(
        web.columns(),
        ["family_id", "session_id", "user_id", "client_id", "revoked", "live", "rotated", "in_grace", "amr", "session_amr", "session_live"]
    );
    assert_eq!(refresh_flags(&web)?, (false, true, false, None, Some(true)));
    assert_eq!((web.get::<String>("family_id")?, web.get::<Option<String>>("session_id")?), ("ses_1".to_string(), Some("ses_1".to_string())));
    assert_eq!((web.get::<String>("user_id")?, web.get::<String>("client_id")?), ("usr_ann".to_string(), "files-web".to_string()));
    assert_eq!(web.get::<Option<Vec<String>>>("session_amr")?, Some(vec!["pwd".to_string(), "otp".to_string()]));

    // No session to join: the row still comes, with NULL where the
    // session's columns would be.
    let ios = db.query_one(refresh_read("ios-1", 10.0)).await?;
    assert_eq!(refresh_flags(&ios)?, (false, true, false, None, None));
    assert_eq!((ios.get::<String>("family_id")?, ios.get::<Option<String>>("session_id")?), ("ses_ios".to_string(), None));
    assert_eq!(ios.get::<Vec<String>>("amr")?, ["pwd"]);
    assert_eq!(ios.get::<Option<Vec<String>>>("session_amr")?, None);

    // Rotated four seconds ago: within a grace of ten seconds, outside
    // one of two and a half.
    db.update(Update::table("refresh_tokens").key([digest("ios-1")]).set("rotated_at", now() - seconds(4))).await?;
    assert_eq!(refresh_flags(&db.query_one(refresh_read("ios-1", 10.0)).await?)?, (false, true, true, Some(true), None));
    assert_eq!(refresh_flags(&db.query_one(refresh_read("ios-1", 2.5)).await?)?, (false, true, true, Some(false), None));

    db.update(Update::table("refresh_tokens").key([digest("ios-1")]).set("revoked_at", now())).await?;
    assert_eq!(refresh_flags(&db.query_one(refresh_read("ios-1", 10.0)).await?)?, (true, true, true, Some(true), None));
    db.update(Update::table("refresh_tokens").key([digest("ios-1")]).set("expires_at", now() - seconds(1))).await?;
    assert_eq!(refresh_flags(&db.query_one(refresh_read("ios-1", 10.0)).await?)?, (true, false, true, Some(true), None));

    // The token is good but its session sat idle too long.
    db.update(Update::table("sessions").key(["ses_1"]).set("expires_at", now() - seconds(1))).await?;
    assert_eq!(refresh_flags(&db.query_one(refresh_read("web-1", 10.0)).await?)?, (false, true, false, None, Some(false)));

    assert!(db.query_opt(refresh_read("never-issued", 10.0)).await?.is_none());
    Ok(())
}

/// Ends a public client's family of tokens. What it returns feeds the
/// events, and says whether there was anything to end.
fn end_family(family: &str, user: &str) -> Update {
    Update::table("refresh_tokens")
        .index("refresh_tokens_family", KeyRange::eq([family]))
        .filter(all([col("user_id").eq(user), col("session_id").is_null(), col("revoked_at").is_null()]))
        .set("revoked_at", now())
        .returning(["client_id"])
}

/// How a refresh request ended.
#[derive(Debug, PartialEq)]
enum Refreshed {
    /// The token was good: it is marked rotated and a new one stands in
    /// its family.
    Rotated,
    /// The token had been rotated, but only just: the retry of a request
    /// whose answer was lost. It gets a new token too.
    Retried,
    /// A rotated token came back after the grace period, so somebody
    /// copied it: the whole family is revoked.
    Reused,
    /// No such token, another client's, revoked, expired, or its user may
    /// no longer use the service.
    Refused,
}

/// The refresh grant for a public client. On Postgres it would take a
/// shared lock on the client's row, read the family to take an advisory
/// lock on it, and lock the token's row while reading it. A writing
/// transaction is alone with the database from its first statement to its
/// commit, so the same statements run here without any of the three.
async fn refresh(db: &Client, client: &str, service: &str, presented: &str, issued: &str) -> DbResult<Refreshed> {
    let tx = db.begin().await?;
    let registered = tx.query_opt(Query::table("clients").key([client]).filter(col("service").eq(service)).select("one", 1)).await?.is_some();
    let Some(token) = tx.query_opt(refresh_read(presented, 10.0)).await? else {
        tx.commit().await?;
        return Ok(Refreshed::Refused);
    };
    let (family, user): (String, String) = (token.get("family_id")?, token.get("user_id")?);
    let (revoked, live, rotated, in_grace, _) = refresh_flags(&token)?;
    if !registered || token.get::<String>("client_id")? != client || revoked || !live {
        tx.commit().await?;
        return Ok(Refreshed::Refused);
    }

    if rotated && in_grace != Some(true) {
        // The family is revoked, its clients are told, and the log says so.
        let live_tokens =
            Update::table("refresh_tokens").index("refresh_tokens_family", KeyRange::eq([family.as_str()])).filter(col("revoked_at").is_null());
        let revoked = tx.update(live_tokens.set("revoked_at", now())).await?.affected();
        for holder in tx.query(clients_of(&family)).await?.column::<String>()? {
            tx.insert(event(service, &format!("family-revoked:{holder}"))).await?;
        }
        tx.insert(audit_entry(None, "token.reuse", Some(&user), json!({"family": family, "revoked": revoked}))).await?;
        tx.commit().await?;
        return Ok(Refreshed::Reused);
    }

    // Still a member of the service, still enabled.
    let member = tx.query_opt(role_of(&user, service)).await?.is_some();
    let enabled = Query::table("users").key([user.as_str()]).filter(col("disabled_at").is_null()).columns(["id", "username", "name", "email"]);
    let holder = tx.query_opt(enabled.select("email_verified", col("email_verified_at").is_not_null())).await?;
    if !member || holder.is_none() {
        tx.commit().await?;
        return Ok(Refreshed::Refused);
    }

    // The time of the first rotation is kept. Then the family's next token.
    tx.update(Update::table("refresh_tokens").key([digest(presented)]).set("rotated_at", coalesce([col("rotated_at"), now()]))).await?;
    tx.insert(public_token(issued, &family, &user, client)).await?;
    tx.commit().await?;
    Ok(if rotated { Refreshed::Retried } else { Refreshed::Rotated })
}

// R-Q9, R-W6, R-W7, R-X1, R-X2: several reads, a decision and the writes
// that follow from it are one writing transaction, where SQL would take two
// row locks and an advisory lock.
#[tokio::test(flavor = "multi_thread")]
async fn a_refresh_token_is_rotated_and_its_reuse_revokes_the_whole_family() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(new_service("files", "invite")).await?;
    db.insert(new_service("chat", "invite")).await?;
    db.insert(new_client("files-ios", "files", "public")).await?;
    db.insert(new_client("chat-ios", "chat", "public")).await?;
    db.insert(person("usr_ann", "ann", None)).await?;
    db.insert(grant_role("usr_ann", "files", "member", "usr_admin")).await?;
    db.insert(public_token("t1", "ses_phone", "usr_ann", "files-ios")).await?;
    db.insert(public_token("tablet-1", "ses_tablet", "usr_ann", "files-ios")).await?;

    // The old token stays, marked; the new one joins its family. Both
    // writes carry the transaction's one time.
    assert_eq!(refresh(&db, "files-ios", "files", "t1", "t2").await?, Refreshed::Rotated);
    let rotated_at: Timestamp = refresh_token(&db, "t1").await?.expect("the old token is kept").get("rotated_at")?;
    let next = refresh_token(&db, "t2").await?.expect("the new token is there");
    assert_eq!((next.get::<String>("family_id")?, next.get::<Option<String>>("session_id")?), ("ses_phone".to_string(), None));
    assert_eq!((next.get::<String>("user_id")?, next.get::<String>("client_id")?), ("usr_ann".to_string(), "files-ios".to_string()));
    assert!(next.is_null("rotated_at")? && next.is_null("revoked_at")?);
    assert_eq!(next.get::<Timestamp>("created_at")?, rotated_at);

    // Another client's token, a client that is not the service's, a
    // token never issued: refused, and nothing is written.
    assert_eq!(refresh(&db, "chat-ios", "chat", "t2", "x1").await?, Refreshed::Refused);
    assert_eq!(refresh(&db, "files-ios", "chat", "t2", "x2").await?, Refreshed::Refused);
    assert_eq!(refresh(&db, "files-ios", "files", "never-issued", "x3").await?, Refreshed::Refused);
    assert!(refresh_token(&db, "t2").await?.expect("the token is there").is_null("rotated_at")?);
    assert_eq!(rows_of(&db, Query::table("refresh_tokens")).await?, 3);

    // The client did not get its answer and asks again with the old token a
    // moment later: it gets another token, and the time of the first
    // rotation stands.
    assert_eq!(refresh(&db, "files-ios", "files", "t1", "t2-again").await?, Refreshed::Retried);
    assert_eq!(refresh_token(&db, "t1").await?.expect("the old token is kept").get::<Timestamp>("rotated_at")?, rotated_at);
    assert!(refresh_token(&db, "t2-again").await?.is_some());
    assert_eq!(refresh(&db, "files-ios", "files", "t2", "t3").await?, Refreshed::Rotated);

    // Long after its rotation the first token comes back. Every token of
    // the family is revoked at one moment, none is issued, the service
    // is told and the log says so.
    db.update(Update::table("refresh_tokens").key([digest("t1")]).set("rotated_at", now() - seconds(30))).await?;
    assert_eq!(refresh(&db, "files-ios", "files", "t1", "t4").await?, Refreshed::Reused);
    assert!(refresh_token(&db, "t4").await?.is_none());
    let family = db.query(Query::table("refresh_tokens").index("refresh_tokens_family", KeyRange::eq(["ses_phone"])).columns(["revoked_at"])).await?;
    let revoked_at = column::<Option<Timestamp>>(&family, "revoked_at");
    assert_eq!(revoked_at.len(), 4);
    assert!(revoked_at[0].is_some() && revoked_at.iter().all(|at| *at == revoked_at[0]));
    assert!(refresh_token(&db, "tablet-1").await?.expect("the other device's token is there").is_null("revoked_at")?);
    assert_eq!(
        db.query(Query::table("outbox").columns(["service", "token"])).await?.map::<(String, String)>()?,
        [("files".to_string(), "family-revoked:files-ios".to_string())]
    );
    let logged = db.query_one(Query::table("audit")).await?;
    assert_eq!((logged.get::<String>("action")?, logged.get::<Option<String>>("user_id")?), ("token.reuse".to_string(), Some("usr_ann".to_string())));
    assert_eq!(logged.get::<Json>("detail")?, json!({"family": "ses_phone", "revoked": 4}));
    assert_eq!(logged.get::<Timestamp>("at")?, revoked_at[0].expect("the family is revoked"));

    // The newest token of the family is as dead as the oldest.
    assert_eq!(refresh(&db, "files-ios", "files", "t3", "t5").await?, Refreshed::Refused);
    assert!(refresh_token(&db, "t5").await?.is_none());

    // Her other device is refused while she has no role, without its
    // token being marked, and carries on once she has one again.
    db.delete(Delete::from("access").key(["usr_ann", "files"])).await?;
    assert_eq!(refresh(&db, "files-ios", "files", "tablet-1", "tablet-2").await?, Refreshed::Refused);
    assert!(refresh_token(&db, "tablet-1").await?.expect("the token is there").is_null("rotated_at")?);
    db.insert(grant_role("usr_ann", "files", "member", "usr_admin")).await?;
    assert_eq!(refresh(&db, "files-ios", "files", "tablet-1", "tablet-2").await?, Refreshed::Rotated);
    Ok(())
}

// R-X2, R-X3, R-X8: writing transactions that race run one after another,
// which also settles a race over a row that is not there yet to be locked.
#[tokio::test(flavor = "multi_thread")]
async fn racing_refreshes_rotate_a_token_once_and_a_racing_sign_out_leaves_no_live_token() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(new_service("files", "invite")).await?;
    db.insert(new_client("files-ios", "files", "public")).await?;
    db.insert(person("usr_ann", "ann", None)).await?;
    db.insert(grant_role("usr_ann", "files", "member", "usr_admin")).await?;
    db.insert(public_token("t1", "ses_phone", "usr_ann", "files-ios")).await?;

    // Five requests carry the same token at once. One of them finds it
    // fresh; each of the others runs after a commit and finds it rotated
    // a moment ago.
    let mut requests = Vec::new();
    for n in 0..5 {
        let db = db.clone();
        requests.push(tokio::spawn(async move { refresh(&db, "files-ios", "files", "t1", &format!("next-{n}")).await }));
    }
    let mut outcomes = Vec::new();
    for request in requests {
        outcomes.push(request.await.expect("the task ran")?);
    }
    assert_eq!(outcomes.iter().filter(|outcome| **outcome == Refreshed::Rotated).count(), 1, "{outcomes:?}");
    assert_eq!(outcomes.iter().filter(|outcome| **outcome == Refreshed::Retried).count(), 4, "{outcomes:?}");
    assert_eq!(rows_of(&db, Query::table("refresh_tokens").index("refresh_tokens_family", KeyRange::eq(["ses_phone"]))).await?, 6);

    // A sign-out races a rotation. Row locks could not make this safe,
    // because the token a rotation is about to insert is not there to be
    // locked: Postgres would need a lock on the family's name. Here one of
    // the two simply runs first, and either way no token of the family is
    // live afterwards.
    for round in 0..10 {
        let family = format!("ses_race_{round}");
        db.insert(public_token(&format!("race-{round}"), &family, "usr_ann", "files-ios")).await?;
        let rotation = {
            let db = db.clone();
            tokio::spawn(async move { refresh(&db, "files-ios", "files", &format!("race-{round}"), &format!("race-{round}-next")).await })
        };
        let sign_out = {
            let (db, family) = (db.clone(), family.clone());
            tokio::spawn(async move { db.update(end_family(&family, "usr_ann")).await.map(|rows| rows.affected()) })
        };
        let outcome = rotation.await.expect("the task ran")?;
        let ended = sign_out.await.expect("the task ran")?;
        assert!(matches!((&outcome, ended), (Refreshed::Rotated, 2) | (Refreshed::Refused, 1)), "{outcome:?}, {ended} ended");
        let live = Query::table("refresh_tokens").index("refresh_tokens_family", KeyRange::eq([family.as_str()])).filter(col("revoked_at").is_null());
        assert_eq!(rows_of(&db, live).await?, 0);
    }
    Ok(())
}

/// "End every session of a user", inside the caller's transaction: each
/// session's clients are told and the session deleted, then each family of
/// a public client is ended. Answers with how many sessions and how many
/// tokens it ended.
async fn end_sessions(tx: &Transaction, user: &str) -> DbResult<(usize, u64)> {
    let sessions: Vec<String> = tx.query(Query::table("sessions").index("sessions_user", KeyRange::eq([user])).columns(["id"])).await?.column()?;
    for id in &sessions {
        for client in tx.query(clients_of(id)).await?.column::<String>()? {
            tx.insert(event(&client, "session-ended")).await?;
        }
        tx.delete(Delete::from("sessions").key([id])).await?;
    }
    let families = Query::table("refresh_tokens")
        .index("refresh_tokens_user", KeyRange::eq([user]))
        .filter(col("session_id").is_null().and(col("revoked_at").is_null()))
        .select("family_id", col("family_id"))
        .distinct();
    let mut tokens = 0;
    for family in tx.query(families).await?.column::<String>()? {
        let ended = tx.update(end_family(&family, user)).await?;
        tokens += ended.affected();
        for client in ended.column::<String>()? {
            tx.insert(event(&client, "family-ended")).await?;
        }
    }
    Ok((sessions.len(), tokens))
}

// R-W6, R-X1: deletes, updates and inserts in several tables commit
// together, and every row they wrote carries the transaction's one time.
#[tokio::test(flavor = "multi_thread")]
async fn a_password_change_ends_every_session_and_token_family_of_the_user() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", None)).await?;
    db.insert(person("usr_bob", "bob", None)).await?;
    db.insert(new_session("ses_1", "usr_ann", "cookie-1")).await?;
    db.insert(new_session("ses_2", "usr_ann", "cookie-2")).await?;
    db.insert(session_token("files-1", "files-web", "ses_1")).await?;
    db.insert(session_token("chat-1", "chat-web", "ses_1")).await?;
    db.insert(session_token("files-2", "files-web", "ses_2")).await?;
    db.insert(public_token("ios-1", "ses_ios", "usr_ann", "files-ios")).await?;
    db.insert(public_token("ios-2", "ses_ios", "usr_ann", "files-ios")).await?;
    db.insert(public_token("tv-1", "ses_tv", "usr_ann", "files-tv")).await?;
    let long_ago = db.now().await?.plus_secs(-3600);
    db.insert(public_token("old-1", "ses_old", "usr_ann", "files-ios").set("revoked_at", long_ago)).await?;
    db.insert(new_session("ses_b", "usr_bob", "cookie-b")).await?;
    db.insert(session_token("files-b", "files-web", "ses_b")).await?;
    db.insert(public_token("ios-b", "ses_ios_b", "usr_bob", "files-ios")).await?;

    let ended = db
        .transaction(|tx| async move {
            tx.update(Update::table("users").key(["usr_ann"]).set("password_hash", "$argon2id$changed").set("updated_at", now())).await?;
            let ended = end_sessions(&tx, "usr_ann").await?;
            tx.insert(audit_entry(Some("usr_ann"), "password.change", Some("usr_ann"), json!({"sessions": ended.0}))).await?;
            Ok::<_, DbError>(ended)
        })
        .await?;
    assert_eq!(ended, (2, 3));

    // Her sessions are gone with the tokens tied to them. The tokens of
    // her apps are kept, revoked, all at the transaction's one time; the
    // one revoked long ago keeps its own.
    assert_eq!(rows_of(&db, Query::table("sessions").index("sessions_user", KeyRange::eq(["usr_ann"]))).await?, 0);
    let hers = db
        .query(Query::table("refresh_tokens").index("refresh_tokens_user", KeyRange::eq(["usr_ann"])).columns([
            "family_id",
            "session_id",
            "revoked_at",
        ]))
        .await?;
    assert_eq!(hers.len(), 4);
    let changed: Timestamp = db.get("users", ["usr_ann"]).await?.expect("ann is there").get("updated_at")?;
    for token in &hers {
        assert!(token.is_null("session_id")?);
        let expected = if token.get::<String>("family_id")? == "ses_old" { long_ago } else { changed };
        assert_eq!(token.get::<Timestamp>("revoked_at")?, expected);
    }
    // Three events for the clients of her two sessions, one for each
    // token ended in a family.
    assert_eq!(
        sorted(&db, Query::table("outbox").columns(["token"])).await?,
        ["family-ended", "family-ended", "family-ended", "session-ended", "session-ended", "session-ended"]
    );
    assert_eq!(db.query_one(Query::table("audit")).await?.get::<Json>("detail")?, json!({"sessions": 2}));

    // Nobody else is signed out.
    assert!(db.query_opt(load_session("cookie-b")).await?.is_some());
    assert!(refresh_token(&db, "files-b").await?.is_some());
    assert!(refresh_token(&db, "ios-b").await?.expect("bob's token is there").is_null("revoked_at")?);

    // A second pass finds nothing left to end.
    assert_eq!(db.transaction(|tx| async move { end_sessions(&tx, "usr_ann").await }).await?, (0, 0));
    assert_eq!(rows_of(&db, Query::table("outbox")).await?, 6);
    Ok(())
}

// R-Q1, R-Q8, R-W6, R-W7, R-X1: a conditional update returns the row and a
// computed column to the first caller, and nothing to the next.
#[tokio::test(flavor = "multi_thread")]
async fn an_auth_code_is_redeemed_once_and_says_whether_it_was_still_live() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", None)).await?;
    db.insert(new_session("ses_1", "usr_ann", "cookie-1")).await?;

    // Sixty seconds to live, by the server's clock.
    let before = db.now().await?;
    db.insert(new_code("code-1", "files-web", "ses_1", 60)).await?;
    let after = db.now().await?;
    let expires: Timestamp = db.get("auth_codes", [digest("code-1")]).await?.expect("the code is there").get("expires_at")?;
    assert!(before.plus_secs(60) <= expires && expires <= after.plus_secs(60));

    // The first redemption gets the row and marks it; the second gets
    // nothing and changes nothing.
    let first = db.update(redeem("code-1")).await?;
    assert_eq!(first.affected(), 1);
    let first = first.one()?;
    assert_eq!(first.columns(), ["client_id", "session_id", "redirect_uri", "code_challenge", "nonce", "live"]);
    assert_eq!((first.get::<String>("client_id")?, first.get::<String>("session_id")?), ("files-web".to_string(), "ses_1".to_string()));
    assert_eq!(first.get::<String>("redirect_uri")?, "https://files.example/callback");
    assert_eq!(first.get::<String>("code_challenge")?, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    assert_eq!(first.get::<Option<String>>("nonce")?.as_deref(), Some("n-0S6_WzA2Mj"));
    assert!(first.get::<bool>("live")?);
    let used: Timestamp = db.get("auth_codes", [digest("code-1")]).await?.expect("the code is there").get("used_at")?;
    let second = db.update(redeem("code-1")).await?;
    assert!(second.is_empty());
    assert_eq!(second.affected(), 0);
    assert_eq!(db.get("auth_codes", [digest("code-1")]).await?.expect("the code is there").get::<Timestamp>("used_at")?, used);
    assert!(db.update(redeem("never-issued")).await?.is_empty());

    // A code past its minute is redeemed all the same, with the answer
    // that it was no longer live. The grant is refused but its
    // transaction commits, so the code stays spent.
    db.insert(new_code("code-2", "files-web", "ses_1", -1)).await?;
    let live = db.transaction(|tx| async move { tx.update(redeem("code-2")).await?.one()?.get::<bool>("live") }).await?;
    assert!(!live);
    assert!(!db.get("auth_codes", [digest("code-2")]).await?.expect("the code is there").is_null("used_at")?);
    assert!(db.update(redeem("code-2")).await?.is_empty());

    // A redemption whose transaction is given up did not happen.
    db.insert(new_code("code-3", "files-web", "ses_1", 60)).await?;
    let tx = db.begin().await?;
    assert_eq!(tx.update(redeem("code-3")).await?.len(), 1);
    assert!(tx.update(redeem("code-3")).await?.is_empty());
    tx.rollback().await?;
    assert_eq!(db.update(redeem("code-3")).await?.len(), 1);

    // A code belongs to a session that exists.
    let err = db.insert(new_code("code-4", "files-web", "ses_gone", 60)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::ForeignKeyViolation);
    Ok(())
}

// R-W7, R-X2, R-X3: of five concurrent conditional updates of one row, in
// transactions or alone, exactly one finds its condition true.
#[tokio::test(flavor = "multi_thread")]
async fn five_racing_redemptions_of_one_auth_code_have_one_winner() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(new_service("files", "invite")).await?;
    db.insert(new_client("files-web", "files", "confidential")).await?;
    db.insert(person("usr_ann", "ann", None)).await?;
    db.insert(new_session("ses_1", "usr_ann", "cookie-1")).await?;
    db.insert(new_code("code-1", "files-web", "ses_1", 60)).await?;
    db.insert(new_code("code-2", "files-web", "ses_1", 60)).await?;

    // As the token endpoint runs it: each request a transaction that
    // checks the client, redeems the code and issues a refresh token.
    let mut requests = Vec::new();
    for n in 0..5 {
        let db = db.clone();
        requests.push(tokio::spawn(async move {
            db.transaction(|tx| async move {
                tx.query_one(Query::table("clients").key(["files-web"]).filter(col("service").eq("files")).select("one", 1)).await?;
                if tx.update(redeem("code-1")).await?.is_empty() {
                    return Ok(false);
                }
                tx.insert(session_token(&format!("issued-{n}"), "files-web", "ses_1")).await?;
                Ok::<_, DbError>(true)
            })
            .await
        }));
    }
    let mut granted = 0;
    for request in requests {
        if request.await.expect("the task ran")? {
            granted += 1;
        }
    }
    assert_eq!(granted, 1);
    assert_eq!(rows_of(&db, Query::table("refresh_tokens")).await?, 1);

    // The same race between lone statements: the condition is tested and
    // the row written as one step, so only one of them finds the code
    // unused.
    let mut requests = Vec::new();
    for _ in 0..5 {
        let db = db.clone();
        requests.push(tokio::spawn(async move { db.update(redeem("code-2")).await.map(|rows| rows.len()) }));
    }
    let mut redeemed = 0;
    for request in requests {
        redeemed += request.await.expect("the task ran")?;
    }
    assert_eq!(redeemed, 1);
    Ok(())
}

// R-Q2, R-Q11, R-S3, R-W1, R-W7: an insert returns the row as stored, with
// the server's defaults; counts and lists through an index; a unique column
// over bytes.
#[tokio::test(flavor = "multi_thread")]
async fn a_credential_is_stored_with_the_servers_defaults_listed_and_revoked_once() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", None)).await?;
    db.insert(person("usr_bob", "bob", None)).await?;

    // The insert answers with the row as stored: the time it was made is
    // the server's, the expiry is the text it was given, read.
    let before = db.now().await?;
    let made = db.insert(new_credential("key_1", "usr_ann", "api_key", "files", "secret-1", Some("2031-05-06T07:08:09.25+02:00"))).await?;
    let after = db.now().await?;
    assert_eq!(made.affected(), 1);
    let made = made.one()?;
    assert_eq!(made.columns(), ["id", "kind", "service", "scopes", "name", "created_at", "last_used_at", "expires_at"]);
    assert_eq!(
        (made.get::<String>("id")?, made.get::<String>("kind")?, made.get::<String>("service")?),
        ("key_1".to_string(), "api_key".to_string(), "files".to_string())
    );
    assert_eq!(
        (made.get::<Vec<String>>("scopes")?, made.get::<String>("name")?),
        (vec!["read".to_string(), "write".to_string()], "laptop".to_string())
    );
    let created: Timestamp = made.get("created_at")?;
    assert!(before <= created && created <= after);
    assert_eq!(made.get::<Option<Timestamp>>("last_used_at")?, None);
    assert_eq!(made.get::<Option<Timestamp>>("expires_at")?, Some(ts("2031-05-06T05:08:09.25Z")));
    let unending = db.insert(new_credential("apw_1", "usr_ann", "app_password", "files", "secret-2", None)).await?.one()?;
    assert!(unending.is_null("expires_at")?);
    db.insert(new_credential("key_2", "usr_ann", "api_key", "chat", "secret-3", None)).await?;
    db.insert(new_credential("key_3", "usr_bob", "api_key", "files", "secret-4", None)).await?;

    // No two credentials share a secret, whoever they belong to.
    let err = db.insert(new_credential("key_9", "usr_bob", "api_key", "files", "secret-1", None)).await.unwrap_err();
    assert!(err.is_unique_violation() && err.violates("credentials_secret_hash_key"), "{err}");

    // How many of a kind a user has, against the limit, and which.
    let live = |user: &str, kind: &str| {
        Query::table("credentials").index("credentials_user", KeyRange::eq([user])).filter(col("kind").eq(kind).and(col("revoked_at").is_null()))
    };
    assert_eq!(rows_of(&db, live("usr_ann", "api_key")).await?, 2);
    assert_eq!(rows_of(&db, live("usr_ann", "app_password")).await?, 1);
    assert_eq!(rows_of(&db, live("usr_bob", "app_password")).await?, 0);
    let listed = db
        .query(live("usr_ann", "api_key").order_by(col("id")).columns([
            "id",
            "kind",
            "service",
            "scopes",
            "name",
            "created_at",
            "last_used_at",
            "expires_at",
        ]))
        .await?;
    assert_eq!(column::<String>(&listed, "id"), ["key_1", "key_2"]);
    assert_eq!(listed.first(), Some(&made));

    // Revoked by its owner, as the kind it is, once. The answer is the
    // service to tell.
    let revoke = |id: &str, user: &str, kind: &str| {
        Update::table("credentials")
            .key([id])
            .filter(all([col("user_id").eq(user), col("kind").eq(kind), col("revoked_at").is_null()]))
            .set("revoked_at", now())
            .returning(["service"])
    };
    assert!(db.update(revoke("key_1", "usr_bob", "api_key")).await?.is_empty());
    assert!(db.update(revoke("key_1", "usr_ann", "app_password")).await?.is_empty());
    assert_eq!(db.update(revoke("key_1", "usr_ann", "api_key")).await?.column::<String>()?, ["files"]);
    assert!(db.update(revoke("key_1", "usr_ann", "api_key")).await?.is_empty());
    assert_eq!(rows_of(&db, live("usr_ann", "api_key")).await?, 1);

    // A revoked credential is kept, and its secret stays taken.
    assert!(!db.get("credentials", ["key_1"]).await?.expect("the row is kept").is_null("revoked_at")?);
    assert!(db
        .insert(new_credential("key_9", "usr_ann", "api_key", "files", "secret-1", None))
        .await
        .unwrap_err()
        .violates("credentials_secret_hash_key"));

    // A use is timed by the server.
    db.update(Update::table("credentials").key(["key_2"]).set("last_used_at", now())).await?;
    let used: Timestamp = db.get("credentials", ["key_2"]).await?.expect("the row is there").get("last_used_at")?;
    assert!(used >= created && used <= db.now().await?);
    Ok(())
}

/// What a service learns about a secret a caller showed it: the credential,
/// its user and the user's role on the service, if all three are in order.
/// Secrets have been hashed in two ways over time and either is looked for:
/// two lookups in the unique index, then one row of `users` and one of
/// `access` by their keys.
fn introspect(secret: &str, service: &str) -> Query {
    let candidates = [digest(secret), digest(&format!("v0:{secret}"))].map(|hash| KeyRange::eq([hash]));
    Query::table("credentials")
        .alias("c")
        .index_ranges("credentials_secret_hash_key", candidates)
        .join(Query::table("users").alias("u").key([col("c.user_id")]).filter(col("u.disabled_at").is_null()))
        .join(Query::table("access").alias("a").key([col("c.user_id"), col("c.service")]))
        .filter(all([col("c.service").eq(service), col("c.revoked_at").is_null(), col("c.expires_at").is_null().or(col("c.expires_at").gt(now()))]))
        .select("id", col("c.id"))
        .select("user_id", col("c.user_id"))
        .select("scopes", col("c.scopes"))
        .select("username", col("u.username"))
        .select("role", col("a.role"))
        .select("stale", col("c.last_used_at").is_null().or(col("c.last_used_at").lt(now() - minutes(1))))
}

// R-Q1, R-Q6, R-Q8, R-Q9, R-T9: two lookups in a unique index, each joined
// to two more tables by key, with conditions on times that may be NULL.
#[tokio::test(flavor = "multi_thread")]
async fn introspection_reads_a_credential_its_user_and_their_role_in_one_query() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", None)).await?;
    db.insert(person("usr_bob", "bob", None)).await?;
    db.insert(grant_role("usr_ann", "files", "member", "usr_admin")).await?;
    db.insert(grant_role("usr_bob", "files", "admin", "usr_admin")).await?;
    db.insert(new_credential("key_1", "usr_ann", "api_key", "files", "s1", None)).await?;

    let found = db.query_one(introspect("s1", "files")).await?;
    assert_eq!(found.columns(), ["id", "user_id", "scopes", "username", "role", "stale"]);
    assert_eq!(
        (found.get::<String>("id")?, found.get::<String>("user_id")?, found.get::<String>("username")?),
        ("key_1".to_string(), "usr_ann".to_string(), "ann".to_string())
    );
    assert_eq!(
        (found.get::<Vec<String>>("scopes")?, found.get::<String>("role")?),
        (vec!["read".to_string(), "write".to_string()], "member".to_string())
    );

    // Never used, or last used over a minute ago: time to note the use.
    // Used within the minute: not again.
    let stale = |row: Row| row.get::<bool>("stale");
    assert!(stale(found)?);
    db.update(Update::table("credentials").key(["key_1"]).set("last_used_at", now())).await?;
    assert!(!stale(db.query_one(introspect("s1", "files")).await?)?);
    db.update(Update::table("credentials").key(["key_1"]).set("last_used_at", now() - seconds(61))).await?;
    assert!(stale(db.query_one(introspect("s1", "files")).await?)?);

    // A secret stored under the older hashing is found by the second
    // lookup, and each row carries its own user's role.
    db.insert(new_credential("key_2", "usr_bob", "api_key", "files", "v0:s2", None)).await?;
    let older = db.query_one(introspect("s2", "files")).await?;
    assert_eq!((older.get::<String>("id")?, older.get::<String>("role")?), ("key_2".to_string(), "admin".to_string()));

    // An expiry still to come does not hide a credential; one that has
    // passed does.
    db.insert(new_credential("key_3", "usr_ann", "api_key", "files", "s3", Some("2099-01-01T00:00:00Z"))).await?;
    assert!(db.query_opt(introspect("s3", "files")).await?.is_some());
    db.update(Update::table("credentials").key(["key_3"]).set("expires_at", now() - seconds(1))).await?;
    assert!(db.query_opt(introspect("s3", "files")).await?.is_none());

    // Asked about by another service; or its user has no role on its
    // own service, until she is given one.
    assert!(db.query_opt(introspect("s1", "chat")).await?.is_none());
    db.insert(new_credential("key_4", "usr_ann", "api_key", "chat", "s4", None)).await?;
    assert!(db.query_opt(introspect("s4", "chat")).await?.is_none());
    db.insert(grant_role("usr_ann", "chat", "owner", "usr_admin")).await?;
    assert_eq!(db.query_one(introspect("s4", "chat")).await?.get::<String>("role")?, "owner");

    // Revoked; unknown; its user disabled.
    db.update(Update::table("credentials").key(["key_4"]).set("revoked_at", now())).await?;
    assert!(db.query_opt(introspect("s4", "chat")).await?.is_none());
    assert!(db.query_opt(introspect("never-issued", "files")).await?.is_none());
    db.update(Update::table("users").key(["usr_bob"]).set("disabled_at", Timestamp::now()).set("updated_at", now())).await?;
    assert!(db.query_opt(introspect("s2", "files")).await?.is_none());
    assert!(db.query_opt(introspect("s1", "files")).await?.is_some());
    Ok(())
}

/// Whether a text is a time, and whether that time is still to come. SQL
/// would ask the server without naming a table; a query here reads one, so
/// this reads the table that always has exactly one row.
fn expiry_check(text: &str) -> Query {
    let parsed = || call(Func::ParseTimestamp, [val(text)]);
    Query::table("services_revision").select("ok", parsed().is_not_null()).select("ahead", parsed().gt(now()))
}

/// An API key carried over from another system with the times it had there,
/// each as text. A key without a time of creation gets the server's.
fn imported_key(
    id: &str,
    user: &str,
    secret: &str,
    created: Option<&str>,
    last_used: Option<&str>,
    expires: Option<&str>,
    revoked: Option<&str>,
) -> Insert {
    Insert::into("credentials")
        .set("id", id)
        .set("user_id", user)
        .set("kind", "api_key")
        .set("service", "files")
        .set("scopes", vec!["read"])
        .set("name", "imported")
        .set("secret_hash", digest(secret))
        .set("created_at", coalesce([val(created).cast(Type::Timestamp), now()]))
        .set("last_used_at", last_used)
        .set("expires_at", expires)
        .set("revoked_at", revoked)
}

// R-Q8, R-Q22, R-T3, R-T4, R-W11: the server reads a timestamp given as
// text, in a column and in an expression, and refuses text that is not one.
#[tokio::test(flavor = "multi_thread")]
async fn a_timestamp_given_as_text_is_read_or_refused_by_the_server() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", None)).await?;

    // The shapes the service lets through to the server, and the moment
    // each means: a zone as Z or an offset, none at all (UTC), fractions of
    // a second, the end of a day written as its 24th hour, a leap day.
    let times = [
        ("2031-05-06T07:08:09Z", "2031-05-06T07:08:09Z"),
        ("2031-05-06T07:08:09.123456+02:00", "2031-05-06T05:08:09.123456Z"),
        ("2031-05-06T07:08:09-0430", "2031-05-06T11:38:09Z"),
        ("2031-05-06t07:08z", "2031-05-06T07:08:00Z"),
        ("2031-05-06T07:08:09", "2031-05-06T07:08:09Z"),
        ("2031-05-06T24:00:00Z", "2031-05-07T00:00:00Z"),
        ("2032-02-29T12:00:00Z", "2032-02-29T12:00:00Z"),
    ];
    for (n, (text, moment)) in times.into_iter().enumerate() {
        let check = db.query_one(expiry_check(text)).await?;
        assert_eq!((check.get::<bool>("ok")?, check.get::<Option<bool>>("ahead")?), (true, Some(true)), "{text}");
        // As SQL would write it: the text converted, then compared.
        let ahead = db.query_one(Query::table("services_revision").select("ahead", val(text).cast(Type::Timestamp).gt(now()))).await?;
        assert!(ahead.get::<bool>("ahead")?, "{text}");
        // Written to the column, the text is the same moment.
        let stored =
            db.insert(new_credential(&format!("key_{n}"), "usr_ann", "api_key", "files", &format!("secret-{n}"), Some(text))).await?.one()?;
        assert_eq!(stored.get::<Option<Timestamp>>("expires_at")?, Some(ts(moment)), "{text}");
    }
    let past = db.query_one(expiry_check("2001-02-03T04:05:06Z")).await?;
    assert_eq!((past.get::<bool>("ok")?, past.get::<Option<bool>>("ahead")?), (true, Some(false)));

    // Days and hours that do not exist, and text that is no time at all:
    // the check says so, and the column refuses them.
    for text in
        ["2031-02-30T00:00:00Z", "2031-02-29T00:00:00Z", "2031-13-01T00:00:00Z", "2031-05-06T25:00:00Z", "2031-05-06T07:61:00Z", "next tuesday", ""]
    {
        let check = db.query_one(expiry_check(text)).await?;
        assert_eq!((check.get::<bool>("ok")?, check.get::<Option<bool>>("ahead")?), (false, None), "{text:?}");
        let err = db.insert(new_credential("key_bad", "usr_ann", "api_key", "files", "secret-bad", Some(text))).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::TypeMismatch, "{text:?}");
    }
    assert!(db.get("credentials", ["key_bad"]).await?.is_none());
    // A conversion fails loudly, unless asked to give NULL instead.
    let impossible = || val("2031-02-30T00:00:00Z");
    let err = db.query(Query::table("services_revision").select("at", impossible().cast(Type::Timestamp))).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::TypeMismatch);
    assert!(db.query_one(Query::table("services_revision").select("at", impossible().try_cast(Type::Timestamp))).await?.is_null("at")?);

    // An imported key keeps the times it came with.
    db.insert(imported_key(
        "key_old",
        "usr_ann",
        "old",
        Some("2019-12-31T23:59:59.5-01:00"),
        Some("2024-06-01T00:00:00Z"),
        None,
        Some("2025-01-01T00:00:00+00:00"),
    ))
    .await?;
    let old = db.get("credentials", ["key_old"]).await?.expect("the key is there");
    assert_eq!(old.get::<Timestamp>("created_at")?, ts("2020-01-01T00:59:59.5Z"));
    assert_eq!(old.get::<Option<Timestamp>>("last_used_at")?, Some(ts("2024-06-01T00:00:00Z")));
    assert_eq!(old.get::<Option<Timestamp>>("expires_at")?, None);
    assert_eq!(old.get::<Option<Timestamp>>("revoked_at")?, Some(ts("2025-01-01T00:00:00Z")));
    let before = db.now().await?;
    db.insert(imported_key("key_new", "usr_ann", "new", None, None, None, None)).await?;
    let created: Timestamp = db.get("credentials", ["key_new"]).await?.expect("the key is there").get("created_at")?;
    assert!(before <= created && created <= db.now().await?);
    Ok(())
}

/// The user import: for each person an account, a line in the log, a role
/// and an activation token, all in one transaction, because the import's
/// contract is to write all or nothing.
async fn import_users(db: &Client, people: &[(String, String, String)]) -> DbResult<()> {
    db.transaction(|tx| async move {
        for (id, username, email) in people {
            tx.insert(user_row(id, username, Some(email), "person", None, None)).await?;
            tx.insert(audit_entry(Some("host-cli"), "user.import", Some(id), json!({"username": username}))).await?;
            tx.insert(grant_role(id, "files", "member", "host-cli")).await?;
            tx.insert(one_time_token(&format!("activate-{id}"), "activate", id, Some(email), 14 * 86_400)).await?;
        }
        Ok(())
    })
    .await
}

// R-S5, R-W2, R-W11, R-X1, R-X2: a transaction of twelve hundred inserts is
// all or nothing, and the numbers the server assigns follow the order of
// writing.
#[tokio::test(flavor = "multi_thread")]
async fn an_import_of_three_hundred_users_that_fails_midway_writes_nothing() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_admin", "admin", Some("admin@example.com"))).await?;
    db.insert(audit_entry(Some("usr_admin"), "login.ok", Some("usr_admin"), json!({}))).await?;
    let mut people: Vec<(String, String, String)> =
        (0..300).map(|n| (format!("usr_{n:03}"), format!("user-{n:03}"), format!("user-{n:03}@example.com"))).collect();
    async fn counts(db: &Client) -> DbResult<Vec<i64>> {
        let mut counts = Vec::new();
        for table in ["users", "audit", "access", "one_time_tokens"] {
            counts.push(rows_of(db, Query::table(table)).await?);
        }
        Ok(counts)
    }

    // The 201st person has the address of the 18th, in capitals. Eight
    // hundred rows were written by then; none of them stays.
    people[200].2 = "USER-017@EXAMPLE.COM".to_string();
    let err = import_users(&db, &people).await.unwrap_err();
    assert!(err.is_unique_violation() && err.violates("users_email"), "{err}");
    assert_eq!(counts(&db).await?, [1, 1, 0, 0]);

    // Until it commits, an import shows nothing to anyone else.
    let tx = db.begin().await?;
    for (id, username, email) in &people[..50] {
        tx.insert(user_row(id, username, Some(email), "person", None, None)).await?;
    }
    assert_eq!(rows_of(&tx, Query::table("users")).await?, 51);
    assert_eq!(rows_of(&db, Query::table("users")).await?, 1);
    tx.rollback().await?;

    // Corrected, the same import writes every row.
    people[200].2 = "user-200@example.com".to_string();
    import_users(&db, &people).await?;
    assert_eq!(counts(&db).await?, [301, 301, 300, 300]);
    // The log's lines were numbered by the server in the order they were
    // written: read by number, they name the people in the import's own
    // order.
    let logged = db.query(Query::table("audit").filter(col("action").eq("user.import")).columns(["user_id"])).await?;
    assert_eq!(logged.column::<Option<String>>()?, people.iter().map(|(id, _, _)| Some(id.clone())).collect::<Vec<_>>());

    // Run again, it fails on its first row and changes nothing.
    let err = import_users(&db, &people).await.unwrap_err();
    assert!(err.is_unique_violation() && err.violates(PRIMARY), "{err}");
    assert_eq!(counts(&db).await?, [301, 301, 300, 300]);
    Ok(())
}

/// A secret for a client, live from the moment it is stored.
fn new_secret(client: &str, secret: &str) -> Insert {
    Insert::into("client_secrets").set("client_id", client).set("secret_hash", digest(secret))
}

/// Whether this is one of the client's live secrets.
fn client_secret(client: &str, secret: &str) -> Query {
    Query::table("client_secrets").key([val(client), val(digest(secret))]).filter(col("revoked_at").is_null()).select("one", 1)
}

/// Revokes every live secret of a client but the newest. SQL would do it in
/// one statement, comparing each row against a subquery for the newest.
/// Here the newest is read and the others are revoked by two statements of
/// the caller's transaction: nobody can add a secret between them and have
/// it revoked for not being the one that was read.
async fn retire_old_secrets(tx: &Transaction, client: &str) -> DbResult<u64> {
    let live = || Query::table("client_secrets").range(KeyRange::eq([client])).filter(col("revoked_at").is_null());
    let Some(newest) = tx.query_opt(live().order_by(col("created_at").desc()).limit(1).columns(["secret_hash"])).await? else {
        return Ok(0);
    };
    let newest: Vec<u8> = newest.get("secret_hash")?;
    let retired = tx.update(Update::rows(live().filter(col("secret_hash").ne(newest))).set("revoked_at", now()).returning_expr("one", 1)).await?;
    Ok(retired.affected())
}

/// Giving a client of the files service a new secret: the client is checked
/// to be the service's, and its old secrets but the newest are retired, so
/// that the one in use keeps working while the new one is rolled out.
async fn add_client_secret(db: &Client, client: &str, secret: &str) -> DbResult<()> {
    db.transaction(|tx| async move {
        tx.query_one(Query::table("clients").key([client]).filter(col("service").eq("files")).select("one", 1)).await?;
        let retired = retire_old_secrets(&tx, client).await?;
        tx.insert(new_secret(client, secret)).await?;
        tx.insert(audit_entry(Some("usr_admin"), "client.secret", None, json!({"client": client, "retired": retired}))).await?;
        Ok(())
    })
    .await
}

// R-Q1, R-Q4, R-Q10, R-W7, R-X2: "all but the newest" as a read and an
// update in one transaction, where SQL would compare against a subquery,
// also when five do it at once.
#[tokio::test(flavor = "multi_thread")]
async fn retiring_a_clients_old_secrets_revokes_all_but_the_newest() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(new_service("files", "invite")).await?;
    db.insert(new_client("files-web", "files", "confidential")).await?;
    // Each secret is given an age of its own, so that "newest" does not
    // hang on how fine the clock is.
    for (client, secret, hours_old) in
        [("files-web", "s1", 3), ("files-web", "s2", 2), ("files-web", "s3", 1), ("chat-web", "d1", 5), ("chat-web", "d2", 4)]
    {
        db.insert(new_secret(client, secret)).await?;
        db.update(Update::table("client_secrets").key([val(client), val(digest(secret))]).set("created_at", now() - hours(hours_old))).await?;
    }
    async fn works(db: &Client, client: &str, secret: &str) -> DbResult<bool> {
        Ok(db.query(client_secret(client, secret)).await?.len() == 1)
    }

    // A client proves itself with any of its live secrets, and only with
    // its own.
    assert!(works(&db, "files-web", "s1").await? && works(&db, "files-web", "s2").await? && works(&db, "files-web", "s3").await?);
    assert!(!works(&db, "chat-web", "s1").await? && !works(&db, "files-web", "never-issued").await?);

    // All but the newest are retired, and only that client's.
    assert_eq!(db.transaction(|tx| async move { retire_old_secrets(&tx, "files-web").await }).await?, 2);
    assert!(works(&db, "files-web", "s3").await?);
    assert!(!works(&db, "files-web", "s1").await? && !works(&db, "files-web", "s2").await?);
    assert!(works(&db, "chat-web", "d1").await? && works(&db, "chat-web", "d2").await?);
    assert_eq!(db.transaction(|tx| async move { retire_old_secrets(&tx, "files-web").await }).await?, 0);
    assert_eq!(db.transaction(|tx| async move { retire_old_secrets(&tx, "wiki-web").await }).await?, 0);
    // Revoked secrets are kept.
    assert_eq!(rows_of(&db, Query::table("client_secrets").range(KeyRange::eq(["files-web"]))).await?, 3);

    // A new secret: the one in use and the new one both work.
    add_client_secret(&db, "files-web", "s4").await?;
    assert!(works(&db, "files-web", "s3").await? && works(&db, "files-web", "s4").await?);

    // Five new secrets asked for at once. Each request retires all but
    // the newest it finds and adds its own, one request after another,
    // so two secrets are live in the end: never more, as there would be
    // if two requests had both found the same secret to be the newest.
    let mut requests = Vec::new();
    for n in 0..5 {
        let db = db.clone();
        requests.push(tokio::spawn(async move { add_client_secret(&db, "files-web", &format!("rolled-{n}")).await }));
    }
    for request in requests {
        request.await.expect("the task ran")?;
    }
    let live = Query::table("client_secrets").filter(col("revoked_at").is_null());
    assert_eq!(rows_of(&db, live.clone().range(KeyRange::eq(["files-web"]))).await?, 2);
    assert!(!works(&db, "files-web", "s3").await? && !works(&db, "files-web", "s4").await?);

    // Every live secret, the newest first, for the admin page.
    let listed = db.query(live.order_by(col("created_at").desc()).columns(["client_id", "created_at"])).await?;
    assert_eq!(column::<String>(&listed, "client_id"), ["files-web", "files-web", "chat-web", "chat-web"]);
    assert!(column::<Timestamp>(&listed, "created_at").windows(2).all(|pair| pair[0] >= pair[1]));
    assert_eq!(rows_of(&db, Query::table("audit")).await?, 6);
    Ok(())
}

/// An invitation that can still be accepted, by its token.
fn open_invite(token: &str) -> Query {
    Query::table("invites")
        .index("invites_token_hash_key", KeyRange::eq([digest(token)]))
        .filter(col("accepted_at").is_null().and(col("expires_at").gt(now())))
}

/// Making an account from an invitation: the user, the log, the invitation
/// accepted, the role it promised and the service's event are one
/// transaction. An error anywhere, such as a taken username or an
/// invitation that is no longer open, undoes all of it.
async fn join_by_invite(db: &Client, token: &str, id: &str, username: &str) -> DbResult<()> {
    db.transaction(|tx| async move {
        tx.insert(person(id, username, None)).await?;
        tx.insert(audit_entry(Some(id), "user.create", Some(id), json!({}))).await?;
        let invite = tx.update(accept_invite(token, id)).await?.one()?;
        let (service, role): (String, String) = (invite.get("service")?, invite.get("role")?);
        tx.insert(grant_role(id, &service, &role, "usr_admin")).await?;
        tx.insert(event(&service, "access-granted")).await?;
        Ok(())
    })
    .await
}

// R-Q1, R-Q8, R-S3, R-W1, R-W7, R-X1: an update through a unique index
// happens once and only before a deadline, and a transaction is undone by
// an error in a later statement.
#[tokio::test(flavor = "multi_thread")]
async fn an_invitation_is_accepted_once_while_it_is_good() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", None)).await?;

    // Fourteen days to accept, by the server's clock.
    let before = db.now().await?;
    let made = db.insert(new_invite("inv_1", "new@example.com", "files", "member", "token-1", 14 * 86_400)).await?.one()?;
    let after = db.now().await?;
    assert_eq!(made.columns(), INVITE_COLUMNS);
    assert_eq!(
        (made.get::<String>("id")?, made.get::<String>("email")?, made.get::<String>("invited_by")?),
        ("inv_1".to_string(), "new@example.com".to_string(), "usr_admin".to_string())
    );
    assert!(made.is_null("accepted_by")? && made.is_null("accepted_at")?);
    let expires: Timestamp = made.get("expires_at")?;
    assert!(before.plus_secs(14 * 86_400) <= expires && expires <= after.plus_secs(14 * 86_400));

    // Found by its token while it is open.
    assert_eq!(db.query_one(open_invite("token-1").columns(INVITE_COLUMNS)).await?, made);
    assert_eq!(db.query_one(open_invite("token-1").columns(["service"])).await?.get::<String>("service")?, "files");
    assert!(db.query_opt(open_invite("never-sent")).await?.is_none());

    // Accepted once, answering with the row as it now is.
    let accepted = db.update(accept_invite("token-1", "usr_ann")).await?;
    assert_eq!(accepted.affected(), 1);
    let accepted = accepted.one()?;
    assert_eq!((accepted.get::<String>("id")?, accepted.get::<Option<String>>("accepted_by")?), ("inv_1".to_string(), Some("usr_ann".to_string())));
    assert!(accepted.get::<Timestamp>("accepted_at")? >= after);
    assert!(db.update(accept_invite("token-1", "usr_ann")).await?.is_empty());
    assert!(db.query_opt(open_invite("token-1")).await?.is_none());

    // One that has expired is neither found nor accepted.
    db.insert(new_invite("inv_2", "late@example.com", "files", "member", "token-2", -1)).await?;
    assert!(db.query_opt(open_invite("token-2")).await?.is_none());
    assert_eq!(db.update(accept_invite("token-2", "usr_ann")).await?.affected(), 0);
    assert!(db.get("invites", ["inv_2"]).await?.expect("the invitation is there").is_null("accepted_at")?);

    // No two invitations share a token, and only a user who exists can
    // accept one.
    let err = db.insert(new_invite("inv_3", "x@example.com", "files", "member", "token-1", 3600)).await.unwrap_err();
    assert!(err.is_unique_violation() && err.violates("invites_token_hash_key"), "{err}");
    db.insert(new_invite("inv_3", "bob@example.com", "chat", "editor", "token-3", 3600)).await?;
    assert_eq!(db.update(accept_invite("token-3", "usr_nobody")).await.unwrap_err().code, ErrorCode::ForeignKeyViolation);

    // The invited person picks a username that is taken: no account, no
    // log line, and the invitation is still open.
    let err = join_by_invite(&db, "token-3", "usr_bob", "ann").await.unwrap_err();
    assert!(err.violates("users_username_key"), "{err}");
    assert!(db.get("users", ["usr_bob"]).await?.is_none());
    assert!(db.query_opt(open_invite("token-3")).await?.is_some());
    assert_eq!((rows_of(&db, Query::table("audit")).await?, rows_of(&db, Query::table("outbox")).await?), (0, 0));

    // With a free one, everything is there at once.
    join_by_invite(&db, "token-3", "usr_bob", "bob").await?;
    assert_eq!(
        db.get("invites", ["inv_3"]).await?.expect("the invitation is there").get::<Option<String>>("accepted_by")?.as_deref(),
        Some("usr_bob")
    );
    assert_eq!(db.query_one(role_of("usr_bob", "chat")).await?.get::<String>("role")?, "editor");
    assert_eq!((rows_of(&db, Query::table("audit")).await?, rows_of(&db, Query::table("outbox")).await?), (1, 1));

    // The same link opened by somebody else: the invitation is taken,
    // and the account made a statement earlier is undone.
    let err = join_by_invite(&db, "token-3", "usr_cy", "cy").await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    assert!(db.get("users", ["usr_cy"]).await?.is_none());
    assert_eq!(rows_of(&db, Query::table("audit")).await?, 1);
    Ok(())
}

/// A page of invitations after `cursor`, all of them or one service's. Each
/// case has an index in the order wanted: the primary key, or
/// `(service, id)`.
fn invite_page(service: &str, cursor: &str, page: u64) -> Query {
    let invites = if service.is_empty() {
        Query::table("invites").range(KeyRange::new(Bound::excluded([cursor]), Bound::Unbounded))
    } else {
        Query::table("invites").index("invites_service", KeyRange::within([service], Bound::excluded([cursor]), Bound::Unbounded))
    };
    invites.columns(INVITE_COLUMNS).limit(page)
}

// R-Q2, R-Q4, R-W7: paging by a cursor over the primary key or within one
// prefix of a two-column index, and a filtered delete that returns its row.
#[tokio::test(flavor = "multi_thread")]
async fn invitations_are_paged_by_id_for_all_services_or_for_one() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    for (id, service) in [
        ("inv_01", "files"),
        ("inv_02", "chat"),
        ("inv_03", "files"),
        ("inv_04", "files"),
        ("inv_05", "chat"),
        ("inv_06", "files"),
        ("inv_07", "wiki"),
    ] {
        db.insert(new_invite(id, "someone@example.com", service, "member", id, 3600)).await?;
    }
    let ids = |rows: Rows| column::<String>(&rows, "id");

    // For the admin: every service's.
    assert_eq!(ids(db.query(invite_page("", "", 3)).await?), ["inv_01", "inv_02", "inv_03"]);
    assert_eq!(ids(db.query(invite_page("", "inv_03", 3)).await?), ["inv_04", "inv_05", "inv_06"]);
    assert_eq!(ids(db.query(invite_page("", "inv_06", 3)).await?), ["inv_07"]);
    assert!(db.query(invite_page("", "inv_07", 3)).await?.is_empty());

    // For a service's client: its own, in the same order, and never a
    // neighbour's once its own run out.
    assert_eq!(ids(db.query(invite_page("files", "", 3)).await?), ["inv_01", "inv_03", "inv_04"]);
    assert_eq!(ids(db.query(invite_page("files", "inv_04", 3)).await?), ["inv_06"]);
    assert!(db.query(invite_page("files", "inv_06", 3)).await?.is_empty());
    assert_eq!(ids(db.query(invite_page("chat", "inv_02", 3)).await?), ["inv_05"]);
    assert!(db.query(invite_page("calendar", "", 3)).await?.is_empty());
    let page = db.query(invite_page("files", "", 3)).await?;
    assert_eq!(page.columns(), INVITE_COLUMNS);
    assert_eq!(column::<String>(&page, "service"), ["files", "files", "files"]);

    // One invitation, which a client sees only if it is its service's.
    let one = |id: &str, service: &str| {
        Query::table("invites").key([id]).filter(any([val(service).eq(""), col("service").eq(service)])).columns(INVITE_COLUMNS)
    };
    assert_eq!(db.query_one(one("inv_02", "")).await?.get::<String>("service")?, "chat");
    assert!(db.query_opt(one("inv_02", "chat")).await?.is_some());
    assert!(db.query_opt(one("inv_02", "files")).await?.is_none());
    assert!(db.query_opt(one("inv_99", "")).await?.is_none());

    // Withdrawn under the same rule, once.
    let withdraw = |id: &str, service: &str| {
        Delete::from("invites").key([id]).filter(any([val(service).eq(""), col("service").eq(service)])).returning(["service"])
    };
    assert!(db.delete(withdraw("inv_02", "files")).await?.is_empty());
    assert!(db.get("invites", ["inv_02"]).await?.is_some());
    assert_eq!(db.delete(withdraw("inv_02", "chat")).await?.column::<String>()?, ["chat"]);
    assert!(db.delete(withdraw("inv_02", "chat")).await?.is_empty());
    assert_eq!(db.delete(withdraw("inv_07", "")).await?.column::<String>()?, ["wiki"]);
    assert_eq!(ids(db.query(invite_page("", "", 10)).await?), ["inv_01", "inv_03", "inv_04", "inv_05", "inv_06"]);
    Ok(())
}

// R-Q4, R-Q6, R-Q8, R-W7, R-W10: a row that expires by the server's clock
// is used once by a conditional update, and an index range is deleted with
// a filter.
#[tokio::test(flavor = "multi_thread")]
async fn a_one_time_token_is_used_once_for_its_purpose_before_it_expires() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", Some("ann@example.com"))).await?;
    db.insert(person("usr_bob", "bob", None)).await?;
    // Looks at a token without using it.
    let peek = |token: &str, purpose: &str| {
        Query::table("one_time_tokens")
            .key([digest(token)])
            .filter(all([col("purpose").eq(purpose), col("used_at").is_null(), col("expires_at").gt(now())]))
            .columns(["user_id", "email"])
    };

    // An hour to use it, by the server's clock.
    let before = db.now().await?;
    db.insert(one_time_token("reset-1", "reset", "usr_ann", None, 3600)).await?;
    let expires: Timestamp = db.get("one_time_tokens", [digest("reset-1")]).await?.expect("the token is there").get("expires_at")?;
    assert!(before.plus_secs(3600) <= expires && expires <= db.now().await?.plus_secs(3600));

    // Looking does not use it, however often; a token is good for the
    // purpose it was made for and no other.
    for _ in 0..2 {
        let seen = db.query_one(peek("reset-1", "reset")).await?;
        assert_eq!((seen.get::<String>("user_id")?, seen.get::<Option<String>>("email")?), ("usr_ann".to_string(), None));
    }
    assert!(db.query_opt(peek("reset-1", "activate")).await?.is_none());
    assert!(db.update(consume_token("reset-1", "activate")).await?.is_empty());

    // Used, once.
    let used = db.update(consume_token("reset-1", "reset")).await?;
    assert_eq!(used.affected(), 1);
    assert_eq!(used.one()?.get::<String>("user_id")?, "usr_ann");
    assert!(db.update(consume_token("reset-1", "reset")).await?.is_empty());
    assert!(db.query_opt(peek("reset-1", "reset")).await?.is_none());

    // Expired a second ago: neither seen nor used.
    db.insert(one_time_token("reset-2", "reset", "usr_ann", None, -1)).await?;
    assert!(db.query_opt(peek("reset-2", "reset")).await?.is_none());
    assert!(db.update(consume_token("reset-2", "reset")).await?.is_empty());

    // The address she most recently asked to switch to, among the requests
    // still open. A used one, an expired one and a token of another purpose
    // do not count, however late they expire.
    db.insert(one_time_token("verify-1", "verify_email", "usr_ann", Some("first@example.com"), 3600)).await?;
    db.insert(one_time_token("verify-2", "verify_email", "usr_ann", Some("second@example.com"), 3 * 3600)).await?;
    db.insert(one_time_token("verify-3", "verify_email", "usr_ann", Some("third@example.com"), 2 * 3600)).await?;
    db.insert(one_time_token("verify-4", "verify_email", "usr_ann", Some("used@example.com"), 9 * 3600)).await?;
    db.update(consume_token("verify-4", "verify_email")).await?;
    db.insert(one_time_token("verify-5", "verify_email", "usr_ann", Some("expired@example.com"), -60)).await?;
    db.insert(one_time_token("activate-1", "activate", "usr_ann", Some("other@example.com"), 9 * 3600)).await?;
    db.insert(one_time_token("verify-b", "verify_email", "usr_bob", Some("bob@example.com"), 9 * 3600)).await?;
    let hers = |purposes: Vec<&str>| {
        Query::table("one_time_tokens").index("one_time_tokens_user", KeyRange::eq(["usr_ann"])).filter(col("purpose").in_list(purposes))
    };
    let pending = hers(vec!["verify_email"])
        .filter(col("used_at").is_null().and(col("expires_at").gt(now())))
        .order_by(col("expires_at").desc())
        .limit(1)
        .columns(["email"]);
    assert_eq!(db.query_one(pending.clone()).await?.get::<Option<String>>("email")?.as_deref(), Some("second@example.com"));

    // A confirmed address withdraws her other unused requests and reset
    // links. Used tokens and other purposes stay.
    let withdrawn = db.delete(Delete::rows(hers(vec!["verify_email", "reset"]).filter(col("used_at").is_null()))).await?;
    assert_eq!(withdrawn.affected(), 5);
    assert!(db.query_opt(pending).await?.is_none());
    assert_eq!(sorted(&db, hers(vec!["verify_email", "reset", "activate"]).columns(["purpose"])).await?, ["activate", "reset", "verify_email"]);

    // Her requests to change address, then every token of hers.
    assert_eq!(db.delete(Delete::rows(hers(vec!["verify_email"]))).await?.affected(), 1);
    assert_eq!(db.delete(Delete::from("one_time_tokens").index("one_time_tokens_user", KeyRange::eq(["usr_ann"]))).await?.affected(), 2);
    assert_eq!(sorted(&db, Query::table("one_time_tokens").columns(["user_id"])).await?, ["usr_bob"]);
    Ok(())
}

/// A signing key, published and signing at the given hours from the
/// server's now.
fn new_key(kid: &str, publish_in_hours: i64, sign_in_hours: i64) -> Insert {
    Insert::into("signing_keys")
        .set("kid", kid)
        .set("public_jwk", json!({"kty": "EC", "crv": "P-256", "x": "f83OJ3D2", "y": "x_FEzRu9", "kid": kid, "use": "sig", "alg": "ES256"}))
        .set("private_key_enc", format!("v1:{kid}").into_bytes())
        .set("publish_at", now() + hours(publish_in_hours))
        .set("sign_from", now() + hours(sign_in_hours))
}

/// The keys to publish, in the order they sign, each saying whether it is
/// the one that signs now: the one whose `sign_from` is the latest already
/// reached.
fn published_keys() -> Query {
    let signing_since = Query::table("signing_keys").alias("n").filter(col("n.sign_from").le(now())).select("latest", max(col("n.sign_from")));
    Query::table("signing_keys")
        .alias("k")
        .filter(col("k.publish_at").le(now()).and(col("k.retire_at").is_null().or(col("k.retire_at").gt(now()))))
        .order_by(col("k.sign_from"))
        .select("kid", col("k.kid"))
        .select("public_jwk", col("k.public_jwk"))
        .select("private_key_enc", col("k.private_key_enc"))
        .select("signs", col("k.sign_from").eq(scalar(signing_since)))
}

// R-Q8, R-Q10, R-Q11, R-T6, R-T9: a column compared with an aggregate from
// a subquery, and JSON that is stored and returned whole.
#[tokio::test(flavor = "multi_thread")]
async fn the_published_keys_say_which_one_signs_now() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    assert!(db.query(published_keys()).await?.is_empty());

    // A key that is published but does not sign yet, alone: no key has
    // started signing, so the question has no answer rather than "no".
    db.insert(new_key("Ck4-next", -1, 23)).await?;
    let alone = db.query_one(published_keys()).await?;
    assert_eq!((alone.get::<String>("kid")?, alone.get::<Option<bool>>("signs")?), ("Ck4-next".to_string(), None));

    // Key ids are thumbprints in mixed case: their order as keys is not
    // the order they sign in.
    db.insert(new_key("Zk1-retired", -100 * 24, -90 * 24)).await?;
    db.insert(new_key("aK2-previous", -31 * 24, -30 * 24)).await?;
    db.insert(new_key("Bk3-current", -48, -24)).await?;
    db.insert(new_key("dk5-unpublished", 1, 48)).await?;
    db.update(Update::table("signing_keys").key(["Zk1-retired"]).set("retire_at", now() - hours(1))).await?;
    db.update(Update::table("signing_keys").key(["aK2-previous"]).set("retire_at", now() + hours(5))).await?;

    let keys = db.query(published_keys()).await?;
    assert_eq!(keys.columns(), ["kid", "public_jwk", "private_key_enc", "signs"]);
    assert_eq!(column::<String>(&keys, "kid"), ["aK2-previous", "Bk3-current", "Ck4-next"]);
    assert_eq!(column::<bool>(&keys, "signs"), [false, true, false]);
    assert_eq!(column::<Vec<u8>>(&keys, "private_key_enc")[1], b"v1:Bk3-current");
    // The public key is served as it is stored: whole, its members in the
    // order Postgres' jsonb gives them (shorter names first, then by
    // bytes), not the order they were written in.
    let jwk: Json = keys.first().expect("a key is published").get("public_jwk")?;
    assert_eq!(jwk.to_string(), r#"{"x":"f83OJ3D2","y":"x_FEzRu9","alg":"ES256","crv":"P-256","kid":"aK2-previous","kty":"EC","use":"sig"}"#);

    // Re-encrypting the private keys reads every id in order, then each key
    // in a transaction of its own.
    let kids: Vec<String> = db.query(Query::table("signing_keys").select("id", col("kid"))).await?.column()?;
    assert_eq!(kids, ["Bk3-current", "Ck4-next", "Zk1-retired", "aK2-previous", "dk5-unpublished"]);
    for kid in &kids {
        db.transaction(|tx| async move {
            let sealed: Vec<u8> =
                tx.query_one(Query::table("signing_keys").key([kid]).select("sealed", col("private_key_enc"))).await?.get("sealed")?;
            let resealed = [b"v2:".as_slice(), &sealed[3..]].concat();
            tx.update(Update::table("signing_keys").key([kid]).set("private_key_enc", resealed)).await?;
            Ok::<_, DbError>(())
        })
        .await?;
    }
    let keys = db.query(published_keys()).await?;
    assert_eq!(column::<Vec<u8>>(&keys, "private_key_enc"), [b"v2:aK2-previous".to_vec(), b"v2:Bk3-current".to_vec(), b"v2:Ck4-next".to_vec()]);
    assert_eq!(keys.first().expect("a key is published").get::<Json>("public_jwk")?, jwk);
    Ok(())
}

/// Where the rotation stands: since when the current key signs, how many
/// keys wait to sign, and whether the current one has signed for 89 days.
/// SQL would select three subqueries from nothing; aggregates with a filter
/// over the one table say the same, and an aggregate without groups always
/// gives one row.
fn rotation_state() -> Query {
    let current_from = || max(col("sign_from")).filter(col("sign_from").le(now()));
    Query::table("signing_keys")
        .select("current_from", current_from())
        .select("pending", count().filter(col("sign_from").gt(now())))
        .select("due", current_from().le(now() - days(89)))
}

/// The hourly key rotation as one transaction, which on Postgres would
/// begin with an advisory lock so that two processes do not both add a key.
/// It reads the state, adds a key if one is due and none waits, dates the
/// retirement of every key that a later one has taken over from and deletes
/// keys a day after their retirement. In SQL the dating is one statement
/// whose subqueries read the table being updated; here the keys are read
/// once and each superseded one is updated by its key. Answers whether a
/// key was added.
async fn rotate_keys(db: &Client, kid: &str) -> DbResult<bool> {
    let tx = db.begin().await?;
    let state = tx.query_one(rotation_state()).await?;
    let (signing, pending, due): (Option<Timestamp>, i64, Option<bool>) = (state.get("current_from")?, state.get("pending")?, state.get("due")?);
    // The first key signs at once; a replacement is published now and
    // signs from tomorrow, when every copy has had time to fetch it.
    let added = if signing.is_none() && pending == 0 {
        tx.insert(new_key(kid, 0, 0)).await?;
        true
    } else if due == Some(true) && pending == 0 {
        tx.insert(new_key(kid, 0, 24)).await?;
        true
    } else {
        false
    };

    let in_order = Query::table("signing_keys")
        .order_by(col("sign_from"))
        .columns(["kid", "sign_from", "retire_at"])
        .select("signing", col("sign_from").le(now()));
    let keys = tx.query(in_order).await?.into_vec();
    for (n, key) in keys.iter().enumerate() {
        let later = &keys[n + 1..];
        let mut superseded = false;
        for next in later {
            superseded |= next.get::<bool>("signing")?;
        }
        if key.is_null("retire_at")? && superseded {
            let successor: Timestamp = later[0].get("sign_from")?;
            tx.update(Update::table("signing_keys").key([key.get::<String>("kid")?]).set("retire_at", val(successor) + days(1))).await?;
        }
    }

    tx.delete(Delete::from("signing_keys").filter(col("retire_at").lt(now() - days(1)))).await?;
    tx.commit().await?;
    Ok(added)
}

// R-Q8, R-Q10, R-Q11, R-W6, R-W10, R-X2: aggregates with a filter always
// give one row, and a periodic job that reads, decides and writes is safe
// to run in two processes at once.
#[tokio::test(flavor = "multi_thread")]
async fn key_rotation_adds_a_key_when_one_is_due_and_retires_the_keys_it_replaces() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    async fn state(db: &Client) -> DbResult<(Option<Timestamp>, i64, Option<bool>)> {
        let state = db.query_one(rotation_state()).await?;
        Ok((state.get("current_from")?, state.get("pending")?, state.get("due")?))
    }
    async fn key(db: &Client, kid: &str) -> DbResult<Row> {
        Ok(db.get("signing_keys", [kid]).await?.expect("the key is there"))
    }

    // No keys: one row all the same. The first rotation makes a key that
    // signs at once, and the next finds nothing to do.
    assert_eq!(state(&db).await?, (None, 0, None));
    assert!(rotate_keys(&db, "k1").await?);
    let k1 = key(&db, "k1").await?;
    assert_eq!(k1.get::<Timestamp>("publish_at")?, k1.get::<Timestamp>("sign_from")?);
    assert_eq!(state(&db).await?, (Some(k1.get("sign_from")?), 0, Some(false)));
    assert!(!rotate_keys(&db, "k2").await?);
    assert_eq!(rows_of(&db, Query::table("signing_keys")).await?, 1);

    // Eighty-nine days on, a successor is published, to sign from a day
    // later. Until it signs nothing is retired, and no third key is made.
    db.update(Update::table("signing_keys").key(["k1"]).set("publish_at", now() - days(90)).set("sign_from", now() - days(89) - seconds(1))).await?;
    let k1_from: Timestamp = key(&db, "k1").await?.get("sign_from")?;
    assert_eq!(state(&db).await?, (Some(k1_from), 0, Some(true)));
    assert!(rotate_keys(&db, "k2").await?);
    let k2 = key(&db, "k2").await?;
    assert_eq!(k2.get::<Timestamp>("sign_from")?.micros() - k2.get::<Timestamp>("publish_at")?.micros(), DAY);
    assert_eq!(state(&db).await?, (Some(k1_from), 1, Some(true)));
    assert!(key(&db, "k1").await?.is_null("retire_at")?);
    assert!(!rotate_keys(&db, "k3").await?);
    assert_eq!(rows_of(&db, Query::table("signing_keys")).await?, 2);

    // The successor has begun to sign: the key before it retires a day
    // after that moment, and until then both are published.
    db.update(Update::table("signing_keys").key(["k2"]).set("sign_from", now() - hours(1))).await?;
    let k2_from: Timestamp = key(&db, "k2").await?.get("sign_from")?;
    assert!(!rotate_keys(&db, "k3").await?);
    assert_eq!(key(&db, "k1").await?.get::<Option<Timestamp>>("retire_at")?, Some(Timestamp(k2_from.micros() + DAY)));
    assert!(key(&db, "k2").await?.is_null("retire_at")?);
    assert_eq!(state(&db).await?, (Some(k2_from), 0, Some(false)));
    let published = db.query(published_keys()).await?;
    assert_eq!(
        (column::<String>(&published, "kid"), column::<bool>(&published, "signs")),
        (vec!["k1".to_string(), "k2".to_string()], vec![false, true])
    );
    // A date of retirement, once set, is not moved by later passes.
    assert!(!rotate_keys(&db, "k3").await?);
    assert_eq!(key(&db, "k1").await?.get::<Option<Timestamp>>("retire_at")?, Some(Timestamp(k2_from.micros() + DAY)));

    // A day after its retirement the old key is deleted.
    db.update(Update::table("signing_keys").key(["k1"]).set("retire_at", now() - days(1) - seconds(1))).await?;
    assert!(!rotate_keys(&db, "k3").await?);
    assert_eq!(db.query(Query::table("signing_keys").columns(["kid"])).await?.column::<String>()?, ["k2"]);

    // Two processes rotate in the same hour when a key is due. One adds
    // it; the other waits for that transaction, then finds a key waiting
    // and adds none.
    db.update(Update::table("signing_keys").key(["k2"]).set("sign_from", now() - days(90))).await?;
    let mut passes = Vec::new();
    for kid in ["k3", "k4"] {
        let db = db.clone();
        passes.push(tokio::spawn(async move { rotate_keys(&db, kid).await }));
    }
    let mut added = 0;
    for pass in passes {
        added += usize::from(pass.await.expect("the task ran")?);
    }
    assert_eq!(added, 1);
    assert_eq!(rows_of(&db, Query::table("signing_keys")).await?, 2);
    Ok(())
}

/// Leases the oldest due events to the caller: counts the attempt and puts
/// the next one a lease away, so that nobody else takes them meanwhile and
/// a caller that dies leaves them to be retried. The index holds only
/// events neither delivered nor given up, by the time they are due.
/// Postgres needs `FOR UPDATE SKIP LOCKED` to keep two callers off the same
/// rows; here a statement is not interleaved with another.
fn lease_events(batch: u64, lease_seconds: i64) -> Update {
    let due = Query::table("outbox").index("outbox_pending", KeyRange::new(Bound::Unbounded, Bound::included([now()])));
    Update::rows(due.order_by(col("id")).limit(batch))
        .set("attempts", col("attempts") + 1)
        .set("next_attempt_at", now() + seconds(lease_seconds))
        .returning(["id", "service", "token", "attempts"])
}

// R-S4, R-S5, R-W6, R-W9, R-X2, R-X3: a queue on an index over part of a
// table, whose oldest due rows an ordered, limited update leases to one
// taker at a time.
#[tokio::test(flavor = "multi_thread")]
async fn the_outbox_leases_its_oldest_due_events_to_one_taker_at_a_time() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    for n in 1..=25 {
        db.insert(event(if n % 2 == 0 { "files" } else { "chat" }, &format!("event-{n}"))).await?;
    }
    // The insert leaves the number, the count of attempts and the time of
    // the first attempt to the server.
    let queued = db.query(Query::table("outbox")).await?;
    assert_eq!(column::<i64>(&queued, "id"), (1..=25).collect::<Vec<i64>>());
    assert!(column::<i64>(&queued, "attempts").iter().all(|attempts| *attempts == 0));
    assert!(column::<Timestamp>(&queued, "next_attempt_at").windows(2).all(|pair| pair[0] <= pair[1]));

    // Not to be leased: an event put off for an hour, one delivered, one
    // given up. And one due for longer than the rest, which does not make
    // it first: the order is that of the numbers.
    db.update(Update::table("outbox").key([7]).set("next_attempt_at", now() + hours(1))).await?;
    db.update(Update::table("outbox").key([9]).set("delivered_at", now())).await?;
    db.update(Update::table("outbox").key([11]).set("failed_at", now())).await?;
    db.update(Update::table("outbox").key([20]).set("next_attempt_at", now() - hours(1))).await?;
    let due: Vec<i64> = (1..=25).filter(|id| ![7, 9, 11].contains(id)).collect();

    let leased = db.update(lease_events(20, 60)).await?;
    assert_eq!(leased.affected(), 20);
    assert_eq!(leased.columns(), ["id", "service", "token", "attempts"]);
    let mut first: Vec<(i64, String, String, i64)> = leased.map()?;
    first.sort();
    assert_eq!(first.iter().map(|event| event.0).collect::<Vec<_>>(), due[..20]);
    for (id, service, token, attempts) in &first {
        assert_eq!((service.as_str(), token, *attempts), (if id % 2 == 0 { "files" } else { "chat" }, &format!("event-{id}"), 1));
    }
    // What is leased is not due again for a minute, so the next call
    // gets the rest, and the one after that nothing.
    let mut second: Vec<i64> = db.update(lease_events(20, 60)).await?.column()?;
    second.sort();
    assert_eq!(second, due[20..]);
    assert!(db.update(lease_events(20, 60)).await?.is_empty());
    assert_eq!(db.get("outbox", [7]).await?.expect("the event is there").get::<i64>("attempts")?, 0);

    // After the attempts: one delivered, one given up, one put off by a
    // back-off the caller computed, here 322.5 seconds. Two were leased by
    // a process that died, and their minute has passed.
    db.update(Update::table("outbox").key([1]).set("delivered_at", now())).await?;
    db.update(Update::table("outbox").key([2]).set("failed_at", now())).await?;
    db.update(Update::table("outbox").key([3]).set("next_attempt_at", now() + seconds_f64(322.5))).await?;
    let put_off: Timestamp = db.get("outbox", [3]).await?.expect("the event is there").get("next_attempt_at")?;
    let ahead = put_off.micros() - db.now().await?.micros();
    assert!((317_500_000..=322_500_000).contains(&ahead), "{ahead} microseconds");
    for id in [1, 2, 4, 5] {
        db.update(Update::table("outbox").key([id]).set("next_attempt_at", now() - seconds(1))).await?;
    }
    let mut retried: Vec<(i64, String, String, i64)> = db.update(lease_events(20, 60)).await?.map()?;
    retried.sort();
    assert_eq!(retried.iter().map(|event| (event.0, event.3)).collect::<Vec<_>>(), [(4, 2), (5, 2)]);

    // Sixty more events and six deliverers at once, each asking for
    // twenty: every event goes to exactly one of them.
    for n in 26..=85 {
        db.insert(event("files", &format!("event-{n}"))).await?;
    }
    let mut deliverers = Vec::new();
    for _ in 0..6 {
        let db = db.clone();
        deliverers.push(tokio::spawn(async move { db.update(lease_events(20, 60)).await?.column::<i64>() }));
    }
    let mut taken = Vec::new();
    for deliverer in deliverers {
        let ids = deliverer.await.expect("the task ran")?;
        assert!(ids.len() == 20 || ids.is_empty(), "a batch of {}", ids.len());
        taken.extend(ids);
    }
    taken.sort();
    assert_eq!(taken, (26..=85).collect::<Vec<i64>>());
    Ok(())
}

/// A grant as the management API makes it: the role, the line in the log
/// and the service's event, inside the caller's transaction.
async fn put_access(tx: &Transaction, user: &str, role: &str) -> DbResult<()> {
    tx.insert(grant_role(user, "files", role, "usr_admin")).await?;
    tx.insert(audit_entry(Some("usr_admin"), "access.put", Some(user), json!({"service": "files", "role": role}))).await?;
    tx.insert(event("files", &format!("access:{user}:{role}"))).await?;
    Ok(())
}

// R-S5, R-W11, R-X1, R-X2: rows written beside a change commit or vanish
// with it, and the numbers the server assigns rise in the order of the
// commits.
#[tokio::test(flavor = "multi_thread")]
async fn an_event_and_its_log_line_exist_only_if_the_change_they_report_commits() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", None)).await?;
    async fn written(db: &Client) -> DbResult<(i64, i64, String)> {
        let role = db.query_one(role_of("usr_ann", "files")).await?.get("role")?;
        Ok((rows_of(db, Query::table("audit")).await?, rows_of(db, Query::table("outbox")).await?, role))
    }

    db.transaction(|tx| async move { put_access(&tx, "usr_ann", "member").await }).await?;
    assert_eq!(written(&db).await?, (1, 1, "member".to_string()));

    // Before the commit nobody else sees the event, so no deliverer can
    // send word of a change that may yet be undone. Undone, it leaves
    // nothing.
    let tx = db.begin().await?;
    put_access(&tx, "usr_ann", "admin").await?;
    assert_eq!(rows_of(&tx, Query::table("outbox")).await?, 2);
    assert_eq!(written(&db).await?, (1, 1, "member".to_string()));
    tx.rollback().await?;
    assert_eq!(written(&db).await?, (1, 1, "member".to_string()));

    // A statement that fails after the event was queued takes the event
    // and the log line with it.
    let failed = db
        .transaction(|tx| async move {
            tx.insert(event("files", "access:usr_nobody:member")).await?;
            tx.insert(audit_entry(Some("usr_admin"), "access.put", Some("usr_nobody"), json!({}))).await?;
            tx.insert(grant_role("usr_nobody", "files", "member", "usr_admin")).await?;
            Ok::<_, DbError>(())
        })
        .await;
    assert_eq!(failed.unwrap_err().code, ErrorCode::ForeignKeyViolation);
    assert_eq!(written(&db).await?, (1, 1, "member".to_string()));

    // The next event gets a higher number than the one before it,
    // whatever numbers the undone ones were given: read by number, the
    // two come in the order they were committed.
    db.transaction(|tx| async move { put_access(&tx, "usr_ann", "owner").await }).await?;
    assert_eq!(written(&db).await?, (2, 2, "owner".to_string()));
    let queued: Vec<String> = db.query(Query::table("outbox").columns(["token"])).await?.column()?;
    assert_eq!(queued, ["access:usr_ann:member", "access:usr_ann:owner"]);

    // An event or a log line may be inserted with a number of its own, far
    // above the server's. The server carries on from there, and refuses a
    // number that is taken.
    db.insert(event("files", "numbered").set("id", 1_000_000)).await?;
    assert_eq!(db.insert(event("files", "next").returning(["id"])).await?.one()?.get::<i64>("id")?, 1_000_001);
    let err = db.insert(event("files", "again").set("id", 1_000_000)).await.unwrap_err();
    assert!(err.is_unique_violation() && err.violates(PRIMARY), "{err}");
    assert_eq!(db.insert(event("files", "after").returning(["id"])).await?.one()?.get::<i64>("id")?, 1_000_002);
    Ok(())
}

/// The log, newest first: the page before the line numbered `before`, of
/// everyone's lines or of one user's. Both are a walk backwards, through
/// the primary key or through `(user_id, id)`.
fn audit_page(user: &str, before: Option<i64>, page: u64) -> Query {
    let upper = before.map_or(Bound::Unbounded, |id| Bound::excluded([id]));
    let lines = if user.is_empty() {
        Query::table("audit").range(KeyRange::new(Bound::Unbounded, upper))
    } else {
        Query::table("audit").index("audit_user", KeyRange::within([user], Bound::Unbounded, upper))
    };
    lines.reverse().limit(page).columns(["id", "at", "actor", "action", "user_id", "ip", "detail"])
}

// R-Q4, R-S5, R-T1, R-T6: keys the server numbers, 64-bit numbers as
// cursors, an index walked backwards, and JSON kept whole.
#[tokio::test(flavor = "multi_thread")]
async fn the_audit_log_is_numbered_by_the_server_and_paged_newest_first() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let before = db.now().await?;
    for n in 1..=12 {
        let user = if n % 2 == 1 { "usr_ann" } else { "usr_bob" };
        db.insert(audit_entry(Some(user), "login.ok", Some(user), json!({"n": n}))).await?;
    }
    let after = db.now().await?;

    // The number and the time are the server's.
    let lines = db.query(Query::table("audit")).await?;
    assert_eq!(column::<i64>(&lines, "id"), (1..=12).collect::<Vec<i64>>());
    let times = column::<Timestamp>(&lines, "at");
    assert!(before <= times[0] && times[11] <= after && times.windows(2).all(|pair| pair[0] <= pair[1]));
    // A line may name nobody and say nothing more.
    let bare = db.insert(Insert::into("audit").set("action", "boot").returning(["id", "actor", "user_id", "ip", "detail"])).await?.one()?;
    assert_eq!(
        (bare.get::<i64>("id")?, bare.get::<Option<String>>("actor")?, bare.get::<Option<String>>("user_id")?, bare.get::<Option<String>>("ip")?),
        (13, None, None, None)
    );
    assert_eq!(bare.get::<Json>("detail")?, json!({}));
    // The detail comes back whole, as Postgres' jsonb keeps it: an object's
    // members by the length of their names and then by bytes, at every
    // depth; a list in the order it was written.
    let detail = json!({"service": "files", "uris": ["https://b.example", "https://a.example"], "by": "usr_admin", "nested": {"zz": 1, "a": null}});
    db.insert(audit_entry(None, "service.update", None, detail)).await?;
    let line = db.query_one(audit_page("", None, 1)).await?;
    assert_eq!(line.columns(), ["id", "at", "actor", "action", "user_id", "ip", "detail"]);
    assert_eq!(
        (line.get::<i64>("id")?, line.get::<String>("action")?, line.get::<Option<String>>("ip")?),
        (14, "service.update".to_string(), Some("203.0.113.7".to_string()))
    );
    assert_eq!(
        line.get::<Json>("detail")?.to_string(),
        r#"{"by":"usr_admin","uris":["https://b.example","https://a.example"],"nested":{"a":null,"zz":1},"service":"files"}"#
    );

    // The cursor is a number: 9 comes after 10, as it would not if the two
    // were compared as text.
    let ids = |rows: Rows| column::<i64>(&rows, "id");
    assert_eq!(ids(db.query(audit_page("", None, 5)).await?), [14, 13, 12, 11, 10]);
    assert_eq!(ids(db.query(audit_page("", Some(10), 5)).await?), [9, 8, 7, 6, 5]);
    assert_eq!(ids(db.query(audit_page("", Some(5), 5)).await?), [4, 3, 2, 1]);
    assert!(db.query(audit_page("", Some(1), 5)).await?.is_empty());
    assert_eq!(ids(db.query(audit_page("", Some(i64::MAX), 3)).await?), [14, 13, 12]);

    // One user's lines, by the same cursor.
    assert_eq!(ids(db.query(audit_page("usr_ann", None, 4)).await?), [11, 9, 7, 5]);
    assert_eq!(ids(db.query(audit_page("usr_ann", Some(5), 4)).await?), [3, 1]);
    assert_eq!(ids(db.query(audit_page("usr_bob", Some(7), 2)).await?), [6, 4]);
    assert_eq!(ids(db.query(audit_page("usr_bob", Some(i64::MAX), 1)).await?), [12]);
    assert!(db.query(audit_page("usr_cy", None, 4)).await?.is_empty());

    // A line written with a number of its own, far above the server's,
    // is the newest, and the server numbers on from it.
    db.insert(audit_entry(Some("usr_ann"), "login.ok", Some("usr_ann"), json!({})).set("id", 5_000_000_000i64)).await?;
    db.insert(audit_entry(Some("usr_ann"), "logout", Some("usr_ann"), json!({}))).await?;
    assert_eq!(ids(db.query(audit_page("usr_ann", None, 3)).await?), [5_000_000_001, 5_000_000_000, 11]);
    assert_eq!(ids(db.query(audit_page("", Some(5_000_000_000), 2)).await?), [14, 13]);
    Ok(())
}

/// How often a user has signed in, and how often from this device. Run at
/// every successful sign-in, over the user's lines only.
fn sign_in_history(user: &str, device: &str) -> Query {
    Query::table("audit")
        .index("audit_user", KeyRange::eq([user]))
        .filter(col("action").eq("login.ok"))
        .select("logins", count())
        .select("on_device", count().filter(col("detail").json_text(["device"]).eq(device)))
}

// R-Q11, R-Q13, R-T9: two counts in one pass over an index range, one of
// them filtered on a member of a JSON column.
#[tokio::test(flavor = "multi_thread")]
async fn sign_in_history_counts_a_users_logins_and_those_from_one_device() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let lines = [
        ("usr_ann", "login.ok", json!({"device": "d1"})),
        ("usr_ann", "login.ok", json!({"device": "d1", "ua": "Firefox"})),
        ("usr_ann", "login.fail", json!({"device": "d1"})),
        ("usr_ann", "login.ok", json!({"device": "d2"})),
        ("usr_ann", "login.ok", json!({})),
        ("usr_ann", "login.ok", json!({"device": null})),
        ("usr_ann", "login.ok", json!({"device": "d1"})),
        ("usr_ann", "login.fail", json!({"device": "d1"})),
        ("usr_bob", "login.ok", json!({"device": "d1"})),
        ("usr_bob", "login.ok", json!({"device": "d1"})),
    ];
    for (user, action, detail) in lines {
        db.insert(audit_entry(Some(user), action, Some(user), detail)).await?;
    }
    async fn history(db: &Client, user: &str, device: &str) -> DbResult<(i64, i64)> {
        let counted = db.query_one(sign_in_history(user, device)).await?;
        Ok((counted.get("logins")?, counted.get("on_device")?))
    }

    // Both counts come from one pass over her lines. A line without a
    // device, or with none, is a login but on no device.
    assert_eq!(history(&db, "usr_ann", "d1").await?, (6, 3));
    assert_eq!(history(&db, "usr_ann", "d2").await?, (6, 1));
    assert_eq!(history(&db, "usr_ann", "d9").await?, (6, 0));
    assert_eq!(history(&db, "usr_bob", "d1").await?, (2, 2));
    // A first sign-in: one row of zeros, not no row.
    assert_eq!(history(&db, "usr_new", "d1").await?, (0, 0));

    // The member the filter reads, as text or NULL.
    let devices =
        db.query(Query::table("audit").index("audit_user", KeyRange::eq(["usr_ann"])).select("device", col("detail").json_text(["device"]))).await?;
    let d1 = || Some("d1".to_string());
    assert_eq!(devices.column::<Option<String>>()?, [d1(), d1(), d1(), Some("d2".to_string()), None, None, d1(), d1()]);
    Ok(())
}

/// What the rate limiter answers as `retry_after`: the seconds until the
/// key's window has passed, rounded up, and at least one.
fn retry_after(window_seconds: i64) -> Expr {
    let left = (col("window_start") + seconds(window_seconds) - now()) / 1_000_000.0;
    greatest([val(1), call(Func::Ceil, [left])]).cast(Type::Int)
}

/// Counts a hit against a key in one statement and answers with the count.
/// The first hit starts a window; a hit after the window has passed starts
/// a new one at 1. Both assignments read the row as it was, so the count's
/// test of the window is not fooled by the window's own assignment before
/// it.
fn hit(key: &str, window_seconds: i64) -> Insert {
    let passed = || col("window_start").le(now() - seconds(window_seconds));
    Insert::into("throttle")
        .set("key", key)
        .set("window_start", now())
        .set("hits", 1)
        .on_conflict_update(PRIMARY)
        .update("window_start", when(passed(), now(), col("window_start")))
        .update("hits", when(passed(), 1, col("hits") + 1))
        .returning(["hits"])
        .returning_expr("retry_after", retry_after(window_seconds))
}

// R-Q16, R-S10, R-W4, R-X2, R-X3, R-X7: an upsert on a volatile table whose
// assignments all read the row as it was, and which counts every one of
// twenty concurrent hits.
#[tokio::test(flavor = "multi_thread")]
async fn the_rate_limiter_counts_a_hit_in_one_statement_and_restarts_a_window_that_has_passed() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let key = "ip:203.0.113.7:login";
    let counted = |rows: Rows| -> DbResult<(i64, i64)> {
        let row = rows.one()?;
        Ok((row.get("hits")?, row.get("retry_after")?))
    };
    async fn window(db: &impl Db, key: &str) -> DbResult<Timestamp> {
        db.get("throttle", [key]).await?.expect("the key is counted").get("window_start")
    }

    // The first hit inserts, with the whole window ahead.
    let first = db.insert(hit(key, 60)).await?;
    assert_eq!((first.affected(), first.columns()), (1, ["hits".to_string(), "retry_after".to_string()].as_slice()));
    assert_eq!(counted(first)?, (1, 60));
    let started = window(&db, key).await?;
    // Later hits count up in the same window, each key on its own.
    for expected in 2..=5 {
        let (hits, wait) = counted(db.insert(hit(key, 60)).await?)?;
        assert_eq!(hits, expected);
        assert!((55..=60).contains(&wait), "{wait} seconds");
    }
    assert_eq!(window(&db, key).await?, started);
    assert_eq!(counted(db.insert(hit("ip:198.51.100.9:login", 60)).await?)?, (1, 60));

    // The window has passed: the count starts over, in a new window.
    db.update(Update::table("throttle").key([key]).set("window_start", now() - seconds(61))).await?;
    assert_eq!(counted(db.insert(hit(key, 60)).await?)?, (1, 60));
    assert!(window(&db, key).await? > started);

    // Inside a transaction the clock stands still, which shows the edges
    // exactly: a window is over when it is exactly its length old; half
    // a second left is one second to wait.
    let tx = db.begin().await?;
    let set_back = |by: Expr, hits: i64| Update::table("throttle").key([key]).set("window_start", now() - by).set("hits", hits);
    tx.update(set_back(seconds(60), 9)).await?;
    assert_eq!(counted(tx.insert(hit(key, 60)).await?)?, (1, 60));
    assert_eq!(window(&tx, key).await?, tx.now().await?);
    tx.update(set_back(seconds_f64(59.5), 9)).await?;
    assert_eq!(counted(tx.insert(hit(key, 60)).await?)?, (10, 1));
    tx.update(set_back(seconds(18), 3)).await?;
    assert_eq!(counted(tx.insert(hit(key, 60)).await?)?, (4, 42));
    tx.commit().await?;

    // Twenty requests at once from one client: each is counted, and each
    // is told a different count.
    let mut requests = Vec::new();
    for _ in 0..20 {
        let db = db.clone();
        requests.push(tokio::spawn(async move { db.insert(hit("client:files-web:token", 60)).await?.one()?.get::<i64>("hits") }));
    }
    let mut counts = Vec::new();
    for request in requests {
        counts.push(request.await.expect("the task ran")?);
    }
    counts.sort();
    assert_eq!(counts, (1..=20).collect::<Vec<i64>>());
    Ok(())
}

/// Counts a failed sign-in, and restarts the clock its back-off is measured
/// from.
fn failure(key: &str) -> Insert {
    Insert::into("throttle")
        .set("key", key)
        .set("window_start", now())
        .set("hits", 1)
        .on_conflict_update(PRIMARY)
        .update("window_start", now())
        .update("hits", col("hits") + 1)
}

/// How many seconds a login that keeps failing must still wait. Nothing for
/// the first four failures; from the fifth a minute, doubled by each
/// further one, up to a quarter of an hour.
fn back_off(key: &str) -> Query {
    let pause = least([val(60) * call(Func::Power, [val(2), col("hits") - 5]), val(900)]);
    let left = (col("window_start") + (pause * 1_000_000).cast(Type::Int) - now()) / 1_000_000.0;
    Query::table("throttle").key([key]).filter(col("hits").ge(5)).select("wait", greatest([val(0), call(Func::Ceil, [left])]).cast(Type::Int))
}

// R-Q16, R-W4, R-X7: an upsert that counts, and arithmetic with POWER,
// LEAST, GREATEST and CEIL against a clock that stands still inside a
// transaction.
#[tokio::test(flavor = "multi_thread")]
async fn repeated_failures_back_off_by_doubling_up_to_a_quarter_of_an_hour() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let key = "login:user:usr_ann";

    // In one transaction, where the clock stands still and so the waits
    // are exact.
    let tx = db.begin().await?;
    for _ in 0..4 {
        tx.insert(failure(key)).await?;
    }
    assert!(tx.query_opt(back_off(key)).await?.is_none());
    for expected in [60, 120, 240, 480, 900, 900] {
        tx.insert(failure(key)).await?;
        assert_eq!(tx.query_one(back_off(key)).await?.get::<i64>("wait")?, expected);
    }
    // The wait runs from the last failure, and is never less than none.
    tx.update(Update::table("throttle").key([key]).set("window_start", now() - seconds(100))).await?;
    assert_eq!(tx.query_one(back_off(key)).await?.get::<i64>("wait")?, 800);
    tx.update(Update::table("throttle").key([key]).set("window_start", now() - seconds_f64(899.25))).await?;
    assert_eq!(tx.query_one(back_off(key)).await?.get::<i64>("wait")?, 1);
    tx.update(Update::table("throttle").key([key]).set("window_start", now() - seconds(1000))).await?;
    assert_eq!(tx.query_one(back_off(key)).await?.get::<i64>("wait")?, 0);
    tx.commit().await?;
    assert_eq!(db.get("throttle", [key]).await?.expect("the key is counted").get::<i64>("hits")?, 10);

    // As the service runs them, each statement on its own: the fifth
    // failure costs about a minute.
    let other = "login:user:usr_bob";
    for _ in 0..5 {
        assert_eq!(db.insert(failure(other)).await?.affected(), 1);
    }
    let wait: i64 = db.query_one(back_off(other)).await?.get("wait")?;
    assert!((55..=60).contains(&wait), "{wait} seconds");

    // A sign-in that succeeds forgets the failures.
    assert_eq!(db.delete(Delete::from("throttle").key([other])).await?.affected(), 1);
    assert!(db.query_opt(back_off(other)).await?.is_none());
    assert_eq!(db.delete(Delete::from("throttle").key([other])).await?.affected(), 0);
    db.insert(failure(other)).await?;
    assert_eq!(db.get("throttle", [other]).await?.expect("the key is counted").get::<i64>("hits")?, 1);
    Ok(())
}

// R-Q16, R-W5, R-X3: an insert that does nothing on a conflict tells the
// first caller from the rest, also when five arrive together.
#[tokio::test(flavor = "multi_thread")]
async fn a_once_only_key_goes_to_the_first_to_ask_and_a_count_can_be_read_without_counting() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;

    // A WebAuthn challenge may be answered once. The insert says whether
    // this was the first time the key was seen.
    let first_time =
        |key: &str| Insert::into("throttle").set("key", key).set("window_start", now()).set("hits", 1).on_conflict_do_nothing().returning(["key"]);
    let taken = db.insert(first_time("once:webauthn:challenge-1")).await?;
    assert_eq!((taken.affected(), taken.column::<String>()?), (1, vec!["once:webauthn:challenge-1".to_string()]));
    let again = db.insert(first_time("once:webauthn:challenge-1")).await?;
    assert!(again.is_empty());
    assert_eq!(again.affected(), 0);
    assert_eq!(db.get("throttle", ["once:webauthn:challenge-1"]).await?.expect("the key is there").get::<i64>("hits")?, 1);

    // The same answer sent five times at once is accepted once.
    let mut answers = Vec::new();
    for _ in 0..5 {
        let db = db.clone();
        answers.push(tokio::spawn(async move { db.insert(first_time("once:webauthn:challenge-2")).await.map(|rows| rows.len()) }));
    }
    let mut accepted = 0;
    for answer in answers {
        accepted += answer.await.expect("the task ran")?;
    }
    assert_eq!(accepted, 1);

    // What a key's count is while its window lasts, without adding to it.
    let peek = |key: &str| {
        Query::table("throttle")
            .key([key])
            .filter(col("window_start").gt(now() - seconds(60)))
            .select("hits", col("hits"))
            .select("retry_after", retry_after(60))
    };
    for _ in 0..3 {
        db.insert(hit("introspect:files-web", 60)).await?;
    }
    for _ in 0..2 {
        let seen = db.query_one(peek("introspect:files-web")).await?;
        assert_eq!(seen.get::<i64>("hits")?, 3);
        assert!((55..=60).contains(&seen.get::<i64>("retry_after")?));
    }
    db.update(Update::table("throttle").key(["introspect:files-web"]).set("window_start", now() - seconds(60))).await?;
    assert!(db.query_opt(peek("introspect:files-web")).await?.is_none());
    assert!(db.query_opt(peek("introspect:chat-web")).await?.is_none());
    Ok(())
}

/// The revision and every service with its clients, in one read, so that
/// all three come from one moment. The first join has no condition: the
/// revision's row is paired with every service, and comes back alone when
/// there is none. A service without clients comes back once, with NULL
/// where a client's columns would be.
fn registry() -> Query {
    Query::table("services_revision")
        .alias("r")
        .left_join(Query::table("services").alias("s"))
        .left_join(Query::table("clients").alias("c").index("clients_service", KeyRange::eq([col("s.name")])))
        .order_by(col("s.created_at"))
        .order_by(col("s.name"))
        .order_by(col("c.created_at"))
        .order_by(col("c.id"))
        .select("revision", col("r.revision"))
        .select("name", col("s.name"))
        .select("roles", col("s.roles"))
        .select("mfa_roles", col("s.mfa_roles"))
        .select("signup", col("s.signup"))
        .select("default_role", col("s.default_role"))
        .select("manages_own_members", col("s.manages_own_members"))
        .select("api_key_scopes", col("s.api_key_scopes"))
        .select("app_password_scopes", col("s.app_password_scopes"))
        .select("events_url", col("s.events_url"))
        .select("client_id", col("c.id"))
        .select("client_type", col("c.type"))
        .select("redirect_uris", col("c.redirect_uris"))
        .select("post_logout_redirect_uris", col("c.post_logout_redirect_uris"))
}

/// Registering a service. The change is counted first, which on Postgres
/// also makes every other change to the services wait. Then the checks: the
/// name is free, there is room for another service, each client's id is
/// free. Then the rows and the log. A refusal undoes the count as well.
async fn register_service(db: &Client, name: &str, clients: &[(&str, &str)]) -> DbResult<bool> {
    let tx = db.begin().await?;
    tx.update(bump_revision()).await?;
    let mut free = tx.query_opt(Query::table("services").key([name]).select("one", 1)).await?.is_none();
    free &= rows_of(&tx, Query::table("services")).await? < 100;
    for (id, _) in clients {
        free &= tx.query_opt(Query::table("clients").key([*id]).columns(["service"])).await?.is_none();
    }
    if !free {
        tx.rollback().await?;
        return Ok(false);
    }
    tx.insert(new_service(name, "invite")).await?;
    for (id, kind) in clients {
        tx.insert(new_client(id, name, kind)).await?;
    }
    tx.insert(audit_entry(Some("usr_admin"), "service.create", None, json!({"service": name}))).await?;
    tx.commit().await?;
    Ok(true)
}

// R-Q9, R-Q11, R-Q21, R-T7, R-X1: two left joins, the first without a
// condition, read three tables at one moment, and lists of texts come back
// as lists.
#[tokio::test(flavor = "multi_thread")]
async fn the_registry_of_services_and_clients_is_loaded_whole_in_one_read() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let some = |text: &str| Some(text.to_string());

    // No service yet: the revision still comes, alone.
    let empty = db.query_one(registry()).await?;
    assert_eq!(empty.columns().len(), 14);
    assert_eq!((empty.get::<i64>("revision")?, empty.get::<Option<String>>("name")?, empty.get::<Option<String>>("client_id")?), (0, None, None));
    assert_eq!(empty.get::<Option<Vec<String>>>("roles")?, None);

    assert!(register_service(&db, "chat", &[("chat-web", "confidential")]).await?);
    assert!(register_service(&db, "files", &[("files-web", "confidential"), ("files-ios", "public")]).await?);
    assert!(register_service(&db, "wiki", &[]).await?);
    // A name or a client id that is taken: refused, and nothing of the
    // attempt is left, not even the count of it.
    assert!(!register_service(&db, "files", &[("files-tv", "public")]).await?);
    assert!(!register_service(&db, "calendar", &[("calendar-web", "confidential"), ("files-web", "confidential")]).await?);
    assert_eq!(
        (rows_of(&db, Query::table("services")).await?, rows_of(&db, Query::table("clients")).await?, rows_of(&db, Query::table("audit")).await?),
        (3, 3, 3)
    );
    assert_eq!(db.query_one(Query::table("clients").key(["files-ios"]).columns(["service"])).await?.get::<String>("service")?, "files");

    // One row per client, and one for the service that has none. Every row
    // carries the revision. The two clients of one service were made in one
    // transaction, at one time, so their ids order them.
    let rows = db.query(registry()).await?;
    assert_eq!(column::<i64>(&rows, "revision"), [3, 3, 3, 3]);
    assert_eq!(column::<Option<String>>(&rows, "name"), [some("chat"), some("files"), some("files"), some("wiki")]);
    assert_eq!(column::<Option<String>>(&rows, "client_id"), [some("chat-web"), some("files-ios"), some("files-web"), None]);
    assert_eq!(column::<Option<String>>(&rows, "client_type"), [some("confidential"), some("public"), some("confidential"), None]);
    // Lists come back as lists, in the order written: a role's rank is
    // its place. An empty list is a list, and a missing client's is NULL.
    let files = rows.iter().nth(1).expect("the service has two rows");
    assert_eq!(files.get::<Vec<String>>("roles")?, ["admin", "member"]);
    assert_eq!(
        (files.get::<Vec<String>>("mfa_roles")?, files.get::<Vec<String>>("api_key_scopes")?),
        (vec!["admin".to_string()], vec!["read".to_string(), "write".to_string()])
    );
    assert_eq!(files.get::<Vec<String>>("app_password_scopes")?, no_texts());
    assert_eq!(
        (files.get::<String>("signup")?, files.get::<Option<String>>("default_role")?, files.get::<bool>("manages_own_members")?),
        ("invite".to_string(), some("member"), false)
    );
    assert_eq!(files.get::<Option<String>>("events_url")?, some("https://files.example/events"));
    assert_eq!(files.get::<Vec<String>>("redirect_uris")?, ["https://files.example/callback"]);
    assert_eq!(files.get::<Vec<String>>("post_logout_redirect_uris")?, no_texts());
    assert_eq!(column::<Option<Vec<String>>>(&rows, "redirect_uris")[3], None);

    // The order is by the time a service was made, then its name; within
    // a service by the time a client was made, then its id.
    db.update(Update::table("services").key(["wiki"]).set("created_at", now() - days(1))).await?;
    db.update(Update::table("clients").key(["files-web"]).set("created_at", now() - days(1))).await?;
    let rows = db.query(registry()).await?;
    assert_eq!(column::<Option<String>>(&rows, "name"), [some("wiki"), some("chat"), some("files"), some("files")]);
    assert_eq!(column::<Option<String>>(&rows, "client_id"), [None, some("chat-web"), some("files-web"), some("files-ios")]);
    Ok(())
}

/// How many members of a service hold each of these roles.
fn members_by_role(service: &str, roles: Vec<&str>) -> Query {
    Query::table("access")
        .index("access_service", KeyRange::eq([service]))
        .filter(col("role").in_list(roles))
        .group_by([col("role")])
        .select("role", col("role"))
        .select("n", count())
        .order_by(col("role"))
}

/// How many open invitations to a service promise each of these roles.
fn invited_by_role(service: &str, roles: Vec<&str>) -> Query {
    Query::table("invites")
        .index("invites_service", KeyRange::eq([service]))
        .filter(all([col("role").in_list(roles), col("accepted_at").is_null(), col("expires_at").gt(now())]))
        .group_by([col("role")])
        .select("role", col("role"))
        .select("n", count())
        .order_by(col("role"))
}

/// What goes with a client: its secrets and its refresh tokens are revoked,
/// its codes deleted. None of the three has a foreign key to `clients`,
/// because a client may be declared in a file instead.
async fn retire_client(tx: &Transaction, client: &str) -> DbResult<(u64, u64, u64)> {
    let live_secrets = Query::table("client_secrets").range(KeyRange::eq([client])).filter(col("revoked_at").is_null());
    let secrets = tx.update(Update::rows(live_secrets).set("revoked_at", now())).await?;
    let tokens = tx
        .update(Update::table("refresh_tokens").filter(col("client_id").eq(client).and(col("revoked_at").is_null())).set("revoked_at", now()))
        .await?;
    let codes = tx.delete(Delete::from("auth_codes").filter(col("client_id").eq(client))).await?;
    Ok((secrets.affected(), tokens.affected(), codes.affected()))
}

// R-Q6, R-Q11, R-W6, R-W10, R-X1: counts by group over an index range, and
// one transaction that reads, updates, deletes and inserts in many tables.
#[tokio::test(flavor = "multi_thread")]
async fn replacing_a_service_changes_it_and_its_clients_and_what_hung_on_them_together() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    assert!(register_service(&db, "files", &[("files-web", "confidential"), ("files-old", "confidential")]).await?);
    assert!(register_service(&db, "chat", &[("chat-web", "confidential")]).await?);
    db.update(Update::table("clients").key(["files-web"]).set("redirect_uris", vec!["https://files.example/callback", "https://files.example/old"]))
        .await?;
    for (id, username, role) in [("usr_ann", "ann", "admin"), ("usr_bob", "bob", "member"), ("usr_cy", "cy", "member")] {
        db.insert(person(id, username, None)).await?;
        db.insert(grant_role(id, "files", role, "usr_admin")).await?;
    }
    db.insert(grant_role("usr_ann", "chat", "admin", "usr_admin")).await?;
    for (id, service, role, lifetime) in [
        ("inv_1", "files", "member", 3600),
        ("inv_2", "files", "member", -1),
        ("inv_3", "files", "admin", 3600),
        ("inv_4", "files", "admin", 3600),
        ("inv_5", "chat", "admin", 3600),
    ] {
        db.insert(new_invite(id, "someone@example.com", service, role, id, lifetime)).await?;
    }
    db.update(accept_invite("inv_3", "usr_ann")).await?;
    db.insert(new_session("ses_1", "usr_ann", "cookie-1")).await?;
    for (token, client) in [("web-1", "files-web"), ("old-1", "files-old"), ("old-2", "files-old"), ("chat-1", "chat-web")] {
        db.insert(session_token(token, client, "ses_1")).await?;
    }
    db.update(Update::table("refresh_tokens").key([digest("old-2")]).set("revoked_at", now() - hours(1))).await?;
    let revoked_before: Timestamp = refresh_token(&db, "old-2").await?.expect("the token is there").get("revoked_at")?;
    for (code, client, redirect_uri) in [
        ("code-kept", "files-web", "callback"),
        ("code-dropped", "files-web", "old"),
        ("code-old", "files-old", "callback"),
        ("code-chat", "chat-web", "callback"),
    ] {
        db.insert(new_code(code, client, "ses_1", 60)).await?;
        db.update(Update::table("auth_codes").key([digest(code)]).set("redirect_uri", format!("https://files.example/{redirect_uri}"))).await?;
    }
    for (client, secret) in [("files-web", "s-web"), ("files-old", "s-old"), ("chat-web", "s-chat")] {
        db.insert(new_secret(client, secret)).await?;
    }

    // Before a role is dropped, those who hold it and those who were
    // promised it are counted. One row per role that anyone has; an expired
    // or accepted invitation promises nothing.
    let counted = |rows: Rows| rows.map::<(String, i64)>();
    let asked = || vec!["admin", "member", "guest"];
    assert_eq!(counted(db.query(members_by_role("files", asked())).await?)?, [("admin".to_string(), 1), ("member".to_string(), 2)]);
    assert_eq!(counted(db.query(invited_by_role("files", asked())).await?)?, [("admin".to_string(), 1), ("member".to_string(), 1)]);
    assert_eq!(counted(db.query(members_by_role("files", vec!["member"])).await?)?, [("member".to_string(), 2)]);
    assert!(db.query(members_by_role("wiki", asked())).await?.is_empty());

    let dropped = db
        .transaction(|tx| async move {
            tx.update(bump_revision()).await?;
            // The service and its clients as they stand, the clients oldest
            // first. Nothing can change them before this transaction ends,
            // which Postgres needs row locks for.
            let service_columns = [
                "name",
                "roles",
                "mfa_roles",
                "signup",
                "default_role",
                "manages_own_members",
                "api_key_scopes",
                "app_password_scopes",
                "events_url",
            ];
            let current = tx.query_one(Query::table("services").key(["files"]).columns(service_columns)).await?;
            assert_eq!(current.get::<Vec<String>>("roles")?, ["admin", "member"]);
            let client_columns = ["id", "type", "redirect_uris", "post_logout_redirect_uris"];
            let clients = tx
                .query(
                    Query::table("clients")
                        .index("clients_service", KeyRange::eq(["files"]))
                        .order_by(col("created_at"))
                        .order_by(col("id"))
                        .columns(client_columns),
                )
                .await?;
            assert_eq!(column::<String>(&clients, "id"), ["files-old", "files-web"]);
            // The new client's id is free, and the role that goes is
            // neither held nor promised.
            assert!(tx.query_opt(Query::table("clients").key(["files-app"]).columns(["service"])).await?.is_none());
            assert!(tx.query(members_by_role("files", vec!["guest"])).await?.is_empty());
            assert!(tx.query(invited_by_role("files", vec!["guest"])).await?.is_empty());

            // The service's new definition.
            let redefined = Update::table("services")
                .key(["files"])
                .set("roles", vec!["owner", "admin", "member"])
                .set("mfa_roles", vec!["owner", "admin"])
                .set("signup", "open")
                .set("default_role", null())
                .set("manages_own_members", true)
                .set("api_key_scopes", no_texts())
                .set("app_password_scopes", vec!["webdav"])
                .set("events_url", "https://files.example/hooks")
                .set("updated_at", now());
            assert_eq!(tx.update(redefined).await?.affected(), 1);
            // The client that goes, after what hung on it.
            let retired = retire_client(&tx, "files-old").await?;
            assert_eq!(tx.delete(Delete::from("clients").key(["files-old"])).await?.affected(), 1);
            // The client that stays loses a redirect URI, and the codes
            // issued for that URI.
            tx.update(
                Update::table("clients")
                    .key(["files-web"])
                    .set("redirect_uris", vec!["https://files.example/callback"])
                    .set("post_logout_redirect_uris", vec!["https://files.example/bye"]),
            )
            .await?;
            let stale = Delete::from("auth_codes")
                .filter(col("client_id").eq("files-web").and(col("redirect_uri").in_list(vec!["https://files.example/old"])));
            let stale = tx.delete(stale).await?.affected();
            // The client that is new, and the log.
            tx.insert(new_client("files-app", "files", "public")).await?;
            tx.insert(audit_entry(Some("usr_admin"), "service.update", None, json!({"service": "files"}))).await?;
            Ok::<_, DbError>((retired, stale))
        })
        .await?;
    assert_eq!(dropped, ((1, 1, 1), 1));

    let files = db.get("services", ["files"]).await?.expect("the service is there");
    assert_eq!(files.get::<Vec<String>>("roles")?, ["owner", "admin", "member"]);
    assert_eq!(
        (files.get::<String>("signup")?, files.get::<Option<String>>("default_role")?, files.get::<bool>("manages_own_members")?),
        ("open".to_string(), None, true)
    );
    assert_eq!(
        (files.get::<Vec<String>>("api_key_scopes")?, files.get::<Vec<String>>("app_password_scopes")?),
        (no_texts(), vec!["webdav".to_string()])
    );
    assert!(files.get::<Timestamp>("updated_at")? > files.get::<Timestamp>("created_at")?);
    assert!(!files.get::<bool>("held")?);
    assert_eq!(db.query_one(Query::table("services_revision")).await?.get::<i64>("revision")?, 3);

    assert_eq!(
        sorted(&db, Query::table("clients").index("clients_service", KeyRange::eq(["files"])).columns(["id"])).await?,
        ["files-app", "files-web"]
    );
    let web = db.get("clients", ["files-web"]).await?.expect("the client is there");
    assert_eq!(
        (web.get::<Vec<String>>("redirect_uris")?, web.get::<Vec<String>>("post_logout_redirect_uris")?),
        (vec!["https://files.example/callback".to_string()], vec!["https://files.example/bye".to_string()])
    );

    // Of the client that went: the secret and the live token are kept,
    // revoked at the time of the change; the token revoked earlier keeps
    // its own time; the code is gone.
    let changed: Timestamp = files.get("updated_at")?;
    assert_eq!(
        db.query_one(Query::table("client_secrets").key([val("files-old"), val(digest("s-old"))])).await?.get::<Option<Timestamp>>("revoked_at")?,
        Some(changed)
    );
    assert_eq!(refresh_token(&db, "old-1").await?.expect("the token is kept").get::<Option<Timestamp>>("revoked_at")?, Some(changed));
    assert_eq!(refresh_token(&db, "old-2").await?.expect("the token is kept").get::<Option<Timestamp>>("revoked_at")?, Some(revoked_before));
    assert!(db.get("auth_codes", [digest("code-old")]).await?.is_none());
    // Of the client that stayed, and of the other service: only the code
    // for the dropped URI is gone.
    assert!(db.get("auth_codes", [digest("code-dropped")]).await?.is_none());
    assert!(db.get("auth_codes", [digest("code-kept")]).await?.is_some());
    assert!(db.get("auth_codes", [digest("code-chat")]).await?.is_some());
    assert!(refresh_token(&db, "web-1").await?.expect("the token is there").is_null("revoked_at")?);
    assert!(refresh_token(&db, "chat-1").await?.expect("the token is there").is_null("revoked_at")?);
    assert_eq!(rows_of(&db, Query::table("client_secrets").filter(col("revoked_at").is_null())).await?, 2);
    assert_eq!(rows_of(&db, Query::table("access")).await?, 4);
    Ok(())
}

/// Deleting a service. What points at a service by name has no foreign key,
/// so it is removed or revoked by hand: roles, invitations, credentials,
/// undelivered events, and what hangs on each client. Only the clients
/// themselves go with the service's row.
async fn delete_service(db: &Client, name: &str) -> DbResult<bool> {
    let tx = db.begin().await?;
    tx.update(bump_revision()).await?;
    // The service, and its clients in a fixed order, which matters where
    // each row is locked as it is read.
    if tx.query_opt(Query::table("services").key([name]).columns(["name"])).await?.is_none() {
        tx.rollback().await?;
        return Ok(false);
    }
    let clients: Vec<String> =
        tx.query(Query::table("clients").index("clients_service", KeyRange::eq([name])).order_by(col("id")).columns(["id"])).await?.column()?;
    tx.delete(Delete::from("access").index("access_service", KeyRange::eq([name]))).await?;
    tx.delete(Delete::from("invites").index("invites_service", KeyRange::eq([name]))).await?;
    tx.update(Update::table("credentials").filter(col("service").eq(name).and(col("revoked_at").is_null())).set("revoked_at", now())).await?;
    tx.delete(Delete::from("outbox").filter(all([col("service").eq(name), col("delivered_at").is_null(), col("failed_at").is_null()]))).await?;
    for client in &clients {
        retire_client(&tx, client).await?;
    }
    let gone = tx.delete(Delete::from("services").key([name])).await?.affected();
    tx.insert(audit_entry(Some("usr_admin"), "service.delete", None, json!({"service": name, "clients": clients}))).await?;
    tx.commit().await?;
    Ok(gone == 1)
}

// R-S6, R-W6, R-W10, R-X1: deletes and updates of many rows by index and by
// filter in one transaction, beside the one cascade a foreign key does.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_service_cleans_up_by_hand_what_no_foreign_key_reaches() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    assert!(register_service(&db, "files", &[("files-web", "confidential"), ("files-ios", "public")]).await?);
    assert!(register_service(&db, "chat", &[("chat-web", "confidential")]).await?);
    db.insert(person("usr_ann", "ann", None)).await?;
    db.insert(new_session("ses_1", "usr_ann", "cookie-1")).await?;
    for service in ["files", "chat"] {
        let client = format!("{service}-web");
        db.insert(grant_role("usr_ann", service, "member", "usr_admin")).await?;
        db.insert(new_invite(&format!("inv_{service}"), "someone@example.com", service, "member", &format!("invite-{service}"), 3600)).await?;
        db.insert(new_credential(&format!("key_{service}"), "usr_ann", "api_key", service, &format!("key-{service}"), None)).await?;
        db.insert(event(service, "pending")).await?;
        db.insert(event(service, "delivered").set("delivered_at", now())).await?;
        db.insert(event(service, "given-up").set("failed_at", now())).await?;
        db.insert(new_secret(&client, &format!("secret-{service}"))).await?;
        db.insert(session_token(&format!("token-{service}"), &client, "ses_1")).await?;
        db.insert(new_code(&format!("code-{service}"), &client, "ses_1", 60)).await?;
    }
    db.insert(public_token("token-ios", "ses_ios", "usr_ann", "files-ios")).await?;
    async fn left(db: &Client, service: &str) -> DbResult<Vec<i64>> {
        let client = format!("{service}-web");
        let key = Query::table("credentials").key([format!("key_{service}")]);
        Ok(vec![
            rows_of(db, Query::table("services").key([service])).await?,
            rows_of(db, Query::table("clients").index("clients_service", KeyRange::eq([service]))).await?,
            rows_of(db, Query::table("access").index("access_service", KeyRange::eq([service]))).await?,
            rows_of(db, Query::table("invites").index("invites_service", KeyRange::eq([service]))).await?,
            rows_of(db, key.clone()).await?,
            rows_of(db, key.filter(col("revoked_at").is_null())).await?,
            rows_of(db, Query::table("outbox").filter(col("service").eq(service))).await?,
            rows_of(db, Query::table("client_secrets").range(KeyRange::eq([client.as_str()])).filter(col("revoked_at").is_null())).await?,
            rows_of(db, Query::table("refresh_tokens").filter(col("client_id").eq(client.as_str()).and(col("revoked_at").is_null()))).await?,
            rows_of(db, Query::table("auth_codes").filter(col("client_id").eq(client.as_str()))).await?,
        ])
    }
    assert_eq!(left(&db, "files").await?, [1, 2, 1, 1, 1, 1, 3, 1, 1, 1]);

    assert!(delete_service(&db, "files").await?);
    // The service and both its clients are gone, with its roles, its
    // invitation, its undelivered event and its client's code. The
    // credential, the secret and the tokens are kept, revoked. Events
    // already delivered or given up stay for the record.
    assert_eq!(left(&db, "files").await?, [0, 0, 0, 0, 1, 0, 2, 0, 0, 0]);
    assert_eq!(sorted(&db, Query::table("outbox").filter(col("service").eq("files")).columns(["token"])).await?, ["delivered", "given-up"]);
    assert_eq!(rows_of(&db, Query::table("client_secrets").range(KeyRange::eq(["files-web"]))).await?, 1);
    assert!(!refresh_token(&db, "token-files").await?.expect("the token is kept").is_null("revoked_at")?);
    assert!(!refresh_token(&db, "token-ios").await?.expect("the token is kept").is_null("revoked_at")?);
    // The other service has everything it had.
    assert_eq!(left(&db, "chat").await?, [1, 1, 1, 1, 1, 1, 3, 1, 1, 1]);
    assert_eq!(db.query_one(Query::table("services_revision")).await?.get::<i64>("revision")?, 3);
    let logged = db.query_one(Query::table("audit").filter(col("action").eq("service.delete"))).await?;
    assert_eq!(logged.get::<Json>("detail")?, json!({"service": "files", "clients": ["files-ios", "files-web"]}));

    // A service that is not there: nothing happens, and the count of
    // changes stays where it was.
    assert!(!delete_service(&db, "files").await?);
    assert_eq!(db.query_one(Query::table("services_revision")).await?.get::<i64>("revision")?, 3);
    Ok(())
}

// R-Q6, R-Q14, R-W7, R-X3: a list column compared with a list, an update of
// several keys at once, and an update that returns each row it changed to
// one caller only.
#[tokio::test(flavor = "multi_thread")]
async fn held_services_are_flagged_and_their_release_is_reported_once() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    for name in ["files", "chat", "wiki"] {
        db.insert(new_service(name, "invite")).await?;
    }
    db.update(Update::table("services").key(["chat"]).set("api_key_scopes", no_texts())).await?;

    // The services that issue API keys, which are those whose list of
    // scopes for them is not the empty list.
    let issuing = db.query(Query::table("services").filter(col("api_key_scopes").ne(no_texts())).columns(["name"])).await?;
    assert_eq!(issuing.column::<String>()?, ["files", "wiki"]);

    // Of the services named, those not yet held are held now.
    let hold = |names: Vec<&str>| {
        Update::rows(Query::table("services").keys(names.into_iter().map(|name| [name])).filter(col("held").not())).set("held", true)
    };
    assert_eq!(db.update(hold(vec!["files", "chat", "calendar"])).await?.affected(), 2);
    assert_eq!(db.update(hold(vec!["files", "chat"])).await?.affected(), 0);
    let held = |rows: Rows| rows.map::<(String, bool)>();
    let flags = Query::table("services").columns(["name", "held"]);
    assert_eq!(held(db.query(flags.clone()).await?)?, [("chat".to_string(), true), ("files".to_string(), true), ("wiki".to_string(), false)]);

    // At start-up every process releases the services that are no longer
    // held and logs each release. Three start together: each service is
    // named to exactly one of them, and logged once.
    let release =
        |names: Vec<&str>| Update::table("services").filter(col("name").in_list(names).and(col("held"))).set("held", false).returning(["name"]);
    let mut starts = Vec::new();
    for _ in 0..3 {
        let db = db.clone();
        starts.push(tokio::spawn(async move {
            db.transaction(|tx| async move {
                let released: Vec<String> = tx.update(release(vec!["files", "chat", "wiki"])).await?.column()?;
                for name in &released {
                    tx.insert(audit_entry(None, "service.released", None, json!({"service": name}))).await?;
                }
                Ok::<_, DbError>(released)
            })
            .await
        }));
    }
    let mut released = Vec::new();
    for start in starts {
        released.extend(start.await.expect("the task ran")?);
    }
    released.sort();
    assert_eq!(released, ["chat", "files"]);
    assert_eq!(rows_of(&db, Query::table("audit")).await?, 2);
    assert!(held(db.query(flags).await?)?.iter().all(|(_, held)| !held));
    Ok(())
}

// R-X2, R-X8: a writing transaction keeps every other writer waiting until
// it ends, where SQL would take shared row locks, and keeps no reader
// waiting.
#[tokio::test(flavor = "multi_thread")]
async fn a_grant_and_a_change_to_its_service_never_overlap() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(new_service("files", "invite")).await?;
    db.insert(new_service("chat", "invite")).await?;
    db.insert(new_client("files-web", "files", "confidential")).await?;
    db.insert(person("usr_ann", "ann", None)).await?;

    // A grant checks the role against the service's definition, then
    // writes. Postgres would hold a shared lock on the service's row in
    // between, so that the definition cannot change or vanish under it.
    // Here the grant's transaction holds the write lock.
    let grant = db.begin().await?;
    let service = grant
        .query_one(Query::table("services").key(["files"]).columns([
            "roles",
            "mfa_roles",
            "signup",
            "default_role",
            "api_key_scopes",
            "app_password_scopes",
        ]))
        .await?;
    assert!(service.get::<Vec<String>>("roles")?.contains(&"member".to_string()));
    assert_eq!(grant.query(Query::table("clients").key(["files-web"]).filter(col("service").eq("files")).select("one", 1)).await?.len(), 1);

    // Another process deletes the service at that moment: it waits.
    let other = db.clone();
    let removal = tokio::spawn(async move { other.delete(Delete::from("services").key(["files"])).await.map(|rows| rows.affected()) });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!removal.is_finished(), "a change to the service waits for the grant");
    // Readers wait for neither, and see what is committed.
    assert_eq!(rows_of(&db, Query::table("services")).await?, 2);
    assert!(db.query_opt(role_of("usr_ann", "files")).await?.is_none());

    grant.insert(grant_role("usr_ann", "files", "member", "usr_admin")).await?;
    grant.commit().await?;
    assert_eq!(removal.await.expect("the task ran")?, 1);
    assert_eq!((rows_of(&db, Query::table("services")).await?, rows_of(&db, Query::table("clients")).await?), (1, 0));
    assert_eq!(db.query_one(role_of("usr_ann", "files")).await?.get::<String>("role")?, "member");

    // The other way round: while a change to a service is open, a grant
    // waits, and then judges by the service as changed.
    let change = db.begin().await?;
    change.update(bump_revision()).await?;
    change.update(Update::table("services").key(["chat"]).set("roles", vec!["owner"]).set("updated_at", now())).await?;
    let other = db.clone();
    let late_grant = tokio::spawn(async move {
        other
            .transaction(|tx| async move {
                let roles: Vec<String> = tx.query_one(Query::table("services").key(["chat"]).columns(["roles"])).await?.get("roles")?;
                if !roles.contains(&"member".to_string()) {
                    return Ok(false);
                }
                tx.insert(grant_role("usr_ann", "chat", "member", "usr_admin")).await?;
                Ok::<_, DbError>(true)
            })
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!late_grant.is_finished(), "the grant waits for the change");
    change.commit().await?;
    assert!(!late_grant.await.expect("the task ran")?);
    assert!(db.query_opt(role_of("usr_ann", "chat")).await?.is_none());
    Ok(())
}

/// The hourly housekeeping: eight deletes in one transaction, each for rows
/// that expired long enough ago. Only one process runs it at a time and the
/// others skip the pass, for which Postgres has a try-lock that lasts as
/// long as the transaction. A writing transaction here would wait instead
/// of skipping, so the pass is put inside a named lock that is only tried.
/// Answers with what each delete removed, or `None` if another process was
/// at it.
async fn housekeeping(db: &Client, audit_days: i64) -> DbResult<Option<[u64; 8]>> {
    let Some(lock) = db.try_lock("app.housekeeping").await? else {
        return Ok(None);
    };
    let passes = [
        Delete::from("auth_codes").filter(col("expires_at").lt(now() - hours(1))),
        Delete::from("one_time_tokens").filter(col("expires_at").lt(now() - days(1))),
        Delete::from("sessions").filter(col("expires_at").lt(now()).or(col("ends_at").lt(now()))),
        Delete::from("refresh_tokens").filter(col("expires_at").lt(now()).or(col("revoked_at").lt(now() - days(1)))),
        Delete::from("invites").filter(col("accepted_at").is_null().and(col("expires_at").lt(now() - days(30)))),
        Delete::from("throttle").filter(col("window_start").lt(now() - days(1))),
        Delete::from("outbox").filter(col("delivered_at").lt(now() - days(7)).or(col("failed_at").lt(now() - days(7)))),
        Delete::rows(Query::table("audit").index("audit_at", KeyRange::new(Bound::Unbounded, Bound::excluded([now() - days(audit_days)])))),
    ];
    let tx = db.begin().await?;
    let mut removed = [0; 8];
    for (n, pass) in passes.into_iter().enumerate() {
        removed[n] = tx.delete(pass).await?.affected();
    }
    tx.commit().await?;
    lock.unlock().await?;
    Ok(Some(removed))
}

// R-Q8, R-S4, R-S6, R-T9, R-W10, R-X1, R-X5: eight filtered deletes in one
// transaction, under a named lock that is tried and not waited for.
#[tokio::test(flavor = "multi_thread")]
async fn housekeeping_deletes_in_one_pass_what_expired_long_enough_ago() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", None)).await?;
    // For each rule a row just past its limit and one just short of it.
    // Codes: an hour after they expire.
    db.insert(new_session("ses_live", "usr_ann", "cookie-live")).await?;
    db.insert(new_code("code-old", "files-web", "ses_live", -61 * 60)).await?;
    db.insert(new_code("code-recent", "files-web", "ses_live", -59 * 60)).await?;
    // One-time tokens: a day after they expire.
    db.insert(one_time_token("token-old", "reset", "usr_ann", None, -25 * 3600)).await?;
    db.insert(one_time_token("token-recent", "reset", "usr_ann", None, -23 * 3600)).await?;
    // Sessions: as soon as either expiry has passed, with their tokens
    // and codes.
    db.insert(new_session("ses_idle", "usr_ann", "cookie-idle")).await?;
    db.insert(new_session("ses_ended", "usr_ann", "cookie-ended")).await?;
    db.insert(session_token("web-idle", "files-web", "ses_idle")).await?;
    db.insert(new_code("code-idle", "files-web", "ses_idle", 60)).await?;
    db.update(Update::table("sessions").key(["ses_idle"]).set("expires_at", now() - seconds(1))).await?;
    db.update(Update::table("sessions").key(["ses_ended"]).set("ends_at", now() - seconds(1))).await?;
    // Refresh tokens: when they expire, or a day after they were revoked.
    db.insert(public_token("ios-expired", "ses_a", "usr_ann", "files-ios")).await?;
    db.update(Update::table("refresh_tokens").key([digest("ios-expired")]).set("expires_at", now() - seconds(1))).await?;
    db.insert(public_token("ios-revoked-old", "ses_b", "usr_ann", "files-ios").set("revoked_at", now() - hours(25))).await?;
    db.insert(public_token("ios-revoked-recent", "ses_c", "usr_ann", "files-ios").set("revoked_at", now() - hours(23))).await?;
    db.insert(public_token("ios-live", "ses_d", "usr_ann", "files-ios")).await?;
    // Invitations nobody accepted: thirty days after they expire. An
    // accepted one is kept for good.
    db.insert(new_invite("inv_old", "a@example.com", "files", "member", "invite-old", -31 * 86_400)).await?;
    db.insert(new_invite("inv_recent", "b@example.com", "files", "member", "invite-recent", -29 * 86_400)).await?;
    db.insert(new_invite("inv_accepted", "c@example.com", "files", "member", "invite-accepted", 3600)).await?;
    db.update(accept_invite("invite-accepted", "usr_ann")).await?;
    db.update(Update::table("invites").key(["inv_accepted"]).set("expires_at", now() - days(60))).await?;
    // Rate-limit windows: after a day.
    for (key, hours_old) in [("old", 25), ("recent", 23)] {
        db.insert(hit(key, 60)).await?;
        db.update(Update::table("throttle").key([key]).set("window_start", now() - hours(hours_old))).await?;
    }
    // Events: a week after they were delivered or given up. One still
    // waiting is kept however old.
    db.insert(event("files", "delivered-old").set("delivered_at", now() - days(8))).await?;
    db.insert(event("files", "given-up-old").set("failed_at", now() - days(8))).await?;
    db.insert(event("files", "delivered-recent").set("delivered_at", now() - days(6))).await?;
    db.insert(event("files", "waiting").set("next_attempt_at", now() - days(30))).await?;
    // The log: after as many days as the deployment says.
    db.insert(audit_entry(None, "old", None, json!({})).set("at", now() - days(366))).await?;
    db.insert(audit_entry(None, "recent", None, json!({})).set("at", now() - days(364))).await?;

    // While another process is at it, this one skips the pass.
    let other = Client::connect(db.config().clone()).await?;
    let busy = other.try_lock("app.housekeeping").await?.expect("nobody is housekeeping yet");
    assert_eq!(housekeeping(&db, 365).await?, None);
    assert!(db.get("auth_codes", [digest("code-old")]).await?.is_some());
    busy.unlock().await?;

    // Each delete counts its own table's rows: the token and the code
    // that went with their session are in nobody's count.
    assert_eq!(housekeeping(&db, 365).await?, Some([1, 1, 2, 2, 1, 1, 2, 1]));
    assert!(db.get("auth_codes", [digest("code-old")]).await?.is_none() && db.get("auth_codes", [digest("code-idle")]).await?.is_none());
    assert!(db.get("auth_codes", [digest("code-recent")]).await?.is_some());
    assert!(
        db.get("one_time_tokens", [digest("token-old")]).await?.is_none() && db.get("one_time_tokens", [digest("token-recent")]).await?.is_some()
    );
    assert_eq!(sorted(&db, Query::table("sessions").columns(["id"])).await?, ["ses_live"]);
    assert_eq!(sorted(&db, Query::table("refresh_tokens").columns(["family_id"])).await?, ["ses_c", "ses_d"]);
    assert_eq!(sorted(&db, Query::table("invites").columns(["id"])).await?, ["inv_accepted", "inv_recent"]);
    assert_eq!(sorted(&db, Query::table("throttle").columns(["key"])).await?, ["recent"]);
    assert_eq!(sorted(&db, Query::table("outbox").columns(["token"])).await?, ["delivered-recent", "waiting"]);
    assert_eq!(sorted(&db, Query::table("audit").columns(["action"])).await?, ["recent"]);

    // A second pass finds nothing, and a shorter retention takes the
    // rest of the log.
    assert_eq!(housekeeping(&db, 365).await?, Some([0; 8]));
    assert_eq!(housekeeping(&db, 30).await?, Some([0, 0, 0, 0, 0, 0, 0, 1]));
    Ok(())
}

/// A migration a later release of the service might carry: a column for a
/// table that has rows, a new table with an index and a foreign key, an
/// index for a table that has rows, and a seed row. `broken` adds a
/// statement that fails, as a mistake in the file would.
async fn migration_0003(tx: &Transaction, broken: bool) -> DbResult<()> {
    tx.add_column("users", text("locale").default("en")).await?;
    let notes = TableDef::new("user_notes")
        .column(int("id").auto_increment())
        .column(text("user_id"))
        .column(text("note"))
        .primary_key(["id"])
        .index(IndexDef::new("user_notes_user").on(["user_id"]))
        .foreign_key(["user_id"], "users", OnDelete::Cascade);
    tx.create_table(notes).await?;
    tx.create_index("credentials", IndexDef::new("credentials_service").on(["service"])).await?;
    tx.insert(Insert::into("user_notes").set("user_id", "usr_ann").set("note", "migrated")).await?;
    if broken {
        tx.add_column("no_such_table", int("n").default(0)).await?;
    }
    Ok(())
}

/// One migration as the migrator applies it: its statements and its row in
/// `schema_migrations` are one transaction.
async fn apply_0003(db: &Client, broken: bool) -> DbResult<()> {
    db.transaction(|tx| async move {
        migration_0003(&tx, broken).await?;
        tx.insert(Insert::into("schema_migrations").set("version", "0003_notes")).await?;
        Ok(())
    })
    .await
}

/// The migrator, which every process runs when it starts: under a lock that
/// one process holds at a time, make sure the table of applied versions is
/// there, read it, and apply what is missing. Answers with the versions it
/// applied.
async fn migrate(db: &Client) -> DbResult<Vec<&'static str>> {
    let lock = db.lock("app.migrate").await?;
    db.create_table_if_not_exists(schema_migrations()).await?;
    let applied: Vec<String> = db.query(Query::table("schema_migrations").columns(["version"])).await?.column()?;
    let mut ran = Vec::new();
    if !applied.iter().any(|version| version == "0003_notes") {
        apply_0003(db, false).await?;
        ran.push("0003_notes");
    }
    lock.unlock().await?;
    Ok(ran)
}

// R-S1, R-S8, R-S11, R-X1, R-X2: changes to the schema are transactional:
// they commit with the rows written beside them, or leave no trace.
#[tokio::test(flavor = "multi_thread")]
async fn a_migration_commits_with_its_bookkeeping_row_or_leaves_no_trace() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", None)).await?;
    db.insert(new_credential("key_1", "usr_ann", "api_key", "files", "secret-1", None)).await?;
    for version in ["0001_initial", "0002_services"] {
        db.insert(Insert::into("schema_migrations").set("version", version)).await?;
    }
    let versions = Query::table("schema_migrations").columns(["version"]);
    assert_eq!(db.query(versions.clone()).await?.column::<String>()?, ["0001_initial", "0002_services"]);

    // The migration fails at its last statement. The column, the table,
    // the index and the row its earlier statements made are undone with
    // it, and it is not recorded.
    let err = apply_0003(&db, true).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    assert!(db.describe_table("user_notes").await?.is_none());
    assert!(db.describe_table("users").await?.expect("users is there").column_index("locale").is_none());
    assert!(db.describe_table("credentials").await?.expect("credentials is there").find_index("credentials_service").is_none());
    assert_eq!(db.query(versions.clone()).await?.len(), 2);
    assert_eq!(db.describe().await?.len(), 20);

    // Corrected, it is applied. Until the commit every other connection
    // works with the schema as it was.
    let tx = db.begin().await?;
    migration_0003(&tx, false).await?;
    tx.insert(Insert::into("schema_migrations").set("version", "0003_notes")).await?;
    assert!(tx.describe_table("user_notes").await?.is_some());
    assert!(db.describe_table("user_notes").await?.is_none());
    assert_eq!(db.query(versions.clone()).await?.len(), 2);
    assert_eq!(db.query_one(Query::table("users").key(["usr_ann"])).await?.columns().len(), 11);
    tx.commit().await?;

    // Committed, all of it is there at once.
    assert_eq!(db.query(versions.clone()).await?.column::<String>()?, ["0001_initial", "0002_services", "0003_notes"]);
    let users = db.describe_table("users").await?.expect("users is there");
    assert_eq!(users.columns.last().map(|column| column.name.as_str()), Some("locale"));
    // A row that was there before reads the new column's default.
    assert_eq!(db.get("users", ["usr_ann"]).await?.expect("ann is there").get::<String>("locale")?, "en");
    let note = db.query_one(Query::table("user_notes")).await?;
    assert_eq!(
        (note.get::<i64>("id")?, note.get::<String>("user_id")?, note.get::<String>("note")?),
        (1, "usr_ann".to_string(), "migrated".to_string())
    );
    // The new index holds the rows that were there before it.
    assert_eq!(
        db.query(Query::table("credentials").index("credentials_service", KeyRange::eq(["files"])).columns(["id"])).await?.column::<String>()?,
        ["key_1"]
    );

    // During a rolling deploy a process built before the migration keeps
    // running: it names the columns it knows, and inserts without the
    // new one.
    let ann = db.query_one(Query::table("users").key(["usr_ann"]).columns(USER_COLUMNS)).await?;
    assert_eq!(ann.columns(), USER_COLUMNS);
    db.insert(person("usr_bob", "bob", None)).await?;
    assert_eq!(db.get("users", ["usr_bob"]).await?.expect("bob is there").get::<String>("locale")?, "en");
    db.update(Update::table("users").key(["usr_bob"]).set("locale", "nl").set("updated_at", now())).await?;
    assert_eq!(db.get("users", ["usr_bob"]).await?.expect("bob is there").get::<String>("locale")?, "nl");
    // The new table's foreign key is in force.
    db.delete(Delete::from("users").key(["usr_ann"])).await?;
    assert_eq!(rows_of(&db, Query::table("user_notes")).await?, 0);

    // Applied a second time it fails at its first statement, and again
    // changes nothing.
    let err = apply_0003(&db, false).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::AlreadyExists);
    assert_eq!(db.query(versions).await?.len(), 3);
    assert_eq!(db.describe().await?.len(), 21);
    Ok(())
}

// R-S8, R-X5, R-X8: a named lock that is waited for puts the migrations of
// four processes that start together one after another.
#[tokio::test(flavor = "multi_thread")]
async fn processes_that_start_together_apply_a_migration_once() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", None)).await?;

    // Four processes, each with connections of its own, start at once.
    // The lock lets one migrate; the others wait, then read that there
    // is nothing left to apply. Without it they would each begin the
    // same migration, and all but one would fail.
    let mut starts = Vec::new();
    for _ in 0..4 {
        let process = Client::connect(db.config().clone()).await?;
        starts.push(tokio::spawn(async move { migrate(&process).await }));
    }
    let mut applied = Vec::new();
    for start in starts {
        applied.extend(start.await.expect("the task ran")?);
    }
    assert_eq!(applied, ["0003_notes"]);
    assert_eq!(db.query(Query::table("schema_migrations").columns(["version"])).await?.column::<String>()?, ["0003_notes"]);
    assert_eq!(rows_of(&db, Query::table("user_notes")).await?, 1);

    // A later start finds it applied, and the lock free.
    assert!(migrate(&db).await?.is_empty());
    Ok(())
}

// R-X5, R-X8: named locks, where SQL would take advisory locks: waited for
// or only tried, held across transactions, and freed when the holder's
// connection goes.
#[tokio::test(flavor = "multi_thread")]
async fn a_named_lock_is_waited_for_or_only_tried_and_is_freed_when_its_holder_goes_away() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let other = Client::connect(db.config().clone()).await?;

    // Waited for: a second process asks for a lock that is held, and
    // gets it when the holder lets go, not before.
    let held = db.lock("app.migrate").await?;
    assert_eq!(held.name(), "app.migrate");
    let waiter = {
        let other = other.clone();
        tokio::spawn(async move {
            let lock = other.lock("app.migrate").await?;
            other.insert(audit_entry(None, "second", None, json!({}))).await?;
            lock.unlock().await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!waiter.is_finished(), "the second process waits for the lock");
    // Only tried: the answer is no, at once, even to the process that
    // holds it. A lock of another name is free.
    assert!(db.try_lock("app.migrate").await?.is_none());
    let housekeeping = db.try_lock("app.housekeeping").await?.expect("a lock of another name is free");
    // A named lock is not the write lock: holding one stops nobody from
    // writing.
    db.insert(audit_entry(None, "first", None, json!({}))).await?;
    held.unlock().await?;
    waiter.await.expect("the task ran")?;
    assert_eq!(db.query(Query::table("audit").columns(["action"])).await?.column::<String>()?, ["first", "second"]);

    assert!(other.try_lock("app.housekeeping").await?.is_none());
    housekeeping.unlock().await?;
    let taken = other.try_lock("app.housekeeping").await?.expect("the lock is free once unlocked");

    // Dropped without being unlocked, as when its process dies: the
    // server frees the lock when it sees the connection gone.
    drop(taken);
    let next = tokio::time::timeout(Duration::from_secs(10), db.lock("app.housekeeping")).await.expect("a lock whose holder went away is freed")?;
    next.unlock().await?;

    // The lock belongs to its holder, not to a transaction: it outlasts
    // a commit and a rollback made while it is held.
    let lock = db.lock("app.keys").await?;
    db.transaction(|tx| async move { tx.insert(audit_entry(None, "third", None, json!({}))).await.map(|_| ()) }).await?;
    let tx = db.begin().await?;
    tx.insert(audit_entry(None, "fourth", None, json!({}))).await?;
    tx.rollback().await?;
    assert!(other.try_lock("app.keys").await?.is_none());
    lock.unlock().await?;
    other.try_lock("app.keys").await?.expect("the lock is free once unlocked").unlock().await?;
    assert_eq!(rows_of(&db, Query::table("audit")).await?, 3);
    Ok(())
}

/// Adding a passkey: the challenge is used up, the passkey stored, the
/// change logged.
async fn add_passkey(db: &Client, id: &str, user: &str) -> DbResult<bool> {
    db.transaction(|tx| async move {
        let challenge = Insert::into("throttle").set("key", format!("once:webauthn:{id}")).set("window_start", now()).set("hits", 1);
        if tx.insert(challenge.on_conflict_do_nothing().returning(["key"])).await?.is_empty() {
            return Ok(false);
        }
        let stored = tx.insert(new_passkey(id, user, id.as_bytes(), "laptop")).await?.len() == 1;
        tx.insert(audit_entry(Some(user), "passkey.add", Some(user), json!({"id": id}))).await?;
        Ok(stored)
    })
    .await
}

// R-X4, R-X8: once a commit or a lone statement has returned, every
// connection of the pool and every other process sees what it wrote.
#[tokio::test(flavor = "multi_thread")]
async fn a_commit_is_seen_at_once_on_every_pooled_connection() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", None)).await?;
    // Eight statements at once open every connection the pool may have.
    let mut pings = Vec::new();
    for _ in 0..8 {
        let db = db.clone();
        pings.push(tokio::spawn(async move { db.ping().await }));
    }
    for ping in pings {
        ping.await.expect("the task ran")?;
    }

    // The service answers a request that added a passkey by reading the
    // passkey back, on whichever connection the pool hands it. The commit
    // has returned, so every connection must see the row.
    for round in 0..20 {
        let id = format!("pk_{round:02}");
        assert!(add_passkey(&db, &id, "usr_ann").await?);
        let mut readers = Vec::new();
        for _ in 0..8 {
            let (db, id) = (db.clone(), id.clone());
            readers.push(tokio::spawn(async move {
                let passkey = db.query_opt(Query::table("passkeys").key([id]).columns(["id", "name", "created_at", "last_used_at"])).await?;
                Ok::<_, DbError>(passkey.is_some() && rows_of(&db, Query::table("audit")).await? == round + 1)
            }));
        }
        for reader in readers {
            assert!(reader.await.expect("the task ran")?, "round {round}");
        }
    }

    // The same holds for a statement outside a transaction, when its
    // call returns: an update, and a delete.
    for round in 0..20 {
        let name = format!("Ann {round}");
        db.update(Update::table("users").key(["usr_ann"]).set("name", name.as_str()).set("updated_at", now())).await?;
        db.delete(Delete::from("passkeys").key([format!("pk_{round:02}")])).await?;
        let mut readers = Vec::new();
        for _ in 0..8 {
            let db = db.clone();
            readers.push(tokio::spawn(async move {
                let stored: String = db.query_one(Query::table("users").key(["usr_ann"]).columns(["name"])).await?.get("name")?;
                Ok::<_, DbError>((stored, rows_of(&db, Query::table("passkeys")).await?))
            }));
        }
        for reader in readers {
            assert_eq!(reader.await.expect("the task ran")?, (name.clone(), 19 - round));
        }
    }

    // And for another process, which has connections of its own.
    let other = Client::connect(db.config().clone()).await?;
    db.insert(person("usr_bob", "bob", None)).await?;
    assert!(other.get("users", ["usr_bob"]).await?.is_some());
    other.delete(Delete::from("users").key(["usr_bob"])).await?;
    assert!(db.get("users", ["usr_bob"]).await?.is_none());
    Ok(())
}

// R-Q21, R-X1, R-X2: what a transaction writes is unseen until it commits
// and gone if it does not, and a reading transaction sees one moment.
#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_seen_before_a_commit_and_nothing_is_left_by_a_rollback() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;

    // A transaction sees what it wrote; nobody else does until it
    // commits, and then they see all of it.
    let tx = db.begin().await?;
    tx.insert(person("usr_ann", "ann", None)).await?;
    tx.insert(audit_entry(Some("usr_ann"), "user.create", Some("usr_ann"), json!({}))).await?;
    assert!(tx.get("users", ["usr_ann"]).await?.is_some());
    assert!(db.get("users", ["usr_ann"]).await?.is_none());
    assert_eq!(rows_of(&db, Query::table("audit")).await?, 0);
    tx.commit().await?;
    assert!(db.get("users", ["usr_ann"]).await?.is_some());
    assert_eq!(rows_of(&db, Query::table("audit")).await?, 1);

    // Rolled back, an insert, an update and a delete are all undone.
    let tx = db.begin().await?;
    tx.insert(person("usr_bob", "bob", None)).await?;
    tx.update(Update::table("users").key(["usr_ann"]).set("name", "Changed")).await?;
    assert_eq!(tx.delete(Delete::from("audit")).await?.affected(), 1);
    tx.rollback().await?;
    assert!(db.get("users", ["usr_bob"]).await?.is_none());
    assert_eq!(db.get("users", ["usr_ann"]).await?.expect("ann is there").get::<String>("name")?, "ann");
    assert_eq!(rows_of(&db, Query::table("audit")).await?, 1);

    // Dropped without a word, as when a request is cancelled or its
    // handler panics: rolled back. The next writer gets the write lock
    // once the server has seen the connection go, and finds nothing of
    // it.
    let tx = db.begin().await?;
    tx.insert(person("usr_cy", "cy", None)).await?;
    drop(tx);
    db.insert(person("usr_dee", "dee", None)).await?;
    assert!(db.get("users", ["usr_cy"]).await?.is_none());
    assert_eq!(rows_of(&db, Query::table("users")).await?, 2);

    // A transaction that has ended takes no more statements.
    let tx = db.begin().await?;
    let handle = tx.clone();
    tx.commit().await?;
    assert_eq!(handle.query(Query::table("users")).await.unwrap_err().code, ErrorCode::TransactionAborted);
    assert_eq!(handle.commit().await.unwrap_err().code, ErrorCode::TransactionAborted);

    // A reading transaction sees one moment, whatever is committed
    // meanwhile, and holds up no writer. The registry is loaded so:
    // the revision and the rows it counts must belong together.
    db.insert(new_service("files", "invite")).await?;
    let revision = Query::table("services_revision").columns(["revision"]);
    let snapshot = db.begin_read().await?;
    assert_eq!(snapshot.query_one(revision.clone()).await?.get::<i64>("revision")?, 0);
    db.transaction(|tx| async move {
        tx.update(bump_revision()).await?;
        tx.insert(new_service("chat", "invite")).await?;
        Ok::<_, DbError>(())
    })
    .await?;
    assert_eq!(rows_of(&snapshot, Query::table("services")).await?, 1);
    assert_eq!(snapshot.query_one(revision.clone()).await?.get::<i64>("revision")?, 0);
    assert_eq!(snapshot.query(registry()).await?.len(), 1);
    snapshot.rollback().await?;

    let loaded = db
        .read(|tx| async move {
            let revision: i64 = tx.query_one(revision).await?.get("revision")?;
            let names: Vec<String> = tx.query(Query::table("services").columns(["name"])).await?.column()?;
            Ok::<_, DbError>((revision, names))
        })
        .await?;
    assert_eq!(loaded, (1, vec!["chat".to_string(), "files".to_string()]));
    Ok(())
}

// R-O3, R-Q8, R-X7: a health check, and the server's clock: one time for a
// whole transaction, a time of its own for each lone statement.
#[tokio::test(flavor = "multi_thread")]
async fn the_clock_is_the_servers_and_stands_still_inside_a_transaction() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(person("usr_ann", "ann", None)).await?;

    // The health check.
    db.ping().await?;

    // Every statement of a transaction reads the time it began at, in
    // what it stores, in what it compares and in what it returns, though
    // the wall clock moves on.
    let before = db.now().await?;
    let tx = db.begin().await?;
    let began = tx.now().await?;
    tx.insert(one_time_token("reset-1", "reset", "usr_ann", None, 60)).await?;
    tokio::time::sleep(Duration::from_millis(30)).await;
    tx.insert(audit_entry(Some("usr_ann"), "reset.request", Some("usr_ann"), json!({}))).await?;
    tx.update(Update::table("users").key(["usr_ann"]).set("updated_at", now())).await?;
    assert_eq!(tx.now().await?, began);
    assert_eq!(tx.query_one(Query::table("services_revision").select("at", now())).await?.get::<Timestamp>("at")?, began);
    assert_eq!(rows_of(&tx, Query::table("audit").filter(col("at").eq(now()))).await?, 1);
    tx.commit().await?;
    let after = db.now().await?;
    assert!(before <= began && began < after);
    assert_eq!(
        db.get("one_time_tokens", [digest("reset-1")]).await?.expect("the token is there").get::<Timestamp>("expires_at")?,
        began.plus_secs(60)
    );
    assert_eq!(db.query_one(Query::table("audit")).await?.get::<Timestamp>("at")?, began);
    assert_eq!(db.get("users", ["usr_ann"]).await?.expect("ann is there").get::<Timestamp>("updated_at")?, began);

    // Outside a transaction each statement has a time of its own, which
    // never runs backwards.
    let first: Timestamp = db.insert(audit_entry(None, "first", None, json!({})).returning(["at"])).await?.one()?.get("at")?;
    tokio::time::sleep(Duration::from_millis(5)).await;
    let second: Timestamp = db.insert(audit_entry(None, "second", None, json!({})).returning(["at"])).await?.one()?.get("at")?;
    assert!(after <= first && first < second && second <= db.now().await?);

    // One statement has one time for all the rows it writes.
    assert_eq!(db.update(Update::table("audit").set("at", now())).await?.affected(), 3);
    let times = column::<Timestamp>(&db.query(Query::table("audit").columns(["at"])).await?, "at");
    assert!(times.iter().all(|at| *at == times[0] && *at >= second));
    Ok(())
}
