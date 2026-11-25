//! A scenario suite for a document workspace: spaces, a tree of pages,
//! typed tables ("bases") kept as JSON cells, comments, public shares,
//! notifications and search. Every shape of statement such a workspace
//! needs is said here through the client, against a full set of tables,
//! and what it returns is pinned.
//!
//! Three things a SQL database does for such an application are the
//! caller's own work here, and so this file's: the SQL functions over a
//! cell are Rust functions that return an `Expr`; the filter and sort of a
//! base are compiled to an `Expr` and a list of `Order`s; and where one
//! statement would lean on data-modifying CTEs or a row lock, a writing
//! transaction groups the statements instead.

mod common;
use common::*;

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::time::Duration;

use serde_json::json;

// ------------------------------------------------------------------ schema

fn id_col() -> ColumnDef {
    ColumnDef::new("id", Type::Uuid).default_random_uuid()
}

fn uuid_col(name: &str) -> ColumnDef {
    ColumnDef::new(name, Type::Uuid)
}

fn text_col(name: &str) -> ColumnDef {
    ColumnDef::new(name, Type::Text)
}

fn time_col(name: &str) -> ColumnDef {
    ColumnDef::new(name, Type::Timestamp)
}

fn flag_col(name: &str) -> ColumnDef {
    ColumnDef::new(name, Type::Bool).default(false)
}

/// A JSON column that starts as an empty object.
fn object_col(name: &str) -> ColumnDef {
    ColumnDef::new(name, Type::Json).default(json!({}))
}

/// Postgres' `uuid[] NOT NULL DEFAULT '{}'`: ids with no foreign key.
fn uuid_list_col(name: &str) -> ColumnDef {
    ColumnDef::new(name, Type::list(Type::Uuid)).default(Vec::<Value>::new())
}

fn created_at() -> ColumnDef {
    time_col("created_at").default_now()
}

fn updated_at() -> ColumnDef {
    time_col("updated_at").default_now()
}

/// `CHECK ((a IS NULL) <> (b IS NULL))`: exactly one of the two is set.
fn exactly_one(a: &str, b: &str) -> Expr {
    col(a).is_null().ne(col(b).is_null())
}

fn schema_migrations() -> TableDef {
    TableDef::new("schema_migrations").column(text_col("name")).column(time_col("applied_at").default_now()).primary_key(["name"])
}

/// The workspace's 26 tables, parents before children so every foreign
/// key finds its table. Indexes behind a constraint carry the names
/// Postgres gives them, which error handling matches on; the others are
/// the access paths a planner would pick and the queries here state.
fn tables() -> Vec<TableDef> {
    let users = TableDef::new("users")
        .column(id_col())
        .column(text_col("email"))
        .column(text_col("name").default(""))
        .column(text_col("password_hash").nullable())
        .column(text_col("avatar").nullable())
        .column(text_col("role").default("member"))
        .column(text_col("locale").nullable())
        .column(text_col("timezone").nullable())
        .column(object_col("preferences"))
        .column(uuid_col("invited_by").nullable())
        .column(time_col("last_login_at").nullable())
        .column(time_col("deactivated_at").nullable())
        .column(created_at())
        .column(updated_at())
        .column(text_col("auth_sub").nullable())
        .column(flag_col("email_from_provider"))
        .primary_key(["id"])
        .index(IndexDef::new("users_email_key").on_expr(col("email").lower()).unique())
        .unique("users_auth_sub_key", ["auth_sub"])
        .foreign_key(["invited_by"], "users", OnDelete::SetNull)
        .check("users_role_check", col("role").in_list(vec!["owner", "admin", "member"]));

    let sessions = TableDef::new("sessions")
        .column(id_col())
        .column(uuid_col("user_id"))
        .column(text_col("token_hash"))
        .column(text_col("user_agent").nullable())
        .column(text_col("ip").nullable())
        .column(created_at())
        .column(time_col("last_seen_at").default_now())
        .column(time_col("expires_at"))
        .column(text_col("auth_sid").nullable())
        .column(text_col("auth_refresh_token").nullable())
        .column(time_col("auth_recheck_at").nullable())
        .primary_key(["id"])
        .unique("sessions_token_hash_key", ["token_hash"])
        .index(IndexDef::new("sessions_user_idx").on(["user_id"]))
        .index(IndexDef::new("sessions_auth_sid_idx").on(["auth_sid"]).filter(col("auth_sid").is_not_null()))
        .foreign_key(["user_id"], "users", OnDelete::Cascade);

    let user_tokens = TableDef::new("user_tokens")
        .column(id_col())
        .column(uuid_col("user_id"))
        .column(text_col("purpose"))
        .column(text_col("token_hash"))
        .column(time_col("expires_at"))
        .column(created_at())
        .primary_key(["id"])
        .unique("user_tokens_token_hash_key", ["token_hash"])
        .index(IndexDef::new("user_tokens_user_idx").on(["user_id"]))
        .foreign_key(["user_id"], "users", OnDelete::Cascade);

    let groups = TableDef::new("groups")
        .column(id_col())
        .column(text_col("name"))
        .column(text_col("description").nullable())
        .column(flag_col("is_default"))
        .column(uuid_col("created_by").nullable())
        .column(created_at())
        .column(updated_at())
        .primary_key(["id"])
        .index(IndexDef::new("groups_name_key").on_expr(col("name").lower()).unique())
        .foreign_key(["created_by"], "users", OnDelete::SetNull);

    let group_members = TableDef::new("group_members")
        .column(uuid_col("group_id"))
        .column(uuid_col("user_id"))
        .column(created_at())
        .primary_key(["group_id", "user_id"])
        .index(IndexDef::new("group_members_user_idx").on(["user_id"]))
        .foreign_key(["group_id"], "groups", OnDelete::Cascade)
        .foreign_key(["user_id"], "users", OnDelete::Cascade);

    let invitations = TableDef::new("invitations")
        .column(id_col())
        .column(text_col("email"))
        .column(text_col("role").default("member"))
        .column(uuid_list_col("group_ids"))
        .column(text_col("token_hash"))
        .column(uuid_col("invited_by").nullable())
        .column(created_at())
        .primary_key(["id"])
        .index(IndexDef::new("invitations_email_key").on_expr(col("email").lower()).unique())
        .foreign_key(["invited_by"], "users", OnDelete::SetNull)
        .check("invitations_role_check", col("role").in_list(vec!["admin", "member"]));

    let spaces = TableDef::new("spaces")
        .column(id_col())
        .column(text_col("name"))
        .column(text_col("slug"))
        .column(text_col("description").nullable())
        .column(text_col("icon").nullable())
        .column(object_col("settings"))
        .column(uuid_col("created_by").nullable())
        .column(created_at())
        .column(updated_at())
        .primary_key(["id"])
        .index(IndexDef::new("spaces_slug_key").on_expr(col("slug").lower()).unique())
        .foreign_key(["created_by"], "users", OnDelete::SetNull);

    let space_members = TableDef::new("space_members")
        .column(id_col())
        .column(uuid_col("space_id"))
        .column(uuid_col("user_id").nullable())
        .column(uuid_col("group_id").nullable())
        .column(text_col("role"))
        .column(uuid_col("added_by").nullable())
        .column(created_at())
        .primary_key(["id"])
        .index(IndexDef::new("space_members_user_key").on(["space_id", "user_id"]).unique().filter(col("user_id").is_not_null()))
        .index(IndexDef::new("space_members_group_key").on(["space_id", "group_id"]).unique().filter(col("group_id").is_not_null()))
        .index(IndexDef::new("space_members_user_idx").on(["user_id"]))
        .index(IndexDef::new("space_members_group_idx").on(["group_id"]))
        // Every grant of a space, users' and groups' alike: the role
        // subquery and the member count read this range.
        .index(IndexDef::new("space_members_space_idx").on(["space_id"]))
        .foreign_key(["space_id"], "spaces", OnDelete::Cascade)
        .foreign_key(["user_id"], "users", OnDelete::Cascade)
        .foreign_key(["group_id"], "groups", OnDelete::Cascade)
        .foreign_key(["added_by"], "users", OnDelete::SetNull)
        .check("space_members_role_check", col("role").in_list(vec!["admin", "writer", "reader"]))
        .check("space_members_check", exactly_one("user_id", "group_id"));

    let workspace = TableDef::new("workspace")
        .column(ColumnDef::new("id", Type::Int).default(1))
        .column(text_col("name").default("My workspace"))
        .column(text_col("description").nullable())
        .column(text_col("logo").nullable())
        .column(ColumnDef::new("email_domains", Type::list(Type::Text)).default(Vec::<Value>::new()))
        .column(object_col("settings"))
        .column(uuid_col("default_space_id").nullable())
        .column(created_at())
        .column(updated_at())
        .primary_key(["id"])
        .foreign_key(["default_space_id"], "spaces", OnDelete::SetNull)
        .check("workspace_id_check", col("id").eq(1));

    let pages = TableDef::new("pages")
        .column(id_col())
        .column(text_col("slug_id"))
        .column(uuid_col("space_id"))
        .column(uuid_col("parent_id").nullable())
        .column(text_col("position"))
        .column(text_col("title").nullable())
        .column(text_col("icon").nullable())
        .column(text_col("cover").nullable())
        .column(ColumnDef::new("content", Type::Json).nullable())
        .column(text_col("text_content").nullable())
        .column(ColumnDef::new("doc_version", Type::Int).default(0))
        .column(uuid_col("creator_id").nullable())
        .column(uuid_col("last_editor_id").nullable())
        .column(uuid_list_col("contributor_ids"))
        .column(created_at())
        .column(updated_at())
        .column(time_col("deleted_at").nullable())
        .column(uuid_col("deleted_by").nullable())
        .column(flag_col("is_base"))
        .primary_key(["id"])
        .unique("pages_slug_id_key", ["slug_id"])
        // Siblings in order: text sorts by its bytes, which is what the
        // fractional position keys need (Postgres: COLLATE "C").
        .index(IndexDef::new("pages_tree_idx").on(["space_id", "parent_id", "position", "id"]))
        // First key `parent_id`: what a walk down the tree reads, giving
        // each page's children in sibling order.
        .index(IndexDef::new("pages_parent_idx").on(["parent_id", "position", "id"]))
        // Postgres' stored `tsvector` column: title weight A (1.0), text
        // weight B (0.4), no stemming. No key prefix: one search covers
        // every space the caller can read, and the scope is a filter.
        .index(
            IndexDef::new("pages_search_idx")
                .full_text(TextConfig::Simple, [(col("title").or_else(""), 1.0), (col("text_content").or_else("").left(1_000_000), 0.4)]),
        )
        .index(IndexDef::new("pages_updated_idx").on_desc("updated_at").on(["id"]).filter(col("deleted_at").is_null()))
        .index(IndexDef::new("pages_deleted_idx").on(["space_id", "deleted_at", "id"]).filter(col("deleted_at").is_not_null()))
        .foreign_key(["space_id"], "spaces", OnDelete::Cascade)
        .foreign_key(["parent_id"], "pages", OnDelete::Cascade)
        .foreign_key(["creator_id"], "users", OnDelete::SetNull)
        .foreign_key(["last_editor_id"], "users", OnDelete::SetNull)
        .foreign_key(["deleted_by"], "users", OnDelete::SetNull);

    let page_versions = TableDef::new("page_versions")
        .column(id_col())
        .column(uuid_col("page_id"))
        .column(text_col("title").nullable())
        .column(text_col("icon").nullable())
        .column(ColumnDef::new("content", Type::Json).nullable())
        .column(uuid_col("editor_id").nullable())
        .column(uuid_list_col("contributor_ids"))
        .column(created_at())
        .primary_key(["id"])
        .index(IndexDef::new("page_versions_page_idx").on(["page_id"]).on_desc("created_at").on(["id"]))
        .foreign_key(["page_id"], "pages", OnDelete::Cascade)
        .foreign_key(["editor_id"], "users", OnDelete::SetNull);

    let comments = TableDef::new("comments")
        .column(id_col())
        .column(uuid_col("page_id"))
        .column(uuid_col("parent_id").nullable())
        .column(uuid_col("author_id").nullable())
        .column(ColumnDef::new("body", Type::Json))
        .column(text_col("quote").nullable())
        .column(flag_col("anchored"))
        .column(created_at())
        .column(updated_at())
        .column(time_col("edited_at").nullable())
        .column(time_col("resolved_at").nullable())
        .column(uuid_col("resolved_by").nullable())
        .primary_key(["id"])
        .index(IndexDef::new("comments_page_idx").on(["page_id", "created_at", "id"]))
        .index(IndexDef::new("comments_parent_idx").on(["parent_id"]))
        .foreign_key(["page_id"], "pages", OnDelete::Cascade)
        .foreign_key(["parent_id"], "comments", OnDelete::Cascade)
        .foreign_key(["author_id"], "users", OnDelete::SetNull)
        .foreign_key(["resolved_by"], "users", OnDelete::SetNull);

    let files = TableDef::new("files")
        .column(id_col())
        .column(text_col("purpose").default("page"))
        .column(uuid_col("page_id").nullable())
        .column(uuid_col("space_id").nullable())
        .column(uuid_col("uploader_id").nullable())
        .column(text_col("name"))
        .column(text_col("mime").default("application/octet-stream"))
        .column(ColumnDef::new("size", Type::Int).default(0))
        .column(text_col("storage_key"))
        .column(created_at())
        .column(updated_at())
        .primary_key(["id"])
        .index(IndexDef::new("files_page_idx").on(["page_id"]))
        .foreign_key(["page_id"], "pages", OnDelete::SetNull)
        .foreign_key(["space_id"], "spaces", OnDelete::SetNull)
        .foreign_key(["uploader_id"], "users", OnDelete::SetNull)
        .check("files_purpose_check", col("purpose").in_list(vec!["page", "avatar", "workspace-logo", "space-icon"]));

    let shares = TableDef::new("shares")
        .column(id_col())
        .column(text_col("key"))
        .column(uuid_col("page_id"))
        .column(flag_col("include_children"))
        .column(flag_col("search_indexing"))
        .column(uuid_col("created_by").nullable())
        .column(created_at())
        .column(updated_at())
        .primary_key(["id"])
        .unique("shares_key_key", ["key"])
        .unique("shares_page_id_key", ["page_id"])
        .foreign_key(["page_id"], "pages", OnDelete::Cascade)
        .foreign_key(["created_by"], "users", OnDelete::SetNull);

    // Favourites and watches have one shape: a user and either a page or
    // a space, each pair at most once.
    let marks = |table: &str| {
        TableDef::new(table)
            .column(id_col())
            .column(uuid_col("user_id"))
            .column(uuid_col("page_id").nullable())
            .column(uuid_col("space_id").nullable())
            .column(created_at())
            .primary_key(["id"])
            .index(IndexDef::new(&format!("{table}_page_key")).on(["user_id", "page_id"]).unique().filter(col("page_id").is_not_null()))
            .index(IndexDef::new(&format!("{table}_space_key")).on(["user_id", "space_id"]).unique().filter(col("space_id").is_not_null()))
            .foreign_key(["user_id"], "users", OnDelete::Cascade)
            .foreign_key(["page_id"], "pages", OnDelete::Cascade)
            .foreign_key(["space_id"], "spaces", OnDelete::Cascade)
            .check(&format!("{table}_check"), exactly_one("page_id", "space_id"))
    };
    let favorites = marks("favorites");
    let watchers =
        marks("watchers").index(IndexDef::new("watchers_page_idx").on(["page_id"])).index(IndexDef::new("watchers_space_idx").on(["space_id"]));

    let labels = TableDef::new("labels")
        .column(id_col())
        .column(text_col("name"))
        .column(created_at())
        .primary_key(["id"])
        .unique("labels_name_key", ["name"]);

    let page_labels = TableDef::new("page_labels")
        .column(uuid_col("page_id"))
        .column(uuid_col("label_id"))
        .column(created_at())
        .primary_key(["page_id", "label_id"])
        .index(IndexDef::new("page_labels_label_idx").on(["label_id"]))
        .foreign_key(["page_id"], "pages", OnDelete::Cascade)
        .foreign_key(["label_id"], "labels", OnDelete::Cascade);

    let page_links = TableDef::new("page_links")
        .column(uuid_col("source_id"))
        .column(uuid_col("target_id"))
        .primary_key(["source_id", "target_id"])
        .index(IndexDef::new("page_links_target_idx").on(["target_id"]))
        .foreign_key(["source_id"], "pages", OnDelete::Cascade)
        .foreign_key(["target_id"], "pages", OnDelete::Cascade);

    let synced_blocks = TableDef::new("synced_blocks")
        .column(uuid_col("page_id"))
        .column(text_col("block_id"))
        .column(ColumnDef::new("content", Type::Json))
        .column(updated_at())
        .primary_key(["page_id", "block_id"])
        .index(IndexDef::new("synced_blocks_block_idx").on(["block_id"]))
        .foreign_key(["page_id"], "pages", OnDelete::Cascade);

    let notifications = TableDef::new("notifications")
        .column(id_col())
        .column(uuid_col("user_id"))
        .column(text_col("kind"))
        .column(uuid_col("actor_id").nullable())
        .column(uuid_col("page_id").nullable())
        .column(uuid_col("comment_id").nullable())
        .column(object_col("data"))
        .column(created_at())
        .column(time_col("read_at").nullable())
        .column(time_col("emailed_at").nullable())
        .primary_key(["id"])
        // The inbox's order, newest first, with the id to make it total.
        .index(IndexDef::new("notifications_user_idx").on(["user_id"]).on_desc("created_at").on(["id"]))
        .foreign_key(["user_id"], "users", OnDelete::Cascade)
        .foreign_key(["actor_id"], "users", OnDelete::SetNull)
        .foreign_key(["page_id"], "pages", OnDelete::Cascade)
        .foreign_key(["comment_id"], "comments", OnDelete::Cascade);

    let base_properties = TableDef::new("base_properties")
        .column(uuid_col("page_id"))
        .column(text_col("id"))
        .column(text_col("name"))
        .column(text_col("type"))
        .column(object_col("options"))
        .column(flag_col("is_primary"))
        .column(text_col("position"))
        .column(created_at())
        .column(updated_at())
        .primary_key(["page_id", "id"])
        .index(IndexDef::new("base_properties_name_key").on(["page_id"]).on_expr(col("name").trim().lower()).unique())
        .index(IndexDef::new("base_properties_order_idx").on(["page_id", "position", "id"]))
        .foreign_key(["page_id"], "pages", OnDelete::Cascade);

    let base_rows = TableDef::new("base_rows")
        .column(id_col())
        .column(uuid_col("page_id"))
        .column(object_col("cells"))
        .column(text_col("position"))
        .column(uuid_col("creator_id").nullable())
        .column(uuid_col("last_editor_id").nullable())
        .column(created_at())
        .column(updated_at())
        .primary_key(["id"])
        .index(IndexDef::new("base_rows_order_idx").on(["page_id", "position", "id"]))
        .foreign_key(["page_id"], "pages", OnDelete::Cascade)
        .foreign_key(["creator_id"], "users", OnDelete::SetNull)
        .foreign_key(["last_editor_id"], "users", OnDelete::SetNull);

    let base_views = TableDef::new("base_views")
        .column(id_col())
        .column(uuid_col("page_id"))
        .column(text_col("name"))
        .column(text_col("type"))
        .column(object_col("config"))
        .column(text_col("position"))
        .column(uuid_col("creator_id").nullable())
        .column(created_at())
        .column(updated_at())
        .primary_key(["id"])
        .index(IndexDef::new("base_views_page_idx").on(["page_id", "position", "id"]))
        .foreign_key(["page_id"], "pages", OnDelete::Cascade)
        .foreign_key(["creator_id"], "users", OnDelete::SetNull)
        .check("base_views_type_check", col("type").in_list(vec!["table", "kanban", "calendar"]));

    let realtime_leader = TableDef::new("realtime_leader")
        .column(ColumnDef::new("id", Type::Int).default(1))
        .column(text_col("holder"))
        .column(time_col("expires_at"))
        .primary_key(["id"])
        .check("realtime_leader_id_check", col("id").eq(1));

    vec![
        users,
        sessions,
        user_tokens,
        groups,
        group_members,
        invitations,
        spaces,
        space_members,
        workspace,
        pages,
        page_versions,
        comments,
        files,
        shares,
        favorites,
        watchers,
        labels,
        page_labels,
        page_links,
        synced_blocks,
        notifications,
        base_properties,
        base_rows,
        base_views,
        realtime_leader,
        schema_migrations(),
    ]
}

/// Creates the workspace's tables. Asking again changes nothing, as at
/// every start of an application.
async fn schema(db: &Client) -> DbResult<()> {
    for table in tables() {
        db.create_table_if_not_exists(table).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------- fixtures

/// A list of ids as one value: an `in_list` operand, or a list column.
fn uuids(ids: &[Uuid]) -> Value {
    Value::List(ids.iter().map(|id| Value::Uuid(id.0)).collect())
}

fn sorted<T: Ord>(mut items: Vec<T>) -> Vec<T> {
    items.sort();
    items
}

fn ids(rows: &Rows) -> Vec<Uuid> {
    column(rows, "id")
}

fn titles(rows: &Rows) -> Vec<String> {
    column(rows, "title")
}

/// The ids of rows whose ids a test chose as `uuid(n)`, as those numbers.
fn numbers(rows: &Rows) -> Vec<u128> {
    ids(rows).into_iter().map(|id| u128::from_be_bytes(id.0)).collect()
}

/// How many rows a query finds.
async fn rows_in(db: &impl Db, query: Query) -> DbResult<i64> {
    db.query_one(query.select("n", count())).await?.get("n")
}

/// Runs two pieces of work at the same time, each on a task of its own.
async fn race<T, A, B>(a: A, b: B) -> (T, T)
where
    T: Send + 'static,
    A: Future<Output = T> + Send + 'static,
    B: Future<Output = T> + Send + 'static,
{
    let (a, b) = (tokio::spawn(a), tokio::spawn(b));
    (a.await.expect("the task ran"), b.await.expect("the task ran"))
}

/// A user called `name`, reachable at `name@example.com`.
async fn new_user(db: &impl Db, name: &str) -> DbResult<Uuid> {
    db.insert(Insert::into("users").set("email", format!("{name}@example.com")).set("name", name).returning(["id"])).await?.one()?.get("id")
}

async fn new_group(db: &impl Db, name: &str) -> DbResult<Uuid> {
    db.insert(Insert::into("groups").set("name", name).returning(["id"])).await?.one()?.get("id")
}

async fn join_group(db: &impl Db, group: Uuid, user: Uuid) -> DbResult<()> {
    db.insert(Insert::into("group_members").set("group_id", group).set("user_id", user)).await?;
    Ok(())
}

async fn new_space(db: &impl Db, slug: &str) -> DbResult<Uuid> {
    db.insert(Insert::into("spaces").set("name", slug).set("slug", slug).returning(["id"])).await?.one()?.get("id")
}

/// Grants a user a role in a space; returns the membership's id.
async fn grant(db: &impl Db, space: Uuid, user: Uuid, role: &str) -> DbResult<Uuid> {
    db.insert(Insert::into("space_members").set("space_id", space).set("user_id", user).set("role", role).returning(["id"])).await?.one()?.get("id")
}

async fn grant_group(db: &impl Db, space: Uuid, group: Uuid, role: &str) -> DbResult<Uuid> {
    db.insert(Insert::into("space_members").set("space_id", space).set("group_id", group).set("role", role).returning(["id"])).await?.one()?.get("id")
}

/// A page's ten-character public id. An application makes these itself;
/// here a counter.
fn slug_id() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    format!("pg{:08}", NEXT.fetch_add(1, Relaxed))
}

/// A page under `parent` (a root without one) at a sibling position. A
/// test adds what it cares about: an id of its own, a document, a date.
fn page_insert(space: Uuid, parent: Option<Uuid>, position: &str, title: &str) -> Insert {
    page_with_slug(&slug_id(), space, parent, position, title)
}

fn page_with_slug(slug: &str, space: Uuid, parent: Option<Uuid>, position: &str, title: &str) -> Insert {
    Insert::into("pages").set("slug_id", slug).set("space_id", space).set("parent_id", parent).set("position", position).set("title", title)
}

async fn new_page(db: &impl Db, space: Uuid, parent: Option<Uuid>, position: &str, title: &str) -> DbResult<Uuid> {
    db.insert(page_insert(space, parent, position, title).returning(["id"])).await?.one()?.get("id")
}

async fn new_share(db: &impl Db, page: Uuid, key: &str, include_children: bool) -> DbResult<Uuid> {
    db.insert(Insert::into("shares").set("key", key).set("page_id", page).set("include_children", include_children).returning(["id"]))
        .await?
        .one()?
        .get("id")
}

async fn new_file(db: &impl Db, page: Uuid, space: Uuid, storage_key: &str) -> DbResult<Uuid> {
    let file = Insert::into("files").set("page_id", page).set("space_id", space).set("name", storage_key).set("storage_key", storage_key);
    db.insert(file.returning(["id"])).await?.one()?.get("id")
}

struct Wiki {
    ann: Uuid,
    bob: Uuid,
    space: Uuid,
}

/// Two users and a space, `docs`, in which ann is an admin. Bob has no
/// grant until a test gives him one.
async fn wiki(db: &impl Db) -> DbResult<Wiki> {
    let ann = new_user(db, "ann").await?;
    let bob = new_user(db, "bob").await?;
    let space = new_space(db, "docs").await?;
    grant(db, space, ann, "admin").await?;
    Ok(Wiki { ann, bob, space })
}

// -------------------------------------------------------- shared fragments

/// Whether the grant `sm` reaches the user: it names them, or a group
/// they are in.
fn grant_reaches(user: Expr) -> Expr {
    let groups = Query::table("group_members")
        .alias("gm")
        .index("group_members_user_idx", KeyRange::eq([user.clone()]))
        .select("group_id", col("gm.group_id"));
    col("sm.user_id").eq(user).or(col("sm.group_id").in_list(list_of(groups)))
}

/// The user's highest role in a space over direct grants and grants
/// to their groups: 3 admin, 2 writer, 1 reader, NULL for none.
fn role_rank(space: Expr, user: Expr) -> Expr {
    let rank = case([(col("sm.role").eq("admin"), val(3)), (col("sm.role").eq("writer"), val(2))], 1);
    scalar(
        Query::table("space_members")
            .alias("sm")
            .index("space_members_space_idx", KeyRange::eq([space]))
            .filter(grant_reaches(user))
            .select("rank", max(rank)),
    )
}

/// The ids of the spaces a user can read, as a list. Postgres finds the
/// index for each half of the `OR`; here each half is its own read.
fn readable_spaces(user: Expr) -> Expr {
    let direct = Query::table("space_members")
        .alias("sm")
        .index("space_members_user_idx", KeyRange::eq([user.clone()]))
        .select("space_id", col("sm.space_id"));
    let through_groups = Query::table("group_members")
        .alias("gm")
        .index("group_members_user_idx", KeyRange::eq([user]))
        .join(Query::table("space_members").alias("sm").index("space_members_group_idx", KeyRange::eq([col("gm.group_id")])))
        .select("space_id", col("sm.space_id"));
    list_of(direct).list_concat(list_of(through_groups))
}

/// A member of a JSON object that may be NULL. `json_build_object` writes
/// a JSON null for a NULL argument; said outright, the object is the same
/// whether or not `Func::JsonObject` does.
fn json_or_null(value: Expr) -> Expr {
    coalesce([call(Func::ToJson, [value]), val(Json::Null)])
}

/// A JSON object of named members: Postgres' `json_build_object`.
fn object<const N: usize>(members: [(&str, Expr); N]) -> Expr {
    call(Func::JsonObject, members.into_iter().flat_map(|(name, value)| [val(name), value]))
}

/// The URL an uploaded image is served from, NULL without an image.
fn image_url(file: Expr) -> Expr {
    concat([val("/api/images/"), file])
}

/// A page's space as a small object, for a query that joined `spaces`
/// as `s`.
fn space_ref() -> Expr {
    object([("id", col("s.id")), ("name", col("s.name")), ("slug", col("s.slug")), ("iconUrl", json_or_null(image_url(col("s.icon"))))])
}

/// A user as a small object, NULL when the left join found nobody.
fn user_ref(alias: &str) -> Expr {
    let of = |name: &str| col(&format!("{alias}.{name}"));
    let person = object([("id", of("id")), ("name", of("name")), ("avatarUrl", json_or_null(image_url(of("avatar"))))]);
    when(of("id").is_null(), null(), person)
}

/// Sharing is on unless the workspace or the space turned it off.
fn sharing_enabled(space_settings: Expr) -> Expr {
    let disabled = |settings: Expr| settings.json_text(["sharingDisabled"]).cast(Type::Bool).or_else(false);
    let workspace = scalar(Query::table("workspace").key([1]).select("settings", col("settings")));
    disabled(workspace).not().and(disabled(space_settings).not())
}

/// Whether the page `p` has a child that is not trashed.
fn has_children() -> Expr {
    exists(Query::table("pages").alias("c").index("pages_parent_idx", KeyRange::eq([col("p.id")])).filter(col("c.deleted_at").is_null()))
}

/// The columns of a tree item, for a query that reads `pages` as `p`.
fn tree_columns(pages: Query) -> Query {
    pages
        .columns(["p.id", "p.slug_id", "p.title", "p.icon", "p.parent_id", "p.space_id", "p.position", "p.is_base"])
        .select("has_children", has_children())
}

/// A page without its document.
fn page_columns(pages: Query) -> Query {
    pages
        .columns(["p.id", "p.slug_id", "p.space_id", "p.parent_id", "p.position", "p.title", "p.icon", "p.cover", "p.doc_version"])
        .columns([
            "p.creator_id", "p.last_editor_id", "p.contributor_ids", "p.created_at", "p.updated_at", "p.deleted_at", "p.deleted_by", "p.is_base",
        ])
        .select("has_children", has_children())
}

/// The scope of every cross-space listing: live pages in spaces the user
/// can read.
fn readable(user: Uuid) -> Expr {
    col("p.deleted_at").is_null().and(col("p.space_id").in_list(readable_spaces(val(user))))
}

// ------------------------------------------------ schema and migrations

// R-S1, R-S2, R-S3, R-S4, R-S6, R-S7: the whole schema, declared twice and read back.
#[tokio::test(flavor = "multi_thread")]
async fn the_schema_holds_every_table_index_and_foreign_key() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    // The schema is declared at every start, on a database that has it all.
    schema(&db).await?;

    let tables = db.describe().await?;
    assert_eq!(tables.len(), 26);
    let table = |name: &str| tables.iter().find(|t| t.name == name).unwrap_or_else(|| panic!("no table {name}"));
    let index = |name: &str, index: &str| table(name).find_index(index).unwrap_or_else(|| panic!("no index {index}")).clone();

    assert_eq!(table("group_members").primary_key, ["group_id", "user_id"]);
    assert_eq!(table("base_properties").primary_key, ["page_id", "id"]);

    // Unique on an expression, and unique on part of the table.
    let email = index("users", "users_email_key");
    assert!(email.unique);
    assert_eq!(email.keys[0].expr, col("email").lower());
    assert_eq!(index("base_properties", "base_properties_name_key").keys[1].expr, col("name").trim().lower());
    let member = index("space_members", "space_members_user_key");
    assert!(member.unique);
    assert_eq!(member.filter, Some(col("user_id").is_not_null()));

    // Ordered, one key descending, over the live pages only.
    let updated = index("pages", "pages_updated_idx");
    assert!(updated.keys[0].desc && !updated.unique);
    assert_eq!(updated.filter, Some(col("deleted_at").is_null()));

    match index("pages", "pages_search_idx").kind {
        IndexKind::FullText { config, fields } => {
            assert_eq!(config, TextConfig::Simple);
            assert_eq!(fields.iter().map(|f| f.weight).collect::<Vec<_>>(), [1.0, 0.4]);
        }
        IndexKind::Ordered => panic!("the search index is a full-text one"),
    }

    // 50 foreign keys, none that refuses a delete, two of them pointing
    // at their own table.
    let acting = |action: OnDelete| tables.iter().flat_map(|t| &t.foreign_keys).filter(|fk| fk.on_delete == action).count();
    assert_eq!((acting(OnDelete::Cascade), acting(OnDelete::SetNull), acting(OnDelete::Restrict)), (30, 20, 0));
    for tree in ["pages", "comments"] {
        let own = table(tree).foreign_keys.iter().find(|fk| fk.ref_table == tree).expect("a self-reference");
        assert_eq!((own.columns.clone(), own.on_delete), (vec!["parent_id".to_string()], OnDelete::Cascade));
    }

    assert_eq!(table("workspace").checks.len(), 1);
    assert_eq!(table("space_members").checks.len(), 2);
    Ok(())
}

// R-S1, R-S8, R-S11, R-X1, R-X2, R-X5: a migration step under a named lock, in one transaction with its bookkeeping row.
#[tokio::test(flavor = "multi_thread")]
async fn a_migration_and_its_bookkeeping_row_commit_together_or_not_at_all() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let space = new_space(&db, "docs").await?;
    let page = new_page(&db, space, None, "a0", "Before the migration").await?;
    new_page(&db, space, None, "a1", "Before the migration").await?;
    // What has been applied so far.
    assert!(db.query(Query::table("schema_migrations").columns(["name"])).await?.is_empty());

    // Postgres makes migrators queue on an advisory lock inside the
    // transaction; here a named lock is held around it, so a second copy
    // of the application starting now waits its turn.
    let turn = db.lock("app.migrate").await?;
    assert!(db.try_lock("app.migrate").await?.is_none());
    let tx = db.begin().await?;
    // Inside the lock: someone else may have applied it meanwhile.
    assert!(tx.get("schema_migrations", ["006_stars"]).await?.is_none());
    // The step's schema changes: a column and an index on a table
    // that has rows, and a new table.
    tx.add_column("pages", ColumnDef::new("stars", Type::Int).default(0)).await?;
    tx.create_index("pages", IndexDef::new("pages_stars_idx").on(["space_id"]).on_desc("stars").on(["id"])).await?;
    tx.create_table(
        TableDef::new("page_stars")
            .column(uuid_col("page_id"))
            .column(uuid_col("user_id"))
            .primary_key(["page_id", "user_id"])
            .foreign_key(["page_id"], "pages", OnDelete::Cascade)
            .foreign_key(["user_id"], "users", OnDelete::Cascade),
    )
    .await?;
    // The bookkeeping row, in the same transaction.
    tx.insert(Insert::into("schema_migrations").set("name", "006_stars")).await?;
    // Nobody else sees a half-applied step.
    assert!(db.describe_table("page_stars").await?.is_none());
    assert!(db.get("schema_migrations", ["006_stars"]).await?.is_none());
    tx.commit().await?;
    turn.unlock().await?;

    assert!(db.describe_table("page_stars").await?.is_some());
    // A row written before the column existed reads the column's default,
    // and the new index holds the rows that were already there.
    assert_eq!(db.get("pages", [page]).await?.expect("the page").get::<i64>("stars")?, 0);
    db.update(Update::table("pages").key([page]).set("stars", 5)).await?;
    let starred = db.query(Query::table("pages").index("pages_stars_idx", KeyRange::eq([space])).columns(["id", "stars"])).await?;
    assert_eq!(column::<i64>(&starred, "stars"), [5, 0]);
    assert_eq!(ids(&starred)[0], page);
    let applied = db.query_one(Query::table("schema_migrations").key(["006_stars"])).await?;
    assert!(!applied.is_null("applied_at")?);

    // The copy that waited takes its turn, re-checks, and has nothing to do.
    let turn = db.try_lock("app.migrate").await?.expect("the lock is free again");
    assert!(db.get("schema_migrations", ["006_stars"]).await?.is_some());

    // A step that fails half way leaves nothing behind: not its table,
    // not its row. Here a unique index meets two pages of one title.
    let tx = db.begin().await?;
    tx.create_table(TableDef::new("half_done").column(ColumnDef::new("id", Type::Int)).primary_key(["id"])).await?;
    tx.insert(Insert::into("schema_migrations").set("name", "007_half_done")).await?;
    let failed = tx.create_index("pages", IndexDef::new("pages_title_key").on(["title"]).unique()).await.unwrap_err();
    assert_eq!(failed.code, ErrorCode::UniqueViolation);
    assert!(failed.violates("pages_title_key"));
    assert_eq!(tx.commit().await.unwrap_err().code, ErrorCode::TransactionAborted);
    turn.unlock().await?;

    assert!(db.describe_table("half_done").await?.is_none());
    assert!(db.describe_table("pages").await?.expect("the table").find_index("pages_title_key").is_none());
    assert_eq!(column::<String>(&db.query(Query::table("schema_migrations")).await?, "name"), ["006_stars"]);
    // Adding what is there already is an error of its own.
    let again = db.add_column("pages", ColumnDef::new("stars", Type::Int).default(0)).await.unwrap_err();
    assert_eq!(again.code, ErrorCode::AlreadyExists);
    Ok(())
}

// --------------------------------------------------------------- workspace

/// The first person to arrive becomes the owner and gets a workspace
/// around them. Postgres needs an advisory lock so that two first
/// sign-ins cannot both find the table empty; the writing transaction is
/// that lock.
async fn first_run_setup(db: &Client, email: &str, workspace: &str) -> DbResult<Option<Uuid>> {
    db.transaction(|tx| async move {
        // Is anybody here yet?
        if !tx.query(Query::table("users").limit(1).columns(["id"])).await?.is_empty() {
            return Ok(None);
        }
        // The owner.
        let owner: Uuid = tx
            .insert(Insert::into("users").set("email", email).set("name", "Owner").set("role", "owner").returning(["id"]))
            .await?
            .one()?
            .get("id")?;
        // The singleton row, made or renamed.
        tx.insert(
            Insert::into("workspace")
                .set("id", 1)
                .set("name", workspace)
                .on_conflict_update(PRIMARY)
                .update("name", excluded("name"))
                .update("updated_at", now()),
        )
        .await?;
        // The default group, with the owner in it.
        let everyone = Insert::into("groups")
            .set("name", "Everyone")
            .set("description", "Every member of the workspace")
            .set("is_default", true)
            .set("created_by", owner);
        let everyone: Uuid = tx.insert(everyone.returning(["id"])).await?.one()?.get("id")?;
        tx.insert(Insert::into("group_members").set("group_id", everyone).set("user_id", owner)).await?;
        // A first space, the owner's grant in it, and the workspace's
        // pointer to it as the default.
        let general: Uuid = tx
            .insert(Insert::into("spaces").set("name", "General").set("slug", "general").set("created_by", owner).returning(["id"]))
            .await?
            .one()?
            .get("id")?;
        tx.insert(Insert::into("space_members").set("space_id", general).set("user_id", owner).set("role", "admin").set("added_by", owner)).await?;
        tx.update(Update::table("workspace").key([1]).set("default_space_id", general)).await?;
        Ok::<_, DbError>(Some(owner))
    })
    .await
}

// R-W4, R-X1, R-X2, R-X3: first-run setup in one writing transaction, raced by two first sign-ins.
#[tokio::test(flavor = "multi_thread")]
async fn first_run_setup_happens_once_however_many_start_it() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;

    let (a, b) = race(
        {
            let db = db.clone();
            async move { first_run_setup(&db, "first@example.com", "Acme").await }
        },
        {
            let db = db.clone();
            async move { first_run_setup(&db, "second@example.com", "Bolt").await }
        },
    )
    .await;
    let owner = match (a?, b?) {
        (Some(owner), None) | (None, Some(owner)) => owner,
        other => panic!("exactly one setup runs, got {other:?}"),
    };

    assert_eq!(rows_in(&db, Query::table("users")).await?, 1);
    assert_eq!(rows_in(&db, Query::table("groups").filter(col("is_default").eq(true))).await?, 1);
    let general = db.query_one(Query::table("spaces").index("spaces_slug_key", KeyRange::eq(["general"]))).await?;
    let workspace = db.query_one(Query::table("workspace").key([1])).await?;
    assert_eq!(workspace.get::<Uuid>("default_space_id")?, general.get::<Uuid>("id")?);
    assert_eq!(general.get::<Uuid>("created_by")?, owner);
    let rank =
        db.query_one(Query::table("spaces").alias("s").key([general.get::<Uuid>("id")?]).select("rank", role_rank(col("s.id"), val(owner)))).await?;
    assert_eq!(rank.get::<i64>("rank")?, 3);

    // The same upsert again renames the one row instead of adding a second.
    let renamed = db
        .insert(
            Insert::into("workspace")
                .set("id", 1)
                .set("name", "Renamed")
                .on_conflict_update(PRIMARY)
                .update("name", excluded("name"))
                .update("updated_at", now()),
        )
        .await?;
    assert_eq!(renamed.affected(), 1);
    let rows = db.query(Query::table("workspace")).await?;
    assert_eq!(column::<String>(&rows, "name"), ["Renamed"]);
    assert_eq!(rows.first().expect("the row").get::<Uuid>("default_space_id")?, general.get::<Uuid>("id")?);
    Ok(())
}

/// A patch of the workspace row: `None` keeps a column; for the
/// description, which can be cleared, `Some(None)` writes NULL.
struct WorkspacePatch<'a> {
    name: Option<&'a str>,
    description: Option<Option<&'a str>>,
    email_domains: Option<Vec<&'a str>>,
    settings: Json,
}

fn patch_workspace(patch: WorkspacePatch) -> Update {
    Update::table("workspace")
        .key([1])
        .set("name", coalesce([val(patch.name), col("name")]))
        .set("description", when(val(patch.description.is_some()), val(patch.description.flatten()), col("description")))
        .set("email_domains", coalesce([val(patch.email_domains), col("email_domains")]))
        .set("settings", col("settings").json_merge(patch.settings))
        .set("updated_at", now())
}

// R-Q13, R-S7, R-T7, R-W6, R-W11: the one workspace row, patched column by column with a shallow merge of its settings.
#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_patch_keeps_what_is_absent_and_clears_what_is_null() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    db.insert(Insert::into("workspace").set("logo", "old.png")).await?;

    // The table holds one row, with id 1, and no other.
    let second = db.insert(Insert::into("workspace").set("name", "Another")).await.unwrap_err();
    assert_eq!(second.code, ErrorCode::UniqueViolation);
    assert!(second.violates(PRIMARY));
    let other_id = db.insert(Insert::into("workspace").set("id", 2)).await.unwrap_err();
    assert_eq!(other_id.code, ErrorCode::CheckViolation);
    assert!(other_id.violates("workspace_id_check"));

    let settings = json!({"sharingDisabled": true, "trashRetentionDays": 7});
    db.update(patch_workspace(WorkspacePatch {
        name: None,
        description: Some(Some("Docs")),
        email_domains: Some(vec!["example.com", "example.org"]),
        settings: settings.clone(),
    }))
    .await?;
    // The row read back, whole here and projected below.
    let row = db.query_one(Query::table("workspace").key([1])).await?;
    assert_eq!(row.get::<String>("name")?, "My workspace");
    assert_eq!(row.get::<Option<String>>("description")?.as_deref(), Some("Docs"));
    assert_eq!(row.get::<Vec<String>>("email_domains")?, ["example.com", "example.org"]);
    assert_eq!(row.get::<Json>("settings")?, settings);
    assert!(row.get::<Timestamp>("updated_at")? >= row.get::<Timestamp>("created_at")?);

    // The merge is shallow: a key the patch does not name stays.
    db.update(patch_workspace(WorkspacePatch {
        name: Some("Acme"),
        description: None,
        email_domains: None,
        settings: json!({"sharingDisabled": false, "defaultPageEditMode": "read"}),
    }))
    .await?;
    let row = db.query_one(Query::table("workspace").key([1]).columns(["name", "description", "email_domains", "settings"])).await?;
    assert_eq!(row.get::<String>("name")?, "Acme");
    assert_eq!(row.get::<Option<String>>("description")?.as_deref(), Some("Docs"));
    assert_eq!(row.get::<Vec<String>>("email_domains")?, ["example.com", "example.org"]);
    assert_eq!(row.get::<Json>("settings")?, json!({"sharingDisabled": false, "trashRetentionDays": 7, "defaultPageEditMode": "read"}));

    db.update(patch_workspace(WorkspacePatch { name: None, description: Some(None), email_domains: Some(vec![]), settings: json!({}) })).await?;
    let row = db.query_one(Query::table("workspace").key([1]).columns(["description", "email_domains"])).await?;
    assert!(row.is_null("description")?);
    assert!(row.get::<Vec<String>>("email_domains")?.is_empty());

    // A setting read out of the JSON as the type it stands for.
    let read = Query::table("workspace")
        .key([1])
        .select("off", col("settings").json_text(["sharingDisabled"]).cast(Type::Bool))
        .select("days", col("settings").json_text(["trashRetentionDays"]).cast(Type::Int))
        .select("unset", col("settings").json_text(["nothing"]).cast(Type::Bool));
    let read = db.query_one(read).await?;
    assert_eq!((read.get::<bool>("off")?, read.get::<i64>("days")?, read.get::<Option<bool>>("unset")?), (false, 7, None));

    // Clearing the logo.
    assert_eq!(db.update(Update::table("workspace").key([1]).set("logo", null()).set("updated_at", now())).await?.affected(), 1);
    assert!(db.query_one(Query::table("workspace").key([1]).columns(["logo"])).await?.is_null("logo")?);
    Ok(())
}

// R-W6, R-W7, R-X3: an update that returns the value it replaced, alone and raced.
#[tokio::test(flavor = "multi_thread")]
async fn a_swap_returns_the_value_it_replaced() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    db.insert(Insert::into("workspace").set("name", "Acme")).await?;

    // Postgres joins the table to itself to return the old value;
    // `old(..)` reads it.
    let swap_logo = |logo: &str| {
        Update::table("workspace").key([1]).set("logo", logo).set("updated_at", now()).returning(["logo"]).returning_expr("previous", old("logo"))
    };
    let first = db.update(swap_logo("a.png")).await?.one()?;
    assert_eq!((first.get::<String>("logo")?, first.get::<Option<String>>("previous")?), ("a.png".to_string(), None));
    let second = db.update(swap_logo("b.png")).await?.one()?;
    assert_eq!(second.get::<Option<String>>("previous")?.as_deref(), Some("a.png"));

    // The same for a user's avatar, and nobody there to swap for.
    let swap_avatar =
        |user: Uuid| Update::table("users").key([user]).set("avatar", "new.png").set("updated_at", now()).returning_expr("previous", old("avatar"));
    assert!(db.update(swap_avatar(w.ann)).await?.one()?.is_null("previous")?);
    let nobody = db.update(swap_avatar(uuid(404))).await?;
    assert_eq!((nobody.affected(), nobody.len()), (0, 0));

    // Two uploads replace one file's object at the same moment.
    // Each learns the key it superseded, so each can remove that object:
    // one replaced the original, the other replaced the first one's.
    let page = new_page(&db, w.space, None, "a0", "Files").await?;
    let file = new_file(&db, page, w.space, "original").await?;
    let replace = |key: &'static str| {
        let db = db.clone();
        async move {
            let replaced =
                Update::table("files").key([file]).set("size", 10).set("mime", "image/png").set("storage_key", key).set("updated_at", now());
            db.update(replaced.returning_expr("old_key", old("storage_key"))).await?.one()?.get::<String>("old_key")
        }
    };
    let (a, b) = race(replace("first"), replace("second")).await;
    let (a, b) = (a?, b?);
    let now_stored: String = db.get("files", [file]).await?.expect("the file").get("storage_key")?;
    assert!(a == "original" || b == "original");
    assert_eq!(sorted(vec![a, b, now_stored]), ["first", "original", "second"]);
    Ok(())
}

// ------------------------------------------------------------------- users

const USER_COLUMNS: [&str; 16] = [
    "id", "email", "name", "password_hash", "avatar", "role", "locale", "timezone", "preferences", "invited_by", "last_login_at", "deactivated_at",
    "created_at", "updated_at", "auth_sub", "email_from_provider",
];

/// A user by email whatever its case; with single sign-on in use,
/// only an account that is not linked to the identity provider.
fn user_by_email(email: &str, sso_off: bool) -> Query {
    Query::table("users").index("users_email_key", KeyRange::eq([email.to_lowercase()])).filter(val(sso_off).or(col("auth_sub").is_null()))
}

// R-Q1, R-Q3, R-Q6, R-S5, R-T2, R-W1: inserting users and finding them by key, by several keys and through unique indexes.
#[tokio::test(flavor = "multi_thread")]
async fn users_are_found_by_id_by_case_folded_email_and_by_provider_subject() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;

    // `RETURNING *` is every column by name; the row comes back with
    // the defaults the database filled in.
    let ann = Insert::into("users").set("email", "Ann@Example.com").set("name", "Ann").set("password_hash", "$argon2id$ann").set("role", "owner");
    let ann = db.insert(ann.returning(USER_COLUMNS)).await?;
    assert_eq!(ann.affected(), 1);
    let ann = ann.one()?;
    let ann_id: Uuid = ann.get("id")?;
    assert_eq!(ann.get::<Json>("preferences")?, json!({}));
    assert!(!ann.get::<bool>("email_from_provider")?);
    assert_eq!(ann.get::<Timestamp>("created_at")?, ann.get::<Timestamp>("updated_at")?);
    assert!(ann.is_null("auth_sub")? && ann.is_null("invited_by")? && ann.is_null("last_login_at")?);

    let bob = Insert::into("users").set("email", "bob@example.com").set("auth_sub", "sub-bob").set("invited_by", ann_id);
    let bob = db.insert(bob.returning(["id", "role", "name"])).await?.one()?;
    let bob_id: Uuid = bob.get("id")?;
    assert_eq!((bob.get::<String>("role")?, bob.get::<String>("name")?), ("member".to_string(), String::new()));
    assert_ne!(ann_id, bob_id);

    // By primary key, whole or in part. A uuid is the same value in
    // either letter case, and comes back in lower case.
    let row = db.get("users", [ann_id]).await?.expect("ann");
    assert_eq!(row.get::<String>("email")?, "Ann@Example.com");
    let upper = ann_id.to_string().to_uppercase();
    let row = db.query_one(Query::table("users").key([upper.as_str()]).columns(["id", "password_hash"])).await?;
    assert_eq!(row.get::<String>("password_hash")?, "$argon2id$ann");
    assert_eq!(row.get::<Uuid>("id")?.to_string(), upper.to_lowercase());
    assert!(db.query_opt(Query::table("users").key([ann_id]).filter(col("deactivated_at").is_null()).columns(["email", "name"])).await?.is_some());

    // Several by id, in the order asked; an id nobody has is left out.
    let people = db.query(Query::table("users").keys([[bob_id], [uuid(404)], [ann_id]]).columns(["id", "name", "avatar"])).await?;
    assert_eq!(ids(&people), [bob_id, ann_id]);

    // By the identity provider's subject.
    let by_subject = db.query_one(Query::table("users").index("users_auth_sub_key", KeyRange::eq(["sub-bob"])).columns(["id"])).await?;
    assert_eq!(by_subject.get::<Uuid>("id")?, bob_id);
    assert!(db.query_opt(Query::table("users").index("users_auth_sub_key", KeyRange::eq(["sub-nobody"]))).await?.is_none());

    // By email in any letter case, with and without single sign-on.
    assert_eq!(db.query_one(user_by_email("ANN@example.com", false)).await?.get::<Uuid>("id")?, ann_id);
    assert!(db.query_opt(user_by_email("bob@example.com", false)).await?.is_none());
    assert_eq!(db.query_one(user_by_email("bob@example.com", true)).await?.get::<Uuid>("id")?, bob_id);
    let active = user_by_email("ann@example.com", true).filter(col("deactivated_at").is_null()).columns(["id", "email", "name"]);
    assert_eq!(db.query_one(active).await?.get::<String>("name")?, "Ann");

    // Is the address taken, by anyone, or by anyone else?
    let taken_by_other =
        |email: &str, me: Uuid| Query::table("users").index("users_email_key", KeyRange::eq([email])).filter(col("id").ne(me)).columns(["id"]);
    assert!(db.query_opt(taken_by_other("ann@example.com", ann_id)).await?.is_none());
    assert!(db.query_opt(taken_by_other("ann@example.com", bob_id)).await?.is_some());

    // Is there any user at all, and which of these addresses are known?
    assert!(!db.query(Query::table("users").limit(1).columns(["id"])).await?.is_empty());
    let asked = ["bob@example.com", "nobody@example.com", "ann@example.com"];
    let known = db
        .query(Query::table("users").index_ranges("users_email_key", asked.map(|email| KeyRange::eq([email]))).select("email", col("email").lower()))
        .await?;
    assert_eq!(column::<String>(&known, "email"), ["bob@example.com", "ann@example.com"]);
    Ok(())
}

// R-S3, R-T9, R-W7, R-W11, R-X2: unique violations that name their index, and two transactions updating the same rows in opposite order.
#[tokio::test(flavor = "multi_thread")]
async fn a_duplicate_email_or_subject_names_the_index_it_broke() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    // Two users without a provider subject: NULL is no key in a unique index.
    let ann = new_user(&db, "ann").await?;
    let bob = new_user(&db, "bob").await?;

    let taken = db.insert(Insert::into("users").set("email", "ANN@example.COM")).await.unwrap_err();
    assert_eq!(taken.code, ErrorCode::UniqueViolation);
    assert!(taken.violates("users_email_key"));
    assert_eq!(taken.table.as_deref(), Some("users"));

    // Link an account to a provider subject only while it has none.
    // A row back means linked; no row, somebody's already; a violation of
    // the subject's index, that another sign-in of the same person won.
    let link = |email: &str, subject: &str| {
        let unlinked = Update::table("users").index("users_email_key", KeyRange::eq([email])).filter(col("auth_sub").is_null());
        unlinked.set("auth_sub", subject).set("updated_at", now()).returning(USER_COLUMNS)
    };
    let linked = db.update(link("ann@example.com", "sub-1")).await?;
    assert_eq!(linked.affected(), 1);
    assert_eq!(linked.one()?.get::<Uuid>("id")?, ann);
    assert!(db.update(link("ann@example.com", "sub-2")).await?.is_empty());
    let raced = db.update(link("bob@example.com", "sub-1")).await.unwrap_err();
    assert_eq!(raced.code, ErrorCode::UniqueViolation);
    assert!(raced.violates("users_auth_sub_key"));

    // The provider's claims, email included. When the address is someone
    // else's the whole statement fails, and the caller says it again
    // without the email.
    let claims = |email: Option<&str>| {
        let user = Update::table("users").key([bob]).set("role", "admin").set("name", "Robert").set("email", coalesce([val(email), col("email")]));
        user.set("email_from_provider", email.is_some()).set("updated_at", now()).returning(USER_COLUMNS)
    };
    let clash = db.update(claims(Some("Ann@example.com"))).await.unwrap_err();
    assert!(clash.is_unique_violation() && clash.violates("users_email_key"));
    let untouched = db.get("users", [bob]).await?.expect("bob");
    assert_eq!((untouched.get::<String>("name")?, untouched.get::<String>("role")?), ("bob".to_string(), "member".to_string()));
    let applied = db.update(claims(None)).await?.one()?;
    assert_eq!(
        (applied.get::<String>("email")?, applied.get::<String>("name")?, applied.get::<String>("role")?),
        ("bob@example.com".to_string(), "Robert".to_string(), "admin".to_string())
    );

    // Two sign-ins of one person each update the same two users in
    // opposite order. In Postgres that can deadlock (40P01) and the caller
    // has to retry; writing transactions run one after the other, so both
    // commit and the later one's values stand on both rows.
    let touch = |first: Uuid, second: Uuid, locale: &'static str| {
        let db = db.clone();
        async move {
            db.transaction(|tx| async move {
                tx.update(Update::table("users").key([first]).set("locale", locale)).await?;
                tokio::time::sleep(Duration::from_millis(20)).await;
                tx.update(Update::table("users").key([second]).set("locale", locale)).await?;
                Ok::<_, DbError>(())
            })
            .await
        }
    };
    let (a, b) = race(touch(ann, bob, "en"), touch(bob, ann, "fr")).await;
    a?;
    b?;
    let locales: Vec<String> = column(&db.query(Query::table("users").keys([[ann], [bob]]).columns(["locale"])).await?, "locale");
    assert_eq!(locales[0], locales[1]);
    Ok(())
}

/// The member list's predicate: no needle, or the needle somewhere in the
/// name or the address. The caller lower-cases and escapes the needle.
fn member_matches(needle: &str) -> Expr {
    let pattern = format!("%{}%", like_escape(&needle.to_lowercase()));
    val(needle.is_empty()).or(col("name").lower().like(pattern.clone())).or(col("email").lower().like(pattern))
}

// R-Q5, R-Q7, R-Q11: a filtered, ordered, paged list with its total, and conditional counts.
#[tokio::test(flavor = "multi_thread")]
async fn the_member_list_filters_orders_and_counts() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    for (name, email, role) in [
        ("ann", "ann@x.test", "owner"),
        ("Bob", "bob@x.test", "member"),
        ("bobby", "bobby@x.test", "member"),
        ("Abby Bobson", "abby@x.test", "admin"),
        ("Carl_1", "carl1@x.test", "member"),
        ("Carlx1", "carlx@x.test", "owner"),
    ] {
        db.insert(Insert::into("users").set("name", name).set("email", email).set("role", role)).await?;
    }
    db.update(Update::table("users").index("users_email_key", KeyRange::eq(["carlx@x.test"])).set("deactivated_at", now())).await?;

    // The active first, then by name without regard to case, then by
    // id; one page of it, and the count of all of it (`count(*) OVER ()`).
    let list = |needle: &str| {
        Query::table("users")
            .filter(member_matches(needle))
            .order_by(col("deactivated_at").is_not_null())
            .order_by(col("name").lower())
            .order_by(col("id"))
    };
    let names = |rows: &Rows| column::<String>(rows, "name");
    let everyone = db.query(list("")).await?;
    assert_eq!(names(&everyone), ["Abby Bobson", "ann", "Bob", "bobby", "Carl_1", "Carlx1"]);
    let page = db.query(list("").limit(2).offset(2).with_total()).await?;
    assert_eq!(names(&page), ["Bob", "bobby"]);
    assert_eq!(page.total(), Some(6));
    let found = db.query(list("BOB").limit(50).with_total()).await?;
    assert_eq!(names(&found), ["Abby Bobson", "Bob", "bobby"]);
    assert_eq!(found.total(), Some(3));
    // `_` in the needle is a character, not a wildcard: Carlx1 does not match.
    assert_eq!(names(&db.query(list("l_1")).await?), ["Carl_1"]);

    // Past the last page the rows are none and the total still stands.
    let beyond = db.query(list("bob").limit(2).offset(10).with_total()).await?;
    assert_eq!((beyond.len(), beyond.total()), (0, Some(3)));
    assert_eq!(rows_in(&db, Query::table("users").filter(member_matches("bob"))).await?, 3);

    // Suggestions. Names that start with the needle come first: a
    // sort key that is a condition.
    let suggest = Query::table("users")
        .filter(col("deactivated_at").is_null())
        .filter(col("name").lower().like("%bob%").or(col("email").lower().like("%bob%")))
        .order_by(col("name").lower().like("bob%").desc())
        .order_by(col("name").lower())
        .order_by(col("id"))
        .limit(5);
    assert_eq!(names(&db.query(suggest).await?), ["Bob", "bobby", "Abby Bobson"]);

    // How many owners there are, and how many of them are active.
    let owners = Query::table("users")
        .select("active", count().filter(col("role").eq("owner").and(col("deactivated_at").is_null())))
        .select("all", count().filter(col("role").eq("owner")));
    let owners = db.query_one(owners).await?;
    assert_eq!((owners.get::<i64>("active")?, owners.get::<i64>("all")?), (1, 2));
    Ok(())
}

// R-Q8, R-Q13, R-W6: a profile patch that merges JSON preferences, and timestamps set by the database's clock.
#[tokio::test(flavor = "multi_thread")]
async fn a_profile_patch_merges_preferences_and_drops_nulled_keys() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = new_user(&db, "ann").await?;
    db.update(Update::table("users").key([ann]).set("preferences", json!({"theme": "dark", "fullWidth": true})).set("timezone", "Europe/Paris"))
        .await?;

    // A null in the patch removes the preference; the rest merge.
    // The locale is given, the time zone is not.
    let prefs = json!({"theme": null, "editorWidth": 720});
    let patch = Update::table("users")
        .key([ann])
        .set("name", coalesce([val("Ann A."), col("name")]))
        .set("email", coalesce([null(), col("email")]))
        .set("locale", when(val(true), val("fr"), col("locale")))
        .set("timezone", when(val(false), null(), col("timezone")))
        .set("preferences", when(val(true), call(Func::JsonStripNulls, [col("preferences").json_merge(prefs)]), col("preferences")))
        .set("updated_at", now());
    assert_eq!(db.update(patch).await?.affected(), 1);
    let row = db.get("users", [ann]).await?.expect("ann");
    assert_eq!(row.get::<String>("name")?, "Ann A.");
    assert_eq!(row.get::<String>("email")?, "ann@example.com");
    assert_eq!((row.get::<String>("locale")?, row.get::<String>("timezone")?), ("fr".to_string(), "Europe/Paris".to_string()));
    // Stored as Postgres' jsonb would return it: shorter keys first.
    assert_eq!(row.get::<Json>("preferences")?.to_string(), r#"{"fullWidth":true,"editorWidth":720}"#);

    // A sign-in stamps its moment; a new password replaces the hash.
    db.update(Update::table("users").key([ann]).set("last_login_at", now())).await?;
    db.update(Update::table("users").key([ann]).set("password_hash", "$argon2id$new").set("updated_at", now())).await?;
    let row = db.get("users", [ann]).await?.expect("ann");
    assert_eq!(row.get::<String>("password_hash")?, "$argon2id$new");
    assert!(row.get::<Timestamp>("last_login_at")? >= row.get::<Timestamp>("created_at")?);

    // Deactivating twice keeps the first moment; reactivating clears it.
    let deactivate = || {
        Update::table("users")
            .key([ann])
            .set("deactivated_at", coalesce([col("deactivated_at"), now()]))
            .set("updated_at", now())
            .returning(["deactivated_at"])
    };
    let first: Timestamp = db.update(deactivate()).await?.one()?.get("deactivated_at")?;
    tokio::time::sleep(Duration::from_millis(5)).await;
    let again: Timestamp = db.update(deactivate()).await?.one()?.get("deactivated_at")?;
    assert_eq!(first, again);
    db.update(Update::table("users").key([ann]).set("deactivated_at", null()).set("updated_at", now())).await?;
    assert!(db.get("users", [ann]).await?.expect("ann").is_null("deactivated_at")?);
    Ok(())
}

// R-S6, R-W10: deleting a user cascades through what they owned and sets their authorship to NULL.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_user_removes_what_was_theirs_and_blanks_authorship() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let (ann, bob) = (w.ann, w.bob);

    // What is bob's own: it goes with him.
    grant(&db, w.space, bob, "writer").await?;
    let team = db.insert(Insert::into("groups").set("name", "team").set("created_by", bob).returning(["id"])).await?.one()?.get::<Uuid>("id")?;
    join_group(&db, team, bob).await?;
    join_group(&db, team, ann).await?;
    db.insert(Insert::into("sessions").set("user_id", bob).set("token_hash", "h1").set("expires_at", now() + days(30))).await?;
    db.insert(
        Insert::into("user_tokens")
            .set("user_id", bob)
            .set("purpose", "password-reset")
            .set("token_hash", "t1")
            .set("expires_at", now() + minutes(30)),
    )
    .await?;

    // What bob wrote: it stays, without his name on it.
    let page = page_insert(w.space, None, "a0", "Bob's page")
        .set("creator_id", bob)
        .set("last_editor_id", bob)
        .set("contributor_ids", uuids(&[ann, bob]))
        .set("deleted_at", now())
        .set("deleted_by", bob);
    let page: Uuid = db.insert(page.returning(["id"])).await?.one()?.get("id")?;
    db.insert(Insert::into("page_versions").set("page_id", page).set("editor_id", bob).set("contributor_ids", uuids(&[bob]))).await?;
    let comment: Uuid = db
        .insert(
            Insert::into("comments")
                .set("page_id", page)
                .set("author_id", bob)
                .set("resolved_by", bob)
                .set("body", json!({"text": "hi"}))
                .returning(["id"]),
        )
        .await?
        .one()?
        .get("id")?;
    let file: Uuid = db
        .insert(Insert::into("files").set("page_id", page).set("uploader_id", bob).set("name", "f").set("storage_key", "k").returning(["id"]))
        .await?
        .one()?
        .get("id")?;
    let share: Uuid =
        db.insert(Insert::into("shares").set("key", "abc").set("page_id", page).set("created_by", bob).returning(["id"])).await?.one()?.get("id")?;
    let row: Uuid = db
        .insert(
            Insert::into("base_rows").set("page_id", page).set("position", "a0").set("creator_id", bob).set("last_editor_id", bob).returning(["id"]),
        )
        .await?
        .one()?
        .get("id")?;
    let view: Uuid = db
        .insert(
            Insert::into("base_views")
                .set("page_id", page)
                .set("name", "Table")
                .set("type", "table")
                .set("position", "a0")
                .set("creator_id", bob)
                .returning(["id"]),
        )
        .await?
        .one()?
        .get("id")?;
    let invitation: Uuid = db
        .insert(Insert::into("invitations").set("email", "new@example.com").set("token_hash", "i1").set("invited_by", bob).returning(["id"]))
        .await?
        .one()?
        .get("id")?;
    let invited: Uuid =
        db.insert(Insert::into("users").set("email", "cat@example.com").set("invited_by", bob).returning(["id"])).await?.one()?.get("id")?;
    let granted = db
        .insert(
            Insert::into("space_members")
                .set("space_id", w.space)
                .set("user_id", invited)
                .set("role", "reader")
                .set("added_by", bob)
                .returning(["id"]),
        )
        .await?
        .one()?
        .get::<Uuid>("id")?;
    db.update(Update::table("spaces").key([w.space]).set("created_by", bob)).await?;

    db.insert(Insert::into("favorites").set("user_id", bob).set("page_id", page)).await?;
    db.insert(Insert::into("watchers").set("user_id", bob).set("space_id", w.space)).await?;
    db.insert(Insert::into("notifications").set("user_id", bob).set("kind", "page.updated").set("actor_id", ann).set("page_id", page)).await?;
    let told_ann: Uuid = db
        .insert(
            Insert::into("notifications")
                .set("user_id", ann)
                .set("kind", "comment.created")
                .set("actor_id", bob)
                .set("page_id", page)
                .returning(["id"]),
        )
        .await?
        .one()?
        .get("id")?;

    // One statement, and the foreign keys do the rest.
    assert_eq!(db.delete(Delete::from("users").key([bob])).await?.affected(), 1);

    for (table, by) in [
        ("sessions", "user_id"),
        ("user_tokens", "user_id"),
        ("group_members", "user_id"),
        ("space_members", "user_id"),
        ("favorites", "user_id"),
        ("watchers", "user_id"),
        ("notifications", "user_id"),
    ] {
        assert_eq!(rows_in(&db, Query::table(table).filter(col(by).eq(bob))).await?, 0, "{table}");
    }
    assert_eq!(rows_in(&db, Query::table("group_members").range(KeyRange::eq([team]))).await?, 1);

    let blank = |table: &'static str, id: Uuid, columns: &'static [&'static str]| {
        let db = db.clone();
        async move {
            let row = db.get(table, [id]).await?.unwrap_or_else(|| panic!("the {table} row survives"));
            for name in columns {
                assert!(row.is_null(name)?, "{table}.{name}");
            }
            Ok::<_, DbError>(row)
        }
    };
    let kept = blank("pages", page, &["creator_id", "last_editor_id", "deleted_by"]).await?;
    // The list of contributors has no foreign key: bob's id stays in it.
    assert_eq!(kept.get::<Vec<Uuid>>("contributor_ids")?, [ann, bob]);
    assert!(!kept.is_null("deleted_at")?);
    blank("comments", comment, &["author_id", "resolved_by"]).await?;
    blank("files", file, &["uploader_id"]).await?;
    blank("shares", share, &["created_by"]).await?;
    blank("base_rows", row, &["creator_id", "last_editor_id"]).await?;
    blank("base_views", view, &["creator_id"]).await?;
    blank("groups", team, &["created_by"]).await?;
    blank("spaces", w.space, &["created_by"]).await?;
    blank("invitations", invitation, &["invited_by"]).await?;
    blank("users", invited, &["invited_by"]).await?;
    blank("space_members", granted, &["added_by"]).await?;
    blank("notifications", told_ann, &["actor_id"]).await?;
    let version = db.query_one(Query::table("page_versions").index("page_versions_page_idx", KeyRange::eq([page]))).await?;
    assert!(version.is_null("editor_id")?);
    assert_eq!(version.get::<Vec<Uuid>>("contributor_ids")?, [bob]);
    Ok(())
}

// ----------------------------------------------------- sessions and tokens

/// A session with its user, while the session has not expired by the
/// database's clock and the user has not been deactivated.
fn live_session(sessions: Query) -> Query {
    sessions
        .alias("s")
        .join(Query::table("users").alias("u").key([col("s.user_id")]).filter(col("u.deactivated_at").is_null()))
        .filter(col("s.expires_at").gt(now()))
        .select("session_id", col("s.id"))
        .select("session_last_seen_at", col("s.last_seen_at"))
        .select("session_auth_sid", col("s.auth_sid"))
        .select("user_id", col("u.id"))
        .select("email", col("u.email"))
        .select("role", col("u.role"))
}

// R-Q1, R-Q6, R-Q8, R-Q9, R-T4, R-W1, R-W6: sessions found by token hash and joined to their user, with expiry by the database's clock.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_resolves_to_its_user_while_both_are_valid() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = new_user(&db, "ann").await?;
    let bob = new_user(&db, "bob").await?;

    // Signing in. The caller computes the expiry and sends it as text.
    let sign_in = |user: Uuid, hash: &str, expires: Expr| {
        let session = Insert::into("sessions")
            .set("user_id", user)
            .set("token_hash", hash)
            .set("user_agent", "tests")
            .set("ip", "127.0.0.1")
            .set("expires_at", expires);
        session.set("auth_sid", null()).set("auth_refresh_token", null()).set("auth_recheck_at", null()).returning(["id"])
    };
    let current: Uuid = db.insert(sign_in(ann, "hash-current", val("2099-01-01T00:00:00Z"))).await?.one()?.get("id")?;
    let expired: Uuid = db.insert(sign_in(ann, "hash-expired", now() - seconds(1))).await?.one()?.get("id")?;
    let bobs: Uuid = db.insert(sign_in(bob, "hash-bob", now() + days(30))).await?.one()?.get("id")?;
    let again = db.insert(sign_in(bob, "hash-bob", now() + days(30))).await.unwrap_err();
    assert!(again.violates("sessions_token_hash_key"));

    // On every request: one read by the unique hash, one by key.
    let by_token = |hash: &str| live_session(Query::table("sessions").index("sessions_token_hash_key", KeyRange::eq([hash])));
    let found = db.query_one(by_token("hash-current")).await?;
    assert_eq!(
        (found.get::<Uuid>("session_id")?, found.get::<Uuid>("user_id")?, found.get::<String>("email")?),
        (current, ann, "ann@example.com".to_string())
    );
    assert!(db.query_opt(by_token("hash-expired")).await?.is_none());
    assert!(db.query_opt(by_token("hash-unknown")).await?.is_none());

    // The same for a batch of sessions.
    let batch = live_session(Query::table("sessions").keys([[bobs], [expired], [current]]));
    assert_eq!(column::<Uuid>(&db.query(batch.clone()).await?, "session_id"), [bobs, current]);
    db.update(Update::table("users").key([bob]).set("deactivated_at", now())).await?;
    assert_eq!(column::<Uuid>(&db.query(batch).await?, "session_id"), [current]);
    assert!(db.query_opt(by_token("hash-bob")).await?.is_none());

    // The expiry slides; a refresh token is replaced only when a new one
    // is given.
    db.update(Update::table("sessions").key([current]).set("last_seen_at", now()).set("expires_at", "2099-02-01T00:00:00Z")).await?;
    let recheck = |token: Option<&str>| {
        Update::table("sessions")
            .key([current])
            .set("auth_recheck_at", "2099-01-01T00:05:00Z")
            .set("auth_refresh_token", coalesce([val(token), col("auth_refresh_token")]))
    };
    db.update(recheck(Some("refresh-1"))).await?;
    db.update(recheck(None)).await?;
    let row = db.get("sessions", [current]).await?.expect("the session");
    assert_eq!(row.get::<Timestamp>("expires_at")?, ts("2099-02-01T00:00:00Z"));
    assert_eq!(row.get::<Timestamp>("auth_recheck_at")?, ts("2099-01-01T00:05:00Z"));
    assert_eq!(row.get::<String>("auth_refresh_token")?, "refresh-1");
    assert!(row.get::<Timestamp>("last_seen_at")? >= row.get::<Timestamp>("created_at")?);

    // A user's live sessions, the most recently used first.
    let older: Uuid = db.insert(sign_in(ann, "hash-older", now() + days(1)).set("last_seen_at", now() - hours(2))).await?.one()?.get("id")?;
    let mine = Query::table("sessions")
        .index("sessions_user_idx", KeyRange::eq([ann]))
        .filter(col("expires_at").gt(now()))
        .order_by(col("last_seen_at").desc());
    let mine = db.query(mine.columns(["id", "user_agent", "ip", "created_at", "last_seen_at"])).await?;
    assert_eq!(ids(&mine), [current, older]);
    Ok(())
}

// R-S4, R-W7, R-W10: deletes by key, through a partial index with the ids returned, and by a filter on time.
#[tokio::test(flavor = "multi_thread")]
async fn sessions_end_by_key_by_user_by_provider_session_and_by_expiry() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = new_user(&db, "ann").await?;
    let bob = new_user(&db, "bob").await?;
    let mut sessions = Vec::new();
    for (n, user, sid, expires) in [
        (1, ann, None, days(30)),
        (2, ann, Some("sid-1"), days(30)),
        (3, ann, Some("sid-1"), days(30)),
        (4, ann, None, days(30)),
        (5, bob, Some("sid-2"), days(30)),
        (6, bob, None, -seconds(5)),
    ] {
        let session = Insert::into("sessions")
            .set("id", uuid(n))
            .set("user_id", user)
            .set("token_hash", format!("hash-{n}"))
            .set("auth_sid", sid)
            .set("expires_at", now() + expires);
        sessions.push(db.insert(session.returning(["id"])).await?.one()?.get::<Uuid>("id")?);
    }
    let left = || async { Ok::<_, DbError>(ids(&db.query(Query::table("sessions").columns(["id"])).await?)) };

    // By id; and by id only if it is this user's.
    assert_eq!(db.delete(Delete::from("sessions").key([sessions[0]]).filter(col("user_id").eq(bob))).await?.affected(), 0);
    assert_eq!(db.delete(Delete::from("sessions").key([sessions[0]]).filter(col("user_id").eq(ann))).await?.affected(), 1);
    assert_eq!(db.delete(Delete::from("sessions").key([sessions[0]])).await?.affected(), 0);

    // Every session of one sign-in at the provider, through the index
    // that holds only sessions with one; the ids come back so their
    // sockets can be closed.
    let ended = db.delete(Delete::from("sessions").index("sessions_auth_sid_idx", KeyRange::eq(["sid-1"])).returning(["id"])).await?;
    assert_eq!(sorted(ids(&ended)), [uuid(2), uuid(3)]);

    // The hourly purge.
    assert_eq!(db.delete(Delete::from("sessions").filter(col("expires_at").lt(now()))).await?.affected(), 1);
    assert_eq!(left().await?, [uuid(4), uuid(5)]);

    // All of a user's but the one in use; then all of them.
    let keep: Uuid = db
        .insert(
            Insert::into("sessions")
                .set("id", uuid(7))
                .set("user_id", ann)
                .set("token_hash", "hash-7")
                .set("expires_at", now() + days(1))
                .returning(["id"]),
        )
        .await?
        .one()?
        .get("id")?;
    assert_eq!(db.delete(Delete::from("sessions").index("sessions_user_idx", KeyRange::eq([ann])).filter(col("id").ne(keep))).await?.affected(), 1);
    assert_eq!(left().await?, [uuid(5), uuid(7)]);
    assert_eq!(db.delete(Delete::from("sessions").index("sessions_user_idx", KeyRange::eq([ann]))).await?.affected(), 1);
    assert_eq!(left().await?, [uuid(5)]);
    Ok(())
}

/// Spends a password-reset token. The delete is the proof: a row
/// back means the token was valid and is now gone.
async fn consume_reset_token(db: &impl Db, hash: &str, sso_off: bool) -> DbResult<Option<Uuid>> {
    let valid = Query::table("user_tokens")
        .alias("t")
        .index("user_tokens_token_hash_key", KeyRange::eq([hash]))
        .join(
            Query::table("users")
                .alias("u")
                .key([col("t.user_id")])
                .filter(col("u.deactivated_at").is_null().and(val(sso_off).or(col("u.auth_sub").is_null()))),
        )
        .filter(col("t.purpose").eq("password-reset").and(col("t.expires_at").gt(now())));
    let spent = db.delete(Delete::rows(valid).returning_expr("user_id", col("t.user_id"))).await?;
    spent.opt().map(|row| row.get("user_id")).transpose()
}

// R-Q8, R-W7, R-W8, R-X3: a delete over a join that returns its row to exactly one of two racing requests.
#[tokio::test(flavor = "multi_thread")]
async fn a_reset_token_is_consumed_exactly_once() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = new_user(&db, "ann").await?;
    let bob = new_user(&db, "bob").await?;
    // The database's clock sets the expiry.
    let issue = |user: Uuid, hash: &str, minutes_left: i64| {
        Insert::into("user_tokens")
            .set("user_id", user)
            .set("purpose", "password-reset")
            .set("token_hash", hash)
            .set("expires_at", now() + minutes(minutes_left))
    };
    db.insert(issue(ann, "reset-ann", 30)).await?;
    db.insert(issue(ann, "reset-old", -1)).await?;
    db.insert(issue(bob, "reset-bob", 30)).await?;
    db.insert(issue(bob, "reset-bob-2", 30)).await?;

    // Two requests carry the same token: one resets the password.
    let spend = || {
        let db = db.clone();
        async move { consume_reset_token(&db, "reset-ann", true).await }
    };
    let (a, b) = race(spend(), spend()).await;
    assert_eq!(sorted(vec![a?, b?]), [None, Some(ann)]);

    // Expired, or of a user who is deactivated or signs in elsewhere: no
    // row, and the token stays for the purge.
    assert_eq!(consume_reset_token(&db, "reset-old", true).await?, None);
    db.update(Update::table("users").key([bob]).set("auth_sub", "sub-bob")).await?;
    assert_eq!(consume_reset_token(&db, "reset-bob", false).await?, None);
    assert_eq!(consume_reset_token(&db, "reset-bob", true).await?, Some(bob));
    db.update(Update::table("users").key([bob]).set("deactivated_at", now())).await?;
    assert_eq!(consume_reset_token(&db, "reset-bob-2", true).await?, None);

    // The purge of expired tokens, then the rest of one user's.
    assert_eq!(db.delete(Delete::from("user_tokens").filter(col("expires_at").lt(now()))).await?.affected(), 1);
    let of_bob = Delete::from("user_tokens").index("user_tokens_user_idx", KeyRange::eq([bob])).filter(col("purpose").eq("password-reset"));
    assert_eq!(db.delete(of_bob).await?.affected(), 1);
    assert_eq!(rows_in(&db, Query::table("user_tokens")).await?, 0);
    Ok(())
}

// --------------------------------------------------- groups and invitations

/// A group with the number of its members.
fn groups_with_counts() -> Query {
    let members = Query::table("group_members").alias("gm").range(KeyRange::eq([col("g.id")])).select("n", count());
    Query::table("groups").alias("g").columns(["g.id", "g.name", "g.description", "g.is_default"]).select("member_count", scalar(members))
}

// R-Q9, R-Q10, R-Q11, R-S3, R-W11: groups with a counted subquery, a case-insensitive unique name, and members joined to their accounts.
#[tokio::test(flavor = "multi_thread")]
async fn groups_list_with_member_counts_and_refuse_a_taken_name() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = new_user(&db, "ann").await?;
    let bob = new_user(&db, "Bob").await?;
    let cat = new_user(&db, "cat").await?;

    // Creating groups; a name is taken whatever its case.
    let everyone: Uuid = db
        .insert(
            Insert::into("groups")
                .set("name", "Everyone")
                .set("description", "Every member")
                .set("is_default", true)
                .set("created_by", ann)
                .returning(["id"]),
        )
        .await?
        .one()?
        .get("id")?;
    let design: Uuid = db
        .insert(Insert::into("groups").set("name", "design").set("description", null()).set("created_by", ann).returning(["id"]))
        .await?
        .one()?
        .get("id")?;
    let admins: Uuid = db.insert(Insert::into("groups").set("name", "Admins").set("created_by", ann).returning(["id"])).await?.one()?.get("id")?;
    let taken = db.insert(Insert::into("groups").set("name", "DESIGN").returning(["id"])).await.unwrap_err();
    assert!(taken.is_unique_violation() && taken.violates("groups_name_key"));

    // Members join; nobody is in a group twice.
    for (group, user) in [(everyone, ann), (everyone, bob), (everyone, cat), (design, cat), (design, bob)] {
        join_group(&db, group, user).await?;
    }
    let twice = db.insert(Insert::into("group_members").set("group_id", design).set("user_id", bob)).await.unwrap_err();
    assert!(twice.violates(PRIMARY));

    // The default group first, then by name whatever its case.
    let all = db.query(groups_with_counts().order_by(col("g.is_default").desc()).order_by(col("g.name").lower())).await?;
    assert_eq!(column::<String>(&all, "name"), ["Everyone", "Admins", "design"]);
    assert_eq!(column::<i64>(&all, "member_count"), [3, 0, 2]);
    let one = db.query_one(groups_with_counts().key([design])).await?;
    assert_eq!((one.get::<i64>("member_count")?, one.get::<bool>("is_default")?, one.is_null("description")?), (2, false, true));

    // A rename onto another group's name fails and changes nothing.
    let rename = |name: Option<&str>, description: Option<Option<&str>>| {
        let group = Update::table("groups").key([design]).set("name", coalesce([val(name), col("name")]));
        group.set("description", when(val(description.is_some()), val(description.flatten()), col("description"))).set("updated_at", now())
    };
    let clash = db.update(rename(Some("admins"), Some(Some("x")))).await.unwrap_err();
    assert!(clash.violates("groups_name_key"));
    db.update(rename(None, Some(Some("Pixels")))).await?;
    let row = db.get("groups", [design]).await?.expect("the group");
    assert_eq!((row.get::<String>("name")?, row.get::<String>("description")?), ("design".to_string(), "Pixels".to_string()));
    // Its own name in another case is no conflict with itself.
    db.update(rename(Some("Design"), None)).await?;

    // The members of one group, and of several.
    let members = db.query(Query::table("group_members").range(KeyRange::eq([design])).columns(["user_id"])).await?;
    assert_eq!(sorted(column::<Uuid>(&members, "user_id")), sorted(vec![bob, cat]));
    let of_both = db.query(Query::table("group_members").keys([[admins], [design]]).columns(["user_id"])).await?;
    assert_eq!(of_both.len(), 2);

    // A page of members with their accounts, by name.
    let page = Query::table("group_members")
        .alias("gm")
        .range(KeyRange::eq([everyone]))
        .join(Query::table("users").alias("u").key([col("gm.user_id")]))
        .order_by(col("u.name").lower())
        .order_by(col("u.id"))
        .columns(["u.id", "u.name", "u.email"]);
    assert_eq!(column::<String>(&db.query(page.clone().limit(2)).await?, "name"), ["ann", "Bob"]);
    assert_eq!(column::<String>(&db.query(page.limit(2).offset(2)).await?, "name"), ["cat"]);

    // A member leaves, once; deleting a group takes its memberships.
    assert_eq!(db.delete(Delete::from("group_members").key([design, bob])).await?.affected(), 1);
    assert_eq!(db.delete(Delete::from("group_members").key([design, bob])).await?.affected(), 0);
    assert_eq!(db.delete(Delete::from("groups").key([everyone])).await?.affected(), 1);
    assert_eq!(rows_in(&db, Query::table("group_members")).await?, 1);
    Ok(())
}

// R-Q6, R-W3, R-W5: inserts fed by a query, skipping the rows that conflict.
#[tokio::test(flavor = "multi_thread")]
async fn members_join_groups_from_a_query_that_skips_who_is_already_in() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = new_user(&db, "ann").await?;
    let bob = new_user(&db, "bob").await?;
    let cat = new_user(&db, "cat").await?;
    let everyone: Uuid =
        db.insert(Insert::into("groups").set("name", "Everyone").set("is_default", true).returning(["id"])).await?.one()?.get("id")?;
    let design = new_group(&db, "design").await?;
    let sales = new_group(&db, "sales").await?;
    join_group(&db, design, ann).await?;

    // Add several users to a group. Reading the users first keeps
    // only ids that exist, so no foreign key can fail; who is in already
    // is skipped and not counted.
    let add = |group: Uuid, users: &[Uuid]| {
        let people = Query::table("users").keys(users.iter().map(|id| [*id])).select("group_id", val(group)).select("user_id", col("id"));
        Insert::into("group_members").columns(["group_id", "user_id"]).from_query(people).on_conflict_do_nothing()
    };
    assert_eq!(db.insert(add(design, &[ann, bob, uuid(404)])).await?.affected(), 1);
    assert_eq!(db.insert(add(design, &[ann, bob])).await?.affected(), 0);
    assert_eq!(rows_in(&db, Query::table("group_members").range(KeyRange::eq([design]))).await?, 2);

    // Of the groups an invitation names, the ones that exist and are
    // not a default group.
    let invited = db.query(Query::table("groups").keys([[sales], [everyone], [uuid(404)]]).filter(col("is_default").not()).columns(["id"])).await?;
    assert_eq!(ids(&invited), [sales]);

    // The same insert fed otherwise: a new user joins the default groups,
    // alone or together with the groups they were invited to.
    let join = |user: Uuid, also: &[Uuid]| {
        let groups = Query::table("groups")
            .filter(col("is_default").eq(true).or(col("id").in_list(uuids(also))))
            .select("group_id", col("id"))
            .select("user_id", val(user));
        Insert::into("group_members").columns(["group_id", "user_id"]).from_query(groups).on_conflict_do_nothing()
    };
    assert_eq!(db.insert(join(cat, &[])).await?.affected(), 1);
    assert_eq!(db.insert(join(cat, &[sales, design])).await?.affected(), 2);
    let cats = db.query(Query::table("group_members").index("group_members_user_idx", KeyRange::eq([cat])).columns(["group_id"])).await?;
    assert_eq!(sorted(column::<Uuid>(&cats, "group_id")), sorted(vec![everyone, design, sales]));
    Ok(())
}

/// Invites an address, or replaces every field of the invitation it
/// already has. The address is unique whatever its case.
fn invite(email: &str, role: &str, groups: &[Uuid], token_hash: &str, inviter: Uuid) -> Insert {
    Insert::into("invitations")
        .set("email", email)
        .set("role", role)
        .set("group_ids", uuids(groups))
        .set("token_hash", token_hash)
        .set("invited_by", inviter)
        .on_conflict_update("invitations_email_key")
        .update_excluded(["role", "group_ids", "token_hash", "invited_by"])
        .update("created_at", now())
        .returning(["id"])
}

// R-Q8, R-Q9, R-T7, R-W4, R-W7, R-X3: an upsert on a case-folded unique index, and a delete that hands its row to one of two racing transactions.
#[tokio::test(flavor = "multi_thread")]
async fn an_invitation_is_replaced_by_email_and_claimed_once() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = new_user(&db, "ann").await?;
    let bob = new_user(&db, "bob").await?;
    let design = new_group(&db, "design").await?;
    let sales = new_group(&db, "sales").await?;

    let first: Uuid = db.insert(invite("New@Example.com", "member", &[design], "hash-1", ann)).await?.one()?.get("id")?;
    db.update(Update::table("invitations").key([first]).set("created_at", now() - days(20))).await?;
    let other: Uuid = db.insert(invite("other@example.com", "member", &[], "hash-o", bob)).await?.one()?.get("id")?;

    // An invitation is good for fourteen days from when it was sent.
    let pending = |id: Uuid| {
        Query::table("invitations")
            .key([id])
            .filter(col("created_at").gt(now() - days(14)))
            .columns(["id", "email", "role", "group_ids", "token_hash", "invited_by"])
    };
    assert!(db.query_opt(pending(first)).await?.is_none());

    // Inviting the address again, in another case, is the same
    // invitation with new terms, sent now.
    let again = db.insert(invite("new@example.COM", "admin", &[sales, design], "hash-2", bob)).await?;
    assert_eq!(again.affected(), 1);
    assert_eq!(again.one()?.get::<Uuid>("id")?, first);
    let row = db.query_one(pending(first)).await?;
    assert_eq!(row.get::<String>("email")?, "New@Example.com");
    assert_eq!(
        (row.get::<String>("role")?, row.get::<String>("token_hash")?, row.get::<Uuid>("invited_by")?),
        ("admin".to_string(), "hash-2".to_string(), bob)
    );
    assert_eq!(row.get::<Vec<Uuid>>("group_ids")?, [sales, design]);
    assert_eq!(rows_in(&db, Query::table("invitations")).await?, 2);
    let bad_role = db.insert(invite("x@example.com", "owner", &[], "hash-x", ann)).await.unwrap_err();
    assert_eq!(bad_role.code, ErrorCode::CheckViolation);

    // Rotating the token; no row back means there is no such
    // invitation any more.
    let rotate = |id: Uuid| Update::table("invitations").key([id]).set("token_hash", "hash-3").set("created_at", now()).returning(["id", "email"]);
    assert_eq!(db.update(rotate(first)).await?.one()?.get::<String>("email")?, "New@Example.com");
    assert!(db.update(rotate(uuid(404))).await?.is_empty());

    // The list, newest first, with who invited; the inviter may have
    // been deleted since.
    db.update(Update::table("invitations").key([other]).set("created_at", now() - hours(1))).await?;
    db.delete(Delete::from("users").key([bob])).await?;
    let list = Query::table("invitations")
        .alias("i")
        .left_join(Query::table("users").alias("u").key([col("i.invited_by")]))
        .order_by(col("i.created_at").desc())
        .columns(["i.id", "i.email", "i.role", "i.group_ids", "i.created_at"])
        .select("inviter_id", col("u.id"))
        .select("inviter_name", col("u.name"));
    let list = db.query(list).await?;
    assert_eq!(ids(&list), [first, other]);
    assert_eq!(column::<Option<String>>(&list, "inviter_name"), [None, None]);
    db.update(Update::table("invitations").key([other]).set("invited_by", ann)).await?;

    // Accepting deletes the invitation, and only the request that got the
    // row back goes on to create the account.
    let accept = |id: Uuid| {
        let db = db.clone();
        async move {
            db.transaction(|tx| async move {
                let Some(claimed) = tx.delete(Delete::from("invitations").key([id]).returning(["id", "email", "role", "group_ids"])).await?.opt()
                else {
                    return Ok(None);
                };
                let email: String = claimed.get("email")?;
                if tx.query_opt(user_by_email(&email, true).columns(["id"])).await?.is_some() {
                    return Ok(None);
                }
                let user: Uuid = tx
                    .insert(Insert::into("users").set("email", email).set("role", claimed.get::<String>("role")?).returning(["id"]))
                    .await?
                    .one()?
                    .get("id")?;
                let groups = Query::table("groups")
                    .filter(col("id").in_list(claimed.value("group_ids")?.clone()))
                    .select("group_id", col("id"))
                    .select("user_id", val(user));
                tx.insert(Insert::into("group_members").columns(["group_id", "user_id"]).from_query(groups).on_conflict_do_nothing()).await?;
                Ok::<_, DbError>(Some(user))
            })
            .await
        }
    };
    let (a, b) = race(accept(first), accept(first)).await;
    let (a, b) = (a?, b?);
    let joined = a.or(b).expect("one of the two accepted");
    assert!(a.is_none() || b.is_none());
    assert_eq!(rows_in(&db, Query::table("users").index("users_email_key", KeyRange::eq(["new@example.com"]))).await?, 1);
    assert_eq!(rows_in(&db, Query::table("group_members").index("group_members_user_idx", KeyRange::eq([joined]))).await?, 2);
    assert_eq!(db.delete(Delete::from("invitations").key([other])).await?.affected(), 1);
    Ok(())
}

// ------------------------------------------------------------------ spaces

/// A space with the caller's role in it and its number of grants.
fn spaces_for(user: Uuid) -> Query {
    let members = Query::table("space_members").alias("sm").index("space_members_space_idx", KeyRange::eq([col("s.id")])).select("n", count());
    Query::table("spaces")
        .alias("s")
        .columns(["s.id", "s.name", "s.slug", "s.description", "s.icon", "s.settings", "s.created_at"])
        .select("rank", role_rank(col("s.id"), val(user)))
        .select("member_count", scalar(members))
}

// R-Q3, R-Q10, R-Q11, R-S6: correlated subqueries for a role and a count, and a cascade that changes who can read what.
#[tokio::test(flavor = "multi_thread")]
async fn a_space_carries_the_callers_highest_role_and_its_member_count() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = new_user(&db, "ann").await?;
    let bob = new_user(&db, "bob").await?;
    let cat = new_user(&db, "cat").await?;
    let eng = new_group(&db, "eng").await?;
    join_group(&db, eng, bob).await?;
    let alpha = new_space(&db, "Alpha").await?;
    let beta = new_space(&db, "beta").await?;
    let gamma = new_space(&db, "Gamma").await?;
    grant(&db, alpha, ann, "admin").await?;
    grant_group(&db, alpha, eng, "writer").await?;
    grant(&db, beta, bob, "reader").await?;
    grant_group(&db, beta, eng, "admin").await?;
    grant(&db, gamma, ann, "writer").await?;

    // Every space, by name whatever its case: what an admin sees.
    let by_name = |spaces: Query| spaces.order_by(col("s.name").lower()).order_by(col("s.id"));
    let all = db.query(by_name(spaces_for(bob))).await?;
    assert_eq!(column::<String>(&all, "name"), ["Alpha", "beta", "Gamma"]);
    // Bob writes in Alpha through his group; in beta the group's grant
    // outranks his own.
    assert_eq!(column::<Option<i64>>(&all, "rank"), [Some(2), Some(3), None]);
    assert_eq!(column::<i64>(&all, "member_count"), [2, 2, 1]);

    // The spaces a member sees: SQL filters a derived table on the
    // computed column; here the filter says the same expression.
    let mine = |user: Uuid| by_name(spaces_for(user).filter(role_rank(col("s.id"), val(user)).is_not_null()));
    assert_eq!(column::<String>(&db.query(mine(bob)).await?, "name"), ["Alpha", "beta"]);
    assert_eq!(column::<String>(&db.query(mine(ann)).await?, "name"), ["Alpha", "Gamma"]);
    assert!(db.query(mine(cat)).await?.is_empty());

    // By id, and by slug whatever its case.
    assert_eq!(db.query_one(spaces_for(ann).key([gamma])).await?.get::<i64>("rank")?, 2);
    let by_slug = db.query_one(spaces_for(ann).index("spaces_slug_key", KeyRange::eq(["alpha"]))).await?;
    assert_eq!((by_slug.get::<Uuid>("id")?, by_slug.get::<i64>("rank")?), (alpha, 3));

    // A missing space is no row; a space without a role for the
    // caller is a row with no rank.
    let rank_in = |space: Uuid, user: Uuid| {
        Query::table("spaces").alias("s").key([space]).select("id", col("s.id")).select("rank", role_rank(col("s.id"), val(user)))
    };
    assert!(db.query_opt(rank_in(uuid(404), bob)).await?.is_none());
    assert!(db.query_one(rank_in(gamma, bob)).await?.is_null("rank")?);
    assert_eq!(db.query_one(rank_in(beta, bob)).await?.get::<i64>("rank")?, 3);

    // Every space a user can read, each once.
    let can_read =
        |user: Uuid| Query::table("spaces").alias("s").filter(col("s.id").in_list(readable_spaces(val(user)))).select("space_id", col("s.id"));
    let spaces_of = |rows: Rows| sorted(column::<Uuid>(&rows, "space_id"));
    assert_eq!(spaces_of(db.query(can_read(bob)).await?), sorted(vec![alpha, beta]));
    assert_eq!(spaces_of(db.query(can_read(ann)).await?), sorted(vec![alpha, gamma]));
    assert!(db.query(can_read(cat)).await?.is_empty());

    // Deleting the group takes its memberships and its grants with
    // it, which silently changes who can read what.
    assert_eq!(db.delete(Delete::from("groups").key([eng])).await?.affected(), 1);
    assert_eq!(rows_in(&db, Query::table("group_members")).await?, 0);
    assert_eq!(rows_in(&db, Query::table("space_members").filter(col("group_id").is_not_null())).await?, 0);
    assert_eq!(column::<Option<i64>>(&db.query(by_name(spaces_for(bob))).await?, "rank"), [None, Some(1), None]);
    assert_eq!(spaces_of(db.query(can_read(bob)).await?), [beta]);
    Ok(())
}

// R-S3, R-W11, R-X1: a space and its first grant in one transaction, a unique slug, and a setting that drops rows when it changes.
#[tokio::test(flavor = "multi_thread")]
async fn a_taken_slug_fails_the_whole_space_creation() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let ann = new_user(&db, "ann").await?;
    db.insert(Insert::into("workspace").set("name", "Acme")).await?;

    // The space and its creator's grant, or neither.
    let create = |name: &'static str, slug: &'static str| {
        let db = db.clone();
        async move {
            db.transaction(|tx| async move {
                let space: Uuid = tx
                    .insert(
                        Insert::into("spaces")
                            .set("name", name)
                            .set("slug", slug)
                            .set("description", null())
                            .set("created_by", ann)
                            .returning(["id"]),
                    )
                    .await?
                    .one()?
                    .get("id")?;
                tx.insert(Insert::into("space_members").set("space_id", space).set("user_id", ann).set("role", "admin").set("added_by", ann)).await?;
                Ok::<_, DbError>(space)
            })
            .await
        }
    };
    let docs = create("Docs", "Docs").await?;
    let taken = create("Other docs", "docs").await.unwrap_err();
    assert_eq!(taken.code, ErrorCode::UniqueViolation);
    assert!(taken.violates("spaces_slug_key"));
    let wiki = create("Wiki", "wiki").await?;
    assert_eq!(rows_in(&db, Query::table("spaces")).await?, 2);
    assert_eq!(rows_in(&db, Query::table("space_members")).await?, 2);

    // A patch that takes another space's slug changes nothing.
    let patch = |space: Uuid, slug: Option<&str>, settings: Json| {
        let update =
            Update::table("spaces").key([space]).set("name", coalesce([null(), col("name")])).set("slug", coalesce([val(slug), col("slug")]));
        update
            .set("description", when(val(true), val("Handbook"), col("description")))
            .set("settings", col("settings").json_merge(settings))
            .set("updated_at", now())
    };
    let clash = db.update(patch(wiki, Some("DOCS"), json!({"sharingDisabled": true}))).await.unwrap_err();
    assert!(clash.violates("spaces_slug_key"));
    let row = db.get("spaces", [wiki]).await?.expect("the space");
    assert_eq!((row.get::<String>("slug")?, row.get::<Json>("settings")?, row.is_null("description")?), ("wiki".to_string(), json!({}), true));

    // Setting the icon and clearing it.
    db.update(Update::table("spaces").key([docs]).set("icon", "icon.png").set("updated_at", now())).await?;
    db.update(Update::table("spaces").key([docs]).set("icon", null()).set("updated_at", now())).await?;

    let in_docs = new_page(&db, docs, None, "a0", "Shared in docs").await?;
    let in_wiki = new_page(&db, wiki, None, "a0", "Shared in wiki").await?;
    new_share(&db, in_docs, "docs-key", false).await?;
    new_share(&db, in_wiki, "wiki-key", false).await?;

    // Sharing is on where neither the workspace nor the space
    // turned it off.
    let enabled = |space: Uuid| Query::table("spaces").alias("s").key([space]).select("ok", sharing_enabled(col("s.settings")));
    assert!(db.query_one(enabled(wiki)).await?.get::<bool>("ok")?);

    // Turning sharing off in a space drops the space's public links in
    // the same transaction: the patch, then a delete by subquery.
    db.transaction(|tx| async move {
        tx.update(patch(wiki, None, json!({"sharingDisabled": true}))).await?;
        let pages = Query::table("pages").index("pages_tree_idx", KeyRange::eq([wiki])).select("id", col("id"));
        let dropped = tx.delete(Delete::from("shares").filter(col("page_id").in_list(list_of(pages)))).await?;
        assert_eq!(dropped.affected(), 1);
        Ok::<_, DbError>(())
    })
    .await?;
    assert_eq!(column::<String>(&db.query(Query::table("shares").columns(["key"])).await?, "key"), ["docs-key"]);
    assert!(!db.query_one(enabled(wiki)).await?.get::<bool>("ok")?);
    assert!(db.query_one(enabled(docs)).await?.get::<bool>("ok")?);

    // A merged null is stored as a null, and a null setting reads as off.
    db.update(patch(wiki, None, json!({"sharingDisabled": null}))).await?;
    assert_eq!(db.get("spaces", [wiki]).await?.expect("the space").get::<Json>("settings")?, json!({"sharingDisabled": null}));
    assert!(db.query_one(enabled(wiki)).await?.get::<bool>("ok")?);
    db.update(Update::table("workspace").key([1]).set("settings", col("settings").json_merge(json!({"sharingDisabled": true})))).await?;
    assert!(!db.query_one(enabled(wiki)).await?.get::<bool>("ok")?);
    assert!(!db.query_one(enabled(docs)).await?.get::<bool>("ok")?);
    Ok(())
}

/// Gives users a role in a space; those who have one get the new
/// one. The conflict target is the unique index over user grants only.
fn add_users(space: Uuid, users: &[Uuid], role: &str, by: Uuid) -> Insert {
    let rows = Query::table("users")
        .keys(users.iter().map(|id| [*id]))
        .select("space_id", val(space))
        .select("user_id", col("id"))
        .select("role", val(role))
        .select("added_by", val(by));
    Insert::into("space_members")
        .columns(["space_id", "user_id", "role", "added_by"])
        .from_query(rows)
        .on_conflict_update("space_members_user_key")
        .update("role", excluded("role"))
}

fn add_groups(space: Uuid, groups: &[Uuid], role: &str, by: Uuid) -> Insert {
    let rows = Query::table("groups")
        .keys(groups.iter().map(|id| [*id]))
        .select("space_id", val(space))
        .select("group_id", col("id"))
        .select("role", val(role))
        .select("added_by", val(by));
    Insert::into("space_members")
        .columns(["space_id", "group_id", "role", "added_by"])
        .from_query(rows)
        .on_conflict_update("space_members_group_key")
        .update("role", excluded("role"))
}

// R-Q9, R-Q13, R-S3, R-S7, R-W3, R-W4: upserts fed by a query onto partial unique indexes, checks, and a list of nested objects.
#[tokio::test(flavor = "multi_thread")]
async fn space_members_are_upserted_on_partial_unique_indexes() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let (ann, bob, space) = (w.ann, w.bob, w.space);
    let cat = new_user(&db, "cat").await?;
    let design = new_group(&db, "Design").await?;
    let sales = new_group(&db, "sales").await?;
    join_group(&db, design, cat).await?;
    join_group(&db, design, bob).await?;
    let anns_grant: Uuid =
        db.query_one(Query::table("space_members").index("space_members_user_key", KeyRange::eq([space, ann]))).await?.get("id")?;

    // Users, then groups, in one transaction. Ann is in already: her
    // grant keeps its id and who added her, and takes the new role.
    let added = db
        .transaction(|tx| async move {
            let users = tx.insert(add_users(space, &[ann, bob, uuid(404)], "writer", ann)).await?.affected();
            let groups = tx.insert(add_groups(space, &[design, sales], "reader", ann)).await?.affected();
            Ok::<_, DbError>((users, groups))
        })
        .await?;
    assert_eq!(added, (2, 2));
    let anns = db.get("space_members", [anns_grant]).await?.expect("ann's grant");
    assert_eq!((anns.get::<String>("role")?, anns.is_null("added_by")?), ("writer".to_string(), true));
    assert_eq!(db.insert(add_groups(space, &[design], "admin", bob)).await?.affected(), 1);
    assert_eq!(rows_in(&db, Query::table("space_members").index("space_members_space_idx", KeyRange::eq([space]))).await?, 4);

    // A grant names a user or a group, never both and never neither, and
    // one of three roles.
    let both = db
        .insert(Insert::into("space_members").set("space_id", space).set("user_id", cat).set("group_id", sales).set("role", "reader"))
        .await
        .unwrap_err();
    assert_eq!(both.code, ErrorCode::CheckViolation);
    assert!(both.violates("space_members_check"));
    let neither = db.insert(Insert::into("space_members").set("space_id", space).set("role", "reader")).await.unwrap_err();
    assert!(neither.violates("space_members_check"));
    let owner = db.insert(add_users(space, &[cat], "owner", ann)).await.unwrap_err();
    assert!(owner.violates("space_members_role_check"));
    // Outside an upsert the partial unique index simply refuses a second
    // grant; a group's grants never collide with a user's.
    let twice = db.insert(Insert::into("space_members").set("space_id", space).set("user_id", bob).set("role", "reader")).await.unwrap_err();
    assert!(twice.violates("space_members_user_key"));

    // The member list. Groups first, then by the name of the group
    // or the user, each as a nested object.
    let member_count = scalar(Query::table("group_members").alias("gm").range(KeyRange::eq([col("g.id")])).select("n", count()));
    let user = object([
        ("id", col("u.id")),
        ("email", col("u.email")),
        ("name", col("u.name")),
        ("avatar", json_or_null(col("u.avatar"))),
        ("role", col("u.role")),
    ]);
    let group = object([("id", col("g.id")), ("name", col("g.name")), ("is_default", col("g.is_default")), ("member_count", member_count)]);
    let members = Query::table("space_members")
        .alias("sm")
        .index("space_members_space_idx", KeyRange::eq([space]))
        .left_join(Query::table("users").alias("u").key([col("sm.user_id")]))
        .left_join(Query::table("groups").alias("g").key([col("sm.group_id")]))
        .order_by(col("sm.group_id").is_null())
        .order_by(coalesce([col("g.name"), col("u.name")]).lower())
        .order_by(col("sm.id"))
        .columns(["sm.id", "sm.role"])
        .select("user", when(col("u.id").is_null(), null(), user))
        .select("group", when(col("g.id").is_null(), null(), group));
    let members = db.query(members).await?;
    assert_eq!(column::<String>(&members, "role"), ["admin", "reader", "writer", "writer"]);
    let listed = members.into_vec();
    assert_eq!(listed[0].get::<Json>("group")?, json!({"id": design.to_string(), "name": "Design", "is_default": false, "member_count": 2}));
    assert!(listed[0].is_null("user")?);
    assert_eq!(listed[1].get::<Json>("group")?["member_count"], json!(0));
    assert_eq!(
        listed[2].get::<Json>("user")?,
        json!({"id": ann.to_string(), "email": "ann@example.com", "name": "ann", "avatar": null, "role": "member"})
    );
    assert!(listed[2].is_null("group")?);
    assert_eq!(listed[3].get::<Json>("user")?["name"], json!("bob"));

    // The users a set of grants reaches, directly or through a group. A
    // UNION in Postgres: two reads here, and a set.
    let grants = ids(&db.query(Query::table("space_members").index("space_members_space_idx", KeyRange::eq([space])).columns(["id"])).await?);
    let direct = Query::table("space_members").keys(grants.iter().map(|id| [*id])).filter(col("user_id").is_not_null()).columns(["user_id"]);
    let through_groups = Query::table("space_members")
        .alias("sm")
        .keys(grants.iter().map(|id| [*id]))
        .join(Query::table("group_members").alias("gm").range(KeyRange::eq([col("sm.group_id")])))
        .select("user_id", col("gm.user_id"));
    let mut reached: BTreeSet<Uuid> = column::<Uuid>(&db.query(direct).await?, "user_id").into_iter().collect();
    reached.extend(column::<Uuid>(&db.query(through_groups).await?, "user_id"));
    assert_eq!(reached, BTreeSet::from([ann, bob, cat]));
    Ok(())
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Guarded {
    Done,
    LastAdmin,
    NotFound,
}

/// Changes a grant's role (`Some`) or removes the grant (`None`), unless
/// the space would be left without an admin. A lock on that one row
/// would still let two admins remove each other at the same moment; a
/// writing transaction counts with nobody in between.
async fn change_grant(db: &Client, space: Uuid, member: Uuid, role: Option<&str>) -> DbResult<Guarded> {
    db.transaction(|tx| async move {
        // The grant, if it is one of this space's.
        let Some(current) =
            tx.query_opt(Query::table("space_members").key([member]).filter(col("space_id").eq(space)).columns(["id", "role"])).await?
        else {
            return Ok(Guarded::NotFound);
        };
        if current.get::<String>("role")? == "admin" && role != Some("admin") {
            // How many admins the space has.
            let admins: i64 = tx
                .query_one(
                    Query::table("space_members")
                        .index("space_members_space_idx", KeyRange::eq([space]))
                        .filter(col("role").eq("admin"))
                        .select("admins", count()),
                )
                .await?
                .get("admins")?;
            if admins <= 1 {
                return Ok(Guarded::LastAdmin);
            }
        }
        // The change itself.
        match role {
            Some(role) => tx.update(Update::table("space_members").key([member]).set("role", role)).await?,
            None => tx.delete(Delete::from("space_members").key([member])).await?,
        };
        Ok::<_, DbError>(Guarded::Done)
    })
    .await
}

/// Gives a user another workspace role unless that leaves no active
/// owner. Postgres needs an advisory lock to serialise these; one writing
/// transaction counts and writes with nobody in between.
async fn change_role(db: &Client, user: Uuid, role: &str) -> DbResult<bool> {
    db.transaction(|tx| async move {
        let current = tx.query_one(Query::table("users").key([user]).columns(["role", "deactivated_at"])).await?;
        if current.get::<String>("role")? == "owner" && role != "owner" {
            // How many active owners there are.
            let owners = rows_in(&tx, Query::table("users").filter(col("role").eq("owner").and(col("deactivated_at").is_null()))).await?;
            if owners <= 1 {
                return Ok(false);
            }
        }
        // The new role.
        let changed = tx.update(Update::table("users").key([user]).set("role", role).set("updated_at", now()).returning(USER_COLUMNS)).await?;
        Ok::<_, DbError>(changed.affected() == 1)
    })
    .await
}

// R-S7, R-W6, R-X1, R-X2, R-X3: count-then-write guards that hold when two transactions race.
#[tokio::test(flavor = "multi_thread")]
async fn the_last_owner_and_the_last_admin_of_a_space_stay() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let (ann, bob) = (w.ann, w.bob);
    db.update(Update::table("users").filter(col("id").in_list(uuids(&[ann, bob]))).set("role", "owner")).await?;

    // Both owners of the workspace step down at once. Whoever goes
    // second finds one owner left, themselves, and stays.
    let demote = |user: Uuid| {
        let db = db.clone();
        async move { change_role(&db, user, "member").await }
    };
    let (a, b) = race(demote(ann), demote(bob)).await;
    assert_eq!(sorted(vec![a?, b?]), [false, true]);
    assert_eq!(rows_in(&db, Query::table("users").filter(col("role").eq("owner"))).await?, 1);
    // The role's check holds whoever asks.
    let bad = db.update(Update::table("users").key([ann]).set("role", "root")).await.unwrap_err();
    assert_eq!(bad.code, ErrorCode::CheckViolation);
    assert!(bad.violates("users_role_check"));
    // The same update by provider subject, nothing returned.
    db.update(Update::table("users").key([bob]).set("auth_sub", "sub-bob")).await?;
    let by_subject = db
        .update(Update::table("users").index("users_auth_sub_key", KeyRange::eq(["sub-bob"])).set("role", "admin").set("updated_at", now()))
        .await?;
    assert_eq!((by_subject.affected(), by_subject.len()), (1, 0));

    // The same guard on the admins of a space.
    let anns =
        db.query_one(Query::table("space_members").index("space_members_user_key", KeyRange::eq([w.space, w.ann]))).await?.get::<Uuid>("id")?;
    let bobs = grant(&db, w.space, w.bob, "admin").await?;
    let other_space = new_space(&db, "other").await?;

    assert_eq!(change_grant(&db, other_space, anns, None).await?, Guarded::NotFound);

    // Two admins are removed at the same moment: one removal goes
    // through, the other finds its target is now the last admin.
    let space = w.space;
    let remove = |member: Uuid| {
        let db = db.clone();
        async move { change_grant(&db, space, member, None).await }
    };
    let (a, b) = race(remove(anns), remove(bobs)).await;
    assert_eq!(sorted(vec![a?, b?]), [Guarded::Done, Guarded::LastAdmin]);
    let left = db.query(Query::table("space_members").index("space_members_space_idx", KeyRange::eq([space]))).await?;
    assert_eq!(column::<String>(&left, "role"), ["admin"]);
    let last: Uuid = left.one()?.get("id")?;

    // Nor can the last admin be demoted, until there is another.
    assert_eq!(change_grant(&db, space, last, Some("reader")).await?, Guarded::LastAdmin);
    let cat = new_user(&db, "cat").await?;
    let cats = grant(&db, space, cat, "reader").await?;
    assert_eq!(change_grant(&db, space, cats, Some("admin")).await?, Guarded::Done);
    assert_eq!(change_grant(&db, space, last, Some("reader")).await?, Guarded::Done);
    assert_eq!(change_grant(&db, space, last, None).await?, Guarded::Done);
    assert_eq!(rows_in(&db, Query::table("space_members").index("space_members_space_idx", KeyRange::eq([space]))).await?, 1);
    Ok(())
}

// R-S6, R-W10, R-X1: deleting a space cascades through everything under it, after a delete that returns its files' storage keys.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_space_returns_its_file_keys_and_takes_everything_under_it() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let (ann, bob, space) = (w.ann, w.bob, w.space);
    let kept_space = new_space(&db, "kept").await?;
    let kept_page = new_page(&db, kept_space, None, "a0", "Kept").await?;
    db.insert(Insert::into("workspace").set("default_space_id", space)).await?;

    let root = new_page(&db, space, None, "a0", "Root").await?;
    let child = new_page(&db, space, Some(root), "a0", "Child").await?;
    db.insert(Insert::into("page_versions").set("page_id", child).set("content", json!({"type": "doc"}))).await?;
    let comment: Uuid = db
        .insert(Insert::into("comments").set("page_id", child).set("author_id", ann).set("body", json!({})).returning(["id"]))
        .await?
        .one()?
        .get("id")?;
    db.insert(Insert::into("comments").set("page_id", child).set("parent_id", comment).set("body", json!({}))).await?;
    new_share(&db, root, "root-key", true).await?;
    db.insert(Insert::into("favorites").set("user_id", ann).set("page_id", child)).await?;
    db.insert(Insert::into("favorites").set("user_id", ann).set("space_id", space)).await?;
    db.insert(Insert::into("watchers").set("user_id", bob).set("page_id", root)).await?;
    db.insert(Insert::into("watchers").set("user_id", bob).set("space_id", space)).await?;
    let label: Uuid = db.insert(Insert::into("labels").set("name", "howto").returning(["id"])).await?.one()?.get("id")?;
    db.insert(Insert::into("page_labels").set("page_id", child).set("label_id", label)).await?;
    db.insert(Insert::into("page_links").set("source_id", child).set("target_id", kept_page)).await?;
    db.insert(Insert::into("page_links").set("source_id", kept_page).set("target_id", root)).await?;
    db.insert(Insert::into("synced_blocks").set("page_id", child).set("block_id", "b1").set("content", json!({}))).await?;
    db.insert(Insert::into("notifications").set("user_id", ann).set("kind", "comment.created").set("page_id", child).set("comment_id", comment))
        .await?;
    db.insert(Insert::into("base_properties").set("page_id", child).set("id", "p1").set("name", "Name").set("type", "text").set("position", "a0"))
        .await?;
    db.insert(Insert::into("base_rows").set("page_id", child).set("position", "a0")).await?;
    db.insert(Insert::into("base_views").set("page_id", child).set("name", "Table").set("type", "table").set("position", "a0")).await?;
    new_file(&db, child, space, "key-page-file").await?;
    db.insert(Insert::into("files").set("purpose", "space-icon").set("space_id", space).set("name", "icon").set("storage_key", "key-space-icon"))
        .await?;
    let kept_file = new_file(&db, kept_page, kept_space, "key-kept").await?;

    // One statement with two data-modifying CTEs in Postgres. The files
    // go first and by hand: their foreign keys only set NULL, and the
    // keys of their objects are needed to remove those.
    let keys = db
        .transaction(|tx| async move {
            let pages = Query::table("pages").index("pages_tree_idx", KeyRange::eq([space])).select("id", col("id"));
            let files = Delete::from("files").filter(col("space_id").eq(space).or(col("page_id").in_list(list_of(pages)))).returning(["storage_key"]);
            let keys: Vec<String> = tx.delete(files).await?.column()?;
            assert_eq!(tx.delete(Delete::from("spaces").key([space])).await?.affected(), 1);
            Ok::<_, DbError>(keys)
        })
        .await?;
    assert_eq!(sorted(keys), ["key-page-file", "key-space-icon"]);

    for (table, left) in [
        ("spaces", 1),
        ("space_members", 0),
        ("pages", 1),
        ("page_versions", 0),
        ("comments", 0),
        ("shares", 0),
        ("favorites", 0),
        ("watchers", 0),
        ("page_labels", 0),
        ("page_links", 0),
        ("synced_blocks", 0),
        ("notifications", 0),
        ("base_properties", 0),
        ("base_rows", 0),
        ("base_views", 0),
        ("files", 1),
        // What only pointed into the space stays.
        ("labels", 1),
        ("users", 2),
    ] {
        assert_eq!(rows_in(&db, Query::table(table)).await?, left, "{table}");
    }
    assert!(db.get("files", [kept_file]).await?.is_some());
    assert!(db.query_one(Query::table("workspace").key([1])).await?.is_null("default_space_id")?);
    Ok(())
}

// ------------------------------------------------- pages: lookups and lists

// R-Q1, R-Q2, R-Q6, R-Q10: a page by key or unique slug, projected with or without its document, with a correlated role.
#[tokio::test(flavor = "multi_thread")]
async fn a_page_loads_by_id_or_slug_with_the_callers_role() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let document = json!({"type": "doc", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Hello"}]}]});
    let page = page_with_slug("welcome001", w.space, None, "a0", "Welcome")
        .set("content", document.clone())
        .set("text_content", "Hello")
        .set("creator_id", w.ann)
        .set("contributor_ids", uuids(&[w.ann]));
    let page: Uuid = db.insert(page.returning(["id"])).await?.one()?.get("id")?;
    let child = new_page(&db, w.space, Some(page), "a0", "Child").await?;
    let trashed: Uuid =
        db.insert(page_insert(w.space, Some(child), "a0", "Trashed").set("deleted_at", now()).returning(["id"])).await?.one()?.get("id")?;

    // The access check behind almost every endpoint. The page, with
    // or without its document, and the caller's role in its space.
    let find = |pages: Query, user: Uuid| {
        page_columns(pages.alias("p")).select("content", col("p.content")).select("rank", role_rank(col("p.space_id"), val(user)))
    };
    let found = db.query_one(find(Query::table("pages").key([page]), w.ann)).await?;
    assert_eq!(found.get::<String>("title")?, "Welcome");
    assert_eq!(found.get::<Json>("content")?, document);
    assert_eq!((found.get::<i64>("rank")?, found.get::<i64>("doc_version")?, found.get::<bool>("has_children")?), (3, 0, true));
    assert_eq!(found.get::<Vec<Uuid>>("contributor_ids")?, [w.ann]);
    assert!(found.columns().iter().all(|name| name != "text_content"));

    let by_slug = db.query_one(find(Query::table("pages").index("pages_slug_id_key", KeyRange::eq(["welcome001"])), w.bob)).await?;
    assert_eq!(by_slug.get::<Uuid>("id")?, page);
    // No role is a row with no rank: the page exists, the caller may not see it.
    assert!(by_slug.is_null("rank")?);
    assert!(db.query_opt(find(Query::table("pages").key([uuid(404)]), w.ann)).await?.is_none());

    // The same without the document, where the caller only needs the check.
    let light =
        page_columns(Query::table("pages").alias("p").key([page])).select("content", null()).select("rank", role_rank(col("p.space_id"), val(w.ann)));
    assert!(db.query_one(light).await?.is_null("content")?);

    // The tree columns alone. A page whose only child is in the trash has none.
    let item = |id: Uuid| tree_columns(Query::table("pages").alias("p").key([id]));
    assert!(db.query_one(item(page)).await?.get::<bool>("has_children")?);
    assert!(!db.query_one(item(child)).await?.get::<bool>("has_children")?);

    // What a collaboration session loads, and its staleness probe.
    let loaded = db.query_one(Query::table("pages").key([page]).columns(["content", "doc_version", "space_id", "deleted_at"])).await?;
    assert_eq!((loaded.get::<Uuid>("space_id")?, loaded.is_null("deleted_at")?), (w.space, true));
    assert_eq!(db.query_one(Query::table("pages").key([page]).columns(["doc_version"])).await?.get::<i64>("doc_version")?, 0);

    // A parent must be a live page of the same space.
    let parent_ok =
        |id: Uuid, space: Uuid| Query::table("pages").key([id]).filter(col("space_id").eq(space).and(col("deleted_at").is_null())).columns(["id"]);
    assert!(db.query_opt(parent_ok(page, w.space)).await?.is_some());
    assert!(db.query_opt(parent_ok(trashed, w.space)).await?.is_none());
    assert!(db.query_opt(parent_ok(page, uuid(404))).await?.is_none());

    // What a public share shows, never a trashed page.
    let public = |pages: Query| {
        tree_columns(pages.alias("p").filter(col("p.deleted_at").is_null())).columns(["p.cover", "p.content", "p.created_at", "p.updated_at"])
    };
    assert!(db.query_opt(public(Query::table("pages").index("pages_slug_id_key", KeyRange::eq(["welcome001"])))).await?.is_some());
    assert!(db.query_opt(public(Query::table("pages").key([trashed]))).await?.is_none());

    // An export reads documents in batches.
    let batch = db.query(Query::table("pages").keys([[child], [page]]).columns(["id", "content"])).await?;
    assert_eq!(ids(&batch), [child, page]);
    assert!(batch.first().expect("the child").is_null("content")?);
    Ok(())
}

/// The sibling predicate: live pages of a space under one parent, or the
/// roots when there is none, leaving out a page that is being moved.
fn sibling(space: Uuid, parent: Option<Uuid>, moving: Option<Uuid>) -> Expr {
    all([
        col("p.space_id").eq(space),
        col("p.parent_id").is_not_distinct_from(parent),
        col("p.deleted_at").is_null(),
        col("p.id").is_distinct_from(moving),
    ])
}

/// The pages of a space that are siblings under `parent`.
fn siblings(space: Uuid, parent: Option<Uuid>, moving: Option<Uuid>) -> Query {
    Query::table("pages").alias("p").index("pages_tree_idx", KeyRange::eq([space])).filter(sibling(space, parent, moving))
}

/// The children of a node, or the roots, in sibling order.
fn children(space: Uuid, parent: Option<Uuid>) -> Query {
    tree_columns(siblings(space, parent, None)).order_by(col("p.position")).order_by(col("p.id"))
}

// R-Q2, R-Q4, R-Q11, R-T9: text ordered by its bytes, `IS NOT DISTINCT FROM` on a nullable parent, and `min`/`max` over siblings.
#[tokio::test(flavor = "multi_thread")]
async fn siblings_are_ordered_by_the_bytes_of_their_position_keys() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let space = w.space;
    let other_space = new_space(&db, "other").await?;
    new_page(&db, other_space, None, "A", "Elsewhere").await?;

    // Fractional position keys over [0-9A-Za-z] order by their bytes:
    // digits, then capitals, then small letters. Two inserts between the
    // same neighbours can get the same key; the id breaks the tie.
    let b = new_page(&db, space, None, "Z", "B").await?;
    let d = new_page(&db, space, None, "Zz", "D").await?;
    db.insert(page_insert(space, None, "a0", "A").set("id", uuid(2))).await?;
    db.insert(page_insert(space, None, "a0", "C").set("id", uuid(1))).await?;
    db.insert(page_insert(space, None, "0", "T").set("deleted_at", now())).await?;
    let small_a = new_page(&db, space, Some(b), "a", "B small a").await?;
    new_page(&db, space, Some(b), "1", "B one").await?;
    new_page(&db, space, Some(b), "A", "B capital A").await?;
    db.insert(page_insert(space, Some(d), "a0", "D trashed").set("deleted_at", now())).await?;

    // `IS NOT DISTINCT FROM` lets one query serve the roots, whose
    // parent is NULL, and the children of a page.
    let roots = db.query(children(space, None)).await?;
    assert_eq!(titles(&roots), ["B", "D", "C", "A"]);
    assert_eq!(column::<bool>(&roots, "has_children"), [true, false, false, false]);
    assert_eq!(titles(&db.query(children(space, Some(b))).await?), ["B one", "B capital A", "B small a"]);
    assert!(db.query(children(space, Some(d))).await?.is_empty());
    // Under a known parent the index already holds the children in this
    // order: no sort, and a limit stops the read.
    let first_two = Query::table("pages").index("pages_parent_idx", KeyRange::eq([b])).filter(col("deleted_at").is_null()).limit(2);
    assert_eq!(titles(&db.query(first_two).await?), ["B one", "B capital A"]);

    // A whole space for an export.
    let exported = Query::table("pages")
        .index("pages_tree_idx", KeyRange::eq([space]))
        .filter(col("deleted_at").is_null())
        .order_by(col("position"))
        .order_by(col("id"));
    let exported = db.query(exported.columns(["id", "slug_id", "parent_id", "title", "is_base"])).await?;
    assert_eq!(titles(&exported), ["B one", "B capital A", "B", "D", "B small a", "C", "A"]);

    // The last and the first key among siblings; none in an
    // empty place, which is still one row.
    let edge = |query: Query, of: fn(Expr) -> Expr| query.select("position", of(col("p.position")));
    let last = db.query_one(edge(siblings(space, None, None), max)).await?;
    assert_eq!(last.get::<Option<String>>("position")?.as_deref(), Some("a0"));
    let first = db.query_one(edge(siblings(space, None, None), min)).await?;
    assert_eq!(first.get::<Option<String>>("position")?.as_deref(), Some("Z"));
    assert!(db.query_one(edge(siblings(space, Some(d), None), max)).await?.is_null("position")?);
    // A page being moved does not count as its own neighbour; its twin
    // at the same key still does.
    let without_last = db.query_one(edge(siblings(space, Some(b), Some(small_a)), max)).await?;
    assert_eq!(without_last.get::<String>("position")?, "A");
    let without_c = db.query_one(edge(siblings(space, None, Some(uuid(1))), max)).await?;
    assert_eq!(without_c.get::<String>("position")?, "a0");

    // The key of a given sibling, then the next key after it.
    let position_of =
        |id: Uuid, parent: Option<Uuid>| Query::table("pages").alias("p").key([id]).filter(sibling(space, parent, None)).columns(["p.position"]);
    let after_b: String = db.query_one(position_of(b, None)).await?.get("position")?;
    let next = db.query_one(edge(siblings(space, None, None).filter(col("p.position").gt(after_b)), min)).await?;
    assert_eq!(next.get::<String>("position")?, "Zz");
    let after_last = db.query_one(edge(siblings(space, None, None).filter(col("p.position").gt("a0")), min)).await?;
    assert!(after_last.is_null("position")?);
    // A page that is not a sibling there is no row: a 404 for the caller.
    assert!(db.query_opt(position_of(b, Some(d))).await?.is_none());
    Ok(())
}

// R-Q4, R-Q5, R-Q7, R-Q9, R-Q10, R-S4: a paged listing off a partial descending index, joined, scoped by a subquery and filtered by pattern.
#[tokio::test(flavor = "multi_thread")]
async fn recent_pages_come_from_readable_spaces_newest_first() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let (ann, bob) = (w.ann, w.bob);
    db.update(Update::table("spaces").key([w.space]).set("icon", "docs.png")).await?;
    let private = new_space(&db, "private").await?;
    grant(&db, private, bob, "admin").await?;
    let team = new_group(&db, "team").await?;
    join_group(&db, team, ann).await?;
    let shared = new_space(&db, "shared").await?;
    grant_group(&db, shared, team, "reader").await?;

    let page = |n: u128, space: Uuid, title: &str, updated: &str, creator: Uuid, editor: Option<Uuid>| {
        let page = page_with_slug(&format!("slug-{n}"), space, None, "a0", title).set("id", uuid(n));
        page.set("updated_at", ts(updated)).set("creator_id", creator).set("last_editor_id", editor)
    };
    db.insert(page(1, w.space, "Roadmap", "2026-03-01T10:00:00Z", ann, Some(bob))).await?;
    db.insert(page(2, w.space, "Old roadmap", "2026-03-02T10:00:00Z", ann, Some(bob)).set("deleted_at", now())).await?;
    db.insert(page(3, w.space, "road trip", "2026-03-03T10:00:00Z", bob, None)).await?;
    db.insert(page(4, w.space, "Offroad", "2026-03-03T10:00:00Z", ann, Some(bob))).await?;
    db.insert(page(5, private, "Roadblocks", "2026-03-04T10:00:00Z", ann, Some(bob))).await?;
    db.insert(page(6, shared, "Shared notes", "2026-03-05T10:00:00Z", ann, Some(bob))).await?;

    // The index over live pages is already newest first with the id as
    // tie-break, so the page is cut without a sort. The optional filters
    // are `$x IS NULL OR col = $x`, as SQL would write them.
    let recent = |user: Uuid, space: Option<Uuid>, creator: Option<Uuid>| {
        let pages = Query::table("pages")
            .alias("p")
            .index("pages_updated_idx", KeyRange::all())
            .join(Query::table("spaces").alias("s").key([col("p.space_id")]))
            .left_join(Query::table("users").alias("u").key([col("p.last_editor_id")]))
            .filter(readable(user))
            .filter(val(space).is_null().or(col("p.space_id").eq(space)))
            .filter(val(creator).is_null().or(col("p.creator_id").eq(creator)));
        tree_columns(pages)
            .columns(["p.updated_at"])
            .select("space", space_ref())
            .select("editor_id", col("u.id"))
            .select("editor_name", col("u.name"))
    };
    let mine = db.query(recent(ann, None, None).limit(10)).await?;
    assert_eq!(numbers(&mine), [6, 3, 4, 1]);
    assert_eq!(column::<Option<String>>(&mine, "editor_name"), [Some("bob".to_string()), None, Some("bob".to_string()), Some("bob".to_string())]);
    assert_eq!(
        mine.first().expect("a row").get::<Json>("space")?,
        json!({"id": shared.to_string(), "name": "shared", "slug": "shared", "iconUrl": null})
    );
    assert_eq!(mine.iter().last().expect("a row").get::<Json>("space")?["iconUrl"], json!("/api/images/docs.png"));
    assert_eq!(numbers(&db.query(recent(ann, None, None).limit(2).offset(1)).await?), [3, 4]);
    assert_eq!(numbers(&db.query(recent(ann, Some(w.space), Some(ann)).limit(10)).await?), [4, 1]);
    assert_eq!(numbers(&db.query(recent(bob, None, None).limit(10)).await?), [5]);

    // Page titles that hold the needle, those that start with it first.
    let suggest = |user: Uuid, needle: &str| {
        let title = || col("p.title").or_else("").lower();
        let pages = Query::table("pages")
            .alias("p")
            .index("pages_updated_idx", KeyRange::all())
            .join(Query::table("spaces").alias("s").key([col("p.space_id")]));
        tree_columns(pages.filter(readable(user)).filter(title().like(format!("%{}%", like_escape(needle)))))
            .select("space", space_ref())
            .order_by(title().like(format!("{}%", like_escape(needle))).desc())
            .order_by(col("p.updated_at").desc())
            .order_by(col("p.id"))
            .limit(8)
    };
    assert_eq!(numbers(&db.query(suggest(ann, "road")).await?), [3, 1, 4]);
    assert!(db.query(suggest(ann, "roadblock")).await?.is_empty());

    // The pages some cells point at, as far as the user can read them.
    let resolve = |pages: Query, user: Uuid| {
        pages
            .alias("p")
            .join(Query::table("spaces").alias("s").key([col("p.space_id")]))
            .filter(readable(user))
            .columns(["p.id", "p.slug_id", "p.title", "p.icon"])
            .select("space_slug", col("s.slug"))
    };
    let asked = [uuid(5), uuid(1), uuid(2), uuid(404), uuid(6)];
    let resolved = db.query(resolve(Query::table("pages").keys(asked.map(|id| [id])), ann)).await?;
    assert_eq!(numbers(&resolved), [1, 6]);
    assert_eq!(column::<String>(&resolved, "space_slug"), ["docs", "shared"]);
    // The same by public slug id.
    let by_slug = Query::table("pages").index_ranges("pages_slug_id_key", ["slug-6", "slug-5", "slug-3"].map(|slug| KeyRange::eq([slug])));
    assert_eq!(numbers(&db.query(resolve(by_slug, ann)).await?), [6, 3]);

    // What a notification email says, with a value from another table.
    let mail = Query::table("pages")
        .alias("p")
        .key([uuid(1)])
        .join(Query::table("spaces").alias("s").key([col("p.space_id")]))
        .columns(["p.title", "p.slug_id"])
        .select("space_slug", col("s.slug"))
        .select("actor_name", scalar(Query::table("users").key([bob]).columns(["name"])));
    let mail = db.query_one(mail).await?;
    assert_eq!(
        (mail.get::<String>("title")?, mail.get::<String>("space_slug")?, mail.get::<String>("actor_name")?),
        ("Roadmap".to_string(), "docs".to_string(), "bob".to_string())
    );
    Ok(())
}

// R-Q9, R-S4, R-T3: a self left join off a partial index, comparing timestamps to the microsecond.
#[tokio::test(flavor = "multi_thread")]
async fn the_trash_lists_the_root_of_each_trashed_subtree() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let (t1, t2, t3) = (ts("2026-03-01T10:00:00.000001Z"), ts("2026-03-01T10:00:00.000002Z"), ts("2026-03-02T10:00:00Z"));
    let page = |n: u128, parent: Option<u128>, title: &str, deleted: Option<Timestamp>| {
        page_insert(w.space, parent.map(uuid), "a0", title).set("id", uuid(n)).set("deleted_at", deleted).set("deleted_by", deleted.map(|_| w.bob))
    };
    // A subtree trashed in one operation carries one timestamp.
    db.insert(page(1, None, "Trashed with its subtree", Some(t3))).await?;
    db.insert(page(2, Some(1), "Went with its parent", Some(t3))).await?;
    db.insert(page(3, Some(2), "Went with its grandparent", Some(t3))).await?;
    // A page trashed on its own under a live parent.
    db.insert(page(4, None, "Live", None)).await?;
    db.insert(page(5, Some(4), "Trashed alone", Some(t1))).await?;
    // A child trashed first, its parent a microsecond later: two entries.
    db.insert(page(6, None, "Trashed after its child", Some(t2))).await?;
    db.insert(page(7, Some(6), "Trashed before its parent", Some(t1))).await?;
    let elsewhere = new_space(&db, "elsewhere").await?;
    db.insert(page_insert(elsewhere, None, "a0", "Another space's trash").set("deleted_at", t3)).await?;

    // A trashed page is listed unless its parent went at the very
    // same instant. The index holds only trashed pages, by space.
    let trash = Query::table("pages")
        .alias("p")
        .index("pages_deleted_idx", KeyRange::eq([w.space]))
        .left_join(Query::table("pages").alias("parent").key([col("p.parent_id")]))
        .left_join(Query::table("users").alias("u").key([col("p.deleted_by")]))
        .filter(any([col("parent.id").is_null(), col("parent.deleted_at").is_null(), col("parent.deleted_at").ne(col("p.deleted_at"))]))
        .order_by(col("p.deleted_at").desc())
        .order_by(col("p.id"))
        .limit(1000);
    let trash =
        db.query(tree_columns(trash).columns(["p.deleted_at"]).select("deleter_id", col("u.id")).select("deleter_name", col("u.name"))).await?;
    assert_eq!(ids(&trash), [uuid(1), uuid(6), uuid(5), uuid(7)]);
    assert_eq!(column::<Timestamp>(&trash, "deleted_at"), [t3, t2, t1, t1]);
    assert_eq!(column::<String>(&trash, "deleter_name"), ["bob"; 4]);
    Ok(())
}

// ------------------------------------------------------ pages: the tree

/// A small tree for the walks: `root` over `a` over `b` over `c`, and a
/// second root `x`.
struct Tree {
    root: Uuid,
    a: Uuid,
    b: Uuid,
    c: Uuid,
    x: Uuid,
}

async fn tree(db: &impl Db, space: Uuid) -> DbResult<Tree> {
    let root = new_page(db, space, None, "a0", "root").await?;
    let a = new_page(db, space, Some(root), "a0", "a").await?;
    let b = new_page(db, space, Some(a), "a0", "b").await?;
    let c = new_page(db, space, Some(b), "a0", "c").await?;
    let x = new_page(db, space, None, "a1", "x").await?;
    Ok(Tree { root, a, b, c, x })
}

/// A page and the pages above it.
fn chain(page: Uuid) -> Query {
    Query::table("pages").alias("p").ancestors([[page]], "parent_id")
}

// R-Q19: a walk up the tree, with its depth, a stop condition and a depth guard.
#[tokio::test(flavor = "multi_thread")]
async fn breadcrumbs_run_from_the_root_down_to_the_page() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let t = tree(&db, w.space).await?;

    // The walk itself starts at the page (depth 0) and climbs.
    let climb = db.query(chain(t.c).columns(["p.id"]).select("depth", Expr::Depth)).await?;
    assert_eq!(ids(&climb), [t.c, t.b, t.a, t.root]);
    assert_eq!(column::<i64>(&climb, "depth"), [0, 1, 2, 3]);

    // Breadcrumbs are the same rows, root first.
    let crumbs = db.query(tree_columns(chain(t.c)).order_by(Expr::Depth.desc())).await?;
    assert_eq!(titles(&crumbs), ["root", "a", "b", "c"]);
    assert_eq!(column::<bool>(&crumbs, "has_children"), [true, true, true, false]);
    assert_eq!(titles(&db.query(tree_columns(chain(t.x)).order_by(Expr::Depth.desc())).await?), ["x"]);
    assert!(db.query(chain(uuid(404))).await?.is_empty());

    // The breadcrumbs of a public share end at the shared page;
    // what is above it is never read.
    let public = |page: Uuid, shared: Uuid| tree_columns(chain(page).stop_at(col("p.id").eq(shared))).order_by(Expr::Depth.desc());
    assert_eq!(titles(&db.query(public(t.c, t.a)).await?), ["a", "b", "c"]);
    assert_eq!(titles(&db.query(public(t.a, t.a)).await?), ["a"]);

    // The depth guard a recursive query in Postgres carries.
    assert_eq!(titles(&db.query(tree_columns(chain(t.c).max_depth(1))).await?), ["c", "b"]);
    Ok(())
}

#[derive(Debug, PartialEq)]
enum Moved {
    To(String),
    NoSuchParent,
    IntoItself,
}

/// A position key after `last`. An application computes a key between
/// two neighbours; the tests only append.
fn key_after(last: Option<String>) -> String {
    last.map_or("a0".to_string(), |key| format!("{key}V"))
}

/// Moves a page under another parent of its space, at the end.
async fn move_page(db: &Client, page: Uuid, space: Uuid, parent: Option<Uuid>) -> DbResult<Moved> {
    db.transaction(|tx| async move {
        if let Some(parent) = parent {
            // The parent must be a live page of the same space.
            if tx
                .query_opt(Query::table("pages").key([parent]).filter(col("space_id").eq(space).and(col("deleted_at").is_null())).columns(["id"]))
                .await?
                .is_none()
            {
                return Ok(Moved::NoSuchParent);
            }
            // The new parent may not be the page or anything under
            // it; that is, the page may not be among the parent's ancestors.
            if !tx.query(chain(parent).filter(col("p.id").eq(page)).columns(["p.id"])).await?.is_empty() {
                return Ok(Moved::IntoItself);
            }
        }
        // The last key among the new siblings.
        let last: Option<String> =
            tx.query_one(siblings(space, parent, Some(page)).select("position", max(col("p.position")))).await?.get("position")?;
        let position = key_after(last);
        // A move is not an edit: `updated_at` stays.
        tx.update(Update::table("pages").key([page]).set("parent_id", parent).set("position", position.as_str())).await?;
        Ok::<_, DbError>(Moved::To(position))
    })
    .await
}

// R-Q19, R-W6, R-X1: a move checked against the new parent's ancestors, in one transaction.
#[tokio::test(flavor = "multi_thread")]
async fn a_page_cannot_move_into_its_own_subtree() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let t = tree(&db, w.space).await?;
    let other_space = new_space(&db, "other").await?;
    let foreign = new_page(&db, other_space, None, "a0", "foreign").await?;
    let trashed: Uuid = db.insert(page_insert(w.space, None, "a2", "trashed").set("deleted_at", now()).returning(["id"])).await?.one()?.get("id")?;
    let before: Timestamp = db.get("pages", [t.a]).await?.expect("a").get("updated_at")?;

    assert_eq!(move_page(&db, t.a, w.space, Some(t.c)).await?, Moved::IntoItself);
    assert_eq!(move_page(&db, t.a, w.space, Some(t.a)).await?, Moved::IntoItself);
    assert_eq!(move_page(&db, t.a, w.space, Some(foreign)).await?, Moved::NoSuchParent);
    assert_eq!(move_page(&db, t.a, w.space, Some(trashed)).await?, Moved::NoSuchParent);
    assert_eq!(db.get("pages", [t.a]).await?.expect("a").get::<Uuid>("parent_id")?, t.root);

    // Under `x`, which has no children yet; then to the roots, after `x`.
    assert_eq!(move_page(&db, t.a, w.space, Some(t.x)).await?, Moved::To("a0".to_string()));
    assert_eq!(titles(&db.query(tree_columns(chain(t.c)).order_by(Expr::Depth.desc())).await?), ["x", "a", "b", "c"]);
    assert_eq!(move_page(&db, t.a, w.space, None).await?, Moved::To("a1V".to_string()));
    assert_eq!(titles(&db.query(children(w.space, None)).await?), ["root", "x", "a"]);
    // Moving it to where it already is leaves it last among the others.
    assert_eq!(move_page(&db, t.a, w.space, None).await?, Moved::To("a1V".to_string()));
    let after = db.get("pages", [t.a]).await?.expect("a");
    assert_eq!(after.get::<Timestamp>("updated_at")?, before);
    assert!(after.is_null("parent_id")?);

    // What was the forbidden move is fine the other way round.
    assert_eq!(move_page(&db, t.root, w.space, Some(t.c)).await?, Moved::To("a0".to_string()));
    Ok(())
}

/// Whether the public share of `shared` exposes `page`: the page is
/// live and `shared` is the page or above it. The walk stops at the
/// shared page, so nothing outside the share is read.
fn exposed_by(page: Uuid, shared: Uuid) -> Query {
    let up_to_share =
        Query::table("pages").alias("p").ancestors([[col("start.id")]], "parent_id").stop_at(col("p.id").eq(shared)).filter(col("p.id").eq(shared));
    Query::table("pages").alias("start").key([page]).filter(col("start.deleted_at").is_null()).select("allowed", exists(up_to_share))
}

/// The nearest page above `page` that is shared with its children.
fn inherited_share(page: Uuid) -> Query {
    let shared = chain(page)
        .join(Query::table("shares").alias("sh").index("shares_page_id_key", KeyRange::eq([col("p.id")])).filter(col("sh.include_children").eq(true)))
        .filter(Expr::Depth.gt(0))
        .limit(1);
    tree_columns(shared)
        .select("share_id", col("sh.id"))
        .select("key", col("sh.key"))
        .select("search_indexing", col("sh.search_indexing"))
        .select("share_created_at", col("sh.created_at"))
}

// R-Q9, R-Q13, R-Q19: walks up the tree that stop at a shared page or join each step to a share, and settings read from JSON.
#[tokio::test(flavor = "multi_thread")]
async fn a_public_share_exposes_its_page_and_what_is_under_it() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let t = tree(&db, w.space).await?;
    db.insert(Insert::into("workspace").set("name", "Acme")).await?;
    let allowed = |page: Uuid, shared: Uuid| {
        let db = db.clone();
        async move {
            let answer = db.query_opt(exposed_by(page, shared)).await?;
            Ok::<_, DbError>(answer.map(|row| row.get::<bool>("allowed")).transpose()?.unwrap_or(false))
        }
    };

    // Asked on every public page view and file download.
    assert!(allowed(t.c, t.a).await?);
    assert!(allowed(t.a, t.a).await?);
    assert!(!allowed(t.root, t.a).await?);
    assert!(!allowed(t.x, t.a).await?);
    assert!(!allowed(uuid(404), t.a).await?);
    // From `c` the walk reaches `b` and the shared `a`, and not the root.
    let walked = db.query(chain(t.c).stop_at(col("p.id").eq(t.a)).columns(["p.title"])).await?;
    assert_eq!(titles(&walked), ["c", "b", "a"]);

    // `c` is public through the nearest page above it that shares
    // its children. Its own share does not count, nor one without children.
    let root_share = new_share(&db, t.root, "root-key", true).await?;
    let a_share = new_share(&db, t.a, "a-key", true).await?;
    new_share(&db, t.b, "b-key", false).await?;
    new_share(&db, t.c, "c-key", true).await?;
    let nearest = db.query_one(inherited_share(t.c)).await?;
    assert_eq!(
        (nearest.get::<Uuid>("share_id")?, nearest.get::<String>("key")?, nearest.get::<String>("title")?),
        (a_share, "a-key".to_string(), "a".to_string())
    );
    assert_eq!(db.query_one(inherited_share(t.a)).await?.get::<Uuid>("share_id")?, root_share);
    assert!(db.query_opt(inherited_share(t.root)).await?.is_none());
    assert!(db.query_opt(inherited_share(t.x)).await?.is_none());

    // A page's own share with the page it shares.
    let with_page = |shares: Query| {
        shares
            .alias("sh")
            .join(Query::table("pages").alias("sp").key([col("sh.page_id")]))
            .columns(["sh.id", "sh.key", "sh.page_id", "sh.include_children", "sh.search_indexing", "sh.created_at"])
            .select("page_title", col("sp.title"))
            .select("page_slug_id", col("sp.slug_id"))
            .select("space_id", col("sp.space_id"))
    };
    let own = db.query_one(with_page(Query::table("shares").index("shares_page_id_key", KeyRange::eq([t.b])))).await?;
    assert_eq!(
        (own.get::<String>("key")?, own.get::<String>("page_title")?, own.get::<bool>("include_children")?),
        ("b-key".to_string(), "b".to_string(), false)
    );

    // The public entry point. A key opens its page while the page
    // is live and sharing is on in the workspace and in the space.
    let open = |key: &str| {
        with_page(Query::table("shares").index("shares_key_key", KeyRange::eq([key])))
            .join(Query::table("spaces").alias("s").key([col("sp.space_id")]))
            .filter(col("sp.deleted_at").is_null())
            .filter(sharing_enabled(col("s.settings")))
    };
    assert_eq!(db.query_one(open("a-key")).await?.get::<Uuid>("page_id")?, t.a);
    assert!(db.query_opt(open("no-such-key")).await?.is_none());
    db.update(Update::table("spaces").key([w.space]).set("settings", json!({"sharingDisabled": true}))).await?;
    assert!(db.query_opt(open("a-key")).await?.is_none());
    db.update(Update::table("spaces").key([w.space]).set("settings", json!({"sharingDisabled": false}))).await?;
    db.update(Update::table("workspace").key([1]).set("settings", json!({"sharingDisabled": true}))).await?;
    assert!(db.query_opt(open("a-key")).await?.is_none());
    db.update(Update::table("workspace").key([1]).set("settings", json!({}))).await?;
    assert!(db.query_opt(open("a-key")).await?.is_some());

    // The shares of a space, newest first, without trashed pages.
    db.update(Update::table("shares").index("shares_key_key", KeyRange::eq(["root-key"])).set("created_at", now() - hours(3))).await?;
    db.update(Update::table("shares").index("shares_key_key", KeyRange::eq(["a-key"])).set("created_at", now() - hours(2))).await?;
    db.update(Update::table("shares").index("shares_key_key", KeyRange::eq(["b-key"])).set("created_at", now() - hours(1))).await?;
    db.update(Update::table("pages").key([t.c]).set("deleted_at", now())).await?;
    let of_space = Query::table("shares")
        .alias("sh")
        .join(Query::table("pages").alias("p").key([col("sh.page_id")]).filter(col("p.space_id").eq(w.space).and(col("p.deleted_at").is_null())))
        .order_by(col("sh.created_at").desc());
    let of_space = db.query(tree_columns(of_space).select("share_id", col("sh.id")).select("key", col("sh.key"))).await?;
    assert_eq!(column::<String>(&of_space, "key"), ["b-key", "a-key", "root-key"]);
    // A trashed page is exposed by no share.
    assert!(!allowed(t.c, t.a).await?);
    assert!(db.query_opt(open("c-key")).await?.is_none());
    Ok(())
}

/// A page and everything under it, trashed or not.
fn subtree(root: Uuid) -> Query {
    Query::table("pages").descendants([[root]], "parent_id", "pages_parent_idx")
}

// R-Q4, R-Q11, R-Q19: a walk down the tree, breadth first, pruned at trashed pages, with aggregates over the walk.
#[tokio::test(flavor = "multi_thread")]
async fn a_live_subtree_reads_breadth_first_in_sibling_order() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let page =
        |n: u128, parent: Option<u128>, position: &str| page_insert(w.space, parent.map(uuid), position, &format!("page {n}")).set("id", uuid(n));
    db.insert(page(1, None, "a0")).await?;
    db.insert(page(20, Some(1), "a")).await?;
    db.insert(page(10, Some(1), "b")).await?;
    db.insert(page(30, Some(1), "c").set("deleted_at", now())).await?;
    db.insert(page(31, Some(30), "0")).await?;
    db.insert(page(21, Some(20), "1")).await?;
    db.insert(page(11, Some(10), "1")).await?;
    db.insert(page(12, Some(10), "2")).await?;
    db.insert(page(22, Some(21), "0")).await?;
    db.insert(page(99, None, "a1")).await?;

    // The ids of a subtree, trashed or not.
    let everything = db.query(subtree(uuid(1)).columns(["id"])).await?;
    assert_eq!(sorted(numbers(&everything)), [1, 10, 11, 12, 20, 21, 22, 30, 31]);
    // The walk is breadth first, and each page's children come in sibling
    // order: without a sort, a limit stops it after the nearest pages.
    assert_eq!(numbers(&db.query(subtree(uuid(1)).columns(["id"]).limit(3)).await?), [1, 20, 10]);
    let shape = db.query_one(subtree(uuid(1)).select("pages", count()).select("deepest", max(Expr::Depth))).await?;
    assert_eq!((shape.get::<i64>("pages")?, shape.get::<i64>("deepest")?), (9, 3));

    // What a public share shows under its page. Trashed pages are skipped
    // with everything under them, live or not (31 is live); the order is
    // depth, parent, sibling position, id; at most 5000 rows.
    let live = || subtree(uuid(1)).alias("p").prune(col("p.deleted_at").is_null());
    let shared = tree_columns(live().filter(Expr::Depth.gt(0))).select("depth", Expr::Depth);
    let shared = shared.order_by(Expr::Depth).order_by(col("p.parent_id")).order_by(col("p.position")).order_by(col("p.id")).limit(5000);
    let rows = db.query(shared.clone()).await?;
    assert_eq!(numbers(&rows), [20, 10, 11, 12, 21, 22]);
    assert_eq!(column::<i64>(&rows, "depth"), [1, 1, 2, 2, 2, 3]);
    assert_eq!(column::<bool>(&rows, "has_children"), [true, true, false, false, true, false]);
    assert_eq!(numbers(&db.query(shared.limit(4)).await?), [20, 10, 11, 12]);

    // "Duplicate page" reads whole documents, root included, by depth and
    // position, and asks for one row more than it will copy.
    let to_copy = live().columns(["p.id", "p.parent_id", "p.position", "p.title", "p.icon", "p.cover", "p.content", "p.is_base"]);
    let to_copy = db.query(to_copy.order_by(Expr::Depth).order_by(col("p.position")).order_by(col("p.id")).limit(2001)).await?;
    assert_eq!(numbers(&to_copy), [1, 20, 10, 11, 21, 12, 22]);

    // An export orders the same rows the same way, without a cap.
    let exported = db
        .query(
            live()
                .columns(["p.id", "p.slug_id", "p.parent_id", "p.title", "p.is_base"])
                .order_by(Expr::Depth)
                .order_by(col("p.position"))
                .order_by(col("p.id")),
        )
        .await?;
    assert_eq!(exported.len(), 7);
    Ok(())
}

/// Moves a live page and everything live under it to the trash and
/// drops the public links of all of them. Every row gets the same
/// `deleted_at`, because `now()` is the transaction's one instant; restore
/// and the trash list key on that equality. Returns how many pages went.
async fn trash(db: &Client, page: Uuid, by: Uuid) -> DbResult<u64> {
    db.transaction(|tx| async move {
        if tx.query_opt(Query::table("pages").key([page]).filter(col("deleted_at").is_null()).columns(["id"])).await?.is_none() {
            return Ok(0);
        }
        let live = || subtree(page).prune(col("deleted_at").is_null());
        tx.delete(Delete::from("shares").filter(col("page_id").in_list(list_of(live().columns(["id"]))))).await?;
        let trashed = tx.update(Update::rows(live()).set("deleted_at", now()).set("deleted_by", by)).await?;
        Ok::<_, DbError>(trashed.affected())
    })
    .await
}

// R-Q8, R-Q19, R-T3, R-W13, R-X1, R-X7: an update over a pruned walk, stamped with the transaction's one instant.
#[tokio::test(flavor = "multi_thread")]
async fn trashing_a_subtree_stamps_one_instant_and_drops_its_shares() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let t = tree(&db, w.space).await?;
    let earlier = ts("2026-01-01T00:00:00Z");
    let old: Uuid = db
        .insert(page_insert(w.space, Some(t.root), "a1", "old").set("deleted_at", earlier).set("deleted_by", w.bob).returning(["id"]))
        .await?
        .one()?
        .get("id")?;
    let under_old = new_page(&db, w.space, Some(old), "a0", "under old").await?;
    new_share(&db, t.a, "a-key", true).await?;
    new_share(&db, t.c, "c-key", false).await?;
    new_share(&db, t.x, "x-key", false).await?;

    assert_eq!(trash(&db, t.root, w.ann).await?, 4);
    let gone = db.query(Query::table("pages").keys([[t.root], [t.a], [t.b], [t.c]]).columns(["deleted_at", "deleted_by"])).await?;
    let stamps: Vec<Timestamp> = column(&gone, "deleted_at");
    assert!(stamps.iter().all(|stamp| *stamp == stamps[0]), "one instant for the whole subtree: {stamps:?}");
    assert!(stamps[0] > earlier);
    assert_eq!(column::<Uuid>(&gone, "deleted_by"), [w.ann; 4]);
    // What was in the trash already keeps its own moment, and the walk
    // did not pass through it.
    let old_row = db.get("pages", [old]).await?.expect("old");
    assert_eq!((old_row.get::<Timestamp>("deleted_at")?, old_row.get::<Uuid>("deleted_by")?), (earlier, w.bob));
    assert!(db.get("pages", [under_old]).await?.expect("under old").is_null("deleted_at")?);
    assert!(db.get("pages", [t.x]).await?.expect("x").is_null("deleted_at")?);
    assert_eq!(column::<String>(&db.query(Query::table("shares").columns(["key"])).await?, "key"), ["x-key"]);

    // Already in the trash, or not there at all: nothing to do.
    assert_eq!(trash(&db, t.root, w.bob).await?, 0);
    assert_eq!(trash(&db, uuid(404), w.bob).await?, 0);
    assert_eq!(db.get("pages", [t.root]).await?.expect("root").get::<Uuid>("deleted_by")?, w.ann);

    // The clock of a transaction does not move between its statements,
    // however long they take; each lone statement reads it anew.
    let tx = db.begin().await?;
    let began = tx.now().await?;
    tx.update(Update::table("pages").key([t.x]).set("deleted_at", now())).await?;
    tokio::time::sleep(Duration::from_millis(30)).await;
    tx.update(Update::table("pages").key([under_old]).set("deleted_at", now())).await?;
    let written = tx.query(Query::table("pages").keys([[t.x], [under_old]]).select("deleted_at", col("deleted_at")).select("clock", now())).await?;
    assert_eq!(tx.now().await?, began);
    tx.commit().await?;
    assert_eq!(column::<Timestamp>(&written, "deleted_at"), [began; 2]);
    assert_eq!(column::<Timestamp>(&written, "clock"), [began; 2]);
    let first = db.now().await?;
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(db.now().await? > first);
    assert!(first > began);
    Ok(())
}

/// Takes a page out of the trash together with exactly the pages
/// under it that went there in the same operation.
async fn restore(db: &Client, page: Uuid) -> DbResult<u64> {
    db.transaction(|tx| async move {
        // The page and its parent's state. Postgres would lock the page's
        // row here, and not its parent's.
        let found = Query::table("pages")
            .alias("p")
            .key([page])
            .left_join(Query::table("pages").alias("parent").key([col("p.parent_id")]))
            .columns(["p.id", "p.space_id", "p.parent_id", "p.deleted_at"])
            .select("parent_deleted_at", col("parent.deleted_at"))
            .select("existing_parent", col("parent.id"));
        let Some(found) = tx.query_opt(found).await? else {
            return Ok(0);
        };
        let Some(stamp) = found.get::<Option<Timestamp>>("deleted_at")? else {
            return Ok(0);
        };
        // A page whose parent stays in the trash comes back as a root, at
        // the end.
        if !found.is_null("parent_deleted_at")? {
            let space: Uuid = found.get("space_id")?;
            let last: Option<String> =
                tx.query_one(siblings(space, None, Some(page)).select("position", max(col("p.position")))).await?.get("position")?;
            tx.update(Update::table("pages").key([page]).set("parent_id", null()).set("position", key_after(last))).await?;
        }
        // The walk follows only pages stamped at the same instant.
        let together = subtree(page).prune(col("deleted_at").is_not_distinct_from(stamp));
        let restored = tx.update(Update::rows(together).set("deleted_at", null()).set("deleted_by", null())).await?;
        Ok::<_, DbError>(restored.affected())
    })
    .await
}

// R-Q19, R-T3, R-W13, R-X1: an update over a walk that follows only rows with an equal timestamp.
#[tokio::test(flavor = "multi_thread")]
async fn restoring_brings_back_exactly_what_was_trashed_together() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let root = new_page(&db, w.space, None, "a0", "root").await?;
    let a = new_page(&db, w.space, Some(root), "a0", "a").await?;
    let a1 = new_page(&db, w.space, Some(a), "a0", "a1").await?;
    let a2 = new_page(&db, w.space, Some(a), "a1", "a2").await?;
    new_page(&db, w.space, Some(root), "a1", "k").await?;
    let in_trash = || async {
        Ok::<_, DbError>(sorted(titles(&db.query(Query::table("pages").filter(col("deleted_at").is_not_null()).columns(["title"])).await?)))
    };

    // `a2` goes first, on its own; the rest of the tree a moment later.
    assert_eq!(trash(&db, a2, w.ann).await?, 1);
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(trash(&db, root, w.bob).await?, 4);
    assert_eq!(in_trash().await?, ["a", "a1", "a2", "k", "root"]);

    // Restoring the root brings back what went with it, and `a2` stays.
    assert_eq!(restore(&db, root).await?, 4);
    assert_eq!(in_trash().await?, ["a2"]);
    let back = db.get("pages", [a1]).await?.expect("a1");
    assert!(back.is_null("deleted_at")? && back.is_null("deleted_by")?);
    assert_eq!(db.get("pages", [a2]).await?.expect("a2").get::<Uuid>("deleted_by")?, w.ann);
    assert_eq!(restore(&db, root).await?, 0);
    assert_eq!(restore(&db, uuid(404)).await?, 0);

    // Its parent is live: `a2` comes back where it was.
    assert_eq!(restore(&db, a2).await?, 1);
    assert_eq!(titles(&db.query(children(w.space, Some(a))).await?), ["a1", "a2"]);

    // Restored alone from under a parent that stays in the trash, a page
    // becomes a root after the last one.
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(trash(&db, a, w.ann).await?, 3);
    assert_eq!(restore(&db, a1).await?, 1);
    let moved = db.get("pages", [a1]).await?.expect("a1");
    assert!(moved.is_null("parent_id")? && moved.is_null("deleted_at")?);
    assert_eq!(moved.get::<String>("position")?, "a0V");
    assert_eq!(in_trash().await?, ["a", "a2"]);
    assert_eq!(titles(&db.query(children(w.space, None)).await?), ["root", "a1"]);

    // And `a` returns with `a2`, which went with it this time.
    assert_eq!(restore(&db, a).await?, 2);
    assert!(in_trash().await?.is_empty());
    assert_eq!(titles(&db.query(children(w.space, Some(root))).await?), ["a", "k"]);
    Ok(())
}

/// Removes whole subtrees for good. In Postgres one statement returns the
/// storage keys of the removed files and the ids of the removed pages as
/// one result; here the two deletes of one transaction return one kind
/// each. Files go by hand because their foreign key only sets NULL and
/// their objects have to be removed too.
async fn delete_subtrees(tx: &Transaction, roots: &[Uuid]) -> DbResult<(Vec<String>, Vec<Uuid>)> {
    if roots.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let pages = || Query::table("pages").descendants(roots.iter().map(|id| [*id]), "parent_id", "pages_parent_idx");
    let files = Delete::from("files").filter(col("page_id").in_list(list_of(pages().columns(["id"])))).returning(["storage_key"]);
    let keys = tx.delete(files).await?.column()?;
    let removed = tx.delete(Delete::rows(pages()).returning(["id"])).await?.column()?;
    Ok((keys, removed))
}

// R-Q8, R-Q19, R-S6, R-W10, R-W13, R-X1: deletes over a walk from several roots, returning what they removed.
#[tokio::test(flavor = "multi_thread")]
async fn a_permanent_delete_returns_file_keys_and_page_ids() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let t = tree(&db, w.space).await?;
    new_file(&db, t.a, w.space, "key-a").await?;
    new_file(&db, t.c, w.space, "key-c").await?;
    new_file(&db, t.x, w.space, "key-x").await?;
    db.insert(Insert::into("files").set("purpose", "avatar").set("name", "me.png").set("storage_key", "key-avatar")).await?;
    db.insert(Insert::into("comments").set("page_id", t.b).set("body", json!({}))).await?;
    db.insert(Insert::into("page_links").set("source_id", t.x).set("target_id", t.c)).await?;
    new_share(&db, t.a, "a-key", true).await?;

    // One subtree, by its root's id.
    let (keys, pages) = db.transaction(|tx| async move { delete_subtrees(&tx, &[t.a]).await }).await?;
    assert_eq!(sorted(keys), ["key-a", "key-c"]);
    assert_eq!(sorted(pages), sorted(vec![t.a, t.b, t.c]));
    assert_eq!(sorted(titles(&db.query(Query::table("pages").columns(["title"])).await?)), ["root", "x"]);
    assert_eq!(sorted(column::<String>(&db.query(Query::table("files").columns(["storage_key"])).await?, "storage_key")), ["key-avatar", "key-x"]);
    for table in ["comments", "page_links", "shares"] {
        assert_eq!(rows_in(&db, Query::table(table)).await?, 0, "{table}");
    }
    assert!(!db.query_one(tree_columns(Query::table("pages").alias("p").key([t.root]))).await?.get::<bool>("has_children")?);

    // The hourly purge: everything trashed longer ago than the workspace
    // keeps its trash. Two of the expired pages are one under the other,
    // and under them is a page that was never trashed; each goes once.
    db.insert(Insert::into("workspace").set("settings", json!({"trashRetentionDays": 30}))).await?;
    let old: Uuid =
        db.insert(page_insert(w.space, None, "b0", "old").set("deleted_at", now() - days(40)).returning(["id"])).await?.one()?.get("id")?;
    let older: Uuid =
        db.insert(page_insert(w.space, Some(old), "a0", "older").set("deleted_at", now() - days(50)).returning(["id"])).await?.one()?.get("id")?;
    let under = new_page(&db, w.space, Some(older), "a0", "never trashed").await?;
    new_file(&db, under, w.space, "key-under").await?;
    db.insert(page_insert(w.space, None, "b1", "recent").set("deleted_at", now() - days(5))).await?;

    let (keys, pages) = db
        .transaction(|tx| async move {
            // The retention is a workspace setting, 30 days unless set.
            let keep =
                Query::table("workspace").key([1]).select("days", col("settings").json_text(["trashRetentionDays"]).cast(Type::Int).or_else(30));
            let keep: i64 = tx.query_one(keep).await?.get("days")?;
            let expired = Query::table("pages").filter(col("deleted_at").is_not_null().and(col("deleted_at").lt(now() - days(keep)))).columns(["id"]);
            let expired = ids(&tx.query(expired).await?);
            assert_eq!(expired.len(), 2);
            delete_subtrees(&tx, &expired).await
        })
        .await?;
    assert_eq!(keys, ["key-under"]);
    assert_eq!(sorted(pages), sorted(vec![old, older, under]));
    assert_eq!(sorted(titles(&db.query(Query::table("pages").columns(["title"])).await?)), ["recent", "root", "x"]);

    // Nothing has expired now: no roots, nothing removed.
    let nothing = db.transaction(|tx| async move { delete_subtrees(&tx, &[]).await }).await?;
    assert_eq!(nothing, (Vec::new(), Vec::new()));
    Ok(())
}

/// Moves a page with everything under it into another space, as its last
/// root. One statement in Postgres: the files follow, the public links go
/// if the target space does not allow sharing, and each page row is
/// rewritten by its place: only the moved page itself loses its parent
/// and takes a new position.
async fn move_to_space(db: &Client, page: Uuid, target: Uuid) -> DbResult<u64> {
    db.transaction(|tx| async move {
        // The last key among the target space's roots.
        let last: Option<String> =
            tx.query_one(siblings(target, None, Some(page)).select("position", max(col("p.position")))).await?.get("position")?;
        let position = key_after(last);
        let in_subtree = || col("page_id").in_list(list_of(subtree(page).columns(["id"])));
        tx.update(Update::table("files").filter(in_subtree()).set("space_id", target)).await?;
        let sharing_off = scalar(Query::table("spaces").key([target]).select("off", col("settings").json_text(["sharingDisabled"]).cast(Type::Bool)))
            .or_else(false);
        tx.delete(Delete::from("shares").filter(in_subtree().and(sharing_off))).await?;
        let moved = Update::rows(subtree(page))
            .set("space_id", target)
            .set("parent_id", when(col("id").eq(page), null(), col("parent_id")))
            .set("position", when(col("id").eq(page), position.as_str(), col("position")));
        Ok::<_, DbError>(tx.update(moved).await?.affected())
    })
    .await
}

// R-Q13, R-Q19, R-W6, R-W13, R-X1: an update over a walk whose values depend on the row, with the dependent writes in the same transaction.
#[tokio::test(flavor = "multi_thread")]
async fn moving_a_subtree_to_another_space_rewrites_each_row_by_its_place() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let t = tree(&db, w.space).await?;
    let open = new_space(&db, "open").await?;
    new_page(&db, open, None, "a0", "already there").await?;
    let closed = new_space(&db, "closed").await?;
    db.update(Update::table("spaces").key([closed]).set("settings", json!({"sharingDisabled": true}))).await?;
    let file = new_file(&db, t.c, w.space, "key-c").await?;
    let outside = new_file(&db, t.root, w.space, "key-root").await?;
    new_share(&db, t.a, "a-key", true).await?;
    new_share(&db, t.c, "c-key", false).await?;
    new_share(&db, t.root, "root-key", false).await?;
    let place = |id: Uuid| {
        let db = db.clone();
        async move {
            let row = db.get("pages", [id]).await?.expect("the page");
            Ok::<_, DbError>((row.get::<Uuid>("space_id")?, row.get::<Option<Uuid>>("parent_id")?, row.get::<String>("position")?))
        }
    };

    // `a` with `b` and `c` under it moves; `root` and `x` stay.
    assert_eq!(move_to_space(&db, t.a, open).await?, 3);
    assert_eq!(place(t.a).await?, (open, None, "a0V".to_string()));
    assert_eq!(place(t.b).await?, (open, Some(t.a), "a0".to_string()));
    assert_eq!(place(t.c).await?, (open, Some(t.b), "a0".to_string()));
    assert_eq!(place(t.root).await?, (w.space, None, "a0".to_string()));
    assert_eq!(db.get("files", [file]).await?.expect("the file").get::<Uuid>("space_id")?, open);
    assert_eq!(db.get("files", [outside]).await?.expect("the file").get::<Uuid>("space_id")?, w.space);
    // Sharing is allowed there: the links stay.
    assert_eq!(rows_in(&db, Query::table("shares")).await?, 3);
    assert_eq!(titles(&db.query(children(open, None)).await?), ["already there", "a"]);
    assert!(db.query(children(w.space, Some(t.root))).await?.is_empty());

    // The next space has sharing turned off: the subtree's links are
    // dropped by the move, and only they.
    assert_eq!(move_to_space(&db, t.a, closed).await?, 3);
    assert_eq!(place(t.a).await?, (closed, None, "a0".to_string()));
    assert_eq!(place(t.c).await?, (closed, Some(t.b), "a0".to_string()));
    assert_eq!(column::<String>(&db.query(Query::table("shares").columns(["key"])).await?, "key"), ["root-key"]);
    assert_eq!(db.get("files", [file]).await?.expect("the file").get::<Uuid>("space_id")?, closed);
    Ok(())
}

// R-Q19, R-S6, R-W2, R-W11: one insert of 2000 rows that reference each other, all or nothing.
#[tokio::test(flavor = "multi_thread")]
async fn two_thousand_pages_are_inserted_by_one_statement_or_none() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let copy = |n: u128| uuid(10_000 + n);
    let columns = [
        "id", "slug_id", "space_id", "parent_id", "position", "title", "icon", "cover", "content", "text_content", "creator_id", "last_editor_id",
        "contributor_ids", "is_base",
    ];
    let row = |n: u128, slug: String, parent: Option<Uuid>| {
        let document = json!({"type": "doc", "content": [{"type": "paragraph", "attrs": {"n": n as u64}}]});
        [
            val(copy(n)),
            val(slug),
            val(w.space),
            val(parent),
            val(format!("a{}", n % 3)),
            val(format!("Copy {n}")),
            null(),
            null(),
            val(document),
            val(format!("text of copy {n}")),
            val(w.ann),
            val(w.ann),
            val(uuids(&[w.ann])),
            val(n.is_multiple_of(500)),
        ]
    };

    // "Duplicate page" copies up to 2000 pages at once. Postgres takes the
    // rows as one JSON parameter and expands it (`jsonb_to_recordset`);
    // here they are the statement's rows. The caller chooses the ids, so
    // a row can name its parent in the same statement, and sends parents
    // first: page n hangs under page (n - 1) / 3.
    let mut pages = Insert::into("pages").columns(columns);
    for n in 0..2000 {
        pages = pages.values(row(n, format!("copy-{n:04}"), (n > 0).then(|| copy((n - 1) / 3))));
    }
    assert_eq!(db.insert(pages).await?.affected(), 2000);

    let shape = db
        .query_one(
            subtree(copy(0)).select("pages", count()).select("deepest", max(Expr::Depth)).select("bases", count().filter(col("is_base").eq(true))),
        )
        .await?;
    assert_eq!((shape.get::<i64>("pages")?, shape.get::<i64>("deepest")?, shape.get::<i64>("bases")?), (2000, 7, 4));
    assert_eq!(titles(&db.query(children(w.space, Some(copy(0)))).await?), ["Copy 3", "Copy 1", "Copy 2"]);
    let last = db.get("pages", [copy(1999)]).await?.expect("the last copy");
    assert_eq!(last.get::<Uuid>("parent_id")?, copy(666));
    assert_eq!(last.get::<Json>("content")?["content"][0]["attrs"]["n"], json!(1999));
    assert_eq!(last.get::<Vec<Uuid>>("contributor_ids")?, [w.ann]);
    assert_eq!(
        titles(&db.query(chain(copy(1999)).columns(["p.title"]).order_by(Expr::Depth.desc()).limit(3)).await?),
        ["Copy 0", "Copy 2", "Copy 7"]
    );

    // One bad row and nothing of the statement stays: a slug that exists,
    // or a parent that does not.
    let clash = Insert::into("pages").columns(columns).values(row(5000, "fresh-1".to_string(), None)).values(row(
        5001,
        "copy-0007".to_string(),
        Some(copy(5000)),
    ));
    let clash = db.insert(clash).await.unwrap_err();
    assert_eq!(clash.code, ErrorCode::UniqueViolation);
    assert!(clash.violates("pages_slug_id_key"));
    let orphan = Insert::into("pages").columns(columns).values(row(5000, "fresh-1".to_string(), None)).values(row(
        5001,
        "fresh-2".to_string(),
        Some(uuid(404)),
    ));
    assert_eq!(db.insert(orphan).await.unwrap_err().code, ErrorCode::ForeignKeyViolation);
    assert_eq!(rows_in(&db, Query::table("pages")).await?, 2000);
    assert!(db.get("pages", [copy(5000)]).await?.is_none());
    Ok(())
}

/// A column mapped through pairs of (old id, new id): in SQL, a join
/// against an id map passed as a JSON parameter.
fn remap(column: Expr, map: &[(Uuid, Uuid)]) -> Expr {
    case(map.iter().map(|(old, new)| (column.clone().eq(*old), val(*new))), null())
}

// R-Q6, R-Q12, R-Q13, R-W2, R-W3, R-W5, R-W6: copying a base with inserts fed by queries, remapped ids, rewritten JSON cells and one row per list element.
#[tokio::test(flavor = "multi_thread")]
async fn duplicating_a_base_copies_its_data_under_new_ids() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let (ann, bob, space) = (w.ann, w.bob, w.space);
    let base = new_page(&db, space, None, "a0", "Tasks").await?;
    let notes = new_page(&db, space, Some(base), "a0", "Notes").await?;
    let target = new_page(&db, space, None, "a1", "Linked from the notes").await?;
    let old_file = new_file(&db, base, space, "key-old").await?;
    for (id, name, kind, primary, position) in [("pname", "Name", "text", true, "a0"), ("pfile", "Files", "file", false, "a1")] {
        db.insert(
            Insert::into("base_properties")
                .set("page_id", base)
                .set("id", id)
                .set("name", name)
                .set("type", kind)
                .set("is_primary", primary)
                .set("position", position)
                .set("options", json!({"wrap": true})),
        )
        .await?;
    }
    let made = ts("2026-01-05T08:00:00Z");
    let with_file = json!({"pname": "Spec", "pfile": [{"id": old_file.to_string(), "name": "spec.pdf"}]});
    db.insert(
        Insert::into("base_rows")
            .set("page_id", base)
            .set("position", "a0")
            .set("cells", with_file)
            .set("creator_id", bob)
            .set("last_editor_id", bob)
            .set("created_at", made),
    )
    .await?;
    db.insert(
        Insert::into("base_rows")
            .set("page_id", base)
            .set("position", "a1")
            .set("cells", json!({"pname": "Plain"}))
            .set("creator_id", bob)
            .set("created_at", made),
    )
    .await?;
    db.insert(
        Insert::into("base_views")
            .set("page_id", base)
            .set("name", "Board")
            .set("type", "kanban")
            .set("config", json!({"groupBy": "pname"}))
            .set("position", "a0")
            .set("creator_id", bob),
    )
    .await?;
    db.insert(Insert::into("synced_blocks").set("page_id", notes).set("block_id", "blk1").set("content", json!({"type": "callout"}))).await?;

    // The pages themselves were copied first: here are the copies of the
    // base and of its child page, and the id of the file's copy.
    let (new_base, new_notes, new_file_id) = (uuid(1), uuid(2), uuid(3));
    db.insert(page_insert(space, None, "a2", "Tasks (copy)").set("id", new_base).set("is_base", true)).await?;
    db.insert(page_insert(space, Some(new_base), "a0", "Notes").set("id", new_notes)).await?;
    let page_map = [(base, new_base), (notes, new_notes)];
    let olds = || page_map.iter().map(|(old, _)| KeyRange::eq([*old]));

    db.transaction(|tx| async move {
        // The properties of every copied base, under the new page ids.
        let properties = Query::table("base_properties")
            .alias("bp")
            .keys(page_map.iter().map(|(old, _)| [*old]))
            .select("page_id", remap(col("bp.page_id"), &page_map));
        let properties = properties.columns(["bp.id", "bp.name", "bp.type", "bp.options", "bp.is_primary", "bp.position"]);
        let copied = tx
            .insert(
                Insert::into("base_properties")
                    .columns(["page_id", "id", "name", "type", "options", "is_primary", "position"])
                    .from_query(properties),
            )
            .await?;
        assert_eq!(copied.affected(), 2);

        // The rows keep when they were made; the copier is their
        // last editor.
        let rows =
            Query::table("base_rows").alias("br").index_ranges("base_rows_order_idx", olds()).select("page_id", remap(col("br.page_id"), &page_map));
        let rows = rows
            .columns(["br.cells", "br.position", "br.creator_id"])
            .select("last_editor_id", val(ann))
            .columns(["br.created_at"])
            .select("updated_at", now());
        let copied = tx
            .insert(
                Insert::into("base_rows")
                    .columns(["page_id", "cells", "position", "creator_id", "last_editor_id", "created_at", "updated_at"])
                    .from_query(rows),
            )
            .await?;
        assert_eq!(copied.affected(), 2);

        // The views, with the copier as their creator.
        let views =
            Query::table("base_views").alias("bv").index_ranges("base_views_page_idx", olds()).select("page_id", remap(col("bv.page_id"), &page_map));
        let views = views.columns(["bv.name", "bv.type", "bv.config", "bv.position"]).select("creator_id", val(ann));
        tx.insert(Insert::into("base_views").columns(["page_id", "name", "type", "config", "position", "creator_id"]).from_query(views)).await?;

        // The copied files, their bytes already copied in storage.
        let files = Insert::into("files").columns(["id", "purpose", "page_id", "space_id", "uploader_id", "name", "mime", "size", "storage_key"]);
        tx.insert(files.values([
            val(new_file_id),
            val("page"),
            val(new_base),
            val(space),
            val(ann),
            val("spec.pdf"),
            val("application/pdf"),
            val(5_000_000_000i64),
            val("key-new"),
        ]))
        .await?;

        // The copied rows whose cells mention a copied file. SQL would
        // match a regular expression against the JSON text; an id has
        // nothing a `like` would read as a wildcard. The caller rewrites
        // the cells and writes them back, each row its own value, at once.
        let mentions = any([col("cells").cast(Type::Text).like(format!("%{old_file}%"))]);
        let found = tx
            .query(Query::table("base_rows").index("base_rows_order_idx", KeyRange::eq([new_base])).filter(mentions).columns(["id", "cells"]))
            .await?;
        assert_eq!(found.len(), 1);
        let mut rewritten = Vec::new();
        for row in &found {
            let cells = row.get::<Json>("cells")?.to_string().replace(&old_file.to_string(), &new_file_id.to_string());
            rewritten.push((row.get::<Uuid>("id")?, serde_json::from_str::<Json>(&cells).expect("still JSON")));
        }
        let cells = case(rewritten.iter().map(|(id, cells)| (col("id").eq(*id), val(cells.clone()))), col("cells"));
        let written = tx.update(Update::rows(Query::table("base_rows").keys(rewritten.iter().map(|(id, _)| [*id]))).set("cells", cells)).await?;
        assert_eq!(written.affected(), 1);

        // The links of the copies, to the pages that exist among
        // the ones their documents name.
        let named = [target, uuid(404)];
        let existing = ids(&tx.query(Query::table("pages").keys(named.map(|id| [id])).columns(["id"])).await?);
        let mut links = Insert::into("page_links").columns(["source_id", "target_id"]);
        for target in existing {
            links = links.values([new_notes, target]);
        }
        assert_eq!(tx.insert(links.on_conflict_do_nothing()).await?.affected(), 1);

        // The synced blocks of the copies.
        let blocks = Insert::into("synced_blocks").columns(["page_id", "block_id", "content"]).values([
            val(new_notes),
            val("blk1"),
            val(json!({"type": "callout"})),
        ]);
        tx.insert(blocks.on_conflict_do_nothing()).await?;

        // The copier watches every copy. Postgres unnests an array of page
        // ids; `each` repeats the user's row once per id.
        let watching = Query::table("users")
            .alias("u")
            .key([ann])
            .each("page", uuids(&[new_base, new_notes]))
            .select("user_id", col("u.id"))
            .select("page_id", col("page"));
        let watches = Insert::into("watchers").columns(["user_id", "page_id"]).from_query(watching).on_conflict_do_nothing_on("watchers_page_key");
        assert_eq!(tx.insert(watches.clone()).await?.affected(), 2);
        assert_eq!(tx.insert(watches).await?.affected(), 0);
        Ok::<_, DbError>(())
    })
    .await?;

    let properties = db.query(Query::table("base_properties").index("base_properties_order_idx", KeyRange::eq([new_base]))).await?;
    assert_eq!(column::<String>(&properties, "id"), ["pname", "pfile"]);
    assert_eq!(column::<bool>(&properties, "is_primary"), [true, false]);
    assert_eq!(properties.first().expect("a property").get::<Json>("options")?, json!({"wrap": true}));

    let rows = db.query(Query::table("base_rows").index("base_rows_order_idx", KeyRange::eq([new_base]))).await?;
    assert_eq!(column::<Timestamp>(&rows, "created_at"), [made, made]);
    assert_eq!(column::<Uuid>(&rows, "last_editor_id"), [ann, ann]);
    assert_eq!(column::<Uuid>(&rows, "creator_id"), [bob, bob]);
    assert!(rows.iter().all(|row| row.get::<Timestamp>("updated_at").expect("a time") > made));
    let cells: Vec<Json> = column(&rows, "cells");
    assert_eq!(cells[0], json!({"pname": "Spec", "pfile": [{"id": new_file_id.to_string(), "name": "spec.pdf"}]}));
    assert_eq!(cells[1], json!({"pname": "Plain"}));
    // The original still points at its own file.
    let original = db.query(Query::table("base_rows").index("base_rows_order_idx", KeyRange::eq([base])).limit(1)).await?.one()?;
    assert_eq!(original.get::<Json>("cells")?["pfile"][0]["id"], json!(old_file.to_string()));
    assert_eq!(original.get::<Uuid>("last_editor_id")?, bob);

    let view = db.query_one(Query::table("base_views").index("base_views_page_idx", KeyRange::eq([new_base]))).await?;
    assert_eq!(
        (view.get::<String>("name")?, view.get::<String>("type")?, view.get::<Json>("config")?, view.get::<Uuid>("creator_id")?),
        ("Board".to_string(), "kanban".to_string(), json!({"groupBy": "pname"}), ann)
    );
    assert_eq!(db.get("files", [new_file_id]).await?.expect("the copy").get::<i64>("size")?, 5_000_000_000);
    assert_eq!(rows_in(&db, Query::table("page_links").range(KeyRange::eq([new_notes]))).await?, 1);
    assert!(db.get("synced_blocks", [val(new_notes), val("blk1")]).await?.is_some());
    assert_eq!(rows_in(&db, Query::table("watchers").index("watchers_page_key", KeyRange::eq([ann]))).await?, 2);
    Ok(())
}

// ------------------------------------------------------ saving a document

/// What a collaboration session hands over when it saves a page.
struct Save {
    page: Uuid,
    /// The stored version the session loaded, and the steps applied since.
    base_version: i64,
    steps: i64,
    content: Json,
    text: String,
    editor: Option<Uuid>,
    contributors: Vec<Uuid>,
    /// The pages the document links to, by id and by public slug.
    link_ids: Vec<Uuid>,
    link_slugs: Vec<String>,
    /// The blocks of the document that other pages transclude.
    blocks: Vec<(String, Json)>,
}

impl Save {
    fn new(page: Uuid, base_version: i64, content: Json, text: &str) -> Save {
        Save {
            page,
            base_version,
            steps: 1,
            content,
            text: text.to_string(),
            editor: None,
            contributors: Vec::new(),
            link_ids: Vec::new(),
            link_slugs: Vec::new(),
            blocks: Vec::new(),
        }
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Saved {
    /// Stored, as this version.
    Stored(i64),
    /// Someone else saved since the session loaded the page.
    Conflict,
    Gone,
}

/// Rebuilds what is derived from a document: its links to other pages,
/// and the blocks other pages show a copy of.
async fn index_page_content(tx: &Transaction, save: &Save) -> DbResult<()> {
    let page = save.page;
    tx.delete(Delete::rows(Query::table("page_links").range(KeyRange::eq([page])))).await?;
    // Only pages that exist become links, and never the page itself.
    // Postgres finds an index for each half of `id = ANY(..) OR slug_id =
    // ANY(..)`; here each half is a read by its key.
    let link = |targets: Query| {
        let rows = targets.filter(col("id").ne(page)).select("source_id", val(page)).select("target_id", col("id"));
        Insert::into("page_links").columns(["source_id", "target_id"]).from_query(rows).on_conflict_do_nothing()
    };
    if !save.link_ids.is_empty() {
        tx.insert(link(Query::table("pages").keys(save.link_ids.iter().map(|id| [*id])))).await?;
    }
    if !save.link_slugs.is_empty() {
        tx.insert(link(Query::table("pages").index_ranges("pages_slug_id_key", save.link_slugs.iter().map(|slug| KeyRange::eq([slug.as_str()])))))
            .await?;
    }

    let kept: Vec<&str> = save.blocks.iter().map(|(id, _)| id.as_str()).collect();
    tx.delete(Delete::rows(Query::table("synced_blocks").range(KeyRange::eq([page]))).filter(col("block_id").not_in_list(kept))).await?;
    if !save.blocks.is_empty() {
        let mut blocks = Insert::into("synced_blocks").columns(["page_id", "block_id", "content"]);
        for (id, content) in &save.blocks {
            blocks = blocks.values([val(page), val(id.as_str()), val(content.clone())]);
        }
        // A block is rewritten only when its content really differs, so
        // that its `updated_at` moves only then.
        let changed = col("content").is_distinct_from(excluded("content"));
        tx.insert(blocks.on_conflict_update(PRIMARY).update("content", excluded("content")).update("updated_at", now()).update_where(changed))
            .await?;
    }
    Ok(())
}

/// Copies the page into its history, unless a version was taken in
/// the last five minutes (the last minute, while the page itself is
/// younger than five), and unless the latest version already holds this
/// very document. Returns how many versions were added: one or none.
async fn snapshot(tx: &Transaction, page: Uuid) -> DbResult<u64> {
    // Newest first is the index's order.
    let versions = || Query::table("page_versions").alias("v").index("page_versions_page_idx", KeyRange::eq([col("p.id")]));
    let window = when(col("p.created_at").gt(now() - minutes(5)), seconds(60), minutes(5));
    let recent = versions().filter(col("v.created_at").gt(now() - window));
    let latest = versions().limit(1).select("content", col("v.content"));
    let source = Query::table("pages")
        .alias("p")
        .key([page])
        .filter(exists(recent).not())
        .filter(col("p.content").is_distinct_from(scalar(latest)))
        .columns(["p.id", "p.title", "p.icon", "p.content", "p.last_editor_id", "p.contributor_ids"]);
    let taken = tx
        .insert(Insert::into("page_versions").columns(["page_id", "title", "icon", "content", "editor_id", "contributor_ids"]).from_query(source))
        .await?;
    Ok(taken.affected())
}

/// The save of a page's document: all of it, or nothing.
async fn save_page(db: &Client, save: Save) -> DbResult<Saved> {
    let tx = db.begin().await?;
    // Postgres locks the page's row (`FOR UPDATE`) so that the version
    // read here cannot change before the write. A writing transaction is
    // alone: a second save waits at `begin`, then reads what the first
    // one committed.
    let Some(stored) = tx.query_opt(Query::table("pages").key([save.page]).columns(["content", "space_id", "deleted_at", "doc_version"])).await?
    else {
        tx.rollback().await?;
        return Ok(Saved::Gone);
    };
    let stored: i64 = stored.get("doc_version")?;
    if stored != save.base_version {
        tx.rollback().await?;
        return Ok(Saved::Conflict);
    }
    let version = stored + save.steps;
    // The whole document and its text are replaced; the editor is
    // kept when the save has none; the contributors are a set union.
    let page = Update::table("pages")
        .key([save.page])
        .set("content", save.content.clone())
        .set("text_content", save.text.as_str())
        .set("doc_version", version)
        .set("last_editor_id", coalesce([val(save.editor), col("last_editor_id")]))
        .set("contributor_ids", call(Func::ListDistinct, [col("contributor_ids").list_concat(uuids(&save.contributors))]))
        .set("updated_at", now());
    tx.update(page).await?;
    index_page_content(&tx, &save).await?;
    snapshot(&tx, save.page).await?;
    tx.commit().await?;
    Ok(Saved::Stored(version))
}

/// A search has to name the key prefix of its index; this one has none.
fn no_prefix() -> [Expr; 0] {
    []
}

/// Creates a page under a live page of its space, or as a root,
/// after the last sibling there, and indexes what its document links to.
/// `None` when the parent is not a live page of the space.
async fn create_page(db: &Client, space: Uuid, parent: Option<Uuid>, title: &str, links: Vec<Uuid>, by: Uuid) -> DbResult<Option<Uuid>> {
    db.exclusive(|tx| async move {
        if let Some(parent) = parent {
            // The parent must be a live page of the space.
            let live = Query::table("pages").key([parent]).filter(col("space_id").eq(space).and(col("deleted_at").is_null())).columns(["id"]);
            if tx.query_opt(live).await?.is_none() {
                return Ok(None);
            }
        }
        // The last key among the siblings.
        let last: Option<String> = tx.query_one(siblings(space, parent, None).select("position", max(col("p.position")))).await?.get("position")?;
        // The caller makes the public slug, the database the id.
        let content = json!({"type": "doc", "content": [{"type": "heading", "text": title}]});
        let page = Insert::into("pages")
            .set("slug_id", slug_id())
            .set("space_id", space)
            .set("parent_id", parent)
            .set("position", key_after(last))
            .set("title", title)
            .set("icon", null());
        let page = page
            .set("content", content.clone())
            .set("text_content", title)
            .set("creator_id", by)
            .set("last_editor_id", by)
            .set("contributor_ids", uuids(&[by]));
        let page: Uuid = tx.insert(page.returning(["id"])).await?.one()?.get("id")?;
        // What is derived from the document.
        let mut derived = Save::new(page, 0, content, title);
        derived.link_ids = links;
        index_page_content(&tx, &derived).await?;
        Ok::<_, DbError>(Some(page))
    })
    .await
}

/// Creates a base: a page flagged as one, seeded with its first
/// columns, an empty row and a table view. All of it or none.
async fn create_base(db: &Client, space: Uuid, title: &str, columns: &[&str], by: Uuid) -> DbResult<Uuid> {
    db.transaction(|tx| async move {
        let last: Option<String> = tx.query_one(siblings(space, None, None).select("position", max(col("p.position")))).await?.get("position")?;
        // The page, flagged as a base.
        let page = page_insert(space, None, &key_after(last), title)
            .set("creator_id", by)
            .set("last_editor_id", by)
            .set("contributor_ids", uuids(&[by]))
            .set("is_base", true);
        let page: Uuid = tx.insert(page.returning(["id"])).await?.one()?.get("id")?;
        // Three or four columns, one statement each.
        for (n, name) in columns.iter().enumerate() {
            let column = Insert::into("base_properties")
                .set("page_id", page)
                .set("id", format!("p{n}"))
                .set("name", *name)
                .set("type", "text")
                .set("options", json!({}));
            tx.insert(column.set("is_primary", n == 0).set("position", format!("a{n}"))).await?;
        }
        // An empty row and a table view.
        tx.insert(Insert::into("base_rows").set("page_id", page).set("position", "a0").set("creator_id", by).set("last_editor_id", by)).await?;
        tx.insert(
            Insert::into("base_views").set("page_id", page).set("name", "Table").set("type", "table").set("position", "a0").set("creator_id", by),
        )
        .await?;
        Ok::<_, DbError>(page)
    })
    .await
}

// R-S5, R-S6, R-W1, R-W6, R-W11, R-X1: a page or a base created with its dependent rows in one transaction, and the violations a bad row raises.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_page_goes_after_its_last_sibling_with_its_links_or_not_at_all() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let other_space = new_space(&db, "other").await?;
    let foreign = new_page(&db, other_space, None, "a0", "Foreign").await?;
    let titled = |title: &'static str| {
        let db = db.clone();
        async move { rows_in(&db, Query::table("pages").filter(col("title").eq(title))).await }
    };

    // Two roots and a child, with links to pages that may not exist.
    let first = create_page(&db, w.space, None, "First", vec![], w.ann).await?.expect("a root");
    let second = create_page(&db, w.space, None, "Second", vec![first, foreign, uuid(404)], w.bob).await?.expect("a root");
    let child = create_page(&db, w.space, Some(first), "Child", vec![second], w.ann).await?.expect("a child");
    // A parent in another space is no parent: nothing is written.
    assert!(create_page(&db, w.space, Some(foreign), "Lost", vec![first], w.ann).await?.is_none());
    assert_eq!(titled("Lost").await?, 0);

    // The page is read back after every write, to answer with.
    let read = || page_columns(Query::table("pages").alias("p").key([second])).select("content", col("p.content"));
    let row = db.query_one(read()).await?;
    assert_eq!(
        (row.get::<String>("position")?, row.get::<i64>("doc_version")?, row.get::<bool>("is_base")?, row.get::<bool>("has_children")?),
        ("a0V".to_string(), 0, false, false)
    );
    assert_eq!(
        (row.get::<Uuid>("creator_id")?, row.get::<Uuid>("last_editor_id")?, row.get::<Vec<Uuid>>("contributor_ids")?),
        (w.bob, w.bob, vec![w.bob])
    );
    assert_eq!(row.get::<Timestamp>("created_at")?, row.get::<Timestamp>("updated_at")?);
    assert!(row.is_null("deleted_at")? && row.is_null("icon")? && row.is_null("parent_id")?);
    assert_eq!(row.get::<Json>("content")?["content"][0]["text"], json!("Second"));
    assert_eq!(titles(&db.query(children(w.space, None)).await?), ["First", "Second"]);
    assert_eq!(ids(&db.query(children(w.space, Some(first))).await?), [child]);
    // Links go to pages that exist, in whatever space.
    let links = db.query(Query::table("page_links").range(KeyRange::eq([second])).columns(["target_id"])).await?;
    assert_eq!(sorted(column::<Uuid>(&links, "target_id")), sorted(vec![first, foreign]));

    // What only the database stops: a slug that exists, a space that is
    // gone, a page without a position.
    let slug: String = row.get("slug_id")?;
    let clash = db.insert(page_with_slug(&slug, w.space, None, "zz", "Clash")).await.unwrap_err();
    assert!(clash.is_unique_violation() && clash.violates("pages_slug_id_key"));
    assert_eq!(db.insert(page_insert(uuid(404), None, "a0", "Nowhere")).await.unwrap_err().code, ErrorCode::ForeignKeyViolation);
    let unplaced = db.insert(Insert::into("pages").set("slug_id", slug_id()).set("space_id", w.space)).await.unwrap_err();
    assert_eq!(unplaced.code, ErrorCode::NotNullViolation);
    assert!(unplaced.violates("position"));

    // A patch of the page's metadata. Each of the three is changed
    // only if given, and given as nothing it is cleared.
    let patch = |title: Option<Option<&str>>, icon: Option<Option<&str>>, cover: Option<Option<&str>>| {
        let given = |value: Option<Option<&str>>, column: &str| when(val(value.is_some()), val(value.flatten()), col(column));
        Update::table("pages")
            .key([second])
            .set("title", given(title, "title"))
            .set("icon", given(icon, "icon"))
            .set("cover", given(cover, "cover"))
            .set("last_editor_id", w.ann)
            .set("updated_at", now())
    };
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(db.update(patch(Some(Some("Renamed")), Some(Some("🚀")), None)).await?.affected(), 1);
    let row = db.query_one(read()).await?;
    assert_eq!((row.get::<String>("title")?, row.get::<String>("icon")?, row.is_null("cover")?), ("Renamed".to_string(), "🚀".to_string(), true));
    assert_eq!((row.get::<Uuid>("last_editor_id")?, row.get::<Vec<Uuid>>("contributor_ids")?), (w.ann, vec![w.bob]));
    assert!(row.get::<Timestamp>("updated_at")? > row.get::<Timestamp>("created_at")?);
    db.update(patch(None, Some(None), Some(Some("cover.png")))).await?;
    let row = db.query_one(read()).await?;
    assert_eq!(
        (row.get::<String>("title")?, row.is_null("icon")?, row.get::<String>("cover")?),
        ("Renamed".to_string(), true, "cover.png".to_string())
    );
    // The page is found by its new title at once.
    assert_eq!(ids(&db.query(Query::table("pages").search("pages_search_idx", no_prefix(), "renamed").columns(["id"])).await?), [second]);

    // A base arrives whole.
    let base = create_base(&db, w.space, "Tasks", &["Name", "Status", "Due"], w.ann).await?;
    let row = db.get("pages", [base]).await?.expect("the base");
    assert_eq!((row.get::<bool>("is_base")?, row.get::<String>("position")?), (true, "a0VV".to_string()));
    let columns = db.query(Query::table("base_properties").index("base_properties_order_idx", KeyRange::eq([base]))).await?;
    assert_eq!(
        (column::<String>(&columns, "name"), column::<bool>(&columns, "is_primary")),
        (vec!["Name".to_string(), "Status".to_string(), "Due".to_string()], vec![true, false, false])
    );
    let seeded = db.query_one(Query::table("base_rows").index("base_rows_order_idx", KeyRange::eq([base]))).await?;
    assert_eq!(seeded.get::<Json>("cells")?, json!({}));
    assert_eq!(rows_in(&db, Query::table("base_views").index("base_views_page_idx", KeyRange::eq([base]))).await?, 1);

    // Or not at all: the third column's name is the first one's again,
    // and the page that was inserted before it is gone with the rest.
    let failed = create_base(&db, w.space, "Broken", &["Name", "Status", " name"], w.ann).await.unwrap_err();
    assert!(failed.violates("base_properties_name_key"));
    assert_eq!(titled("Broken").await?, 0);
    assert_eq!(rows_in(&db, Query::table("base_properties")).await?, 3);
    assert_eq!(rows_in(&db, Query::table("base_rows")).await?, 1);
    Ok(())
}

// R-Q13, R-Q14, R-Q17, R-T6, R-W3, R-W4, R-W5, R-X1: a save that replaces a document, its links, its blocks and its search entry in one transaction.
#[tokio::test(flavor = "multi_thread")]
async fn a_save_replaces_the_document_and_rebuilds_what_derives_from_it() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let (ann, bob, space) = (w.ann, w.bob, w.space);
    let created = ts("2026-01-01T00:00:00Z");
    let page = page_with_slug("handbook01", space, None, "a0", "Handbook").set("content", json!({"type": "doc"})).set("text_content", "old words");
    let page = page
        .set("creator_id", ann)
        .set("last_editor_id", ann)
        .set("contributor_ids", uuids(&[ann]))
        .set("created_at", created)
        .set("updated_at", created);
    let page: Uuid = db.insert(page.returning(["id"])).await?.one()?.get("id")?;
    let by_id = new_page(&db, space, None, "a1", "Linked by id").await?;
    let by_slug: Uuid = db.insert(page_with_slug("target0001", space, None, "a2", "Linked by slug").returning(["id"])).await?.one()?.get("id")?;
    let no_longer = new_page(&db, space, None, "a3", "No longer linked").await?;
    db.insert(Insert::into("page_links").set("source_id", page).set("target_id", no_longer)).await?;
    db.insert(Insert::into("page_links").set("source_id", by_id).set("target_id", page)).await?;
    let block = |of: Uuid, id: &str, content: Json| {
        Insert::into("synced_blocks").set("page_id", of).set("block_id", id).set("content", content).set("updated_at", created)
    };
    db.insert(block(page, "keep", json!({"a": 1, "b": {"x": 1, "y": 2}}))).await?;
    db.insert(block(page, "edit", json!({"text": "before"}))).await?;
    db.insert(block(page, "drop", json!({"text": "gone"}))).await?;
    db.insert(block(by_id, "drop", json!({"text": "another page's"}))).await?;

    let document = json!({"type": "doc", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Hello world"}]}]});
    let mut save = Save::new(page, 0, document.clone(), "Hello world");
    save.steps = 3;
    save.editor = Some(bob);
    save.contributors = vec![bob, ann, bob];
    save.link_ids = vec![by_id, page, uuid(404)];
    save.link_slugs = vec!["target0001".to_string(), "handbook01".to_string(), "nosuchpage".to_string()];
    save.blocks = vec![
        // The same block with its keys in another order: no change.
        ("keep".to_string(), json!({"b": {"y": 2, "x": 1}, "a": 1})),
        ("edit".to_string(), json!({"text": "after"})),
        ("new".to_string(), json!({"text": "fresh"})),
    ];
    assert_eq!(save_page(&db, save).await?, Saved::Stored(3));

    let row = db.get("pages", [page]).await?.expect("the page");
    assert_eq!(row.get::<i64>("doc_version")?, 3);
    assert_eq!(row.get::<Json>("content")?, document);
    // The document comes back with its keys as jsonb orders them: by
    // length, then by bytes.
    assert_eq!(
        row.get::<Json>("content")?.to_string(),
        r#"{"type":"doc","content":[{"type":"paragraph","content":[{"text":"Hello world","type":"text"}]}]}"#
    );
    assert_eq!(row.get::<String>("text_content")?, "Hello world");
    assert_eq!(row.get::<Uuid>("last_editor_id")?, bob);
    assert_eq!(row.get::<Vec<Uuid>>("contributor_ids")?, [ann, bob]);
    assert_eq!(row.get::<Timestamp>("created_at")?, created);
    assert!(row.get::<Timestamp>("updated_at")? > created);

    let links = db.query(Query::table("page_links").range(KeyRange::eq([page])).columns(["target_id"])).await?;
    assert_eq!(sorted(column::<Uuid>(&links, "target_id")), sorted(vec![by_id, by_slug]));
    // Links that point at the page are other pages' business.
    assert_eq!(rows_in(&db, Query::table("page_links").index("page_links_target_idx", KeyRange::eq([page]))).await?, 1);

    let blocks = db.query(Query::table("synced_blocks").range(KeyRange::eq([page]))).await?;
    assert_eq!(column::<String>(&blocks, "block_id"), ["edit", "keep", "new"]);
    assert_eq!(column::<Json>(&blocks, "content"), [json!({"text": "after"}), json!({"a": 1, "b": {"x": 1, "y": 2}}), json!({"text": "fresh"})]);
    let written: Vec<Timestamp> = column(&blocks, "updated_at");
    assert!(written[0] > created && written[1] == created && written[2] > created, "{written:?}");
    assert_eq!(rows_in(&db, Query::table("synced_blocks").range(KeyRange::eq([by_id]))).await?, 1);

    // The search index follows the document: the new words are found and
    // the old ones are not.
    let finds = |word: &str| Query::table("pages").search("pages_search_idx", no_prefix(), word).columns(["id"]);
    assert_eq!(ids(&db.query(finds("hello")).await?), [page]);
    assert!(db.query(finds("old")).await?.is_empty());

    // A save without an editor keeps the last one; without links or
    // blocks it leaves none behind.
    assert_eq!(save_page(&db, Save::new(page, 3, json!({"type": "doc", "content": []}), "")).await?, Saved::Stored(4));
    let row = db.get("pages", [page]).await?.expect("the page");
    assert_eq!((row.get::<Uuid>("last_editor_id")?, row.get::<Vec<Uuid>>("contributor_ids")?), (bob, vec![ann, bob]));
    assert_eq!(rows_in(&db, Query::table("page_links").range(KeyRange::eq([page]))).await?, 0);
    assert_eq!(rows_in(&db, Query::table("synced_blocks").range(KeyRange::eq([page]))).await?, 0);
    assert_eq!(rows_in(&db, Query::table("synced_blocks").index("synced_blocks_block_idx", KeyRange::eq(["drop"]))).await?, 1);
    Ok(())
}

// R-W7, R-X1, R-X2, R-X3, R-X4: a version check inside a writing transaction, raced, and a writer that waits while readers do not.
#[tokio::test(flavor = "multi_thread")]
async fn a_stale_save_changes_nothing_and_of_two_at_once_one_wins() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let stamp = ts("2026-02-01T00:00:00Z");
    let page = page_insert(w.space, None, "a0", "Contested")
        .set("content", json!({"words": "stored"}))
        .set("text_content", "stored")
        .set("doc_version", 5)
        .set("last_editor_id", w.ann)
        .set("updated_at", stamp);
    let page: Uuid = db.insert(page.returning(["id"])).await?.one()?.get("id")?;
    let target = new_page(&db, w.space, None, "a1", "Target").await?;
    db.insert(Insert::into("page_links").set("source_id", page).set("target_id", target)).await?;

    // The session loaded version 4; the page is at 5. The save is
    // refused and nothing of it is written, not even the derived rows.
    let mut stale = Save::new(page, 4, json!({"words": "stale"}), "stale");
    stale.editor = Some(w.bob);
    stale.blocks = vec![("b1".to_string(), json!({}))];
    assert_eq!(save_page(&db, stale).await?, Saved::Conflict);
    let row = db.get("pages", [page]).await?.expect("the page");
    assert_eq!(
        (row.get::<Json>("content")?, row.get::<String>("text_content")?, row.get::<i64>("doc_version")?),
        (json!({"words": "stored"}), "stored".to_string(), 5)
    );
    assert_eq!((row.get::<Uuid>("last_editor_id")?, row.get::<Timestamp>("updated_at")?), (w.ann, stamp));
    assert_eq!(rows_in(&db, Query::table("page_links").range(KeyRange::eq([page]))).await?, 1);
    assert_eq!(rows_in(&db, Query::table("synced_blocks")).await?, 0);
    assert_eq!(rows_in(&db, Query::table("page_versions")).await?, 0);
    assert_eq!(save_page(&db, Save::new(uuid(404), 0, json!({}), "")).await?, Saved::Gone);

    // Two sessions save from version 5 at the same moment. The second
    // waits for the first, reads version 6, and is refused.
    let attempt = |words: &'static str| {
        let db = db.clone();
        async move { save_page(&db, Save::new(page, 5, json!({"words": words}), words)).await }
    };
    let (a, b) = race(attempt("first"), attempt("second")).await;
    assert_eq!(sorted(vec![a?, b?]), [Saved::Stored(6), Saved::Conflict]);

    // What anyone reads next is the winner's commit, whole.
    assert_eq!(db.query_one(Query::table("pages").key([page]).columns(["doc_version"])).await?.get::<i64>("doc_version")?, 6);
    let row = db.get("pages", [page]).await?.expect("the page");
    let words: String = row.get("text_content")?;
    assert!(words == "first" || words == "second");
    assert_eq!(row.get::<Json>("content")?, json!({"words": words}));
    let versions = db.query(Query::table("page_versions").index("page_versions_page_idx", KeyRange::eq([page]))).await?;
    assert_eq!(column::<Json>(&versions, "content"), [json!({"words": words})]);
    assert_eq!(rows_in(&db, Query::table("page_links").range(KeyRange::eq([page]))).await?, 0);

    // Under row locks, a save that finds the page's row held by another
    // connection waits, does not fail, and then goes on from what the
    // holder committed. Here another writer holds the database, and the
    // save waits at `begin`.
    let holder = db.begin().await?;
    holder.update(Update::table("pages").key([page]).set("title", "Held")).await?;
    let waiting = tokio::spawn({
        let db = db.clone();
        async move { save_page(&db, Save::new(page, 6, json!({"words": "patient"}), "patient")).await }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!waiting.is_finished(), "the save waits for the writer before it");
    // Readers do not wait, and see nothing that is not committed.
    assert_eq!(db.get("pages", [page]).await?.expect("the page").get::<String>("title")?, "Contested");
    holder.commit().await?;
    assert_eq!(waiting.await.expect("the task ran")?, Saved::Stored(7));
    let row = db.get("pages", [page]).await?.expect("the page");
    assert_eq!((row.get::<String>("title")?, row.get::<String>("text_content")?), ("Held".to_string(), "patient".to_string()));
    Ok(())
}

// R-Q8, R-Q10, R-S4, R-T6, R-W3: an insert from a query guarded by `NOT EXISTS` over a time window and by JSON equality.
#[tokio::test(flavor = "multi_thread")]
async fn a_snapshot_is_taken_only_when_none_is_recent_and_the_document_changed() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let page: Uuid = db
        .insert(page_insert(w.space, None, "a0", "History").set("icon", "📜").set("contributor_ids", uuids(&[w.ann])).returning(["id"]))
        .await?
        .one()?
        .get("id")?;
    let versions = || async { rows_in(&db, Query::table("page_versions").index("page_versions_page_idx", KeyRange::eq([page]))).await };
    // Makes every version of the page that much older.
    let age =
        |by: Expr| Update::table("page_versions").index("page_versions_page_idx", KeyRange::eq([page])).set("created_at", col("created_at") - by);
    let save = |version: i64, content: Json| {
        let mut save = Save::new(page, version, content, "words");
        save.editor = Some(w.bob);
        save_page(&db, save)
    };

    // The first save of a page has no version to compare with.
    assert_eq!(save(0, json!({"n": 1})).await?, Saved::Stored(1));
    assert_eq!(versions().await?, 1);
    // A second save right after: a version exists that is seconds old.
    assert_eq!(save(1, json!({"n": 2})).await?, Saved::Stored(2));
    assert_eq!(versions().await?, 1);

    // The page is minutes old, so a version older than a minute is not
    // recent any more.
    db.update(age(minutes(2))).await?;
    assert_eq!(save(2, json!({"n": 3, "meta": {"a": 1, "b": 2}})).await?, Saved::Stored(3));
    assert_eq!(versions().await?, 2);

    // Once the page itself is older than five minutes, so is the window.
    db.update(Update::table("pages").key([page]).set("created_at", now() - hours(1))).await?;
    db.update(age(minutes(2))).await?;
    assert_eq!(save(3, json!({"n": 4})).await?, Saved::Stored(4));
    assert_eq!(versions().await?, 2);

    // No recent version now; but the document is the one the latest
    // version holds, key order aside, and is not copied again.
    db.update(age(minutes(4))).await?;
    assert_eq!(save(4, json!({"meta": {"b": 2, "a": 1}, "n": 3})).await?, Saved::Stored(5));
    assert_eq!(versions().await?, 2);
    assert_eq!(save(5, json!({"n": 5})).await?, Saved::Stored(6));
    assert_eq!(versions().await?, 3);

    // The history, newest first, without the documents.
    let history = Query::table("page_versions")
        .alias("v")
        .index("page_versions_page_idx", KeyRange::eq([page]))
        .limit(500)
        .columns(["v.id", "v.title", "v.created_at", "v.editor_id"]);
    let history = db.query(history).await?;
    let taken: Vec<Timestamp> = column(&history, "created_at");
    assert!(taken[0] > taken[1] && taken[1] > taken[2], "{taken:?}");
    assert_eq!(history.columns(), ["id", "title", "created_at", "editor_id"]);

    // One version, whole. It holds the page as the save left it.
    let newest = db.query_one(Query::table("page_versions").key([ids(&history)[0]])).await?;
    assert_eq!(
        (newest.get::<Uuid>("page_id")?, newest.get::<String>("title")?, newest.get::<String>("icon")?),
        (page, "History".to_string(), "📜".to_string())
    );
    assert_eq!(
        (newest.get::<Json>("content")?, newest.get::<Uuid>("editor_id")?, newest.get::<Vec<Uuid>>("contributor_ids")?),
        (json!({"n": 5}), w.bob, vec![w.ann])
    );
    let middle = db.query_one(Query::table("page_versions").key([ids(&history)[1]]).columns(["content"])).await?;
    assert_eq!(middle.get::<Json>("content")?, json!({"n": 3, "meta": {"a": 1, "b": 2}}));
    Ok(())
}

// R-Q8, R-Q10, R-W3, R-W5: recipients found with `NOT EXISTS` over a time window, and an insert from a query that checks access as it writes.
#[tokio::test(flavor = "multi_thread")]
async fn watchers_of_an_edited_page_are_told_once_per_window() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let (ann, bob, space) = (w.ann, w.bob, w.space);
    let cat = new_user(&db, "cat").await?;
    let dan = new_user(&db, "dan").await?;
    let eve = new_user(&db, "eve").await?;
    let zed = new_user(&db, "zed").await?;
    let page = new_page(&db, space, None, "a0", "Edited").await?;
    let readers = new_group(&db, "readers").await?;
    join_group(&db, readers, cat).await?;
    grant_group(&db, space, readers, "reader").await?;
    grant(&db, space, bob, "reader").await?;
    grant(&db, space, dan, "reader").await?;
    grant(&db, space, zed, "reader").await?;
    db.update(Update::table("users").key([zed]).set("deactivated_at", now())).await?;

    // After a save its editors watch the page. Only users that
    // exist are added, and who watches already is skipped.
    let watch = |users: &[Uuid]| {
        let rows = Query::table("users").keys(users.iter().map(|id| [*id])).select("user_id", col("id")).select("page_id", val(page));
        Insert::into("watchers").columns(["user_id", "page_id"]).from_query(rows).on_conflict_do_nothing_on("watchers_page_key")
    };
    assert_eq!(db.insert(watch(&[ann, cat, uuid(404)])).await?.affected(), 2);
    assert_eq!(db.insert(watch(&[ann, cat, dan, eve, zed])).await?.affected(), 3);
    db.insert(Insert::into("watchers").set("user_id", bob).set("space_id", space)).await?;
    db.insert(Insert::into("watchers").set("user_id", cat).set("space_id", space)).await?;
    // Who watches the page.
    let watching = db.query(Query::table("watchers").index("watchers_page_idx", KeyRange::eq([page])).select("id", col("user_id"))).await?;
    assert_eq!(sorted(ids(&watching)), sorted(vec![ann, cat, dan, eve, zed]));

    let told = |user: Uuid, ago: Expr| {
        Insert::into("notifications").set("user_id", user).set("kind", "page.updated").set("page_id", page).set("created_at", now() - ago)
    };
    db.insert(told(dan, hours(1))).await?;
    db.insert(told(eve, hours(8))).await?;
    db.insert(Insert::into("notifications").set("user_id", bob).set("kind", "comment.created").set("page_id", page)).await?;

    // Who to tell about the edit. Watchers of the page or of its
    // space, each once; not the editors themselves; not anyone told about
    // this page in the last seven hours.
    let recently_told = || {
        let told = Query::table("notifications").alias("n").index("notifications_user_idx", KeyRange::eq([col("w.user_id")]));
        told.filter(all([col("n.page_id").eq(page), col("n.kind").eq("page.updated"), col("n.created_at").gt(now() - hours(7))]))
    };
    let watching = || Query::table("watchers").alias("w").filter(col("w.page_id").eq(page).or(col("w.space_id").eq(space)));
    let to_tell = watching().filter(col("w.user_id").not_in_list(uuids(&[ann]))).filter(exists(recently_told()).not());
    let to_tell: Vec<Uuid> = db.query(to_tell.select("user_id", col("w.user_id")).distinct()).await?.column()?;
    assert_eq!(to_tell.len(), 4);
    assert_eq!(sorted(to_tell.clone()), sorted(vec![bob, cat, eve, zed]));

    // One notification for each of them who is active and can read
    // the page's space, checked inside the insert; the rows come back.
    let notify = |recipients: &[Uuid]| {
        let can_read = Query::table("space_members")
            .alias("sm")
            .index("space_members_space_idx", KeyRange::eq([col("t.space_id")]))
            .filter(grant_reaches(col("u.id")));
        let rows = Query::table("users")
            .alias("u")
            .keys(recipients.iter().map(|id| [*id]))
            .join(Query::table("pages").alias("t").key([page]).filter(col("t.deleted_at").is_null()))
            .filter(col("u.deactivated_at").is_null())
            .filter(exists(can_read))
            .select("user_id", col("u.id"))
            .select("kind", val("page.updated"))
            .select("actor_id", val(ann))
            .select("page_id", val(page))
            .select("comment_id", null());
        Insert::into("notifications").columns(["user_id", "kind", "actor_id", "page_id", "comment_id"]).from_query(rows).returning(["id", "user_id"])
    };
    let inserted = db.insert(notify(&to_tell)).await?;
    assert_eq!(inserted.affected(), 2);
    assert_eq!(sorted(column::<Uuid>(&inserted, "user_id")), sorted(vec![bob, cat]));
    // The statement's last join in Postgres: where to push and email.
    let recipients = db
        .query(Query::table("users").keys(column::<Uuid>(&inserted, "user_id").into_iter().map(|id| [id])).columns(["id", "email", "preferences"]))
        .await?;
    assert_eq!(sorted(column::<String>(&recipients, "email")), ["bob@example.com", "cat@example.com"]);
    let stored = db.get("notifications", [ids(&inserted)[0]]).await?.expect("the notification");
    assert_eq!((stored.get::<Json>("data")?, stored.is_null("read_at")?, stored.is_null("comment_id")?), (json!({}), true, true));

    // Stamping the ones that were emailed.
    let mailed =
        db.update(Update::rows(Query::table("notifications").keys(ids(&inserted).into_iter().map(|id| [id]))).set("emailed_at", now())).await?;
    assert_eq!(mailed.affected(), 2);
    assert_eq!(rows_in(&db, Query::table("notifications").filter(col("emailed_at").is_not_null())).await?, 2);

    // The next edit, minutes later: they have just been told.
    let again = watching().filter(exists(recently_told()));
    let again: Vec<Uuid> = db.query(again.select("user_id", col("w.user_id")).distinct()).await?.column()?;
    assert_eq!(sorted(again), sorted(vec![bob, cat, dan]));

    // A page in the trash tells nobody.
    db.update(Update::table("pages").key([page]).set("deleted_at", now())).await?;
    assert_eq!(db.insert(notify(&[bob, cat])).await?.affected(), 0);
    Ok(())
}

// ------------------------------------------------------------------ search

/// What the caller makes of a search box's text before it asks: lower
/// case, the words in it, and the text as a `like` pattern.
struct Needle {
    raw: String,
    words: Vec<String>,
    like: String,
}

fn needle(text: &str) -> Needle {
    let raw = text.trim().to_lowercase();
    let mut words: Vec<String> = Vec::new();
    for word in raw.split(|c: char| !c.is_alphanumeric()).filter(|word| !word.is_empty()) {
        if !words.iter().any(|seen| seen == word) {
            words.push(word.to_string());
        }
    }
    words.truncate(8);
    Needle { like: format!("%{}%", like_escape(&raw)), raw, words }
}

fn lower_title() -> Expr {
    col("p.title").or_else("").lower()
}

/// The snippet, cut by the database: 400 characters of the page's text
/// from 100 before the first occurrence of `word`, or from the start when
/// it does not occur.
fn snippet(word: &str) -> Expr {
    let text = col("p.text_content");
    text.clone().substr(greatest([val(1), text.lower().strpos(word) - 100]), 400)
}

/// What a search returns for a page, whichever branch found it.
fn search_columns(pages: Query, needle: &Needle, rank: Expr) -> Query {
    let word = needle.words.first().unwrap_or(&needle.raw);
    tree_columns(pages)
        .columns(["p.updated_at"])
        .select("space", space_ref())
        .select("snippet", snippet(word))
        .select("exact", lower_title().eq(needle.raw.as_str()))
        .select("rank", rank)
}

/// The first branch of a search: pages whose title or text holds every
/// word of the needle, each as the start of a word. In order: a title
/// that is exactly the needle, then rank, then the latest edit.
fn full_text_hits(needle: &Needle, scope: Expr) -> Query {
    let pages = Query::table("pages")
        .alias("p")
        .search("pages_search_idx", no_prefix(), needle.words.join(" "))
        .match_prefix()
        .join(Query::table("spaces").alias("s").key([col("p.space_id")]))
        .filter(col("p.deleted_at").is_null())
        .filter(scope);
    search_columns(pages, needle, Expr::Rank)
        .order_by(lower_title().eq(needle.raw.as_str()).desc())
        .order_by(Expr::Rank.desc())
        .order_by(col("p.updated_at").desc())
        .order_by(col("p.id"))
}

/// The second branch of a search: the needle anywhere in the title. No
/// index serves it, in Postgres either: it reads every live page.
fn title_hits(needle: &Needle, scope: Expr) -> Query {
    let pages = Query::table("pages")
        .alias("p")
        .index("pages_updated_idx", KeyRange::all())
        .join(Query::table("spaces").alias("s").key([col("p.space_id")]))
        .filter(scope)
        .filter(lower_title().like(needle.like.as_str()));
    search_columns(pages, needle, val(0.0))
}

struct Hit {
    id: Uuid,
    exact: bool,
    rank: f64,
    updated_at: Timestamp,
    snippet: Option<String>,
}

fn hits(rows: Rows) -> DbResult<Vec<Hit>> {
    rows.into_iter()
        .map(|row| {
            Ok(Hit {
                id: row.get("id")?,
                exact: row.get("exact")?,
                rank: row.get("rank")?,
                updated_at: row.get("updated_at")?,
                snippet: row.get("snippet")?,
            })
        })
        .collect()
}

/// The whole search. In Postgres one statement says "matches the words OR
/// has the text in its title"; a search reads only what its index finds,
/// so the two branches are two reads of one moment, merged here: a page
/// found only by its title ranks zero.
async fn search(db: &Client, text: &str, scope: Expr) -> DbResult<Vec<Hit>> {
    let needle = needle(text);
    let mut found = db
        .read(|tx| async move {
            let mut found = if needle.words.is_empty() { Vec::new() } else { hits(tx.query(full_text_hits(&needle, scope.clone())).await?)? };
            for hit in hits(tx.query(title_hits(&needle, scope)).await?)? {
                if !found.iter().any(|seen: &Hit| seen.id == hit.id) {
                    found.push(hit);
                }
            }
            Ok::<_, DbError>(found)
        })
        .await?;
    found.sort_by(|a, b| b.exact.cmp(&a.exact).then(b.rank.total_cmp(&a.rank)).then(b.updated_at.cmp(&a.updated_at)).then(a.id.cmp(&b.id)));
    Ok(found)
}

fn hit_numbers(found: &[Hit]) -> Vec<u128> {
    found.iter().map(|hit| u128::from_be_bytes(hit.id.0)).collect()
}

/// When the search found nothing: the needle anywhere in the text of a
/// page, whatever its case; the latest edits first, twenty at most. The
/// index over live pages is in that order already.
fn fallback_hits(text: &str, scope: Expr, limit: u64) -> Query {
    let text = text.trim();
    let pages = Query::table("pages")
        .alias("p")
        .index("pages_updated_idx", KeyRange::all())
        .join(Query::table("spaces").alias("s").key([col("p.space_id")]))
        .filter(scope)
        .filter(col("p.text_content").ilike(format!("%{}%", like_escape(text))));
    tree_columns(pages).columns(["p.updated_at"]).select("space", space_ref()).select("snippet", snippet(&text.to_lowercase())).limit(limit.min(20))
}

struct Library {
    ann: Uuid,
    bob: Uuid,
    space: Uuid,
    hidden: Uuid,
}

/// Pages to search, numbered: page n was last edited on the n-th of
/// March. Ann reads `docs`; nobody was granted `hidden`.
async fn library(db: &Client) -> DbResult<Library> {
    let w = wiki(db).await?;
    let hidden = new_space(db, "hidden").await?;
    let page = |n: u128, space: Uuid, title: &str, text: Option<&str>| {
        page_insert(space, None, "a0", title).set("id", uuid(n)).set("text_content", text).set("updated_at", ts(&format!("2026-03-{n:02}T12:00:00Z")))
    };
    db.insert(page(1, w.space, "Roadmap", Some("Plans for the year."))).await?;
    db.insert(page(2, w.space, "Roadmap roadmap 2026", Some("The roadmap in detail: roadmap."))).await?;
    db.insert(page(3, w.space, "Notes", Some("See the roadmap."))).await?;
    db.insert(page(4, w.space, "Minutes", Some("See the roadmap."))).await?;
    db.insert(page(5, w.space, "Admin guide", Some("How to administer: well-known apple_pie v2 invoices."))).await?;
    db.insert(page(6, w.space, "Roadmap", Some("roadmap")).set("deleted_at", now())).await?;
    db.insert(page(7, hidden, "Roadmap", Some("roadmap"))).await?;
    db.insert(page(8, w.space, "Unrelated", None)).await?;
    db.insert(page(9, w.space, "100% done_deal", Some("Progress report."))).await?;
    db.insert(page(10, w.space, "100 x d", None)).await?;
    Ok(Library { ann: w.ann, bob: w.bob, space: w.space, hidden })
}

// R-Q17: full-text matching: every word required, prefixes on request, no stemming, and an index that follows updates.
#[tokio::test(flavor = "multi_thread")]
async fn search_requires_every_word_as_a_prefix_and_does_not_stem() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let lib = library(&db).await?;
    let found = |words: &'static str, prefix: bool| {
        let db = db.clone();
        async move {
            let pages = Query::table("pages").alias("p").search("pages_search_idx", no_prefix(), words);
            let pages = if prefix { pages.match_prefix() } else { pages };
            Ok::<_, DbError>(sorted(numbers(&db.query(pages.filter(readable(lib.ann)).columns(["p.id"])).await?)))
        }
    };

    // A word matches in the title or in the text, whatever its case.
    assert_eq!(found("roadmap", false).await?, [1, 2, 3, 4]);
    assert_eq!(found("ROADMAP", false).await?, [1, 2, 3, 4]);
    // Whole words only, unless the query asks for prefixes.
    assert!(found("road", false).await?.is_empty());
    assert_eq!(found("road", true).await?, [1, 2, 3, 4]);
    // Every word is required; they may be in different fields, in any order.
    assert_eq!(found("road det", true).await?, [2]);
    assert_eq!(found("detail road", true).await?, [2]);
    assert_eq!(found("roadmap minutes", true).await?, [4]);
    assert!(found("roadmap zebra", true).await?.is_empty());
    // The `simple` configuration: no stemming, so a longer form does not
    // find a shorter one, and no dropped words.
    assert!(found("roadmaps", true).await?.is_empty());
    assert!(found("invoice", false).await?.is_empty());
    assert_eq!(found("invoice", true).await?, [5]);
    assert_eq!(found("invoices", false).await?, [5]);
    assert_eq!(found("the", false).await?, [1, 2, 3, 4]);
    // Words are runs of letters and digits; punctuation only separates.
    assert_eq!(found("well known", false).await?, [5]);
    assert_eq!(found("apple pie", false).await?, [5]);
    assert_eq!(found("v2", false).await?, [5]);
    assert_eq!(found("2026", false).await?, [2]);
    assert_eq!(found("100", false).await?, [9, 10]);

    // The index knows nothing of the trash or of permissions: without the
    // scope it finds the trashed page and the page in another space too.
    let unscoped = db.query(Query::table("pages").search("pages_search_idx", no_prefix(), "roadmap").columns(["id"])).await?;
    assert_eq!(sorted(numbers(&unscoped)), [1, 2, 3, 4, 6, 7]);
    // The index follows every update: a page is found by the words it
    // holds now, and no longer by the ones it held.
    db.update(Update::table("pages").key([uuid(8)]).set("title", "Roadmap appendix")).await?;
    db.update(Update::table("pages").key([uuid(1)]).set("title", "Plan").set("text_content", "Nothing")).await?;
    assert_eq!(found("roadmap", false).await?, [2, 3, 4, 8]);
    Ok(())
}

// R-Q5, R-Q15, R-Q17, R-Q21: search results ordered by an exact title, then rank, then recency, with a snippet cut by the database.
#[tokio::test(flavor = "multi_thread")]
async fn search_puts_an_exact_title_first_then_rank_then_recency() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let lib = library(&db).await?;

    let asked = needle("  Roadmap ");
    assert_eq!((asked.raw.as_str(), asked.like.as_str()), ("roadmap", "%roadmap%"));
    let rows = db.query(full_text_hits(&asked, readable(lib.ann))).await?;
    // Page 1's title is the needle. Page 2 holds the word twice in its
    // title and twice in its text; pages 3 and 4 once in the same text,
    // and page 4 was edited later.
    assert_eq!(numbers(&rows), [1, 2, 4, 3]);
    assert_eq!(column::<bool>(&rows, "exact"), [true, false, false, false]);
    let rank: Vec<f64> = column(&rows, "rank");
    assert!(rank[1] > rank[0], "more matches rank higher: {rank:?}");
    assert!(rank[0] > rank[2], "a title match outranks a text match: {rank:?}");
    assert!(rank[2] == rank[3] && rank[3] > 0.0, "{rank:?}");
    assert_eq!(rows.first().expect("a hit").get::<Json>("space")?["slug"], json!("docs"));

    // The same order, paged.
    assert_eq!(numbers(&db.query(full_text_hits(&asked, readable(lib.ann)).limit(2).offset(1)).await?), [2, 4]);

    // The snippet: the text around the first word; from the start of the
    // text when the word is only in the title; NULL without a text.
    let snippets: Vec<Option<String>> = column(&rows, "snippet");
    assert_eq!(snippets[0].as_deref(), Some("Plans for the year."));
    assert_eq!(snippets[3].as_deref(), Some("See the roadmap."));
    let title_only = db.query_one(full_text_hits(&needle("unrelated"), readable(lib.ann))).await?;
    assert!(title_only.is_null("snippet")?);

    // The merged search gives the same four: the title branch finds
    // pages 1 and 2 again, and they are counted once.
    let merged = search(&db, "Roadmap", readable(lib.ann)).await?;
    assert_eq!(hit_numbers(&merged), [1, 2, 4, 3]);
    assert!(merged[0].exact && merged[1].rank > merged[0].rank);
    Ok(())
}

// R-Q7, R-Q17, R-Q21: a full-text read and a `like` scan of one moment, merged by the caller.
#[tokio::test(flavor = "multi_thread")]
async fn a_title_substring_joins_the_results_at_rank_zero() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let lib = library(&db).await?;
    let scope = || readable(lib.ann);

    // No word starts with "oadma": the words find nothing, the titles do.
    assert!(db.query(full_text_hits(&needle("oadma"), scope())).await?.is_empty());
    let found = search(&db, "oadma", scope()).await?;
    assert_eq!(hit_numbers(&found), [2, 1]);
    assert!(found.iter().all(|hit| hit.rank == 0.0 && !hit.exact));

    // "min" starts "Minutes" and sits inside "Admin guide": the word
    // match comes first, the title-only match after it.
    let found = search(&db, "min", scope()).await?;
    assert_eq!(hit_numbers(&found), [4, 5]);
    assert!(found[0].rank > 0.0 && found[1].rank == 0.0);

    // Several words: all of them as words, or the whole text in a title.
    assert_eq!(hit_numbers(&search(&db, "ROADMAP 2026", scope()).await?), [2]);
    assert_eq!(hit_numbers(&search(&db, "guide admin", scope()).await?), [5]);

    // `%` and `_` in the needle are characters. "0% d" is in the title of
    // page 9 and not in "100 x d", which the bare pattern would match.
    let found = search(&db, "0% d", scope()).await?;
    assert_eq!(hit_numbers(&found), [9]);
    assert_eq!(hit_numbers(&search(&db, "done_d", scope()).await?), [9]);
    // Text with no word in it skips the full-text branch altogether.
    assert_eq!(hit_numbers(&search(&db, "%", scope()).await?), [9]);
    assert!(search(&db, "zebra", scope()).await?.is_empty());

    // A title that is the needle itself leads the merged list too.
    db.update(Update::table("pages").key([uuid(10)]).set("title", "oadma")).await?;
    let found = search(&db, "Oadma", scope()).await?;
    assert_eq!(hit_numbers(&found), [10, 2, 1]);
    assert!(found[0].exact);
    Ok(())
}

// R-Q4, R-Q7, R-Q15, R-Q16: a case-insensitive substring scan in index order, and string functions that count characters.
#[tokio::test(flavor = "multi_thread")]
async fn the_fallback_scan_finds_text_inside_words_and_the_snippet_counts_characters() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let lib = library(&db).await?;
    let scope = || readable(lib.ann);
    let page = |n: u128, title: &str, text: &str| {
        page_insert(lib.space, None, "a0", title)
            .set("id", uuid(n))
            .set("text_content", text)
            .set("updated_at", ts(&format!("2026-03-{n:02}T12:00:00Z")))
    };
    db.insert(page(11, "Progress", "Progress: 100%_done today")).await?;
    db.insert(page(12, "Countdown", "1000 x_done later")).await?;
    let long = format!("{} Needle in a haystack.{}", "é".repeat(150), " tail".repeat(100));
    db.insert(page(13, "Long", &long)).await?;

    // Neither branch of the search finds text that starts inside a word.
    assert!(search(&db, "oadmap in det", scope()).await?.is_empty());
    let found = db.query(fallback_hits("OADMAP in Det", scope(), 50)).await?;
    assert_eq!(numbers(&found), [2]);
    assert_eq!(found.first().expect("a hit").get::<String>("snippet")?, "The roadmap in detail: roadmap.");

    // The latest edit first, and no more than asked for, or twenty.
    assert_eq!(numbers(&db.query(fallback_hits("ee the road", scope(), 50)).await?), [4, 3]);
    assert_eq!(numbers(&db.query(fallback_hits("ee the road", scope(), 1)).await?), [4]);
    assert_eq!(fallback_hits("x", scope(), 50).limit, Some(20));

    // The user's `%` and `_` are escaped: page 12 holds "100", then some
    // characters, then "d", and does not match.
    assert_eq!(numbers(&db.query(fallback_hits("100%_D", scope(), 50)).await?), [11]);

    // The snippet starts 100 characters, not bytes, before the match and
    // runs for 400; the text before it is 150 two-byte letters.
    let expected: String = long.chars().skip(51).take(400).collect();
    let found = search(&db, "needle", scope()).await?;
    assert_eq!(hit_numbers(&found), [13]);
    assert_eq!(found[0].snippet.as_deref(), Some(expected.as_str()));
    assert_eq!(expected.chars().count(), 400);
    let scanned = db.query_one(fallback_hits("EEDLE IN A HAY", scope(), 50)).await?;
    assert_eq!(scanned.get::<String>("snippet")?, long.chars().skip(52).take(400).collect::<String>());
    // The pieces the cut is made of, one by one.
    let pieces = Query::table("pages")
        .alias("p")
        .key([uuid(13)])
        .select("at", col("p.text_content").lower().strpos("needle"))
        .select("missing", col("p.text_content").strpos("zebra"))
        .select("chars", col("p.text_content").length())
        .select("bytes", col("p.text_content").octet_length())
        .select("start", col("p.text_content").left(3));
    let pieces = db.query_one(pieces).await?;
    assert_eq!(
        (pieces.get::<i64>("at")?, pieces.get::<i64>("missing")?, pieces.get::<i64>("chars")?, pieces.get::<i64>("bytes")?),
        (152, 0, 672, 822)
    );
    assert_eq!(pieces.get::<String>("start")?, "ééé");
    Ok(())
}

// R-Q10, R-Q17, R-Q19: a search scoped by a subquery of readable spaces, or by a walk of the tree.
#[tokio::test(flavor = "multi_thread")]
async fn search_stays_inside_readable_spaces_or_a_shared_subtree() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let lib = library(&db).await?;
    let extra = new_space(&db, "extra").await?;
    grant(&db, extra, lib.ann, "reader").await?;
    db.insert(page_insert(extra, None, "a0", "Roadmap extras").set("id", uuid(14)).set("updated_at", ts("2026-03-14T12:00:00Z"))).await?;

    // The scope of a signed-in search: spaces the user reads, and one of
    // them if the user chose one (`$s IS NULL OR p.space_id = $s`).
    let scope = |user: Uuid, space: Option<Uuid>| readable(user).and(val(space).is_null().or(col("p.space_id").eq(space)));
    assert_eq!(hit_numbers(&search(&db, "roadmap", scope(lib.ann, None)).await?), [1, 2, 14, 4, 3]);
    assert_eq!(hit_numbers(&search(&db, "roadmap", scope(lib.ann, Some(lib.space))).await?), [1, 2, 4, 3]);
    assert_eq!(hit_numbers(&search(&db, "roadmap", scope(lib.ann, Some(extra))).await?), [14]);
    // A space the user was not granted is no scope at all.
    assert!(search(&db, "roadmap", scope(lib.ann, Some(lib.hidden))).await?.is_empty());
    assert!(search(&db, "roadmap", scope(lib.bob, None)).await?.is_empty());
    grant(&db, lib.hidden, lib.bob, "reader").await?;
    assert_eq!(hit_numbers(&search(&db, "roadmap", scope(lib.bob, None)).await?), [7]);

    // Title-only search: the words are looked for in the title alone,
    // computed per row as Postgres does, and the snippet is the title.
    let titled = |text: &str, scope: Expr| {
        let needle = needle(text);
        let words = || [lower_title(), val(needle.words.join(" ")), val("simple"), val(true)];
        let pages = Query::table("pages")
            .alias("p")
            .index("pages_updated_idx", KeyRange::all())
            .filter(scope)
            .filter(call(Func::Matches, words()).or(lower_title().like(needle.like.as_str())));
        pages
            .columns(["p.id"])
            .select("snippet", col("p.title"))
            .select("rank", call(Func::MatchRank, words()))
            .order_by(lower_title().eq(needle.raw.as_str()).desc())
            .order_by(call(Func::MatchRank, words()).desc())
            .order_by(col("p.updated_at").desc())
            .order_by(col("p.id"))
    };
    let by_title = db.query(titled("roadmap", scope(lib.ann, None))).await?;
    assert_eq!(numbers(&by_title), [1, 2, 14]);
    assert_eq!(column::<String>(&by_title, "snippet"), ["Roadmap", "Roadmap roadmap 2026", "Roadmap extras"]);
    let rank: Vec<f64> = column(&by_title, "rank");
    assert!(rank[1] > rank[2] && rank[2] > 0.0, "{rank:?}");
    assert_eq!(numbers(&db.query(titled("road", scope(lib.ann, Some(lib.space)))).await?), [2, 1]);

    // A public share searches its page, or its page and the live
    // pages under it: a walk of the tree as the scope of a search.
    let shared = uuid(2);
    let child = |n: u128, parent: u128, title: &str| {
        page_insert(lib.space, Some(uuid(parent)), "a0", title)
            .set("id", uuid(n))
            .set("text_content", "roadmap")
            .set("updated_at", ts(&format!("2026-03-{n:02}T12:00:00Z")))
    };
    db.insert(child(20, 2, "Child")).await?;
    db.insert(child(21, 20, "Trashed grandchild").set("deleted_at", now())).await?;
    db.insert(child(22, 21, "Under the trashed one")).await?;
    let only_page = col("p.id").eq(shared);
    let with_children = col("p.id").in_list(list_of(subtree(shared).prune(col("deleted_at").is_null()).columns(["id"])));
    assert_eq!(hit_numbers(&search(&db, "roadmap", only_page).await?), [2]);
    assert_eq!(hit_numbers(&search(&db, "roadmap", with_children.clone()).await?), [2, 20]);
    assert_eq!(hit_numbers(&search(&db, "child", with_children).await?), [20]);
    Ok(())
}

// --------------------------------------- comments, files, shares, labels

/// Comments with their author and their resolver as small objects.
fn comments_with_people(comments: Query) -> Query {
    comments
        .alias("c")
        .left_join(Query::table("users").alias("a").key([col("c.author_id")]))
        .left_join(Query::table("users").alias("r").key([col("c.resolved_by")]))
        .columns([
            "c.id", "c.page_id", "c.parent_id", "c.author_id", "c.body", "c.quote", "c.anchored", "c.created_at", "c.edited_at", "c.resolved_at",
        ])
        .select("author", user_ref("a"))
        .select("resolver", user_ref("r"))
}

// R-Q9, R-Q13, R-S6, R-W6, R-W11: a thread read in index order with two left joins, and a self-referencing cascade.
#[tokio::test(flavor = "multi_thread")]
async fn a_comment_thread_reads_with_its_people_and_falls_with_its_first_comment() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let (ann, bob) = (w.ann, w.bob);
    db.update(Update::table("users").key([ann]).set("avatar", "ann.png")).await?;
    let page = new_page(&db, w.space, None, "a0", "Discussed").await?;
    let other_page = new_page(&db, w.space, None, "a1", "Quiet").await?;
    let (first, second, third) = (ts("2026-03-01T09:00:00Z"), ts("2026-03-01T10:00:00Z"), ts("2026-03-01T11:00:00Z"));

    // Writing comments: two threads, one of them with replies.
    let comment = |n: u128, parent: Option<u128>, author: Option<Uuid>, at: Timestamp| {
        let body = json!({"type": "doc", "content": [{"type": "text", "text": format!("comment {n}")}]});
        let comment = Insert::into("comments")
            .set("id", uuid(n))
            .set("page_id", page)
            .set("parent_id", parent.map(uuid))
            .set("author_id", author)
            .set("body", body);
        comment.set("quote", parent.is_none().then_some("the plan")).set("anchored", parent.is_none()).set("created_at", at).returning(["id"])
    };
    assert_eq!(db.insert(comment(11, None, Some(ann), first)).await?.one()?.get::<Uuid>("id")?, uuid(11));
    db.insert(comment(13, Some(11), Some(bob), second)).await?;
    db.insert(comment(12, None, None, second)).await?;
    db.insert(comment(14, Some(11), Some(ann), third)).await?;
    let no_body = db.insert(Insert::into("comments").set("page_id", page).set("author_id", ann)).await.unwrap_err();
    assert_eq!(no_body.code, ErrorCode::NotNullViolation);
    assert!(no_body.violates("body"));

    // The thread of a page, oldest first; the index is in that order.
    let thread = db.query(comments_with_people(Query::table("comments").index("comments_page_idx", KeyRange::eq([page])))).await?;
    assert_eq!(numbers(&thread), [11, 12, 13, 14]);
    assert_eq!(column::<Option<Uuid>>(&thread, "parent_id"), [None, None, Some(uuid(11)), Some(uuid(11))]);
    let people: Vec<Option<Json>> = column(&thread, "author");
    assert_eq!(people[0], Some(json!({"id": ann.to_string(), "name": "ann", "avatarUrl": "/api/images/ann.png"})));
    assert_eq!(people[1], None);
    assert_eq!(people[2], Some(json!({"id": bob.to_string(), "name": "bob", "avatarUrl": null})));
    assert!(thread.iter().all(|row| row.is_null("resolver").expect("the column")));
    assert_eq!(thread.first().expect("a comment").get::<Json>("body")?["content"][0]["text"], json!("comment 11"));
    let one = db.query_one(comments_with_people(Query::table("comments").key([uuid(13)]))).await?;
    assert_eq!((one.get::<bool>("anchored")?, one.is_null("quote")?), (false, true));

    // A comment of this page, and what it replies to.
    let of_page = |id: Uuid, page: Uuid| Query::table("comments").key([id]).filter(col("page_id").eq(page)).columns(["id", "parent_id"]);
    assert_eq!(db.query_one(of_page(uuid(13), page)).await?.get::<Uuid>("parent_id")?, uuid(11));
    assert!(db.query_opt(of_page(uuid(13), other_page)).await?.is_none());

    // Everyone who wrote in a thread, each once.
    let in_thread = |root: Uuid| {
        let thread =
            Query::table("comments").index("comments_page_idx", KeyRange::eq([page])).filter(col("id").eq(root).or(col("parent_id").eq(root)));
        thread.filter(col("author_id").is_not_null()).select("id", col("author_id")).distinct()
    };
    assert_eq!(sorted(ids(&db.query(in_thread(uuid(11))).await?)), sorted(vec![ann, bob]));
    assert!(db.query(in_thread(uuid(12))).await?.is_empty());

    // Editing a comment stamps the edit and keeps when it was written.
    db.update(
        Update::table("comments")
            .key([uuid(13)])
            .set("body", json!({"type": "doc", "edited": true}))
            .set("edited_at", now())
            .set("updated_at", now()),
    )
    .await?;
    let edited = db.get("comments", [uuid(13)]).await?.expect("the comment");
    assert_eq!(edited.get::<Json>("body")?, json!({"type": "doc", "edited": true}));
    assert!(edited.get::<Timestamp>("edited_at")? > third && edited.get::<Timestamp>("created_at")? == second);

    // Resolving keeps the first moment and names the latest
    // resolver; unresolving clears both.
    let resolve = |by: Uuid| {
        Update::table("comments")
            .key([uuid(11)])
            .set("resolved_at", coalesce([col("resolved_at"), now()]))
            .set("resolved_by", by)
            .set("updated_at", now())
            .returning(["resolved_at"])
    };
    let at: Timestamp = db.update(resolve(ann)).await?.one()?.get("resolved_at")?;
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(db.update(resolve(bob)).await?.one()?.get::<Timestamp>("resolved_at")?, at);
    let resolved = db.query_one(comments_with_people(Query::table("comments").key([uuid(11)]))).await?;
    assert_eq!(resolved.get::<Json>("resolver")?["name"], json!("bob"));
    db.update(Update::table("comments").key([uuid(11)]).set("resolved_at", null()).set("resolved_by", null()).set("updated_at", now())).await?;
    assert!(db.get("comments", [uuid(11)]).await?.expect("the comment").is_null("resolved_at")?);

    // Deleting a comment takes its replies, and the notifications
    // about any of them.
    for about in [11, 13, 12] {
        db.insert(Insert::into("notifications").set("user_id", ann).set("kind", "comment.reply").set("page_id", page).set("comment_id", uuid(about)))
            .await?;
    }
    assert_eq!(db.delete(Delete::from("comments").key([uuid(11)])).await?.affected(), 1);
    assert_eq!(numbers(&db.query(Query::table("comments").columns(["id"])).await?), [12]);
    let left = db.query(Query::table("notifications").columns(["comment_id"])).await?;
    assert_eq!(column::<Uuid>(&left, "comment_id"), [uuid(12)]);
    Ok(())
}

// R-Q2, R-Q6, R-S6, R-S7, R-W10, R-W11: file rows with a check on their purpose, listed through an index and deleted by an unindexed column.
#[tokio::test(flavor = "multi_thread")]
async fn files_are_listed_by_page_and_removed_by_storage_key() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let page = new_page(&db, w.space, None, "a0", "With files").await?;
    let other = new_page(&db, w.space, None, "a1", "With one file").await?;

    // A page's file under an id the caller chose, and an image under
    // one the database made.
    let upload = |n: u128, page: Uuid, name: &str, key: &str, at: &str| {
        let file =
            Insert::into("files").set("id", uuid(n)).set("purpose", "page").set("page_id", page).set("space_id", w.space).set("uploader_id", w.ann);
        file.set("name", name).set("mime", "application/pdf").set("size", 1024).set("storage_key", key).set("created_at", ts(at))
    };
    db.insert(upload(1, page, "a.pdf", "key-1", "2026-03-01T00:00:00Z")).await?;
    db.insert(upload(2, page, "b.pdf", "key-2", "2026-03-03T00:00:00Z")).await?;
    db.insert(upload(3, page, "c.pdf", "key-2", "2026-03-02T00:00:00Z")).await?;
    db.insert(upload(4, other, "d.pdf", "key-4", "2026-03-04T00:00:00Z")).await?;
    let image = Insert::into("files")
        .set("purpose", "avatar")
        .set("space_id", null())
        .set("uploader_id", w.ann)
        .set("name", "me.png")
        .set("storage_key", "key-avatar");
    let image = db.insert(image.returning(["id", "mime", "size", "page_id"])).await?.one()?;
    assert_eq!(
        (image.get::<String>("mime")?, image.get::<i64>("size")?, image.is_null("page_id")?),
        ("application/octet-stream".to_string(), 0, true)
    );
    let same_id = db.insert(upload(1, page, "again.pdf", "key-9", "2026-03-05T00:00:00Z")).await.unwrap_err();
    assert!(same_id.violates(PRIMARY));
    let bad_purpose = db.insert(Insert::into("files").set("purpose", "cover").set("name", "x").set("storage_key", "k")).await.unwrap_err();
    assert_eq!(bad_purpose.code, ErrorCode::CheckViolation);
    assert!(bad_purpose.violates("files_purpose_check"));
    // The page vanished between the access check and the insert.
    let orphan = db.insert(upload(9, uuid(404), "x.pdf", "key-9", "2026-03-05T00:00:00Z")).await.unwrap_err();
    assert_eq!(orphan.code, ErrorCode::ForeignKeyViolation);

    // On every download, one file by id, if it is a page's file;
    // and for a replacement, if it is also in this space.
    let page_file = |id: Uuid| {
        Query::table("files")
            .key([id])
            .filter(col("purpose").eq("page"))
            .columns(["id", "page_id", "space_id", "name", "mime", "size", "storage_key", "updated_at"])
    };
    assert_eq!(db.query_one(page_file(uuid(1))).await?.get::<String>("storage_key")?, "key-1");
    assert!(db.query_opt(page_file(image.get::<Uuid>("id")?)).await?.is_none());
    assert!(db.query_opt(page_file(uuid(1)).filter(col("space_id").eq(uuid(404)))).await?.is_none());

    // A page's files, newest first; and the files of several pages.
    let of_page = Query::table("files")
        .index("files_page_idx", KeyRange::eq([page]))
        .filter(col("purpose").eq("page"))
        .order_by(col("created_at").desc())
        .limit(500);
    assert_eq!(numbers(&db.query(of_page.columns(["id", "name", "mime", "size", "created_at", "uploader_id"])).await?), [2, 3, 1]);
    let of_pages = Query::table("files").index_ranges("files_page_idx", [other, page].map(|id| KeyRange::eq([id]))).filter(col("purpose").eq("page"));
    assert_eq!(sorted(numbers(&db.query(of_pages.columns(["id", "page_id", "name", "mime", "size", "storage_key"])).await?)), [1, 2, 3, 4]);

    // Deleting a page keeps its files' rows, detached: the caller removes
    // them itself, to learn the keys of their objects.
    db.delete(Delete::from("pages").key([other])).await?;
    assert!(db.get("files", [uuid(4)]).await?.expect("the file").is_null("page_id")?);

    // Removing by storage key, which is neither unique nor indexed.
    assert_eq!(db.delete(Delete::from("files").filter(col("storage_key").eq("key-2"))).await?.affected(), 2);
    assert_eq!(db.delete(Delete::from("files").filter(col("storage_key").eq("key-2"))).await?.affected(), 0);
    assert_eq!(rows_in(&db, Query::table("files")).await?, 3);
    Ok(())
}

/// Shares a page, or changes the flags of its share: only the ones
/// given. The same values are bound twice in Postgres, once to insert and
/// once as nullable overrides.
fn share_page(key: &str, page: Uuid, include_children: Option<bool>, search_indexing: Option<bool>, by: Uuid) -> Insert {
    Insert::into("shares")
        .set("key", key)
        .set("page_id", page)
        .set("include_children", include_children.unwrap_or(false))
        .set("search_indexing", search_indexing.unwrap_or(false))
        .set("created_by", by)
        .on_conflict_update("shares_page_id_key")
        .update("include_children", coalesce([val(include_children), col("include_children")]))
        .update("search_indexing", coalesce([val(search_indexing), col("search_indexing")]))
        .update("updated_at", now())
        .returning(["id", "key", "include_children", "search_indexing", "created_by"])
}

// R-S3, R-W4, R-W10, R-W11: an upsert whose update keeps what was not given, and a second unique index it does not cover.
#[tokio::test(flavor = "multi_thread")]
async fn a_share_is_created_or_patched_by_one_upsert() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let page = new_page(&db, w.space, None, "a0", "Shared").await?;
    let other = new_page(&db, w.space, None, "a1", "Also shared").await?;
    let flags = |row: &Row| Ok::<_, DbError>((row.get::<String>("key")?, row.get::<bool>("include_children")?, row.get::<bool>("search_indexing")?));

    let created = db.insert(share_page("key-one", page, Some(true), None, w.ann)).await?.one()?;
    assert_eq!(flags(&created)?, ("key-one".to_string(), true, false));

    // The page is shared already: the new key is dropped, the flag that
    // was given changes, the one that was not stays, and so does who
    // shared it first.
    let patched = db.insert(share_page("key-two", page, None, Some(true), w.bob)).await?;
    assert_eq!(patched.affected(), 1);
    let patched = patched.one()?;
    assert_eq!(flags(&patched)?, ("key-one".to_string(), true, true));
    assert_eq!((patched.get::<Uuid>("id")?, patched.get::<Uuid>("created_by")?), (created.get::<Uuid>("id")?, w.ann));
    let cleared = db.insert(share_page("key-three", page, Some(false), Some(false), w.bob)).await?.one()?;
    assert_eq!(flags(&cleared)?, ("key-one".to_string(), false, false));
    assert_eq!(rows_in(&db, Query::table("shares")).await?, 1);

    // The upsert's target is the page; a key that another page's share
    // holds is still an error, and names its own index.
    let taken = db.insert(share_page("key-one", other, None, None, w.ann)).await.unwrap_err();
    assert_eq!(taken.code, ErrorCode::UniqueViolation);
    assert!(taken.violates("shares_key_key"));
    db.insert(share_page("key-other", other, None, None, w.ann)).await?;

    // Unsharing, once.
    assert_eq!(db.delete(Delete::from("shares").index("shares_page_id_key", KeyRange::eq([page]))).await?.affected(), 1);
    assert_eq!(db.delete(Delete::from("shares").index("shares_page_id_key", KeyRange::eq([page]))).await?.affected(), 0);
    assert_eq!(column::<String>(&db.query(Query::table("shares").columns(["key"])).await?, "key"), ["key-other"]);
    Ok(())
}

// R-Q9, R-Q10, R-S3, R-S7, R-W5, R-W10: idempotent inserts on partial unique indexes, and lists joined through them.
#[tokio::test(flavor = "multi_thread")]
async fn favorites_and_watches_are_added_once_and_listed_in_the_order_added() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let (ann, bob, space) = (w.ann, w.bob, w.space);
    let hidden = new_space(&db, "hidden").await?;
    let page = |n: u128, space: Uuid, title: &str| page_insert(space, None, "a0", title).set("id", uuid(n));
    db.insert(page(1, space, "First made")).await?;
    db.insert(page(2, space, "Second made")).await?;
    db.insert(page(3, space, "In the trash").set("deleted_at", now())).await?;
    db.insert(page(4, hidden, "Out of reach")).await?;

    // Adding is idempotent. The conflict target is the unique index
    // over the rows that name a page, or the one over those naming a space.
    let favor_page = |user: Uuid, n: u128, at: &str| {
        Insert::into("favorites")
            .set("user_id", user)
            .set("page_id", uuid(n))
            .set("created_at", ts(at))
            .on_conflict_do_nothing_on("favorites_page_key")
    };
    let favor_space = |user: Uuid, space: Uuid| {
        Insert::into("favorites").set("user_id", user).set("space_id", space).on_conflict_do_nothing_on("favorites_space_key")
    };
    assert_eq!(db.insert(favor_page(ann, 2, "2026-03-01T00:00:00Z")).await?.affected(), 1);
    assert_eq!(db.insert(favor_page(ann, 2, "2026-03-09T00:00:00Z")).await?.affected(), 0);
    for (n, at) in [(1, "2026-03-02T00:00:00Z"), (3, "2026-03-03T00:00:00Z"), (4, "2026-03-04T00:00:00Z")] {
        db.insert(favor_page(ann, n, at)).await?;
    }
    db.insert(favor_page(bob, 1, "2026-03-05T00:00:00Z")).await?;
    assert_eq!(db.insert(favor_space(ann, space)).await?.affected(), 1);
    assert_eq!(db.insert(favor_space(ann, space)).await?.affected(), 0);
    assert_eq!(db.insert(favor_space(ann, hidden)).await?.affected(), 1);
    assert_eq!(rows_in(&db, Query::table("favorites")).await?, 7);
    // A favourite is of a page or of a space.
    let both = db.insert(Insert::into("favorites").set("user_id", bob).set("page_id", uuid(2)).set("space_id", hidden)).await.unwrap_err();
    assert!(both.code == ErrorCode::CheckViolation && both.violates("favorites_check"));
    let neither = db.insert(Insert::into("favorites").set("user_id", bob)).await.unwrap_err();
    assert!(neither.violates("favorites_check"));

    // Favourite pages in the order they were added, as far as the
    // user can still read them.
    let pages = Query::table("favorites")
        .alias("f")
        .index("favorites_page_key", KeyRange::eq([ann]))
        .join(Query::table("pages").alias("p").key([col("f.page_id")]).filter(readable(ann)))
        .join(Query::table("spaces").alias("s").key([col("p.space_id")]))
        .order_by(col("f.created_at"));
    let pages = db.query(tree_columns(pages).select("space", space_ref())).await?;
    assert_eq!(titles(&pages), ["Second made", "First made"]);

    // Favourite spaces with the caller's role, without the ones the
    // caller has no role in.
    let spaces = Query::table("favorites")
        .alias("f")
        .index("favorites_space_key", KeyRange::eq([ann]))
        .join(Query::table("spaces").alias("s").key([col("f.space_id")]))
        .filter(role_rank(col("s.id"), val(ann)).is_not_null())
        .order_by(col("f.created_at"))
        .columns(["s.id", "s.name"])
        .select("rank", role_rank(col("s.id"), val(ann)));
    let spaces = db.query(spaces).await?;
    assert_eq!((column::<String>(&spaces, "name"), column::<i64>(&spaces, "rank")), (vec!["docs".to_string()], vec![3]));

    // Removing, by the pair that identifies it.
    assert_eq!(db.delete(Delete::from("favorites").index("favorites_page_key", KeyRange::eq([ann, uuid(2)]))).await?.affected(), 1);
    assert_eq!(db.delete(Delete::from("favorites").index("favorites_page_key", KeyRange::eq([ann, uuid(2)]))).await?.affected(), 0);
    assert_eq!(db.delete(Delete::from("favorites").index("favorites_space_key", KeyRange::eq([ann, hidden]))).await?.affected(), 1);
    assert_eq!(rows_in(&db, Query::table("favorites").index("favorites_page_key", KeyRange::eq([bob]))).await?, 1);

    // Watches have the same shape.
    let watch_space =
        |user: Uuid| Insert::into("watchers").set("user_id", user).set("space_id", space).on_conflict_do_nothing_on("watchers_space_key");
    let watch_page =
        |user: Uuid, n: u128| Insert::into("watchers").set("user_id", user).set("page_id", uuid(n)).on_conflict_do_nothing_on("watchers_page_key");
    assert_eq!((db.insert(watch_space(ann)).await?.affected(), db.insert(watch_space(ann)).await?.affected()), (1, 0));
    assert_eq!((db.insert(watch_page(ann, 1)).await?.affected(), db.insert(watch_page(ann, 1)).await?.affected()), (1, 0));
    let watches_page =
        |user: Uuid, n: u128| Query::table("watchers").index("watchers_page_key", KeyRange::eq([user, uuid(n)])).limit(1).columns(["id"]);
    let watches_space = |user: Uuid| Query::table("watchers").index("watchers_space_key", KeyRange::eq([user, space])).limit(1).columns(["id"]);
    assert!(db.query_opt(watches_page(ann, 1)).await?.is_some());
    assert!(db.query_opt(watches_page(ann, 2)).await?.is_none());
    assert!(db.query_opt(watches_space(ann)).await?.is_some());
    assert!(db.query_opt(watches_space(bob)).await?.is_none());
    assert_eq!(db.delete(Delete::from("watchers").index("watchers_page_key", KeyRange::eq([ann, uuid(1)]))).await?.affected(), 1);
    assert_eq!(db.delete(Delete::from("watchers").index("watchers_space_key", KeyRange::eq([ann, space]))).await?.affected(), 1);
    assert_eq!(rows_in(&db, Query::table("watchers")).await?, 0);
    Ok(())
}

/// Puts labels on a page, making the ones that do not exist yet.
/// Returns how many labels were made and how many were newly attached.
async fn add_labels(db: &Client, page: Uuid, names: &[&str]) -> DbResult<(u64, u64)> {
    db.transaction(|tx| async move {
        // Get-or-create by the unique name.
        let mut labels = Insert::into("labels").columns(["name"]);
        for name in names {
            labels = labels.values([*name]);
        }
        let made = tx.insert(labels.on_conflict_do_nothing_on("labels_name_key")).await?.affected();
        let found = Query::table("labels")
            .index_ranges("labels_name_key", names.iter().map(|name| KeyRange::eq([*name])))
            .select("page_id", val(page))
            .select("label_id", col("id"));
        let attached =
            tx.insert(Insert::into("page_labels").columns(["page_id", "label_id"]).from_query(found).on_conflict_do_nothing()).await?.affected();
        Ok::<_, DbError>((made, attached))
    })
    .await
}

// R-Q7, R-Q9, R-Q10, R-Q11, R-W5, R-W7, R-X1: get-or-create in one transaction, a grouped count over joins, and a delete conditional on `NOT EXISTS`.
#[tokio::test(flavor = "multi_thread")]
async fn labels_are_made_once_counted_by_readable_pages_and_collected_when_unused() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let ann = w.ann;
    let hidden = new_space(&db, "hidden").await?;
    let page = |n: u128, space: Uuid| {
        page_insert(space, None, "a0", &format!("page {n}")).set("id", uuid(n)).set("updated_at", ts(&format!("2026-03-{n:02}T00:00:00Z")))
    };
    for n in 1..=4 {
        db.insert(page(n, w.space)).await?;
    }
    db.insert(page(5, hidden)).await?;

    assert_eq!(add_labels(&db, uuid(1), &["rust", "docs"]).await?, (2, 2));
    assert_eq!(add_labels(&db, uuid(2), &["rust", "alpha", "docs"]).await?, (1, 3));
    assert_eq!(add_labels(&db, uuid(3), &["rust", "alpha"]).await?, (0, 2));
    assert_eq!(add_labels(&db, uuid(3), &["alpha"]).await?, (0, 0));
    assert_eq!(add_labels(&db, uuid(4), &["trash", "rust"]).await?, (1, 2));
    assert_eq!(add_labels(&db, uuid(5), &["hidden", "rust"]).await?, (1, 2));
    db.update(Update::table("pages").key([uuid(4)]).set("deleted_at", now())).await?;
    // The name is unique as written: lower-casing it is the caller's job.
    assert_eq!(add_labels(&db, uuid(1), &["Rust"]).await?, (1, 1));
    assert_eq!(rows_in(&db, Query::table("labels")).await?, 6);

    // The labels of a page, by name.
    let of_page = |n: u128| {
        let labels =
            Query::table("page_labels").alias("pl").range(KeyRange::eq([uuid(n)])).join(Query::table("labels").alias("l").key([col("pl.label_id")]));
        labels.order_by(col("l.name")).columns(["l.id", "l.name"])
    };
    assert_eq!(column::<String>(&db.query(of_page(2)).await?, "name"), ["alpha", "docs", "rust"]);
    assert_eq!(column::<String>(&db.query(of_page(1)).await?, "name"), ["Rust", "docs", "rust"]);

    // A GROUP BY: labels with the number of live pages the user can read,
    // the most used first. Labels used only in the trash or out of reach
    // are not listed.
    let counted = |needle: &str| {
        Query::table("labels")
            .alias("l")
            .join(Query::table("page_labels").alias("pl").index("page_labels_label_idx", KeyRange::eq([col("l.id")])))
            .join(Query::table("pages").alias("p").key([col("pl.page_id")]).filter(readable(ann)))
            .filter(val(needle.is_empty()).or(col("l.name").like(format!("%{}%", like_escape(needle)))))
            .group_by([col("l.id"), col("l.name")])
            .columns(["l.id", "l.name"])
            .select("page_count", count())
            .order_by(count().desc())
            .order_by(col("l.name"))
            .limit(200)
    };
    let all = db.query(counted("")).await?;
    assert_eq!(column::<String>(&all, "name"), ["rust", "alpha", "docs", "Rust"]);
    assert_eq!(column::<i64>(&all, "page_count"), [3, 2, 2, 1]);
    // `LIKE`, unlike the search, minds the case.
    assert_eq!(column::<String>(&db.query(counted("ru")).await?, "name"), ["rust"]);
    assert_eq!(column::<String>(&db.query(counted("R")).await?, "name"), ["Rust"]);
    assert!(db.query(counted("hid")).await?.is_empty());

    // The pages of a label, latest edit first.
    let rust: Uuid = db.query_one(Query::table("labels").index("labels_name_key", KeyRange::eq(["rust"]))).await?.get("id")?;
    let labelled = Query::table("page_labels")
        .alias("pl")
        .index("page_labels_label_idx", KeyRange::eq([rust]))
        .join(Query::table("pages").alias("p").key([col("pl.page_id")]).filter(readable(ann)))
        .join(Query::table("spaces").alias("s").key([col("p.space_id")]))
        .order_by(col("p.updated_at").desc())
        .order_by(col("p.id"))
        .limit(500);
    assert_eq!(numbers(&db.query(tree_columns(labelled).select("space", space_ref())).await?), [3, 2, 1]);

    // Taking a label off a page, then collecting the label if no
    // page has it any more. The second delete is conditional on that.
    let alpha: Uuid = db.query_one(Query::table("labels").index("labels_name_key", KeyRange::eq(["alpha"]))).await?.get("id")?;
    let collect_unused = |label: Uuid| {
        let still_used = Query::table("page_labels").alias("pl").index("page_labels_label_idx", KeyRange::eq([col("l.id")]));
        Delete::rows(Query::table("labels").alias("l").key([label]).filter(exists(still_used).not()))
    };
    assert_eq!(db.delete(Delete::from("page_labels").key([uuid(3), alpha])).await?.affected(), 1);
    assert_eq!(db.delete(collect_unused(alpha)).await?.affected(), 0);
    assert_eq!(db.delete(Delete::from("page_labels").key([uuid(2), alpha])).await?.affected(), 1);
    assert_eq!(db.delete(collect_unused(alpha)).await?.affected(), 1);
    assert!(db.get("labels", [alpha]).await?.is_none());

    // Deleting a label that is in use takes it off its pages.
    assert_eq!(db.delete(Delete::from("labels").key([rust])).await?.affected(), 1);
    assert_eq!(column::<String>(&db.query(of_page(2)).await?, "name"), ["docs"]);
    assert_eq!(rows_in(&db, Query::table("page_labels").index("page_labels_label_idx", KeyRange::eq([rust]))).await?, 0);
    Ok(())
}

// ------------------------------------------------------------ notifications

/// A user's inbox, newest first. A page the user can no longer read
/// shows as NULL instead of hiding the notification: the permission check
/// is the join's own condition.
fn inbox(user: Uuid, unread_only: bool) -> Query {
    let page = object([
        ("id", col("p.id")),
        ("slugId", col("p.slug_id")),
        ("title", json_or_null(col("p.title"))),
        ("icon", json_or_null(col("p.icon"))),
        ("spaceId", col("p.space_id")),
        ("spaceSlug", col("s.slug")),
    ]);
    let comment = object([("id", col("c.id")), ("parentId", json_or_null(col("c.parent_id")))]);
    Query::table("notifications")
        .alias("n")
        .index("notifications_user_idx", KeyRange::eq([user]))
        .left_join(Query::table("users").alias("a").key([col("n.actor_id")]))
        .left_join(Query::table("pages").alias("p").key([col("n.page_id")]).filter(readable(user)))
        .left_join(Query::table("spaces").alias("s").key([col("p.space_id")]))
        .left_join(Query::table("comments").alias("c").key([col("n.comment_id")]))
        .filter(val(!unread_only).or(col("n.read_at").is_null()))
        .columns(["n.id", "n.kind", "n.data", "n.created_at", "n.read_at"])
        .select("actor", user_ref("a"))
        .select("page", when(col("p.id").is_null(), null(), page))
        .select("comment", when(col("c.id").is_null().or(col("p.id").is_null()), null(), comment))
}

// R-Q4, R-Q5, R-Q9, R-Q10, R-Q11, R-Q13, R-S4: a list in index order with four left joins, paged by offset and by keyset, and a count.
#[tokio::test(flavor = "multi_thread")]
async fn the_inbox_pages_by_time_and_blanks_pages_the_user_cannot_read() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let (ann, bob, space) = (w.ann, w.bob, w.space);
    let cat = new_user(&db, "cat").await?;
    let hidden = new_space(&db, "hidden").await?;
    let readable_page: Uuid = db.insert(page_with_slug("readable01", space, None, "a0", "Readable").returning(["id"])).await?.one()?.get("id")?;
    let hidden_page = new_page(&db, hidden, None, "a0", "Hidden").await?;
    let trashed_page: Uuid =
        db.insert(page_insert(space, None, "a1", "Trashed").set("deleted_at", now()).returning(["id"])).await?.one()?.get("id")?;
    let comment: Uuid =
        db.insert(Insert::into("comments").set("page_id", readable_page).set("body", json!({})).returning(["id"])).await?.one()?.get("id")?;
    let reply: Uuid = db
        .insert(Insert::into("comments").set("page_id", readable_page).set("parent_id", comment).set("body", json!({})).returning(["id"]))
        .await?
        .one()?
        .get("id")?;
    let hidden_comment: Uuid =
        db.insert(Insert::into("comments").set("page_id", hidden_page).set("body", json!({})).returning(["id"])).await?.one()?.get("id")?;

    let notification = |n: u128, user: Uuid, kind: &str, actor: Option<Uuid>, page: Option<Uuid>, comment: Option<Uuid>, at: &str| {
        Insert::into("notifications")
            .set("id", uuid(n))
            .set("user_id", user)
            .set("kind", kind)
            .set("actor_id", actor)
            .set("page_id", page)
            .set("comment_id", comment)
            .set("created_at", ts(at))
    };
    db.insert(notification(1, ann, "page.updated", Some(bob), Some(readable_page), None, "2026-03-05T10:00:00Z")).await?;
    db.insert(notification(2, ann, "comment.reply", Some(bob), Some(readable_page), Some(reply), "2026-03-04T10:00:00Z")).await?;
    // Two at the same instant: the id decides, ascending.
    db.insert(notification(4, ann, "page.mention", Some(bob), Some(trashed_page), None, "2026-03-03T10:00:00Z")).await?;
    db.insert(notification(3, ann, "comment.created", Some(cat), Some(hidden_page), Some(hidden_comment), "2026-03-03T10:00:00Z")).await?;
    db.insert(notification(5, ann, "page.updated", None, None, None, "2026-03-01T10:00:00Z").set("read_at", ts("2026-03-02T00:00:00Z"))).await?;
    db.insert(notification(6, bob, "page.updated", Some(ann), Some(readable_page), None, "2026-03-06T10:00:00Z")).await?;
    db.delete(Delete::from("users").key([cat])).await?;

    let rows = db.query(inbox(ann, false).limit(50)).await?;
    assert_eq!(numbers(&rows), [1, 2, 3, 4, 5]);
    assert_eq!(column::<String>(&rows, "kind"), ["page.updated", "comment.reply", "comment.created", "page.mention", "page.updated"]);
    let actors: Vec<Option<Json>> = column(&rows, "actor");
    assert_eq!(actors[0], Some(json!({"id": bob.to_string(), "name": "bob", "avatarUrl": null})));
    assert_eq!((&actors[2], &actors[4]), (&None, &None));
    let pages: Vec<Option<Json>> = column(&rows, "page");
    let readable = json!({
        "id": readable_page.to_string(), "slugId": "readable01", "title": "Readable", "icon": null,
        "spaceId": space.to_string(), "spaceSlug": "docs"
    });
    assert_eq!(pages[0], Some(readable));
    assert_eq!(pages[1..].iter().filter(|page| page.is_some()).count(), 1);
    assert_eq!((&pages[2], &pages[3], &pages[4]), (&None, &None, &None));
    let comments: Vec<Option<Json>> = column(&rows, "comment");
    assert_eq!(comments[1], Some(json!({"id": reply.to_string(), "parentId": comment.to_string()})));
    // The comment of a page that is out of reach is out of reach too.
    assert_eq!((&comments[0], &comments[2]), (&None, &None));
    assert_eq!(rows.first().expect("a row").get::<Json>("data")?, json!({}));

    // The unread only; and a page of them by offset.
    assert_eq!(numbers(&db.query(inbox(ann, true).limit(50)).await?), [1, 2, 3, 4]);
    assert_eq!(numbers(&db.query(inbox(ann, false).limit(2).offset(1)).await?), [2, 3]);
    assert_eq!(numbers(&db.query(inbox(bob, false).limit(50)).await?), [6]);

    // The same list by keyset: after the last row seen, in the index's
    // own order, descending time then ascending id.
    let after = |at: &str, id: u128| {
        let rest = KeyRange::within([ann], Bound::excluded([val(ts(at)), val(uuid(id))]), Bound::Unbounded);
        Query::table("notifications").index("notifications_user_idx", rest).limit(2).columns(["id"])
    };
    assert_eq!(numbers(&db.query(after("2026-03-04T10:00:00Z", 2)).await?), [3, 4]);
    assert_eq!(numbers(&db.query(after("2026-03-03T10:00:00Z", 3)).await?), [4, 5]);
    assert!(db.query(after("2026-03-01T10:00:00Z", 5)).await?.is_empty());

    // The badge, polled by every open client.
    let unread = |user: Uuid| {
        Query::table("notifications").index("notifications_user_idx", KeyRange::eq([user])).filter(col("read_at").is_null()).select("count", count())
    };
    assert_eq!(db.query_one(unread(ann)).await?.get::<i64>("count")?, 4);
    assert_eq!(db.query_one(unread(uuid(404))).await?.get::<i64>("count")?, 0);
    Ok(())
}

// R-Q6, R-W2, R-W6, R-W7: one update over a list of 2000 ids, counting the rows it changed and stamping one instant.
#[tokio::test(flavor = "multi_thread")]
async fn two_thousand_notifications_are_marked_read_by_one_statement() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let (ann, bob) = (w.ann, w.bob);
    let mut batch = Insert::into("notifications").columns(["id", "user_id", "kind"]);
    for n in 0..2000 {
        batch = batch.values([val(uuid(1000 + n)), val(ann), val("page.updated")]);
    }
    assert_eq!(db.insert(batch).await?.affected(), 2000);
    db.insert(Insert::into("notifications").set("id", uuid(1)).set("user_id", ann).set("kind", "page.mention")).await?;
    db.insert(
        Insert::into("notifications").set("id", uuid(2)).set("user_id", ann).set("kind", "page.mention").set("read_at", ts("2026-03-01T00:00:00Z")),
    )
    .await?;
    db.insert(Insert::into("notifications").set("id", uuid(3)).set("user_id", bob).set("kind", "page.mention")).await?;
    let unread = |user: Uuid| Query::table("notifications").index("notifications_user_idx", KeyRange::eq([user])).filter(col("read_at").is_null());

    // Mark these read. The list holds 2000 of ann's, one she has
    // read already, one that is bob's and one that does not exist; the
    // count is of the rows that changed.
    let mut listed: Vec<Uuid> = (0..2000).map(|n| uuid(1000 + n)).collect();
    listed.extend([uuid(2), uuid(3), uuid(404)]);
    let mark = Update::table("notifications")
        .index("notifications_user_idx", KeyRange::eq([ann]))
        .filter(col("id").in_list(uuids(&listed)).and(col("read_at").is_null()))
        .set("read_at", now());
    assert_eq!(db.update(mark.clone()).await?.affected(), 2000);
    assert_eq!(db.update(mark).await?.affected(), 0);
    assert_eq!(rows_in(&db, unread(ann)).await?, 1);
    assert_eq!(rows_in(&db, unread(bob)).await?, 1);
    assert_eq!(db.get("notifications", [uuid(2)]).await?.expect("the row").get::<Timestamp>("read_at")?, ts("2026-03-01T00:00:00Z"));
    // One statement, one instant.
    let stamps = db
        .query(
            Query::table("notifications")
                .range(KeyRange::new(Bound::included([uuid(1000)]), Bound::Unbounded))
                .select("at", col("read_at"))
                .distinct(),
        )
        .await?;
    assert_eq!(stamps.len(), 1);

    // The same without a list: everything of this user's that is unread.
    assert_eq!(
        db.update(
            Update::table("notifications")
                .index("notifications_user_idx", KeyRange::eq([ann]))
                .filter(col("read_at").is_null())
                .set("read_at", now())
        )
        .await?
        .affected(),
        1
    );
    assert_eq!(rows_in(&db, unread(ann)).await?, 0);

    // The ones an email went out for, by id.
    let mailed = Update::rows(Query::table("notifications").keys(listed.iter().map(|id| [*id]))).set("emailed_at", now());
    assert_eq!(db.update(mailed).await?.affected(), 2002);
    Ok(())
}

// R-Q6, R-Q9, R-Q10: joins from a link table in both directions, and a read of many composite keys, each scoped to what the user can read.
#[tokio::test(flavor = "multi_thread")]
async fn backlinks_and_synced_blocks_are_read_only_where_the_user_may() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let ann = w.ann;
    let hidden = new_space(&db, "hidden").await?;
    let page = |n: u128, space: Uuid, title: Option<&str>| {
        Insert::into("pages").set("id", uuid(n)).set("slug_id", slug_id()).set("space_id", space).set("position", "a0").set("title", title)
    };
    db.insert(page(1, w.space, Some("Target"))).await?;
    db.insert(page(2, w.space, Some("beta"))).await?;
    db.insert(page(3, w.space, Some("Alpha"))).await?;
    db.insert(page(4, w.space, Some("In the trash")).set("deleted_at", now())).await?;
    db.insert(page(5, hidden, Some("Hidden"))).await?;
    db.insert(page(6, w.space, None)).await?;
    for (source, target) in [(2, 1), (3, 1), (4, 1), (5, 1), (6, 1), (2, 5), (2, 3)] {
        db.insert(Insert::into("page_links").set("source_id", uuid(source)).set("target_id", uuid(target))).await?;
    }

    // The pages that link here, and the pages this one links to:
    // the same statement with the two columns swapped. By title whatever
    // its case, an untitled page first.
    let linked = |links: Query, other_end: &str| {
        let pages = links
            .alias("l")
            .join(Query::table("pages").alias("p").key([col(other_end)]).filter(readable(ann)))
            .join(Query::table("spaces").alias("s").key([col("p.space_id")]))
            .order_by(col("p.title").or_else("").lower())
            .order_by(col("p.id"))
            .limit(500);
        tree_columns(pages).select("space", space_ref())
    };
    let backlinks = db.query(linked(Query::table("page_links").index("page_links_target_idx", KeyRange::eq([uuid(1)])), "l.source_id")).await?;
    assert_eq!(numbers(&backlinks), [6, 3, 2]);
    assert_eq!(column::<Option<String>>(&backlinks, "title"), [None, Some("Alpha".to_string()), Some("beta".to_string())]);
    let outgoing = db.query(linked(Query::table("page_links").range(KeyRange::eq([uuid(2)])), "l.target_id")).await?;
    assert_eq!(numbers(&outgoing), [3, 1]);

    // Up to a hundred (page, block) references of a document, to show
    // each block's current content. Postgres joins a JSON parameter to
    // the tables and returns a row per reference, NULL where the user may
    // not read; here the keys are read in the order asked and what is
    // missing or out of reach is simply not among the rows.
    for (of, block) in [(2, "b1"), (2, "b2"), (5, "b9"), (4, "b4")] {
        db.insert(Insert::into("synced_blocks").set("page_id", uuid(of)).set("block_id", block).set("content", json!({"block": block}))).await?;
    }
    let asked = [(2, "b2"), (5, "b9"), (3, "nope"), (4, "b4"), (2, "b1")];
    let blocks = Query::table("synced_blocks")
        .alias("sb")
        .keys(asked.map(|(of, block)| [val(uuid(of)), val(block)]))
        .join(Query::table("pages").alias("p").key([col("sb.page_id")]).filter(readable(ann)))
        .columns(["sb.page_id", "sb.block_id", "sb.content"]);
    let blocks = db.query(blocks).await?;
    assert_eq!(column::<String>(&blocks, "block_id"), ["b2", "b1"]);
    let answer: Vec<Option<Json>> = asked
        .iter()
        .map(|(of, block)| {
            blocks
                .iter()
                .find(|row| row.get::<Uuid>("page_id").ok() == Some(uuid(*of)) && row.get::<String>("block_id").ok().as_deref() == Some(*block))
                .map(|row| row.get::<Json>("content").expect("a block"))
        })
        .collect();
    assert_eq!(answer, [Some(json!({"block": "b2"})), None, None, None, Some(json!({"block": "b1"}))]);

    // Which pages show a copy of a block: by the block's id.
    let shown = db.query(Query::table("synced_blocks").index("synced_blocks_block_idx", KeyRange::eq(["b1"])).columns(["page_id"])).await?;
    assert_eq!(column::<Uuid>(&shown, "page_id"), [uuid(2)]);
    Ok(())
}

// ------------------------------------------- bases: columns, views, rows

/// Deletes a view unless it is its base's last. Postgres has to lock all
/// the base's view rows to count them safely; the transaction is alone.
async fn delete_view(db: &Client, base: Uuid, view: Uuid) -> DbResult<bool> {
    db.transaction(|tx| async move {
        // The base's views, to count them.
        let views = ids(&tx.query(Query::table("base_views").index("base_views_page_idx", KeyRange::eq([base])).columns(["id"])).await?);
        if views.len() <= 1 || !views.contains(&view) {
            return Ok(false);
        }
        tx.delete(Delete::from("base_views").key([view])).await?;
        Ok::<_, DbError>(true)
    })
    .await
}

// R-Q4, R-Q11, R-S3, R-S7, R-W1, R-W6, R-W11, R-X3: a composite key, a unique index on a folded name, ordered lists, a check on a type, and a guarded delete raced.
#[tokio::test(flavor = "multi_thread")]
async fn a_base_keeps_its_columns_in_order_unique_by_name_and_at_least_one_view() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let base = new_page(&db, w.space, None, "a0", "Tasks").await?;
    let other_base = new_page(&db, w.space, None, "a1", "Contacts").await?;

    // A page becomes a base once, when it has no columns yet, and is
    // seeded with its first columns, row and view.
    let has_columns = |page: Uuid| Query::table("base_properties").range(KeyRange::eq([page])).limit(1).columns(["id"]);
    assert!(db.query_opt(has_columns(base)).await?.is_none());
    db.update(Update::table("pages").key([base]).set("is_base", true).set("updated_at", now()).set("last_editor_id", w.ann)).await?;

    // The columns, each under an id the caller chose.
    let property = |page: Uuid, id: &str, name: &str, kind: &str, position: &str| {
        Insert::into("base_properties")
            .set("page_id", page)
            .set("id", id)
            .set("name", name)
            .set("type", kind)
            .set("options", json!({}))
            .set("position", position)
    };
    db.insert(property(base, "ptitle", "Name", "text", "a1").set("is_primary", true)).await?;
    db.insert(property(base, "pstatus", "Status", "select", "a2")).await?;
    db.insert(property(base, "pamount", "Amount", "number", "Zz")).await?;
    db.insert(property(other_base, "ptitle", "Name", "text", "a0")).await?;
    assert!(db.query_opt(has_columns(base)).await?.is_some());

    // At the start of every base endpoint: the columns in order.
    let columns = |page: Uuid| {
        Query::table("base_properties")
            .index("base_properties_order_idx", KeyRange::eq([page]))
            .columns(["id", "name", "type", "options", "is_primary", "position"])
    };
    let listed = db.query(columns(base)).await?;
    assert_eq!(column::<String>(&listed, "id"), ["pamount", "ptitle", "pstatus"]);
    assert_eq!(column::<bool>(&listed, "is_primary"), [false, true, false]);

    // A name is taken within its base whatever its case and the spaces
    // around it. A caller may check first with its own folding; the index
    // decides.
    for name in ["name", "  NAME ", "Status "] {
        let taken = db.insert(property(base, "pnew", name, "text", "a9")).await.unwrap_err();
        assert_eq!(taken.code, ErrorCode::UniqueViolation, "{name:?}");
        assert!(taken.violates("base_properties_name_key"), "{name:?}");
    }
    let same_id = db.insert(property(base, "ptitle", "Another", "text", "a9")).await.unwrap_err();
    assert!(same_id.violates(PRIMARY));
    // Spaces inside a name are part of it.
    db.insert(property(base, "pnew", "Na me", "text", "a9")).await?;

    // A change by the two-part key. A rename onto another column's name
    // fails and leaves the row as it was.
    let change = |id: &str, name: &str, kind: &str, options: Json, position: &str| {
        Update::table("base_properties")
            .key([val(base), val(id)])
            .set("name", name)
            .set("type", kind)
            .set("options", options)
            .set("position", position)
            .set("updated_at", now())
    };
    let clash = db.update(change("pnew", " amount", "number", json!({}), "a9")).await.unwrap_err();
    assert!(clash.violates("base_properties_name_key"));
    assert_eq!(db.get("base_properties", [val(base), val("pnew")]).await?.expect("the column").get::<String>("name")?, "Na me");
    let options = json!({"choices": [{"id": "c1", "name": "Todo"}, {"id": "c2", "name": "Done"}]});
    assert_eq!(db.update(change("pstatus", "State", "status", options.clone(), "a0")).await?.affected(), 1);
    assert_eq!(
        db.update(
            Update::table("base_properties").key([val(base), val("pamount")]).set("options", json!({"format": "euro"})).set("updated_at", now())
        )
        .await?
        .affected(),
        1
    );
    let listed = db.query(columns(base)).await?;
    assert_eq!(column::<String>(&listed, "name"), ["Amount", "State", "Name", "Na me"]);
    assert_eq!(column::<Json>(&listed, "options"), [json!({"format": "euro"}), options, json!({}), json!({})]);

    // Deleting a column, by the same key.
    assert_eq!(db.delete(Delete::from("base_properties").key([val(base), val("pnew")])).await?.affected(), 1);
    assert_eq!(rows_in(&db, Query::table("base_properties").range(KeyRange::eq([other_base]))).await?, 1);

    // Views. The next one goes after the last.
    db.insert(
        Insert::into("base_views").set("page_id", base).set("name", "Table").set("type", "table").set("position", "a0").set("creator_id", w.ann),
    )
    .await?;
    db.insert(
        Insert::into("base_views")
            .set("page_id", base)
            .set("name", "Board")
            .set("type", "kanban")
            .set("config", json!({"groupBy": "pstatus"}))
            .set("position", "a1")
            .set("creator_id", w.ann),
    )
    .await?;
    let last: Option<String> = db
        .query_one(Query::table("base_views").index("base_views_page_idx", KeyRange::eq([base])).select("position", max(col("position"))))
        .await?
        .get("position")?;
    assert_eq!(last.as_deref(), Some("a1"));
    let calendar = Insert::into("base_views")
        .set("page_id", base)
        .set("name", "Calendar")
        .set("type", "calendar")
        .set("config", json!({"date": "pdue"}))
        .set("position", key_after(last))
        .set("creator_id", w.bob);
    let calendar = db.insert(calendar.returning(["id", "name", "type", "config", "position"])).await?.one()?;
    assert_eq!((calendar.get::<String>("position")?, calendar.get::<Json>("config")?), ("a1V".to_string(), json!({"date": "pdue"})));
    let gallery = db
        .insert(Insert::into("base_views").set("page_id", base).set("name", "Gallery").set("type", "gallery").set("position", "a2"))
        .await
        .unwrap_err();
    assert_eq!(gallery.code, ErrorCode::CheckViolation);
    assert!(gallery.violates("base_views_type_check"));

    // The views in order; and one view of this base.
    let views = db
        .query(Query::table("base_views").index("base_views_page_idx", KeyRange::eq([base])).columns(["id", "name", "type", "config", "position"]))
        .await?;
    assert_eq!(column::<String>(&views, "name"), ["Table", "Board", "Calendar"]);
    let (table, board, calendar) = (ids(&views)[0], ids(&views)[1], ids(&views)[2]);
    let view_of = |page: Uuid, view: Uuid| {
        Query::table("base_views").key([view]).filter(col("page_id").eq(page)).columns(["id", "name", "type", "config", "position"])
    };
    assert!(db.query_opt(view_of(base, board)).await?.is_some());
    assert!(db.query_opt(view_of(other_base, board)).await?.is_none());

    // Changing a view: its name, its configuration, its place.
    let renamed = Update::table("base_views")
        .key([board])
        .set("name", "Kanban")
        .set("type", "kanban")
        .set("config", json!({"groupBy": "pstatus", "hidden": ["pamount"]}))
        .set("position", "Zz")
        .set("updated_at", now());
    let renamed = db.update(renamed.returning(["id", "name", "type", "config", "position"])).await?.one()?;
    assert_eq!((renamed.get::<String>("name")?, renamed.get::<String>("position")?), ("Kanban".to_string(), "Zz".to_string()));
    db.update(Update::table("base_views").key([table]).set("config", json!({"widths": {"ptitle": 240}})).set("updated_at", now())).await?;
    let views = db.query(Query::table("base_views").index("base_views_page_idx", KeyRange::eq([base])).columns(["name"])).await?;
    assert_eq!(column::<String>(&views, "name"), ["Kanban", "Table", "Calendar"]);

    // A base keeps at least one view. The last two are deleted at
    // the same moment: one delete goes through.
    assert!(!delete_view(&db, other_base, board).await?);
    assert!(delete_view(&db, base, calendar).await?);
    let delete = |view: Uuid| {
        let db = db.clone();
        async move { delete_view(&db, base, view).await }
    };
    let (a, b) = race(delete(table), delete(board)).await;
    assert_eq!(sorted(vec![a?, b?]), [false, true]);
    assert_eq!(rows_in(&db, Query::table("base_views").index("base_views_page_idx", KeyRange::eq([base]))).await?, 1);
    Ok(())
}

const ROW_COLUMNS: [&str; 7] = ["id", "cells", "position", "creator_id", "last_editor_id", "created_at", "updated_at"];

/// `apply_cell_patch(cells, patch)`, a SQL function in Postgres: a JSON
/// null in the patch removes the cell, anything else sets it. The caller
/// knows the patch, so the expression is a merge of what is set followed
/// by the removal of each cleared key. (Stripping nulls from the merge
/// would not do: it would also take nulls out of the cells' own values.)
fn apply_cell_patch(cells: Expr, patch: &Json) -> Expr {
    let mut set = serde_json::Map::new();
    let mut cleared = Vec::new();
    for (key, value) in patch.as_object().into_iter().flatten() {
        if value.is_null() {
            cleared.push(key.clone());
        } else {
            set.insert(key.clone(), value.clone());
        }
    }
    cleared.into_iter().fold(cells.json_merge(Json::Object(set)), |cells, key| call(Func::JsonRemove, [cells, val(key)]))
}

/// Adds a row to a base after a given row, or at the end. The
/// neighbours' keys are the first entries of the order index on either
/// side: no aggregate, two reads of one entry each.
async fn add_row(db: &Client, base: Uuid, after: Option<Uuid>, cells: Json, by: Uuid) -> DbResult<Option<Row>> {
    db.exclusive(|tx| async move {
        let in_order = |range: KeyRange| {
            Query::table("base_rows")
                .index("base_rows_order_idx", range)
                .limit(1)
                .columns(["position"])
        };
        let position = match after {
            Some(after) => {
                // That row's key, and the smallest key above it.
                let Some(row) = tx.query_opt(Query::table("base_rows").key([after]).filter(col("page_id").eq(base)).columns(["position"])).await?
                else {
                    return Ok(None);
                };
                let low: String = row.get("position")?;
                let above = KeyRange::within([base], Bound::excluded([low.as_str()]), Bound::Unbounded);
                match tx.query_opt(in_order(above)).await? {
                    // A real key is computed between the two; appending
                    // a small digit is enough of that for these keys.
                    Some(next) => {
                        let key = format!("{low}E");
                        assert!(key < next.get::<String>("position")?);
                        key
                    }
                    None => key_after(Some(low)),
                }
            }
            // The largest key of the base.
            None => key_after(tx.query_opt(in_order(KeyRange::eq([base])).reverse()).await?.map(|last| last.get("position")).transpose()?),
        };
        // The row itself.
        let row = Insert::into("base_rows")
            .set("page_id", base)
            .set("cells", cells)
            .set("position", position)
            .set("creator_id", by)
            .set("last_editor_id", by);
        Ok::<_, DbError>(Some(tx.insert(row.returning(ROW_COLUMNS)).await?.one()?))
    })
    .await
}

/// Changes some cells of a row. Postgres locks the row for the
/// read-modify-write (`FOR UPDATE`); here the transaction is alone, so
/// two editors patching different cells of one row both keep theirs.
async fn patch_row(db: &Client, base: Uuid, row: Uuid, patch: Json, position: Option<&str>, by: Uuid) -> DbResult<Option<Row>> {
    db.exclusive(|tx| async move {
        // The row as it is. A caller diffs the patch against these cells
        // and recomputes formulas before it writes.
        if tx
            .query_opt(
                Query::table("base_rows")
                    .alias("r")
                    .key([row])
                    .filter(col("r.page_id").eq(base))
                    .columns(ROW_COLUMNS.map(|name| format!("r.{name}"))),
            )
            .await?
            .is_none()
        {
            return Ok(None);
        }
        // The patch, applied to the cells as they are stored.
        let patched = Update::table("base_rows")
            .key([row])
            .set("cells", apply_cell_patch(col("cells"), &patch))
            .set("updated_at", now())
            .set("last_editor_id", by)
            .set("position", coalesce([val(position), col("position")]))
            .returning(ROW_COLUMNS);
        Ok::<_, DbError>(tx.update(patched).await?.opt())
    })
    .await
}

// R-Q4, R-Q13, R-W1, R-W6, R-X2, R-X3: neighbours read off an ordered index, and a JSON patch applied inside the update so that two at once both hold.
#[tokio::test(flavor = "multi_thread")]
async fn a_row_patch_sets_and_removes_cells_and_two_at_once_both_stay() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let base = new_page(&db, w.space, None, "a0", "Tasks").await?;
    let other_base = new_page(&db, w.space, None, "a1", "Contacts").await?;
    // A seeded row, with nothing in it.
    db.insert(Insert::into("base_rows").set("page_id", other_base).set("position", "zz").set("creator_id", w.ann).set("last_editor_id", w.ann))
        .await?;

    // Rows added at the end, between two others, and after the last.
    let first = add_row(&db, base, None, json!({"name": "first"}), w.ann).await?.expect("a row");
    let second = add_row(&db, base, None, json!({"name": "second"}), w.ann).await?.expect("a row");
    let (first_id, second_id): (Uuid, Uuid) = (first.get("id")?, second.get("id")?);
    assert_eq!((first.get::<String>("position")?, second.get::<String>("position")?), ("a0".to_string(), "a0V".to_string()));
    assert_eq!(
        (first.get::<Json>("cells")?, first.get::<Uuid>("creator_id")?, first.get::<Uuid>("last_editor_id")?),
        (json!({"name": "first"}), w.ann, w.ann)
    );
    assert_eq!(first.get::<Timestamp>("created_at")?, first.get::<Timestamp>("updated_at")?);
    let between = add_row(&db, base, Some(first_id), json!({"name": "between"}), w.bob).await?.expect("a row");
    assert_eq!(between.get::<String>("position")?, "a0E");
    let last = add_row(&db, base, Some(second_id), json!({}), w.bob).await?.expect("a row");
    assert_eq!(last.get::<String>("position")?, "a0VV");
    assert!(add_row(&db, other_base, Some(first_id), json!({}), w.bob).await?.is_none());
    let order = db.query(Query::table("base_rows").index("base_rows_order_idx", KeyRange::eq([base])).columns(["id", "position"])).await?;
    assert_eq!(column::<String>(&order, "position"), ["a0", "a0E", "a0V", "a0VV"]);
    // The same neighbours the way SQL asks for them.
    let edges = Query::table("base_rows")
        .index("base_rows_order_idx", KeyRange::eq([base]))
        .select("last", max(col("position")))
        .select("next", min(col("position")).filter(col("position").gt("a0")));
    let edges = db.query_one(edges).await?;
    assert_eq!((edges.get::<String>("last")?, edges.get::<String>("next")?), ("a0VV".to_string(), "a0E".to_string()));

    // A null clears a cell, anything else sets it; cells the patch
    // does not name stay, with their own nulls.
    let file = json!([{"id": "f1", "name": "spec.pdf", "thumbnail": null}]);
    db.update(Update::table("base_rows").key([first_id]).set("cells", json!({"name": "first", "count": 2, "files": file}))).await?;
    tokio::time::sleep(Duration::from_millis(5)).await;
    let patched =
        patch_row(&db, base, first_id, json!({"name": null, "count": 3, "tags": ["x", null], "never": null}), None, w.bob).await?.expect("the row");
    assert_eq!(patched.get::<Json>("cells")?, json!({"count": 3, "tags": ["x", null], "files": file}));
    assert_eq!(
        (patched.get::<Uuid>("last_editor_id")?, patched.get::<Uuid>("creator_id")?, patched.get::<String>("position")?),
        (w.bob, w.ann, "a0".to_string())
    );
    assert!(patched.get::<Timestamp>("updated_at")? > patched.get::<Timestamp>("created_at")?);
    // A patch can move the row too.
    let moved = patch_row(&db, base, first_id, json!({}), Some("a0X"), w.ann).await?.expect("the row");
    assert_eq!(
        (moved.get::<String>("position")?, moved.get::<Json>("cells")?),
        ("a0X".to_string(), json!({"count": 3, "tags": ["x", null], "files": file}))
    );
    // Not a row of this base: nothing is written.
    assert!(patch_row(&db, other_base, first_id, json!({"count": 9}), None, w.ann).await?.is_none());
    assert!(patch_row(&db, base, uuid(404), json!({"count": 9}), None, w.ann).await?.is_none());

    // Two editors change different cells of one row at the same moment.
    let edit = |patch: Json| {
        let db = db.clone();
        async move { patch_row(&db, base, second_id, patch, None, w.bob).await }
    };
    let (a, b) = race(edit(json!({"status": "done"})), edit(json!({"owner": "ann", "name": null}))).await;
    assert!(a?.is_some() && b?.is_some());
    assert_eq!(db.get("base_rows", [second_id]).await?.expect("the row").get::<Json>("cells")?, json!({"status": "done", "owner": "ann"}));
    Ok(())
}

/// Many rows, each with a patch of its own, in one statement. Postgres
/// joins the table to a JSON object of `{row id: patch}`; here each row
/// picks its own expression. `updated_at` is left alone: this is the
/// back-fill of a formula or the conversion of a column, not an edit.
fn patch_rows(base: Uuid, patches: &[(Uuid, Json)]) -> Update {
    let cells = case(patches.iter().map(|(id, patch)| (col("id").eq(*id), apply_cell_patch(col("cells"), patch))), col("cells"));
    Update::rows(Query::table("base_rows").keys(patches.iter().map(|(id, _)| [*id])).filter(col("page_id").eq(base))).set("cells", cells)
}

// R-Q4, R-Q6, R-Q13, R-W2, R-W6, R-W7, R-W10, R-X1: one update with a value per row, a keyset back-fill inside one transaction, and bulk changes that count or return their rows.
#[tokio::test(flavor = "multi_thread")]
async fn rows_are_patched_stripped_and_deleted_in_bulk() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let base = new_page(&db, w.space, None, "a0", "Numbers").await?;
    let other_base = new_page(&db, w.space, None, "a1", "Other").await?;
    let stamp = ts("2026-03-01T00:00:00Z");
    let mut rows = Insert::into("base_rows").columns(["id", "page_id", "cells", "position", "updated_at"]);
    for n in 0..1200u64 {
        // Every third row has a cell of the column that will be dropped.
        let cells = if n % 3 == 0 { json!({"n": n, "old": "x"}) } else { json!({"n": n}) };
        rows = rows.values([val(uuid(n.into())), val(base), val(cells), val(format!("p{n:04}")), val(stamp)]);
    }
    assert_eq!(db.insert(rows).await?.affected(), 1200);
    db.insert(
        Insert::into("base_rows").set("id", uuid(5000)).set("page_id", other_base).set("cells", json!({"n": 1, "old": "x"})).set("position", "a0"),
    )
    .await?;

    // A patch per row; another base's row and an id nobody has are left out.
    let patches = [
        (uuid(0), json!({"n": null, "label": "zero"})),
        (uuid(1), json!({"label": "one"})),
        (uuid(5000), json!({"label": "not this base's"})),
        (uuid(9999), json!({"label": "nobody's"})),
    ];
    assert_eq!(db.update(patch_rows(base, &patches)).await?.affected(), 2);
    let cells = |n: u128| {
        let db = db.clone();
        async move { db.get("base_rows", [uuid(n)]).await?.expect("the row").get::<Json>("cells") }
    };
    assert_eq!(cells(0).await?, json!({"old": "x", "label": "zero"}));
    assert_eq!(cells(1).await?, json!({"n": 1, "label": "one"}));
    assert_eq!(cells(5000).await?, json!({"n": 1, "old": "x"}));
    assert_eq!(db.get("base_rows", [uuid(0)]).await?.expect("the row").get::<Timestamp>("updated_at")?, stamp);

    // A new formula column is back-filled over the whole base in one
    // transaction, in batches of 500 read by keyset inside it.
    let batches = db
        .transaction(|tx| async move {
            let mut batches = Vec::new();
            let mut cursor: Option<(String, Uuid)> = None;
            loop {
                let range = match &cursor {
                    None => KeyRange::eq([base]),
                    Some((position, id)) => KeyRange::within([base], Bound::excluded([val(position.as_str()), val(*id)]), Bound::Unbounded),
                };
                let batch = tx.query(Query::table("base_rows").index("base_rows_order_idx", range).limit(500).columns(ROW_COLUMNS)).await?;
                let Some(last) = batch.iter().last() else {
                    break;
                };
                cursor = Some((last.get("position")?, last.get("id")?));
                let mut computed = Vec::new();
                for row in &batch {
                    let n = row.get::<Json>("cells")?["n"].as_u64().unwrap_or(0);
                    computed.push((row.get::<Uuid>("id")?, json!({"double": n * 2})));
                }
                batches.push(tx.update(patch_rows(base, &computed)).await?.affected());
            }
            Ok::<_, DbError>(batches)
        })
        .await?;
    assert_eq!(batches, [500, 500, 200]);
    assert_eq!(cells(0).await?, json!({"old": "x", "label": "zero", "double": 0}));
    assert_eq!(cells(1199).await?, json!({"n": 1199, "double": 2398}));
    assert_eq!(
        rows_in(&db, Query::table("base_rows").index("base_rows_order_idx", KeyRange::eq([base])).filter(col("cells").json_has_key("double")))
            .await?,
        1200
    );
    assert!(!cells(5000).await?.as_object().expect("an object").contains_key("double"));

    // A deleted column's key is taken out of every row that has it; the
    // count says how many did.
    let strip = Update::table("base_rows")
        .index("base_rows_order_idx", KeyRange::eq([base]))
        .filter(col("cells").json_has_key("old"))
        .set("cells", call(Func::JsonRemove, [col("cells"), val("old")]));
    assert_eq!(db.update(strip.clone()).await?.affected(), 400);
    assert_eq!(db.update(strip).await?.affected(), 0);
    assert_eq!(cells(0).await?, json!({"label": "zero", "double": 0}));
    assert_eq!(cells(5000).await?, json!({"n": 1, "old": "x"}));

    // Deleting some rows returns which of the ids really were rows
    // of this base.
    let asked = [uuid(3), uuid(5000), uuid(9999), uuid(4)];
    let gone =
        db.delete(Delete::rows(Query::table("base_rows").keys(asked.map(|id| [id])).filter(col("page_id").eq(base))).returning(["id"])).await?;
    assert_eq!((gone.affected(), ids(&gone)), (2, vec![uuid(3), uuid(4)]));
    assert_eq!(rows_in(&db, Query::table("base_rows")).await?, 1199);
    Ok(())
}

// --------------------------------------------- bases: the dynamic row query

// The functions below, which a Postgres schema would define in SQL, read
// one cell of a row's `cells` object as the type its column stands for,
// and give NULL for anything that does not convert. Here they are
// expressions the caller builds; each takes the property id whose cell it
// reads, in a query that reads `base_rows` as `r`.

fn cell(id: &str) -> Expr {
    col("r.cells").json([id])
}

/// The cell as text: a string as it is, anything else as its JSON text.
fn cell_text(id: &str) -> Expr {
    col("r.cells").json_text([id])
}

fn cell_type(id: &str) -> Expr {
    call(Func::JsonTypeof, [cell(id)])
}

/// `cell_number`: a JSON number, or a string that reads as a decimal
/// number. Exact, so that 10.50 equals 10.5 and nothing else.
fn cell_number(id: &str) -> Expr {
    when(cell_type(id).in_list(vec!["number", "string"]), cell_text(id).try_cast(Type::Decimal(None)), null())
}

/// `cell_time`: strings only. A bare `YYYY-MM-DD` is midnight in the
/// viewer's zone, `offset_minutes` east of UTC; anything else is read as
/// a timestamp, in UTC when it names no zone; what is neither is NULL.
fn cell_time(id: &str, offset_minutes: i64) -> Expr {
    let text = cell_text(id).trim();
    let bare_date = text.clone().try_cast(Type::Date).is_not_null();
    let local_midnight = call(Func::ParseTimestamp, [text.clone(), val(offset_minutes * 60)]);
    when(cell_type(id).eq("string"), when(bare_date, local_midnight, call(Func::ParseTimestamp, [text])), null())
}

/// `cell_flag`: booleans; the strings true/yes/on/1 and false/no/off/0
/// and the empty one, trimmed and whatever their case; numbers other
/// than zero.
fn cell_flag(id: &str) -> Expr {
    let word = cell_text(id).trim().lower();
    case(
        [
            (cell_type(id).eq("boolean"), cell_text(id).eq("true")),
            (cell_type(id).eq("number"), cell_text(id).try_cast(Type::Decimal(None)).ne(0)),
            (cell_type(id).eq("string").and(word.clone().in_list(vec!["true", "yes", "on", "1"])), val(true)),
            (cell_type(id).eq("string").and(word.in_list(vec!["false", "no", "off", "0", ""])), val(false)),
        ],
        null(),
    )
}

/// `cell_list`: an array stays one, a single string becomes an array of
/// itself, everything else is NULL.
fn cell_list(id: &str) -> Expr {
    case([(cell_type(id).eq("array"), cell(id)), (cell_type(id).eq("string"), call(Func::JsonArray, [cell(id)]))], null())
}

/// What kind of value a column of a base holds.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Text,
    Number,
    Flag,
    Time,
    List,
    CreatedAt,
    UpdatedAt,
    LastEditedBy,
}

/// A column of a base, as much of it as filters and sorts need.
#[derive(Clone, Copy, Debug)]
struct Prop {
    id: &'static str,
    kind: Kind,
    /// For a select: its choices, in the order the column lists them.
    choices: &'static [&'static str],
    /// The cell holds a user's id.
    person: bool,
}

const fn prop(id: &'static str, kind: Kind) -> Prop {
    Prop { id, kind, choices: &[], person: false }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
    IsEmpty,
    IsNotEmpty,
    Eq,
    Neq,
    Contains,
    NotContains,
    StartsWith,
    EndsWith,
    Gt,
    Gte,
    Lt,
    Lte,
    AnyOf,
    NoneOf,
    AllOf,
    Before,
    OnOrBefore,
    After,
    OnOrAfter,
}

/// A filter as a view stores it: conditions on columns, in groups that
/// nest.
#[derive(Clone, Debug)]
enum Filter {
    And(Vec<Filter>),
    Or(Vec<Filter>),
    Is(&'static str, Op, Json),
}

struct Sort {
    prop: &'static str,
    desc: bool,
}

fn strings(value: &Json) -> Vec<String> {
    value.as_array().into_iter().flatten().filter_map(|item| item.as_str().map(str::to_string)).collect()
}

/// The day a time filter names, as the half-open range of instants it
/// covers in the viewer's zone. The bounds are sent as ISO text.
fn day_range(day: &Json, offset_minutes: i64) -> Option<(String, String)> {
    let day = Date::parse(day.as_str()?)?;
    let start = i64::from(day.0) * 86_400_000_000 - offset_minutes * 60_000_000;
    Some((Timestamp(start).to_rfc3339(), Timestamp(start + 86_400_000_000).to_rfc3339()))
}

/// Compiles the filter and the sorts of a base's view into a query.
struct Compiler<'a> {
    props: &'a [Prop],
    /// The viewer's zone, minutes east of UTC.
    offset_minutes: i64,
}

impl Compiler<'_> {
    fn prop(&self, id: &str) -> Prop {
        *self.props.iter().find(|prop| prop.id == id).unwrap_or_else(|| panic!("no property {id}: a caller answers 400 before any query"))
    }

    /// The value of a column in a row.
    fn column(&self, prop: Prop) -> Expr {
        match prop.kind {
            Kind::Text => cell_text(prop.id),
            Kind::Number => cell_number(prop.id),
            Kind::Flag => cell_flag(prop.id),
            Kind::Time => cell_time(prop.id, self.offset_minutes),
            Kind::List => cell_list(prop.id),
            Kind::CreatedAt => col("r.created_at"),
            Kind::UpdatedAt => col("r.updated_at"),
            Kind::LastEditedBy => coalesce([col("r.last_editor_id"), col("r.creator_id")]).cast(Type::Text),
        }
    }

    /// A filter as one expression. An empty group is true.
    fn filter(&self, filter: &Filter) -> Expr {
        match filter {
            Filter::And(parts) if parts.is_empty() => val(true),
            Filter::Or(parts) if parts.is_empty() => val(true),
            Filter::And(parts) => all(parts.iter().map(|part| self.filter(part))),
            Filter::Or(parts) => any(parts.iter().map(|part| self.filter(part))),
            Filter::Is(id, op, value) => self.condition(self.prop(id), *op, value),
        }
    }

    /// One operator on one column.
    fn condition(&self, prop: Prop, op: Op, value: &Json) -> Expr {
        let e = self.column(prop);
        let empty = match prop.kind {
            Kind::Text | Kind::LastEditedBy => e.clone().is_null().or(e.clone().eq("")),
            Kind::List => e.clone().json_len().or_else(0).eq(0),
            _ => e.clone().is_null(),
        };
        match (prop.kind, op) {
            (_, Op::IsEmpty) => empty,
            (_, Op::IsNotEmpty) => empty.not(),
            (Kind::Text | Kind::LastEditedBy, _) => Self::text(e, op, value),
            (Kind::Number, _) => Self::number(e, op, value),
            (Kind::Flag, _) => Self::flag(e, op, value),
            (Kind::Time | Kind::CreatedAt | Kind::UpdatedAt, _) => self.time(e, op, value),
            (Kind::List, _) => Self::list(e, op, value),
        }
    }

    fn text(e: Expr, op: Op, value: &Json) -> Expr {
        let text = value.as_str().unwrap_or_default();
        let pattern = |before: &str, after: &str| format!("{before}{}{after}", like_escape(text));
        let listed = strings(value);
        match op {
            Op::Eq => e.eq(text),
            Op::Neq => e.is_distinct_from(text),
            Op::Contains => e.ilike(pattern("%", "%")),
            Op::NotContains => e.ilike(pattern("%", "%")).or_else(false).not(),
            Op::StartsWith => e.ilike(pattern("", "%")),
            Op::EndsWith => e.ilike(pattern("%", "")),
            // Text compares by its bytes here; Postgres uses the
            // database's collation.
            Op::Gt => e.gt(text),
            Op::Gte => e.ge(text),
            Op::Lt => e.lt(text),
            Op::Lte => e.le(text),
            // An empty list is decided by the caller, not asked.
            Op::AnyOf if listed.is_empty() => val(false),
            Op::AnyOf => e.in_list(listed),
            Op::NoneOf if listed.is_empty() => val(true),
            Op::NoneOf => e.clone().is_null().or(e.not_in_list(listed)),
            Op::AllOf => match listed.as_slice() {
                [] => val(true),
                [only] => e.eq(only.as_str()),
                _ => val(false),
            },
            _ => panic!("{op:?} is not an operator on text"),
        }
    }

    fn number(e: Expr, op: Op, value: &Json) -> Expr {
        // The filter's number as an exact decimal. One that is not a
        // finite number matches nothing, and everything for "is not".
        let number = match value {
            Json::Number(n) => n.to_string().parse::<Decimal>().ok(),
            Json::String(s) => s.parse::<Decimal>().ok(),
            _ => None,
        };
        let Some(n) = number else {
            return val(op == Op::Neq);
        };
        match op {
            Op::Eq => e.eq(n),
            Op::Neq => e.is_distinct_from(n),
            Op::Gt => e.gt(n),
            Op::Gte => e.ge(n),
            Op::Lt => e.lt(n),
            Op::Lte => e.le(n),
            _ => panic!("{op:?} is not an operator on numbers"),
        }
    }

    fn flag(e: Expr, op: Op, value: &Json) -> Expr {
        // An unchecked box and a box never touched are the same.
        let is = e.or_else(false).eq(value.as_bool().unwrap_or(false));
        match op {
            Op::Eq => is,
            Op::Neq => is.not(),
            _ => panic!("{op:?} is not an operator on checkboxes"),
        }
    }

    fn time(&self, e: Expr, op: Op, value: &Json) -> Expr {
        // A value that names no day matches nothing, whatever the operator.
        let Some((start, end)) = day_range(value, self.offset_minutes) else {
            return val(false);
        };
        let within = e.clone().ge(start.as_str()).and(e.clone().lt(end.as_str()));
        match op {
            Op::Eq => within,
            Op::Neq => within.or_else(false).not(),
            Op::Before | Op::Lt => e.lt(start),
            Op::OnOrBefore | Op::Lte => e.lt(end),
            Op::After | Op::Gt => e.ge(end),
            Op::OnOrAfter | Op::Gte => e.ge(start),
            _ => panic!("{op:?} is not an operator on dates"),
        }
    }

    fn list(e: Expr, op: Op, value: &Json) -> Expr {
        let text = value.as_str().unwrap_or_default();
        let listed = strings(value);
        // The array as text, the way Postgres prints jsonb: `["a", "b"]`.
        let holds_text = e.clone().cast(Type::Text).ilike(format!("%{}%", like_escape(text))).or_else(false);
        match op {
            Op::Eq => e.json_has_key(text).or_else(false),
            Op::Neq => e.json_has_key(text).or_else(false).not(),
            Op::AnyOf if listed.is_empty() => val(false),
            Op::AnyOf => e.json_has_any(listed).or_else(false),
            Op::NoneOf if listed.is_empty() => val(true),
            Op::NoneOf => e.json_has_any(listed).or_else(false).not(),
            Op::AllOf if listed.is_empty() => val(true),
            Op::AllOf => e.json_has_all(listed).or_else(false),
            Op::Contains => holds_text,
            Op::NotContains => holds_text.not(),
            _ => panic!("{op:?} is not an operator on lists"),
        }
    }

    /// What a column sorts by.
    fn sort_key(&self, prop: Prop) -> Expr {
        let e = self.column(prop);
        // A person sorts by their name, read from `users` for each row.
        let name_of = |user: Expr| scalar(Query::table("users").alias("u").key([user]).select("name", col("u.name").lower()));
        match prop.kind {
            // A select sorts by the place of its choice among the choices.
            Kind::Text if !prop.choices.is_empty() => call(Func::ListPosition, [val(prop.choices.to_vec()), e]),
            Kind::Text if prop.person => name_of(e.try_cast(Type::Uuid)),
            Kind::LastEditedBy => name_of(coalesce([col("r.last_editor_id"), col("r.creator_id")])),
            Kind::List => e.cast(Type::Text),
            _ => e,
        }
    }

    /// The sorts of a view: empty cells last whichever way a key runs,
    /// and the rows' own order to finish.
    fn sorted(&self, rows: Query, sorts: &[Sort]) -> Query {
        let rows = sorts.iter().fold(rows, |rows, sort| {
            let key = self.sort_key(self.prop(sort.prop));
            rows.order_by(if sort.desc { key.desc().nulls_last() } else { key.asc().nulls_last() })
        });
        rows.order_by(col("r.position")).order_by(col("r.id"))
    }
}

/// The columns of the base the row-query tests use.
const PROPS: [Prop; 10] = [
    prop("name", Kind::Text),
    prop("amount", Kind::Number),
    prop("done", Kind::Flag),
    prop("due", Kind::Time),
    prop("tags", Kind::List),
    Prop { choices: &["todo", "doing", "done"], ..prop("status", Kind::Text) },
    Prop { person: true, ..prop("owner", Kind::Text) },
    prop("created", Kind::CreatedAt),
    prop("edited", Kind::UpdatedAt),
    prop("editor", Kind::LastEditedBy),
];

struct Base {
    page: Uuid,
    ann: Uuid,
    bob: Uuid,
}

/// A base of six rows, `uuid(1)` to `uuid(6)` in their own order (rows 4
/// and 5 share a position), with cells of every shape a column can meet:
/// numbers as numbers and as text, dates in three spellings, a list that
/// is a bare string, wrong types, and a row with no cells at all. Row n
/// was made on the n-th of March and edited ten days later.
async fn base(db: &Client) -> DbResult<Base> {
    let w = wiki(db).await?;
    let (ann, bob) = (w.ann, w.bob);
    db.update(Update::table("users").key([bob]).set("name", "Bob")).await?;
    let page = new_page(db, w.space, None, "a0", "Tasks").await?;
    let other = new_page(db, w.space, None, "a1", "Another base").await?;
    let cells = json!([
        {"name": "Alpha", "amount": 10, "done": true, "due": "2026-03-05", "tags": ["a", "b"], "status": "doing", "owner": ann.to_string()},
        {"name": "beta", "amount": "10.50", "done": "yes", "due": "2026-03-05T23:30:00Z", "tags": ["b"], "status": "todo", "owner": bob.to_string()},
        {"name": "Gamma 100%", "amount": 9.99, "done": false, "due": "March 6, 2026", "tags": "c", "status": "done"},
        {"name": "", "amount": "abc", "done": "off", "due": "2026-02-30", "tags": [], "status": 7},
        {},
        {"name": "delta_x", "amount": "1e1", "done": 1, "due": "2026", "tags": ["a", "c"], "status": "todo", "owner": "not-a-uuid"}
    ]);
    // Each row's position, who made it and who edited it last.
    let rows = [
        ("a0", Some(ann), None),
        ("a1", Some(ann), Some(bob)),
        ("a2", Some(bob), None),
        ("a3", None, None),
        ("a3", Some(bob), Some(ann)),
        ("a4", Some(ann), None),
    ];
    let mut insert =
        Insert::into("base_rows").columns(["id", "page_id", "position", "creator_id", "last_editor_id", "cells", "created_at", "updated_at"]);
    for (n, ((position, creator, editor), cells)) in (1u128..).zip(rows.into_iter().zip(cells.as_array().expect("an array"))) {
        let (made, edited) = (ts(&format!("2026-03-{n:02}T00:00:00Z")), ts(&format!("2026-03-{:02}T00:00:00Z", n + 10)));
        insert = insert.values([val(uuid(n)), val(page), val(position), val(creator), val(editor), val(cells.clone()), val(made), val(edited)]);
    }
    db.insert(insert).await?;
    db.insert(
        Insert::into("base_rows")
            .set("id", uuid(7))
            .set("page_id", other)
            .set("position", "a0")
            .set("cells", json!({"name": "beta", "amount": 10, "done": true, "tags": ["a"]})),
    )
    .await?;
    Ok(Base { page, ann, bob })
}

/// The rows of a base, read as `r` in their own order.
fn rows_of(base: Uuid) -> Query {
    Query::table("base_rows").alias("r").index("base_rows_order_idx", KeyRange::eq([base]))
}

/// The numbers of the rows a filter keeps, for a viewer in a given zone.
async fn kept_in_zone(db: &Client, base: Uuid, offset_minutes: i64, filter: Filter) -> DbResult<Vec<u128>> {
    let compiler = Compiler { props: &PROPS, offset_minutes };
    Ok(numbers(&db.query(rows_of(base).filter(compiler.filter(&filter)).columns(["r.id"])).await?))
}

async fn kept(db: &Client, base: Uuid, prop: &'static str, op: Op, value: Json) -> DbResult<Vec<u128>> {
    kept_in_zone(db, base, 0, Filter::Is(prop, op, value)).await
}

/// A cell; then what it is as a number, as a time for a viewer two hours
/// east of UTC, as a checkbox and as a list.
type CellCase = (Json, Option<Decimal>, Option<Timestamp>, Option<bool>, Option<Json>);

// R-Q13, R-Q20, R-Q22, R-T4, R-T5: one JSON cell read as a decimal, a timestamp, a boolean, a list and text, NULL where it does not convert.
#[tokio::test(flavor = "multi_thread")]
async fn cells_read_as_numbers_times_flags_and_lists_or_as_nothing() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let page = new_page(&db, w.space, None, "a0", "Cells").await?;
    let number = |text: &str| Some(text.parse::<Decimal>().expect("a decimal"));
    let time = |text: &str| Some(ts(text));
    let cases: Vec<CellCase> = vec![
        (json!(12.5), number("12.5"), None, Some(true), None),
        (json!(0), number("0"), None, Some(false), None),
        (json!("12.50"), number("12.5"), None, None, Some(json!(["12.50"]))),
        (json!(" 7 "), number("7"), None, None, Some(json!([" 7 "]))),
        (json!("1e2"), number("100"), None, None, Some(json!(["1e2"]))),
        (json!("-.5"), number("-0.5"), None, None, Some(json!(["-.5"]))),
        (json!("12abc"), None, None, None, Some(json!(["12abc"]))),
        (json!(""), None, None, Some(false), Some(json!([""]))),
        (json!(true), None, None, Some(true), None),
        (json!(false), None, None, Some(false), None),
        (json!(" Yes "), None, None, Some(true), Some(json!([" Yes "]))),
        (json!("OFF"), None, None, Some(false), Some(json!(["OFF"]))),
        (json!("1"), number("1"), None, Some(true), Some(json!(["1"]))),
        // A bare date is the viewer's midnight; a full timestamp is
        // itself; a date and time without a zone is UTC.
        (json!("2026-03-05"), None, time("2026-03-04T22:00:00Z"), None, Some(json!(["2026-03-05"]))),
        (json!("2026-03-05T10:00:00+02:00"), None, time("2026-03-05T08:00:00Z"), None, Some(json!(["2026-03-05T10:00:00+02:00"]))),
        (json!("2026-03-05 10:00"), None, time("2026-03-05T10:00:00Z"), None, Some(json!(["2026-03-05 10:00"]))),
        (json!("March 5, 2026"), None, time("2026-03-05T00:00:00Z"), None, Some(json!(["March 5, 2026"]))),
        // A year alone is a number and no date; the 30th of February is
        // neither.
        (json!("2026"), number("2026"), None, None, Some(json!(["2026"]))),
        (json!("2026-02-30"), None, None, None, Some(json!(["2026-02-30"]))),
        (json!(["a", "b"]), None, None, None, Some(json!(["a", "b"]))),
        (json!([]), None, None, None, Some(json!([]))),
        (json!({"k": 1}), None, None, None, None),
        (json!(null), None, None, None, None),
    ];
    let mut rows = Insert::into("base_rows").columns(["id", "page_id", "position", "cells"]);
    for (n, case) in cases.iter().enumerate() {
        rows = rows.values([val(uuid(n as u128 + 1)), val(page), val(format!("p{n:02}")), val(json!({"v": case.0}))]);
    }
    // And a row without the cell.
    rows = rows.values([val(uuid(100)), val(page), val("p99"), val(json!({"other": 1}))]);
    db.insert(rows).await?;

    let read = rows_of(page)
        .select("number", cell_number("v"))
        .select("time", cell_time("v", 120))
        .select("flag", cell_flag("v"))
        .select("list", cell_list("v"))
        .select("text", cell_text("v"));
    let read = db.query(read).await?.into_vec();
    assert_eq!(read.len(), cases.len() + 1);
    for (row, (cell, number, time, flag, list)) in read.iter().zip(&cases) {
        assert_eq!(&row.get::<Option<Decimal>>("number")?, number, "{cell} as a number");
        assert_eq!(&row.get::<Option<Timestamp>>("time")?, time, "{cell} as a time");
        assert_eq!(&row.get::<Option<bool>>("flag")?, flag, "{cell} as a checkbox");
        assert_eq!(&row.get::<Option<Json>>("list")?, list, "{cell} as a list");
    }
    let absent = read.last().expect("the last row");
    for name in ["number", "time", "flag", "list", "text"] {
        assert!(absent.is_null(name)?, "an absent cell as {name}");
    }
    // As text, a string is itself and anything else is its JSON text; a
    // JSON null is no text at all.
    let texts: Vec<Option<String>> = read.iter().map(|row| row.get("text")).collect::<DbResult<_>>()?;
    assert_eq!(texts[0].as_deref(), Some("12.5"));
    assert_eq!(texts[2].as_deref(), Some("12.50"));
    assert_eq!(texts[8].as_deref(), Some("true"));
    assert_eq!(texts[19].as_deref(), Some(r#"["a", "b"]"#));
    assert_eq!(texts[21].as_deref(), Some(r#"{"k": 1}"#));
    assert_eq!(texts[22], None);
    Ok(())
}

// R-Q7, R-Q20, R-T9: filters on a cell as text: equality, patterns, order by bytes, membership and emptiness.
#[tokio::test(flavor = "multi_thread")]
async fn text_filters_compare_the_cell_as_text() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let b = base(&db).await?;
    let name = |op: Op, value: Json| kept(&db, b.page, "name", op, value);

    assert_eq!(name(Op::Eq, json!("beta")).await?, [2]);
    // "Is not" counts an empty cell; a plain `<>` would not.
    assert_eq!(name(Op::Neq, json!("beta")).await?, [1, 3, 4, 5, 6]);
    assert_eq!(name(Op::Contains, json!("A")).await?, [1, 2, 3, 6]);
    assert_eq!(name(Op::NotContains, json!("a")).await?, [4, 5]);
    assert_eq!(name(Op::StartsWith, json!("al")).await?, [1]);
    assert_eq!(name(Op::EndsWith, json!("X")).await?, [6]);
    // The user's `%` and `_` are characters.
    assert_eq!(name(Op::Contains, json!("_")).await?, [6]);
    assert_eq!(name(Op::Contains, json!("0%")).await?, [3]);
    assert_eq!(name(Op::StartsWith, json!("%")).await?, Vec::<u128>::new());

    // Order comparisons are by bytes: capitals before small letters. A
    // NULL cell is on neither side.
    assert_eq!(name(Op::Lt, json!("B")).await?, [1, 4]);
    assert_eq!(name(Op::Lte, json!("Alpha")).await?, [1, 4]);
    assert_eq!(name(Op::Gt, json!("beta")).await?, [6]);
    assert_eq!(name(Op::Gte, json!("Gamma")).await?, [2, 3, 6]);

    assert_eq!(name(Op::AnyOf, json!(["Alpha", "beta"])).await?, [1, 2]);
    assert_eq!(name(Op::NoneOf, json!(["Alpha", "beta"])).await?, [3, 4, 5, 6]);
    assert_eq!(name(Op::AllOf, json!(["beta"])).await?, [2]);
    assert!(name(Op::AnyOf, json!([])).await?.is_empty());
    assert_eq!(name(Op::NoneOf, json!([])).await?, [1, 2, 3, 4, 5, 6]);

    // Empty is no cell or an empty string.
    assert_eq!(name(Op::IsEmpty, json!(null)).await?, [4, 5]);
    assert_eq!(name(Op::IsNotEmpty, json!(null)).await?, [1, 2, 3, 6]);

    // A cell that is not a string reads as its JSON text.
    assert_eq!(kept(&db, b.page, "status", Op::Eq, json!("7")).await?, [4]);
    assert_eq!(kept(&db, b.page, "status", Op::AnyOf, json!(["todo", "done"])).await?, [2, 3, 6]);
    // "Last edited by" is the editor, or the creator of a row nobody
    // edited, compared as the text of an id.
    assert_eq!(kept(&db, b.page, "editor", Op::Eq, json!(b.ann.to_string())).await?, [1, 5, 6]);
    assert_eq!(kept(&db, b.page, "editor", Op::Neq, json!(b.bob.to_string())).await?, [1, 4, 5, 6]);
    assert_eq!(kept(&db, b.page, "editor", Op::IsEmpty, json!(null)).await?, [4]);
    Ok(())
}

// R-Q20, R-Q22, R-T5, R-T9: filters on a cell as an exact decimal and as a boolean.
#[tokio::test(flavor = "multi_thread")]
async fn number_and_checkbox_filters_convert_the_cell_first() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let b = base(&db).await?;
    let amount = |op: Op, value: Json| kept(&db, b.page, "amount", op, value);

    // 10, "10.50", 9.99, "abc", nothing, "1e1".
    assert_eq!(amount(Op::Eq, json!(10)).await?, [1, 6]);
    assert_eq!(amount(Op::Eq, json!(10.5)).await?, [2]);
    assert_eq!(amount(Op::Neq, json!(10)).await?, [2, 3, 4, 5]);
    assert_eq!(amount(Op::Gt, json!(9.99)).await?, [1, 2, 6]);
    assert_eq!(amount(Op::Gte, json!(9.99)).await?, [1, 2, 3, 6]);
    assert_eq!(amount(Op::Lt, json!(10)).await?, [3]);
    assert_eq!(amount(Op::Lte, json!(10)).await?, [1, 3, 6]);
    // The comparison is of exact decimals: as floats these two bounds
    // would both be 10.5.
    assert_eq!(amount(Op::Gt, json!("10.49999999999999999")).await?, [2]);
    assert_eq!(amount(Op::Lt, json!("10.50000000000000001")).await?, [1, 2, 3, 6]);
    // Text that is no number is an empty cell to a number column.
    assert_eq!(amount(Op::IsEmpty, json!(null)).await?, [4, 5]);
    assert_eq!(amount(Op::IsNotEmpty, json!(null)).await?, [1, 2, 3, 6]);
    // A filter value that is no number is decided without asking.
    assert!(amount(Op::Eq, json!("many")).await?.is_empty());
    assert_eq!(amount(Op::Neq, json!("many")).await?, [1, 2, 3, 4, 5, 6]);

    // Compared as what it is, the same cell is another matter: text
    // against a number is an error, not an empty result.
    let mixed = db.query(rows_of(b.page).filter(cell_text("amount").eq(10))).await.unwrap_err();
    assert_eq!(mixed.code, ErrorCode::TypeMismatch);
    let strict = db.query(rows_of(b.page).select("n", cell_text("amount").cast(Type::Decimal(None)))).await.unwrap_err();
    assert_eq!(strict.code, ErrorCode::TypeMismatch);

    // true, "yes", false, "off", nothing, 1.
    let done = |op: Op, value: Json| kept(&db, b.page, "done", op, value);
    assert_eq!(done(Op::Eq, json!(true)).await?, [1, 2, 6]);
    assert_eq!(done(Op::Eq, json!(false)).await?, [3, 4, 5]);
    assert_eq!(done(Op::Neq, json!(true)).await?, [3, 4, 5]);
    assert_eq!(done(Op::Neq, json!(false)).await?, [1, 2, 6]);
    assert_eq!(done(Op::IsEmpty, json!(null)).await?, [5]);
    assert_eq!(done(Op::IsNotEmpty, json!(null)).await?, [1, 2, 3, 4, 6]);
    Ok(())
}

// R-Q20, R-T3, R-T4: filters on a cell as a timestamp, by day in the viewer's zone.
#[tokio::test(flavor = "multi_thread")]
async fn time_filters_are_half_open_ranges_in_the_viewers_zone() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let b = base(&db).await?;
    let due = |offset: i64, op: Op, day: &str| kept_in_zone(&db, b.page, offset, Filter::Is("due", op, json!(day)));
    assert_eq!(day_range(&json!("2026-03-05"), 120), Some(("2026-03-04T22:00:00.000000Z".to_string(), "2026-03-05T22:00:00.000000Z".to_string())));

    // For a viewer in UTC the cells are the 5th at 00:00, the 5th at
    // 23:30, the 6th at 00:00 ("March 6, 2026"), and three that are no
    // time: the 30th of February, nothing, and "2026".
    assert_eq!(due(0, Op::Eq, "2026-03-05").await?, [1, 2]);
    assert_eq!(due(0, Op::Neq, "2026-03-05").await?, [3, 4, 5, 6]);
    assert!(due(0, Op::Before, "2026-03-05").await?.is_empty());
    assert_eq!(due(0, Op::OnOrBefore, "2026-03-05").await?, [1, 2]);
    assert_eq!(due(0, Op::After, "2026-03-05").await?, [3]);
    assert_eq!(due(0, Op::OnOrAfter, "2026-03-05").await?, [1, 2, 3]);
    assert_eq!(due(0, Op::Lt, "2026-03-06").await?, [1, 2]);
    assert_eq!(due(0, Op::Lte, "2026-03-06").await?, [1, 2, 3]);
    assert_eq!(due(0, Op::Gt, "2026-03-04").await?, [1, 2, 3]);
    assert_eq!(due(0, Op::Gte, "2026-03-06").await?, [3]);
    assert_eq!(due(0, Op::IsEmpty, "").await?, [4, 5, 6]);
    assert_eq!(due(0, Op::IsNotEmpty, "").await?, [1, 2, 3]);

    // Two hours east of UTC the 5th runs from 22:00 on the 4th to 22:00
    // on the 5th. Row 1's bare date is that viewer's midnight, the first
    // instant of the day; rows 2 and 3 name instants after its end.
    assert_eq!(due(120, Op::Eq, "2026-03-05").await?, [1]);
    assert_eq!(due(120, Op::After, "2026-03-05").await?, [2, 3]);
    assert!(due(120, Op::Before, "2026-03-05").await?.is_empty());
    assert_eq!(due(-300, Op::Eq, "2026-03-05").await?, [1, 2, 3]);

    // A value that is no day matches nothing, even for "is not".
    assert!(due(0, Op::Eq, "2026-02-30").await?.is_empty());
    assert!(due(0, Op::Neq, "2026-02-30").await?.is_empty());

    // The row's own timestamps are filtered the same way.
    assert_eq!(kept(&db, b.page, "created", Op::OnOrAfter, json!("2026-03-04")).await?, [4, 5, 6]);
    assert_eq!(kept(&db, b.page, "created", Op::Eq, json!("2026-03-02")).await?, [2]);
    assert_eq!(kept(&db, b.page, "edited", Op::Before, json!("2026-03-13")).await?, [1, 2]);
    Ok(())
}

// R-Q13, R-Q20: filters on a cell as a list: membership of one, any or all, and the text of the array.
#[tokio::test(flavor = "multi_thread")]
async fn list_filters_test_membership_and_the_text_of_the_array() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let b = base(&db).await?;
    let tags = |op: Op, value: Json| kept(&db, b.page, "tags", op, value);

    // ["a", "b"], ["b"], "c" (a bare string: a list of one), [], nothing,
    // ["a", "c"].
    assert_eq!(tags(Op::Eq, json!("a")).await?, [1, 6]);
    assert_eq!(tags(Op::Eq, json!("c")).await?, [3, 6]);
    assert_eq!(tags(Op::Neq, json!("a")).await?, [2, 3, 4, 5]);
    assert_eq!(tags(Op::AnyOf, json!(["a", "c"])).await?, [1, 3, 6]);
    assert_eq!(tags(Op::NoneOf, json!(["a", "c"])).await?, [2, 4, 5]);
    assert_eq!(tags(Op::AllOf, json!(["a", "b"])).await?, [1]);
    assert_eq!(tags(Op::AllOf, json!(["a"])).await?, [1, 6]);
    assert!(tags(Op::AnyOf, json!([])).await?.is_empty());
    assert_eq!(tags(Op::NoneOf, json!([])).await?, [1, 2, 3, 4, 5, 6]);
    assert_eq!(tags(Op::AllOf, json!([])).await?, [1, 2, 3, 4, 5, 6]);

    // "Contains" looks in the text of the array as jsonb prints it, with
    // a space after each comma: a stored filter relies on that rendering.
    assert_eq!(tags(Op::Contains, json!(r#""a", "b""#)).await?, [1]);
    assert!(tags(Op::Contains, json!(r#""a","b""#)).await?.is_empty());
    assert_eq!(tags(Op::Contains, json!("C")).await?, [3, 6]);
    assert_eq!(tags(Op::NotContains, json!("c")).await?, [1, 2, 4, 5]);
    assert_eq!(tags(Op::Contains, json!("[]")).await?, [4]);

    // An empty array and no array are both empty.
    assert_eq!(tags(Op::IsEmpty, json!(null)).await?, [4, 5]);
    assert_eq!(tags(Op::IsNotEmpty, json!(null)).await?, [1, 2, 3, 6]);
    Ok(())
}

// R-Q20, R-T9: filter groups compiled to nested AND and OR, where a NULL condition keeps nothing.
#[tokio::test(flavor = "multi_thread")]
async fn filter_groups_nest_and_an_empty_group_is_true() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let b = base(&db).await?;
    let kept = |filter: Filter| kept_in_zone(&db, b.page, 0, filter);
    let is = |prop: &'static str, op: Op, value: Json| Filter::Is(prop, op, value);

    let done_and_tagged_or_large =
        Filter::And(vec![is("done", Op::Eq, json!(true)), Filter::Or(vec![is("tags", Op::Eq, json!("b")), is("amount", Op::Gt, json!(10))])]);
    assert_eq!(kept(done_and_tagged_or_large).await?, [1, 2]);
    let ten_todo_or_unnamed = Filter::Or(vec![
        Filter::And(vec![is("status", Op::Eq, json!("todo")), is("amount", Op::Eq, json!(10))]),
        is("name", Op::IsEmpty, json!(null)),
    ]);
    assert_eq!(kept(ten_todo_or_unnamed).await?, [4, 5, 6]);

    // A condition that is NULL for a row does not keep it on its own, and
    // does not stop another branch of an OR from keeping it.
    assert_eq!(kept(Filter::Or(vec![is("amount", Op::Gt, json!(100)), is("done", Op::Eq, json!(false))])).await?, [3, 4, 5]);
    assert_eq!(kept(Filter::And(vec![is("amount", Op::Lt, json!(100)), is("done", Op::Eq, json!(false))])).await?, [3]);

    assert_eq!(kept(Filter::And(vec![])).await?, [1, 2, 3, 4, 5, 6]);
    assert_eq!(kept(Filter::Or(vec![])).await?, [1, 2, 3, 4, 5, 6]);
    assert_eq!(kept(Filter::And(vec![Filter::Or(vec![]), is("name", Op::Eq, json!("beta"))])).await?, [2]);

    // Ten groups deep, as deep as a view's filter may go, around every
    // kind of column at once.
    let every_kind = Filter::And(vec![
        is("name", Op::Contains, json!("a")),
        is("amount", Op::Gte, json!(10)),
        is("done", Op::Eq, json!(true)),
        is("due", Op::OnOrAfter, json!("2026-03-01")),
        is("tags", Op::AnyOf, json!(["a", "b"])),
        is("created", Op::Before, json!("2026-03-06")),
        is("editor", Op::IsNotEmpty, json!(null)),
    ]);
    let nested =
        (0..10).fold(every_kind, |inner, depth| if depth % 2 == 0 { Filter::Or(vec![inner]) } else { Filter::And(vec![inner, Filter::And(vec![])]) });
    assert_eq!(kept(nested).await?, [1, 2]);
    Ok(())
}

// R-Q10, R-Q14, R-Q20: sort keys that are expressions and subqueries, each with its direction and its NULLs last.
#[tokio::test(flavor = "multi_thread")]
async fn sorts_put_empty_cells_last_in_both_directions() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let b = base(&db).await?;
    let compiler = Compiler { props: &PROPS, offset_minutes: 0 };
    let order = |sorts: Vec<Sort>| {
        let rows = compiler.sorted(rows_of(b.page), &sorts).columns(["r.id"]);
        let db = db.clone();
        async move { Ok::<_, DbError>(numbers(&db.query(rows).await?)) }
    };
    let asc = |prop: &'static str| Sort { prop, desc: false };
    let desc = |prop: &'static str| Sort { prop, desc: true };

    // No sort: the rows' own order, with the id between rows 4 and 5.
    assert_eq!(order(vec![]).await?, [1, 2, 3, 4, 5, 6]);

    // Numbers: 10, 10.5, 9.99, none, none, 10. The rows without one are
    // last going up and last going down; equal keys keep the rows' order.
    assert_eq!(order(vec![asc("amount")]).await?, [3, 1, 6, 2, 4, 5]);
    assert_eq!(order(vec![desc("amount")]).await?, [2, 1, 6, 3, 4, 5]);

    // A select sorts by the place of its choice in the column's list
    // (todo, doing, done), not by its text; a value that is no choice is
    // an empty cell.
    assert_eq!(order(vec![asc("status")]).await?, [2, 6, 1, 3, 4, 5]);
    assert_eq!(order(vec![desc("status")]).await?, [3, 1, 2, 6, 4, 5]);

    // A person sorts by name, whatever its case, looked up in `users`
    // for each row: "ann" before "Bob". The last editor is the editor or
    // else the creator; the owner is an id in a cell, and text that is
    // no id is nobody.
    assert_eq!(order(vec![asc("editor")]).await?, [1, 5, 6, 2, 3, 4]);
    assert_eq!(order(vec![desc("editor")]).await?, [2, 3, 1, 5, 6, 4]);
    assert_eq!(order(vec![asc("owner")]).await?, [1, 2, 3, 4, 5, 6]);
    assert_eq!(order(vec![desc("owner")]).await?, [2, 1, 3, 4, 5, 6]);

    // A list sorts by the text of its array; an empty array is not an
    // empty cell.
    assert_eq!(order(vec![asc("tags")]).await?, [1, 6, 2, 3, 4, 5]);
    // Times, checkboxes (unchecked first) and text (by bytes; an empty
    // string is a value).
    assert_eq!(order(vec![desc("due")]).await?, [3, 2, 1, 4, 5, 6]);
    assert_eq!(order(vec![asc("done")]).await?, [3, 4, 1, 2, 6, 5]);
    assert_eq!(order(vec![asc("name")]).await?, [4, 1, 3, 2, 6, 5]);
    assert_eq!(order(vec![desc("name")]).await?, [6, 2, 3, 1, 4, 5]);
    assert_eq!(order(vec![desc("created")]).await?, [6, 5, 4, 3, 2, 1]);
    assert_eq!(order(vec![asc("edited")]).await?, [1, 2, 3, 4, 5, 6]);

    // Several keys, each with its own direction.
    assert_eq!(order(vec![asc("status"), asc("amount")]).await?, [6, 2, 1, 3, 4, 5]);
    assert_eq!(order(vec![desc("done"), desc("amount"), asc("name")]).await?, [2, 1, 6, 3, 4, 5]);
    let ten: Vec<Sort> = ["done", "status", "amount", "due", "tags", "owner", "editor", "created", "edited", "name"].into_iter().map(asc).collect();
    assert_eq!(order(ten).await?, [3, 4, 6, 2, 1, 5]);
    Ok(())
}

/// A page of a base's rows in their own order, and the cursor of
/// the next one: the position and id of the last row returned. One row
/// more than the page is read to learn whether there is a next.
async fn rows_page(
    db: &impl Db,
    base: Uuid,
    filter: Expr,
    cursor: Option<(String, Uuid)>,
    size: usize,
) -> DbResult<(Vec<u128>, Option<(String, Uuid)>)> {
    let range = match &cursor {
        None => KeyRange::eq([base]),
        // `(r.position, r.id) > ($2, $3)`: start just after that entry.
        Some((position, id)) => KeyRange::within([base], Bound::excluded([val(position.as_str()), val(*id)]), Bound::Unbounded),
    };
    let rows = Query::table("base_rows")
        .alias("r")
        .index("base_rows_order_idx", range)
        .filter(filter)
        .limit(size as u64 + 1)
        .columns(ROW_COLUMNS.map(|name| format!("r.{name}")));
    let mut rows = db.query(rows).await?.into_vec();
    let more = rows.len() > size;
    rows.truncate(size);
    let next = match rows.last() {
        Some(last) if more => Some((last.get("position")?, last.get("id")?)),
        _ => None,
    };
    let page = rows.iter().map(|row| row.get::<Uuid>("id").map(|id| u128::from_be_bytes(id.0))).collect::<DbResult<_>>()?;
    Ok((page, next))
}

// R-Q4, R-Q5, R-Q20, R-Q21, R-T2: keyset pages off an index, offset pages of a sorted list with a total, and two pages read at one moment.
#[tokio::test(flavor = "multi_thread")]
async fn unsorted_rows_page_by_keyset_and_sorted_rows_by_offset_with_a_total() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let b = base(&db).await?;
    let compiler = Compiler { props: &PROPS, offset_minutes: 0 };

    // Unsorted: pages of two. Rows 4 and 5 share a position, and
    // the cursor falls between them: the id is part of it.
    let (page, cursor) = rows_page(&db, b.page, val(true), None, 2).await?;
    assert_eq!(page, [1, 2]);
    assert_eq!(cursor, Some(("a1".to_string(), uuid(2))));
    let (page, cursor) = rows_page(&db, b.page, val(true), cursor, 2).await?;
    assert_eq!((page, cursor.clone()), (vec![3, 4], Some(("a3".to_string(), uuid(4)))));
    let (page, cursor) = rows_page(&db, b.page, val(true), cursor, 2).await?;
    assert_eq!((page, cursor), (vec![5, 6], None));
    // A cursor at the last row, or past it, gives an empty page.
    assert_eq!(rows_page(&db, b.page, val(true), Some(("a4".to_string(), uuid(6))), 2).await?, (vec![], None));
    assert_eq!(rows_page(&db, b.page, val(true), Some(("zz".to_string(), uuid(1))), 2).await?, (vec![], None));

    // With a filter the pages are of the rows it keeps.
    let done = || compiler.filter(&Filter::Is("done", Op::Eq, json!(true)));
    let (page, cursor) = rows_page(&db, b.page, done(), None, 2).await?;
    assert_eq!((page, cursor.clone()), (vec![1, 2], Some(("a1".to_string(), uuid(2)))));
    assert_eq!(rows_page(&db, b.page, done(), cursor, 2).await?, (vec![6], None));
    // An export walks the whole base in batches.
    assert_eq!(rows_page(&db, b.page, val(true), None, 500).await?, (vec![1, 2, 3, 4, 5, 6], None));

    // Sorted: the sort keys are expressions that may be NULL, so
    // the cursor is an offset, and the count of all the rows the filter
    // keeps comes with the page.
    let sorted_page = |filter: Filter, sorts: Vec<Sort>, offset: u64, limit: u64| {
        compiler
            .sorted(rows_of(b.page).filter(compiler.filter(&filter)), &sorts)
            .offset(offset)
            .limit(limit)
            .with_total()
            .columns(ROW_COLUMNS.map(|name| format!("r.{name}")))
    };
    let by_amount = || vec![Sort { prop: "amount", desc: false }];
    let first = db.query(sorted_page(Filter::And(vec![]), by_amount(), 0, 4)).await?;
    assert_eq!((numbers(&first), first.total()), (vec![3, 1, 6, 2], Some(6)));
    let second = db.query(sorted_page(Filter::And(vec![]), by_amount(), 4, 4)).await?;
    assert_eq!((numbers(&second), second.total()), (vec![4, 5], Some(6)));
    let beyond = db.query(sorted_page(Filter::And(vec![]), by_amount(), 8, 4)).await?;
    assert_eq!((beyond.len(), beyond.total()), (0, Some(6)));
    let filtered = db.query(sorted_page(Filter::Is("amount", Op::IsNotEmpty, json!(null)), vec![Sort { prop: "amount", desc: true }], 1, 2)).await?;
    assert_eq!((numbers(&filtered), filtered.total()), (vec![1, 6], Some(4)));
    assert_eq!(first.first().expect("a row").get::<Json>("cells")?["name"], json!("Gamma 100%"));

    // Both pages of a sorted view read at one moment see the same rows,
    // whatever is written between the two reads.
    let (one, two) = db
        .read(|tx| async move {
            let one = numbers(&tx.query(sorted_page(Filter::And(vec![]), by_amount(), 0, 3)).await?);
            let two = numbers(&tx.query(sorted_page(Filter::And(vec![]), by_amount(), 3, 3)).await?);
            Ok::<_, DbError>((one, two))
        })
        .await?;
    assert_eq!((one, two), (vec![3, 1, 6], vec![2, 4, 5]));
    Ok(())
}

// --------------------------------------------------- JSON, and the lease

// R-Q13, R-Q22, R-T6, R-T9: JSON stored in a canonical key order, compared by structure, read by path and changed by merge.
#[tokio::test(flavor = "multi_thread")]
async fn json_is_stored_in_jsonb_key_order_and_compared_by_structure() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    let w = wiki(&db).await?;
    let page = new_page(&db, w.space, None, "a0", "JSON").await?;
    let cells = json!({
        "zeta": 1,
        "bb": "text",
        "a": {"yy": [3, {"k2": 1, "k": 2}], "x": null},
        "é": 1.5,
        "tags": ["a", "b"],
        "on": true
    });
    let row: Uuid = db
        .insert(Insert::into("base_rows").set("page_id", page).set("position", "a0").set("cells", cells.clone()).returning(["id"]))
        .await?
        .one()?
        .get("id")?;
    let read = |query: Query| {
        let db = db.clone();
        async move { db.query_one(query).await }
    };
    let of_row = || Query::table("base_rows").key([row]);

    // Stored whole, and returned with each object's keys by length, then
    // by bytes, at every depth: the order of Postgres' jsonb, which a
    // response or an ETag can rest on. A document sent back unchanged is
    // still the same document.
    let stored: Json = read(of_row().columns(["cells"])).await?.get("cells")?;
    assert_eq!(stored, cells);
    assert_eq!(stored.to_string(), r#"{"a":{"x":null,"yy":[3,{"k":2,"k2":1}]},"bb":"text","on":true,"é":1.5,"tags":["a","b"],"zeta":1}"#);

    // Equality is of structure: key order does not matter, array order does.
    let reordered = json!({"on": true, "tags": ["a", "b"], "é": 1.5, "a": {"x": null, "yy": [3, {"k": 2, "k2": 1}]}, "bb": "text", "zeta": 1});
    let swapped = json!({"on": true, "tags": ["b", "a"], "é": 1.5, "a": {"x": null, "yy": [3, {"k": 2, "k2": 1}]}, "bb": "text", "zeta": 1});
    let compared = of_row()
        .select("same", col("cells").eq(reordered.clone()))
        .select("distinct_reordered", col("cells").is_distinct_from(reordered.clone()))
        .select("distinct_swapped", col("cells").is_distinct_from(swapped))
        .select("distinct_null", col("cells").is_distinct_from(null()))
        .select("null_distinct_null", null().is_distinct_from(null()))
        .select("equals_null", col("cells").eq(null()));
    let compared = read(compared).await?;
    assert_eq!(
        (compared.get::<bool>("same")?, compared.get::<bool>("distinct_reordered")?, compared.get::<bool>("distinct_swapped")?),
        (true, false, true)
    );
    assert_eq!(
        (compared.get::<bool>("distinct_null")?, compared.get::<bool>("null_distinct_null")?, compared.get::<Option<bool>>("equals_null")?),
        (true, false, None)
    );
    assert_eq!(rows_in(&db, Query::table("base_rows").filter(col("cells").is_not_distinct_from(reordered))).await?, 1);

    // Members: as JSON or as text, by key and by index, from either end.
    let members = of_row()
        .select("object", col("cells").json(["a"]))
        .select("deep", col("cells").json([val("a"), val("yy"), val(1), val("k")]))
        .select("from_end", col("cells").json([val("a"), val("yy"), val(-1), val("k2")]))
        .select("string", col("cells").json_text(["bb"]))
        .select("number", col("cells").json_text(["é"]))
        .select("object_text", col("cells").json_text(["a"]))
        .select("absent", col("cells").json_text(["nothing"]))
        .select("json_null", col("cells").json_text([val("a"), val("x")]))
        .select("past_the_end", col("cells").json([val("tags"), val(5)]))
        .select("as_text", col("cells").json(["tags"]).cast(Type::Text))
        .select("flag", col("cells").json_text(["on"]).cast(Type::Bool))
        .select("not_a_flag", col("cells").json_text(["bb"]).try_cast(Type::Bool));
    let members = read(members).await?;
    assert_eq!(members.get::<Json>("object")?, json!({"x": null, "yy": [3, {"k": 2, "k2": 1}]}));
    assert_eq!((members.get::<Json>("deep")?, members.get::<Json>("from_end")?), (json!(2), json!(1)));
    assert_eq!((members.get::<String>("string")?, members.get::<String>("number")?), ("text".to_string(), "1.5".to_string()));
    assert_eq!(members.get::<String>("object_text")?, r#"{"x": null, "yy": [3, {"k": 2, "k2": 1}]}"#);
    assert!(members.is_null("absent")? && members.is_null("json_null")? && members.is_null("past_the_end")?);
    assert_eq!(members.get::<String>("as_text")?, r#"["a", "b"]"#);
    assert_eq!((members.get::<bool>("flag")?, members.get::<Option<bool>>("not_a_flag")?), (true, None));

    // What a value is, how long, and what it holds.
    let kind = |path: &'static str| call(Func::JsonTypeof, [col("cells").json([path])]);
    let shape = of_row()
        .select("object", kind("a"))
        .select("array", kind("tags"))
        .select("string", kind("bb"))
        .select("number", kind("zeta"))
        .select("boolean", kind("on"))
        .select("absent", kind("nothing"))
        .select("null", call(Func::JsonTypeof, [col("cells").json([val("a"), val("x")])]))
        .select("length", col("cells").json(["tags"]).json_len())
        .select("length_of_text", col("cells").json(["bb"]).json_len())
        .select("has_key", col("cells").json_has_key("bb"))
        .select("has_no_key", col("cells").json_has_key("b"))
        .select("has_item", col("cells").json(["tags"]).json_has_key("b"))
        .select("has_any", col("cells").json(["tags"]).json_has_any(vec!["x", "b"]))
        .select("has_all", col("cells").json(["tags"]).json_has_all(vec!["x", "b"]))
        .select("contains", col("cells").json_contains(json!({"a": {"yy": [{"k": 2}]}, "tags": ["b"]})))
        .select("contains_not", col("cells").json_contains(json!({"tags": ["c"]})));
    let shape = read(shape).await?;
    let kinds: Vec<String> =
        ["object", "array", "string", "number", "boolean", "null"].iter().map(|name| shape.get(name)).collect::<DbResult<_>>()?;
    assert_eq!(kinds, ["object", "array", "string", "number", "boolean", "null"]);
    assert!(shape.is_null("absent")? && shape.is_null("length_of_text")?);
    assert_eq!(shape.get::<i64>("length")?, 2);
    assert_eq!((shape.get::<bool>("has_key")?, shape.get::<bool>("has_no_key")?, shape.get::<bool>("has_item")?), (true, false, true));
    assert_eq!(
        (shape.get::<bool>("has_any")?, shape.get::<bool>("has_all")?, shape.get::<bool>("contains")?, shape.get::<bool>("contains_not")?),
        (true, false, true, false)
    );

    // Changing a value: a merge replaces members whole (it is shallow)
    // and keeps a null; stripping nulls is deep; a key is removed whether
    // or not it is there.
    let patch = json!({"a": {"z": null}, "bb": null, "new": [1, {"gone": null, "kept": 0}]});
    let changed = of_row()
        .select("merged", col("cells").json_merge(patch.clone()))
        .select("stripped", call(Func::JsonStripNulls, [col("cells").json_merge(patch)]))
        .select("removed", call(Func::JsonRemove, [call(Func::JsonRemove, [col("cells"), val("a")]), val("nothing")]))
        .select("built", object([("id", col("id")), ("items", call(Func::JsonArray, [val(1), val("two"), col("cells").json(["on"])]))]));
    let changed = read(changed).await?;
    assert_eq!(
        changed.get::<Json>("merged")?,
        json!({"zeta": 1, "bb": null, "a": {"z": null}, "é": 1.5, "tags": ["a", "b"], "on": true, "new": [1, {"gone": null, "kept": 0}]})
    );
    assert_eq!(changed.get::<Json>("stripped")?, json!({"zeta": 1, "a": {}, "é": 1.5, "tags": ["a", "b"], "on": true, "new": [1, {"kept": 0}]}));
    assert_eq!(changed.get::<Json>("removed")?, json!({"zeta": 1, "bb": "text", "é": 1.5, "tags": ["a", "b"], "on": true}));
    assert_eq!(changed.get::<Json>("built")?, json!({"id": row.to_string(), "items": [1, "two", true]}));

    // A page's document may be absent: NULL is distinct from every
    // document and not distinct from NULL, which a snapshot relies on.
    let empty = db
        .query_one(
            Query::table("pages")
                .key([page])
                .select("is_new", col("content").is_distinct_from(json!({})))
                .select("is_none", col("content").is_not_distinct_from(null())),
        )
        .await?;
    assert_eq!((empty.get::<bool>("is_new")?, empty.get::<bool>("is_none")?), (true, true));
    Ok(())
}

/// Claims or renews the realtime lease. A row back means "you lead"; no
/// row, that someone else holds a lease that has not expired by the
/// database's clock: the one clock every copy of the application shares.
async fn claim_lease(db: &impl Db, holder: &str, seconds: f64) -> DbResult<bool> {
    let claim = Insert::into("realtime_leader")
        .set("holder", holder)
        .set("expires_at", now() + seconds_f64(seconds))
        .on_conflict_update(PRIMARY)
        .update_excluded(["holder", "expires_at"])
        .update_where(col("holder").eq(excluded("holder")).or(col("expires_at").lt(now())))
        .returning(["holder"]);
    let claimed = db.insert(claim).await?;
    assert_eq!(claimed.affected() as usize, claimed.len());
    Ok(!claimed.is_empty())
}

// R-O3, R-Q8, R-S7, R-W4, R-W7, R-X3, R-X4: a lease taken by a conditional upsert on the database's clock: raced, renewed, expired and released.
#[tokio::test(flavor = "multi_thread")]
async fn the_leader_lease_passes_only_when_released_or_expired() -> DbResult<()> {
    let (_server, db) = fresh().await;
    schema(&db).await?;
    // The health probe; and the clock a lease is judged by.
    db.ping().await?;
    let clock = db.now().await?;
    assert!((clock.micros() - Timestamp::now().micros()).abs() < 60_000_000, "the server's clock is the machine's");
    let holder = || async { db.query_opt(Query::table("realtime_leader").key([1])).await?.map(|row| row.get::<String>("holder")).transpose() };
    let expires = || async { db.query_one(Query::table("realtime_leader").key([1])).await?.get::<Timestamp>("expires_at") };

    // Three copies of the application start at once: one leads.
    let start = |name: &'static str| {
        let db = db.clone();
        async move { claim_lease(&db, name, 10.0).await }
    };
    let (a, b) = race(start("copy-a"), start("copy-b")).await;
    let (a, b) = (a?, b?);
    assert!(a != b, "exactly one of two claims wins");
    let (leader, follower) = if a { ("copy-a", "copy-b") } else { ("copy-b", "copy-a") };
    assert!(!claim_lease(&db, "copy-c", 10.0).await?);
    assert_eq!(holder().await?.as_deref(), Some(leader));

    // The leader renews, every few seconds: its expiry moves forward. The
    // others keep trying and keep getting no row.
    let first = expires().await?;
    assert!(first > clock.plus_secs(9) && first < clock.plus_secs(70));
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(claim_lease(&db, leader, 10.0).await?);
    assert!(expires().await? > first);
    assert!(!claim_lease(&db, follower, 10.0).await?);
    assert_eq!(holder().await?.as_deref(), Some(leader));
    // A lease can be claimed for a fraction of a second too.
    assert!(claim_lease(&db, leader, 0.25).await?);
    assert!(expires().await? < first);
    assert!(claim_lease(&db, leader, 10.0).await?);

    // The leader crashes: its lease runs out. Rather than wait ten
    // seconds, the row says it expired a moment ago.
    db.update(Update::table("realtime_leader").key([1]).set("expires_at", now() - seconds(1))).await?;
    assert!(claim_lease(&db, follower, 10.0).await?);
    assert_eq!(holder().await?.as_deref(), Some(follower));
    // The old leader, back up, is a follower now.
    assert!(!claim_lease(&db, leader, 10.0).await?);

    // A clean shutdown releases the lease, and only its holder's.
    assert_eq!(db.delete(Delete::from("realtime_leader").filter(col("holder").eq(leader))).await?.affected(), 0);
    assert_eq!(db.delete(Delete::from("realtime_leader").filter(col("holder").eq(follower))).await?.affected(), 1);
    assert_eq!(holder().await?, None);
    assert!(claim_lease(&db, "copy-c", 10.0).await?);

    // A lease left behind by a copy that died long ago is no obstacle.
    db.delete(Delete::from("realtime_leader")).await?;
    db.insert(Insert::into("realtime_leader").set("holder", "long-dead").set("expires_at", now() - hours(3))).await?;
    assert!(claim_lease(&db, leader, 10.0).await?);
    assert_eq!(holder().await?.as_deref(), Some(leader));

    // There is one lease.
    let second = db.insert(Insert::into("realtime_leader").set("id", 2).set("holder", "x").set("expires_at", now())).await.unwrap_err();
    assert_eq!(second.code, ErrorCode::CheckViolation);
    assert!(second.violates("realtime_leader_id_check"));
    assert_eq!(rows_in(&db, Query::table("realtime_leader")).await?, 1);
    Ok(())
}
